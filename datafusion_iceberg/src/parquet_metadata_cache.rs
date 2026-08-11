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

# Page indexes

The parquet page index is not stored inline in the footer, so a plain
`get_metadata` does not return it. DataFusion's opener therefore loads the footer
with [`PageIndexPolicy::Skip`], and *separately* calls its own `load_page_index`
when a page-pruning predicate exists — which rebuilds an enriched
[`ParquetMetaData`] locally and then drops it. The enriched copy never reaches any
cache, so **every query re-fetches and re-parses the page index of every file it
touches**. Measured on TPC-H SF1000, metadata loading was 44 % of total
partition-time.

A cold load therefore overrides the caller's policy to
[`PageIndexPolicy::Optional`], so the page index is fetched in the *same*
`get_metadata` round trip and cached with the footer. The opener's
`load_page_index` then finds both indexes already present and short-circuits
without any I/O. The trade is one extra range read per file on first touch — paid
back on the second query that touches the file, and wasted only on a file no
query ever predicates over.

Measured 2026-07-30/31 on the deployed service, the trade turned out to be
**scale-dependent**, so it is now **guarded** rather than unconditional.

Where the working set fits the cache it is a large win — warm `metadata_load_time`
collapses (TPC-H SF100 Q8: **15.1 s → 0.2 s**, 75x, at a 100 % footer hit rate
either way, because that 15.1 s was *entirely* the uncached page index). End to end
with 2x scan partitions: **SF1 −29 %, SF10 −15 %, ClickBench −34 %, SF100 −18 %** of
server time.

Where it does not fit, it inverts. At TPC-H SF1000 the 64 MiB cache churns, so the
extra first-touch range read is paid on nearly every load and the warm payback
almost never arrives. Attribution on Q10, one variable at a time, same binary:

| config | Q10 |
|---|---|
| page index off | **393 s** (baseline was 383 s) |
| page index on, 64 MiB | 511 s (+33 %) |
| page index on, 2048 MiB | >1860 s (killed) |

Across that suite it took Q18 from 2 078 s to a 7 200 s **timeout** and Q21 +62 %.
Raising the cap does not rescue it — it worsens it, because gigabytes of cached
metadata inside a fixed container compete with the query budget.

Hence [`PAGE_INDEX_BACKOFF_EVICTIONS`]: widening is on by default and latches off
once the cache demonstrates, by evicting, that the working set does not fit. The
guard is one-way and fails safe — see [`MetadataCache::note_evictions`].
`parquet_metadata_cache_page_index_suppressed` reports when it has engaged.

# Accounting

Hits, misses and evictions are reported as DataFusion plan metrics on the scan
node, so `EXPLAIN ANALYZE VERBOSE` shows them against any deployed binary with no
wire change. Without them a thrashing cache and a cold cache are
indistinguishable — both just look like slow metadata loading.

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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use datafusion::datasource::listing::PartitionedFile;
use datafusion::datasource::physical_plan::parquet::{
    DefaultParquetFileReaderFactory, ParquetFileReaderFactory,
};
use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::errors::Result as ParquetResult;
use datafusion::parquet::file::metadata::{PageIndexPolicy, ParquetMetaData};
use datafusion::physical_plan::metrics::{Count, ExecutionPlanMetricsSet, MetricBuilder};
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

/// Cumulative evictions after which cold loads stop fetching the page index.
///
/// Caching page indexes only pays while the metadata working set fits: the entries
/// it produces are much larger, so once the cache is churning we pay the extra
/// first-touch range read on nearly every load and the warm payback never arrives.
/// Measured at TPC-H SF1000 that inversion took Q18 from 2 078 s to a 7 200 s
/// timeout; at SF100, where nothing evicts, the same feature is a 75x cut in warm
/// metadata load.
///
/// The threshold is small but non-zero so a single oversized file cannot latch the
/// guard, and it separates the measured cases by orders of magnitude: SF100 evicts
/// 0, SF1000 evicts 619 on one cold run.
const PAGE_INDEX_BACKOFF_EVICTIONS: u64 = 64;

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

    /// Current `(entry count, total bytes)` — the working-set measurement that
    /// sizes the cap per deployment.
    fn stats(&self) -> (usize, usize) {
        (self.entries.len(), self.total_bytes)
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
    ///
    /// Returns how many entries this insert evicted — the direct thrashing
    /// signal. A cache that is merely cold evicts nothing; a cache that is too
    /// small for the working set evicts on almost every insert.
    fn insert(&mut self, key: &str, entry: Entry) -> usize {
        let weight = entry.weight;
        if weight > self.cap_bytes {
            return 0;
        }
        if let Some(previous) = self.entries.put(key.to_string(), entry) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.weight);
        }
        self.total_bytes += weight;
        let mut evicted = 0;
        while self.total_bytes > self.cap_bytes {
            match self.entries.pop_lru() {
                Some((_, entry)) => {
                    self.total_bytes = self.total_bytes.saturating_sub(entry.weight);
                    evicted += 1;
                }
                None => break,
            }
        }
        evicted
    }
}

