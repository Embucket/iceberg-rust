# Iceberg-rust

Low level implementation of the Apache iceberg specification.

Featues:
- catalog trait as an interface to the different iceberg catalogs
- table structs to simplify the access to the iceberg table metadata
- table transactions to guarantee atomic changes to the iceberg table metadata
- view structs to define iceberg views

`Catalog::rename_view_from_metadata` is an object-safe, default-Unsupported seam
for preserving an already-authorized view image through rename. Implementations
must compare the full typed predecessor under publication exclusion and must not
fall back to unchecked rename. The bounded local implementation and its separate
embedding ownership boundary are documented in
[FileCatalog view rename](../catalogs/iceberg-file-catalog/VIEW_RENAME.md).

The separate default-Unsupported
`Catalog::rename_view_from_metadata_with_property` accepts the complete expected
predecessor plus a private-fields, non-Clone `ExistingViewPropertyReplacement`.
It describes exactly one existing bounded property replacement in the same
namespace decision; it supplies no arbitrary full successor, callback or SQL
authorization. The bounded native FileCatalog V3 implementation and source-bound
CI qualification are documented at the same link. Existing location-only callers
retain V2 semantics; embedding SQL activation requires its own qualification.

`ExistingViewPropertyPairReplacement::try_new([single_a, single_b])` is the separate
fixed-two bounded description for `rename_view_from_metadata_with_property_pair`.
The new object-safe method defaults to Unsupported. Both keys must already exist
with exact old values under full-predecessor validation; no arbitrary After,
callback or postcommit metadata update is accepted. The V4 FileCatalog source is
[documented separately](../catalogs/iceberg-file-catalog/VIEW_RENAME_PROPERTY_PAIR.md)
with source-bound `9cc1c57d` configured native qualification: all 19 new controls,
full FileCatalog 122 PASS / 0 FAIL / 1 ignored and configured 1.95 Clippy/fmt PASS.
The original `13c1290c` guard failures remain retained. V2/V3 behavior and
qualification stay separate, as do embedding SQL workflows and engine pins.
