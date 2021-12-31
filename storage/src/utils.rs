// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Utility helpers to supprt the storage subsystem.
use std::cmp::{self, min};
use std::io::{ErrorKind, Result};
use std::os::unix::io::RawFd;
use std::slice::from_raw_parts_mut;

use fuse_backend_rs::transport::FileVolatileSlice;
use libc::off64_t;
use nix::sys::uio::{preadv, IoVec};
use nydus_utils::{
    digest::{self, RafsDigest},
    round_down_4k,
};
use vm_memory::Bytes;

use crate::{StorageError, StorageResult};

/// Just a simple wrapper for posix `preadv`. Provide a slice of `IoVec` as input.
pub fn readv(fd: RawFd, iovec: &[IoVec<&mut [u8]>], offset: u64) -> Result<usize> {
    loop {
        match preadv(fd, iovec, offset as off64_t).map_err(|_| last_error!()) {
            Ok(ret) => return Ok(ret),
            // Retry if the IO is interrupted by signal.
            Err(err) if err.kind() != ErrorKind::Interrupted => return Err(err),
            _ => continue,
        }
    }
}

/// Copy from buffer slice to another buffer slice.
///
/// `offset` is where to start copy in the first buffer of source slice.
/// Up to bytes of `length` is wanted in `src`.
/// `dst_index` and `dst_slice_offset` indicate from where to start write destination.
/// Return (Total copied bytes, (Final written destination index, Final written destination offset))
pub fn copyv<S: AsRef<[u8]>>(
    src: &[S],
    dst: &[FileVolatileSlice],
    offset: usize,
    length: usize,
    mut dst_index: usize,
    mut dst_offset: usize,
) -> StorageResult<(usize, (usize, usize))> {
    // Validate input parameters first to protect following loop block.
    if src.is_empty() || length == 0 {
        return Ok((0, (dst_index, dst_offset)));
    } else if offset > src[0].as_ref().len()
        || dst_index >= dst.len()
        || dst_offset > dst[dst_index].len()
    {
        return Err(StorageError::MemOverflow);
    }

    let mut copied = 0;
    let mut src_offset = offset;
    'next_source: for s in src {
        let s = s.as_ref();
        let mut buffer_len = min(s.len() - src_offset, length - copied);

        loop {
            if dst_index >= dst.len() {
                return Err(StorageError::MemOverflow);
            }

            let dst_slice = &dst[dst_index];
            let buffer = &s[src_offset..src_offset + buffer_len];
            let written = dst_slice
                .write(buffer, dst_offset)
                .map_err(StorageError::VolatileSlice)?;

            copied += written;
            if dst_slice.len() - dst_offset == written {
                dst_index += 1;
                dst_offset = 0;
            } else {
                dst_offset += written;
            }

            // Move to next source buffer if the current source buffer has been exhausted.
            if written == buffer_len {
                src_offset = 0;
                continue 'next_source;
            } else {
                src_offset += written;
                buffer_len -= written;
            }
        }
    }

    Ok((copied, (dst_index, dst_offset)))
}

/// An memory cursor to access an `FileVolatileSlice` array.
pub struct MemSliceCursor<'a> {
    pub mem_slice: &'a [FileVolatileSlice<'a>],
    pub index: usize,
    pub offset: usize,
}

impl<'a> MemSliceCursor<'a> {
    /// Create a new `MemSliceCursor` object.
    pub fn new<'b: 'a>(slice: &'b [FileVolatileSlice]) -> Self {
        Self {
            mem_slice: slice,
            index: 0,
            offset: 0,
        }
    }

    /// Move cursor forward by `size`.
    pub fn move_cursor(&mut self, mut size: usize) {
        while size > 0 && self.index < self.mem_slice.len() {
            let slice = self.mem_slice[self.index];
            let this_left = slice.len() - self.offset;

            match this_left.cmp(&size) {
                cmp::Ordering::Equal => {
                    self.index += 1;
                    self.offset = 0;
                    return;
                }
                cmp::Ordering::Greater => {
                    self.offset += size;
                    return;
                }
                cmp::Ordering::Less => {
                    self.index += 1;
                    self.offset = 0;
                    size -= this_left;
                    continue;
                }
            }
        }
    }

