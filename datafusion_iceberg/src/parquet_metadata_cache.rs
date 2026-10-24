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
scan builds, so both cold and warm queries flow through it. Footer metadata
(`get_metadata`) uses this cache. Row-group/page reads optionally use the separate
byte-bounded `parquet_data_cache`; with its default zero capacity they pass
straight through to the underlying reader.

Cold readers must remain independently pollable. A query may prefetch metadata
on a probe input and then stop polling that input while it builds a join whose
other input reads the same file. Waiting on a per-file single-flight gate owned
by the paused probe creates a cycle: build waits for metadata, metadata waits
for probe, probe waits for build. This was reproduced with cold TPC-H SF100 Q2.

Per-file gates therefore never block a reader: a contender rechecks the cache
and, if still cold, independently fetches and parses the immutable footer.
`parquet_metadata_cache_contention_bypasses` exposes this trade. Concurrent cold
readers can issue duplicate I/O, but no task is detached and warm hits retain
the zero-I/O path. The synchronous LRU lock is never held across an await.
The gate map is bounded by [`INFLIGHT_GATES_CAP`]; eviction can only weaken
contention accounting, not correctness. With metadata cap `0`, the footer cache
and its gates are bypassed; the data cache has its own independent capacity.
Retained parsed metadata remains byte-capped; in-flight reads
and query-owned metadata still require separate memory headroom.

[`ParquetSource`]: datafusion::datasource::physical_plan::parquet::source::ParquetSource
*/

use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::parquet_data_cache::{DataCache, DataMetrics, ScanAdmission, DATA_CACHE};
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

/// Upper bound on the number of distinct nonblocking per-file gates kept live
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
    /// Independent loads used instead of waiting for another input's loader.
    contention_bypasses: Count,
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
            contention_bypasses: MetricBuilder::new(metrics)
                .counter("parquet_metadata_cache_contention_bypasses", partition),
        }
    }
}

/// The parsed-footer LRU together with nonblocking per-file load gates.
///
/// A single instance backs the whole process ([`CACHE`]); the tests construct
/// isolated instances (including disabled ones) to drive [`load`] deterministically.
///
/// [`load`]: MetadataCache::load
struct MetadataCache {
    /// Parsed footers, byte-capped. `None` when caching is disabled (cap `0`),
    /// in which case lookups, inserts, and load gates are all
    /// bypassed.
    store: Option<Mutex<ByteCappedCache>>,
    /// Nonblocking per-file gates, keyed exactly like [`ByteCappedCache`]
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

    /// Whether caching and load-gate accounting are active.
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

