# datafusion_iceberg

This module owns DataFusion providers and physical scans over Iceberg snapshots.
Keep schema, delete-file and snapshot semantics intact when optimizing reads.

`parquet_metadata_cache.rs` wraps every Iceberg Parquet reader with the shared
footer cache. `parquet_data_cache.rs` optionally caches immutable byte ranges;
the default capacity is zero. Both use store-qualified file identity and bounded
memory. Keep data-cache metrics attached to each scan. Preserve batched I/O on
misses, do not hold synchronous locks across awaits, and account for retained
buffer ownership rather than only a shared slice's visible length.

Validate cache changes with focused unit tests for identities, memory limits,
partial hits, error recovery, and representative Iceberg query result checks.
Consumers must include configured cache capacity in their memory budgeting.

Metadata readers must remain independently pollable. Never await a per-file
load gate owned by another query input: a prefetched probe can be paused while
the build input needs the same footer. Contended cold loads currently read
independently, expose a bypass counter and preserve the byte-capped warm cache.
Any future miss coalescing must pass the paused-loader liveness regression and
must not detach unbounded work or retain canceled readers in a global cache.
