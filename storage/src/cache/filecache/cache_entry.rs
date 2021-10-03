// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Result, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::slice;
use std::sync::Arc;

use nix::sys::uio;
use nix::unistd::dup;
use nydus_utils::digest;
use nydus_utils::metrics::{BlobcacheMetrics, Metric};
use tokio::runtime::Runtime;
use vm_memory::VolatileSlice;

use crate::backend::BlobReader;
use crate::cache::chunkmap::{BlobChunkMap, ChunkMap, DigestedChunkMap, IndexedChunkMap};
use crate::cache::filecache::FileCacheMgr;
use crate::cache::{BlobCache, BlobIoMergeState, BlobIoMerged, BlobIoSegment, BlobIoTag};
use crate::device::{
    BlobChunkInfo, BlobFeatures, BlobInfo, BlobIoChunk, BlobIoDesc, BlobIoVec, BlobObject,
    BlobPrefetchRequest,
};
use crate::utils::{alloc_buf, copyv, readv, MemSliceCursor};
use crate::{compress, StorageError, StorageResult, RAFS_DEFAULT_CHUNK_SIZE};
use fuse_backend_rs::api::filesystem::ZeroCopyWriter;

pub(crate) struct FileCacheEntry {
    blob_info: Arc<BlobInfo>,
    chunk_map: Arc<dyn ChunkMap>,
    metrics: Arc<BlobcacheMetrics>,
    reader: Arc<dyn BlobReader>,
    runtime: Arc<Runtime>,
    file: Arc<File>,
    size: u64,
    compressor: compress::Algorithm,
    digester: digest::Algorithm,
    // Whether `get_blob_object()` is supported.
    is_get_blob_object_supported: bool,
    // The compressed data instead of uncompressed data is cached if `compressed` is true.
    is_compressed: bool,
    // Whether direct chunkmap is used.
    is_direct_chunkmap: bool,
    // The blob is for an stargz image.
    is_stargz: bool,
    // Data from the file cache should be validated before use.
    need_validate: bool,
}

impl FileCacheEntry {
    pub fn new(
        mgr: &FileCacheMgr,
        blob_info: &Arc<BlobInfo>,
        runtime: Arc<Runtime>,
    ) -> Result<Self> {
        let blob_file_path = format!("{}/{}", mgr.work_dir, blob_info.blob_id());
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&blob_file_path)?;
        let (chunk_map, is_direct_chunkmap) =
            Self::create_chunk_map(mgr, blob_info, &blob_file_path)?;
        let reader = mgr
            .backend
            .get_reader(blob_info.blob_id())
            .map_err(|_e| eio!("failed to get blob reader"))?;

        // TODO: check blob size with blob.compressed_size()
        let size = Self::get_blob_size(&reader, blob_info)?;
        // TODO: prepare compression

        let is_get_blob_object_supported =
            !mgr.is_compressed && is_direct_chunkmap && !blob_info.is_stargz();

        Ok(FileCacheEntry {
            blob_info: blob_info.clone(),
            chunk_map,
            metrics: mgr.metrics.clone(),
            reader,
            runtime,
            file: Arc::new(file),
            size,
            compressor: blob_info.compressor(),
            digester: blob_info.digester(),
            is_get_blob_object_supported,
            is_compressed: mgr.is_compressed,
            is_direct_chunkmap,
            is_stargz: blob_info.is_stargz(),
            need_validate: mgr.validate,
        })
    }

    fn create_chunk_map(
        mgr: &FileCacheMgr,
        blob_info: &BlobInfo,
        blob_file: &str,
    ) -> Result<(Arc<dyn ChunkMap>, bool)> {
        let mut direct_chunkmap = true;
        // The builder now records the number of chunks in the blob table, so we can
        // use IndexedChunkMap as a chunk map, but for the old Nydus bootstrap, we
        // need downgrade to use DigestedChunkMap as a compatible solution.
        let chunk_map: Arc<dyn ChunkMap> = if mgr.disable_indexed_map
            || blob_info.is_stargz()
            || blob_info.has_feature(BlobFeatures::V5_NO_EXT_BLOB_TABLE)
        {
            direct_chunkmap = false;
            Arc::new(BlobChunkMap::from(DigestedChunkMap::new()))
        } else {
            Arc::new(BlobChunkMap::from(IndexedChunkMap::new(
                blob_file,
                blob_info.chunk_count(),
            )?))
        };

        Ok((chunk_map, direct_chunkmap))
    }

    fn get_blob_size(reader: &Arc<dyn BlobReader>, blob_info: &BlobInfo) -> Result<u64> {
        // Stargz blobs doesn't provide size information, so hacky!
        let size = if blob_info.is_stargz() {
            0
        } else {
            reader.blob_size().map_err(|e| einval!(e))?
        };

        Ok(size)
    }
}

