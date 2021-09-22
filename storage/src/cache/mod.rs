// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! A blob cache layer over storage backend to improve performance.
//!
//! One of Rafs filesystem's goal is to support "on demand data loading". On demand loading may
//! help to speed up application/container startup, but it may also cause serious performance
//! penalty if all data chunks are retrieved from remoted backend storage. So cache layer is
//! introduced between Rafs filesystem and backend storage, which caches remote data onto local
//! storage and merge small data request into bigger request to improve network performance.
//!
//! There are several cache drivers implemented:
//! - [DummyCacheMgr](dummycache/struct.DummyCacheMgr.html): a dummy implementation of
//!   `BlobCacheMgr`, simply reporting each chunk as cached or not cached according to
//!   configuration.

use std::cmp;
use std::fmt::{Debug, Formatter};
use std::fs::File;
use std::io::Result;
use std::slice;
use std::sync::Arc;

use nydus_utils::digest;
use vm_memory::VolatileSlice;

use crate::backend::{BlobBackend, BlobReader};
use crate::device::{BlobChunkInfo, BlobInfo, BlobIoChunk, BlobIoDesc, BlobPrefetchRequest};
use crate::utils::{alloc_buf, digest_check};
use crate::{compress, StorageResult, RAFS_MAX_BLOCK_SIZE};

//pub mod blobcache;
pub mod chunkmap;
pub mod dummycache;

/// Timeout in milli-seconds to retrieve blob data from backend storage.
pub const SINGLE_INFLIGHT_WAIT_TIMEOUT: u64 = 2000;

/// A segment representing a continuous range in a chunk.
#[derive(Clone, Debug)]
pub struct ChunkSegment {
    /// Start position of the range within the chunk
    pub offset: u32,
    /// Size of the range within the chunk
    pub len: u32,
}

impl ChunkSegment {
    /// Create a new instance of `ChunkSegment`.
    pub fn new(offset: u32, len: u32) -> Self {
        Self { offset, len }
    }
}

/// Struct to maintain information about chunk IO operation.
#[derive(Clone, Debug)]
enum ChunkIoTag {
    /// Io requests to fulfill user requests.
    User(ChunkSegment),
    /// Io requests to fulfill internal requirements with (Chunk index, blob/compressed offset).
    Internal(Arc<BlobInfo>, u64),
}

/// Struct to merge multiple continuous chunk IO as one storage backend request.
///
/// For network based remote storage backend, such as Registry/OS, it may have limited IOPs
/// due to high request round-trip time, but have enough network bandwidth. In such cases,
/// it may help to improve performance by merging multiple continuous and small chunk IO
/// requests into one big backend request.
#[derive(Default, Clone)]
struct ChunkIoMerged {
    pub blob_info: Arc<BlobInfo>,
    pub blob_offset: u64,
    pub blob_size: u64,
    pub chunks: Vec<BlobIoChunk>,
    pub chunk_tags: Vec<ChunkIoTag>,
}

impl Debug for ChunkIoMerged {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("MergedBackendRequest")
            .field("blob id", &self.blob_info.blob_id())
            .field("blob offset", &self.blob_offset)
            .field("blob size", &self.blob_size)
            .field("chunk tags", &self.chunk_tags)
            .finish()
    }
}

impl ChunkIoMerged {
    pub fn new(blob_info: Arc<BlobInfo>, cki: BlobIoChunk, bio: &BlobIoDesc) -> Self {
        let blob_size = cki.compress_size() as u64;
        let blob_offset = cki.compress_offset();
        let tag = if bio.user_io {
            ChunkIoTag::User(ChunkSegment::new(bio.offset, bio.size as u32))
        } else {
            ChunkIoTag::Internal(blob_info.clone(), cki.compress_offset())
        };

        assert!(blob_offset.checked_add(blob_size).is_some());

        ChunkIoMerged {
            blob_info,
            blob_offset,
            blob_size,
            chunks: vec![cki],
            chunk_tags: vec![tag],
        }
    }