/// Per-scan cache accounting, surfaced as plan metrics so
/// `EXPLAIN ANALYZE VERBOSE` reports it without any wire change.
///
/// `evictions` counts entries pushed out by *this scan's* inserts, which is what
/// separates "cache is cold" from "cache is too small": a cold cache misses and
/// evicts nothing, an undersized one misses and evicts in equal measure.
#[derive(Debug, Clone)]
struct CacheMetrics {
    hits: Count,
    misses: Count,
    evictions: Count,
    /// Cold loads that did NOT fetch the page index because the adaptive guard had
    /// latched. Non-zero means this deployment's working set outgrew the cache and
    /// the optimization backed itself off.
    page_index_suppressed: Count,
}

impl CacheMetrics {
    fn new(metrics: &ExecutionPlanMetricsSet, partition: usize) -> Self {
        Self {
            hits: MetricBuilder::new(metrics).counter("parquet_metadata_cache_hits", partition),
            misses: MetricBuilder::new(metrics).counter("parquet_metadata_cache_misses", partition),
            evictions: MetricBuilder::new(metrics)
                .counter("parquet_metadata_cache_evictions", partition),
            page_index_suppressed: MetricBuilder::new(metrics)
                .counter("parquet_metadata_cache_page_index_suppressed", partition),
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
    /// Entries evicted over the process lifetime. Drives [`page_index_backed_off`].
    evictions: AtomicU64,
    /// Set once cumulative evictions pass [`PAGE_INDEX_BACKOFF_EVICTIONS`], after
    /// which cold loads stop fetching the page index. See [`page_index_backed_off`].
    backed_off: AtomicBool,
    /// Inserts over the process lifetime; drives the periodic working-set log.
    inserts: AtomicU64,
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
            evictions: AtomicU64::new(0),
            backed_off: AtomicBool::new(false),
            inserts: AtomicU64::new(0),
        }
    }