    /// Return the nonblocking load gate for `key`, creating it on first use. The
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
    /// through `reader` on a miss, without depending on another reader's polling.
    ///
    /// When caching is disabled the fetch is issued straight through with no
    /// gate and no lock. Contenders never wait for an in-flight loader: it may
    /// be a prefetched input that the query parent has stopped polling. They
    /// recheck the cache, then load independently when necessary. The trade is
    /// duplicate cold I/O, not a cross-input dependency or detached background
    /// task. Synchronous LRU locks are never held across the metadata await.
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
        let permit = gate.try_lock().ok();
        // A racing reader may have populated the cache since our first lookup.
        if let Some(meta) = self.get(key, size) {
            metrics.hits.add(1);
            return Ok(meta);
        }
        if permit.is_none() {
            metrics.contention_bypasses.add(1);
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
/// `0` disables caching and load gates entirely).
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

/// Guards the process-wide [`CACHE`] against the benches' cache clearing.
/// Tests anywhere in the crate that expect entries they put there to stay
/// hold the read side for their whole body; the benches hold the write side,
/// so they can never empty the cache under them, even when ignored tests run
/// alongside the rest (`--include-ignored`).
#[cfg(test)]
pub(crate) static PROCESS_CACHE_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Whether the process-wide footer cache is enabled
/// (`ICEBERG_PARQUET_METADATA_CACHE_MB` is not `0`), for test preconditions
/// outside this module.
#[cfg(test)]
pub(crate) fn process_cache_enabled() -> bool {
    CACHE.enabled()
}

/// A [`ParquetFileReaderFactory`] that serves parsed footer metadata from the
/// process-wide [`CACHE`], falling back to an inner factory (the DataFusion
/// default, which reads from the object store) on a miss.
///
/// One factory is built per scan; it carries the scan's object-store URL as a
/// key prefix so cache keys are unique across stores and buckets even though
/// the [`PartitionedFile`] locations it receives are only store-relative.
/// Being per scan, it also carries that scan's data-cache admission budget,
/// which therefore lasts as long as the physical plan: executing the same
/// plan again continues it rather than starting a fresh one.
#[derive(Debug)]
pub(crate) struct CachingParquetFileReaderFactory {
    inner: Arc<dyn ParquetFileReaderFactory>,
    key_prefix: Arc<str>,
    /// The process-wide [`DATA_CACHE`] outside tests.
    data_cache: &'static DataCache,
    /// Shared by every reader of this scan; `None` while the data cache is
    /// disabled, in which case data reads go straight to the inner reader.
    data_scan: Option<Arc<ScanAdmission>>,
}

impl CachingParquetFileReaderFactory {
    /// Build a caching factory over `store`, namespacing cache keys with
    /// `key_prefix` (the scan's object-store URL).
    pub(crate) fn new(store: Arc<dyn ObjectStore>, key_prefix: &str) -> Self {
        Self::with_data_cache(store, key_prefix, &DATA_CACHE)
    }

    fn with_data_cache(
        store: Arc<dyn ObjectStore>,
        key_prefix: &str,
        data_cache: &'static DataCache,
    ) -> Self {
        Self {
            inner: Arc::new(DefaultParquetFileReaderFactory::new(store)),
            key_prefix: Arc::from(key_prefix),
            data_cache,
            data_scan: data_cache.enabled().then(Arc::default),
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
        let data_metrics = DataMetrics::new(metrics, partition_index, self.data_scan.is_some());
        let inner = self.inner.create_reader(
            partition_index,
            partitioned_file,
            metadata_size_hint,
            metrics,
        )?;
        Ok(Box::new(CachingMetadataReader {
            inner,
            key: Arc::from(key),
            size,
            metrics: cache_metrics,
            data_metrics,
            data_cache: self.data_cache,
            data_scan: self.data_scan.clone(),
        }))
    }
}

/// [`AsyncFileReader`] decorator that serves `get_metadata` from the
/// process-wide cache and forwards data reads to `inner`.
struct CachingMetadataReader {
    inner: Box<dyn AsyncFileReader + Send>,
    key: Arc<str>,
    size: u64,
    metrics: CacheMetrics,
    data_metrics: DataMetrics,
    data_cache: &'static DataCache,
    data_scan: Option<Arc<ScanAdmission>>,
}

impl AsyncFileReader for CachingMetadataReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, ParquetResult<Bytes>> {
        let Self {
            inner,
            key,
            size,
            data_metrics,
            data_cache,
            data_scan,
            ..
        } = self;
        let Some(scan) = data_scan.as_deref() else {
            return inner.get_bytes(range);
        };
        async move {
            let mut bytes = data_cache
                .read(&mut **inner, key, *size, vec![range], scan, data_metrics)
                .await?;
            bytes.pop().ok_or_else(|| {
                datafusion::parquet::errors::ParquetError::General(
                    "Missing Parquet byte range".into(),
                )
            })
        }
        .boxed()
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, ParquetResult<Vec<Bytes>>> {
        let Some(scan) = self.data_scan.as_deref() else {
            return self.inner.get_byte_ranges(ranges);
        };
        self.data_cache
            .read(
                &mut *self.inner,
                &self.key,
                self.size,
                ranges,
                scan,
                &self.data_metrics,
            )
            .boxed()
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
    use std::time::{Duration, Instant};

    use datafusion::arrow::array::{ArrayRef, Int32Array, Int64Array, RecordBatch};
    use datafusion::arrow::datatypes::SchemaRef;
    use datafusion::common::ScalarValue;
    use datafusion::datasource::physical_plan::{FileGroup, FileScanConfigBuilder, ParquetSource};
    use datafusion::datasource::source::DataSourceExec;
    use datafusion::execution::object_store::ObjectStoreUrl;
    use datafusion::logical_expr::Operator;
    use datafusion::parquet::arrow::ArrowWriter;
    use datafusion::parquet::file::metadata::FileMetaData;
    use datafusion::parquet::file::properties::WriterProperties;
    use datafusion::parquet::schema::types::{SchemaDescriptor, Type};
    use datafusion::physical_expr::expressions::{BinaryExpr, Column, Literal};
    use datafusion::physical_plan::{collect, ExecutionPlan};
    use datafusion::prelude::{SessionConfig, SessionContext};
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetRange, GetResult, ListResult, MultipartUpload, ObjectMeta,
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
        let _shared = PROCESS_CACHE_LOCK.blocking_read();
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
    /// readers overlap, exercising cold-read contention. `probe` additionally
    /// tracks GET concurrency and splits GETs of registered files into metadata
    /// and data reads.
    #[derive(Debug)]
    struct CountingStore {
        inner: Arc<dyn ObjectStore>,
        gets: Arc<AtomicUsize>,
        delay: Option<Duration>,
        probe: Arc<GetProbe>,
    }

    /// Per-GET accounting shared between a [`CountingStore`] and the test that
    /// drives it.
    ///
    /// HEAD requests (`get_opts` with `head` set) are only counted, in
    /// `heads`. Every other GET counts towards the in-flight gauge (held from
    /// request start, including the injected delay, until the inner store
    /// answers). GETs of a file registered with [`GetProbe::register`] are also
    /// classified by byte range: Parquet places the page index and the footer
    /// after the last column chunk, so a range that starts at or past the end
    /// of column-chunk data, or that reaches the end of the file (the 8-byte
    /// tail probe and any hinted suffix read), is a metadata GET; anything else
    /// is a data-range GET.
    #[derive(Debug, Default)]
    struct GetProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        metadata: AtomicUsize,
        data: AtomicUsize,
        heads: AtomicUsize,
        /// `(end of column-chunk data, file size)` per registered location.
        layouts: Mutex<std::collections::HashMap<String, (u64, u64)>>,
    }

    /// GET counts accumulated since the previous [`GetProbe::take`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct GetCounts {
        metadata: usize,
        data: usize,
        heads: usize,
        max_in_flight: usize,
    }

    impl GetProbe {
        fn register(&self, location: &str, data_end: u64, size: u64) {
            self.layouts
                .lock()
                .unwrap()
                .insert(location.to_string(), (data_end, size));
        }

        /// Classify one GET of `location`. Unregistered locations are only
        /// counted by the gauge.
        fn record(&self, location: &ObjectPath, range: Option<&GetRange>) {
            let Some((data_end, size)) =
                self.layouts.lock().unwrap().get(location.as_ref()).copied()
            else {
                return;
            };
            let metadata = match range {
                Some(GetRange::Suffix(_) | GetRange::Offset(_)) => true,
                Some(GetRange::Bounded(r)) => r.start >= data_end || r.end >= size,
                None => false,
            };
            let counter = if metadata { &self.metadata } else { &self.data };
            counter.fetch_add(1, Ordering::SeqCst);
        }

        /// Account for one `get_opts` call. A HEAD is only counted; a GET is
        /// classified and enters the in-flight gauge, which the returned guard
        /// leaves on drop, so a cancelled request does not leak a slot.
        fn begin(&self, location: &ObjectPath, options: &GetOptions) -> Option<InFlight<'_>> {
            if options.head {
                self.heads.fetch_add(1, Ordering::SeqCst);
                return None;
            }
            self.record(location, options.range.as_ref());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            Some(InFlight(&self.in_flight))
        }

        /// Counts since the previous call; resets them for the next measurement.
        fn take(&self) -> GetCounts {
            GetCounts {
                metadata: self.metadata.swap(0, Ordering::SeqCst),
                data: self.data.swap(0, Ordering::SeqCst),
                heads: self.heads.swap(0, Ordering::SeqCst),
                max_in_flight: self
                    .max_in_flight
                    .swap(self.in_flight.load(Ordering::SeqCst), Ordering::SeqCst),
            }
        }
    }

    struct InFlight<'a>(&'a AtomicUsize);

    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
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
            let _in_flight = self.probe.begin(location, &options);
            if let Some(delay) = self.delay {
                // Yield while "fetching" so cold readers overlap before any
                // reader inserts its result.
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
        let _shared = PROCESS_CACHE_LOCK.read().await;
        let inner = Arc::new(InMemory::new()) as Arc<dyn ObjectStore>;
        let path = "db/tbl/data/warm.parquet";
        let size = write_parquet(&inner, path).await;

        let gets = Arc::new(AtomicUsize::new(0));
        let counting = Arc::new(CountingStore {
            inner,
            gets: Arc::clone(&gets),
            delay: None,
            probe: Arc::default(),
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
            probe: Arc::default(),
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

    #[tokio::test]
    async fn paused_cold_reader_cannot_block_an_independently_polled_reader() {
        let path = "db/tbl/data/paused_leader.parquet";
        let (_inner, factory, size, _gets) =
            counting_setup(path, Some(Duration::from_millis(1))).await;
        let cache = MetadataCache::new(64 * 1024 * 1024);
        let plan_metrics = ExecutionPlanMetricsSet::new();
        let key = "iceberg-rust://paused-leader/file";
        let mut first = factory
            .create_reader(0, partitioned_file(path, size), None, &plan_metrics)
            .unwrap();
        let mut second = factory
            .create_reader(0, partitioned_file(path, size), None, &plan_metrics)
            .unwrap();
        let first_metrics = discard_metrics();
        let second_metrics = discard_metrics();
        let mut paused = Box::pin(cache.load(&mut *first, None, key, size, &first_metrics));
        assert!(paused.as_mut().now_or_never().is_none());
        assert!(
            cache.inflight_gate(key).try_lock().is_err(),
            "the first load is paused while holding the per-file gate"
        );

        // A query parent may stop polling its prefetched probe input while it
        // consumes a build input reading the same immutable file. The cache
        // must not introduce a dependency on that paused probe future.
        let completed = tokio::time::timeout(
            Duration::from_millis(100),
            cache.load(&mut *second, None, key, size, &second_metrics),
        )
        .await
        .expect("a parked metadata loader must not deadlock another query input")
        .unwrap();
        assert_eq!(completed.file_metadata().num_rows(), 3);
        assert_eq!(second_metrics.contention_bypasses.value(), 1);
        let resumed = paused.await.unwrap();
        assert_eq!(resumed.as_ref(), completed.as_ref());
        assert_eq!(cache.stats().unwrap().0, 1);
    }

    /// Racing cold readers may load independently, but values and the cache
    /// byte bound remain exact. This deliberately replaces the old one-fetch
    /// performance guarantee, whose blocking gate could deadlock a query.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_reads_preserve_metadata_and_bounded_cache() {
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

        let actual_gets = gets.load(Ordering::SeqCst);
        assert!(actual_gets >= per_fetch && actual_gets <= per_fetch * n);
        for meta in &metas[1..] {
            assert_eq!(metas[0].as_ref(), meta.as_ref());
        }
        let (entries, bytes) = cache.stats().unwrap();
        assert_eq!(entries, 1);
        assert!(bytes <= 64 * 1024 * 1024);
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
        let widened = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Optional);

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

    /// Footer and data cache keys are prefixed with the scan's object-store
    /// URL, so a URL no earlier scan used makes the next scan cold even though
    /// [`CACHE`] and the data cache live for the whole process.
    fn fresh_store_url(tag: &str) -> ObjectStoreUrl {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        ObjectStoreUrl::parse(format!("iceberg-rust://{tag}-{run}")).unwrap()
    }

    /// Empty the process-wide footer cache without counting the dropped
    /// entries as evictions, so every bench configuration starts from the
    /// same state and earlier configurations cannot latch the page-index
    /// backoff for later ones. Requires the write side of
    /// [`PROCESS_CACHE_LOCK`], so no test relying on [`CACHE`] runs meanwhile.
    fn clear_process_cache(_exclusive: &tokio::sync::RwLockWriteGuard<'static, ()>) {
        if let Some(store) = CACHE.store.as_ref() {
            let mut cache = store.lock().unwrap();
            cache.entries.clear();
            cache.total_bytes = 0;
        }
    }

    const OPEN_AHEAD_KEY: &str = "datafusion.execution.file_stream_open_ahead";

    /// Whether the DataFusion this crate is built against has the per-stream
    /// open-ahead option. Probed through the string config API so the tests
    /// compile against builds with and without it.
    fn open_ahead_supported() -> bool {
        let mut config = SessionConfig::new();
        config.options_mut().set(OPEN_AHEAD_KEY, "true").is_ok()
    }

    /// Layout of every file in a [`ScanFixture`]: `rows` rows of
    /// `(id: 0..rows, file: index)` in row groups of `rows_per_group`.
    #[derive(Debug, Clone, Copy)]
    struct FileShape {
        name: &'static str,
        rows: usize,
        rows_per_group: usize,
        /// Scan predicate `id = probe_id`; row-group statistics resolve it to a
        /// single group per file. `None` scans every row.
        probe_id: Option<i64>,
    }

    /// One small row group per file: the scan is dominated by per-file opens.
    const SMALL_FILES: FileShape = FileShape {
        name: "small",
        rows: 1_000,
        rows_per_group: 1_000,
        probe_id: None,
    };

    /// 100 row groups per file (100x the row-group and page-index metadata)
    /// under a predicate that keeps one group, so data reads stay at one range
    /// per file and the metadata cost is what varies.
    const HUNDRED_GROUP_FILES: FileShape = FileShape {
        name: "100-rg",
        rows: 10_000,
        rows_per_group: 100,
        probe_id: Some(4_250),
    };

    /// Parquet files of one [`FileShape`] in an in-memory store, read through a
    /// [`CountingStore`] whose probe knows every file's layout.
    struct ScanFixture {
        store: Arc<dyn ObjectStore>,
        probe: Arc<GetProbe>,
        files: Vec<(String, u64)>,
        schema: SchemaRef,
        shape: FileShape,
    }

    /// One scan's output, wall time, GET counts and data-cache counters.
    struct ScanRun {
        batches: Vec<RecordBatch>,
        wall: Duration,
        gets: GetCounts,
        data: DataCacheCounts,
    }

    /// Data-cache plan metrics of one scan, summed over partitions. `None` for
    /// a counter the scan did not register (the cache is off, or the build
    /// predates the counter).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct DataCacheCounts {
        misses: Option<usize>,
        hit_bytes: Option<usize>,
        miss_bytes: Option<usize>,
        evictions: Option<usize>,
        admission_skips: Option<usize>,
    }

    impl DataCacheCounts {
        fn of(plan: &dyn ExecutionPlan) -> Self {
            let metrics = plan.metrics().unwrap_or_default();
            let sum = |name: &str| metrics.sum_by_name(name).map(|value| value.as_usize());
            Self {
                misses: sum("parquet_data_cache_misses"),
                hit_bytes: sum("parquet_data_cache_hit_bytes"),
                miss_bytes: sum("parquet_data_cache_miss_bytes"),
                evictions: sum("parquet_data_cache_evictions"),
                admission_skips: sum("parquet_data_cache_admission_skips"),
            }
        }
    }

    impl ScanFixture {
        async fn build(files: usize, shape: FileShape, delay: Option<Duration>) -> Self {
            let inner = Arc::new(InMemory::new()) as Arc<dyn ObjectStore>;
            let probe = Arc::new(GetProbe::default());
            let rows = i64::try_from(shape.rows).unwrap();
            let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows));
            let props = WriterProperties::builder()
                .set_max_row_group_row_count(Some(shape.rows_per_group))
                .build();
            let mut written = Vec::with_capacity(files);
            let mut schema = None;
            for index in 0..files {
                let file: ArrayRef = Arc::new(Int64Array::from(vec![
                    i64::try_from(index).unwrap();
                    shape.rows
                ]));
                let batch =
                    RecordBatch::try_from_iter(vec![("id", Arc::clone(&ids)), ("file", file)])
                        .unwrap();
                let mut buf = Vec::new();
                let mut writer =
                    ArrowWriter::try_new(&mut buf, batch.schema(), Some(props.clone())).unwrap();
                writer.write(&batch).unwrap();
                let meta = writer.close().unwrap();
                assert_eq!(
                    meta.num_row_groups(),
                    shape.rows / shape.rows_per_group,
                    "fixture row-group layout"
                );
                let data_end = meta
                    .row_groups()
                    .iter()
                    .flat_map(|group| group.columns())
                    .map(|column| {
                        let (start, len) = column.byte_range();
                        start + len
                    })
                    .max()
                    .unwrap();
                let path = format!("db/tbl/data/{index:05}.parquet");
                let size = buf.len() as u64;
                probe.register(&path, data_end, size);
                inner
                    .put(&ObjectPath::from(path.as_str()), PutPayload::from(buf))
                    .await
                    .unwrap();
                schema.get_or_insert_with(|| batch.schema());
                written.push((path, size));
            }
            let store = Arc::new(CountingStore {
                inner,
                gets: Arc::default(),
                delay,
                probe: Arc::clone(&probe),
            }) as Arc<dyn ObjectStore>;
            Self {
                store,
                probe,
                files: written,
                schema: schema.unwrap(),
                shape,
            }
        }

