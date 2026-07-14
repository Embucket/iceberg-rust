/*!
Process-wide cache for parsed Parquet footer metadata.

Parquet data files written by Iceberg are immutable by path — a rewrite
produces a new file at a new path, never an in-place mutation — so the parsed
footer ([`ParquetMetaData`]) of a given file URI never goes stale and needs no
invalidation. This mirrors the soundness argument behind the manifest byte
cache in `iceberg-rust/src/util/manifest_cache.rs`.

Without this cache every scan re-reads and re-parses each data file's footer
(one or two object-store `GET`s per file per query, each paying S3 first-byte
latency), which dominates `time_elapsed_scanning_until_data`. Caching the
parsed metadata by path removes both the round-trip and the re-parse on warm
scans.

Keys are namespaced by the scan's object-store URL, NOT the store-relative
data-file path: the cache is process-wide, and two object stores can hold
different bytes at the same relative path (`s3://a/x` vs `s3://b/x`). The
per-scan object-store URL is derived from the table's base location, so it
uniquely identifies the store and keeps keys collision-free across tables and
buckets. The file size is stored alongside as a sanity stamp — a lookup whose
requested size disagrees with the cached entry is treated as a miss.

The cache is byte-capped LRU, weighted by each entry's
[`ParquetMetaData::memory_size`]. Capacity comes from
`ICEBERG_PARQUET_METADATA_CACHE_MB` (read once per process; default 64 MiB;
`0` disables caching entirely). Entries larger than the cap are never inserted.

Interception happens through a custom [`ParquetFileReaderFactory`]
([`CachingParquetFileReaderFactory`]) installed on every [`ParquetSource`] the
scan builds, so both cold and warm queries flow through it. Only footer
metadata (`get_metadata`) is cached; row-group/page data reads pass straight
through to the underlying reader.

Concurrent *cold* readers of the same file are collapsed by per-file
single-flight. Without it, N queries racing on the same not-yet-cached footer
each fetch and parse it independently (a dogpile) before the first `put` lands.
The first reader to miss takes the file's single-flight gate — a per-key async
mutex — and performs the sole fetch+parse+insert; the rest wait on the gate,
then re-check the cache after acquiring it and return the winner's `Arc`. So N
racing cold scans pay one round-trip, not N. The synchronous LRU lock is only
taken to look up or insert an entry and is never held across the fetch await;
only the per-file async gate is. The gate map is itself a small LRU capped at
[`INFLIGHT_GATES_CAP`] live gates, so it stays bounded no matter how many
distinct files the process reads; a gate evicted while a load is still in flight
merely lets that one file be fetched twice, never breaking correctness. When
caching is disabled (cap `0`) this path is skipped entirely — reads pass
straight through with no gate and no lock, exactly as before single-flight.

[`ParquetSource`]: datafusion::datasource::physical_plan::parquet::source::ParquetSource
*/

use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use datafusion::datasource::listing::PartitionedFile;
use datafusion::datasource::physical_plan::parquet::{
    DefaultParquetFileReaderFactory, ParquetFileReaderFactory,
};
use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::errors::Result as ParquetResult;
use datafusion::parquet::file::metadata::ParquetMetaData;
use datafusion::physical_plan::metrics::ExecutionPlanMetricsSet;
use futures::future::BoxFuture;
use futures::FutureExt;
use lru::LruCache;
use object_store::ObjectStore;
use tokio::sync::Mutex as AsyncMutex;

const DEFAULT_CAP_MB: usize = 64;

/// Upper bound on the number of distinct per-file single-flight gates kept live
/// at once. The gate map is an LRU capped here so it stays bounded no matter how
/// many distinct files the process reads over its lifetime; a gate evicted while
/// a load is still in flight only weakens de-duplication for that one file (an
/// extra fetch), never correctness.
const INFLIGHT_GATES_CAP: usize = 1024;

/// A cached parsed footer plus the file size it was parsed from.
struct Entry {
    /// Object size in bytes; used as a sanity stamp against the lookup.
    size: u64,
    /// Weight charged against the byte cap (`meta.memory_size()`).
    weight: usize,
    meta: Arc<ParquetMetaData>,
}

struct ByteCappedCache {
    entries: LruCache<String, Entry>,
    total_bytes: usize,
    cap_bytes: usize,
}