impl BlobCache for FileCacheEntry {
    fn blob_id(&self) -> &str {
        self.blob_info.blob_id()
    }

    fn blob_size(&self) -> Result<u64> {
        Ok(self.size)
    }

    fn compressor(&self) -> compress::Algorithm {
        self.compressor
    }

    fn digester(&self) -> digest::Algorithm {
        self.digester
    }

    fn is_stargz(&self) -> bool {
        self.is_stargz
    }

    fn need_validate(&self) -> bool {
        self.need_validate
    }

    fn reader(&self) -> &dyn BlobReader {
        &*self.reader
    }

    fn get_blob_object(&self) -> Option<&dyn BlobObject> {
        if self.is_get_blob_object_supported {
            Some(self)
        } else {
            None
        }
    }

    fn is_chunk_ready(&self, chunk: &dyn BlobChunkInfo) -> bool {
        self.chunk_map.is_ready_nowait(chunk).unwrap_or(false)
    }

    fn prefetch(
        &self,
        prefetches: &[BlobPrefetchRequest],
        bios: &[BlobIoDesc],
    ) -> StorageResult<usize> {
        todo!()
    }

    fn stop_prefetch(&self) -> StorageResult<()> {
        todo!()
    }

    fn read(&self, iovec: &BlobIoVec, buffers: &[VolatileSlice]) -> Result<usize> {
        debug_assert!(iovec.validate());
        self.metrics.total.inc();

        /*
        // Try to get rid of effect from prefetch.
        if self.prefetch_ctx.is_working() {
            if let Some(ref limiter) = self.limiter {
                if let Some(v) = NonZeroU32::new(bufs.len() as u32) {
                    // Even fails in getting tokens, continue to read
                    limiter.check_n(v).unwrap_or(());
                }
            }
        }
         */

        // TODO: Single bio optimization here? So we don't have to involve other management
        // structures.
        self.read_iter(&iovec.bi_vec, buffers)
    }
}

impl AsRawFd for FileCacheEntry {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl BlobObject for FileCacheEntry {
    fn base_offset(&self) -> u64 {
        0
    }

    fn is_all_data_ready(&self) -> bool {
        if let Some(b) = self.chunk_map.as_bitmap() {
            b.is_bitmap_all_ready()
        } else {
            false
        }
    }

    fn fetch(&self, offset: u64, size: u64) -> Result<usize> {
        todo!()
    }

