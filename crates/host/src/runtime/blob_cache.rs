//! A disk cache in front of any blob store. A blob never changes, so a cached
//! chunk never goes stale: nothing is ever invalidated, only evicted.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::blob::{BlobError, BlobRef, BlobStore, NewBlob};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub dir: PathBuf,
    /// Bytes the cache keeps on disk before it drops the least recently used.
    pub max_bytes: u64,
    /// Bytes per cached chunk.
    pub chunk: u64,
}

impl CacheConfig {
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        CacheConfig {
            dir: dir.into(),
            max_bytes,
            chunk: 4 << 20,
        }
    }
}

/// Reads whole chunks of blobs from `inner` and keeps them on disk, at
/// `<dir>/<tenant hash>/<blob id>/<chunk>`. Several processes may share the
/// directory: a chunk lands by rename, so a reader sees all of it or none,
/// and two processes that miss together both fetch it.
pub struct CachedBlobStore {
    inner: Arc<dyn BlobStore>,
    config: CacheConfig,
    index: Mutex<Index>,
    fetching: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    warned: AtomicBool,
}

#[derive(Default)]
struct Index {
    chunks: HashMap<PathBuf, Entry>,
    bytes: u64,
    clock: u64,
}

struct Entry {
    size: u64,
    used: u64,
}

impl Index {
    fn add(&mut self, path: PathBuf, size: u64) {
        self.clock += 1;
        let used = self.clock;
        if let Some(old) = self.chunks.insert(path, Entry { size, used }) {
            self.bytes -= old.size;
        }
        self.bytes += size;
    }

    fn touch(&mut self, path: &Path) -> bool {
        self.clock += 1;
        let clock = self.clock;
        self.chunks.get_mut(path).map(|e| e.used = clock).is_some()
    }

    fn forget(&mut self, path: &Path) {
        if let Some(old) = self.chunks.remove(path) {
            self.bytes -= old.size;
        }
    }

    /// The least recently used chunks to drop until the cache fits `max`.
    fn over(&mut self, max: u64) -> Vec<PathBuf> {
        if self.bytes <= max {
            return Vec::new();
        }
        let mut by_age: Vec<(u64, PathBuf, u64)> = self
            .chunks
            .iter()
            .map(|(p, e)| (e.used, p.clone(), e.size))
            .collect();
        by_age.sort();
        let mut dropped = Vec::new();
        for (_, path, size) in by_age {
            if self.bytes <= max {
                break;
            }
            self.chunks.remove(&path);
            self.bytes -= size;
            dropped.push(path);
        }
        dropped
    }
}

impl CachedBlobStore {
    /// Opens the cache over `inner`, and reads what an earlier process left
    /// in `config.dir`.
    pub fn new(inner: Arc<dyn BlobStore>, config: CacheConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let index = Mutex::new(scan(&config.dir));
        Ok(CachedBlobStore {
            inner,
            config,
            index,
            fetching: Mutex::default(),
            warned: AtomicBool::new(false),
        })
    }

    /// Drops every chunk of `tenant_id`'s blobs, as when its blobs are purged.
    pub async fn purge_tenant(&self, tenant_id: &str) -> std::io::Result<()> {
        let dir = self.config.dir.join(tenant_dir(tenant_id));
        {
            let mut index = self.index.lock().expect("cache index");
            let gone: Vec<PathBuf> = index
                .chunks
                .keys()
                .filter(|p| p.starts_with(&dir))
                .cloned()
                .collect();
            for path in gone {
                index.forget(&path);
            }
        }
        match tokio::fs::remove_dir_all(&dir).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn path(&self, r: &BlobRef, chunk: u64) -> Option<PathBuf> {
        let safe = !r.id.is_empty() && r.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        safe.then(|| {
            self.config
                .dir
                .join(tenant_dir(&r.tenant_id))
                .join(&r.id)
                .join(chunk.to_string())
        })
    }

    async fn chunk(&self, r: &BlobRef, chunk: u64) -> Result<Vec<u8>, BlobError> {
        let start = chunk * self.config.chunk;
        let end = (start + self.config.chunk).min(r.size);
        let Some(path) = self.path(r, chunk) else {
            return self.inner.read(r, start..end).await;
        };
        if let Some(bytes) = self.cached(&path).await {
            return Ok(bytes);
        }
        let flight = self
            .fetching
            .lock()
            .expect("cache flights")
            .entry(path.clone())
            .or_default()
            .clone();
        let _guard = flight.lock().await;
        let bytes = match self.cached(&path).await {
            Some(bytes) => Ok(bytes),
            None => {
                let bytes = self.inner.read(r, start..end).await;
                if let Ok(bytes) = &bytes {
                    self.keep(&path, bytes).await;
                }
                bytes
            }
        };
        self.fetching.lock().expect("cache flights").remove(&path);
        bytes
    }

    async fn cached(&self, path: &Path) -> Option<Vec<u8>> {
        let known = self.index.lock().expect("cache index").touch(path);
        if !known {
            return None;
        }
        match tokio::fs::read(path).await {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                self.index.lock().expect("cache index").forget(path);
                None
            }
        }
    }

    async fn keep(&self, path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
        let written = async {
            tokio::fs::create_dir_all(path.parent().expect("a chunk has a parent")).await?;
            tokio::fs::write(&tmp, bytes).await?;
            tokio::fs::rename(&tmp, path).await
        }
        .await;
        if let Err(error) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(%error, dir = %self.config.dir.display(), "the blob cache could not write a chunk; reads go to the store");
            }
            return;
        }
        let dropped = {
            let mut index = self.index.lock().expect("cache index");
            index.add(path.to_path_buf(), bytes.len() as u64);
            index.over(self.config.max_bytes)
        };
        for old in dropped {
            let _ = tokio::fs::remove_file(old).await;
        }
    }
}