    /// Current `(entry count, total bytes)` of the store, `None` when disabled.
    fn stats(&self) -> Option<(usize, usize)> {
        let store = self.store.as_ref()?;
        let guard = store.lock().ok()?;
        Some(guard.stats())
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
    /// caching is disabled. Returns how many entries the insert evicted.
    fn put(&self, key: &str, size: u64, meta: Arc<ParquetMetaData>) -> usize {
        let Some(store) = self.store.as_ref() else {
            return 0;
        };
        let evicted = {
            let Ok(mut guard) = store.lock() else {
                return 0;
            };
            let weight = meta.memory_size();
            guard.insert(key, Entry { size, weight, meta })
        };
        if evicted > 0 {
            self.note_evictions(evicted as u64);
        }
        // Periodic working-set snapshot: mean entry weight × distinct files is
        // what sizes the cap per node, and nothing else reports it.
        let inserts = self.inserts.fetch_add(1, Ordering::Relaxed) + 1;
        if inserts.is_multiple_of(4096) {
            if let Some((entries, total_bytes)) = self.stats() {
                tracing::info!(
                    target: "datafusion_iceberg::parquet_metadata_cache",
                    inserts,
                    entries,
                    total_bytes,
                    mean_entry_bytes = total_bytes.checked_div(entries).unwrap_or(0),
                    evictions = self.evictions.load(Ordering::Relaxed),
                    "parquet metadata cache working-set snapshot"
                );
            }
        }
        evicted
    }

    /// Accumulate evictions and latch the page-index guard once they pass the
    /// threshold.
    ///
    /// The latch is deliberately **one-way**. Backing off makes subsequent entries
    /// footer-sized, which stops the eviction that triggered it -- so a recovering
    /// guard would re-enable widening, grow entries, evict again, and oscillate. A
    /// latch also fails in the safe direction: the worst case is losing an
    /// optimization for the life of the process, never reintroducing the cliff.
    fn note_evictions(&self, evicted: u64) {
        let total = self.evictions.fetch_add(evicted, Ordering::Relaxed) + evicted;
        if total > PAGE_INDEX_BACKOFF_EVICTIONS && !self.backed_off.swap(true, Ordering::Relaxed) {
            let (entries, total_bytes) = self.stats().unwrap_or((0, 0));
            tracing::info!(
                target: "datafusion_iceberg::parquet_metadata_cache",
                evictions = total,
                threshold = PAGE_INDEX_BACKOFF_EVICTIONS,
                entries,
                total_bytes,
                mean_entry_bytes = total_bytes.checked_div(entries).unwrap_or(0),
                "metadata working set exceeds the cache; page-index caching backed off"
            );
        }
    }

    /// Whether the guard has latched. Cold loads consult this before widening.
    fn page_index_backed_off(&self) -> bool {
        self.backed_off.load(Ordering::Relaxed)
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
        metrics: &CacheMetrics,
    ) -> ParquetResult<Arc<ParquetMetaData>> {
        if let Some(meta) = self.get(key, size) {
            metrics.hits.add(1);
            return Ok(meta);
        }
        if !self.enabled() {
            // Disabled cache: no gate, no lock overhead — read straight through.
            metrics.misses.add(1);
            return reader.get_metadata(options).await;
        }
        let gate = self.inflight_gate(key);
        let _permit = gate.lock().await;
        // The winner may have populated the cache while we waited on the gate.
        if let Some(meta) = self.get(key, size) {
            metrics.hits.add(1);
            return Ok(meta);
        }
        metrics.misses.add(1);
        // Fetch the page index in this same round trip when configured, so the
        // cached entry is complete and the opener's own `load_page_index` finds
        // nothing left to do -- but only while the working set still fits. Once the
        // cache is churning, the extra range read is pure cost. See the module docs.
        let widen = *CACHE_PAGE_INDEX && !self.page_index_backed_off();
        if *CACHE_PAGE_INDEX && !widen {
            metrics.page_index_suppressed.add(1);
        }
        let widened = page_index_options(options, widen);
        let meta = reader.get_metadata(widened.as_ref().or(options)).await?;
        metrics
            .evictions
            .add(self.put(key, size, Arc::clone(&meta)));
        Ok(meta)
    }
}

/// Whether cold loads should widen the caller's page-index policy so the index is
/// fetched and cached alongside the footer. `ICEBERG_PARQUET_METADATA_CACHE_PAGE_INDEX`,
/// read once per process, **default on** — the [`PAGE_INDEX_BACKOFF_EVICTIONS`] guard
/// disables it automatically on deployments whose working set does not fit, so the
/// default no longer carries the large-scale cliff. Set to `0` to disable outright.
static CACHE_PAGE_INDEX: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ICEBERG_PARQUET_METADATA_CACHE_PAGE_INDEX")
        .map(|v| !matches!(v.trim(), "0" | "false" | "FALSE" | "no" | "off"))
        .unwrap_or(true)
});

/// Per-file metadata prefetch hint in bytes, from
/// `ICEBERG_PARQUET_METADATA_SIZE_HINT_KB` (read once per process; absent or
/// `0` disables the hint — the shipped default, byte-identical to before).
///
/// Without a hint the parquet reader prefetches only the 8-byte footer tail, so
/// every cold metadata load costs two sequential ranged GETs (tail probe, then
/// footer body) plus a third for the page index. A hint that covers
/// footer + page index turns all three into one suffix GET whose retained
/// remainder serves the index ranges with no further fetch.
static METADATA_SIZE_HINT: LazyLock<Option<usize>> = LazyLock::new(|| {
    parse_metadata_size_hint_kb(
        std::env::var("ICEBERG_PARQUET_METADATA_SIZE_HINT_KB")
            .ok()
            .as_deref(),
    )
});