    fn read(&self, w: &mut dyn ZeroCopyWriter, offset: u64, size: u64) -> Result<usize> {
        todo!()
    }
}

impl FileCacheEntry {
    // There are some assumption applied to the `bios` passed to `read_iter()`.
    // - The blob address of chunks in `bios` are continuous.
    // - There is at most one user io request in the `bios`.
    // - The user io request may not aligned on chunk boundary.
    // - The user io request may partially consume data from the first and last chunk of user io
    //   request.
    // - Optionally there may be some prefetch/read amplify requests following the user io request.
    // - The optional prefetch/read amplify requests may be silently dropped.
    fn read_iter(&self, bios: &[BlobIoDesc], buffers: &[VolatileSlice]) -> Result<usize> {
        debug!("bios {:?}", bios);
        // Merge requests with continuous blob addresses.
        let requests = self
            .merge_requests_for_user(bios, RAFS_DEFAULT_CHUNK_SIZE as usize * 2)
            .ok_or_else(|| einval!("Empty bios list"))?;
        let mut state = FileIoMergeState::new();
        let mut cursor = MemSliceCursor::new(buffers);
        let mut total_read: usize = 0;

        for req in requests {
            debug!("A merged request {:?}", req);
            for (i, chunk) in req.chunks.iter().enumerate() {
                let is_ready = self.chunk_map.is_ready(chunk.as_base(), true)?;

                // Directly read data from the file cache into the user buffer iff:
                // - the chunk is ready in the file cache
                // - the data in the file cache is uncompressed.
                // - data validation is disabled
                if is_ready && !self.is_compressed && !self.need_validate {
                    // Silently drop prefetch requests.
                    if req.tags[i].is_user_io() {
                        state.push(
                            RegionType::CacheFast,
                            chunk.uncompress_offset(),
                            chunk.uncompress_size(),
                            req.tags[i].clone(),
                            None,
                        )?;
                    }
                } else if self.is_stargz || !self.is_direct_chunkmap || is_ready {
                    // Case to try loading data from cache
                    // - chunk is ready but data validation is needed.
                    // - direct chunk map is not used, so there may be data in the file cache but
                    //   the readiness flag has been lost.
                    // - special path for stargz blobs. An stargz blob is abstracted as a compressed
                    //   file cache always need validation.
                    if req.tags[i].is_user_io() {
                        state.push(
                            RegionType::CacheSlow,
                            chunk.uncompress_offset(),
                            chunk.uncompress_size(),
                            req.tags[i].clone(),
                            Some(req.chunks[i].clone()),
                        )?;
                    } else {
                        // On slow path, don't try to handle internal(read amplification) IO.
                        self.chunk_map.notify_ready(chunk.as_base());
                    }
                } else {
                    let tag = if let BlobIoTag::User(ref s) = req.tags[i] {
                        BlobIoTag::User(s.clone())
                    } else {
                        BlobIoTag::Internal(chunk.compress_offset())
                    };
                    // NOTE: Only this request region can steak more chunks from backend with user io.
                    state.push(
                        RegionType::Backend,
                        chunk.compress_offset(),
                        chunk.compress_size(),
                        tag,
                        Some(chunk.clone()),
                    )?;
                }
            }

            for r in &state.regions {
                use RegionType::*;

                total_read += match r.r#type {
                    CacheFast => self.dispatch_cache_fast(&mut cursor, r)?,
                    CacheSlow => self.dispatch_cache_slow(&mut cursor, r)?,
                    Backend => self.dispatch_backend(&mut cursor, r)?,
                }
            }

            state.reset();
        }

        Ok(total_read)
    }

    // Directly read data requested by user from the file cache into the user memory buffer.
    fn dispatch_cache_fast(&self, cursor: &mut MemSliceCursor, region: &Region) -> Result<usize> {
        let offset = region.blob_address + region.seg.offset as u64;
        let size = region.seg.len as usize;
        let iovec = cursor.consume(size);

        self.metrics.partial_hits.inc();
        readv(self.file.as_raw_fd(), &iovec, offset)
    }

    fn dispatch_cache_slow(&self, cursor: &mut MemSliceCursor, region: &Region) -> Result<usize> {
        let mut total_read = 0;

        for (i, c) in region.chunks.iter().enumerate() {
            let user_offset = if i == 0 { region.seg.offset } else { 0 };
            let size = std::cmp::min(
                c.uncompress_size() - user_offset,
                region.seg.len - total_read as u32,
            );
            total_read += self.read_single_chunk(c, user_offset, size, cursor)?;
        }

        Ok(total_read)
    }

    fn dispatch_backend(&self, mem_cursor: &mut MemSliceCursor, region: &Region) -> Result<usize> {
        if !region.has_user_io() {
            debug!("No user data");
            for c in &region.chunks {
                self.chunk_map.notify_ready(c.as_base());
            }
            return Ok(0);
        } else if region.chunks.len() == 0 {
            return Ok(0);
        }

        let blob_size = region.blob_len as usize;
        debug!("total backend data {}KB", blob_size / 1024);
        let mut chunks = self.read_chunks(region.blob_address, blob_size, &region.chunks)?;
        assert_eq!(region.chunks.len(), chunks.len());

        let mut chunk_buffers = Vec::with_capacity(region.chunks.len());
        let mut buffer_holder = Vec::with_capacity(region.chunks.len());
        for (i, v) in chunks.drain(..).enumerate() {
            let d = Arc::new(DataBuffer::Allocated(v));
            if region.tags[i] {
                buffer_holder.push(d.clone());
            }
            self.delay_persist(region.chunks[i].clone(), d);
        }
        for d in buffer_holder.iter() {
            chunk_buffers.push(d.as_ref().slice());
        }

        let total_read = copyv(
            &chunk_buffers,
            mem_cursor.mem_slice,
            region.seg.offset as usize,
            region.seg.len as usize,
            mem_cursor.index,
            mem_cursor.offset,
        )
        .map(|(n, _)| n)
        .map_err(|e| {
            error!("failed to copy from chunk buf to buf: {:?}", e);
            eio!(e)
        })?;
        mem_cursor.move_cursor(total_read);

        Ok(total_read)
    }

