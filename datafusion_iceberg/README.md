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
A miss the store returned in a buffer of its own is kept in that buffer, shared
with the decoder, instead of being copied; of the slices of one coalesced
response, the first keeps the response buffer (cut down to its own bytes) and
the others are copied. A first pass that coalesces adjacent column chunks into
one request therefore still pays a copy for most of its bytes.
Entries plus conservative key overhead count against the cap. The process's
other memory users and in-flight reads still need their own budget.

The cache resists scans larger than itself. All readers of one table scan
share an admission budget that charges every range the scan touches, hits as
well as misses, weighted like the cap; a miss is inserted only while that
total stays within the cap. A scan that fits is cached in full; a larger one
stops copying and inserting after a cap's worth, so it evicts about that much
at most, and every repeat spends its budget on the ranges an earlier run
admitted and keeps hitting them instead of displacing them. Ranges that can
never be cached (empty, or larger than the cap) do not spend the budget. Each
scan starts with a fresh budget. The budget lasts as long as the physical
plan: executing one plan again (a recursive query's working table, a cached
plan) continues it, so a re-execution that has spent it still hits but
admits nothing new. Eviction also spares a range that has been hit since it was
inserted or last spared (so a spared range that is hit again is spared
again), for at most a quarter of the cap of such ranges and at most 16
reprieves per insert, so a large scan's
admitted share recycles its own unused ranges before the reused ranges in
that share. A new working set up to three quarters of the cap still
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
the cap. Like the hit and miss counters they are registered per file and
partition, so an enabled cache registers seven data-cache metric entries per
file instead of four. Scans that each fit but together exceed the cap still churn unreused
ranges as in a plain LRU. This initial implementation does not reuse overlapping
ranges or deduplicate concurrent misses. It is intended for immutable Iceberg
files, not files overwritten in place at the same URI and size.