fn parse_metadata_size_hint_kb(raw: Option<&str>) -> Option<usize> {
    let kb = raw?.trim().parse::<usize>().ok()?;
    (kb > 0).then(|| kb.saturating_mul(1024))
}

/// The configured metadata prefetch hint clamped to `file_size`, for stamping
/// onto a `PartitionedFile`. `None` when the hint is unconfigured.
pub(crate) fn metadata_size_hint(file_size: u64) -> Option<usize> {
    clamp_metadata_size_hint(*METADATA_SIZE_HINT, file_size)
}

/// Split from [`metadata_size_hint`] so the clamp is testable without the
/// process-wide env var.
fn clamp_metadata_size_hint(hint: Option<usize>, file_size: u64) -> Option<usize> {
    let hint = hint?;
    Some(hint.min(usize::try_from(file_size).unwrap_or(usize::MAX)))
}

/// The caller's options with the page-index policy raised to `Optional`, or
/// `None` to leave the caller's options untouched.
///
/// Returns `None` when `enabled` is false, so the disabled path is byte-identical
/// to not having the feature at all. Everything else on the options (decryption
/// properties, the supplied schema) is preserved by cloning rather than rebuilding.
///
/// `enabled` is a parameter rather than a read of [`CACHE_PAGE_INDEX`] so the
/// policy logic is testable without a process-wide env var.
fn page_index_options(
    options: Option<&ArrowReaderOptions>,
    enabled: bool,
) -> Option<ArrowReaderOptions> {
    if !enabled {
        return None;
    }
    let base = options.cloned().unwrap_or_default();
    // Already asking for the index: nothing to widen, and cloning would be waste.
    if base.column_index_policy() != PageIndexPolicy::Skip
        && base.offset_index_policy() != PageIndexPolicy::Skip
    {
        return None;
    }
    Some(base.with_page_index_policy(PageIndexPolicy::Optional))
}

/// The process-wide metadata cache. Capacity comes from
/// `ICEBERG_PARQUET_METADATA_CACHE_MB` (read once per process; default 64 MiB;
/// `0` disables caching and single-flight entirely).
static CACHE: LazyLock<MetadataCache> = LazyLock::new(|| {
    let raw = std::env::var("ICEBERG_PARQUET_METADATA_CACHE_MB").ok();
    let cap_mb = raw
        .as_deref()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_MB);
    // Logged on first scan, not at startup (this is a LazyLock). Worth the line:
    // this cache ran at its 64 MiB default across every benchmarked tier because
    // nothing set the env var and nothing reported the resolved value, so a
    // deploy-spec typo is indistinguishable from a deliberate default.
    tracing::info!(
        target: "datafusion_iceberg::parquet_metadata_cache",
        cap_mb,
        configured = raw.is_some(),
        page_index_cached = *CACHE_PAGE_INDEX,
        metadata_size_hint_bytes = *METADATA_SIZE_HINT,
        "parquet metadata cache initialized"
    );
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
        let cache_metrics = CacheMetrics::new(metrics, partition_index);
        let inner = self.inner.create_reader(
            partition_index,
            partitioned_file,
            metadata_size_hint,
            metrics,
        )?;
        Ok(Box::new(CachingMetadataReader {
            inner,
            key,
            size,
            metrics: cache_metrics,
        }))
    }
}

/// [`AsyncFileReader`] decorator that serves `get_metadata` from the
/// process-wide cache and forwards data reads to `inner`.
struct CachingMetadataReader {
    inner: Box<dyn AsyncFileReader + Send>,
    key: String,
    size: u64,
    metrics: CacheMetrics,
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
            .load(
                &mut *self.inner,
                options,
                &self.key,
                self.size,
                &self.metrics,
            )
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

    /// Throwaway metric handles, for `load` call sites whose test does not inspect
    /// them. `Count` is `Arc`-backed, so the handles stay live after the set drops.
    fn discard_metrics() -> CacheMetrics {
        CacheMetrics::new(&ExecutionPlanMetricsSet::new(), 0)
    }