#[async_trait]
impl BlobStore for CachedBlobStore {
    async fn put(&self, blob: NewBlob) -> Result<BlobRef, BlobError> {
        self.inner.put(blob).await
    }

    async fn get(&self, r: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.read(r, 0..r.size).await
    }

    async fn read(&self, r: &BlobRef, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
        let end = range.end.min(r.size);
        if range.start >= end {
            return Ok(Vec::new());
        }
        let size = self.config.chunk;
        let mut out = Vec::with_capacity((end - range.start) as usize);
        for chunk in range.start / size..=(end - 1) / size {
            let bytes = self.chunk(r, chunk).await?;
            let base = chunk * size;
            let from = range.start.saturating_sub(base) as usize;
            let to = ((end - base) as usize).min(bytes.len());
            out.extend_from_slice(&bytes[from.min(to)..to]);
        }
        Ok(out)
    }
}

fn tenant_dir(tenant_id: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(tenant_id.as_bytes()))
}

/// The chunks on disk, oldest first by modified time, and stray temp files
/// removed.
fn scan(dir: &Path) -> Index {
    let mut found = Vec::new();
    let entries = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten();
    for tenant in entries(dir) {
        for blob in entries(&tenant.path()) {
            for chunk in entries(&blob.path()) {
                let path = chunk.path();
                if path.extension().is_some_and(|e| e == "tmp") {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if let Ok(meta) = chunk.metadata() {
                    let modified = meta.modified().ok();
                    found.push((modified, path, meta.len()));
                }
            }
        }
    }
    found.sort_by_key(|a| a.0);
    let mut index = Index::default();
    for (_, path, size) in found {
        index.add(path, size);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A store that counts the bytes it serves.
    struct Counting {
        inner: super::super::blob::MemoryBlobStore,
        served: AtomicUsize,
    }

    #[async_trait]
    impl BlobStore for Counting {
        async fn put(&self, blob: NewBlob) -> Result<BlobRef, BlobError> {
            self.inner.put(blob).await
        }
        async fn get(&self, r: &BlobRef) -> Result<Vec<u8>, BlobError> {
            self.inner.get(r).await
        }
        async fn read(&self, r: &BlobRef, range: Range<u64>) -> Result<Vec<u8>, BlobError> {
            let bytes = self.inner.read(r, range).await?;
            self.served.fetch_add(bytes.len(), Ordering::Relaxed);
            Ok(bytes)
        }
    }

    fn store() -> Arc<Counting> {
        Arc::new(Counting {
            inner: super::super::blob::MemoryBlobStore::new(),
            served: AtomicUsize::new(0),
        })
    }

    async fn blob(store: &Arc<Counting>, tenant: &str, bytes: Vec<u8>) -> BlobRef {
        store
            .put(NewBlob {
                tenant_id: tenant.to_string(),
                mime: "text/plain".to_string(),
                name: None,
                bytes,
            })
            .await
            .unwrap()
    }

    fn config(dir: &Path, max_bytes: u64) -> CacheConfig {
        CacheConfig {
            dir: dir.to_path_buf(),
            max_bytes,
            chunk: 1024,
        }
    }

    #[tokio::test]
    async fn a_range_is_read_once_then_served_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let inner = store();
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let r = blob(&inner, "t1", bytes.clone()).await;
        let cache = CachedBlobStore::new(inner.clone(), config(dir.path(), 1 << 20)).unwrap();
        assert_eq!(cache.read(&r, 1500..4100).await.unwrap(), bytes[1500..4100]);
        let first = inner.served.load(Ordering::Relaxed);
        assert_eq!(first, 4 * 1024, "the four chunks the range touches");
        assert_eq!(cache.read(&r, 2000..3000).await.unwrap(), bytes[2000..3000]);
        assert_eq!(
            inner.served.load(Ordering::Relaxed),
            first,
            "no second fetch"
        );
        assert_eq!(cache.get(&r).await.unwrap(), bytes);
        assert_eq!(cache.read(&r, 9_999..20_000).await.unwrap(), bytes[9_999..]);
    }

    #[tokio::test]
    async fn the_cache_outlives_the_process_and_keeps_to_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let inner = store();
        let r = blob(&inner, "t1", vec![7; 8 * 1024]).await;
        {
            let cache = CachedBlobStore::new(inner.clone(), config(dir.path(), 3 * 1024)).unwrap();
            cache.get(&r).await.unwrap();
            assert!(cache.index.lock().unwrap().bytes <= 3 * 1024);
        }
        let served = inner.served.load(Ordering::Relaxed);
        let cache = CachedBlobStore::new(inner.clone(), config(dir.path(), 3 * 1024)).unwrap();
        cache.read(&r, 7 * 1024..8 * 1024).await.unwrap();
        assert_eq!(
            inner.served.load(Ordering::Relaxed),
            served,
            "the last chunk was still on disk"
        );
    }

    #[tokio::test]
    async fn purging_a_tenant_leaves_none_of_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let inner = store();
        let mine = blob(&inner, "../../etc", vec![1; 2048]).await;
        let theirs = blob(&inner, "t2", vec![2; 2048]).await;
        let cache = CachedBlobStore::new(inner.clone(), config(dir.path(), 1 << 20)).unwrap();
        cache.get(&mine).await.unwrap();
        cache.get(&theirs).await.unwrap();
        assert!(
            std::fs::read_dir(dir.path()).unwrap().count() == 2,
            "a tenant is a hashed directory inside the cache"
        );
        cache.purge_tenant("../../etc").await.unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
        assert_eq!(cache.get(&theirs).await.unwrap(), vec![2; 2048]);
    }
}
