# Datafusion iceberg

Provides the functionality to use apache iceberg with datafusion including the `TableProvider`, `SchemaProvider` and `CatalogProvider` traits.

## Parquet footer cache and pipeline progress

The process-wide parsed-footer cache is byte-capped by
`ICEBERG_PARQUET_METADATA_CACHE_MB` (default 64 MiB, zero disables it) and preserves
store-qualified immutable-file identity plus a file-size sanity stamp. Warm
reads reuse parsed metadata without object-store I/O.

Cold readers do not wait for a per-file gate owned by another reader. A lazy
query pipeline can stop polling a prefetched probe while a build input needs
the same footer; blocking single-flight would then deadlock. A contended cold
reader instead performs its own bounded read and records
`parquet_metadata_cache_contention_bypasses`. Duplicate cold fetches are an
intentional liveness trade, not a query-result cache or a background fill task.
The retained cache remains byte-capped; concurrent reads and query-owned
metadata require their own headroom. A deterministic paused-loader unit test
covers this scheduling condition independently of benchmark table names.

## Immutable Parquet data cache

`ICEBERG_PARQUET_DATA_CACHE_MB` enables a process-wide, byte-bounded LRU for exact
Parquet read ranges. Its default is `0` (disabled). Cache identity includes the
store-qualified file URI, file size, and range. Iceberg rewrites and time travel
use immutable data-file paths; query results, table snapshots and credentials
are not cached here. Metadata resolution and delete-file handling still run.

Warm hits avoid object-store reads while retaining normal Parquet decoding and
DataFusion execution. Misses keep the reader's batched multi-range API. Retained
bytes own compact buffers, so a small slice cannot pin a large coalesced response.
Entries plus conservative key overhead count against the cap. The process's
other memory users and in-flight reads still need their own budget.

The cache resists scans larger than itself. All readers of one table scan
share an admission budget: a miss is inserted only while that scan's misses,
weighted like the cap, total at most the cap. A scan that fits is cached in
full; a larger one stops copying and inserting after a cap's worth, so it can
evict at most that much and its repeat reuses the ranges it admitted first.
Each scan starts with a fresh budget. Eviction also gives a range that has been
hit one second chance, for at most a quarter of the cap of such ranges, so a
large scan's admitted share recycles its own unused ranges before the reused
ranges in that share. A new working set up to three quarters of the cap still
displaces an older reused one in one pass, as with a plain LRU; a larger one
can lose up to a quarter of the cap on its first pass. Neither rule changes
query results.

Scan metrics expose `parquet_data_cache_hits`, `parquet_data_cache_misses`,
`parquet_data_cache_hit_bytes`, and `parquet_data_cache_miss_bytes` through
`EXPLAIN ANALYZE VERBOSE`. With the cache enabled they also include
`parquet_data_cache_evictions` and `parquet_data_cache_eviction_bytes` (entries
and data bytes dropped to make room for the scan's inserts) and
`parquet_data_cache_admission_skips` (misses the scan's budget declined).
Evictions mean the working set does not fit; skips mean single scans outgrow
the cap. Scans that each fit but together exceed the cap still churn unreused
ranges as in a plain LRU. This initial implementation does not reuse overlapping
ranges or deduplicate concurrent misses. It is intended for immutable Iceberg
files, not files overwritten in place at the same URI and size.
