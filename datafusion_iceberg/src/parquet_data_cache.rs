//! Optional process-wide cache of immutable Iceberg Parquet byte ranges.
//!
//! `ICEBERG_PARQUET_DATA_CACHE_MB` bounds resident cached bytes (including a
//! conservative key/entry charge); zero, the default, bypasses this cache. Keys
//! include the full store-qualified file identity, file size, and exact range.
//! Iceberg rewrites create new paths, so snapshot changes require no invalidation.
//! Metadata and credentials are still resolved by the catalog in the usual way.
//!
//! Each retained range owns its buffer: an object-store multi-range response can
//! return slices backed by a much larger coalesced allocation. Retaining those
//! slices while charging only their lengths would evade the cache's byte cap.
//!
//! # Scan resistance
//!
//! A plain LRU is pure overhead when one scan reads more than the cap: every
//! miss is copied and inserted, each insert evicts an older entry (first the
//! rest of the process's working set, then the scan's own earlier ranges), and
//! a repeat of the scan finds nothing, because LRU evicts a cyclic scan larger
//! than the cache just ahead of its next pass. Two rules bound that:
//!
//! - **Per-scan admission budget.** Every reader of one table scan shares a
//!   [`ScanAdmission`]. It charges every range the scan touches, hits as well
//!   as misses, by weight (bytes plus the per-entry charge, the unit of the
//!   cap), and a miss is admitted only while that total, the miss included,
//!   stays within the cap. A scan that fits is admitted in full, exactly as
//!   before; one that does not stops copying and inserting once it has
//!   touched a cap's worth, so it evicts about a cap's worth of entries at
//!   most. Charging hits is what makes repeats stable: a repeat of a scan
//!   larger than the cap spends its budget on the ranges an earlier run
//!   admitted and keeps hitting them on every repeat, instead of admitting a
//!   fresh cap's worth that would evict them and alternate between full and
//!   no hits. The threshold is the cap itself: any lower value would drop
//!   warm hits for scans that fit, and admitting more than a cap's worth from
//!   one scan can only evict that scan's own ranges. Ranges that can never be
//!   cached (empty, or heavier than the cap) are neither charged nor counted
//!   as skips. Every scan starts with a fresh budget, so admission resumes as
//!   soon as the large scan ends; there is no global latch, clock or tunable.
//!   The budget lives as long as the reader factory, i.e. one physical plan:
//!   executing the same plan again (a recursive query's working table, a
//!   cached plan) continues its budget, so a re-execution that has spent it
//!   still hits resident ranges but admits no new ones.
//! - **Second chance for reused ranges, bounded to a quarter of the cap.** A
//!   hit marks its entry. Eviction moves a marked least-recently-used entry to
//!   the most-recent end and clears the mark instead of dropping it, as long
//!   as entries reprieved this way (and not evicted since) total at most a
//!   quarter of the cap; past that, marked entries are evicted in LRU order.
//!   A reprieved entry that is hit again earns another reprieve. One insert
//!   reprieves at most [`MAX_REPRIEVES_PER_INSERT`] entries before it evicts
//!   in plain LRU order, which bounds the work an insert does under the
//!   shared lock. The cap's worth a large scan may still admit then recycles
//!   that scan's own unreused ranges before it reaches up to a quarter of the
//!   cap of ranges other queries actually reuse. The bound limits what reprieves can cost a working-set change:
//!   reprieved older ranges outrank the new set's unhit ones, so a new set
//!   keeps on its first pass only the cap minus the reprieved share. Up to
//!   three quarters of the cap, a new working set therefore displaces an
//!   older reused one in one pass, exactly like a plain LRU; a larger one
//!   loses at most a quarter of the cap on that pass and re-admits it on the
//!   next.
//!
//! Neither rule affects results: a range that is not admitted is returned from
//! the fetch exactly like an admitted one. The scan-local
//! `parquet_data_cache_admission_skips` counts misses the budget declined, and
//! `parquet_data_cache_evictions`/`parquet_data_cache_eviction_bytes` count
//! entries (and their data bytes) dropped to make room for a scan's inserts.
//! A cache whose working set does not fit shows evictions; one whose scans
//! outgrow it shows skips. Neither is registered while the cache is disabled.
//! Like the hit and miss counters they are registered per reader, i.e. per
//! file and partition, so an enabled cache registers seven data-cache entries
//! per file in the scan's metrics set instead of four.
//!
//! Many concurrent or successive scans that each fit but together exceed the
//! cap still churn the unreused ranges like a plain LRU; the second chance
//! only keeps up to a quarter of the cap of reused ranges resident through
//! that.

use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::errors::{ParquetError, Result};
use datafusion::physical_plan::metrics::{Count, ExecutionPlanMetricsSet, MetricBuilder};
use lru::LruCache;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    file: Arc<str>,
    size: u64,
    start: u64,
    end: u64,
}

impl Key {
    fn weight(&self, bytes: usize) -> usize {
        // Includes the LRU/hash entry, shared-string allocation and allocator
        // overhead conservatively, even though sibling keys share the string.
        bytes.saturating_add(self.file.len()).saturating_add(192)
    }
}

struct Entry {
    bytes: Bytes,
    /// Hit since it was inserted or last spared from eviction.
    hit: bool,
    /// Spared from eviction at least once; its weight is in `State::spared`.
    spared: bool,
}

struct State {
    entries: LruCache<Key, Entry>,
    used: usize,
    /// Weight of resident entries that eviction has spared; at most
    /// `cap / SPARED_SHARE`.
    spared: usize,
}

/// Reprieved entries may hold at most `1 / SPARED_SHARE` of the cap.
const SPARED_SHARE: usize = 4;

