// Copyright (C) 2022 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use tokio::runtime::Runtime;

use nydus_api::CacheConfigV2;
use nydus_utils::metrics::BlobcacheMetrics;

use crate::backend::BlobBackend;
use crate::cache::cachedfile::{FileCacheEntry, FileCacheMeta};
use crate::cache::state::{BlobStateMap, IndexedChunkMap};
use crate::cache::worker::{AsyncPrefetchConfig, AsyncWorkerMgr};
use crate::cache::{BlobCache, BlobCacheMgr};
use crate::device::{BlobFeatures, BlobInfo, BlobObject};
use crate::factory::BLOB_FACTORY;
use crate::meta::BLOB_META_FEATURE_ZRAN;
use crate::RAFS_DEFAULT_CHUNK_SIZE;

pub const FSCACHE_BLOBS_CHECK_NUM: u8 = 1;

/// An implementation of [BlobCacheMgr](../trait.BlobCacheMgr.html) to improve performance by
/// caching uncompressed blob with Linux fscache subsystem.
#[derive(Clone)]
pub struct FsCacheMgr {
    blobs: Arc<RwLock<HashMap<String, Arc<FileCacheEntry>>>>,
    backend: Arc<dyn BlobBackend>,
    metrics: Arc<BlobcacheMetrics>,
    prefetch_config: Arc<AsyncPrefetchConfig>,
    runtime: Arc<Runtime>,
    worker_mgr: Arc<AsyncWorkerMgr>,
    work_dir: String,
    need_validation: bool,
    blobs_check_count: Arc<AtomicU8>,
    closed: Arc<AtomicBool>,
}