    /// Consume `size` bytes of memory content from the cursor.
    pub fn consume(&mut self, mut size: usize) -> Vec<IoVec<&mut [u8]>> {
        let mut vectors: Vec<IoVec<&mut [u8]>> = Vec::with_capacity(8);

        while size > 0 && self.index < self.mem_slice.len() {
            let slice = self.mem_slice[self.index];
            let this_left = slice.len() - self.offset;

            match this_left.cmp(&size) {
                cmp::Ordering::Greater => {
                    // Safe because self.offset is valid and we have checked `size`.
                    let p = unsafe { slice.as_ptr().add(self.offset) };
                    let s = unsafe { from_raw_parts_mut(p, size) };
                    vectors.push(IoVec::from_mut_slice(s));
                    self.offset += size;
                    break;
                }
                cmp::Ordering::Equal => {
                    // Safe because self.offset is valid and we have checked `size`.
                    let p = unsafe { slice.as_ptr().add(self.offset) };
                    let s = unsafe { from_raw_parts_mut(p, size) };
                    vectors.push(IoVec::from_mut_slice(s));
                    self.index += 1;
                    self.offset = 0;
                    break;
                }
                cmp::Ordering::Less => {
                    let p = unsafe { slice.as_ptr().add(self.offset) };
                    let s = unsafe { from_raw_parts_mut(p, this_left) };
                    vectors.push(IoVec::from_mut_slice(s));
                    self.index += 1;
                    self.offset = 0;
                    size -= this_left;
                }
            }
        }

        vectors
    }

    /// Get the inner `FileVolatileSlice` array.
    pub fn inner_slice(&self) -> &[FileVolatileSlice] {
        self.mem_slice
    }
}

/// A customized readahead function to ask kernel to fault in all pages from offset to end.
///
/// Call libc::readahead on every 128KB range because otherwise readahead stops at kernel bdi
/// readahead size which is 128KB by default.
pub fn readahead(fd: libc::c_int, mut offset: u64, end: u64) {
    offset = round_down_4k(offset);
    while offset < end {
        // Kernel default 128KB readahead size
        let count = std::cmp::min(128 << 10, end - offset);
        unsafe { libc::readahead(fd, offset as i64, count as usize) };
        offset += count;
    }
}

/// A customized buf allocator that avoids zeroing
pub fn alloc_buf(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    unsafe { buf.set_len(size) };
    buf
}

