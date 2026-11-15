# File Catalog

The separate [local view rename](VIEW_RENAME.md) adds a default-Unsupported
Catalog API and an explicit local FileCatalog publisher. Its 23 focused owning
controls pass, including actual process-exit recovery and worker cancellation.
The complete file-catalog package passes 78 tests/zero failures/one inherited
ignored benchmark; configured 1.95 Clippy and whole-workspace format checks pass.
Independent source review and embedding SQL/account/cache acceptance remain
separate; the prepared-pair foundation checks below do not qualify that adapter.

Filesystem and object-store-backed Iceberg catalog.

## Local Namespace Mutations

For filesystem namespaces containing one exact component matching
`[A-Za-z_][A-Za-z0-9_. ]*`, CREATE and DROP run as blocking filesystem jobs.
This includes quoted SQL schema names such as `a.b` and `Quoted Name`: dots and
spaces remain inside that single component. Separators, NUL/control characters,
percent-encoded spellings, `.` and `..` do not enter this durable subset.
The existing process-local catalog mutation lock is owned by the actual job,
through directory changes and DROP's cache cleanup. Cancelling its asynchronous
waiter does not release that lock while the filesystem job can still mutate.
Later mutations on the same catalog-lock key wait for the job to finish.

Cancellation does not roll back the operation or establish its outcome. Blocking
filesystem IO cannot be forcibly stopped, so a caller's deadline must not be
interpreted as proof that CREATE or DROP did not happen. DROP treats an absent
directory as success and does not follow symlink targets outside the namespace.

Memory/object-store namespaces and other identifier forms retain their existing
behavior. This is not distributed locking, general catalog transaction atomicity,
durable namespace-incarnation fencing, or namespace-property persistence. It does
not fence data writers outside the catalog protocol or another component's late
display-marker writes.

The `filesystem_quoted_empty_namespaces_*`, `durable_namespace_subset_*`,
`cancelled_namespace_*` and
`filesystem_namespace_drop_does_not_follow_nested_symlinks` tests exercise real
blocking jobs, separate catalog instances, encoded roots, populated deletion,
cache cleanup and recreation. Their pauses and waits are bounded.

```sh
cargo test -p iceberg-file-catalog --lib cancelled_namespace
cargo test -p iceberg-file-catalog --lib filesystem_namespace_drop
```

## Prepared append pairs

Status: the original prepared-pair foundation's canonical package checks passed: 55 tests,
zero failures and one inherited ignored benchmark, including all 24 pair controls
and the existing namespace and real S3/LocalStack controls. Configured Clippy
1.95.0 passes for this package's all-targets/all-features check with warnings
denied; whole-workspace format and independent source review also pass. Broader
fork CI and embedding-engine acceptance remain separate. This API
does not implement SQL INSERT ALL, multi-table user transactions, engine cache
snapshots, or distributed transactions.

The existing file catalog still owns ordinary table metadata lookup and single
head updates. The explicit prepared_pair module adds a bounded local primitive:

- Arc<FileCatalog>::prepare_append_pair([CommitTable; 2]) returns an opaque
  PreparedAppendPair, retaining catalog-wide write exclusion. It accepts two
  distinct existing local tables and exactly one native main-branch Append
  snapshot/ref update per table. Use native TableTransaction::prepare_commit
  after writing data files; do not call a committing INSERT to prepare inputs.
- PreparedAppendPair::publish() returns CommittedPairReceipt with operation
  UUID and both exact table metadata images. It prepares one fsynced decision
  and installs the two heads as steps of that same recoverable decision.
- FileCatalog::read_scope() returns an opaque cooperating reader scope.
  scope.load(identifier) borrows its existing exclusion, with no recursive
  fair-RwLock acquisition. Ordinary filesystem Catalog::load_tabular takes one individual
  read scope and shares its implementation with borrowed loads. Non-filesystem
  construction and loads retain their original unlocked dispatch without entering
  the process-wide pair-state registry; the new reader scope refuses unsupported
  backends.
- recover_append_pair() runs under catalog-wide write exclusion. The async
  FileCatalog::new constructor runs recovery before returning a usable handle.
  A synchronous FileCatalogList::catalog handle refuses required journals
  until asynchronous construction/recovery establishes a healthy receipt.

Preparation publishes nothing. Dropping it has no control-file or table effect.
All normal head writers and namespace mutations remain excluded while a prepared
pair exists. Both target requirements, exact predecessor bytes, UUID markers and
derived successor locations are validated before a decision. Non-filesystem,
renamed, foreign-storage, branch, schema/property/location-changing and duplicate
target inputs refuse explicitly.

The subset uses one-component plain namespace/table names, with ASCII initial
letter/underscore and subsequent alphanumeric/underscore/dot/space, up to 255
bytes. Each metadata image is at most 1 MiB; the full journal is at most 16 MiB.
At most 4096 reachable artifacts are admitted, each manifest/list is at most
1 MiB, their total encoded metadata is at most 16 MiB, and the complete synced
artifact payload is at most 256 MiB. These are explicit first-version limits,
not general Iceberg or SQL limits. Paths are derived through the configured
filesystem store. Catalog mutation/fence keys bind the configured native root at
construction. If the root does not exist yet, its nearest existing ancestor is
canonicalized before appending only known-missing normal path components. A
stable symlink ancestor therefore has the same key before and after root
creation, including across distinct catalog instances. Unresolvable existing
ancestors refuse; no lexical fallback creates healthy authority. Async
construction returns that error, while the synchronous catalog-list trait
returns a fenced handle. URI/prefix aliases share exclusion; different native
roots do not share authority. Symlink/foreign-root artifacts and unexpected heads
refuse.
The root owner must exclude outside path swaps/writers. Process-local locks do
not establish that external ownership.