    fn delay_persist(&self, chunk_info: BlobIoChunk, buffer: Arc<DataBuffer>) {
        let delayed_chunk_map = self.chunk_map.clone();
        let file = self.file.clone();
        let offset = if self.is_compressed {
            chunk_info.compress_offset()
        } else {
            chunk_info.uncompress_offset()
        };

        self.runtime.spawn(async move {
            match Self::persist_chunk(file, offset, buffer.slice()) {
                Ok(_) => delayed_chunk_map
                    .set_ready(chunk_info.as_base())
                    .unwrap_or_else(|e| {
                        error!(
                            "Failed change caching state for chunk of offset {}, {:?}",
                            chunk_info.compress_offset(),
                            e
                        )
                    }),
                Err(e) => {
                    error!(
                        "Persist chunk of offset {} failed, {:?}",
                        chunk_info.compress_offset(),
                        e
                    );
                    delayed_chunk_map.notify_ready(chunk_info.as_base())
                }
            }
        });
    }

    /// Persist a single chunk into local blob cache file. We have to write to the cache
    /// file in unit of chunk size
    fn persist_chunk(file: Arc<File>, offset: u64, buffer: &[u8]) -> Result<()> {
        let fd = file.as_raw_fd();

        let n = loop {
            let ret = uio::pwrite(fd, buffer, offset as i64).map_err(|_| last_error!());
            match ret {
                Ok(nr_write) => {
                    trace!("write {}(offset={}) bytes to cache file", nr_write, offset);
                    break nr_write;
                }
                Err(err) => {
                    // Retry if the IO is interrupted by signal.
                    if err.kind() != ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
            }
        };

        if n != buffer.len() {
            Err(eio!("failed to write data to file cache"))
        } else {
            Ok(())
        }
    }

    fn read_single_chunk(
        &self,
        chunk: &BlobIoChunk,
        user_offset: u32,
        size: u32,
        mem_cursor: &mut MemSliceCursor,
    ) -> Result<usize> {
        debug!("single bio, blob offset {}", chunk.compress_offset());

        let buffer_holder;
        let d_size = chunk.uncompress_size() as usize;
        let mut d = DataBuffer::Allocated(alloc_buf(d_size));
        let is_ready = self.chunk_map.is_ready(chunk.as_base(), false)?;
        let try_cache = self.is_stargz || !self.is_direct_chunkmap || is_ready;

        let buffer = if try_cache && self.read_file_cache(chunk, d.mut_slice()).is_ok() {
            self.metrics.whole_hits.inc();
            self.chunk_map.set_ready(chunk.as_base())?;
            trace!(
                "recover blob cache {} {} offset {} size {}",
                chunk.id(),
                d_size,
                user_offset,
                size,
            );
            &d
        } else if !self.is_compressed {
            self.read_raw_chunk(chunk, d.mut_slice(), None)?;
            buffer_holder = Arc::new(d.to_owned());
            self.delay_persist(chunk.clone(), buffer_holder.clone());
            buffer_holder.as_ref()
        } else {
            let delayed_chunk_map = self.chunk_map.clone();
            let file = self.file.clone();
            let offset = chunk.compress_offset();
            let persist_compressed =
                |buffer: &[u8]| match Self::persist_chunk(file.clone(), offset, buffer) {
                    Ok(_) => {
                        delayed_chunk_map
                            .set_ready(chunk.as_base())
                            .unwrap_or_else(|e| error!("set ready failed, {}", e));
                    }
                    Err(e) => {
                        error!("Failed in writing compressed blob cache index, {}", e);
                        delayed_chunk_map.notify_ready(chunk.as_base())
                    }
                };
            self.read_raw_chunk(chunk, d.mut_slice(), Some(&persist_compressed))?;
            &d
        };

        let dst_buffers = mem_cursor.inner_slice();
        let read_size = copyv(
            &[buffer.slice()],
            dst_buffers,
            user_offset as usize,
            size as usize,
            mem_cursor.index,
            mem_cursor.offset,
        )
        .map(|r| r.0)
        .map_err(|e| {
            error!("failed to copy from chunk buf to buf: {:?}", e);
            eother!(e)
        })?;
        mem_cursor.move_cursor(read_size);

        Ok(read_size)
    }

    fn read_file_cache(&self, chunk: &BlobIoChunk, buffer: &mut [u8]) -> Result<()> {
        let offset = if self.is_compressed {
            chunk.compress_offset()
        } else {
            chunk.uncompress_offset()
        };

        let mut d;
        let raw_buffer = if self.is_compressed && !self.is_stargz {
            // Need to put compressed data into a temporary buffer so as to perform decompression.
            //
            // gzip is special that it doesn't carry compress_size, instead, we make an IO stream
            // out of the file cache. So no need for an internal buffer here.
            let c_size = chunk.compress_size() as usize;
            d = alloc_buf(c_size);
            d.as_mut_slice()
        } else {
            // We have this unsafe assignment as it can directly store data into call's buffer.
            unsafe { slice::from_raw_parts_mut(buffer.as_mut_ptr(), buffer.len()) }
        };

        let mut raw_stream = None;
        if self.is_stargz {
            debug!("using blobcache file offset {} as data stream", offset,);
            // FIXME: In case of multiple threads duplicating the same fd, they still share the
            // same file offset.
            let fd = dup(self.file.as_raw_fd()).map_err(|_| last_error!())?;
            let mut f = unsafe { File::from_raw_fd(fd) };
            f.seek(SeekFrom::Start(offset)).map_err(|_| last_error!())?;
            raw_stream = Some(f)
        } else {
            debug!(
                "reading blob cache file offset {} size {}",
                offset,
                raw_buffer.len()
            );
            let nr_read = uio::pread(self.file.as_raw_fd(), raw_buffer, offset as i64)
                .map_err(|_| last_error!())?;
            if nr_read == 0 || nr_read != raw_buffer.len() {
                return Err(einval!());
            }
        }

        // Try to validate data just fetched from backend inside.
        self.process_raw_chunk(chunk, raw_buffer, raw_stream, buffer, self.is_compressed)?;

        Ok(())
    }

    /*
    fn generate_merged_requests_for_prefetch(
        &self,
        bios: &mut [BlobIoDesc],
        tx: &mut spmc::Sender<MergedBackendRequest>,
        merging_size: usize,
    ) {
        let limiter = |merged_size: u32| {
            if let Some(ref limiter) = self.limiter {
                let cells = NonZeroU32::new(merged_size).unwrap();
                if let Err(e) = limiter
                    .check_n(cells)
                    .or_else(|_| block_on(limiter.until_n_ready(cells)))
                {
                    // `InsufficientCapacity` is the only possible error
                    // Have to give up to avoid dead-loop
                    error!("{}: give up rate-limiting", e);
                }
            }
        };

            bios.sort_by_key(|entry| entry.chunkinfo.compress_offset());

        self.merge_and_issue(bios, merging_size, true, &mut |mr: MergedBackendRequest| {
            limiter(mr.blob_size);
            // Safe to unwrap because channel won't be closed.
            tx.send(mr).unwrap();
        })
    }
    */

    fn merge_requests_for_user(
        &self,
        bios: &[BlobIoDesc],
        merging_size: usize,
    ) -> Option<Vec<BlobIoMerged>> {
        let mut requests: Vec<BlobIoMerged> = Vec::with_capacity(bios.len());

        BlobIoMergeState::merge_and_issue(bios, merging_size, |mr: BlobIoMerged| {
            requests.push(mr);
        });

        if requests.is_empty() {
            None
        } else {
            Some(requests)
        }
    }
}
//>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>

/// An enum to reuse existing buffers for IO operations, and CoW on demand.
#[allow(dead_code)]
enum DataBuffer {
    Reuse(ManuallyDrop<Vec<u8>>),
    Allocated(Vec<u8>),
}

impl DataBuffer {
    fn slice(&self) -> &[u8] {
        match self {
            Self::Reuse(data) => data.as_slice(),
            Self::Allocated(data) => data.as_slice(),
        }
    }