/// Entries one insert may reprieve before it evicts in plain LRU order, so a
/// run of marked entries at the LRU end cannot make one insert walk up to a
/// quarter of the cap while every hit waits on the lock.
const MAX_REPRIEVES_PER_INSERT: usize = 16;

/// Entries (and their data bytes) one insert evicted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Evicted {
    entries: usize,
    bytes: usize,
}

/// Data-cache admission budget shared by every reader of one table scan.
///
/// Holds the cumulative weight of the ranges the scan touched, hits and
/// misses alike; see the module docs.
#[derive(Debug, Default)]
pub(crate) struct ScanAdmission {
    touched: AtomicUsize,
}

impl ScanAdmission {
    /// Charge the scan's hits of total `weight` against its budget.
    fn charge(&self, weight: usize, cap: usize) {
        // Past the budget further charges change nothing; skip the shared
        // read-modify-write so the scan's readers stop contending on it.
        if weight != 0 && self.touched.load(Ordering::Relaxed) <= cap {
            self.touched.fetch_add(weight, Ordering::Relaxed);
        }
    }

    /// Charge one missed range of `weight` and say whether it may be inserted:
    /// only while the scan's charges, this one included, total at most `cap`.
    fn admit(&self, weight: usize, cap: usize) -> bool {
        // Once over budget the scan never admits again; skip the shared
        // read-modify-write so its readers stop contending on the counter.
        if self.touched.load(Ordering::Relaxed) > cap {
            return false;
        }
        self.touched
            .fetch_add(weight, Ordering::Relaxed)
            .saturating_add(weight)
            <= cap
    }
}

pub(crate) struct DataCache {
    cap: usize,
    state: Mutex<State>,
}

impl std::fmt::Debug for DataCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataCache")
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl DataCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            state: Mutex::new(State {
                entries: LruCache::unbounded(),
                used: 0,
                spared: 0,
            }),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.cap != 0
    }

    /// Drop every entry, so a benchmark repeat starts from an empty cache.
    #[cfg(test)]
    pub(crate) fn clear_for_tests(&self) {
        let mut state = self.state.lock().unwrap();
        state.entries.clear();
        state.used = 0;
        state.spared = 0;
    }

    /// Resident weight, for tests that check the cap holds.
    #[cfg(test)]
    pub(crate) fn used_for_tests(&self) -> usize {
        self.state.lock().unwrap().used
    }

    fn get(&self, key: &Key) -> Option<Bytes> {
        let mut state = self.state.lock().ok()?;
        let entry = state.entries.get_mut(key)?;
        entry.hit = true;
        Some(entry.bytes.clone())
    }

    fn put(&self, key: Key, bytes: &Bytes) -> Evicted {
        let mut evicted = Evicted::default();
        let weight = key.weight(bytes.len());
        if weight > self.cap || bytes.is_empty() {
            return evicted;
        }
        // Copy outside the shared lock; never retain a slice of a larger range.
        let owned = Bytes::copy_from_slice(bytes);
        let Ok(mut state) = self.state.lock() else {
            return evicted;
        };
        let entry = Entry {
            bytes: owned,
            hit: false,
            spared: false,
        };
        if let Some(previous) = state.entries.put(key.clone(), entry) {
            let previous_weight = key.weight(previous.bytes.len());
            state.used = state.used.saturating_sub(previous_weight);
            if previous.spared {
                state.spared = state.spared.saturating_sub(previous_weight);
            }
        }
        state.used = state.used.saturating_add(weight);
        // Terminates: at most MAX_REPRIEVES_PER_INSERT passes clear a mark,
        // every other pass evicts, and the entry just inserted (weight <= cap)
        // fits once it is alone.
        let spared_cap = self.cap / SPARED_SHARE;
        let mut reprieves = 0;
        while state.used > self.cap {
            let Some((lru, entry)) = state.entries.peek_lru() else {
                break;
            };
            let lru_weight = lru.weight(entry.bytes.len());
            if entry.hit
                && reprieves < MAX_REPRIEVES_PER_INSERT
                && (entry.spared || state.spared + lru_weight <= spared_cap)
            {
                // Second chance: refresh instead of evicting, once per hit.
                reprieves += 1;
                let newly_spared = !entry.spared;
                let lru = lru.clone();
                if let Some(entry) = state.entries.get_mut(&lru) {
                    entry.hit = false;
                    entry.spared = true;
                }
                if newly_spared {
                    state.spared += lru_weight;
                }
                continue;
            }
            let Some((_, old)) = state.entries.pop_lru() else {
                break;
            };
            state.used = state.used.saturating_sub(lru_weight);
            if old.spared {
                state.spared = state.spared.saturating_sub(lru_weight);
            }
            evicted.entries += 1;
            evicted.bytes += old.bytes.len();
        }
        evicted
    }

    pub(crate) async fn read(
        &self,
        inner: &mut (dyn AsyncFileReader + Send),
        file: &Arc<str>,
        size: u64,
        ranges: Vec<Range<u64>>,
        scan: &ScanAdmission,
        metrics: &DataMetrics,
    ) -> Result<Vec<Bytes>> {
        let mut output = Vec::with_capacity(ranges.len());
        let mut missing = Vec::new();
        let mut missing_keys = Vec::new();
        let mut hit_weight = 0usize;
        for (index, range) in ranges.into_iter().enumerate() {
            if range.start > range.end || range.end > size {
                return Err(ParquetError::General(
                    "Parquet range outside file bounds".into(),
                ));
            }
            let key = Key {
                file: Arc::clone(file),
                size,
                start: range.start,
                end: range.end,
            };
            if let Some(bytes) = self.get(&key) {
                metrics.hits.add(1);
                metrics.hit_bytes.add(bytes.len());
                hit_weight = hit_weight.saturating_add(key.weight(bytes.len()));
                output.push(Some(bytes));
            } else {
                metrics.misses.add(1);
                missing.push(range);
                missing_keys.push((index, key));
                output.push(None);
            }
        }
        // Hits are charged before this call's misses: they hold resident
        // ranges the misses would otherwise evict.
        scan.charge(hit_weight, self.cap);
        if !missing.is_empty() {
            let fetched = inner.get_byte_ranges(missing).await?;
            if fetched.len() != missing_keys.len() {
                return Err(ParquetError::General(
                    "Incomplete Parquet multi-range response".into(),
                ));
            }
            let mut evicted = Evicted::default();
            let mut skipped = 0;
            for ((index, key), bytes) in missing_keys.into_iter().zip(fetched) {
                if bytes.len() as u64 != key.end - key.start {
                    return Err(ParquetError::General("Truncated Parquet byte range".into()));
                }
                metrics.miss_bytes.add(bytes.len());
                let weight = key.weight(bytes.len());
                // A range `put` would reject (empty, or heavier than the cap)
                // must not spend the budget the scan's other ranges can use.
                if weight <= self.cap && !bytes.is_empty() {
                    if scan.admit(weight, self.cap) {
                        let put = self.put(key, &bytes);
                        evicted.entries += put.entries;
                        evicted.bytes += put.bytes;
                    } else {
                        skipped += 1;
                    }
                }
                output[index] = Some(bytes);
            }
            if let Some(guard) = &metrics.guard {
                guard.evictions.add(evicted.entries);
                guard.eviction_bytes.add(evicted.bytes);
                guard.admission_skips.add(skipped);
            }
        }
        output
            .into_iter()
            .map(|bytes| {
                bytes.ok_or_else(|| ParquetError::General("Missing Parquet byte range".into()))
            })
            .collect()
    }
}

