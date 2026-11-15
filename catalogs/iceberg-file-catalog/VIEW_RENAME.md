# Local view rename preparation

Status: test-first preparation, runtime **Missing**. The new Catalog method has
an explicit Unsupported default and FileCatalog has not implemented it yet.
The first three owning tests compiled and produced the expected actual RED
(zero passes/three failures) on the Unsupported stub before implementation.
The later peer-cache/old-name recreation refinements remain uncompiled. The existing
prepared-pair foundation's qualification does not qualify this new operation.

`Catalog::rename_view(self: Arc<Self>, source, destination)` returns the renamed
View and allows an implementation to own its resources through native drain.
The selected FileCatalog subset is two different plain names in one local
filesystem namespace, one existing View, and an absent destination. Memory,
remote, cross-namespace, materialized-view and unsupported-location variants stay
explicit refusals in the first version. Existing table rename is unchanged.

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

Further required controls before implementation acceptance include table/view
destination collisions, wrong source kind, source/target no-follow path bounds,
real paused worker cancellation and held read/write exclusion, predecision and
postdecision failure injection, actual process exits/repeated recovery, missing
required controls, raw history byte preservation and catalog reopen. Embedding
SQL/account/cache publication remains a separate owner boundary; no SQL support,
XFAIL retirement, Rustice pin or P0 closure follows from this preparation.

The proposed native state machine is bounded full-image publication:

1. Under the existing catalog writer, capture the exact source head and all
   regular metadata/history files with root-relative length/checksum receipts.
   Validate plain local locations and an absent same-namespace destination before
   any control or destination write. A discarded preparation changes no names.
2. Persist the required root witness and READY journal before creating an owned
   destination directory. Stage byte-identical historical files plus one new
   current head whose typed image differs only in location. The destination's
   owner marker binds this operation, so cleanup never adopts unrelated files.
   Sync files and directories. Cooperative namespace/list/existence/load readers
   remain excluded by the writer throughout this interval.
3. Fsync COMMIT as the decision. Source retirement then removes only exact captured
   files; persist COMPLETE and publish the destination cache only after its exact
   image/history is durable and the old namespace entry is gone. Delivery follows
   the foundation's enqueueing-versus-acknowledgement contract.
4. READY recovery verifies the unchanged source and removes only a proven owned
   partial destination. COMMIT recovery validates the full decided destination
   graph before retiring any remaining exact source files. Missing/corrupt/foreign
   evidence fences admission; it is never repaired by inventing history. Terminal
   journals do not undo later ordinary updates, DROP or old-name recreation.

Required controls for the pair and view families must be checked together before
either recovery changes a head. Two simultaneous pending decisions are invalid:
the common writer cannot legitimately create them. Synchronous constructor
admission must detect either required-control family; explicit asynchronous
recovery may clear health only after both are valid. No extra native query-file
probe is needed after the root's existing one-time UNPREPARED admission check.

The exact codec, source/destination receipt budgets and fault stages remain to be
implemented and tested. The above is a proposed protocol, not a successful
publication or recovery receipt.