    fn mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Reuse(ref mut data) => data.as_mut_slice(),
            Self::Allocated(ref mut data) => data.as_mut_slice(),
        }
    }

    /// Make sure it owns the underlying memory buffer.
    fn to_owned(self) -> Self {
        if let DataBuffer::Reuse(data) = self {
            DataBuffer::Allocated((*data).to_vec())
        } else {
            self
        }
    }

    #[allow(dead_code)]
    unsafe fn from_mut_slice(buf: &mut [u8]) -> Self {
        DataBuffer::Reuse(ManuallyDrop::new(Vec::from_raw_parts(
            buf.as_mut_ptr(),
            buf.len(),
            buf.len(),
        )))
    }
}

#[derive(PartialEq, Debug)]
enum RegionStatus {
    Init,
    Open,
    Committed,
}

#[derive(PartialEq, Copy, Clone)]
enum RegionType {
    // Fast path to read data from the cache directly, no decompression and validation needed.
    CacheFast,
    // Slow path to read data from the cache, due to decompression or validation.
    CacheSlow,
    // Need to read data from storage backend.
    Backend,
}

impl RegionType {
    fn joinable(&self, other: Self) -> bool {
        *self == other
    }
}

/// A continuous region in cache file or backend storage/blob, it may contains several chunks.
struct Region {
    r#type: RegionType,
    status: RegionStatus,
    // For debug and trace purpose implying how many chunks are concatenated
    count: u32,