impl ByteCappedCache {
    fn new(cap_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            cap_bytes,
        }
    }

    /// Return the cached metadata for `key`, touching its LRU position, but
    /// only if the stored size matches `size`. A size mismatch is a stale
    /// stamp and is reported as a miss.
    fn lookup(&mut self, key: &str, size: u64) -> Option<Arc<ParquetMetaData>> {
        match self.entries.get(key) {
            Some(entry) if entry.size == size => Some(Arc::clone(&entry.meta)),
            _ => None,
        }
    }

    /// Insert `entry` under `key`, evicting least-recently-used entries until
    /// the byte cap holds. Oversized payloads are skipped.
    fn insert(&mut self, key: &str, entry: Entry) {
        let weight = entry.weight;
        if weight > self.cap_bytes {
            return;
        }
        if let Some(previous) = self.entries.put(key.to_string(), entry) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.weight);
        }
        self.total_bytes += weight;
        while self.total_bytes > self.cap_bytes {
            match self.entries.pop_lru() {
                Some((_, evicted)) => {
                    self.total_bytes = self.total_bytes.saturating_sub(evicted.weight);
                }
                None => break,
            }
        }
    }
}

/// The parsed-footer LRU together with the per-file single-flight gates that
/// collapse concurrent cold loads of the same file into one fetch+parse.
///
/// A single instance backs the whole process ([`CACHE`]); the tests construct
/// isolated instances (including disabled ones) to drive [`load`] deterministically.
///
/// [`load`]: MetadataCache::load
struct MetadataCache {
    /// Parsed footers, byte-capped. `None` when caching is disabled (cap `0`),
    /// in which case lookups, inserts, and the single-flight gate are all
    /// bypassed.
    store: Option<Mutex<ByteCappedCache>>,
    /// Per-file single-flight gates, keyed exactly like [`ByteCappedCache`]
    /// entries. Bounded to [`INFLIGHT_GATES_CAP`] live gates by its own LRU.
    inflight: Mutex<LruCache<String, Arc<AsyncMutex<()>>>>,
}

impl MetadataCache {
    fn new(cap_bytes: usize) -> Self {
        let store = if cap_bytes == 0 {
            None
        } else {
            Some(Mutex::new(ByteCappedCache::new(cap_bytes)))
        };
        Self {
            store,
            inflight: Mutex::new(LruCache::new(
                NonZeroUsize::new(INFLIGHT_GATES_CAP).expect("INFLIGHT_GATES_CAP is non-zero"),
            )),
        }
    }

    /// Whether caching (and therefore single-flight) is active.
    fn enabled(&self) -> bool {
        self.store.is_some()
    }

    /// Cached parsed footer for `key`, if present and the size stamp matches.
    /// Takes the LRU lock only briefly and never across an await.
    fn get(&self, key: &str, size: u64) -> Option<Arc<ParquetMetaData>> {
        let store = self.store.as_ref()?;
        let mut guard = store.lock().ok()?;
        guard.lookup(key, size)
    }

    /// Insert `meta` under `key`, weighted by its in-memory size. A no-op when
    /// caching is disabled.
    fn put(&self, key: &str, size: u64, meta: Arc<ParquetMetaData>) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let Ok(mut guard) = store.lock() else {
            return;
        };
        let weight = meta.memory_size();
        guard.insert(key, Entry { size, weight, meta });
    }

    /// Return the single-flight gate for `key`, creating it on first use. The
    /// gate map is a small LRU, so live gates stay bounded to
    /// [`INFLIGHT_GATES_CAP`]; a gate evicted while still held survives through
    /// its holders' `Arc` clones. Takes the map lock only briefly and never
    /// across an await.
    fn inflight_gate(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(gate) = map.get(key) {
            return Arc::clone(gate);
        }
        let gate = Arc::new(AsyncMutex::new(()));
        map.put(key.to_string(), Arc::clone(&gate));
        gate
    }

    /// Resolve the footer for `key`: serve it from cache, or fetch+parse it
    /// through `reader` on a miss, collapsing concurrent cold misses of the same
    /// file into a single fetch.
    ///
    /// When caching is disabled the fetch is issued straight through with no
    /// gate and no lock. Otherwise the first caller to miss takes the file's
    /// single-flight gate and performs the sole fetch+parse+insert; the rest
    /// wait on the gate and, once it releases, re-check the cache and return the
    /// winner's `Arc`. The synchronous LRU locks are never held across the
    /// `reader.get_metadata` await — only the per-file async gate is.
    async fn load(
        &self,
        reader: &mut (dyn AsyncFileReader + Send),
        options: Option<&ArrowReaderOptions>,
        key: &str,
        size: u64,
    ) -> ParquetResult<Arc<ParquetMetaData>> {
        if let Some(meta) = self.get(key, size) {
            return Ok(meta);
        }
        if !self.enabled() {
            // Disabled cache: no gate, no lock overhead — read straight through.
            return reader.get_metadata(options).await;
        }
        let gate = self.inflight_gate(key);
        let _permit = gate.lock().await;
        // The winner may have populated the cache while we waited on the gate.
        if let Some(meta) = self.get(key, size) {
            return Ok(meta);
        }
        let meta = reader.get_metadata(options).await?;
        self.put(key, size, Arc::clone(&meta));
        Ok(meta)
    }
}