/// Check hash of data matches provided one
pub fn digest_check(data: &[u8], digest: &RafsDigest, digester: digest::Algorithm) -> bool {
    digest == &RafsDigest::from_buf(data, digester)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuse_backend_rs::transport::FileVolatileSlice;

    #[test]
    fn test_copyv() {
        let mut dst_buf1 = vec![0x0u8; 4];
        let mut dst_buf2 = vec![0x0u8; 4];
        let volatile_slice_1 =
            unsafe { FileVolatileSlice::new(dst_buf1.as_mut_ptr(), dst_buf1.len()) };
        let volatile_slice_2 =
            unsafe { FileVolatileSlice::new(dst_buf2.as_mut_ptr(), dst_buf2.len()) };
        let dst_bufs = [volatile_slice_1, volatile_slice_2];

        let src_buf_1 = vec![1u8, 2u8, 3u8];
        let src_buf_2 = vec![4u8, 5u8, 6u8];
        let src_bufs = vec![src_buf_1.as_slice(), src_buf_2.as_slice()];

        assert_eq!(
            copyv(&[Vec::<u8>::new(); 0], &dst_bufs, 0, 1, 1, 1).unwrap(),
            (0, (1, 1))
        );
        assert_eq!(
            copyv(&src_bufs, &dst_bufs, 0, 0, 1, 1).unwrap(),
            (0, (1, 1))
        );
        assert!(copyv(&src_bufs, &dst_bufs, 5, 1, 1, 1).is_err());
        assert!(copyv(&src_bufs, &dst_bufs, 0, 1, 2, 0).is_err());
        assert!(copyv(&src_bufs, &dst_bufs, 0, 1, 1, 3).is_err());

        assert_eq!(
            copyv(&src_bufs, &dst_bufs, 1, 5, 0, 0,).unwrap(),
            (5, (1, 1))
        );
        assert_eq!(dst_buf1[0], 2);
        assert_eq!(dst_buf1[1], 3);
        assert_eq!(dst_buf1[2], 4);
        assert_eq!(dst_buf1[3], 5);
        assert_eq!(dst_buf2[0], 6);

        assert_eq!(
            copyv(&src_bufs, &dst_bufs, 1, 3, 1, 0,).unwrap(),
            (3, (1, 3))
        );
        assert_eq!(dst_buf2[0], 2);
        assert_eq!(dst_buf2[1], 3);
        assert_eq!(dst_buf2[2], 4);

        assert_eq!(
            copyv(&src_bufs, &dst_bufs, 1, 3, 1, 1,).unwrap(),
            (3, (2, 0))
        );
        assert_eq!(dst_buf2[1], 2);
        assert_eq!(dst_buf2[2], 3);
        assert_eq!(dst_buf2[3], 4);

        assert_eq!(
            copyv(&src_bufs, &dst_bufs, 1, 6, 0, 3,).unwrap(),
            (5, (2, 0))
        );
        assert_eq!(dst_buf1[3], 2);
        assert_eq!(dst_buf2[0], 3);
        assert_eq!(dst_buf2[1], 4);
        assert_eq!(dst_buf2[2], 5);
        assert_eq!(dst_buf2[3], 6);
    }

    #[test]
    fn test_mem_slice_cursor_move() {
        let mut buf1 = vec![0x0u8; 2];
        let vs1 = unsafe { FileVolatileSlice::new(buf1.as_mut_ptr(), buf1.len()) };
        let mut buf2 = vec![0x0u8; 2];
        let vs2 = unsafe { FileVolatileSlice::new(buf2.as_mut_ptr(), buf2.len()) };
        let vs = [vs1, vs2];

        let mut cursor = MemSliceCursor::new(&vs);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 0);

        cursor.move_cursor(0);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 0);

        cursor.move_cursor(1);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 1);

        cursor.move_cursor(1);
        assert_eq!(cursor.index, 1);
        assert_eq!(cursor.offset, 0);

        cursor.move_cursor(1);
        assert_eq!(cursor.index, 1);
        assert_eq!(cursor.offset, 1);

        cursor.move_cursor(2);
        assert_eq!(cursor.index, 2);
        assert_eq!(cursor.offset, 0);

        cursor.move_cursor(1);
        assert_eq!(cursor.index, 2);
        assert_eq!(cursor.offset, 0);
    }

    #[test]
    fn test_mem_slice_cursor_consume() {
        let mut buf1 = vec![0x0u8; 2];
        let vs1 = unsafe { FileVolatileSlice::new(buf1.as_mut_ptr(), buf1.len()) };
        let mut buf2 = vec![0x0u8; 2];
        let vs2 = unsafe { FileVolatileSlice::new(buf2.as_mut_ptr(), buf2.len()) };
        let vs = [vs1, vs2];

        let mut cursor = MemSliceCursor::new(&vs);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 0);

        assert_eq!(cursor.consume(0).len(), 0);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 0);

        assert_eq!(cursor.consume(1).len(), 1);
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.offset, 1);

        assert_eq!(cursor.consume(2).len(), 2);
        assert_eq!(cursor.index, 1);
        assert_eq!(cursor.offset, 1);

        assert_eq!(cursor.consume(2).len(), 1);
        assert_eq!(cursor.index, 2);
        assert_eq!(cursor.offset, 0);

        assert_eq!(cursor.consume(2).len(), 0);
        assert_eq!(cursor.index, 2);
        assert_eq!(cursor.offset, 0);
    }
}