    chunks: Vec<BlobIoChunk>,
    tags: Vec<bool>,

    // The range [blob_address, blob_address + blob_len) specifies data to be read from backend.
    blob_address: u64,
    blob_len: u32,
    // The range specifying data to return to user.
    seg: BlobIoSegment,
}

impl Region {
    fn new(region_type: RegionType) -> Self {
        Region {
            r#type: region_type,
            status: RegionStatus::Init,
            count: 0,
            chunks: Vec::with_capacity(8),
            tags: Vec::with_capacity(8),
            blob_address: 0,
            blob_len: 0,
            seg: Default::default(),
        }
    }

    fn append(
        &mut self,
        start: u64,
        len: u32,
        tag: BlobIoTag,
        chunk: Option<BlobIoChunk>,
    ) -> StorageResult<()> {
        debug_assert!(self.status != RegionStatus::Committed);

        if self.status == RegionStatus::Init {
            self.status = RegionStatus::Open;
            self.blob_address = start;
            self.blob_len = len;
            self.count = 1;
        } else {
            debug_assert!(self.status == RegionStatus::Open);
            if self.blob_address + self.blob_len as u64 != start
                || start.checked_add(len as u64).is_none()
            {
                return Err(StorageError::NotContinuous);
            }
            self.blob_len += len;
            self.count += 1;
        }

        // Maintain information for user triggered IO requests.
        if let BlobIoTag::User(ref s) = tag {
            if self.seg.is_empty() {
                self.seg = BlobIoSegment::new(s.offset, s.len);
            } else {
                self.seg.append(s.offset, s.len);
            }
        }

        if let Some(c) = chunk {
            self.chunks.push(c);
            self.tags.push(tag.is_user_io());
        }

        Ok(())
    }

    fn has_user_io(&self) -> bool {
        !self.seg.is_empty()
    }
}

struct FileIoMergeState {
    regions: Vec<Region>,
}

impl FileIoMergeState {
    fn new() -> Self {
        FileIoMergeState {
            regions: Vec::with_capacity(8),
        }
    }

    fn push(
        &mut self,
        region_type: RegionType,
        start: u64,
        len: u32,
        tag: BlobIoTag,
        chunk: Option<BlobIoChunk>,
    ) -> Result<()> {
        if self.regions.len() == 0 || !self.joinable(region_type) {
            self.regions.push(Region::new(region_type));
        }

        let idx = self.regions.len() - 1;
        self.regions[idx]
            .append(start, len, tag, chunk)
            .map_err(|e| einval!(e))
    }

    fn reset(&mut self) {
        self.regions.truncate(0);
    }