pub(crate) struct DataMetrics {
    hits: Count,
    misses: Count,
    hit_bytes: Count,
    miss_bytes: Count,
    /// Registered only for an enabled cache, so plans with the cache off keep
    /// exactly the metrics they always had.
    guard: Option<GuardMetrics>,
}

struct GuardMetrics {
    evictions: Count,
    eviction_bytes: Count,
    admission_skips: Count,
}

impl DataMetrics {
    pub(crate) fn new(metrics: &ExecutionPlanMetricsSet, partition: usize, enabled: bool) -> Self {
        Self {
            hits: MetricBuilder::new(metrics).counter("parquet_data_cache_hits", partition),
            misses: MetricBuilder::new(metrics).counter("parquet_data_cache_misses", partition),
            hit_bytes: MetricBuilder::new(metrics)
                .counter("parquet_data_cache_hit_bytes", partition),
            miss_bytes: MetricBuilder::new(metrics)
                .counter("parquet_data_cache_miss_bytes", partition),
            guard: enabled.then(|| GuardMetrics {
                evictions: MetricBuilder::new(metrics)
                    .counter("parquet_data_cache_evictions", partition),
                eviction_bytes: MetricBuilder::new(metrics)
                    .counter("parquet_data_cache_eviction_bytes", partition),
                admission_skips: MetricBuilder::new(metrics)
                    .counter("parquet_data_cache_admission_skips", partition),
            }),
        }
    }
}

