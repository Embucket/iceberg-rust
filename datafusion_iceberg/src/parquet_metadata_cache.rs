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

[`ParquetSource`]: datafusion::datasource::physical_plan::parquet::source::ParquetSource
*/

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

const DEFAULT_CAP_MB: usize = 64;

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

static CACHE: LazyLock<Option<Mutex<ByteCappedCache>>> = LazyLock::new(|| {
    let cap_mb = std::env::var("ICEBERG_PARQUET_METADATA_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_MB);
    if cap_mb == 0 {
        return None;
    }
    Some(Mutex::new(ByteCappedCache::new(
        cap_mb.saturating_mul(1024 * 1024),
    )))
});

/// Cached parsed footer for `key`, if present and the size stamp matches.
fn get(key: &str, size: u64) -> Option<Arc<ParquetMetaData>> {
    let cache = CACHE.as_ref()?;
    let mut guard = cache.lock().ok()?;
    guard.lookup(key, size)
}

/// Insert `meta` under `key`, weighted by its in-memory size.
fn put(key: &str, size: u64, meta: Arc<ParquetMetaData>) {
    let Some(cache) = CACHE.as_ref() else {
        return;
    };
    let Ok(mut guard) = cache.lock() else {
        return;
    };
    let weight = meta.memory_size();
    guard.insert(key, Entry { size, weight, meta });
}

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
        if let Some(meta) = get(&self.key, self.size) {
            return futures::future::ready(Ok(meta)).boxed();
        }
        let key = self.key.clone();
        let size = self.size;
        let fetch = self.inner.get_metadata(options);
        async move {
            let meta = fetch.await?;
            put(&key, size, Arc::clone(&meta));
            Ok(meta)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

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
        put(key_a, 10, dummy_meta());
        put(key_b, 20, dummy_meta());
        assert!(get(key_a, 10).is_some());
        assert!(get(key_b, 20).is_some());
        // The bare store-relative path is not a key at all.
        assert!(get("/db/tbl/data/f.parquet", 10).is_none());
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
    /// funnel through it) and delegates everything else to an inner store.
    #[derive(Debug)]
    struct CountingStore {
        inner: Arc<dyn ObjectStore>,
        gets: Arc<AtomicUsize>,
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
}