    pub fn merge(&mut self, cki: BlobIoChunk, bio: &BlobIoDesc) {
        assert_eq!(
            self.blob_offset.checked_add(self.blob_size),
            Some(cki.compress_offset())
        );
        self.blob_size += cki.compress_size() as u64;
        assert!(self.blob_offset.checked_add(self.blob_size).is_some());

        let tag = if bio.user_io {
            ChunkIoTag::User(ChunkSegment::new(bio.offset, bio.size as u32))
        } else {
            ChunkIoTag::Internal(self.blob_info.clone(), cki.compress_offset())
        };

        self.chunks.push(cki);
        self.chunk_tags.push(tag);
    }
}

/// Configuration information for blob data prefetching.
#[derive(Clone, Default)]
pub struct BlobPrefetchConfig {
    /// Whether to enable blob data prefetching.
    pub enable: bool,
    /// Number of data prefetching working threads.
    pub threads_count: usize,
    /// The maximum size of a merged IO request.
    pub merging_size: usize,
    /// Network bandwidth rate limit in unit of Bytes and Zero means no limit.
    pub bandwidth_rate: u32,
}

/// Trait representing a cache object for a blob on backend storage.
///
/// The caller may use the `BlobCache` trait to access blob data on backend storage, with an
/// optional intermediate cache layer to improve performance.
pub trait BlobCache: Send + Sync {
    /// Get size of the blob object.
    fn blob_size(&self) -> Result<u64>;

    /// Get data compression algorithm to handle chunks in the blob.
    fn compressor(&self) -> compress::Algorithm;

    /// Get message digest algorithm to handle chunks in the blob.
    fn digester(&self) -> digest::Algorithm;

    /// Get the [BlobReader](../backend/trait.BlobReader.html) to read data from storage backend.
    fn reader(&self) -> &dyn BlobReader;

    /// Check whether need to validate the data chunk by digest value.
    fn need_validate(&self) -> bool;

    /// Check whether data of a chunk has been cached and ready for use.
    fn is_chunk_ready(&self, chunk: &dyn BlobChunkInfo) -> bool;

    /// Start to prefetch requested data in background.
    fn prefetch(
        &self,
        prefetches: &[BlobPrefetchRequest],
        bios: &[BlobIoDesc],
    ) -> StorageResult<usize>;

    /// Stop prefetching blob data in background.
    fn stop_prefetch(&self) -> StorageResult<()>;

    /// Read chunk data described by the blob Io descriptors from the blob cache into the buffer.
    fn read(&self, bios: &[BlobIoDesc], bufs: &[VolatileSlice]) -> Result<usize>;

    /// Read multiple chunks from the blob cache in batch mode.
    ///
    /// This is an interface to optimize chunk data fetch performance by merging multiple continuous
    /// chunks into one backend request. Callers must ensure that chunks in `cki_set` covers a
    /// continuous range, and the range exactly matches [`blob_offset`..`blob_offset` + `blob_size`].
    /// Function `read_chunks()` returns one buffer containing decompressed chunk data for each
    /// entry in the `cki_set` array in corresponding order.
    ///
    /// This method returns success only if all requested data are successfully fetched.
    fn read_chunks(
        &self,
        blob_offset: u64,
        blob_size: usize,
        cki_set: &[BlobIoChunk],
    ) -> Result<Vec<Vec<u8>>> {
        // Read requested data from the backend by altogether.
        let mut c_buf = alloc_buf(blob_size);
        let nr_read = self
            .reader()
            .read(c_buf.as_mut_slice(), blob_offset)
            .map_err(|e| eio!(e))?;
        if nr_read != blob_size {
            return Err(eio!(format!(
                "request for {} bytes but got {} bytes",
                blob_size, nr_read
            )));
        }

        let mut last = blob_offset;
        let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(cki_set.len());
        for cki in cki_set {
            // Ensure BlobIoChunk is valid and continuous.
            let offset = cki.compress_offset();
            let size = cki.compress_size();
            let d_size = cki.decompress_size() as usize;
            if offset != last
                || offset - blob_offset > usize::MAX as u64
                || offset.checked_add(size as u64).is_none()
                || d_size as u64 > RAFS_MAX_BLOCK_SIZE
            {
                return Err(eio!("cki_set to read_chunks() is invalid"));
            }

            let offset_merged = (offset - blob_offset) as usize;
            let end_merged = offset_merged + size as usize;
            let buf = &c_buf[offset_merged..end_merged];
            let mut chunk = alloc_buf(d_size);

            self.process_raw_chunk(cki, buf, None, &mut chunk, cki.is_compressed())?;
            chunks.push(chunk);
            last = offset + size as u64;
        }

        Ok(chunks)
    }