        /// Scan every file once through `DataSourceExec` with the caching
        /// reader factory a table scan installs, spreading the files over
        /// `partitions` file groups (siblings share them by work stealing).
        /// `hint` is stamped on each `PartitionedFile`, clamped to the file
        /// size the way table scans clamp the configured hint. Only plan
        /// execution is timed.
        async fn scan(
            &self,
            url: &ObjectStoreUrl,
            partitions: usize,
            hint: Option<usize>,
            open_ahead: bool,
        ) -> ScanRun {
            self.scan_through(url, partitions, hint, open_ahead, &DATA_CACHE)
                .await
        }

        /// [`Self::scan`] with its data reads served by `data_cache`.
        async fn scan_through(
            &self,
            url: &ObjectStoreUrl,
            partitions: usize,
            hint: Option<usize>,
            open_ahead: bool,
            data_cache: &'static DataCache,
        ) -> ScanRun {
            let mut config = SessionConfig::new().with_target_partitions(partitions);
            if open_ahead {
                config
                    .options_mut()
                    .set(OPEN_AHEAD_KEY, "true")
                    .expect("open-ahead requested on a build that supports it");
            }
            let ctx = SessionContext::new_with_config(config);
            ctx.register_object_store(url.as_ref(), Arc::clone(&self.store));
            let factory = Arc::new(CachingParquetFileReaderFactory::with_data_cache(
                Arc::clone(&self.store),
                url.as_str(),
                data_cache,
            ));
            let mut source = ParquetSource::new(Arc::clone(&self.schema))
                .with_parquet_file_reader_factory(factory);
            if let Some(id) = self.shape.probe_id {
                source = source.with_predicate(Arc::new(BinaryExpr::new(
                    Arc::new(Column::new("id", 0)),
                    Operator::Eq,
                    Arc::new(Literal::new(ScalarValue::Int64(Some(id)))),
                )));
            }
            let mut groups = vec![Vec::new(); partitions];
            for (index, (path, size)) in self.files.iter().enumerate() {
                let mut file = PartitionedFile::new(path.clone(), *size);
                file.metadata_size_hint = clamp_metadata_size_hint(hint, *size);
                groups[index % partitions].push(file);
            }
            let scan_config = FileScanConfigBuilder::new(url.clone(), Arc::new(source))
                .with_file_groups(groups.into_iter().map(FileGroup::new).collect())
                .build();
            let plan: Arc<dyn ExecutionPlan> = DataSourceExec::from_data_source(scan_config);
            let task_ctx = ctx.task_ctx();
            self.probe.take();
            let started = Instant::now();
            let batches = collect(Arc::clone(&plan), task_ctx).await.unwrap();
            let wall = started.elapsed();
            ScanRun {
                batches,
                wall,
                gets: self.probe.take(),
                data: DataCacheCounts::of(plan.as_ref()),
            }
        }