impl FsCacheMgr {
    /// Create a new instance of `FileCacheMgr`.
    pub fn new(
        config: &CacheConfigV2,
        backend: Arc<dyn BlobBackend>,
        runtime: Arc<Runtime>,
        id: &str,
    ) -> Result<FsCacheMgr> {
        if config.cache_compressed {
            return Err(enosys!("fscache doesn't support compressed cache mode"));
        }

        let blob_cfg = config.get_fscache_config()?;
        let work_dir = blob_cfg.get_work_dir()?;
        let metrics = BlobcacheMetrics::new(id, work_dir);
        let prefetch_config: Arc<AsyncPrefetchConfig> = Arc::new((&config.prefetch).into());
        let worker_mgr = AsyncWorkerMgr::new(metrics.clone(), prefetch_config.clone())?;

        BLOB_FACTORY.start_mgr_checker();

        Ok(FsCacheMgr {
            blobs: Arc::new(RwLock::new(HashMap::new())),
            backend,
            metrics,
            prefetch_config,
            runtime,
            worker_mgr: Arc::new(worker_mgr),
            work_dir: work_dir.to_owned(),
            need_validation: config.cache_validate,
            blobs_check_count: Arc::new(AtomicU8::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    // Get the file cache entry for the specified blob object.
    fn get(&self, blob: &Arc<BlobInfo>) -> Option<Arc<FileCacheEntry>> {
        self.blobs.read().unwrap().get(blob.blob_id()).cloned()
    }

    // Create a file cache entry for the specified blob object if not present, otherwise
    // return the existing one.
    fn get_or_create_cache_entry(&self, blob: &Arc<BlobInfo>) -> Result<Arc<FileCacheEntry>> {
        if let Some(entry) = self.get(blob) {
            return Ok(entry);
        }

        let entry = FileCacheEntry::new_fs_cache(
            self,
            blob.clone(),
            self.prefetch_config.clone(),
            self.runtime.clone(),
            self.worker_mgr.clone(),
        )?;
        let entry = Arc::new(entry);
        let mut guard = self.blobs.write().unwrap();
        if let Some(entry) = guard.get(blob.blob_id()) {
            Ok(entry.clone())
        } else {
            guard.insert(blob.blob_id().to_owned(), entry.clone());
            self.metrics
                .underlying_files
                .lock()
                .unwrap()
                .insert(blob.blob_id().to_string());
            Ok(entry)
        }
    }
}

impl BlobCacheMgr for FsCacheMgr {
    fn init(&self) -> Result<()> {
        AsyncWorkerMgr::start(self.worker_mgr.clone())
    }

    fn destroy(&self) {
        if !self.closed.load(Ordering::Acquire) {
            self.closed.store(true, Ordering::Release);
            self.worker_mgr.stop();
            self.backend().shutdown();
            self.metrics.release().unwrap_or_else(|e| error!("{:?}", e));
        }
    }

    fn gc(&self, id: Option<&str>) -> bool {
        if let Some(blob_id) = id {
            self.blobs.write().unwrap().remove(blob_id);
        } else {
            let mut reclaim = Vec::new();
            let guard = self.blobs.write().unwrap();
            for (id, entry) in guard.iter() {
                if Arc::strong_count(entry) == 1 {
                    reclaim.push(id.to_owned());
                }
            }
            drop(guard);

            for key in reclaim.iter() {
                let mut guard = self.blobs.write().unwrap();
                if let Some(entry) = guard.get(key) {
                    if Arc::strong_count(entry) == 1 {
                        guard.remove(key);
                    }
                }
            }
        }

        self.blobs.read().unwrap().len() == 0
    }

    fn backend(&self) -> &(dyn BlobBackend) {
        self.backend.as_ref()
    }

    fn get_blob_cache(&self, blob_info: &Arc<BlobInfo>) -> Result<Arc<dyn BlobCache>> {
        self.get_or_create_cache_entry(blob_info)
            .map(|v| v as Arc<dyn BlobCache>)
    }

    fn check_stat(&self) {
        let guard = self.blobs.read().unwrap();

        let mut all_ready = true;
        for (_id, entry) in guard.iter() {
            if !entry.is_all_data_ready() {
                all_ready = false;
                break;
            }
        }

        // we should double check blobs stat, in case some blobs hadn't been created when we checked.
        if all_ready {
            if self.blobs_check_count.load(Ordering::Acquire) == FSCACHE_BLOBS_CHECK_NUM {
                self.worker_mgr.stop();
                self.metrics.data_all_ready.store(true, Ordering::Release);
            } else {
                self.blobs_check_count.fetch_add(1, Ordering::Acquire);
            }
        } else {
            self.blobs_check_count.store(0, Ordering::Release);
        }
    }
}

impl Drop for FsCacheMgr {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl FileCacheEntry {
    pub fn new_fs_cache(
        mgr: &FsCacheMgr,
        blob_info: Arc<BlobInfo>,
        prefetch_config: Arc<AsyncPrefetchConfig>,
        runtime: Arc<Runtime>,
        workers: Arc<AsyncWorkerMgr>,
    ) -> Result<Self> {
        if blob_info.has_feature(BlobFeatures::V5_NO_EXT_BLOB_TABLE) {
            return Err(einval!("fscache does not support Rafs v5 blobs"));
        }
        let file = blob_info
            .get_fscache_file()
            .ok_or_else(|| einval!("No fscache file associated with the blob_info"))?;

        let blob_file_path = format!("{}/{}", mgr.work_dir, blob_info.blob_id());
        let chunk_map = Arc::new(BlobStateMap::from(IndexedChunkMap::new(
            &blob_file_path,
            blob_info.chunk_count(),
            false,
        )?));
        let reader = mgr
            .backend
            .get_reader(blob_info.blob_id())
            .map_err(|_e| eio!("failed to get blob reader"))?;
        let blob_compressed_size = Self::get_blob_size(&reader, &blob_info)?;
        let meta = if blob_info.meta_ci_is_valid() {
            let meta = FileCacheMeta::new(blob_file_path, blob_info.clone(), Some(reader.clone()))?;
            Some(meta)
        } else {
            return Err(enosys!(
                "fscache doesn't support blobs without blob meta information"
            ));
        };
        let is_zran = blob_info.meta_flags() & BLOB_META_FEATURE_ZRAN != 0;

        Ok(FileCacheEntry {
            blob_info: blob_info.clone(),
            chunk_map,
            file,
            meta,
            metrics: mgr.metrics.clone(),
            prefetch_state: Arc::new(AtomicU32::new(0)),
            reader,
            runtime,
            workers,

            blob_compressed_size,
            blob_uncompressed_size: blob_info.uncompressed_size(),
            compressor: blob_info.compressor(),
            digester: blob_info.digester(),
            is_get_blob_object_supported: true,
            is_compressed: false,
            is_direct_chunkmap: true,
            is_legacy_stargz: blob_info.is_legacy_stargz(),
            is_zran,
            dio_enabled: true,
            need_validation: mgr.need_validation && !blob_info.is_legacy_stargz(),
            batch_size: RAFS_DEFAULT_CHUNK_SIZE,
            prefetch_config,
        })
    }
}