    /// Read a whole chunk directly from the storage backend.
    ///
    /// The fetched chunk data may be compressed or not, which depends chunk information from `cki`.
    /// Moreover, chunk data from backend storage may be validated per user's configuration.
    /// Above is not redundant with blob cache's validation given IO path backend -> blobcache
    /// `raw_hook` provides caller a chance to read fetched compressed chunk data.
    fn read_raw_chunk(
        &self,
        cki: &BlobIoChunk,
        chunk: &mut [u8],
        raw_hook: Option<&dyn Fn(&[u8])>,
    ) -> Result<usize> {
        let mut d;
        let offset = cki.compress_offset();
        let raw_chunk = if cki.is_compressed() {
            // Need a scratch buffer to decompress compressed data.
            let max_size = self
                .blob_size()?
                .checked_sub(offset)
                .ok_or_else(|| einval!("chunk compressed offset is bigger than blob file size"))?;
            let max_size = cmp::min(max_size, usize::MAX as u64);
            let c_size = if self.compressor() == compress::Algorithm::GZip {
                compress::compute_compressed_gzip_size(chunk.len(), max_size as usize)
            } else {
                cki.compress_size() as usize
            };
            d = alloc_buf(c_size);
            d.as_mut_slice()
        } else {
            // We have this unsafe assignment as it can directly store data into call's buffer.
            unsafe { slice::from_raw_parts_mut(chunk.as_mut_ptr(), chunk.len()) }
        };

        let size = self.reader().read(raw_chunk, offset).map_err(|e| eio!(e))?;
        if size != raw_chunk.len() {
            return Err(eio!("storage backend returns less data than requested"));
        }

        self.process_raw_chunk(cki.as_base(), raw_chunk, None, chunk, cki.is_compressed())
            .map_err(|e| eio!(format!("fail to read from backend: {}", e)))?;
        if let Some(hook) = raw_hook {
            hook(raw_chunk)
        }

        Ok(chunk.len())
    }

    /// Hook point to post-process data received from storage backend.
    ///
    /// This hook method provides a chance to transform data received from storage backend into
    /// data cached on local disk.
    fn process_raw_chunk(
        &self,
        cki: &dyn BlobChunkInfo,
        raw_chunk: &[u8],
        raw_stream: Option<File>,
        chunk: &mut [u8],
        need_decompress: bool,
    ) -> Result<usize> {
        if need_decompress {
            compress::decompress(raw_chunk, raw_stream, chunk, self.compressor()).map_err(|e| {
                error!("failed to decompress chunk: {}", e);
                e
            })?;
        } else if raw_chunk.as_ptr() != chunk.as_ptr() {
            // raw_chunk and chunk may point to the same buffer, so only copy data when needed.
            chunk.copy_from_slice(raw_chunk);
        }

        let d_size = cki.decompress_size() as usize;
        if chunk.len() != d_size {
            Err(eio!("decompressed size and buffer size doesn't match"))
        } else if self.need_validate() && !digest_check(chunk, cki.chunk_id(), self.digester()) {
            Err(eio!("data digest value doesn't match"))
        } else {
            Ok(d_size)
        }
    }
}

