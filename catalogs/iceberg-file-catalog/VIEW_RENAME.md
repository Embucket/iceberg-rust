# Local view rename

## One existing property: V3 source candidate

Status: bounded native API qualified at code source `779e2ba9`, based on merged
native `8ac7e285`. The [configured test job](https://github.com/Embucket/iceberg-rust/actions/runs/44613303413/job/135875252568)
passes all six package steps. FileCatalog passes **103 tests, zero failures and
one inherited ignored benchmark**, including all 41 rename test functions and
all 12 added property test functions. The latter include one child helper that
is inert in the ordinary root run; its real five-phase exit/recovery parent
also passes. Nested child test summaries are excluded from the 103 count.
The [configured lint job](https://github.com/Embucket/iceberg-rust/actions/runs/44613303413/job/135875252059)
passes Rust 1.95 all-target/all-feature Clippy with warnings denied and workspace
format checks. These results qualify the native API, not a Rustice consumer.

Test-first checkpoint `607823ea` was authored without an observed RED run.
The first implementation CI at `d486d72f` retains its 102-pass/one-failure/one-ignored
result: a negative control cleared an already-empty version log. The test-only
successor makes that mutation change the image and checks every corruption
case for nonidentity before testing rejection. Production code is unchanged by
that correction. Older qualification counts below retain their original scope.

`ExistingViewPropertyReplacement::try_new(key, expected, replacement)` is a
non-Clone, non-serialized untrusted description with private fields. It admits
one 1..255-byte ASCII key (initial letter/underscore, then alphanumeric/underscore/
hyphen/dot), two different nonempty UTF-8 values of at most 64 KiB each, and no
NUL in either value. Newline/tab remain opaque value bytes. Missing keys, wrong
old values, insertion/deletion and descriptions outside those bounds refuse.
The primitive accepts no complete After image and no callback.

The explicit object-safe
`Catalog::rename_view_from_metadata_with_property(source, destination, expected,
replacement)` defaults to Unsupported. FileCatalog's
`prepare_view_rename_with_property` retains the original preparation writer and
checks the entire actual typed predecessor against `expected` before controls,
staging, destination or cache effects. It derives the successor from that Before
by changing only location and the one declared existing value. Publication and
recovery retain UUID, all schemas/versions/logs/representations and every other
property. There is no second metadata commit after the name publication.

The same original blocking publisher, root witness, exclusion, artifact history,
outcome/health, receiver-delivery and exact redo protocol are reused. V3 records
have exact kind `view-rename-property` and one strict three-string
`property_replacement` descriptor, alongside the existing full old/new images.
Decode rechecks the bounds, exact old property and computed full After before
any recovery effect. Unknown versions/kinds/fields and inconsistent images
refuse; they cannot be interpreted as absent legacy data. V2 location-only
records retain their exact old field set, bytes and semantics; V1 pair records
retain their original decoder. A valid terminal V3 record shares the same one
required decision slot, and grants no authority over later ordinary writes.

Native controls cover dynamic checked dispatch/full-image and raw
history preservation/reopen, key/value bounds, full-Before and actual warmed
predecessor changes, coherent descriptor/After corruption, V2 byte golden,
five real publisher fault checkpoints, actual blocking cancellation and lost
response, corrupt pending records with no evidence cleanup, and child process
exit/repeated recovery at five phases. These controls pass in the source-bound
run above. Existing default callers, old controls, ordinary update and runtime
dependencies are unchanged.

This generic metadata primitive interprets no SQL or opaque creator marker and
grants no embedding rights. A future Rustice ALTER VIEW consumer must separately
bind **only** its exact captured source-SQL property and prove the bounded
CREATE-header-to-destination replacement from the actual original statement and
view image. It must coordinate account, transaction, cache and SQL publication
lifetimes before activation. ALTER TABLE-on-VIEW continues to select the
location-only API. No engine pin, marker retirement, SQL support, workflow
qualification or performance claim is made here.

## Existing location-only qualification

Status: bounded native source candidate; 29 focused owning controls pass.
The first three owning tests compiled and produced actual RED (zero passes/three
failures) on the Unsupported stub before implementation. On runtime source
`4d5a618d`, the full owning package passes 82 tests/zero failures/one inherited
ignored benchmark, including existing namespace and real S3/LocalStack controls.
Configured 1.95 all-targets/all-features Clippy with warnings denied and whole
workspace format checks pass. The publisher/accessor source review passed; review
of the later checked-predecessor seam remains pending. Embedding SQL/account/cache
publication remains **Missing**; the
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

An embedding owner must validate the exact captured predecessor, rather than a
View loaded before preparation. The selected read-only
`PreparedViewRename::source_metadata() -> &ViewMetadata` accessor borrows the
immutable captured UUID/properties/history while its native writer remains held.
It interprets no engine marker and grants no authority itself. Preparation creates
no witness, journal, READY stage, head or cache entry: an owner that denies the
captured image can drop preparation, releasing exclusion without cleanup or
publication effects. A test-first control replaces the name through a distinct
FileCatalog while the preparing catalog retains its old cache, checks the new
captured UUID/opaque creator marker and a blocked native writer, then denies/drops
the preparation and proves unchanged names/controls plus later writer progress.
The missing-method control produced only the expected E0599 compile refusals;
the accessor follow-up passes all 23 focused owning controls. Validation must not
re-enter any FileCatalog method requiring a reader or writer while this exclusive
preparation is held; that includes committing pending native table writes.

The object-safe checked integration seam is
`Catalog::rename_view_from_metadata(self: Arc<Self>, source, destination,
expected: &ViewMetadata)`. Its default is explicitly Unsupported, never unchecked
fallback. FileCatalog must compare the complete authorized typed image against
`prepared.source_metadata()` under the retained native writer. A mismatch returns
`CommitConflict` before witness, READY, staging, decision, head or cache effects;
the embedding engine still owns authorization of the expected image. No arbitrary
native handle, catalog downcast or interpretation of an engine marker is needed.
Existing unchecked `rename_view` remains a separate native convenience.

This compares modeled semantic state, not the number of physical writes. A new
head containing an identical typed UUID/location/schema/version/body/property
image is admissible; head basenames alone can be reused after DROP/recreation and
are not an incarnation proof. The root owner excludes hostile/out-of-protocol
writers capable of forging both UUID and all opaque creator metadata. This is
not authenticity or external-writer isolation. Test-first controls use warmed
caller images and a distinct native writer to replace the UUID, alter body or an
unrelated property with the UUID/creator marker unchanged, and advance the head
without changing the typed image. Dynamic Catalog dispatch and denial followed
by exact retry must prove the seam does not depend on a concrete FileCatalog.
The four controls produced actual native RED on the default-Unsupported stub
before the FileCatalog override; all 27 focused owning controls then pass. The
override retains the same publisher, typed outcome and cancellation boundaries.

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

Native name loads have no cached-metadata return path. Both ordinary
`load_tabular` and borrowed `scope.load` resolve the physical direct-child head,
read its actual metadata, then populate the cache. Two additional owning controls
keep an independent peer's predecessor cache deliberately warm: old-name direct
and scoped loads return CatalogNotFound after rename, and a recreated old name
with the same v1 head basename returns its different new UUID/image. The renamed
destination keeps the original UUID/image. These controls passed immediately on
unchanged production; no native read-cache gap or failing RED was observed and no
loader/ordinary-table/remote fast path was changed. Engine provider caches and
late background refresh publication still require their own embedding barrier;
native liveness does not invalidate an already-retained engine provider.

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
cargo test --offline --locked -p iceberg-file-catalog --lib -- --test-threads=1
cargo +1.95.0 clippy --offline --locked -p iceberg-file-catalog --all-targets --all-features -- -D warnings
cargo +1.95.0 fmt --all -- --check
```

The focused native receipt is 29 passed/zero failed (including the inert helper
under normal discovery); its recovery test invokes the actual test binary in five
children exiting from the real blocking worker. Native tests used stable 1.99.1
and two jobs in an isolated fork target; configured Clippy used its own 1.95
target and two jobs. SQL/stock fixture acceptance and broader fork CI must be
recorded independently on their final source trees.
The two later peer-load controls are a tests/docs-only addition: all 29 focused
controls and configured Clippy pass, while the full 82-test package receipt stays
bound to unchanged production source `4d5a618d`.

## Exactly two existing properties: V4 source only

The separate [fixed-two V4 candidate](VIEW_RENAME_PROPERTY_PAIR.md) derives exactly
two existing opaque value replacements in the same native rename decision. It is
uncompiled/unexecuted and does not inherit the V3 qualification above. Old V2/V3
APIs, journal bytes, controls and qualification records remain unchanged.
