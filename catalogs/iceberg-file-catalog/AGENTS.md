# File Catalog Guidance

- The explicit V3 existing-property rename has source-bound native qualification
  at `779e2ba9`; see [the evidence](VIEW_RENAME.md#one-existing-property-v3-source-candidate).
  Embedding SQL/account/cache qualification remains separate. Keep V2 encodings
  and callers unchanged. A V3
  successor is derived from the full captured Before, destination and exactly
  one existing bounded property replacement; never accept a caller-supplied
  full After, mutation callback or postcommit property write. Decode the exact
  version/kind and descriptor before recovery effects; unknown records fence.
  Native property strings supply no SQL rights or marker interpretation. The
  embedding owner must separately prove its selected key and replacement.

- [Local view rename](VIEW_RENAME.md) is a bounded source candidate.
  Preserve full typed/raw history, UUID and opaque creator markers; never adapt
  the table relocation's UUID-reset/current-snapshot semantics. Its owned
  publisher shares the root's single versioned decision slot with append pairs.
  Preserve actual worker exclusion, staged-history receipts, durable COMMIT redo
  and required-control fences. READY staging belongs inside source metadata;
  neither a new logical name nor cache entry may appear before the decision.
  Old-name local view updates require a fresh physical head/image check, including
  across distinct catalog instances and old-name recreation with a new UUID.
  Borrowed reader listing/existence must not reacquire the fair root lock.
  Embedding callers use the object-safe checked-predecessor method, comparing
  full typed metadata under the retained writer before any publication effect.
  Never fall back from that method to unchecked rename or interpret creator
  properties as native authorization. Identical typed no-op head advances are
  admissible; arbitrary external writers remain outside the owner contract.

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

Derive a stable native mutation key even before the catalog root exists: resolve
the nearest existing ancestor, then append only known-missing normal components.
Never cache a guessed lexical key after canonicalization fails. Existing
unresolvable ancestors must fail async construction or produce a fenced
synchronous catalog-list handle. Keep a stable-symlink/missing-leaf old-reader
versus new-writer control. Unsupported non-filesystem construction and lookup
must bypass the shared pair-state registry.

Normal load_tabular owns a single read scope; borrowed scope.load uses its
unlocked internal implementation. Do not reacquire the fair RwLock through that
trait method while holding a reader and a writer is queued. This scope is not a
Rustice SQL/cache/physical lifetime snapshot. The embedding layer must supply a
separate statement read/publication boundary before enabling INSERT ALL.

`scope.load_if_present` is a separate bounded Unix-filesystem read API. Return
None only for a positively missing exact plain endpoint below the same actual
live nonsymlink namespace. Existing endpoint/metadata/head corruption, redirected
or nonregular paths and unknown parents remain errors. Keep the original reader
through the physical proof, ordinary full native load and identity rechecks.
Never map CatalogNotFound to None: native load also uses it for missing metadata.
Borrowed exists/list preserve their old under-scope helper behavior and are not
coherent physical absence evidence. No trait reentry, writes, head-selection
change, new dependency or row conversion belongs in this path. Source `3a706411`
has seven native controls plus the full owning package 91 PASS / 0 FAIL /
1 inherited ignored benchmark, and configured 1.95 all-target/all-feature Clippy
PASS. Preserve the original b37 compiler failure and wrong-CWD full90/1/1 result.
Use the canonical Cargo package CWD for inherited relative fixtures; an unchanged
ELF may produce a setup failure from the workspace root. This qualification does
not authorize an embedding engine pin, SQL/account/cache activation or a claim of
fork-wide CI/performance coverage.

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

Operation outcome and catalog health are distinct. A NotCommitted error must
not reset an observed required-control corruption fence or overwrite the bad
control with an abort record. Validate existing terminal controls under the
same owner before publication, preserve bad bytes and require explicit repair.

Delivery is successful oneshot enqueueing, not proof the caller polled a receipt.
An already-closed receiver fences; a queued receipt abandoned later can leave a
coherent healthy commit. Do not infer cancel implies a fork fence or hold an
acknowledgement wait against an idle retained future. Keep a deterministic
queued-result cancellation control and describe this boundary to embedders.

## Exactly two existing-property rename source

The separate [V4 pair source](VIEW_RENAME_PROPERTY_PAIR.md) is uncompiled and
unexecuted. Keep its public fixed-two description private, non-Clone and
nonserialized. Derive the complete After from one captured full Before, location
and exactly two distinct bounded existing-value replacements under the original
writer. No callback, insertion/removal or later property commit is authority.
Preserve V2/V3 encodings, old controls and their already-qualified source claims.
V4 descriptors require exact length, ascending distinct keys, exact fields and
full-image revalidation before recovery effects. Native strings interpret no SQL
or clock: an embedding owner generates its clock once and retains that exact
value through its own intent/recovery. Existing owner, history, drain, outcome,
health and delivery boundaries are shared, not reimplemented.
