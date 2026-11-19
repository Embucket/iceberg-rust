# Exactly two existing-property view rename

Status: implementation source candidate, uncompiled and unexecuted. The separate
test-first parent is retained. No native or embedding SQL success is claimed.

The fixed-array description accepts exactly two distinct existing keys
with the existing single-value bounds. Its constructor canonicalizes key order.
The V4 kind is `view-rename-property-pair` with exactly two strict descriptors in
`property_replacements`. Recovery derives the entire successor from the complete
Before, destination and these two old-to-new value replacements. There is no
caller After, callback, insertion, removal or postpublication update.

V2 location-only and V3 one-property bytes/semantics remain independent. New
fixtures and controls cover exact full-image/history/reopen, bounds, stale full
Before and either old value, malformed and noncanonical V4 descriptors, extra
edits, V2/V3 golden bytes, mixed terminal decisions, actual worker cancellation,
queued lost response and five-phase owned child-process recovery.

The intended engine owner separately validates `embucket.sql.view-source-sql.v1`
and `embucket.sql.view-options.v1`, generating its clock once and preserving it in
its own intent. Native strings remain opaque, and `embucket.sql.view-definition`
is preserved. SQL rights, account/cache/transaction lifetimes, clock semantics,
engine pin and workflow qualification remain outside this primitive.

`ExistingViewPropertyPairReplacement::try_new([single_a, single_b])` consumes two
validated `ExistingViewPropertyReplacement` descriptions, rejects duplicate keys
and orders them canonically. Its fields are private; it implements neither Clone
nor serialization. `Catalog::rename_view_from_metadata_with_property_pair` is
object-safe and default-Unsupported. FileCatalog preparation compares the full
actual Before under the same retained writer, derives both replacements in memory
and revalidates the complete After before any namespace/control/cache effects.

V4 recovery requires the exact version/kind, exactly two ascending unique keys,
strict three-string descriptors and exact field set. Both old values and every
modeled successor field are recomputed. Unknown successor JSON fields also refuse
rather than disappearing through typed deserialization. V2/V3 dispatch, descriptor
field sets and encoded bytes are unchanged, including V2/V3 mixed-terminal use.
The same required decision slot, writer, blocking job, history receipts, outcome
and health fences, delivery and recovery own the full operation; no new state
machine or dependency is introduced. This does not protect against hostile
external writers or establish SQL rights.

Fifteen new source test functions include 39 nonidentity malformed-descriptor or
extra-edit cases and one deliberately mixed private plan. Real cancellation uses
an acknowledged pause in the blocking writer, actual caller cancellation and an
independently blocked reader before drain. The queued-result control distinguishes
lost response from failed publication. Five-stage subprocess recovery requires
actual child exit86 and idempotent full-image recovery. Counts are source inventory
only; compilation, CI and all runtime results remain pending Root review.

V4 additionally requires both raw predecessor and successor JSON Values to equal
the corresponding complete typed native image. Whitespace and object-key order
are immaterial. This is the supported existing native-creator subset: current
FileCatalog plain JSON metadata is emitted from that typed serialization. A
legacy/external raw image containing unknown fields, or a serde-accepted omitted
or alternate field spelling that reserializes differently, refuses V4 before
publication rather than dropping unmodeled data. Its historical bytes remain
untouched. V2/V3 retain their previous accepted forms and exact descriptor rules.
A new real-file control requires unknown-raw-Before refusal without effects and
acceptance of pretty-printed equivalent Before JSON. Actual compilation/results
remain pending; this scope is documented rather than assumed legacy support.

## Causal source correction: ID-keyed native arrays

The initial `13c1290c` raw-JSON guard missed an existing native representation
property: `versions` and `schemas` deserialize to HashMaps and serialize using
unsorted `into_values()`. A fresh native capture can therefore reorder their
arrays without changing any metadata. The initial source/CI outcome is preserved;
that source is not eligible for merge even if a particular random ordering passes.

The separate causal source compares these two top-level arrays by integral
`version-id` and `schema-id` only. Missing, nonintegral or duplicate IDs refuse.
Whole entries and unknown fields remain in the comparison; there is no general
JSON sorting or projection. All other arrays, especially chronological
`version-log`, schema field order and representation order, remain exact. Old
V2/V3 handling and the fixed-two public API do not change.

Four additional test-first controls build at least three versions, two schemas
and two chronological log entries. They force all four array-order combinations
for raw Before and successor JSON, refuse ambiguous IDs without effects, retain
nested/top-level unknown-field refusals and require a genuine log reversal to
refuse. They are authored but uncompiled/unexecuted locally. These four extend the
original fifteen-function source cohort; remote CI and qualification remain
pending independent review. No native or engine result is inferred here.
