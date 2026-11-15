# Local view rename

Status: bounded native source candidate; 22 focused owning controls pass.
The first three owning tests compiled and produced actual RED (zero passes/three
failures) on the Unsupported stub before implementation. Full owning package,
configured Clippy and independent source review are still pending for the new
operation. Embedding SQL/account/cache publication remains **Missing**; the
prepared-pair foundation's qualification does not qualify that integration.

`Catalog::rename_view(self: Arc<Self>, source, destination)` returns the renamed
View and allows an implementation to own its resources through native drain.
The selected FileCatalog subset is two different plain names in one local
filesystem namespace, one existing View, and an absent destination. Memory,
remote, cross-namespace, materialized-view and unsupported-location variants stay
explicit refusals in the first version. Existing table rename is unchanged.

`Arc<FileCatalog>::prepare_view_rename(source, destination)` returns an opaque
`PreparedViewRename`, retaining the native root writer without changing either
name or creating controls. Dropping preparation releases it. `publish()` returns
the renamed `View` or `ViewRenameError { outcome: PairOutcome, source }`.
`NotCommitted` describes the operation, independently of catalog health: observed
required-control corruption can still fence the catalog. `DecisionUnknown` and
`CommittedNeedsRecovery` always require explicit recovery before admission.
The Catalog convenience method delegates to that owned publisher and retains the
typed error as its external source. Other catalog implementations keep the
default Unsupported result.

The native operation must preserve ViewMetadata UUID, all schemas, versions,
version_log, current version, representations and properties exactly. Only its
location changes to the destination. Old raw metadata images stay byte-identical;
this is not CreateView with a new identity or a reconstruction from current SQL.
Opaque creator-incarnation properties belong to the embedding engine and must
survive without interpretation or generation here.

Reuse the prepared-pair root capability, cooperative read/write exclusion,
blocking ownership, phase-aware health and exact redo boundaries. A new durable
view namespace decision must validate both names, retain prior history files,
publish the exact new location, retire the old name and recover pending work
before ordinary admission. Do not acknowledge partial publication or clear
foreign/corrupt required-control fences. A predecision failure leaves source
usable and destination absent; postdecision uncertainty is fenced until redo.

Live old-name view writers require a fresh name/head identity fence: the old
update_view cache alone can otherwise recreate a removed old location. Tests must
also recreate the old name and prove its new UUID cannot be touched by a retained
predecessor handle. Existing non-filesystem and ordinary table behavior stays on
its current dispatch.

The retained-writer control uses distinct preloaded FileCatalog instances, then
recreates the old name up to the same v1 head basename with a different UUID.
The peer-listing control requires cached old names to disappear after rename.
Namespace enumeration/existence must participate in native rename read exclusion;
internal calls under existing guards must use borrowed implementations rather
than recursively reacquiring the fair read lock.

Passing owning controls cover a table collision/wrong source kind, same-name and
cross-namespace refusals, memory refusal, source/target symlink bounds, real paused
worker cancellation and independent blocked reader/namespace writer, predecision
abort, four postdecision failure boundaries, actual process exits at five phases,
repeated recovery, missing/corrupt successor and historical images, required
control corruption/loss, preserved unknown evidence, byte-identical raw history,
reopen, repeated rename and later ordinary update/DROP/recreation. A V2 rename,
V1 prepared append pair, then V2 rename share the same terminal control slot.
No SQL support, XFAIL retirement, Rustice pin or P0 closure follows from these
native metadata controls.

The native state machine is bounded full-image publication:

1. Under the existing catalog writer, capture the exact source head and all
   regular metadata/history files with root-relative length/checksum receipts.
   Validate plain local locations and an absent same-namespace destination before
   any control or destination write. A discarded preparation changes no names.
2. Persist the required root witness and READY journal before creating an owned
   operation staging directory inside the source's metadata directory. Nested
   staging cannot introduce another catalog object or replace the old direct-child
   head. Stage byte-identical historical files plus one new current head whose
   typed image differs only in location, and an operation-bound owner marker.
   Sync files and directories; the public destination remains absent.
3. Fsync COMMIT as the decision, then move the complete staged directory to the
   absent destination under exclusive namespace ownership. Source retirement
   removes only exact captured files. Persist COMPLETE and publish the destination
   cache only after its exact image/history is durable and the old name is gone.
   Cooperative namespace/list/existence/load readers remain excluded throughout.
   Delivery follows the foundation's enqueueing-versus-acknowledgement contract.
4. READY recovery verifies the unchanged source and removes only its operation's
   known partial staging files. COMMIT recovery validates the full staged or
   already-published destination graph before publishing/retiring any remaining
   exact source files. Missing/corrupt/foreign
   evidence fences admission; it is never repaired by inventing history. Terminal
   journals do not undo later ordinary updates, DROP or old-name recreation.

Reuse the existing single required witness/journal slot: append records retain
strict V1 decoding; view rename uses a strict tagged V2 image. A new publication
can replace only a valid terminal prior record. One slot/common writer cannot
legitimately hold two pending decisions. Recovery dispatches the validated record
kind under one owner before clearing health; synchronous constructor admission
already detects the required slot. No extra native query-file probe is needed
after the root's existing one-time UNPREPARED admission check. Older implementations
must refuse the new journal version, not reinterpret it as absent legacy data.

Each current/predecessor head is plain JSON, at most 1 MiB; gzipped current heads
refuse before effects in this first subset. At most 4094 regular direct-child
history files and 16 MiB of total history/new-head/owner allowance are admitted;
the journal is at most 16 MiB. Historical metadata files are hard-linked
byte-identically into private staging. The mutable version hint is rebuilt and a
new current catalog head differs only in location. Receipt paths/count/framed
FNV-1a checksums bind the exact bounded history, with the foundation's explicit
noncryptographic accidental-corruption contract. Recovery never invents a missing
or corrupt image and does not silently treat required controls as legacy absence.

The blocking owner retains the root writer through IO and cache cleanup after its
async waiter is cancelled. Closed receiver at delivery fences. Successfully
enqueueing a receipt is not proof the caller polled it: abandoning an already
queued result can leave a coherent healthy commit. No acknowledgement wait is
held against an idle future. The embedding layer must separately coordinate SQL
transaction/DDL guards, account identity/grants, provider caches and its statement
read lifetime before exposing view rename. Native preservation of an opaque
creator marker is not an ownership check.

```sh
cargo test --offline --locked -p iceberg-file-catalog --lib view_rename -- --test-threads=1
```

The focused native receipt is 22 passed/zero failed (including the inert helper
under normal discovery); its recovery test invokes the actual test binary in five
children exiting from the real blocking worker. Full package checks and SQL/stock
fixture acceptance must be recorded independently on their final source trees.