pub(crate) static DATA_CACHE: LazyLock<DataCache> = LazyLock::new(|| {
    let cap_mb = std::env::var("ICEBERG_PARQUET_DATA_CACHE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    tracing::info!(cap_mb, "Iceberg Parquet data cache initialized");
    DataCache::new(cap_mb.saturating_mul(1024 * 1024))
});

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
    use datafusion::parquet::file::metadata::ParquetMetaData;
    use futures::{future::BoxFuture, FutureExt};

    struct Reader {
        data: Bytes,
        reads: Vec<Range<u64>>,
        truncate: bool,
    }

    impl AsyncFileReader for Reader {
        fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, Result<Bytes>> {
            self.reads.push(range.clone());
            let end = range.end as usize - usize::from(self.truncate);
            futures::future::ready(Ok(self.data.slice(range.start as usize..end))).boxed()
        }

        fn get_metadata<'a>(
            &'a mut self,
            _: Option<&'a ArrowReaderOptions>,
        ) -> BoxFuture<'a, Result<Arc<ParquetMetaData>>> {
            futures::future::ready(Err(ParquetError::General(
                "unused in data cache tests".into(),
            )))
            .boxed()
        }
    }

    fn scan() -> ScanAdmission {
        ScanAdmission::default()
    }

    #[tokio::test]
    async fn mixed_hits_and_misses_preserve_requested_order() {
        let cache = DataCache::new(4096);
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let mut reader = Reader {
            data: Bytes::from_static(b"abcdefgh"),
            reads: vec![],
            truncate: false,
        };
        let file = Arc::from("s3://bucket/file");
        let first = cache
            .read(&mut reader, &file, 8, vec![0..2, 4..6], &scan(), &metrics)
            .await
            .unwrap();
        assert_eq!(
            first,
            vec![Bytes::from_static(b"ab"), Bytes::from_static(b"ef")]
        );
        reader.reads.clear();
        let mixed = cache
            .read(
                &mut reader,
                &file,
                8,
                vec![4..6, 2..4, 0..2],
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(
            mixed,
            vec![
                Bytes::from_static(b"ef"),
                Bytes::from_static(b"cd"),
                Bytes::from_static(b"ab")
            ]
        );
        assert_eq!(reader.reads, vec![2..4]);
        assert_eq!(metrics.hit_bytes.value(), 4);
    }

    #[tokio::test]
    async fn failed_read_is_not_cached_and_retry_can_succeed() {
        let cache = DataCache::new(4096);
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let mut reader = Reader {
            data: Bytes::from_static(b"abcd"),
            reads: vec![],
            truncate: true,
        };
        let file = Arc::from("s3://bucket/file");
        assert!(cache
            .read(
                &mut reader,
                &file,
                4,
                std::iter::once(0..4).collect(),
                &scan(),
                &metrics
            )
            .await
            .is_err());
        reader.truncate = false;
        let retry = cache
            .read(
                &mut reader,
                &file,
                4,
                std::iter::once(0..4).collect(),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(retry, vec![Bytes::from_static(b"abcd")]);
        assert_eq!(reader.reads.len(), 2);
    }

    #[tokio::test]
    async fn empty_requests_and_invalid_bounds_do_not_read_storage() {
        let cache = DataCache::new(4096);
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let mut reader = Reader {
            data: Bytes::from_static(b"abcd"),
            reads: vec![],
            truncate: false,
        };
        let file = Arc::from("s3://bucket/file");
        assert!(cache
            .read(&mut reader, &file, 4, vec![], &scan(), &metrics)
            .await
            .unwrap()
            .is_empty());
        for range in [0..5, Range { start: 3, end: 2 }] {
            assert!(cache
                .read(&mut reader, &file, 4, vec![range], &scan(), &metrics)
                .await
                .is_err());
        }
        assert!(reader.reads.is_empty());
    }

    fn key(file: &str, size: u64, start: u64, end: u64) -> Key {
        Key {
            file: Arc::from(file),
            size,
            start,
            end,
        }
    }

    #[test]
    fn keys_isolate_stores_sizes_and_ranges() {
        let cache = DataCache::new(4096);
        let a = key("s3://a/file", 20, 0, 4);
        cache.put(a.clone(), &Bytes::from_static(b"data"));
        assert_eq!(cache.get(&a).unwrap(), "data");
        for different in [
            key("s3://b/file", 20, 0, 4),
            key("s3://a/file", 21, 0, 4),
            key("s3://a/file", 20, 1, 5),
        ] {
            assert!(cache.get(&different).is_none());
        }
    }

    #[test]
    fn lru_replacement_and_eviction_stay_bounded() {
        let a = key("a", 20, 0, 4);
        let b = key("b", 20, 0, 4);
        let cache = DataCache::new(a.weight(4));
        cache.put(a.clone(), &Bytes::from_static(b"abcd"));
        cache.put(a.clone(), &Bytes::from_static(b"efgh"));
        assert_eq!(cache.state.lock().unwrap().used, a.weight(4));
        cache.put(b.clone(), &Bytes::from_static(b"ijkl"));
        assert!(cache.get(&a).is_none());
        assert_eq!(cache.get(&b).unwrap(), "ijkl");
        assert!(cache.state.lock().unwrap().used <= cache.cap);
    }

    #[test]
    fn retained_slice_owns_only_its_range() {
        let cache = DataCache::new(4096);
        let large = Bytes::from(vec![7; 1024 * 1024]);
        let slice = large.slice(100..104);
        let a = key("a", 1024 * 1024, 100, 104);
        cache.put(a.clone(), &slice);
        let retained = cache.get(&a).unwrap();
        assert_eq!(retained, slice);
        assert_ne!(retained.as_ptr(), slice.as_ptr());
    }

    #[test]
    fn disabled_and_oversized_entries_are_not_retained() {
        for cap in [0, 1] {
            let cache = DataCache::new(cap);
            let a = key("a", 4, 0, 4);
            cache.put(a.clone(), &Bytes::from_static(b"data"));
            assert!(cache.get(&a).is_none());
            assert_eq!(cache.state.lock().unwrap().used, 0);
        }
    }

    /// Bytes of one range in the guard tests.
    const RANGE: u64 = 64;

    /// `n` adjacent [`RANGE`]-byte ranges from the start of a file.
    fn ranges(n: u64) -> Vec<Range<u64>> {
        (0..n).map(|i| i * RANGE..(i + 1) * RANGE).collect()
    }

    /// A reader over a file of `n` ranges whose every byte is `fill`.
    fn file_of(n: u64, fill: u8) -> Reader {
        Reader {
            data: Bytes::from(vec![fill; usize::try_from(n * RANGE).unwrap()]),
            reads: vec![],
            truncate: false,
        }
    }

    /// The cache weight of one [`RANGE`]-byte range of `file`.
    fn range_weight(file: &Arc<str>) -> usize {
        key(file, 0, 0, RANGE).weight(usize::try_from(RANGE).unwrap())
    }

    fn guard(metrics: &DataMetrics) -> &GuardMetrics {
        metrics.guard.as_ref().unwrap()
    }

    #[tokio::test]
    async fn scan_larger_than_cap_stops_admitting() {
        // Equal-length names, so hot and large ranges weigh the same.
        let hot_file: Arc<str> = Arc::from("s3://bucket/hot");
        let big_file: Arc<str> = Arc::from("s3://bucket/big");
        let weight = range_weight(&hot_file);
        assert_eq!(weight, range_weight(&big_file));
        let cache = DataCache::new(16 * weight);
        let mut hot = file_of(2, b'h');

        // A small query caches two ranges and reuses them.
        for _ in 0..2 {
            let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
            cache
                .read(&mut hot, &hot_file, 2 * RANGE, ranges(2), &scan(), &metrics)
                .await
                .unwrap();
        }
        assert_eq!(hot.reads, ranges(2), "the repeat is served from the cache");

        // One scan four times the cap: results are complete either way.
        let large = scan();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let mut big = file_of(64, b'b');
        let out = cache
            .read(
                &mut big,
                &big_file,
                64 * RANGE,
                ranges(64),
                &large,
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 64);
        assert!(out
            .iter()
            .all(|bytes| bytes.len() == 64 && bytes[0] == b'b'));
        // The first cap's worth of misses (16 ranges) is admitted; the rest
        // is returned without being copied or inserted.
        assert_eq!(guard(&metrics).admission_skips.value(), 48);
        // Room for the last two admitted ranges came from the scan's own
        // first ranges, not from the hot ones.
        assert_eq!(guard(&metrics).evictions.value(), 2);
        assert_eq!(guard(&metrics).eviction_bytes.value(), 2 * 64);
        assert!(cache.state.lock().unwrap().used <= cache.cap);

        // The previously cached hot ranges still hit.
        hot.reads.clear();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let again = cache
            .read(&mut hot, &hot_file, 2 * RANGE, ranges(2), &scan(), &metrics)
            .await
            .unwrap();
        assert_eq!(again, vec![Bytes::from(vec![b'h'; 64]); 2]);
        assert!(
            hot.reads.is_empty(),
            "hot ranges were evicted: {:?}",
            hot.reads
        );
        assert_eq!(metrics.hits.value(), 2);

        // The same scan keeps declining: its budget is spent.
        big.reads.clear();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(&mut big, &big_file, 64 * RANGE, ranges(1), &large, &metrics)
            .await
            .unwrap();
        assert_eq!(big.reads, vec![0..64], "the evicted first range misses");
        assert_eq!(guard(&metrics).admission_skips.value(), 1);
        assert_eq!(guard(&metrics).evictions.value(), 0);
    }

    #[tokio::test]
    async fn admission_resumes_with_the_next_scan() {
        let file: Arc<str> = Arc::from("s3://bucket/file");
        let cache = DataCache::new(4 * range_weight(&file));
        let mut reader = file_of(16, b'x');
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        // An over-budget scan admits its first four ranges only.
        cache
            .read(
                &mut reader,
                &file,
                16 * RANGE,
                ranges(16),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(guard(&metrics).admission_skips.value(), 12);
        // The scan that follows starts with a fresh budget and is admitted.
        let tail: Vec<_> = (12..16).map(|i| i * RANGE..(i + 1) * RANGE).collect();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(
                &mut reader,
                &file,
                16 * RANGE,
                tail.clone(),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(guard(&metrics).admission_skips.value(), 0);
        assert_eq!(guard(&metrics).evictions.value(), 4);
        reader.reads.clear();
        cache
            .read(&mut reader, &file, 16 * RANGE, tail, &scan(), &metrics)
            .await
            .unwrap();
        assert!(reader.reads.is_empty(), "the new scan's ranges were cached");
    }

    #[tokio::test]
    async fn a_scan_that_fits_is_admitted_in_full() {
        let file: Arc<str> = Arc::from("s3://bucket/file");
        let cache = DataCache::new(16 * range_weight(&file));
        let mut reader = file_of(16, b'x');
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(
                &mut reader,
                &file,
                16 * RANGE,
                ranges(16),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert_eq!(guard(&metrics).admission_skips.value(), 0);
        assert_eq!(guard(&metrics).evictions.value(), 0);
        reader.reads.clear();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(
                &mut reader,
                &file,
                16 * RANGE,
                ranges(16),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert!(reader.reads.is_empty());
        assert_eq!(metrics.hits.value(), 16);
    }

    #[tokio::test]
    async fn evictions_are_counted() {
        let file: Arc<str> = Arc::from("s3://bucket/file");
        let cache = DataCache::new(2 * range_weight(&file));
        let mut reader = file_of(4, b'x');
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        // Separate scans, so the budget never declines; the third insert
        // evicts the least recently used range.
        for range in ranges(3) {
            cache
                .read(
                    &mut reader,
                    &file,
                    4 * RANGE,
                    vec![range],
                    &scan(),
                    &metrics,
                )
                .await
                .unwrap();
        }
        assert_eq!(guard(&metrics).evictions.value(), 1);
        assert_eq!(guard(&metrics).eviction_bytes.value(), 64);
        assert_eq!(guard(&metrics).admission_skips.value(), 0);
        assert!(cache.get(&key(&file, 4 * RANGE, 0, RANGE)).is_none());
        assert_eq!(cache.state.lock().unwrap().used, cache.cap);
    }

    #[tokio::test]
    async fn a_new_working_set_displaces_an_old_reused_one_in_one_pass() {
        let old_file: Arc<str> = Arc::from("s3://bucket/old");
        let new_file: Arc<str> = Arc::from("s3://bucket/new");
        let cache = DataCache::new(16 * range_weight(&old_file));
        // A reused working set of ten ranges, then a new one of ten: together
        // more than the cap, each within three quarters of it.
        let mut old = file_of(10, b'o');
        for _ in 0..2 {
            let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
            cache
                .read(
                    &mut old,
                    &old_file,
                    10 * RANGE,
                    ranges(10),
                    &scan(),
                    &metrics,
                )
                .await
                .unwrap();
        }
        let mut new = file_of(10, b'n');
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(
                &mut new,
                &new_file,
                10 * RANGE,
                ranges(10),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        // Six ranges fit the free room; the other four evict old ones, after
        // the first four old ones (a quarter of the cap) were reprieved.
        assert_eq!(guard(&metrics).evictions.value(), 4);
        assert_eq!(
            cache.state.lock().unwrap().spared,
            4 * range_weight(&old_file)
        );
        new.reads.clear();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(
                &mut new,
                &new_file,
                10 * RANGE,
                ranges(10),
                &scan(),
                &metrics,
            )
            .await
            .unwrap();
        assert!(new.reads.is_empty(), "the new set lost {:?}", new.reads);
        assert_eq!(metrics.hits.value(), 10);
        assert!(cache.state.lock().unwrap().used <= cache.cap);
    }

    #[test]
    fn a_hit_range_is_spared_once_per_hit() {
        let k = |name: &str| key(name, 20, 0, 4);
        let put = |cache: &DataCache, name: &str| {
            cache.put(k(name), &Bytes::from(name.repeat(4).into_bytes()))
        };
        // Four entries; the spared share holds exactly one.
        let cache = DataCache::new(4 * k("a").weight(4));
        assert_eq!(cache.cap / SPARED_SHARE, k("a").weight(4));
        for name in ["a", "b", "c", "d"] {
            assert_eq!(put(&cache, name), Evicted::default());
        }
        // Hit `a`, then refresh the others: `a` is least recently used but
        // marked, so `b` goes instead.
        assert!(cache.get(&k("a")).is_some());
        for name in ["b", "c", "d"] {
            put(&cache, name);
        }
        assert_eq!(
            put(&cache, "e"),
            Evicted {
                entries: 1,
                bytes: 4
            }
        );
        assert!(cache.get(&k("b")).is_none());
        // Hit again, an already reprieved entry is reprieved again; a second
        // marked entry would exceed the spared share and is evicted in turn.
        assert!(cache.get(&k("a")).is_some());
        assert!(cache.get(&k("c")).is_some());
        let order = ["a", "c", "d", "e"];
        for name in order {
            cache.state.lock().unwrap().entries.promote(&k(name));
        }
        put(&cache, "f");
        let resident = |name: &str| cache.state.lock().unwrap().entries.contains(&k(name));
        assert!(resident("a"), "the reprieved entry is reprieved again");
        assert!(!resident("c"), "the spared share is full");
        assert_eq!(cache.state.lock().unwrap().spared, k("a").weight(4));
        // Unhit since its reprieve, `a` is evicted when it is next in line.
        for name in ["d", "e", "f"] {
            cache.state.lock().unwrap().entries.promote(&k(name));
        }
        put(&cache, "g");
        assert!(!resident("a"));
        assert_eq!(cache.state.lock().unwrap().spared, 0);
        assert!(cache.state.lock().unwrap().used <= cache.cap);
    }

    #[tokio::test]
    async fn a_repeated_large_scan_keeps_its_hits() {
        let file: Arc<str> = Arc::from("s3://bucket/file");
        // Sixteen ranges fit; scans of four times and 1.25 times the cap.
        for n in [64, 20] {
            let cache = DataCache::new(16 * range_weight(&file));
            let mut reader = file_of(n, b'x');
            let mut hits = Vec::new();
            for _ in 0..5 {
                let scan = scan();
                let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
                // Row-group-sized calls, as a Parquet reader issues them.
                for chunk in ranges(n).chunks(4) {
                    let out = cache
                        .read(
                            &mut reader,
                            &file,
                            n * RANGE,
                            chunk.to_vec(),
                            &scan,
                            &metrics,
                        )
                        .await
                        .unwrap();
                    assert!(out.iter().all(|bytes| bytes.len() == 64));
                }
                hits.push(metrics.hits.value());
                assert!(cache.used_for_tests() <= cache.cap);
            }
            // Every repeat spends its budget on the cap's worth the first run
            // admitted instead of admitting (and evicting) another one.
            assert_eq!(hits, [0, 16, 16, 16, 16], "hits per repeat, {n} ranges");
        }
    }

    #[tokio::test]
    async fn uncacheable_ranges_do_not_spend_the_budget() {
        let file: Arc<str> = Arc::from("s3://bucket/file");
        let cache = DataCache::new(4 * range_weight(&file));
        let mut reader = file_of(24, b'x');
        let size = 24 * RANGE;
        // Seventeen ranges' bytes alone outweigh the cap of four.
        let oversized = 0..17 * RANGE;
        assert!(key(&file, size, 0, 17 * RANGE).weight(17 * 64) > cache.cap);
        let scan = scan();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        let small: Vec<_> = (17..21).map(|i| i * RANGE..(i + 1) * RANGE).collect();
        let empty = 17 * RANGE..17 * RANGE;
        for request in [vec![oversized], vec![empty], small.clone()] {
            cache
                .read(&mut reader, &file, size, request, &scan, &metrics)
                .await
                .unwrap();
        }
        assert_eq!(guard(&metrics).admission_skips.value(), 0);
        reader.reads.clear();
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true);
        cache
            .read(&mut reader, &file, size, small, &scan, &metrics)
            .await
            .unwrap();
        assert!(reader.reads.is_empty(), "small ranges were not admitted");
        assert_eq!(metrics.hits.value(), 4);
        assert!(cache.used_for_tests() <= cache.cap);
    }

    #[test]
    fn reprieves_per_insert_are_bounded() {
        let k = |i: u64| key("f", 1 << 20, i * 4, i * 4 + 4);
        let weight = k(0).weight(4);
        let cache = DataCache::new(100 * weight);
        // The spared share (25 entries) exceeds the per-insert bound.
        assert!(cache.cap / SPARED_SHARE > MAX_REPRIEVES_PER_INSERT * weight);
        let data = Bytes::from_static(b"data");
        for i in 0..100 {
            cache.put(k(i), &data);
        }
        for i in 0..100 {
            assert!(cache.get(&k(i)).is_some());
        }
        // One insert meets a run of 100 marked entries at the LRU end.
        assert_eq!(
            cache.put(k(100), &data),
            Evicted {
                entries: 1,
                bytes: 4
            }
        );
        let bound = MAX_REPRIEVES_PER_INSERT as u64;
        let resident = |i: u64| cache.state.lock().unwrap().entries.contains(&k(i));
        assert!((0..bound).all(resident), "the first entries are reprieved");
        assert!(
            !resident(bound),
            "past the bound the insert evicts in LRU order"
        );
        assert_eq!(
            cache.state.lock().unwrap().spared,
            MAX_REPRIEVES_PER_INSERT * weight
        );
        assert!(cache.used_for_tests() <= cache.cap);
    }
}

/// Hit- and insert-path cost of the shared cache under thread contention,
/// measured directly on [`DataCache::read`] with no object store or decode.
#[cfg(test)]
mod lock_bench {
    use super::*;
    use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
    use datafusion::parquet::file::metadata::ParquetMetaData;
    use futures::{future::BoxFuture, FutureExt};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Bytes of one range; ranges are requested eight per call, like the
    /// column chunks of one row group.
    const RANGE: u64 = 4096;
    const PER_CALL: usize = 8;
    /// Ranges of the scanned file (32 MiB).
    const RANGES: u64 = 8192;
    /// Ranges of the hot file the hit threads reuse (1 MiB).
    const HOT_RANGES: u64 = 256;
    const REPEATS: usize = 7;

    struct Mem(Bytes);

    impl AsyncFileReader for Mem {
        fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, Result<Bytes>> {
            futures::future::ready(Ok(self.0.slice(range.start as usize..range.end as usize)))
                .boxed()
        }

        fn get_metadata<'a>(
            &'a mut self,
            _: Option<&'a ArrowReaderOptions>,
        ) -> BoxFuture<'a, Result<Arc<ParquetMetaData>>> {
            futures::future::ready(Err(ParquetError::General("unused".into()))).boxed()
        }
    }

    fn metrics() -> DataMetrics {
        DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0, true)
    }

    /// One `read` of `ranges` of `file` charged to `scan`.
    fn read(
        cache: &DataCache,
        reader: &mut Mem,
        file: &Arc<str>,
        ranges: Vec<Range<u64>>,
        scan: &ScanAdmission,
        metrics: &DataMetrics,
    ) {
        let size = reader.0.len() as u64;
        futures::executor::block_on(cache.read(reader, file, size, ranges, scan, metrics)).unwrap();
    }

    /// The calls one of `threads` threads issues for its share of `n` ranges.
    fn calls(n: u64, threads: usize, thread: usize) -> Vec<Vec<Range<u64>>> {
        let share = n / threads as u64;
        let first = share * thread as u64;
        (first..first + share)
            .map(|i| i * RANGE..(i + 1) * RANGE)
            .collect::<Vec<_>>()
            .chunks(PER_CALL)
            .map(<[_]>::to_vec)
            .collect()
    }

    fn file(n: u64) -> Mem {
        Mem(Bytes::from(vec![7; usize::try_from(n * RANGE).unwrap()]))
    }

    /// Median and min-max of `runs` in milliseconds.
    fn summary(runs: &mut [Duration]) -> (f64, f64, f64) {
        runs.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        (
            ms(runs[runs.len() / 2]),
            ms(runs[0]),
            ms(runs[runs.len() - 1]),
        )
    }

    /// `threads` readers pass `passes` times over a resident 32 MiB file;
    /// every pass is one scan shared by all of them, as one table scan's
    /// partitions share its budget.
    fn hits(threads: usize, passes: usize) -> Duration {
        let cache = DataCache::new(256 << 20);
        let name: Arc<str> = Arc::from("s3://bench/hits");
        let warm = ScanAdmission::default();
        let mut reader = file(RANGES);
        for call in calls(RANGES, 1, 0) {
            read(&cache, &mut reader, &name, call, &warm, &metrics());
        }
        let scans: Vec<ScanAdmission> = (0..passes).map(|_| ScanAdmission::default()).collect();
        let started = Instant::now();
        std::thread::scope(|s| {
            for thread in 0..threads {
                let (cache, name, scans) = (&cache, &name, &scans);
                s.spawn(move || {
                    let mut reader = file(RANGES);
                    let metrics = metrics();
                    let calls = calls(RANGES, threads, thread);
                    for scan in scans {
                        for call in &calls {
                            read(cache, &mut reader, name, call.clone(), scan, &metrics);
                        }
                    }
                    assert_eq!(metrics.misses.value(), 0);
                });
            }
        });
        started.elapsed()
    }

    /// `threads` readers pass `passes` times over a 32 MiB file through a
    /// 4 MiB cache, so every read misses. `fresh` gives every call its own
    /// scan, so every miss is copied, inserted and evicts (the insert path's
    /// full cost); otherwise each pass is one shared scan, which the budget
    /// stops admitting after a cap's worth.
    fn churn(threads: usize, passes: usize, fresh: bool) -> Duration {
        let cache = DataCache::new(4 << 20);
        let name: Arc<str> = Arc::from("s3://bench/churn");
        let scans: Vec<ScanAdmission> = (0..passes).map(|_| ScanAdmission::default()).collect();
        let started = Instant::now();
        std::thread::scope(|s| {
            for thread in 0..threads {
                let (cache, name, scans) = (&cache, &name, &scans);
                s.spawn(move || {
                    let mut reader = file(RANGES);
                    let metrics = metrics();
                    let calls = calls(RANGES, threads, thread);
                    for scan in scans {
                        for call in &calls {
                            if fresh {
                                let own = ScanAdmission::default();
                                read(cache, &mut reader, name, call.clone(), &own, &metrics);
                            } else {
                                read(cache, &mut reader, name, call.clone(), scan, &metrics);
                            }
                        }
                    }
                });
            }
        });
        started.elapsed()
    }

    /// `threads` hit threads reread a 1 MiB hot file while `threads` churn
    /// threads insert misses (a fresh scan per call) through a 4 MiB cache.
    /// Returns the hit threads' slowest elapsed time and their hit ratio.
    fn hits_under_churn(threads: usize, passes: usize) -> (Duration, f64) {
        let cache = DataCache::new(4 << 20);
        let hot: Arc<str> = Arc::from("s3://bench/hot");
        let cold: Arc<str> = Arc::from("s3://bench/cold");
        let warm = ScanAdmission::default();
        let mut reader = file(HOT_RANGES);
        for call in calls(HOT_RANGES, 1, 0) {
            read(&cache, &mut reader, &hot, call, &warm, &metrics());
        }
        let done = AtomicBool::new(false);
        let hit_metrics: Vec<DataMetrics> = (0..threads).map(|_| metrics()).collect();
        let elapsed = std::thread::scope(|s| {
            for thread in 0..threads {
                let (cache, cold, done) = (&cache, &cold, &done);
                s.spawn(move || {
                    let mut reader = file(RANGES);
                    let metrics = metrics();
                    let calls = calls(RANGES, threads, thread);
                    while !done.load(Ordering::Relaxed) {
                        for call in &calls {
                            let own = ScanAdmission::default();
                            read(cache, &mut reader, cold, call.clone(), &own, &metrics);
                            if done.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                    }
                });
            }
            let hitters: Vec<_> = hit_metrics
                .iter()
                .map(|metrics| {
                    let (cache, hot) = (&cache, &hot);
                    s.spawn(move || {
                        let mut reader = file(HOT_RANGES);
                        let calls = calls(HOT_RANGES, 1, 0);
                        let started = Instant::now();
                        for _ in 0..passes {
                            let scan = ScanAdmission::default();
                            for call in &calls {
                                read(cache, &mut reader, hot, call.clone(), &scan, metrics);
                            }
                        }
                        started.elapsed()
                    })
                })
                .collect();
            let slowest = hitters
                .into_iter()
                .map(|h| h.join().unwrap())
                .max()
                .unwrap();
            done.store(true, Ordering::Relaxed);
            slowest
        });
        let hits: usize = hit_metrics.iter().map(|m| m.hits.value()).sum();
        let misses: usize = hit_metrics.iter().map(|m| m.misses.value()).sum();
        (elapsed, hits as f64 / (hits + misses) as f64)
    }

    /// Print one table row (after the header, for the first thread count):
    /// the median and min-max wall time of `runs`, and nanoseconds per range
    /// read by one thread (`ranges` per thread).
    fn row(name: &str, threads: usize, ranges: u64, runs: &mut [Duration], ratio: &str) {
        let (median, min, max) = summary(runs);
        if threads == 1 {
            println!("| workload | threads | ms | ms range | ns/range/thread | hot hit ratio |");
            println!("|---|---|---|---|---|---|");
        }
        println!(
            "| {name} | {threads} | {median:.1} | {min:.1}-{max:.1} | {:.0} | {ratio} |",
            median * 1e6 / ranges as f64
        );
    }

    // Each workload is its own ignored test, so one can be measured alone
    // (for example under `perf stat`). Run them on a release build, one at a
    // time and pinned with `taskset`: `cargo test --release -p
    // datafusion_iceberg --lib parquet_data_cache::lock_bench -- --ignored
    // --nocapture --test-threads=1`.

    #[test]
    #[ignore = "data-cache lock benchmark; run with `--ignored --test-threads=1 --nocapture`"]
    fn lock_bench_hits() {
        for threads in [1, 4] {
            let passes = 40;
            let per_thread = RANGES / threads as u64 * passes as u64;
            let mut runs: Vec<_> = (0..REPEATS).map(|_| hits(threads, passes)).collect();
            row("hits", threads, per_thread, &mut runs, "-");
        }
    }

    #[test]
    #[ignore = "data-cache lock benchmark; run with `--ignored --test-threads=1 --nocapture`"]
    fn lock_bench_churn_scan_per_call() {
        for threads in [1, 4] {
            let passes = 4;
            let per_thread = RANGES / threads as u64 * passes as u64;
            let mut runs: Vec<_> = (0..REPEATS).map(|_| churn(threads, passes, true)).collect();
            row("churn, scan per call", threads, per_thread, &mut runs, "-");
        }
    }

    #[test]
    #[ignore = "data-cache lock benchmark; run with `--ignored --test-threads=1 --nocapture`"]
    fn lock_bench_churn_scan_per_pass() {
        for threads in [1, 4] {
            let passes = 4;
            let per_thread = RANGES / threads as u64 * passes as u64;
            let mut runs: Vec<_> = (0..REPEATS)
                .map(|_| churn(threads, passes, false))
                .collect();
            row("churn, scan per pass", threads, per_thread, &mut runs, "-");
        }
    }

    #[test]
    #[ignore = "data-cache lock benchmark; run with `--ignored --test-threads=1 --nocapture`"]
    fn lock_bench_hits_under_churn() {
        for threads in [1, 4] {
            let passes = 400;
            let mut ratios = Vec::new();
            let mut runs: Vec<_> = (0..REPEATS)
                .map(|_| {
                    let (elapsed, ratio) = hits_under_churn(threads, passes);
                    ratios.push(ratio);
                    elapsed
                })
                .collect();
            ratios.sort_by(f64::total_cmp);
            let ratio = format!("{:.3}", ratios[ratios.len() / 2]);
            let ranges = HOT_RANGES * passes as u64;
            row("hits under churn", threads, ranges, &mut runs, &ratio);
        }
    }
}