/// The process-wide metadata cache. Capacity comes from
/// `ICEBERG_PARQUET_METADATA_CACHE_MB` (read once per process; default 64 MiB;
/// `0` disables caching and single-flight entirely).
static CACHE: LazyLock<MetadataCache> = LazyLock::new(|| {
    let cap_mb = std::env::var("ICEBERG_PARQUET_METADATA_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_MB);
    MetadataCache::new(cap_mb.saturating_mul(1024 * 1024))
});

/// A [`ParquetFileReaderFactory`] that serves parsed footer metadata from the
/// process-wide [`CACHE`], falling back to an inner factory (the DataFusion
/// default, which reads from the object store) on a miss.
///
/// One factory is built per scan; it carries the scan's object-store URL as a
/// key prefix so cache keys are unique across stores and buckets even though
/// the [`PartitionedFile`] locations it receives are only store-relative.
#[derive(Debug)]
pub(crate) struct CachingParquetFileReaderFactory {
    inner: Arc<dyn ParquetFileReaderFactory>,
    key_prefix: Arc<str>,
}

impl CachingParquetFileReaderFactory {
    /// Build a caching factory over `store`, namespacing cache keys with
    /// `key_prefix` (the scan's object-store URL).
    pub(crate) fn new(store: Arc<dyn ObjectStore>, key_prefix: &str) -> Self {
        Self {
            inner: Arc::new(DefaultParquetFileReaderFactory::new(store)),
            key_prefix: Arc::from(key_prefix),
        }
    }
}

impl ParquetFileReaderFactory for CachingParquetFileReaderFactory {
    fn create_reader(
        &self,
        partition_index: usize,
        partitioned_file: PartitionedFile,
        metadata_size_hint: Option<usize>,
        metrics: &ExecutionPlanMetricsSet,
    ) -> datafusion::error::Result<Box<dyn AsyncFileReader + Send>> {
        let key = format!(
            "{}{}",
            self.key_prefix,
            partitioned_file.object_meta.location.as_ref()
        );
        let size = partitioned_file.object_meta.size;
        let inner = self.inner.create_reader(
            partition_index,
            partitioned_file,
            metadata_size_hint,
            metrics,
        )?;
        Ok(Box::new(CachingMetadataReader { inner, key, size }))
    }
}

/// [`AsyncFileReader`] decorator that serves `get_metadata` from the
/// process-wide cache and forwards data reads to `inner`.
struct CachingMetadataReader {
    inner: Box<dyn AsyncFileReader + Send>,
    key: String,
    size: u64,
}

impl AsyncFileReader for CachingMetadataReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, ParquetResult<Bytes>> {
        self.inner.get_bytes(range)
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, ParquetResult<Vec<Bytes>>> {
        self.inner.get_byte_ranges(ranges)
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, ParquetResult<Arc<ParquetMetaData>>> {
        CACHE
            .load(&mut *self.inner, options, &self.key, self.size)
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use datafusion::arrow::array::{ArrayRef, Int32Array, RecordBatch};
    use datafusion::parquet::arrow::ArrowWriter;
    use datafusion::parquet::file::metadata::FileMetaData;
    use datafusion::parquet::schema::types::{SchemaDescriptor, Type};
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OsResult,
    };

    /// A minimal parsed footer with a known [`memory_size`] weight is awkward
    /// to construct, so the byte-cap mechanics are exercised with an empty
    /// [`ParquetMetaData`] and explicit weights, mirroring the manifest cache's
    /// locally-built test instance.
    fn dummy_meta() -> Arc<ParquetMetaData> {
        let schema = Type::group_type_builder("schema").build().unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));
        let file_meta = FileMetaData::new(1, 0, None, None, descr, None);
        Arc::new(ParquetMetaData::new(file_meta, vec![]))
    }

    fn entry(size: u64, weight: usize) -> Entry {
        Entry {
            size,
            weight,
            meta: dummy_meta(),
        }
    }

    #[test]
    fn uri_keys_do_not_collide_across_stores() {
        // Same store-relative path under two object-store URLs must stay two
        // entries. Exercises the real process-wide cache (default cap, env
        // unset in tests). Keys mirror how the factory namespaces the
        // store-relative location with the scan's object-store URL.
        let key_a = "iceberg-rust://bucket-a/db/tbl/data/f.parquet";
        let key_b = "iceberg-rust://bucket-b/db/tbl/data/f.parquet";
        CACHE.put(key_a, 10, dummy_meta());
        CACHE.put(key_b, 20, dummy_meta());
        assert!(CACHE.get(key_a, 10).is_some());
        assert!(CACHE.get(key_b, 20).is_some());
        // The bare store-relative path is not a key at all.
        assert!(CACHE.get("/db/tbl/data/f.parquet", 10).is_none());
    }

    #[test]
    fn size_stamp_mismatch_is_a_miss() {
        let mut cache = ByteCappedCache::new(1024);
        cache.insert("a", entry(100, 8));
        assert!(cache.lookup("a", 100).is_some());
        // Same key, different size => stale stamp => miss.
        assert!(cache.lookup("a", 200).is_none());
    }

    #[test]
    fn byte_cap_evicts_lru() {
        let mut cache = ByteCappedCache::new(100);
        cache.insert("a", entry(1, 60));
        cache.insert("b", entry(1, 60));
        // "a" evicted to fit "b".
        assert!(cache.lookup("a", 1).is_none());
        assert!(cache.lookup("b", 1).is_some());
        assert!(cache.total_bytes <= 100);
    }

    #[test]
    fn oversized_payload_skipped() {
        let mut cache = ByteCappedCache::new(50);
        cache.insert("big", entry(1, 51));
        assert!(cache.lookup("big", 1).is_none());
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn replacing_entry_updates_accounting() {
        let mut cache = ByteCappedCache::new(100);
        cache.insert("a", entry(1, 40));
        cache.insert("a", entry(1, 20));
        assert_eq!(cache.total_bytes, 20);
    }

    #[test]
    fn zero_disables_cache() {
        // A cap of 0 stores nothing and always misses.
        let mut cache = ByteCappedCache::new(0);
        cache.insert("a", entry(1, 1));
        assert!(cache.lookup("a", 1).is_none());
        assert_eq!(cache.total_bytes, 0);
    }

    /// An [`ObjectStore`] that counts `get_opts` calls (all footer/data reads
    /// funnel through it) and delegates everything else to an inner store. An
    /// optional per-`get` `delay` widens the window in which concurrent cold
    /// readers overlap, making the single-flight tests deterministic.
    #[derive(Debug)]
    struct CountingStore {
        inner: Arc<dyn ObjectStore>,
        gets: Arc<AtomicUsize>,
        delay: Option<Duration>,
    }

    impl std::fmt::Display for CountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CountingStore({})", self.inner)
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for CountingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> OsResult<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> OsResult<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> OsResult<GetResult> {
            if let Some(delay) = self.delay {
                // Yield while "fetching" so racing cold readers pile onto the
                // single-flight gate before the winner inserts.
                tokio::time::sleep(delay).await;
            }
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, OsResult<ObjectPath>>,
        ) -> BoxStream<'static, OsResult<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(&self, prefix: Option<&ObjectPath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(&self, prefix: Option<&ObjectPath>) -> OsResult<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: CopyOptions,
        ) -> OsResult<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    async fn write_parquet(store: &Arc<dyn ObjectStore>, path: &str) -> u64 {
        let col: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let batch = RecordBatch::try_from_iter(vec![("c1", col)]).unwrap();
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let len = buf.len() as u64;
        store
            .put(&ObjectPath::from(path), PutPayload::from(buf))
            .await
            .unwrap();
        len
    }

    fn partitioned_file(path: &str, size: u64) -> PartitionedFile {
        PartitionedFile::new(path.to_string(), size)
    }

    #[tokio::test]
    async fn warm_scan_performs_zero_metadata_fetches() {
        let inner = Arc::new(InMemory::new()) as Arc<dyn ObjectStore>;
        let path = "db/tbl/data/warm.parquet";
        let size = write_parquet(&inner, path).await;

        let gets = Arc::new(AtomicUsize::new(0));
        let counting = Arc::new(CountingStore {
            inner,
            gets: Arc::clone(&gets),
            delay: None,
        }) as Arc<dyn ObjectStore>;

        // Unique prefix so the shared process cache does not collide with
        // other tests.
        let factory =
            CachingParquetFileReaderFactory::new(counting, "iceberg-rust://warm-scan-test/");
        let metrics = ExecutionPlanMetricsSet::new();

        // First (cold) scan: fetches and parses the footer.
        let mut reader = factory
            .create_reader(0, partitioned_file(path, size), None, &metrics)
            .unwrap();
        let cold = reader.get_metadata(None).await.unwrap();
        let after_cold = gets.load(Ordering::SeqCst);
        assert!(after_cold >= 1, "cold scan should hit the store");

        // Second (warm) scan through a fresh reader: served from cache, no
        // further store access.
        let mut reader = factory
            .create_reader(0, partitioned_file(path, size), None, &metrics)
            .unwrap();
        let warm = reader.get_metadata(None).await.unwrap();
        assert_eq!(
            gets.load(Ordering::SeqCst),
            after_cold,
            "warm scan must not fetch metadata from the store"
        );
        assert!(
            Arc::ptr_eq(&cold, &warm),
            "warm scan returns the cached Arc"
        );
    }

    /// Build a counting store over a freshly written parquet file, returning the
    /// inner store (so callers can add more files), a reader factory over the
    /// counting wrapper, the file size, and the shared `get` counter. Each `get`
    /// sleeps `delay` so concurrent cold readers overlap deterministically.
    async fn counting_setup(
        path: &str,
        delay: Option<Duration>,
    ) -> (
        Arc<dyn ObjectStore>,
        Arc<DefaultParquetFileReaderFactory>,
        u64,
        Arc<AtomicUsize>,
    ) {
        let inner = Arc::new(InMemory::new()) as Arc<dyn ObjectStore>;
        let size = write_parquet(&inner, path).await;
        let gets = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(CountingStore {
            inner: Arc::clone(&inner),
            gets: Arc::clone(&gets),
            delay,
        }) as Arc<dyn ObjectStore>;
        (
            inner,
            Arc::new(DefaultParquetFileReaderFactory::new(store)),
            size,
            gets,
        )
    }

    /// Fetch `path`'s footer once through a fresh reader and return the number
    /// of store `get`s a single metadata parse costs (1 or 2 depending on the
    /// footer-tail read pattern). Resets the counter afterwards.
    async fn single_fetch_cost(
        factory: &DefaultParquetFileReaderFactory,
        path: &str,
        size: u64,
        gets: &AtomicUsize,
    ) -> usize {
        let cache = MetadataCache::new(64 * 1024 * 1024);
        let metrics = ExecutionPlanMetricsSet::new();
        let mut reader = factory
            .create_reader(0, partitioned_file(path, size), None, &metrics)
            .unwrap();
        cache
            .load(
                &mut *reader,
                None,
                "iceberg-rust://cost-probe/f.parquet",
                size,
            )
            .await
            .unwrap();
        gets.swap(0, Ordering::SeqCst)
    }

    /// N concurrent cold readers of the same file collapse to exactly one
    /// fetch+parse, and every reader receives the winner's `Arc`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_reads_single_flight_one_fetch() {
        let path = "db/tbl/data/single_flight.parquet";
        let (_inner, factory, size, gets) =
            counting_setup(path, Some(Duration::from_millis(25))).await;

        let per_fetch = single_fetch_cost(&factory, path, size, &gets).await;
        assert!(per_fetch >= 1, "a cold fetch must hit the store");

        let cache = Arc::new(MetadataCache::new(64 * 1024 * 1024));
        let key = "iceberg-rust://single-flight/db/tbl/data/single_flight.parquet";
        let n = 8;
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let factory = Arc::clone(&factory);
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                let metrics = ExecutionPlanMetricsSet::new();
                let mut reader = factory
                    .create_reader(0, partitioned_file(path, size), None, &metrics)
                    .unwrap();
                cache.load(&mut *reader, None, key, size).await.unwrap()
            }));
        }
        let metas: Vec<Arc<ParquetMetaData>> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|joined| joined.unwrap())
            .collect();

        assert_eq!(
            gets.load(Ordering::SeqCst),
            per_fetch,
            "N concurrent cold readers must trigger exactly one underlying fetch"
        );
        for meta in &metas[1..] {
            assert!(
                Arc::ptr_eq(&metas[0], meta),
                "every reader shares the single-flight winner's Arc"
            );
        }
    }

    /// Concurrent cold reads of *different* files are not serialized against
    /// each other: both complete and each file is fetched exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_of_different_files_are_independent() {
        let delay = Some(Duration::from_millis(25));
        let path_a = "db/tbl/data/independent_a.parquet";
        let path_b = "db/tbl/data/independent_b.parquet";
        let (inner, factory, size, gets) = counting_setup(path_a, delay).await;
        // A second, byte-identical file in the same store: same footer layout,
        // so the same per-fetch cost. Writes go straight to the inner store
        // (`put` is not counted); reads still flow through the counting wrapper.
        let size_b = write_parquet(&inner, path_b).await;
        assert_eq!(size, size_b, "identical content must yield identical size");

        let per_fetch = single_fetch_cost(&factory, path_a, size, &gets).await;
        assert!(per_fetch >= 1);

        let cache = Arc::new(MetadataCache::new(64 * 1024 * 1024));
        let spawn_load = |path: &'static str, key: &'static str, size: u64| {
            let factory = Arc::clone(&factory);
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                let metrics = ExecutionPlanMetricsSet::new();
                let mut reader = factory
                    .create_reader(0, partitioned_file(path, size), None, &metrics)
                    .unwrap();
                cache.load(&mut *reader, None, key, size).await.unwrap()
            })
        };
        let a = spawn_load(path_a, "iceberg-rust://independent/a", size);
        let b = spawn_load(path_b, "iceberg-rust://independent/b", size_b);
        let (ra, rb) = tokio::join!(a, b);
        let meta_a = ra.unwrap();
        let meta_b = rb.unwrap();

        assert_eq!(
            gets.load(Ordering::SeqCst),
            per_fetch * 2,
            "two different files are fetched once each, not deduped together"
        );
        assert!(
            !Arc::ptr_eq(&meta_a, &meta_b),
            "distinct files yield distinct metadata"
        );
    }

    /// With caching disabled (cap `0`) there is no single-flight: concurrent
    /// cold readers of the same file each fetch independently, and the disabled
    /// path never touches a lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disabled_cache_bypasses_single_flight() {
        let path = "db/tbl/data/disabled.parquet";
        let (_inner, factory, size, gets) =
            counting_setup(path, Some(Duration::from_millis(25))).await;

        let cache = Arc::new(MetadataCache::new(0));
        assert!(!cache.enabled(), "cap 0 disables the cache");

        // Cost of a single fetch straight through the disabled cache.
        let metrics = ExecutionPlanMetricsSet::new();
        let mut probe = factory
            .create_reader(0, partitioned_file(path, size), None, &metrics)
            .unwrap();
        cache
            .load(&mut *probe, None, "iceberg-rust://disabled/probe", size)
            .await
            .unwrap();
        let per_fetch = gets.swap(0, Ordering::SeqCst);
        assert!(per_fetch >= 1);

        let key = "iceberg-rust://disabled/db/tbl/data/disabled.parquet";
        let n = 6;
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let factory = Arc::clone(&factory);
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                let metrics = ExecutionPlanMetricsSet::new();
                let mut reader = factory
                    .create_reader(0, partitioned_file(path, size), None, &metrics)
                    .unwrap();
                cache.load(&mut *reader, None, key, size).await.unwrap()
            }));
        }
        let metas: Vec<Arc<ParquetMetaData>> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|joined| joined.unwrap())
            .collect();

        assert_eq!(
            gets.load(Ordering::SeqCst),
            per_fetch * n,
            "disabled cache: every concurrent reader fetches (no single-flight)"
        );
        for meta in &metas[1..] {
            assert!(
                !Arc::ptr_eq(&metas[0], meta),
                "disabled cache shares nothing between readers"
            );
        }
    }
}