        /// Assert a scan returned exactly the fixture's expected rows.
        fn check(&self, batches: &[RecordBatch]) {
            let column_sum = |name: &str| -> i64 {
                batches
                    .iter()
                    .map(|batch| {
                        let column = batch.column_by_name(name).unwrap();
                        let values = column.as_any().downcast_ref::<Int64Array>().unwrap();
                        values.values().iter().sum::<i64>()
                    })
                    .sum()
            };
            let ids = || {
                batches.iter().flat_map(|batch| {
                    let column = batch.column_by_name("id").unwrap();
                    let values = column.as_any().downcast_ref::<Int64Array>().unwrap();
                    values.values().to_vec()
                })
            };
            let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
            let files = self.files.len();
            let (per_file, id_range) = match self.shape.probe_id {
                None => (self.shape.rows, 0..i64::try_from(self.shape.rows).unwrap()),
                Some(id) => {
                    let group = i64::try_from(self.shape.rows_per_group).unwrap();
                    let start = id - id % group;
                    (self.shape.rows_per_group, start..start + group)
                }
            };
            assert_eq!(rows, files * per_file, "scan row count");
            assert!(
                ids().all(|id| id_range.contains(&id)),
                "ids outside {id_range:?}"
            );
            let id_sum: i64 = id_range.clone().sum();
            let files_i64 = i64::try_from(files).unwrap();
            assert_eq!(column_sum("id"), id_sum * files_i64, "id checksum");
            let per_file_i64 = i64::try_from(per_file).unwrap();
            assert_eq!(
                column_sum("file"),
                per_file_i64 * files_i64 * (files_i64 - 1) / 2,
                "every file read exactly once"
            );
        }
    }

    /// A cold scan with no metadata size hint, through the same
    /// `DataSourceExec` and caching reader factory a table scan uses, pays
    /// exactly three metadata GETs per file: the 8-byte footer tail, the
    /// footer body, and the page index the cold load is widened to fetch. The
    /// page-index GET exists only while
    /// `ICEBERG_PARQUET_METADATA_CACHE_PAGE_INDEX` keeps its default (on) and
    /// the eviction backoff has not latched; otherwise the unfiltered scan
    /// pays two. A warm repeat pays none, which needs the footer cache, so
    /// `ICEBERG_PARQUET_METADATA_CACHE_MB=0` is the one setting this test
    /// rejects. Data-range GETs are counted separately: with the data cache
    /// at its default (off) the warm scan pays them again. The scan issues no
    /// HEAD requests: every `PartitionedFile` carries its size.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_scan_get_count_per_file() {
        let _shared = PROCESS_CACHE_LOCK.read().await;
        assert!(
            CACHE.enabled(),
            "precondition: ICEBERG_PARQUET_METADATA_CACHE_MB is not 0"
        );
        let page_index_gets = usize::from(*CACHE_PAGE_INDEX && !CACHE.page_index_backed_off());
        let files = 4;
        let fixture = ScanFixture::build(files, SMALL_FILES, None).await;
        let url = fresh_store_url("cold-scan-get-count");

        let cold = fixture.scan(&url, 2, None, false).await;
        fixture.check(&cold.batches);
        assert_eq!(
            cold.gets.metadata,
            (2 + page_index_gets) * files,
            "cold, unhinted: tail + footer (+ page index when cached) per file, saw {:?}",
            cold.gets
        );
        assert_eq!(cold.gets.heads, 0, "no HEAD requests, saw {:?}", cold.gets);
        assert!(
            cold.gets.data >= files,
            "every file reads its column chunks, saw {:?}",
            cold.gets
        );
        // Hit and miss counters are always registered; the guard counters
        // only while the data cache is on, and this small scan fits any
        // enabled cap.
        assert!(cold.data.hit_bytes.is_some() && cold.data.miss_bytes.is_some());
        assert_eq!(cold.data.evictions.is_some(), DATA_CACHE.enabled());
        assert_eq!(cold.data.admission_skips.is_some(), DATA_CACHE.enabled());
        if DATA_CACHE.enabled() {
            assert_eq!(cold.data.admission_skips, Some(0));
        }

        let warm = fixture.scan(&url, 2, None, false).await;
        fixture.check(&warm.batches);
        assert_eq!(
            warm.gets.metadata, 0,
            "warm repeat is served from the footer cache, saw {:?}",
            warm.gets
        );
        assert_eq!(warm.gets.heads, 0, "no HEAD requests, saw {:?}", warm.gets);
        if !DATA_CACHE.enabled() {
            assert_eq!(
                warm.gets.data, cold.gets.data,
                "without the data cache warm data reads match cold ones"
            );
        }
    }

    /// One admission budget per table scan, shared by every reader: the
    /// caching factory is built once per scan and creates a reader per file
    /// and partition, so a budget per reader would let each file (each well
    /// under the cap here) be admitted in full and churn the cache like a
    /// plain LRU. Uses its own small data cache, so it runs whatever
    /// `ICEBERG_PARQUET_DATA_CACHE_MB` says.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_data_cache_budget_per_table_scan() {
        let _shared = PROCESS_CACHE_LOCK.read().await;
        let files = 8;
        let fixture = ScanFixture::build(files, SMALL_FILES, None).await;
        // A reference scan through a cache that holds everything measures
        // the data bytes and admits them all.
        let roomy: &'static DataCache = Box::leak(Box::new(DataCache::new(1 << 30)));
        let reference = fixture
            .scan_through(&fresh_store_url("budget-reference"), 4, None, false, roomy)
            .await;
        fixture.check(&reference.batches);
        assert_eq!(reference.data.admission_skips, Some(0));
        let data = reference.data.miss_bytes.unwrap();
        // A cap of about three files' data: every file fits, the scan does not.
        let cap = data * 3 / files;
        let cache: &'static DataCache = Box::leak(Box::new(DataCache::new(cap)));
        for tag in ["budget-first", "budget-second"] {
            let run = fixture
                .scan_through(&fresh_store_url(tag), 4, None, false, cache)
                .await;
            fixture.check(&run.batches);
            let misses = run.data.misses.unwrap();
            let skips = run.data.admission_skips.unwrap();
            assert!(
                skips > 0,
                "{tag}: the scan's shared budget declined nothing"
            );
            assert!(
                skips < misses,
                "{tag}: a new scan admits again, saw {:?}",
                run.data
            );
            assert!(cache.used_for_tests() <= cap);
        }
    }

    /// Scan-latency instrument: prints, per configuration, metadata GETs per
    /// file, data-range GETs, peak concurrent GETs, and cold/warm wall time of
    /// one `DataSourceExec` scan over an in-memory store whose every GET
    /// sleeps 20 ms. Each configuration is measured [`BENCH_REPEATS`] times;
    /// the table reports the median wall time and the min-max range, the
    /// largest peak in-flight count, and GET counts that must agree across
    /// repeats. Results are checked after the timer stops.
    ///
    /// Varied in-process: file count (64, 256), file shape ([`SMALL_FILES`],
    /// [`HUNDRED_GROUP_FILES`]), `target_partitions` (1, 4, 8), the per-file
    /// metadata size hint (none, 512 KiB via `PartitionedFile`), and
    /// open-ahead (`datafusion.execution.file_stream_open_ahead`) when the
    /// DataFusion build has that option; the header says whether it does.
    ///
    /// Each repeat empties the process-wide footer cache and scans under a
    /// fresh object-store URL, so its first scan is cold and its second, under
    /// the same URL, is warm. Cache capacities and the page-index switch
    /// (`ICEBERG_PARQUET_METADATA_CACHE_MB`, `ICEBERG_PARQUET_DATA_CACHE_MB`,
    /// `ICEBERG_PARQUET_METADATA_CACHE_PAGE_INDEX`) are read once per process
    /// by `LazyLock` statics and cannot be varied here; run one process per
    /// value instead, e.g.
    /// `ICEBERG_PARQUET_DATA_CACHE_MB=256 cargo test -p datafusion_iceberg --lib
    /// parquet_metadata_cache::tests::scan_latency_bench -- --ignored --exact --nocapture`.
    ///
    /// Coverage limits, so the table is not over-read:
    /// - The cost model is latency only: every GET costs the same flat delay
    ///   whatever its size, so bytes transferred are not measured.
    /// - Every fixture file is smaller than the 512 KiB hint, so a hinted
    ///   metadata GET is a suffix read of the whole file (and the data GET
    ///   then reads its row group again). A hint smaller than footer plus page
    ///   index, which needs a second metadata GET, is not exercised.
    /// - The scan keeps `ParquetSource`'s default `pushdown_filters` (off),
    ///   whereas table scans configure it; the [`HUNDRED_GROUP_FILES`]
    ///   predicate prunes row groups by statistics but does not exercise the
    ///   row-filter read path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "latency benchmark that prints a table; run alone with \
                `-- --ignored --exact parquet_metadata_cache::tests::scan_latency_bench --nocapture`"]
    async fn scan_latency_bench() {
        const DELAY: Duration = Duration::from_millis(20);
        const HINT: usize = 512 * 1024;
        let exclusive = PROCESS_CACHE_LOCK.write().await;
        let open_ahead_modes: &[bool] = if open_ahead_supported() {
            &[false, true]
        } else {
            &[false]
        };
        println!(
            "scan_latency_bench: {} ms per GET; metadata cache {}; data cache {}; \
             page-index caching {}; open-ahead {}; times are the median (and min-max) \
             of {BENCH_REPEATS} repeats",
            DELAY.as_millis(),
            CACHE.store.as_ref().map_or("off".to_string(), |s| format!(
                "{} MiB",
                s.lock().unwrap().cap_bytes / (1024 * 1024)
            )),
            if DATA_CACHE.enabled() { "on" } else { "off" },
            if *CACHE_PAGE_INDEX { "on" } else { "off" },
            if open_ahead_supported() {
                "available (measured off and on)"
            } else {
                "unavailable in this DataFusion build (every row is without open-ahead)"
            },
        );
        println!(
            "| shape | files | P | hint | open-ahead | cold meta GET/file | cold data GETs \
             | cold max in-flight | cold ms | cold ms range | warm meta GET/file \
             | warm data GETs | warm max in-flight | warm ms | warm ms range | evictions \
             | page index |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for shape in [SMALL_FILES, HUNDRED_GROUP_FILES] {
            for files in [64, 256] {
                let fixture = ScanFixture::build(files, shape, Some(DELAY)).await;
                for &open_ahead in open_ahead_modes {
                    for hint in [None, Some(HINT)] {
                        for partitions in [1, 4, 8] {
                            let evictions = CACHE.evictions.load(Ordering::Relaxed);
                            let mut cold = BenchCell::default();
                            let mut warm = BenchCell::default();
                            for _ in 0..BENCH_REPEATS {
                                clear_process_cache(&exclusive);
                                let url = fresh_store_url("scan-latency-bench");
                                let cold_run =
                                    fixture.scan(&url, partitions, hint, open_ahead).await;
                                let warm_run =
                                    fixture.scan(&url, partitions, hint, open_ahead).await;
                                fixture.check(&cold_run.batches);
                                fixture.check(&warm_run.batches);
                                cold.push(&cold_run);
                                warm.push(&warm_run);
                            }
                            let (cold_gets, warm_gets) = (cold.gets(), warm.gets());
                            assert_eq!(
                                (cold_gets.heads, warm_gets.heads),
                                (0, 0),
                                "no HEAD requests"
                            );
                            let per_file = |n: usize| n as f64 / files as f64;
                            println!(
                                "| {} | {files} | {partitions} | {} | {} | {:.2} | {} | {} | {} \
                                 | {} | {:.2} | {} | {} | {} | {} | {} | {} |",
                                shape.name,
                                hint.map_or("none".to_string(), |h| format!("{} KiB", h / 1024)),
                                if open_ahead { "on" } else { "off" },
                                per_file(cold_gets.metadata),
                                cold_gets.data,
                                cold_gets.max_in_flight,
                                cold.median_ms(),
                                cold.range_ms(),
                                per_file(warm_gets.metadata),
                                warm_gets.data,
                                warm_gets.max_in_flight,
                                warm.median_ms(),
                                warm.range_ms(),
                                CACHE.evictions.load(Ordering::Relaxed) - evictions,
                                if CACHE.page_index_backed_off() {
                                    "backed off"
                                } else {
                                    "cached"
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    /// Repeats of every [`scan_latency_bench`] configuration.
    const BENCH_REPEATS: usize = 3;

    /// Four 64 Ki-row groups of two `Int64` columns per file, scanned in full:
    /// about 2.5 MiB of column chunks per file, read as data ranges.
    const DATA_CACHE_FILES: FileShape = FileShape {
        name: "4x64Ki",
        rows: 262_144,
        rows_per_group: 65_536,
        probe_id: None,
    };

    /// Repeats of every [`data_cache_bench`] configuration.
    const DATA_CACHE_REPEATS: usize = 5;

    /// Data-cache instrument: cold and warm wall time, data-range GETs, hit
    /// and miss bytes, evictions and admission skips of full scans of 16
    /// [`DATA_CACHE_FILES`] (about 40 MiB of data ranges) through the caching
    /// reader factory. Each repeat runs the scan four times: cold, warm, and
    /// two steady-state repeats (`s3`, `s4`) that show whether hits hold
    /// beyond the first warm scan or alternate. Each configuration is measured [`DATA_CACHE_REPEATS`]
    /// times and reports the median and the min-max range; byte and GET
    /// columns are the median too. Results are checked after the timer stops.
    ///
    /// Varied in-process: the per-GET delay (20 ms, a latency-bound store;
    /// none, where the in-memory store's zero-copy reads make the cache's
    /// copies and locking the visible cost), `target_partitions` (1, 4) and
    /// the data-cache state each repeat starts from: `empty` (cleared) or
    /// `previous` (still holding the previous repeat's ranges, which its warm
    /// scan reused, so the cold scan must displace another working set).
    /// Every repeat empties the footer cache and scans under a fresh
    /// object-store URL, so its first scan is cold and the later ones warm.
    ///
    /// The data-cache capacity is a process-wide `LazyLock` read once from
    /// `ICEBERG_PARQUET_DATA_CACHE_MB`; run one process per value, e.g. `0`
    /// (off), `64` (the working set fits) and `8` (about a fifth of it):
    /// `ICEBERG_PARQUET_DATA_CACHE_MB=8 cargo test -p datafusion_iceberg --lib
    /// parquet_metadata_cache::tests::data_cache_bench -- --ignored --exact --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "data-cache benchmark that prints a table; run alone with \
                `-- --ignored --exact parquet_metadata_cache::tests::data_cache_bench --nocapture`"]
    async fn data_cache_bench() {
        const FILES: usize = 16;
        let exclusive = PROCESS_CACHE_LOCK.write().await;
        let mib = |bytes: Option<usize>| {
            bytes.map_or("n/a".to_string(), |b| {
                format!("{:.1}", b as f64 / 1048576.0)
            })
        };
        let count = |n: Option<usize>| n.map_or("n/a".to_string(), |n| n.to_string());
        println!(
            "data_cache_bench: ICEBERG_PARQUET_DATA_CACHE_MB={}; data cache {}; {FILES} files \
             of {}; times are the median (and min-max) of {DATA_CACHE_REPEATS} repeats; \
             GET, byte and counter columns are medians",
            std::env::var("ICEBERG_PARQUET_DATA_CACHE_MB").unwrap_or_else(|_| "unset".into()),
            if DATA_CACHE.enabled() { "on" } else { "off" },
            DATA_CACHE_FILES.name,
        );
        println!(
            "| GET delay | P | start | cold ms | cold ms range | cold data GETs | cold miss MiB \
             | cold evictions | cold skips | warm ms | warm ms range | warm data GETs \
             | warm hit MiB | warm miss MiB | warm evictions | warm skips | s3 hit MiB \
             | s4 ms | s4 ms range | s4 data GETs | s4 hit MiB | s4 evictions | s4 skips |"
        );
        println!(
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\
             ---|---|---|"
        );
        for delay in [Some(Duration::from_millis(20)), None] {
            let fixture = ScanFixture::build(FILES, DATA_CACHE_FILES, delay).await;
            for partitions in [1, 4] {
                for carry in [false, true] {
                    let mut cold = Vec::with_capacity(DATA_CACHE_REPEATS);
                    let mut warm = Vec::with_capacity(DATA_CACHE_REPEATS);
                    let mut third = Vec::with_capacity(DATA_CACHE_REPEATS);
                    let mut fourth = Vec::with_capacity(DATA_CACHE_REPEATS);
                    // The `previous` cells start from a primed working set.
                    DATA_CACHE.clear_for_tests();
                    if carry {
                        let url = fresh_store_url("data-cache-bench-prime");
                        fixture.scan(&url, partitions, None, false).await;
                        fixture.scan(&url, partitions, None, false).await;
                    }
                    for _ in 0..DATA_CACHE_REPEATS {
                        clear_process_cache(&exclusive);
                        if !carry {
                            DATA_CACHE.clear_for_tests();
                        }
                        let url = fresh_store_url("data-cache-bench");
                        let cold_run = fixture.scan(&url, partitions, None, false).await;
                        let warm_run = fixture.scan(&url, partitions, None, false).await;
                        let third_run = fixture.scan(&url, partitions, None, false).await;
                        let fourth_run = fixture.scan(&url, partitions, None, false).await;
                        for run in [&cold_run, &warm_run, &third_run, &fourth_run] {
                            fixture.check(&run.batches);
                        }
                        cold.push(cold_run);
                        warm.push(warm_run);
                        third.push(third_run);
                        fourth.push(fourth_run);
                    }
                    let ms = |runs: &[ScanRun]| {
                        let mut ms: Vec<f64> =
                            runs.iter().map(|r| r.wall.as_secs_f64() * 1e3).collect();
                        ms.sort_by(f64::total_cmp);
                        (
                            format!("{:.1}", ms[ms.len() / 2]),
                            format!("{:.1}-{:.1}", ms[0], ms[ms.len() - 1]),
                        )
                    };
                    let median = |runs: &[ScanRun], f: &dyn Fn(&ScanRun) -> Option<usize>| {
                        let mut values: Vec<Option<usize>> = runs.iter().map(f).collect();
                        values.sort();
                        values[values.len() / 2]
                    };
                    let data_gets = |r: &ScanRun| Some(r.gets.data);
                    let (cold_ms, cold_range) = ms(&cold);
                    let (warm_ms, warm_range) = ms(&warm);
                    let (fourth_ms, fourth_range) = ms(&fourth);
                    println!(
                        "| {} | {partitions} | {} | {cold_ms} | {cold_range} | {} | {} | {} | {} \
                         | {warm_ms} | {warm_range} | {} | {} | {} | {} | {} | {} \
                         | {fourth_ms} | {fourth_range} | {} | {} | {} | {} |",
                        delay.map_or("none".to_string(), |d| format!("{} ms", d.as_millis())),
                        if carry { "previous" } else { "empty" },
                        count(median(&cold, &data_gets)),
                        mib(median(&cold, &|r| r.data.miss_bytes)),
                        count(median(&cold, &|r| r.data.evictions)),
                        count(median(&cold, &|r| r.data.admission_skips)),
                        count(median(&warm, &data_gets)),
                        mib(median(&warm, &|r| r.data.hit_bytes)),
                        mib(median(&warm, &|r| r.data.miss_bytes)),
                        count(median(&warm, &|r| r.data.evictions)),
                        count(median(&warm, &|r| r.data.admission_skips)),
                        mib(median(&third, &|r| r.data.hit_bytes)),
                        count(median(&fourth, &data_gets)),
                        mib(median(&fourth, &|r| r.data.hit_bytes)),
                        count(median(&fourth, &|r| r.data.evictions)),
                        count(median(&fourth, &|r| r.data.admission_skips)),
                    );
                }
            }
        }
        DATA_CACHE.clear_for_tests();
    }

    /// The cold or the warm scans of one bench configuration across repeats.
    #[derive(Default)]
    struct BenchCell {
        walls: Vec<Duration>,
        gets: Vec<GetCounts>,
    }

    impl BenchCell {
        fn push(&mut self, run: &ScanRun) {
            self.walls.push(run.wall);
            self.gets.push(run.gets);
        }

        /// GET counts of the first repeat, with the largest peak in-flight
        /// count of any repeat. Request counts are deterministic for a fixed
        /// configuration, so every repeat must match the first.
        fn gets(&self) -> GetCounts {
            let first = self.gets[0];
            for gets in &self.gets[1..] {
                assert_eq!(
                    (gets.metadata, gets.data, gets.heads),
                    (first.metadata, first.data, first.heads),
                    "GET counts differ between repeats"
                );
            }
            GetCounts {
                max_in_flight: self.gets.iter().map(|g| g.max_in_flight).max().unwrap(),
                ..first
            }
        }

        fn sorted_ms(&self) -> Vec<f64> {
            let mut ms: Vec<f64> = self.walls.iter().map(|w| w.as_secs_f64() * 1e3).collect();
            ms.sort_by(f64::total_cmp);
            ms
        }

        fn median_ms(&self) -> String {
            let ms = self.sorted_ms();
            format!("{:.0}", ms[ms.len() / 2])
        }

        fn range_ms(&self) -> String {
            let ms = self.sorted_ms();
            format!("{:.0}-{:.0}", ms[0], ms[ms.len() - 1])
        }
    }
}
