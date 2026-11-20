# Exactly two existing-property view rename

Status: native source `9cc1c57d` is qualified by the complete configured
[CI run 44638774469](https://github.com/Embucket/iceberg-rust/actions/runs/44638774469):
FileCatalog 122 PASS / 0 FAIL / 1 ignored, including all 19 new V4 controls,
plus configured Rust 1.95 all-target/all-feature Clippy and fmt PASS. The separate
test-first parents and initial failed run remain retained. Embedding SQL, clocks,
engine pins and runtime workflows require their own qualification.

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
actual child exit86 and idempotent full-image recovery. All fifteen original
V4 test functions pass in the qualified run; the four later ID-order controls
also pass. The inert direct crash-helper test is included in these root counts;
its normal OK result alone does not prove a subprocess recovery phase.

V4 additionally requires both raw predecessor and successor JSON Values to equal
the corresponding complete typed native image. Whitespace and object-key order
are immaterial. This is the supported existing native-creator subset: current
FileCatalog plain JSON metadata is emitted from that typed serialization. A
legacy/external raw image containing unknown fields, or a serde-accepted omitted
or alternate field spelling that reserializes differently, refuses V4 before
publication rather than dropping unmodeled data. Its historical bytes remain
untouched. V2/V3 retain their previous accepted forms and exact descriptor rules.
A new real-file control requires unknown-raw-Before refusal without effects and
acceptance of pretty-printed equivalent Before JSON. Both controls pass in the
qualified run; this bounded native-creator subset does not imply legacy support.

## Causal source correction: ID-keyed native arrays

The initial `13c1290c` raw-JSON guard missed an existing native representation
property: `versions` and `schemas` deserialize to HashMaps and serialize using
unsorted `into_values()`. A fresh native capture can therefore reorder their
arrays without changing any metadata. The
[original 13c run](https://github.com/Embucket/iceberg-rust/actions/runs/44633628348)
is retained: FileCatalog 109 PASS / 9 FAIL / 1 ignored; all nine failures were in
the new raw-predecessor guard. Its old 104-test cohort remained 103 PASS / 0 FAIL /
1 ignored, lint passed and SQL catalog was skipped. No result from that failed
source is relabeled as a corrected-source success.

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
refuse. These four and the original fifteen-function V4 cohort pass on
`9cc1c57d`. No local Cargo or native test execution was used; the recorded native
result comes from the configured remote run.

## Exact configured qualification scope

The test and lint jobs both checked out the synthetic merge
`eafdc466e4750085692f107c16cc33fc29d250c4` of `9cc1c57d` into
`98fb442d` on `fix/create-schema-runtime-integration`. Both jobs completed
SUCCESS. Complete original test and lint logs were retained and independently
reviewed against the exact source declarations.

| Package | Passed | Failed | Ignored |
| --- | ---: | ---: | ---: |
| iceberg-rust-spec | 207 | 0 | 1009 |
| iceberg-rust | 244 | 0 | 2064 |
| datafusion_iceberg (25 test binaries) | 251 | 0 | 401 |
| iceberg-rest-catalog | 22 | 0 | 513 |
| iceberg-file-catalog | 122 | 0 | 1 |
| iceberg-sql-catalog | 26 | 0 | 0 |

Every actual summary reports zero filtered-out tests. FileCatalog declares 123
root tests: the unchanged 104 names/outcomes from qualified `779e2ba9` (103 PASS
and one ignored benchmark), the original 15 V4 functions and four ID-order
functions. All nine original V4 failures now pass. Its 18 nested `running 1 test`
child launches are excluded from those root totals. All four process-exit parent
controls pass; the V4 parent requires five actual child exit86 decisions and
twice-idempotent recovery for each phase. This does not invent separate child
summary records.

The final evidence update changes documentation only. Its separate source bridge
compares every non-document Git blob and file mode with qualified `9cc1c57d`; it
does not claim a new native run for the documentation child. Rustice integration,
SQL compatibility and dependency activation remain separate.