    /// The counters must separate a cold cache from an undersized one. Without
    /// this distinction both look identical from the outside — just slow metadata
    /// loading — which is why SF1000's 9x per-file metadata regression went
    /// unexplained.
    #[tokio::test]
    async fn load_reports_hits_then_misses() {
        let inner = Arc::new(InMemory::new()) as Arc<dyn ObjectStore>;
        let path = "db/tbl/data/counted.parquet";
        let size = write_parquet(&inner, path).await;
        let factory = CachingParquetFileReaderFactory::new(inner, "iceberg-rust://counter-test/");
        let cache = MetadataCache::new(64 * 1024 * 1024);
        let plan_metrics = ExecutionPlanMetricsSet::new();
        let m = CacheMetrics::new(&plan_metrics, 0);
        let key = "iceberg-rust://counter-test/counted.parquet";

        let mut reader = factory
            .create_reader(0, partitioned_file(path, size), None, &plan_metrics)
            .unwrap();
        cache.load(&mut *reader, None, key, size, &m).await.unwrap();
        assert_eq!(
            (m.hits.value(), m.misses.value()),
            (0, 1),
            "cold load is a miss"
        );

        let mut reader = factory
            .create_reader(0, partitioned_file(path, size), None, &plan_metrics)
            .unwrap();
        cache.load(&mut *reader, None, key, size, &m).await.unwrap();
        assert_eq!(
            (m.hits.value(), m.misses.value()),
            (1, 1),
            "warm load is a hit"
        );
        assert_eq!(m.evictions.value(), 0, "a cache with room evicts nothing");
    }

    /// An undersized cache reports evictions, which is the thrashing signal a
    /// merely-cold cache does not produce.
    #[test]
    fn insert_reports_how_many_entries_it_evicted() {
        let mut cache = ByteCappedCache::new(1000);
        assert_eq!(cache.insert("a", entry(1, 600)), 0, "fits, evicts nothing");
        // 600 + 600 > 1000, so inserting b must push a out.
        assert_eq!(
            cache.insert("b", entry(1, 600)),
            1,
            "over cap, evicts the LRU"
        );
        assert_eq!(cache.total_bytes, 600);
        assert!(cache.lookup("a", 1).is_none(), "a was the evicted entry");
    }

    /// A cold cache with room must NOT latch: cold misses are the normal case and
    /// backing off there would disable the optimization exactly when it pays.
    #[test]
    fn a_cache_with_room_never_backs_off() {
        let cache = MetadataCache::new(64 * 1024 * 1024);
        for i in 0..500 {
            cache.put(&format!("k{i}"), 1, dummy_meta());
        }
        assert!(
            !cache.page_index_backed_off(),
            "no eviction, so no back-off"
        );
        assert_eq!(cache.evictions.load(Ordering::Relaxed), 0);
    }

    /// A cache too small for its working set latches, and stays latched: recovery
    /// would oscillate, because backing off is what stops the eviction.
    #[test]
    fn a_churning_cache_backs_off_and_stays_backed_off() {
        // Room for ~2 entries, so almost every insert evicts.
        let cache = MetadataCache::new(2_000);
        for i in 0..(PAGE_INDEX_BACKOFF_EVICTIONS + 20) {
            let meta = dummy_meta();
            let weight = meta.memory_size().max(900);
            let Some(store) = cache.store.as_ref() else {
                unreachable!()
            };
            let evicted = store.lock().unwrap().insert(
                &format!("k{i}"),
                Entry {
                    size: 1,
                    weight,
                    meta,
                },
            );
            cache.note_evictions(evicted as u64);
        }
        assert!(
            cache.page_index_backed_off(),
            "a churning cache must back off; evictions={}",
            cache.evictions.load(Ordering::Relaxed)
        );
        // Latched state survives further quiet activity -- one-way by design.
        cache.note_evictions(0);
        assert!(cache.page_index_backed_off(), "the latch must not clear");
    }

    /// Below the threshold a transient eviction must not disable the feature.
    #[test]
    fn a_single_eviction_does_not_latch() {
        let cache = MetadataCache::new(64 * 1024 * 1024);
        cache.note_evictions(1);
        assert!(!cache.page_index_backed_off());
        cache.note_evictions(PAGE_INDEX_BACKOFF_EVICTIONS - 1);
        assert!(
            !cache.page_index_backed_off(),
            "still at the threshold, not past it"
        );
        cache.note_evictions(1);
        assert!(
            cache.page_index_backed_off(),
            "one past the threshold latches"
        );
    }

