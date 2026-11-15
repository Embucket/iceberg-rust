# File Catalog Guidance

- This crate owns file-catalog metadata lookup and filesystem/object-store
  mutations, not SQL session selection or Rustice display markers.
- Preserve the existing URI/object-path conversion helpers. Tests must derive
  native paths through the configured store rather than assuming URL spelling
  is already a native filesystem path.
- A filesystem mutation's lock must be owned by its blocking job through IO and
  cache cleanup, not only by the cancellable future awaiting its JoinHandle.
- Keep non-filesystem behavior and table/query fast paths unchanged when fixing
  local namespace lifecycle. Process-local locks do not establish distributed
  atomicity or stop external data writers.
- Durable empty filesystem namespaces admit exactly one component matching
  `[A-Za-z_][A-Za-z0-9_. ]*`. Keep dots/spaces inside that component; never admit
  separators, controls, encoded spellings or traversal components. Preserve the
  existing behavior outside this bounded durable subset.
- Cancellation tests must pause inside the actual blocking worker, await the
  caller's cancellation, and independently verify the lock is still held. Bound
  both worker pauses and async waits; a single pending poll is not proof.
- Run focused namespace tests and an independent review before finalizing a PR.

## Prepared append pairs

The [explicit prepared-pair API](README.md) owns a bounded local two-head commit
decision and redo recovery. The caller supplies native prepared commits; no
ordinary update_table loop is an atomic transaction. Preparation retains one
catalog-wide writer and publishes nothing. Only the blocking publisher may
persist the ready/commit/complete controls, install heads or clean up its cache.
It owns that guard until IO finishes, including a cancelled receiver.

A durable or uncertain decision is never rolled back. Fence all ordinary head
admission until exact recovery; validate root binding, required controls, both
old/new byte identities, physical UUIDs and native transitions. Terminal records
must not undo later normal writes. Configured store prefixes are part of the
runtime fence and durable root identity; identical URI spellings for different
native roots must not share authority.

Normal load_tabular owns a single read scope; borrowed scope.load uses its
unlocked internal implementation. Do not reacquire the fair RwLock through that
trait method while holding a reader and a writer is queued. This scope is not a
Rustice SQL/cache/physical lifetime snapshot. The embedding layer must supply a
separate statement read/publication boundary before enabling INSERT ALL.

Retain source-before-implementation controls, real paused blocking-worker
cancellation/exclusion proof and actual child-process recovery tests. No new
runtime dependencies, row conversions or successful SQL support claims belong
to this metadata primitive.

Pending COMMIT recovery must validate all persisted bounded artifact receipts
before any head/cache installation. Receipts bind canonical root-relative graph
members, exact lengths and explicitly noncryptographic accidental-corruption
checksums; their set has a count/framed checksum. They are not authenticity
proofs against an external writer. Parse plain predecessor bytes and compare the
typed image; v1 refuses gzipped predecessor heads rather than accepting an
unverified correspondence. Preserve ordinary gzip and non-filesystem paths.
Check distinct table UUIDs again at the publication boundary, not only decode.