Two required fixed controls live in the catalog directory:

- .iceberg-append-pair-required-v1 binds the actual configured native root;
- .iceberg-append-pair-v1 holds one bounded exact-image operation with phase
  ready, commit, complete or aborted.

The journal includes both predecessor byte images and typed native commits plus
the exact successor bytes. Version 1 explicitly requires plain JSON predecessor
heads; ordinary single-table gzip metadata reads/writes are unchanged. Decoding
and publication compare the parsed predecessor bytes with the captured typed
metadata, so a self-consistent typed transition cannot invent old properties. COMMIT is the durable decision; immutable data,
manifest/list files and parent directories are synced first. Each head is
installed atomically from a fully synced private temporary file, with create
semantics. Recovery accepts only the exact old or exact decided new head and
checks physical UUIDs. It never overwrites an unrelated successor.

The complete prepared artifact graph is retained as sorted unique root-relative
receipts with byte lengths and fixed FNV-1a-64 checksums, plus a framed checksum
and count for the receipt set. Snapshot manifest-list references must be members.
Preparation captures those receipts in an owned blocking reader; publication
checks them again before the decision. Pending COMMIT recovery verifies every
receipt, including data files, manifests and lists, before installing any head or
publishing a cache image. Missing, truncated or changed files fence recovery.
Lengths and FNV checksums detect accidental corruption; this is explicitly not
cryptographic authenticity or a hostile-writer/collision-resistance contract.
The embedding root owner must exclude writers outside the catalog protocol.

PairPublishError.outcome distinguishes NotCommitted, DecisionUnknown and
CommittedNeedsRecovery. A predecision failure changes no table. NotCommitted
describes the operation, not catalog health. Observed foreign/corrupt/missing required
controls independently fence ordinary admission, preserve their exact bytes and
prevent an abort journal from replacing them. Fresh controls are checked during
preparation and again at the publication boundary. An uncertain
decision or postdecision failure fences ordinary head admission until exact redo
recovery; it must never be reported as a rollback. Ready recovery keeps old
heads. Commit recovery installs both new heads idempotently before exposure.
Complete/aborted journals do not roll back later ordinary writes or DROP/recreate,
but their root, format and transition syntax must still validate. Losing a
required witness/journal is an error, never legacy-absence fallback.

The actual blocking publisher owns the FileCatalog, writer guard and cache cleanup
through IO. Cancelling its waiting future does not release exclusion. If its
receiver is already closed when the worker delivers the result, admission
remains fenced until explicit recovery. Successfully enqueueing a receipt is the
fork's delivery boundary, not an acknowledgement that the caller polled it. A
receipt queued to a retained idle future can later be discarded while the fully
installed coherent commit remains healthy. No acknowledgement wait blocks the
native worker. Rustice must not infer cancellation implies a fork fence; its
statement/cache visibility and result-delivery owner must resolve the outcome.
Embedding engines must retain their own root owner, transaction/database guards
and admission through that native worker's drain.

A cooperating read scope only protects catalog loads. An embedding engine must
also hold a separate statement-wide visibility scope across warm caches, retained
providers/views and physical snapshot construction. Two individually locked
loads are not a statement snapshot. Never claim atomic SQL visibility by looping
update_table or by holding only name/root metadata locks. Cache epoch refresh,
stale statistics and late result publication remain embedding responsibilities.
No broader SQL support or XFAIL retirement follows from this crate primitive.

Owning controls cover exact two-head publication and reopen, invalid second
requirements, discarded preparation, borrowed-reader writer exclusion,
predecision failure, first-head failure/fencing/redo, actual blocking-worker
cancellation and held exclusion, foreign successor/required-control loss, and
actual child-process termination at ready/commit/first-head with repeat recovery.
Review controls additionally cover missing/same-length-corrupt data/manifest/list
artifacts before and after the first installed head, predecessor typed/byte-image
tampering, explicit gzip refusal, duplicate physical UUID admission/publication,
and unchanged non-filesystem construction/load while its writer lock is held.
Final controls separately prove foreign/corrupt/dangling controls are present
authority errors, and NotCommitted outcomes retain a health fence, and deterministically queue then abandon a delivered receipt to
prove a coherent healthy commit is allowed in that specific cancellation window.
Alias controls construct the first catalog before its leaf exists, hold its
reader against a peer constructed after creation, then publish/reopen through
the alias. Separate controls prove unresolved ancestors remain fenced and remote
construction/catalog-list lookup bypass a held process-wide pair registry.
The tiny data-file test payload only validates metadata/manifest publication;
it makes no Parquet decoding or SQL row-acceptance claim.

Run the focused controls and existing namespace controls before a PR:

~~~sh
cargo test -p iceberg-file-catalog prepared_pair::tests -- --test-threads=1
cargo test -p iceberg-file-catalog namespace
cargo test -p iceberg-file-catalog --lib -- --test-threads=1
cargo +1.95.0 clippy -p iceberg-file-catalog --all-targets --all-features -- -D warnings
cargo +1.95.0 fmt --all -- --check
~~~

The isolated run compiles the actual FileCatalog production source and these
owning tests against pinned frozen dependency rlibs; it excludes the unrelated
original DataFusion/Docker test modules. The tests-first unsupported stub compiles
and its two-head/reopen success control fails before implementation. This is not
a full Cargo workspace or Rustice runtime acceptance receipt. The later canonical
package run includes the complete original DataFusion/Docker test module. Owning
package, namespace, configured static checks and independent source review now
pass; this does not qualify SQL INSERT ALL, engine caches or a fork-wide CI run.