/// Trait representing blob manager to manage a group of [BlobCache](trait.BlobCache.html) objects.
///
/// The main responsibility of the blob cache manager is to create blob cache objects for blobs,
/// all IO requests should be issued to the blob cache object directly.
pub trait BlobCacheMgr: Send + Sync {
    /// Initialize the blob cache manager.
    fn init(&self) -> Result<()>;

    /// Tear down the blob cache manager.
    fn destroy(&self);

    /// Get the underlying `BlobBackend` object of the blob cache object.
    fn backend(&self) -> &(dyn BlobBackend);

    /// Get the blob cache to provide access to the `blob` object.
    fn get_blob_cache(&self, blob: BlobInfo) -> Result<Arc<dyn BlobCache>>;
}

/// Blob cache manager to access Rafs V5 images.
pub mod v5 {

    use super::*;
    use crate::device::v5::BlobV5ChunkInfo;

    /// Trait representing a blob cache manager to access Rafs V5 images.
    pub trait BlobV5Cache: BlobCacheMgr {
        //<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<
        /// Get data compression algorithm used by the underlying blob.
        fn compressor(&self) -> compress::Algorithm;

        /// Get message digest algorithm used by the underlying blob.
        fn digester(&self) -> digest::Algorithm;

        /// Check whether need to validate the data chunk.
        fn need_validate(&self) -> bool;

        /// Get size of the blob object.
        fn blob_size(&self, blob: &BlobInfo) -> Result<u64>;

        /// Check whether data of a chunk has been cached.
        fn is_chunk_cached(&self, chunk: &dyn BlobV5ChunkInfo, blob: &BlobInfo) -> bool;

        /// Check whether data of a chunk has been cached and ready for use.
        fn prefetch(&self, bio: &mut [BlobIoDesc]) -> StorageResult<usize>;

        /// Stop prefetching blob data.
        fn stop_prefetch(&self) -> StorageResult<()>;
        //>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>

        //<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<
        /// Read chunk data described by `bio` from the blob into the `bufs`.
        ///
        /// This method should only used to serve RAFS v4/v5 data blobs only because it depends on
        /// the RAFS v4/v5 filesystem metadata information to serve the request.
        //
        // TODO: Cache is indexed by each chunk's block id. When this read request can't
        // hit local cache and it spans two chunks, group more than one requests to backend
        // storage could benefit the performance.
        fn read(&self, bio: &mut [BlobIoDesc], bufs: &[VolatileSlice]) -> Result<usize>;

        /// Read multiple full chunks from the backend storage in batch.
        ///
        /// Callers must ensure that chunks in `cki_set` covers a continuous range, and the range
        /// exactly matches [`blob_offset`..`blob_offset` + `blob_size`].
        /// Function `read_chunks()` returns one buffer containing decompressed chunk data for each
        /// entry in the `cki_set` array in corresponding order.
        fn read_chunks(
            &self,
            blob_id: &str,
            blob_offset: u64,
            blob_size: usize,
            cki_set: &[Arc<dyn BlobV5ChunkInfo>],
        ) -> Result<Vec<Vec<u8>>> {
            // TODO: Also check if sorted and continuous here?

            let mut c_buf = alloc_buf(blob_size);
            let nr_read = self
                .backend()
                .read(blob_id, c_buf.as_mut_slice(), blob_offset)
                .map_err(|e| eio!(e))?;
            if nr_read != blob_size {
                return Err(eio!(format!(
                    "request for {} bytes but got {} bytes",
                    blob_size, nr_read
                )));
            }

            let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(cki_set.len());
            for cki in cki_set {
                let offset_merged = (cki.compress_offset() - blob_offset) as usize;
                let size_merged = cki.compress_size() as usize;
                let buf = &c_buf[offset_merged..(offset_merged + size_merged)];
                let mut chunk = alloc_buf(cki.decompress_size() as usize);

                self.process_raw_chunk(
                    cki.as_ref(),
                    buf,
                    None,
                    &mut chunk,
                    cki.is_compressed(),
                    self.need_validate(),
                )?;
                chunks.push(chunk);
            }

            Ok(chunks)
        }