    #[test]
    fn page_index_widening_is_inert_when_disabled() {
        assert!(
            page_index_options(None, false).is_none(),
            "disabled must leave the caller's options untouched"
        );
        let skip = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Skip);
        assert!(page_index_options(Some(&skip), false).is_none());
    }

    #[test]
    fn page_index_widening_requests_the_index_when_enabled() {
        let skip = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Skip);
        let widened = page_index_options(Some(&skip), true).expect("should widen a Skip policy");
        assert_eq!(widened.column_index_policy(), PageIndexPolicy::Optional);
        assert_eq!(widened.offset_index_policy(), PageIndexPolicy::Optional);
        // The opener passes Skip explicitly, so `None` must widen too.
        let from_none = page_index_options(None, true).expect("should widen absent options");
        assert_eq!(from_none.column_index_policy(), PageIndexPolicy::Optional);
    }

    /// A caller already asking for the index needs no widening, and cloning its
    /// options would be pure waste on every cold load.
    #[test]
    fn page_index_widening_skips_a_caller_that_already_asked() {
        let already = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required);
        assert!(page_index_options(Some(&already), true).is_none());
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
                &discard_metrics(),
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
                cache
                    .load(&mut *reader, None, key, size, &discard_metrics())
                    .await
                    .unwrap()
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
                cache
                    .load(&mut *reader, None, key, size, &discard_metrics())
                    .await
                    .unwrap()
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
            .load(
                &mut *probe,
                None,
                "iceberg-rust://disabled/probe",
                size,
                &discard_metrics(),
            )
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
                cache
                    .load(&mut *reader, None, key, size, &discard_metrics())
                    .await
                    .unwrap()
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

    #[test]
    fn metadata_size_hint_parses_and_clamps() {
        assert_eq!(parse_metadata_size_hint_kb(None), None);
        assert_eq!(parse_metadata_size_hint_kb(Some("0")), None, "0 disables");
        assert_eq!(parse_metadata_size_hint_kb(Some("garbage")), None);
        assert_eq!(parse_metadata_size_hint_kb(Some("512")), Some(512 * 1024));
        assert_eq!(parse_metadata_size_hint_kb(Some(" 64 ")), Some(64 * 1024));

        assert_eq!(clamp_metadata_size_hint(None, 1_000), None);
        assert_eq!(
            clamp_metadata_size_hint(Some(512 * 1024), 1_000),
            Some(1_000),
            "hint never exceeds the file"
        );
        assert_eq!(
            clamp_metadata_size_hint(Some(1_000), 512 * 1024),
            Some(1_000)
        );
    }

    /// The mechanism the hint exists for: with a suffix hint covering
    /// footer + page index, a cold metadata load (page index included) costs
    /// exactly ONE store GET; without it the reader probes the 8-byte tail
    /// first and pays per piece.
    #[tokio::test]
    async fn size_hint_collapses_cold_metadata_load_to_one_get() {
        let path = "db/tbl/data/hint.parquet";
        let (_inner, factory, size, gets) = counting_setup(path, None).await;
        let widened =
            ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Optional);

        let mut reader = factory
            .create_reader(
                0,
                partitioned_file(path, size),
                None,
                &ExecutionPlanMetricsSet::new(),
            )
            .unwrap();
        reader.get_metadata(Some(&widened)).await.unwrap();
        let unhinted = gets.swap(0, Ordering::SeqCst);
        assert!(
            unhinted >= 2,
            "unhinted cold load pays multiple GETs, saw {unhinted}"
        );

        let mut reader = factory
            .create_reader(
                0,
                partitioned_file(path, size),
                Some(size as usize),
                &ExecutionPlanMetricsSet::new(),
            )
            .unwrap();
        reader.get_metadata(Some(&widened)).await.unwrap();
        let hinted = gets.swap(0, Ordering::SeqCst);
        assert_eq!(hinted, 1, "hinted cold load must be a single suffix GET");
    }
}
