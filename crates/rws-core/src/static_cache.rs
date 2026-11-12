use crate::{config::StaticCacheConfig, metrics::Metrics};
use axum::{body::Bytes, http::HeaderValue};
use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileFingerprint {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl FileFingerprint {
    fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            length: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct CachedAsset {
    pub body: Bytes,
    pub content_type: HeaderValue,
    pub content_length: HeaderValue,
}

impl CachedAsset {
    fn new(path: &Path, body: Bytes) -> Self {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        Self {
            content_type: HeaderValue::from_str(mime.as_ref())
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            content_length: HeaderValue::from_str(&body.len().to_string())
                .expect("numeric content length"),
            body,
        }
    }
}

struct CacheEntry {
    asset: CachedAsset,
    fingerprint: FileFingerprint,
    last_used: AtomicU64,
    valid_until_ns: AtomicU64,
}

#[derive(Default)]
struct ShardState {
    sites: HashMap<usize, HashMap<String, Arc<CacheEntry>>>,
    total_bytes: usize,
    total_entries: usize,
}

struct CacheShard {
    state: RwLock<ShardState>,
    max_bytes: usize,
    max_entries: usize,
}

pub(crate) enum FastLookup {
    Hit(CachedAsset),
    Validate,
    Miss,
}

pub struct StaticCache {
    config: StaticCacheConfig,
    shards: Vec<CacheShard>,
    hash_builder: RandomState,
    started: Instant,
    clock: AtomicU64,
    metrics: Arc<Metrics>,
}

impl StaticCache {
    pub fn new(config: StaticCacheConfig, metrics: Arc<Metrics>) -> Self {
        let shard_count = if config.enabled {
            let shards_allowed_by_file_size = config.max_total_bytes / config.max_file_bytes;
            16.min(config.max_entries)
                .min(config.max_total_bytes)
                .min(shards_allowed_by_file_size.max(1))
        } else {
            1
        };
        let shards = (0..shard_count)
            .map(|index| CacheShard {
                state: RwLock::new(ShardState::default()),
                max_bytes: quota(config.max_total_bytes, shard_count, index),
                max_entries: quota(config.max_entries, shard_count, index),
            })
            .collect();
        Self {
            config,
            shards,
            hash_builder: RandomState::new(),
            started: Instant::now(),
            clock: AtomicU64::new(1),
            metrics,
        }
    }

    pub(crate) fn fast_lookup(&self, site_id: usize, request_path: &str) -> FastLookup {
        if !self.config.enabled {
            return FastLookup::Miss;
        }
        let shard = self.shard(site_id, request_path);
        let cached = {
            let state = shard.state.read().expect("static cache read lock poisoned");
            state
                .sites
                .get(&site_id)
                .and_then(|entries| entries.get(request_path))
                .cloned()
        };
        let Some(entry) = cached else {
            return FastLookup::Miss;
        };
        entry.last_used.store(self.next_clock(), Ordering::Relaxed);

        let interval_ns = self.revalidation_interval_ns();
        if interval_ns == 0 {
            self.record_revalidation();
            return FastLookup::Validate;
        }
        let now = self.elapsed_ns();
        let valid_until = entry.valid_until_ns.load(Ordering::Acquire);
        if now >= valid_until
            && entry
                .valid_until_ns
                .compare_exchange(
                    valid_until,
                    now.saturating_add(interval_ns),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            self.record_revalidation();
            return FastLookup::Validate;
        }

        self.metrics
            .static_cache_hits_total
            .fetch_add(1, Ordering::Relaxed);
        FastLookup::Hit(entry.asset.clone())
    }

    pub(crate) async fn get_or_load(
        &self,
        site_id: usize,
        request_path: &str,
        path: &Path,
        mut file: tokio::fs::File,
        metadata: &std::fs::Metadata,
    ) -> std::io::Result<CachedAsset> {
        if !self.config.enabled {
            return self.bypass(path, &mut file, metadata.len()).await;
        }
        let Some(mut fingerprint) = FileFingerprint::from_metadata(metadata) else {
            return self.bypass(path, &mut file, metadata.len()).await;
        };
        let mut counted_miss = false;
        for attempt in 0..2 {
            if !self.cacheable_size(fingerprint.length) {
                return self.bypass(path, &mut file, metadata.len()).await;
            }
            if let Some(asset) = self.lookup_validated(site_id, request_path, fingerprint) {
                return Ok(asset);
            }
            if !counted_miss {
                self.metrics
                    .static_cache_misses_total
                    .fetch_add(1, Ordering::Relaxed);
                counted_miss = true;
            }

            file.rewind().await?;
            let mut data = Vec::new();
            (&mut file)
                .take(fingerprint.length.saturating_add(1))
                .read_to_end(&mut data)
                .await?;
            if data.len() as u64 > fingerprint.length {
                return Err(std::io::Error::other("file grew during cache read"));
            }
            let body = Bytes::from(data);
            let after = file.metadata().await?;
            let after_fingerprint = FileFingerprint::from_metadata(&after);
            if after_fingerprint == Some(fingerprint) && body.len() as u64 == fingerprint.length {
                let asset = CachedAsset::new(path, body);
                return Ok(self.insert(site_id, request_path.to_owned(), fingerprint, asset));
            }
            if attempt == 1 {
                self.metrics
                    .static_cache_bypasses_total
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(CachedAsset::new(path, body));
            }
            let Some(after_fingerprint) = after_fingerprint else {
                self.metrics
                    .static_cache_bypasses_total
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(CachedAsset::new(path, body));
            };
            fingerprint = after_fingerprint;
        }
        unreachable!("bounded cache load loop always returns")
    }

    pub(crate) fn invalidate(&self, site_id: usize, request_path: &str) {
        if !self.config.enabled {
            return;
        }
        let shard = self.shard(site_id, request_path);
        let mut state = shard
            .state
            .write()
            .expect("static cache write lock poisoned");
        if let Some(removed) = remove_entry(&mut state, site_id, request_path) {
            self.adjust_gauges(removed.asset.body.len(), 1, 0, 0);
            self.metrics
                .static_cache_invalidations_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn validate_metadata(
        &self,
        site_id: usize,
        request_path: &str,
        metadata: &std::fs::Metadata,
    ) {
        if let Some(fingerprint) = FileFingerprint::from_metadata(metadata) {
            let _ = self.lookup_validated(site_id, request_path, fingerprint);
        } else {
            self.invalidate(site_id, request_path);
        }
    }

    pub(crate) fn should_stream(&self, length: u64) -> bool {
        usize::try_from(length).map_or(true, |length| {
            length > self.config.max_file_bytes || length > self.config.max_total_bytes
        })
    }

    pub(crate) fn record_bypass(&self) {
        self.metrics
            .static_cache_bypasses_total
            .fetch_add(1, Ordering::Relaxed);
    }

    async fn bypass(
        &self,
        path: &Path,
        file: &mut tokio::fs::File,
        length: u64,
    ) -> std::io::Result<CachedAsset> {
        self.record_bypass();
        let mut data = Vec::new();
        file.rewind().await?;
        file.take(length.saturating_add(1))
            .read_to_end(&mut data)
            .await?;
        if data.len() as u64 > length {
            return Err(std::io::Error::other("file grew during read"));
        }
        let body = Bytes::from(data);
        Ok(CachedAsset::new(path, body))
    }

    fn lookup_validated(
        &self,
        site_id: usize,
        request_path: &str,
        fingerprint: FileFingerprint,
    ) -> Option<CachedAsset> {
        let shard = self.shard(site_id, request_path);
        let cached = {
            let state = shard.state.read().expect("static cache read lock poisoned");
            state
                .sites
                .get(&site_id)
                .and_then(|entries| entries.get(request_path))
                .cloned()
        };
        match cached {
            Some(entry) if entry.fingerprint == fingerprint => {
                entry.last_used.store(self.next_clock(), Ordering::Relaxed);
                entry.valid_until_ns.store(
                    self.elapsed_ns()
                        .saturating_add(self.revalidation_interval_ns()),
                    Ordering::Release,
                );
                self.metrics
                    .static_cache_hits_total
                    .fetch_add(1, Ordering::Relaxed);
                Some(entry.asset.clone())
            }
            Some(_) => {
                self.invalidate(site_id, request_path);
                None
            }
            None => None,
        }
    }

    fn insert(
        &self,
        site_id: usize,
        request_path: String,
        fingerprint: FileFingerprint,
        asset: CachedAsset,
    ) -> CachedAsset {
        let shard = self.shard(site_id, &request_path);
        let mut state = shard
            .state
            .write()
            .expect("static cache write lock poisoned");
        if let Some(existing) = state
            .sites
            .get(&site_id)
            .and_then(|entries| entries.get(request_path.as_str()))
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return existing.asset.clone();
        }
        if let Some(replaced) = remove_entry(&mut state, site_id, &request_path) {
            self.adjust_gauges(replaced.asset.body.len(), 1, 0, 0);
        }

        let body_len = asset.body.len();
        while state.total_entries > 0
            && (state.total_bytes.saturating_add(body_len) > shard.max_bytes
                || state.total_entries >= shard.max_entries)
        {
            let (old_site, old_path) = lru_key(&state).expect("non-empty shard has an LRU entry");
            let removed =
                remove_entry(&mut state, old_site, &old_path).expect("LRU entry came from cache");
            self.adjust_gauges(removed.asset.body.len(), 1, 0, 0);
            self.metrics
                .static_cache_evictions_total
                .fetch_add(1, Ordering::Relaxed);
        }

        state.total_bytes += body_len;
        state.total_entries += 1;
        state.sites.entry(site_id).or_default().insert(
            request_path,
            Arc::new(CacheEntry {
                asset: asset.clone(),
                fingerprint,
                last_used: AtomicU64::new(self.next_clock()),
                valid_until_ns: AtomicU64::new(
                    self.elapsed_ns()
                        .saturating_add(self.revalidation_interval_ns()),
                ),
            }),
        );
        self.adjust_gauges(0, 0, body_len, 1);
        asset
    }

    fn cacheable_size(&self, length: u64) -> bool {
        usize::try_from(length).is_ok_and(|length| {
            length <= self.config.max_file_bytes && length <= self.config.max_total_bytes
        })
    }

    fn shard(&self, site_id: usize, request_path: &str) -> &CacheShard {
        let hash = self.hash_builder.hash_one((site_id, request_path));
        &self.shards[hash as usize % self.shards.len()]
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn revalidation_interval_ns(&self) -> u64 {
        u64::try_from(Duration::from_millis(self.config.revalidate_after_ms).as_nanos())
            .unwrap_or(u64::MAX)
    }

    fn record_revalidation(&self) {
        self.metrics
            .static_cache_revalidations_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn next_clock(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    fn adjust_gauges(
        &self,
        removed_bytes: usize,
        removed_entries: usize,
        added_bytes: usize,
        added_entries: usize,
    ) {
        if removed_bytes > 0 {
            self.metrics
                .static_cache_bytes
                .fetch_sub(removed_bytes, Ordering::Relaxed);
        }
        if removed_entries > 0 {
            self.metrics
                .static_cache_entries
                .fetch_sub(removed_entries, Ordering::Relaxed);
        }
        if added_bytes > 0 {
            self.metrics
                .static_cache_bytes
                .fetch_add(added_bytes, Ordering::Relaxed);
        }
        if added_entries > 0 {
            self.metrics
                .static_cache_entries
                .fetch_add(added_entries, Ordering::Relaxed);
        }
    }
}

fn quota(total: usize, count: usize, index: usize) -> usize {
    total / count + usize::from(index < total % count)
}

fn remove_entry(
    state: &mut ShardState,
    site_id: usize,
    request_path: &str,
) -> Option<Arc<CacheEntry>> {
    let entries = state.sites.get_mut(&site_id)?;
    let removed = entries.remove(request_path)?;
    let remove_site = entries.is_empty();
    state.total_bytes = state.total_bytes.saturating_sub(removed.asset.body.len());
    state.total_entries = state.total_entries.saturating_sub(1);
    if remove_site {
        state.sites.remove(&site_id);
    }
    Some(removed)
}

fn lru_key(state: &ShardState) -> Option<(usize, String)> {
    state
        .sites
        .iter()
        .flat_map(|(site_id, entries)| {
            entries
                .iter()
                .map(move |(path, entry)| (entry.last_used.load(Ordering::Relaxed), *site_id, path))
        })
        .min_by_key(|(last_used, _, _)| *last_used)
        .map(|(_, site_id, path)| (site_id, path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cache(total: usize, file: usize, entries: usize, revalidate_ms: u64) -> StaticCache {
        StaticCache::new(
            StaticCacheConfig {
                enabled: true,
                max_total_bytes: total,
                max_file_bytes: file,
                max_entries: entries,
                revalidate_after_ms: revalidate_ms,
            },
            Arc::new(Metrics::default()),
        )
    }

    async fn load(cache: &StaticCache, key: &str, path: &Path) -> CachedAsset {
        let file = tokio::fs::File::open(path).await.unwrap();
        let metadata = file.metadata().await.unwrap();
        cache
            .get_or_load(0, key, path, file, &metadata)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fast_hit_avoids_revalidation_until_deadline() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "cached").unwrap();
        let cache = cache(1024, 1024, 16, 20);
        assert_eq!(load(&cache, "/asset.txt", &path).await.body, "cached");
        assert!(matches!(
            cache.fast_lookup(0, "/asset.txt"),
            FastLookup::Hit(_)
        ));
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(matches!(
            cache.fast_lookup(0, "/asset.txt"),
            FastLookup::Validate
        ));
        assert!(matches!(
            cache.fast_lookup(0, "/asset.txt"),
            FastLookup::Hit(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_concurrent_request_claims_revalidation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "cached").unwrap();
        let cache = Arc::new(cache(1024, 1024, 16, 10));
        let _ = load(&cache, "/asset.txt", &path).await;
        tokio::time::sleep(Duration::from_millis(15)).await;

        let validators = (0..64)
            .map(|_| {
                let cache = Arc::clone(&cache);
                tokio::spawn(async move {
                    matches!(cache.fast_lookup(0, "/asset.txt"), FastLookup::Validate)
                })
            })
            .collect::<Vec<_>>();
        let mut claimed = 0;
        for validator in validators {
            claimed += usize::from(validator.await.unwrap());
        }
        assert_eq!(claimed, 1);
    }

    #[tokio::test]
    async fn invalidates_a_changed_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "old").unwrap();
        let cache = cache(1024, 1024, 16, 0);
        assert_eq!(load(&cache, "/asset.txt", &path).await.body, "old");
        std::fs::write(&path, "new contents").unwrap();
        assert_eq!(load(&cache, "/asset.txt", &path).await.body, "new contents");
        assert_eq!(
            cache
                .metrics
                .static_cache_invalidations_total
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn obeys_global_file_total_and_entry_limits() {
        let directory = tempdir().unwrap();
        let oversized = directory.path().join("oversized.txt");
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        std::fs::write(&oversized, "12345").unwrap();
        std::fs::write(&first, "1234").unwrap();
        std::fs::write(&second, "5678").unwrap();
        let cache = cache(6, 4, 1, 1000);
        assert_eq!(load(&cache, "/oversized", &oversized).await.body, "12345");
        assert_eq!(load(&cache, "/first", &first).await.body, "1234");
        assert_eq!(load(&cache, "/second", &second).await.body, "5678");
        assert_eq!(
            cache.metrics.static_cache_entries.load(Ordering::Relaxed),
            1
        );
        assert_eq!(cache.metrics.static_cache_bytes.load(Ordering::Relaxed), 4);
        assert_eq!(
            cache
                .metrics
                .static_cache_evictions_total
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serves_concurrent_requests_within_bounds() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "concurrent").unwrap();
        let cache = Arc::new(cache(1024, 1024, 16, 1000));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let cache = Arc::clone(&cache);
            let path = path.clone();
            tasks.push(tokio::spawn(async move {
                load(&cache, "/asset.txt", &path).await.body
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), "concurrent");
        }
        assert_eq!(
            cache.metrics.static_cache_entries.load(Ordering::Relaxed),
            1
        );
        assert_eq!(cache.metrics.static_cache_bytes.load(Ordering::Relaxed), 10);
    }

    #[tokio::test]
    async fn disabled_cache_always_bypasses_storage() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "uncached").unwrap();
        let metrics = Arc::new(Metrics::default());
        let cache = StaticCache::new(StaticCacheConfig::default(), Arc::clone(&metrics));
        assert_eq!(load(&cache, "/asset.txt", &path).await.body, "uncached");
        assert_eq!(
            metrics.static_cache_bypasses_total.load(Ordering::Relaxed),
            1
        );
        assert_eq!(metrics.static_cache_entries.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn rejects_file_that_grows_after_metadata_is_read() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "small").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let metadata = file.metadata().await.unwrap();
        std::fs::write(&path, "a much larger file").unwrap();
        let cache = StaticCache::new(StaticCacheConfig::default(), Arc::new(Metrics::default()));
        assert!(
            cache
                .get_or_load(0, "/asset.txt", &path, file, &metadata)
                .await
                .is_err()
        );
    }

    #[test]
    fn shard_quotas_sum_to_global_limit() {
        for total in 1..100 {
            let count = 16.min(total);
            assert_eq!(
                (0..count).map(|i| quota(total, count, i)).sum::<usize>(),
                total
            );
        }
    }

    #[test]
    fn cached_headers_are_prebuilt() {
        let asset = CachedAsset::new(Path::new("asset.css"), Bytes::from_static(b"abc"));
        assert_eq!(asset.content_type, "text/css");
        assert_eq!(asset.content_length, "3");
    }
}