        /// Read a whole chunk directly from the storage backend.
        ///
        /// The fetched chunk data may be compressed or not, which depends chunk information from `cki`.
        /// Moreover, chunk data from backend storage may be validated per user's configuration.
        /// Above is not redundant with blob cache's validation given IO path backend -> blobcache
        /// `raw_hook` provides caller a chance to read fetched compressed chunk data.
        fn read_backend_chunk(
            &self,
            blob: &BlobInfo,
            cki: &dyn BlobV5ChunkInfo,
            chunk: &mut [u8],
            raw_hook: Option<&dyn Fn(&[u8])>,
        ) -> Result<usize> {
            let mut d;
            let offset = cki.compress_offset();
            let raw_chunk = if cki.is_compressed() {
                // Need a scratch buffer to decompress compressed data.
                let max_size = self.blob_size(blob)?.checked_sub(offset).ok_or_else(|| {
                    einval!("chunk compressed offset is bigger than blob file size")
                })?;
                let max_size = cmp::min(max_size, usize::MAX as u64);
                let c_size = if self.compressor() == compress::Algorithm::GZip {
                    compress::compute_compressed_gzip_size(chunk.len(), max_size as usize)
                } else {
                    cki.compress_size() as usize
                };
                d = alloc_buf(c_size);
                d.as_mut_slice()
            } else {
                // We have this unsafe assignment as it can directly store data into call's buffer.
                unsafe { slice::from_raw_parts_mut(chunk.as_mut_ptr(), chunk.len()) }
            };

            let size = self
                .backend()
                .read(blob.blob_id(), raw_chunk, offset)
                .map_err(|e| eio!(e))?;

            if size != raw_chunk.len() {
                return Err(eio!("storage backend returns less data than requested"));
            }
            self.process_raw_chunk(
                cki,
                raw_chunk,
                None,
                chunk,
                cki.is_compressed(),
                self.need_validate(),
            )
            .map_err(|e| eio!(format!("fail to read from backend: {}", e)))?;
            if let Some(hook) = raw_hook {
                hook(raw_chunk)
            }

            Ok(chunk.len())
        }
        //>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>

        /// Before storing chunk data into blob cache file. We have cook the raw chunk from
        /// backend a bit as per the chunk description as blob cache always saves plain data
        /// into cache file rather than compressed.
        /// An inside trick is that it tries to directly save data into caller's buffer.
        fn process_raw_chunk(
            &self,
            cki: &dyn BlobV5ChunkInfo,
            raw_chunk: &[u8],
            raw_stream: Option<File>,
            chunk: &mut [u8],
            need_decompress: bool,
            need_validate: bool,
        ) -> Result<usize> {
            if need_decompress {
                compress::decompress(raw_chunk, raw_stream, chunk, self.compressor()).map_err(
                    |e| {
                        error!("failed to decompress chunk: {}", e);
                        e
                    },
                )?;
            } else if raw_chunk.as_ptr() != chunk.as_ptr() {
                // Sometimes, caller directly put data into consumer provided buffer.
                // Then we don't have to copy data between slices.
                chunk.copy_from_slice(raw_chunk);
            }

            let d_size = cki.decompress_size() as usize;
            if chunk.len() != d_size {
                return Err(eio!());
            }
            if need_validate && !digest_check(chunk, cki.chunk_id(), self.digester()) {
                return Err(eio!());
            }

            Ok(d_size)
        }
    }
}