    #[inline]
    fn joinable(&self, region_type: RegionType) -> bool {
        debug_assert!(self.regions.len() > 0);
        let idx = self.regions.len() - 1;

        self.regions[idx].r#type.joinable(region_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_buffer() {
        let mut buf1 = vec![0x1u8; 8];
        let buf2 = unsafe { DataBuffer::from_mut_slice(buf1.as_mut_slice()) };

        assert_eq!(buf2.slice()[1], 0x1);
        let mut buf2 = buf2.to_owned();
        buf2.mut_slice()[1] = 0x2;
        assert_eq!(buf1[1], 0x1);
    }

    #[test]
    fn test_region_type() {
        assert!(RegionType::CacheFast.joinable(RegionType::CacheFast));
        assert!(RegionType::CacheSlow.joinable(RegionType::CacheSlow));
        assert!(RegionType::Backend.joinable(RegionType::Backend));

        assert!(!RegionType::CacheFast.joinable(RegionType::CacheSlow));
        assert!(!RegionType::CacheFast.joinable(RegionType::Backend));
        assert!(!RegionType::CacheSlow.joinable(RegionType::CacheFast));
        assert!(!RegionType::CacheSlow.joinable(RegionType::Backend));
        assert!(!RegionType::Backend.joinable(RegionType::CacheFast));
        assert!(!RegionType::Backend.joinable(RegionType::CacheSlow));
    }

    #[test]
    fn test_region_new() {
        let region = Region::new(RegionType::CacheFast);

        assert_eq!(region.status, RegionStatus::Init);
        assert!(!region.has_user_io());
        assert!(region.seg.is_empty());
        assert_eq!(region.chunks.len(), 0);
        assert_eq!(region.tags.len(), 0);
        assert_eq!(region.blob_address, 0);
        assert_eq!(region.blob_len, 0);
    }

    #[test]
    fn test_region_append() {
        let mut region = Region::new(RegionType::CacheFast);

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x1800,
            len: 0x1800,
        });
        region.append(0x1000, 0x2000, tag, None).unwrap();
        assert_eq!(region.status, RegionStatus::Open);
        assert_eq!(region.blob_address, 0x1000);
        assert_eq!(region.blob_len, 0x2000);
        assert_eq!(region.chunks.len(), 0);
        assert_eq!(region.tags.len(), 0);
        assert!(!region.seg.is_empty());
        assert!(region.has_user_io());

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x4000,
            len: 0x2000,
        });
        region.append(0x4000, 0x2000, tag, None).unwrap_err();
        assert_eq!(region.status, RegionStatus::Open);
        assert_eq!(region.blob_address, 0x1000);
        assert_eq!(region.blob_len, 0x2000);
        assert_eq!(region.seg.offset, 0x1800);
        assert_eq!(region.seg.len, 0x1800);
        assert_eq!(region.chunks.len(), 0);
        assert_eq!(region.tags.len(), 0);
        assert!(region.has_user_io());

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x3000,
            len: 0x2000,
        });
        region.append(0x3000, 0x2000, tag, None).unwrap();
        assert_eq!(region.status, RegionStatus::Open);
        assert_eq!(region.blob_address, 0x1000);
        assert_eq!(region.blob_len, 0x4000);
        assert_eq!(region.seg.offset, 0x1800);
        assert_eq!(region.seg.len, 0x3800);
        assert_eq!(region.chunks.len(), 0);
        assert_eq!(region.tags.len(), 0);
        assert!(!region.seg.is_empty());
        assert!(region.has_user_io());
    }

    #[test]
    fn test_file_io_merge_state() {
        let mut state = FileIoMergeState::new();
        assert_eq!(state.regions.len(), 0);

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x1800,
            len: 0x1800,
        });
        state
            .push(RegionType::CacheFast, 0x1000, 0x2000, tag, None)
            .unwrap();
        assert_eq!(state.regions.len(), 1);

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x3000,
            len: 0x2000,
        });
        state
            .push(RegionType::CacheFast, 0x3000, 0x2000, tag, None)
            .unwrap();
        assert_eq!(state.regions.len(), 1);

        let tag = BlobIoTag::User(BlobIoSegment {
            offset: 0x5000,
            len: 0x2000,
        });
        state
            .push(RegionType::CacheSlow, 0x5000, 0x2000, tag, None)
            .unwrap();
        assert_eq!(state.regions.len(), 2);
    }
}
