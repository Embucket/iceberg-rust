# datafusion_iceberg

This module owns DataFusion providers and physical scans over Iceberg snapshots.
Keep schema, delete-file and snapshot semantics intact when optimizing reads.

`parquet_metadata_cache.rs` wraps every Iceberg Parquet reader with the shared
footer cache. Build every scan exec (data, delete and sort-order-attested
files) with `DataSourceExec::from_data_source`; `ParquetFormat::create_physical_plan`
swaps in DataFusion's reader factory and resets the source's Parquet options,
which drops both caches, their metrics and the row-filter pushdown decision,
and applies DataFusion's 512 KiB default metadata size hint. Every scan takes
its footer size hint per file from `ICEBERG_PARQUET_METADATA_SIZE_HINT_KB`
instead (off by default).
`parquet_data_cache.rs` optionally caches immutable byte ranges;
the default capacity is zero. Both use store-qualified file identity and bounded
memory. Keep data-cache metrics attached to each scan. Preserve batched I/O on
misses, do not hold synchronous locks across awaits, and account for retained
buffer ownership rather than only a shared slice's visible length. Keep an
admitted miss in the buffer it was fetched in when that buffer is exactly the
range and nothing else shares it, and copy every other admitted miss; an
unconditional copy sits on every first pass's path to its first batch. Do not
rely on shrinking a reused buffer for accounting, since an allocator may keep
the whole block. The data
cache's per-scan admission budget lives on the reader factory, which is built
once per table scan; keep it shared by every reader of that scan and absent
when the cache is disabled, so cap `0` adds no work to the read path. The
budget charges hits as well as misses (so repeats of a scan larger than the
cap keep their hits) and lasts for one physical plan; re-executing a plan
continues its budget. Hits keep the mutex scope of a plain LRU lookup, and
one insert reprieves a bounded number of entries.

Validate cache changes with focused unit tests for identities, memory limits,
partial hits, error recovery, and representative Iceberg query result checks.
Consumers must include configured cache capacity in their memory budgeting.

Metadata readers must remain independently pollable. Never await a per-file
load gate owned by another query input: a prefetched probe can be paused while
the build input needs the same footer. Contended cold loads currently read
independently, expose a bypass counter and preserve the byte-capped warm cache.
Any future miss coalescing must pass the paused-loader liveness regression and
must not detach unbounded work or retain canceled readers in a global cache.
