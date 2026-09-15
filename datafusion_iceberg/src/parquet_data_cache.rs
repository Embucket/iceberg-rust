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

use std::ops::Range;
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

struct State {
    entries: LruCache<Key, Bytes>,
    used: usize,
}

pub(crate) struct DataCache {
    cap: usize,
    state: Mutex<State>,
}

impl DataCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            state: Mutex::new(State {
                entries: LruCache::unbounded(),
                used: 0,
            }),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.cap != 0
    }

    fn get(&self, key: &Key) -> Option<Bytes> {
        self.state.lock().ok()?.entries.get(key).cloned()
    }

    fn put(&self, key: Key, bytes: &Bytes) {
        let weight = key.weight(bytes.len());
        if weight > self.cap || bytes.is_empty() {
            return;
        }
        // Copy outside the shared lock; never retain a slice of a larger range.
        let owned = Bytes::copy_from_slice(bytes);
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(previous) = state.entries.put(key.clone(), owned) {
            state.used = state.used.saturating_sub(key.weight(previous.len()));
        }
        state.used = state.used.saturating_add(weight);
        while state.used > self.cap {
            let Some((old_key, old_bytes)) = state.entries.pop_lru() else {
                break;
            };
            state.used = state.used.saturating_sub(old_key.weight(old_bytes.len()));
        }
    }

    pub(crate) async fn read(
        &self,
        inner: &mut (dyn AsyncFileReader + Send),
        file: &Arc<str>,
        size: u64,
        ranges: Vec<Range<u64>>,
        metrics: &DataMetrics,
    ) -> Result<Vec<Bytes>> {
        let mut output = Vec::with_capacity(ranges.len());
        let mut missing = Vec::new();
        let mut missing_keys = Vec::new();
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
                output.push(Some(bytes));
            } else {
                metrics.misses.add(1);
                missing.push(range);
                missing_keys.push((index, key));
                output.push(None);
            }
        }
        if !missing.is_empty() {
            let fetched = inner.get_byte_ranges(missing).await?;
            if fetched.len() != missing_keys.len() {
                return Err(ParquetError::General(
                    "Incomplete Parquet multi-range response".into(),
                ));
            }
            for ((index, key), bytes) in missing_keys.into_iter().zip(fetched) {
                if bytes.len() as u64 != key.end - key.start {
                    return Err(ParquetError::General("Truncated Parquet byte range".into()));
                }
                metrics.miss_bytes.add(bytes.len());
                self.put(key, &bytes);
                output[index] = Some(bytes);
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
}

impl DataMetrics {
    pub(crate) fn new(metrics: &ExecutionPlanMetricsSet, partition: usize) -> Self {
        Self {
            hits: MetricBuilder::new(metrics).counter("parquet_data_cache_hits", partition),
            misses: MetricBuilder::new(metrics).counter("parquet_data_cache_misses", partition),
            hit_bytes: MetricBuilder::new(metrics)
                .counter("parquet_data_cache_hit_bytes", partition),
            miss_bytes: MetricBuilder::new(metrics)
                .counter("parquet_data_cache_miss_bytes", partition),
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

    #[tokio::test]
    async fn mixed_hits_and_misses_preserve_requested_order() {
        let cache = DataCache::new(4096);
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0);
        let mut reader = Reader {
            data: Bytes::from_static(b"abcdefgh"),
            reads: vec![],
            truncate: false,
        };
        let file = Arc::from("s3://bucket/file");
        let first = cache
            .read(&mut reader, &file, 8, vec![0..2, 4..6], &metrics)
            .await
            .unwrap();
        assert_eq!(
            first,
            vec![Bytes::from_static(b"ab"), Bytes::from_static(b"ef")]
        );
        reader.reads.clear();
        let mixed = cache
            .read(&mut reader, &file, 8, vec![4..6, 2..4, 0..2], &metrics)
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
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0);
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
        let metrics = DataMetrics::new(&ExecutionPlanMetricsSet::new(), 0);
        let mut reader = Reader {
            data: Bytes::from_static(b"abcd"),
            reads: vec![],
            truncate: false,
        };
        let file = Arc::from("s3://bucket/file");
        assert!(cache
            .read(&mut reader, &file, 4, vec![], &metrics)
            .await
            .unwrap()
            .is_empty());
        for range in [0..5, Range { start: 3, end: 2 }] {
            assert!(cache
                .read(&mut reader, &file, 4, vec![range], &metrics)
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
}
