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
