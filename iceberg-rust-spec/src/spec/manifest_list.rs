//! Manifest list handling and management for Iceberg tables.
//!
//! This module provides the core types and implementations for working with manifest lists,
//! which track all manifest files in a table snapshot. Key components include:
//!
//! - [`ManifestListEntry`] - Entries describing manifest files and their contents
//! - [`Content`] - Types of content tracked by manifests (data vs deletes)
//! - [`FieldSummary`] - Statistics and metadata about partition fields
//!
//! Manifest lists are a critical part of Iceberg's metadata hierarchy, providing an
//! index of all manifest files and enabling efficient manifest pruning during scans.
//! They include summary statistics that can be used to skip reading manifests that
//! don't contain relevant data for a query.

use std::{collections::HashMap, sync::OnceLock};

use apache_avro::{types::Value as AvroValue, Schema as AvroSchema};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::error::Error;

use self::_serde::{
    FieldSummarySerde, ManifestListEntryV1, ManifestListEntryV2, ManifestListEntryV3,
};

use super::{
    schema::Schema,
    table_metadata::{FormatVersion, TableMetadata},
    types::Type,
    values::Value,
};

#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
#[serde(into = "ManifestListEntryEnum")]
/// A manifest list includes summary metadata that can be used to avoid scanning all of the manifests in a snapshot when planning a table scan.
/// This includes the number of added, existing, and deleted files, and a summary of values for each field of the partition spec used to write the manifest.
pub struct ManifestListEntry {
    /// Table format version
    pub format_version: FormatVersion,
    /// Location of the manifest file
    pub manifest_path: String,
    /// Length of the manifest file in bytes
    pub manifest_length: i64,
    /// ID of a partition spec used to write the manifest; must be listed in table metadata partition-specs
    pub partition_spec_id: i32,
    /// The type of files tracked by the manifest, either data or delete files; 0 for all v1 manifests
    pub content: Content,
    /// The sequence number when the manifest was added to the table; use 0 when reading v1 manifest lists
    pub sequence_number: i64,
    /// The minimum sequence number of all data or delete files in the manifest; use 0 when reading v1 manifest lists
    pub min_sequence_number: i64,
    /// ID of the snapshot where the manifest file was added
    pub added_snapshot_id: i64,
    /// Number of entries in the manifest that have status ADDED (1), when null this is assumed to be non-zero
    pub added_files_count: Option<i32>,
    /// Number of entries in the manifest that have status EXISTING (0), when null this is assumed to be non-zero
    pub existing_files_count: Option<i32>,
    /// Number of entries in the manifest that have status DELETED (2), when null this is assumed to be non-zero
    pub deleted_files_count: Option<i32>,
    /// Number of rows in all of files in the manifest that have status ADDED, when null this is assumed to be non-zero
    pub added_rows_count: Option<i64>,
    /// Number of rows in all of files in the manifest that have status EXISTING, when null this is assumed to be non-zero
    pub existing_rows_count: Option<i64>,
    /// Number of rows in all of files in the manifest that have status DELETED, when null this is assumed to be non-zero
    pub deleted_rows_count: Option<i64>,
    /// A list of field summaries for each partition field in the spec. Each field in the list corresponds to a field in the manifest file’s partition spec.
    pub partitions: Option<Vec<FieldSummary>>,
    /// Implementation-specific key metadata for encryption
    pub key_metadata: Option<ByteBuf>,
    /// First row ID assigned to newly added rows in this Iceberg v3 data manifest.
    pub first_row_id: Option<i64>,
}

/// Entry in manifest file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(untagged)]
pub enum ManifestListEntryEnum {
    /// Version 3 of the manifest file
    V3(ManifestListEntryV3),
    /// Version 2 of the manifest file
    V2(ManifestListEntryV2),
    /// Version 1 of the manifest file
    V1(ManifestListEntryV1),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(into = "FieldSummarySerde")]
/// DataFile found in Manifest.
pub struct FieldSummary {
    /// Whether the manifest contains at least one partition with a null value for the field
    pub contains_null: bool,
    /// Whether the manifest contains at least one partition with a NaN value for the field
    pub contains_nan: Option<bool>,
    /// Lower bound for the non-null, non-NaN values in the partition field, or null if all values are null or NaN.
    /// If -0.0 is a value of the partition field, the lower_bound must not be +0.0
    pub lower_bound: Option<Value>,
    /// Upper bound for the non-null, non-NaN values in the partition field, or null if all values are null or NaN .
    /// If +0.0 is a value of the partition field, the upper_bound must not be -0.0.
    pub upper_bound: Option<Value>,
}

#[derive(Debug, Serialize_repr, Deserialize_repr, PartialEq, Eq, Clone, Copy)]
#[repr(u8)]
/// Type of content stored by the data file.
pub enum Content {
    /// Data.
    Data = 0,
    /// Deletes
    Deletes = 1,
}

mod _serde {
    use crate::spec::table_metadata::FormatVersion;

    use super::{Content, FieldSummary, ManifestListEntry, ManifestListEntryEnum};
    use serde::{Deserialize, Serialize};
    use serde_bytes::ByteBuf;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
    /// V3 manifest list entry. Initially structurally identical to V2; will diverge as
    /// V3-specific manifest fields are added.
    pub struct ManifestListEntryV3 {
        /// Location of the manifest file
        pub manifest_path: String,
        /// Length of the manifest file in bytes
        pub manifest_length: i64,
        /// ID of a partition spec used to write the manifest; must be listed in table metadata partition-specs
        pub partition_spec_id: i32,
        /// The type of files tracked by the manifest, either data or delete files
        pub content: Content,
        /// The sequence number when the manifest was added to the table
        pub sequence_number: i64,
        /// The minimum sequence number of all data or delete files in the manifest
        pub min_sequence_number: i64,
        /// ID of the snapshot where the manifest file was added
        pub added_snapshot_id: i64,
        /// Number of entries in the manifest that have status ADDED (1)
        #[serde(alias = "added_data_files_count")]
        pub added_files_count: i32,
        /// Number of entries in the manifest that have status EXISTING (0)
        #[serde(alias = "existing_data_files_count")]
        pub existing_files_count: i32,
        /// Number of entries in the manifest that have status DELETED (2)
        #[serde(alias = "deleted_data_files_count")]
        pub deleted_files_count: i32,
        /// Number of rows in all of files in the manifest that have status ADDED
        pub added_rows_count: i64,
        /// Number of rows in all of files in the manifest that have status EXISTING
        pub existing_rows_count: i64,
        /// Number of rows in all of files in the manifest that have status DELETED
        pub deleted_rows_count: i64,
        /// A list of field summaries for each partition field in the spec.
        pub partitions: Option<Vec<FieldSummarySerde>>,
        /// Implementation-specific key metadata for encryption
        pub key_metadata: Option<ByteBuf>,
        /// First row ID assigned to newly added rows in this manifest.
        #[serde(default)]
        pub first_row_id: Option<i64>,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
    /// A manifest list includes summary metadata that can be used to avoid scanning all of the manifests in a snapshot when planning a table scan.
    /// This includes the number of added, existing, and deleted files, and a summary of values for each field of the partition spec used to write the manifest.
    pub struct ManifestListEntryV2 {
        /// Location of the manifest file
        pub manifest_path: String,
        /// Length of the manifest file in bytes
        pub manifest_length: i64,
        /// ID of a partition spec used to write the manifest; must be listed in table metadata partition-specs
        pub partition_spec_id: i32,
        /// The type of files tracked by the manifest, either data or delete files; 0 for all v1 manifests
        pub content: Content,
        /// The sequence number when the manifest was added to the table; use 0 when reading v1 manifest lists
        pub sequence_number: i64,
        /// The minimum sequence number of all data or delete files in the manifest; use 0 when reading v1 manifest lists
        pub min_sequence_number: i64,
        /// ID of the snapshot where the manifest file was added
        pub added_snapshot_id: i64,
        /// Number of entries in the manifest that have status ADDED (1), when null this is assumed to be non-zero
        #[serde(alias = "added_data_files_count")]
        pub added_files_count: i32,
        /// Number of entries in the manifest that have status EXISTING (0), when null this is assumed to be non-zero
        #[serde(alias = "existing_data_files_count")]
        pub existing_files_count: i32,
        /// Number of entries in the manifest that have status DELETED (2), when null this is assumed to be non-zero
        #[serde(alias = "deleted_data_files_count")]
        pub deleted_files_count: i32,
        /// Number of rows in all of files in the manifest that have status ADDED, when null this is assumed to be non-zero
        pub added_rows_count: i64,
        /// Number of rows in all of files in the manifest that have status EXISTING, when null this is assumed to be non-zero
        pub existing_rows_count: i64,
        /// Number of rows in all of files in the manifest that have status DELETED, when null this is assumed to be non-zero
        pub deleted_rows_count: i64,
        /// A list of field summaries for each partition field in the spec. Each field in the list corresponds to a field in the manifest file’s partition spec.
        pub partitions: Option<Vec<FieldSummarySerde>>,
        /// Implementation-specific key metadata for encryption
        pub key_metadata: Option<ByteBuf>,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
    /// A manifest list includes summary metadata that can be used to avoid scanning all of the manifests in a snapshot when planning a table scan.
    /// This includes the number of added, existing, and deleted files, and a summary of values for each field of the partition spec used to write the manifest.
    pub struct ManifestListEntryV1 {
        /// Location of the manifest file
        pub manifest_path: String,
        /// Length of the manifest file in bytes
        pub manifest_length: i64,
        /// ID of a partition spec used to write the manifest; must be listed in table metadata partition-specs
        pub partition_spec_id: i32,
        /// ID of the snapshot where the manifest file was added
        pub added_snapshot_id: i64,
        /// Number of entries in the manifest that have status ADDED (1), when null this is assumed to be non-zero
        #[serde(alias = "added_data_files_count")]
        pub added_files_count: Option<i32>,
        /// Number of entries in the manifest that have status EXISTING (0), when null this is assumed to be non-zero
        #[serde(alias = "existing_data_files_count")]
        pub existing_files_count: Option<i32>,
        /// Number of entries in the manifest that have status DELETED (2), when null this is assumed to be non-zero
        #[serde(alias = "deleted_data_files_count")]
        pub deleted_files_count: Option<i32>,
        /// Number of rows in all of files in the manifest that have status ADDED, when null this is assumed to be non-zero
        pub added_rows_count: Option<i64>,
        /// Number of rows in all of files in the manifest that have status EXISTING, when null this is assumed to be non-zero
        pub existing_rows_count: Option<i64>,
        /// Number of rows in all of files in the manifest that have status DELETED, when null this is assumed to be non-zero
        pub deleted_rows_count: Option<i64>,
        /// A list of field summaries for each partition field in the spec. Each field in the list corresponds to a field in the manifest file’s partition spec.
        pub partitions: Option<Vec<FieldSummarySerde>>,
        /// Implementation-specific key metadata for encryption
        pub key_metadata: Option<ByteBuf>,
    }

    impl From<ManifestListEntry> for ManifestListEntryEnum {
        fn from(value: ManifestListEntry) -> Self {
            match &value.format_version {
                FormatVersion::V3 => ManifestListEntryEnum::V3(value.into()),
                FormatVersion::V2 => ManifestListEntryEnum::V2(value.into()),
                FormatVersion::V1 => ManifestListEntryEnum::V1(value.into()),
            }
        }
    }

    impl From<ManifestListEntry> for ManifestListEntryV3 {
        fn from(value: ManifestListEntry) -> Self {
            ManifestListEntryV3 {
                manifest_path: value.manifest_path,
                manifest_length: value.manifest_length,
                partition_spec_id: value.partition_spec_id,
                content: value.content,
                sequence_number: value.sequence_number,
                min_sequence_number: value.min_sequence_number,
                added_snapshot_id: value.added_snapshot_id,
                added_files_count: value.added_files_count.unwrap(),
                existing_files_count: value.existing_files_count.unwrap(),
                deleted_files_count: value.deleted_files_count.unwrap(),
                added_rows_count: value.added_rows_count.unwrap(),
                existing_rows_count: value.existing_rows_count.unwrap(),
                deleted_rows_count: value.deleted_rows_count.unwrap(),
                partitions: value
                    .partitions
                    .map(|v| v.into_iter().map(Into::into).collect()),
                key_metadata: value.key_metadata,
                first_row_id: value.first_row_id,
            }
        }
    }

    impl From<ManifestListEntry> for ManifestListEntryV1 {
        fn from(value: ManifestListEntry) -> Self {
            ManifestListEntryV1 {
                manifest_path: value.manifest_path,
                manifest_length: value.manifest_length,
                partition_spec_id: value.partition_spec_id,
                added_snapshot_id: value.added_snapshot_id,
                added_files_count: value.added_files_count,
                existing_files_count: value.existing_files_count,
                deleted_files_count: value.deleted_files_count,
                added_rows_count: value.added_rows_count,
                existing_rows_count: value.existing_rows_count,
                deleted_rows_count: value.deleted_rows_count,
                partitions: value
                    .partitions
                    .map(|v| v.into_iter().map(Into::into).collect()),
                key_metadata: value.key_metadata,
            }
        }
    }

    impl From<ManifestListEntry> for ManifestListEntryV2 {
        fn from(value: ManifestListEntry) -> Self {
            ManifestListEntryV2 {
                manifest_path: value.manifest_path,
                manifest_length: value.manifest_length,
                partition_spec_id: value.partition_spec_id,
                content: value.content,
                sequence_number: value.sequence_number,
                min_sequence_number: value.min_sequence_number,
                added_snapshot_id: value.added_snapshot_id,
                added_files_count: value.added_files_count.unwrap(),
                existing_files_count: value.existing_files_count.unwrap(),
                deleted_files_count: value.deleted_files_count.unwrap(),
                added_rows_count: value.added_rows_count.unwrap(),
                existing_rows_count: value.existing_rows_count.unwrap(),
                deleted_rows_count: value.deleted_rows_count.unwrap(),
                partitions: value
                    .partitions
                    .map(|v| v.into_iter().map(Into::into).collect()),
                key_metadata: value.key_metadata,
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
    /// DataFile found in Manifest.
    pub struct FieldSummarySerde {
        /// Whether the manifest contains at least one partition with a null value for the field
        pub contains_null: bool,
        /// Whether the manifest contains at least one partition with a NaN value for the field
        pub contains_nan: Option<bool>,
        /// Lower bound for the non-null, non-NaN values in the partition field, or null if all values are null or NaN.
        /// If -0.0 is a value of the partition field, the lower_bound must not be +0.0
        pub lower_bound: Option<ByteBuf>,
        /// Upper bound for the non-null, non-NaN values in the partition field, or null if all values are null or NaN .
        /// If +0.0 is a value of the partition field, the upper_bound must not be -0.0.
        pub upper_bound: Option<ByteBuf>,
    }

    impl From<FieldSummary> for FieldSummarySerde {
        fn from(value: FieldSummary) -> Self {
            FieldSummarySerde {
                contains_null: value.contains_null,
                contains_nan: value.contains_nan,
                lower_bound: value.lower_bound.map(Into::into),
                upper_bound: value.upper_bound.map(Into::into),
            }
        }
    }
}

impl ManifestListEntry {
    /// Decodes one manifest-list entry from its Avro value for a known writer format version.
    pub fn from_avro_value(
        value: Result<AvroValue, apache_avro::Error>,
        format_version: FormatVersion,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        ManifestListEntryDecoder::new(table_metadata).decode(value, format_version)
    }

    /// Decodes an entry using the schema of the snapshot being scanned.
    pub fn from_avro_value_with_schema(
        value: Result<AvroValue, apache_avro::Error>,
        format_version: FormatVersion,
        table_metadata: &TableMetadata,
        schema: &Schema,
    ) -> Result<ManifestListEntry, Error> {
        ManifestListEntryDecoder::with_schema(table_metadata, schema).decode(value, format_version)
    }

    pub fn try_from_enum(
        entry: ManifestListEntryEnum,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        match entry {
            ManifestListEntryEnum::V3(entry) => {
                ManifestListEntry::try_from_v3(entry, table_metadata)
            }
            ManifestListEntryEnum::V2(entry) => {
                ManifestListEntry::try_from_v2(entry, table_metadata)
            }
            ManifestListEntryEnum::V1(entry) => {
                ManifestListEntry::try_from_v1(entry, table_metadata)
            }
        }
    }

    pub fn try_from_v3(
        entry: _serde::ManifestListEntryV3,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        let (schema, _) = historical_entry_schema(
            table_metadata,
            entry.partition_spec_id,
            entry.added_snapshot_id,
        )?;
        Self::try_from_v3_with_schema(entry, table_metadata, schema)
    }

    fn try_from_v3_with_schema(
        entry: _serde::ManifestListEntryV3,
        table_metadata: &TableMetadata,
        schema: &Schema,
    ) -> Result<ManifestListEntry, Error> {
        let types = partition_field_types(table_metadata, entry.partition_spec_id, schema)?;
        Self::try_from_v3_with_types(entry, &types, true)
    }

    fn try_from_v3_with_types(
        entry: _serde::ManifestListEntryV3,
        types: &[Option<Type>],
        promote: bool,
    ) -> Result<ManifestListEntry, Error> {
        Ok(ManifestListEntry {
            format_version: FormatVersion::V3,
            manifest_path: entry.manifest_path,
            manifest_length: entry.manifest_length,
            partition_spec_id: entry.partition_spec_id,
            content: entry.content,
            sequence_number: entry.sequence_number,
            min_sequence_number: entry.min_sequence_number,
            added_snapshot_id: entry.added_snapshot_id,
            added_files_count: Some(entry.added_files_count),
            existing_files_count: Some(entry.existing_files_count),
            deleted_files_count: Some(entry.deleted_files_count),
            added_rows_count: Some(entry.added_rows_count),
            existing_rows_count: Some(entry.existing_rows_count),
            deleted_rows_count: Some(entry.deleted_rows_count),
            partitions: decode_partition_summaries(entry.partitions, types, promote)?,
            key_metadata: entry.key_metadata,
            first_row_id: entry.first_row_id,
        })
    }

    pub fn try_from_v2(
        entry: _serde::ManifestListEntryV2,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        let (schema, _) = historical_entry_schema(
            table_metadata,
            entry.partition_spec_id,
            entry.added_snapshot_id,
        )?;
        Self::try_from_v2_with_schema(entry, table_metadata, schema)
    }

    fn try_from_v2_with_schema(
        entry: _serde::ManifestListEntryV2,
        table_metadata: &TableMetadata,
        schema: &Schema,
    ) -> Result<ManifestListEntry, Error> {
        let types = partition_field_types(table_metadata, entry.partition_spec_id, schema)?;
        Self::try_from_v2_with_types(entry, &types, true)
    }

    fn try_from_v2_with_types(
        entry: _serde::ManifestListEntryV2,
        types: &[Option<Type>],
        promote: bool,
    ) -> Result<ManifestListEntry, Error> {
        Ok(ManifestListEntry {
            format_version: FormatVersion::V2,
            manifest_path: entry.manifest_path,
            manifest_length: entry.manifest_length,
            partition_spec_id: entry.partition_spec_id,
            content: entry.content,
            sequence_number: entry.sequence_number,
            min_sequence_number: entry.min_sequence_number,
            added_snapshot_id: entry.added_snapshot_id,
            added_files_count: Some(entry.added_files_count),
            existing_files_count: Some(entry.existing_files_count),
            deleted_files_count: Some(entry.deleted_files_count),
            added_rows_count: Some(entry.added_rows_count),
            existing_rows_count: Some(entry.existing_rows_count),
            deleted_rows_count: Some(entry.deleted_rows_count),
            partitions: decode_partition_summaries(entry.partitions, types, promote)?,
            key_metadata: entry.key_metadata,
            first_row_id: None,
        })
    }

    pub fn try_from_v1(
        entry: _serde::ManifestListEntryV1,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        let (schema, _) = historical_entry_schema(
            table_metadata,
            entry.partition_spec_id,
            entry.added_snapshot_id,
        )?;
        Self::try_from_v1_with_schema(entry, table_metadata, schema)
    }

    fn try_from_v1_with_schema(
        entry: _serde::ManifestListEntryV1,
        table_metadata: &TableMetadata,
        schema: &Schema,
    ) -> Result<ManifestListEntry, Error> {
        let types = partition_field_types(table_metadata, entry.partition_spec_id, schema)?;
        Self::try_from_v1_with_types(entry, &types, true)
    }

    fn try_from_v1_with_types(
        entry: _serde::ManifestListEntryV1,
        types: &[Option<Type>],
        promote: bool,
    ) -> Result<ManifestListEntry, Error> {
        Ok(ManifestListEntry {
            format_version: FormatVersion::V1,
            manifest_path: entry.manifest_path,
            manifest_length: entry.manifest_length,
            partition_spec_id: entry.partition_spec_id,
            content: Content::Data,
            sequence_number: 0,
            min_sequence_number: 0,
            added_snapshot_id: entry.added_snapshot_id,
            added_files_count: entry.added_files_count,
            existing_files_count: entry.existing_files_count,
            deleted_files_count: entry.deleted_files_count,
            added_rows_count: entry.added_rows_count,
            existing_rows_count: entry.existing_rows_count,
            deleted_rows_count: entry.deleted_rows_count,
            partitions: decode_partition_summaries(entry.partitions, types, promote)?,
            key_metadata: entry.key_metadata,
            first_row_id: None,
        })
    }
}

/// Resolves each partition field's result type against the scan schema. A field whose source
/// column is absent (e.g. dropped, but still referenced by a historical spec) is left untyped so
/// its bounds are skipped — you cannot prune on a column that no longer exists.
fn partition_field_types(
    table_metadata: &TableMetadata,
    partition_spec_id: i32,
    schema: &Schema,
) -> Result<Vec<Option<Type>>, Error> {
    let partition_spec = table_metadata
        .partition_specs
        .get(&partition_spec_id)
        .ok_or_else(|| Error::NotFound(format!("Partition spec with id {partition_spec_id}")))?;
    Ok(partition_spec.data_types_lenient(schema.fields()))
}

struct CachedPartitionTypes {
    fields: Vec<Option<Type>>,
    complete: bool,
}

/// Stateful decoder that reuses partition field types for all entries of a manifest list.
pub struct ManifestListEntryDecoder<'a> {
    table_metadata: &'a TableMetadata,
    schema: Option<&'a Schema>,
    partition_types: HashMap<(i32, i32), CachedPartitionTypes>,
}

impl<'a> ManifestListEntryDecoder<'a> {
    pub fn new(table_metadata: &'a TableMetadata) -> Self {
        Self {
            table_metadata,
            schema: None,
            partition_types: HashMap::new(),
        }
    }

    pub fn with_schema(table_metadata: &'a TableMetadata, schema: &'a Schema) -> Self {
        Self {
            table_metadata,
            schema: Some(schema),
            partition_types: HashMap::new(),
        }
    }

    fn partition_types(
        &mut self,
        spec_id: i32,
        snapshot_id: i64,
    ) -> Result<(&[Option<Type>], bool), Error> {
        let preferred_schema_id = self.schema.map_or_else(
            || {
                self.table_metadata
                    .snapshots
                    .get(&snapshot_id)
                    .and_then(|snapshot| *snapshot.schema_id())
                    .unwrap_or(self.table_metadata.current_schema_id)
            },
            |schema| *schema.schema_id(),
        );
        let key = (spec_id, preferred_schema_id);
        if !self.partition_types.contains_key(&key) {
            let (schema, has_partition_types) = if let Some(schema) = self.schema {
                (schema, true)
            } else {
                historical_entry_schema(self.table_metadata, spec_id, snapshot_id)?
            };
            self.partition_types.insert(
                key,
                CachedPartitionTypes {
                    fields: partition_field_types(self.table_metadata, spec_id, schema)?,
                    complete: has_partition_types,
                },
            );
        }
        let cached = &self.partition_types[&key];
        Ok((&cached.fields, cached.complete))
    }

    pub fn decode(
        &mut self,
        value: Result<AvroValue, apache_avro::Error>,
        format_version: FormatVersion,
    ) -> Result<ManifestListEntry, Error> {
        let entry = value?;
        match format_version {
            FormatVersion::V1 => {
                let entry = apache_avro::from_value::<ManifestListEntryV1>(&entry)?;
                let promote = self.schema.is_some();
                let (types, has_partition_types) =
                    self.partition_types(entry.partition_spec_id, entry.added_snapshot_id)?;
                let mut decoded = ManifestListEntry::try_from_v1_with_types(entry, types, promote)?;
                if !has_partition_types {
                    decoded.partitions = None;
                }
                Ok(decoded)
            }
            FormatVersion::V2 => {
                let entry = apache_avro::from_value::<ManifestListEntryV2>(&entry)?;
                let promote = self.schema.is_some();
                let (types, has_partition_types) =
                    self.partition_types(entry.partition_spec_id, entry.added_snapshot_id)?;
                let mut decoded = ManifestListEntry::try_from_v2_with_types(entry, types, promote)?;
                if !has_partition_types {
                    decoded.partitions = None;
                }
                Ok(decoded)
            }
            FormatVersion::V3 => {
                let entry = apache_avro::from_value::<ManifestListEntryV3>(&entry)?;
                let promote = self.schema.is_some();
                let (types, has_partition_types) =
                    self.partition_types(entry.partition_spec_id, entry.added_snapshot_id)?;
                let mut decoded = ManifestListEntry::try_from_v3_with_types(entry, types, promote)?;
                if !has_partition_types {
                    decoded.partitions = None;
                }
                Ok(decoded)
            }
        }
    }
}

/// Decodes partition summaries against the selected schema, promoting bounds only for scans.
fn decode_partition_summaries(
    partitions: Option<Vec<FieldSummarySerde>>,
    types: &[Option<Type>],
    promote: bool,
) -> Result<Option<Vec<FieldSummary>>, Error> {
    let Some(summaries) = partitions else {
        return Ok(None);
    };
    if summaries.len() != types.len() {
        return Ok(None);
    }
    summaries
        .into_iter()
        .zip(types.iter())
        .map(|(summary, data_type)| {
            FieldSummary::from_serde_with_mode(summary, data_type.as_ref(), promote)
        })
        .collect::<Result<Vec<_>, Error>>()
        .map(Some)
}

impl FieldSummary {
    #[cfg(test)]
    fn from_serde(value: FieldSummarySerde, data_type: Option<&Type>) -> Result<Self, Error> {
        Self::from_serde_with_mode(value, data_type, true)
    }

    fn from_serde_with_mode(
        value: FieldSummarySerde,
        data_type: Option<&Type>,
        promote: bool,
    ) -> Result<Self, Error> {
        let decode = |bytes: Option<ByteBuf>| -> Result<Option<Value>, Error> {
            match (bytes, data_type) {
                (Some(bytes), Some(data_type)) if promote => {
                    super::manifest::decode_bound(&bytes, data_type).map(Some)
                }
                (Some(bytes), Some(data_type)) => {
                    super::manifest::decode_bound_for_copy(&bytes, data_type).map(Some)
                }
                _ => Ok(None),
            }
        };
        Ok(FieldSummary {
            contains_null: value.contains_null,
            contains_nan: value.contains_nan,
            lower_bound: decode(value.lower_bound)?,
            upper_bound: decode(value.upper_bound)?,
        })
    }
}

pub fn manifest_list_schema_v1() -> &'static AvroSchema {
    static MANIFEST_LIST_SCHEMA_V1: OnceLock<AvroSchema> = OnceLock::new();
    MANIFEST_LIST_SCHEMA_V1.get_or_init(|| {
        AvroSchema::parse_str(
            r#"
        {
            "type": "record",
            "name": "manifest_file",
            "fields": [
                {
                    "name": "manifest_path",
                    "type": "string",
                    "field-id": 500
                },
                {
                    "name": "manifest_length",
                    "type": "long",
                    "field-id": 501
                },
                {
                    "name": "partition_spec_id",
                    "type": "int",
                    "field-id": 502
                },
                {
                    "name": "added_snapshot_id",
                    "type": "long",
                    "field-id": 503
                },
                {
                    "name": "added_files_count",
                    "type": [
                        "null",
                        "int"
                    ],
                    "default": null,
                    "field-id": 504
                },
                {
                    "name": "existing_files_count",
                    "type": [
                        "null",
                        "int"
                    ],
                    "default": null,
                    "field-id": 505
                },
                {
                    "name": "deleted_files_count",
                    "type": [
                        "null",
                        "int"
                    ],
                    "default": null,
                    "field-id": 506
                },
                {
                    "name": "added_rows_count",
                    "type": [
                        "null",
                        "long"
                    ],
                    "default": null,
                    "field-id": 512
                },
                {
                    "name": "existing_rows_count",
                    "type": [
                        "null",
                        "long"
                    ],
                    "default": null,
                    "field-id": 513
                },
                {
                    "name": "deleted_rows_count",
                    "type": [
                        "null",
                        "long"
                    ],
                    "default": null,
                    "field-id": 514
                },
                {
                    "name": "partitions",
                    "type": [
                        "null",
                        {
                            "type": "array",
                            "items": {
                                "type": "record",
                                "name": "r508",
                                "fields": [
                                    {
                                        "name": "contains_null",
                                        "type": "boolean",
                                        "field-id": 509
                                    },
                                    {
                                        "name": "contains_nan",
                                        "type": [
                                            "null",
                                            "boolean"
                                        ],
                                        "field-id": 518
                                    },
                                    {
                                        "name": "lower_bound",
                                        "type": [
                                            "null",
                                            "bytes"
                                        ],
                                        "field-id": 510
                                    },
                                    {
                                        "name": "upper_bound",
                                        "type": [
                                            "null",
                                            "bytes"
                                        ],
                                        "field-id": 511
                                    }
                                ]
                            },
                            "element-id": 508
                        }
                    ],
                    "default": null,
                    "field-id": 507
                },
                {
                    "name": "key_metadata",
                    "type": [
                        "null",
                        "bytes"
                    ],
                    "default": null,
                    "field-id": 519
                }
            ]
        }
        "#,
        )
        .unwrap()
    })
}
pub fn manifest_list_schema_v2() -> &'static AvroSchema {
    static MANIFEST_LIST_SCHEMA_V2: OnceLock<AvroSchema> = OnceLock::new();
    MANIFEST_LIST_SCHEMA_V2.get_or_init(|| {
        AvroSchema::parse_str(
            r#"
        {
            "type": "record",
            "name": "manifest_file",
            "fields": [
                {
                    "name": "manifest_path",
                    "type": "string",
                    "field-id": 500
                },
                {
                    "name": "manifest_length",
                    "type": "long",
                    "field-id": 501
                },
                {
                    "name": "partition_spec_id",
                    "type": "int",
                    "field-id": 502
                },
                {
                    "name": "content",
                    "type": "int",
                    "field-id": 517
                },
                {
                    "name": "sequence_number",
                    "type": "long",
                    "field-id": 515
                },
                {
                    "name": "min_sequence_number",
                    "type": "long",
                    "field-id": 516
                },
                {
                    "name": "added_snapshot_id",
                    "type": "long",
                    "field-id": 503
                },
                {
                    "name": "added_files_count",
                    "type": "int",
                    "field-id": 504
                },
                {
                    "name": "existing_files_count",
                    "type": "int",
                    "field-id": 505
                },
                {
                    "name": "deleted_files_count",
                    "type": "int",
                    "field-id": 506
                },
                {
                    "name": "added_rows_count",
                    "type": "long",
                    "field-id": 512
                },
                {
                    "name": "existing_rows_count",
                    "type": "long",
                    "field-id": 513
                },
                {
                    "name": "deleted_rows_count",
                    "type": "long",
                    "field-id": 514
                },
                {
                    "name": "partitions",
                    "type": [
                        "null",
                        {
                            "type": "array",
                            "items": {
                                "type": "record",
                                "name": "r508",
                                "fields": [
                                    {
                                        "name": "contains_null",
                                        "type": "boolean",
                                        "field-id": 509
                                    },
                                    {
                                        "name": "contains_nan",
                                        "type": [
                                            "null",
                                            "boolean"
                                        ],
                                        "field-id": 518
                                    },
                                    {
                                        "name": "lower_bound",
                                        "type": [
                                            "null",
                                            "bytes"
                                        ],
                                        "field-id": 510
                                    },
                                    {
                                        "name": "upper_bound",
                                        "type": [
                                            "null",
                                            "bytes"
                                        ],
                                        "field-id": 511
                                    }
                                ]
                            },
                            "element-id": 508
                        }
                    ],
                    "default": null,
                    "field-id": 507
                },
                {
                    "name": "key_metadata",
                    "type": [
                        "null",
                        "bytes"
                    ],
                    "default": null,
                    "field-id": 519
                }
            ]
        }
        "#,
        )
        .unwrap()
    })
}

/// Manifest list Avro schema for V3 tables.
pub fn manifest_list_schema_v3() -> &'static AvroSchema {
    static MANIFEST_LIST_SCHEMA_V3: OnceLock<AvroSchema> = OnceLock::new();
    MANIFEST_LIST_SCHEMA_V3.get_or_init(|| {
        let mut schema = serde_json::to_value(manifest_list_schema_v2()).unwrap();
        schema["fields"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "first_row_id",
                "type": ["null", "long"],
                "default": null,
                "field-id": 520
            }));
        AvroSchema::parse(&schema).unwrap()
    })
}

/// Convert an avro value result to a manifest list version according to the provided format version
pub fn avro_value_to_manifest_list_entry(
    value: Result<AvroValue, apache_avro::Error>,
    table_metadata: &TableMetadata,
) -> Result<ManifestListEntry, Error> {
    avro_value_to_manifest_list_entry_for_format_version(
        value,
        table_metadata,
        table_metadata.format_version,
    )
}

/// Converts an Avro value using the format version of the manifest list writer.
///
/// This is required when a table has been upgraded: historical manifest lists retain their
/// original writer schema and must not be decoded as the table's current format version.
pub fn avro_value_to_manifest_list_entry_for_format_version(
    value: Result<AvroValue, apache_avro::Error>,
    table_metadata: &TableMetadata,
    format_version: FormatVersion,
) -> Result<ManifestListEntry, Error> {
    let value = value?;
    let entry = match format_version {
        FormatVersion::V1 => ManifestListEntryEnum::V1(apache_avro::from_value(&value)?),
        FormatVersion::V2 => ManifestListEntryEnum::V2(apache_avro::from_value(&value)?),
        FormatVersion::V3 => ManifestListEntryEnum::V3(apache_avro::from_value(&value)?),
    };
    let (spec_id, snapshot_id) = match &entry {
        ManifestListEntryEnum::V1(entry) => (entry.partition_spec_id, entry.added_snapshot_id),
        ManifestListEntryEnum::V2(entry) => (entry.partition_spec_id, entry.added_snapshot_id),
        ManifestListEntryEnum::V3(entry) => (entry.partition_spec_id, entry.added_snapshot_id),
    };
    let (schema, has_partition_types) =
        historical_entry_schema(table_metadata, spec_id, snapshot_id)?;
    let mut decoded = match entry {
        ManifestListEntryEnum::V1(entry) => {
            ManifestListEntry::try_from_v1_with_schema(entry, table_metadata, schema)?
        }
        ManifestListEntryEnum::V2(entry) => {
            ManifestListEntry::try_from_v2_with_schema(entry, table_metadata, schema)?
        }
        ManifestListEntryEnum::V3(entry) => {
            ManifestListEntry::try_from_v3_with_schema(entry, table_metadata, schema)?
        }
    };
    if !has_partition_types {
        decoded.partitions = None;
    }
    Ok(decoded)
}

fn historical_entry_schema(
    metadata: &TableMetadata,
    spec_id: i32,
    snapshot_id: i64,
) -> Result<(&Schema, bool), Error> {
    let spec = metadata
        .partition_specs
        .get(&spec_id)
        .ok_or_else(|| Error::NotFound(format!("Partition spec with id {spec_id}")))?;
    let snapshot_schema = metadata
        .snapshots
        .get(&snapshot_id)
        .and_then(|snapshot| *snapshot.schema_id())
        .and_then(|schema_id| metadata.schemas.get(&schema_id));
    if let Some(schema) = snapshot_schema {
        if spec.data_types(schema.fields()).is_ok() {
            return Ok((schema, true));
        }
    }
    let current = metadata.current_schema()?;
    if spec.data_types(current.fields()).is_ok() {
        return Ok((current, true));
    }
    let historical = metadata
        .schemas
        .iter()
        .filter(|(_, schema)| spec.data_types(schema.fields()).is_ok())
        .max_by_key(|(schema_id, _)| *schema_id)
        .map(|(_, schema)| schema);
    Ok(historical.map_or((current, false), |schema| (schema, true)))
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use ordered_float::OrderedFloat;

    use super::*;

    use crate::spec::{
        decimal::decimal_from_i128_with_scale,
        partition::{PartitionField, PartitionSpec, Transform},
        schema::Schema,
        snapshot::{SnapshotBuilder, Summary},
        table_metadata::TableMetadataBuilder,
        types::{PrimitiveType, StructField, StructType},
    };

    #[test]
    fn partition_summaries_decode_pre_promotion_widths() {
        let summary = _serde::FieldSummarySerde {
            contains_null: false,
            contains_nan: None,
            lower_bound: Some(ByteBuf::from((-42_i32).to_le_bytes())),
            upper_bound: Some(ByteBuf::from(7_i32.to_le_bytes())),
        };
        let decoded =
            FieldSummary::from_serde(summary, Some(&Type::Primitive(PrimitiveType::Long))).unwrap();
        assert_eq!(decoded.lower_bound, Some(Value::LongInt(-42)));
        assert_eq!(decoded.upper_bound, Some(Value::LongInt(7)));
    }

    #[test]
    fn partition_bound_promotes_float_to_double() {
        // A bound written as a 4-byte float before a float->double promotion decodes against the
        // current `double` type via the spec's width-inference rule.
        let decoded = crate::spec::manifest::decode_bound(
            &1.5_f32.to_le_bytes(),
            &Type::Primitive(PrimitiveType::Double),
        )
        .unwrap();
        assert_eq!(decoded, Value::Double(OrderedFloat(1.5_f64)));
    }

    #[test]
    fn manifest_list_entry_uses_its_own_partition_spec() {
        let mut metadata = TableMetadataBuilder::default()
            .location("/")
            .current_schema_id(1)
            .schemas(HashMap::from([(
                1,
                Schema::from_struct_type(
                    StructType::new(vec![
                        StructField::new(1, "a", false, Type::Primitive(PrimitiveType::Int), None),
                        StructField::new(2, "b", false, Type::Primitive(PrimitiveType::Long), None),
                    ]),
                    1,
                    None,
                ),
            )]))
            .default_spec_id(1)
            .partition_specs(HashMap::from([
                (
                    0,
                    PartitionSpec::builder()
                        .with_partition_field(PartitionField::new(
                            1,
                            1000,
                            "a",
                            Transform::Identity,
                        ))
                        .build()
                        .unwrap(),
                ),
                (
                    1,
                    PartitionSpec::builder()
                        .with_partition_field(PartitionField::new(
                            2,
                            1001,
                            "b",
                            Transform::Identity,
                        ))
                        .build()
                        .unwrap(),
                ),
            ]))
            .build()
            .unwrap();
        let old_entry = _serde::ManifestListEntryV2 {
            manifest_path: "/old.avro".into(),
            manifest_length: 1,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 1,
            min_sequence_number: 1,
            added_snapshot_id: 1,
            added_files_count: 1,
            existing_files_count: 0,
            deleted_files_count: 0,
            added_rows_count: 1,
            existing_rows_count: 0,
            deleted_rows_count: 0,
            partitions: Some(vec![_serde::FieldSummarySerde {
                contains_null: false,
                contains_nan: None,
                lower_bound: Some(ByteBuf::from(7_i32.to_le_bytes())),
                upper_bound: None,
            }]),
            key_metadata: None,
        };
        let decoded = ManifestListEntry::try_from_v2(old_entry.clone(), &metadata).unwrap();
        assert_eq!(
            decoded.partitions.unwrap()[0].lower_bound,
            Some(Value::Int(7))
        );

        let mut unknown_spec = old_entry.clone();
        unknown_spec.partition_spec_id = 999;
        unknown_spec.partitions = None;
        assert!(matches!(
            ManifestListEntry::try_from_v2(unknown_spec, &metadata),
            Err(Error::NotFound(_))
        ));

        let mut mismatched = old_entry.clone();
        let summaries = mismatched.partitions.as_mut().unwrap();
        summaries.push(summaries[0].clone());
        assert!(ManifestListEntry::try_from_v2(mismatched, &metadata)
            .unwrap()
            .partitions
            .is_none());

        metadata.schemas.insert(
            1,
            Schema::from_struct_type(
                StructType::new(vec![StructField::new(
                    2,
                    "b",
                    false,
                    Type::Primitive(PrimitiveType::Long),
                    None,
                )]),
                1,
                None,
            ),
        );
        let decoded = ManifestListEntry::try_from_v2(old_entry, &metadata).unwrap();
        let summary = &decoded.partitions.unwrap()[0];
        assert!(!summary.contains_null);
        assert_eq!(summary.lower_bound, None);
    }

    #[test]
    fn field_summary_without_resolvable_type_skips_bounds() {
        // A partition field whose source column was dropped resolves to no type, so its bounds are
        // skipped while the null/nan flags are preserved.
        let serde = FieldSummarySerde {
            contains_null: true,
            contains_nan: Some(false),
            lower_bound: Some(ByteBuf::from(1_i32.to_le_bytes().to_vec())),
            upper_bound: Some(ByteBuf::from(9_i32.to_le_bytes().to_vec())),
        };
        let summary = FieldSummary::from_serde(serde, None).unwrap();
        assert_eq!(
            summary,
            FieldSummary {
                contains_null: true,
                contains_nan: Some(false),
                lower_bound: None,
                upper_bound: None,
            }
        );
    }

    #[test]
    #[ignore = "manual manifest-list decoder throughput comparison"]
    fn benchmark_manifest_list_decoder_cache() {
        use std::{hint::black_box, time::Instant};

        let metadata = TableMetadataBuilder::default()
            .location("/")
            .current_schema_id(1)
            .schemas(HashMap::from([(
                1,
                Schema::from_struct_type(
                    StructType::new(vec![StructField::new(
                        1,
                        "id",
                        false,
                        Type::Primitive(PrimitiveType::Int),
                        None,
                    )]),
                    1,
                    None,
                ),
            )]))
            .default_spec_id(0)
            .partition_specs(HashMap::from([(
                0,
                PartitionSpec::builder()
                    .with_partition_field(PartitionField::new(1, 1000, "id", Transform::Identity))
                    .build()
                    .unwrap(),
            )]))
            .build()
            .unwrap();
        let value = apache_avro::to_value(_serde::ManifestListEntryV1 {
            manifest_path: "manifest.avro".into(),
            manifest_length: 1,
            partition_spec_id: 0,
            added_snapshot_id: 1,
            added_files_count: Some(1),
            existing_files_count: Some(0),
            deleted_files_count: Some(0),
            added_rows_count: Some(1),
            existing_rows_count: Some(0),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummarySerde {
                contains_null: false,
                contains_nan: None,
                lower_bound: Some(ByteBuf::from(1_i32.to_le_bytes())),
                upper_bound: Some(ByteBuf::from(9_i32.to_le_bytes())),
            }]),
            key_metadata: None,
        })
        .unwrap();
        let schema = metadata.current_schema().unwrap();
        const ITERATIONS: usize = 20_000;

        let mut decoder = ManifestListEntryDecoder::with_schema(&metadata, schema);
        let cached_start = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                decoder
                    .decode(Ok(value.clone()), FormatVersion::V1)
                    .unwrap(),
            );
        }
        let cached = cached_start.elapsed();

        let uncached_start = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                ManifestListEntry::from_avro_value_with_schema(
                    Ok(value.clone()),
                    FormatVersion::V1,
                    &metadata,
                    schema,
                )
                .unwrap(),
            );
        }
        let uncached = uncached_start.elapsed();
        eprintln!(
            "manifest-list decode {ITERATIONS} entries: cached={cached:?}, uncached={uncached:?}"
        );
    }

    #[test]
    pub fn test_manifest_list_v2() {
        let table_metadata = TableMetadataBuilder::default()
            .location("/")
            .current_schema_id(1)
            .schemas(HashMap::from_iter(vec![(
                1,
                Schema::builder()
                    .with_schema_id(1)
                    .with_struct_field(StructField {
                        id: 0,
                        name: "date".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Date),
                        doc: None,
                        initial_default: None,
                        write_default: None,
                    })
                    .build()
                    .unwrap(),
            )]))
            .default_spec_id(0)
            .partition_specs(HashMap::from_iter(vec![(
                0,
                PartitionSpec::builder()
                    .with_partition_field(PartitionField::new(0, 1000, "day", Transform::Day))
                    .build()
                    .unwrap(),
            )]))
            .build()
            .unwrap();

        let manifest_file = ManifestListEntry {
            format_version: FormatVersion::V2,
            manifest_path: "".to_string(),
            manifest_length: 1200,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 566,
            min_sequence_number: 0,
            added_snapshot_id: 39487483032,
            added_files_count: Some(1),
            existing_files_count: Some(2),
            deleted_files_count: Some(0),
            added_rows_count: Some(1000),
            existing_rows_count: Some(8000),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: true,
                contains_nan: Some(false),
                lower_bound: Some(Value::Int(1234)),
                upper_bound: Some(Value::Int(76890)),
            }]),
            key_metadata: None,
            first_row_id: None,
        };

        let schema = manifest_list_schema_v2();

        let mut writer = apache_avro::Writer::new(schema, Vec::new());

        writer.append_ser(manifest_file.clone()).unwrap();

        let encoded = writer.into_inner().unwrap();

        let reader = apache_avro::Reader::new(&*encoded).unwrap();

        for record in reader {
            let result =
                apache_avro::from_value::<_serde::ManifestListEntryV2>(&record.unwrap()).unwrap();
            assert_eq!(
                manifest_file,
                ManifestListEntry::try_from_v2(result, &table_metadata).unwrap()
            );
        }
    }

    #[test]
    pub fn test_manifest_list_v3() {
        let table_metadata = TableMetadataBuilder::default()
            .format_version(FormatVersion::V3)
            .location("/")
            .current_schema_id(1)
            .schemas(HashMap::from_iter(vec![(
                1,
                Schema::builder()
                    .with_schema_id(1)
                    .with_struct_field(StructField {
                        id: 0,
                        name: "date".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Date),
                        doc: None,
                        initial_default: None,
                        write_default: None,
                    })
                    .build()
                    .unwrap(),
            )]))
            .default_spec_id(0)
            .partition_specs(HashMap::from_iter(vec![(
                0,
                PartitionSpec::builder()
                    .with_partition_field(PartitionField::new(0, 1000, "day", Transform::Day))
                    .build()
                    .unwrap(),
            )]))
            .build()
            .unwrap();

        let manifest_file = ManifestListEntry {
            format_version: FormatVersion::V3,
            manifest_path: "".to_string(),
            manifest_length: 1200,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 566,
            min_sequence_number: 0,
            added_snapshot_id: 39487483032,
            added_files_count: Some(1),
            existing_files_count: Some(2),
            deleted_files_count: Some(0),
            added_rows_count: Some(1000),
            existing_rows_count: Some(8000),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: true,
                contains_nan: Some(false),
                lower_bound: Some(Value::Int(1234)),
                upper_bound: Some(Value::Int(76890)),
            }]),
            key_metadata: None,
            first_row_id: Some(42),
        };

        let schema = manifest_list_schema_v3();

        let mut writer = apache_avro::Writer::new(schema, Vec::new());

        writer.append_ser(manifest_file.clone()).unwrap();

        let encoded = writer.into_inner().unwrap();

        let reader = apache_avro::Reader::new(&*encoded).unwrap();

        for record in reader {
            let result =
                apache_avro::from_value::<_serde::ManifestListEntryV3>(&record.unwrap()).unwrap();
            assert_eq!(
                manifest_file,
                ManifestListEntry::try_from_v3(result, &table_metadata).unwrap()
            );
        }
    }

    #[test]
    pub fn test_manifest_list_v1() {
        let table_metadata = TableMetadataBuilder::default()
            .format_version(FormatVersion::V1)
            .location("/")
            .current_schema_id(1)
            .schemas(HashMap::from_iter(vec![(
                1,
                Schema::builder()
                    .with_schema_id(1)
                    .with_struct_field(StructField {
                        id: 0,
                        name: "date".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Date),
                        doc: None,
                        initial_default: None,
                        write_default: None,
                    })
                    .build()
                    .unwrap(),
            )]))
            .default_spec_id(0)
            .partition_specs(HashMap::from_iter(vec![(
                0,
                PartitionSpec::builder()
                    .with_partition_field(PartitionField::new(0, 1000, "day", Transform::Day))
                    .build()
                    .unwrap(),
            )]))
            .build()
            .unwrap();

        let manifest_file = ManifestListEntry {
            format_version: FormatVersion::V1,
            manifest_path: "".to_string(),
            manifest_length: 1200,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 0,
            min_sequence_number: 0,
            added_snapshot_id: 39487483032,
            added_files_count: Some(1),
            existing_files_count: Some(2),
            deleted_files_count: Some(0),
            added_rows_count: Some(1000),
            existing_rows_count: Some(8000),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: true,
                contains_nan: Some(false),
                lower_bound: Some(Value::Int(1234)),
                upper_bound: Some(Value::Int(76890)),
            }]),
            key_metadata: None,
            first_row_id: None,
        };

        let schema = manifest_list_schema_v1();

        let mut writer = apache_avro::Writer::new(schema, Vec::new());

        writer.append_ser(manifest_file.clone()).unwrap();

        let encoded = writer.into_inner().unwrap();

        let reader = apache_avro::Reader::new(&*encoded).unwrap();

        for record in reader {
            let result =
                apache_avro::from_value::<_serde::ManifestListEntryV1>(&record.unwrap()).unwrap();
            assert_eq!(
                manifest_file,
                ManifestListEntry::try_from_v1(result, &table_metadata).unwrap()
            );
        }
    }

    #[test]
    fn historical_manifest_uses_its_partition_spec_and_snapshot_schema() {
        let historical_schema = Schema::builder()
            .with_schema_id(1)
            .with_struct_field(StructField {
                id: 1,
                name: "historical_id".to_string(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Int),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let current_schema = Schema::builder()
            .with_schema_id(2)
            .with_struct_field(StructField {
                id: 2,
                name: "current_value".to_string(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::String),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        let historical_snapshot = SnapshotBuilder::default()
            .with_snapshot_id(7)
            .with_sequence_number(1)
            .with_timestamp_ms(1)
            .with_manifest_list("historical-list.avro".to_string())
            .with_summary(Summary::default())
            .with_schema_id(1)
            .build()
            .unwrap();
        let historical_snapshot_without_schema = SnapshotBuilder::default()
            .with_snapshot_id(7)
            .with_sequence_number(1)
            .with_timestamp_ms(1)
            .with_manifest_list("historical-list.avro".to_string())
            .with_summary(Summary::default())
            .build()
            .unwrap();
        let table_metadata = |snapshots| {
            TableMetadataBuilder::default()
                .format_version(FormatVersion::V3)
                .location("/")
                .current_schema_id(2)
                .schemas(HashMap::from([
                    (1, historical_schema.clone()),
                    (2, current_schema.clone()),
                ]))
                .default_spec_id(1)
                .partition_specs(HashMap::from([
                    (
                        0,
                        PartitionSpec::builder()
                            .with_spec_id(0)
                            .with_partition_field(PartitionField::new(
                                1,
                                1000,
                                "historical_id",
                                Transform::Identity,
                            ))
                            .build()
                            .unwrap(),
                    ),
                    (
                        1,
                        PartitionSpec::builder()
                            .with_spec_id(1)
                            .with_partition_field(PartitionField::new(
                                2,
                                1001,
                                "current_value",
                                Transform::Identity,
                            ))
                            .build()
                            .unwrap(),
                    ),
                ]))
                .snapshots(snapshots)
                .build()
                .unwrap()
        };
        let historical_entry = ManifestListEntry {
            format_version: FormatVersion::V1,
            manifest_path: "historical-manifest.avro".to_string(),
            manifest_length: 1,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 0,
            min_sequence_number: 0,
            added_snapshot_id: 7,
            added_files_count: Some(1),
            existing_files_count: Some(0),
            deleted_files_count: Some(0),
            added_rows_count: Some(1),
            existing_rows_count: Some(0),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: false,
                contains_nan: None,
                lower_bound: Some(Value::Int(42)),
                upper_bound: Some(Value::Int(42)),
            }]),
            key_metadata: None,
            first_row_id: None,
        };
        let mut writer = apache_avro::Writer::new(manifest_list_schema_v1(), Vec::new());
        writer.append_ser(historical_entry).unwrap();
        let bytes = writer.into_inner().unwrap();
        for metadata in [
            table_metadata(HashMap::from([(7, historical_snapshot)])),
            table_metadata(HashMap::from([(7, historical_snapshot_without_schema)])),
            table_metadata(HashMap::new()),
        ] {
            let mut decoder = ManifestListEntryDecoder::new(&metadata);
            for _ in 0..2 {
                let record = apache_avro::Reader::new(&bytes[..])
                    .unwrap()
                    .next()
                    .unwrap();
                assert_eq!(
                    decoder
                        .decode(record, FormatVersion::V1)
                        .unwrap()
                        .partitions
                        .unwrap()[0]
                        .lower_bound,
                    Some(Value::Int(42))
                );
            }
            assert_eq!(decoder.partition_types.len(), 1);
            let record = apache_avro::Reader::new(&bytes[..])
                .unwrap()
                .next()
                .unwrap();
            let direct_entry =
                apache_avro::from_value::<ManifestListEntryV1>(record.as_ref().unwrap()).unwrap();
            assert_eq!(
                ManifestListEntry::try_from_v1(direct_entry, &metadata)
                    .unwrap()
                    .partitions
                    .unwrap()[0]
                    .lower_bound,
                Some(Value::Int(42))
            );
            let decoded = avro_value_to_manifest_list_entry_for_format_version(
                record,
                &metadata,
                FormatVersion::V1,
            )
            .unwrap();

            assert_eq!(
                decoded.partitions.unwrap()[0].lower_bound,
                Some(Value::Int(42))
            );
        }

        let mut metadata = table_metadata(HashMap::new());
        metadata.schemas.remove(&1);
        let record = apache_avro::Reader::new(&bytes[..])
            .unwrap()
            .next()
            .unwrap();
        let decoded = avro_value_to_manifest_list_entry_for_format_version(
            record,
            &metadata,
            FormatVersion::V1,
        )
        .unwrap();
        assert!(decoded.partitions.is_none());
    }

    fn decode_historical_bound_after_snapshot_expiration(
        source_type: PrimitiveType,
        target_type: PrimitiveType,
        value: Value,
        retain_source_schema: bool,
    ) -> Value {
        let schema = |schema_id, primitive_type| {
            Schema::builder()
                .with_schema_id(schema_id)
                .with_struct_field(StructField {
                    id: 1,
                    name: "partition_source".to_string(),
                    required: true,
                    field_type: Type::Primitive(primitive_type),
                    doc: None,
                    initial_default: None,
                    write_default: None,
                })
                .build()
                .unwrap()
        };
        let mut metadata = TableMetadataBuilder::default()
            .format_version(FormatVersion::V3)
            .location("/")
            .current_schema_id(2)
            .schemas(HashMap::from([
                (1, schema(1, source_type)),
                (2, schema(2, target_type)),
            ]))
            .default_spec_id(0)
            .partition_specs(HashMap::from([(
                0,
                PartitionSpec::builder()
                    .with_spec_id(0)
                    .with_partition_field(PartitionField::new(
                        1,
                        1000,
                        "partition_source",
                        Transform::Identity,
                    ))
                    .build()
                    .unwrap(),
            )]))
            .build()
            .unwrap();
        if !retain_source_schema {
            metadata.schemas.remove(&1);
        }
        let entry = ManifestListEntry {
            format_version: FormatVersion::V1,
            manifest_path: "expired-snapshot-manifest.avro".to_string(),
            manifest_length: 1,
            partition_spec_id: 0,
            content: Content::Data,
            sequence_number: 0,
            min_sequence_number: 0,
            added_snapshot_id: 7,
            added_files_count: Some(1),
            existing_files_count: Some(0),
            deleted_files_count: Some(0),
            added_rows_count: Some(1),
            existing_rows_count: Some(0),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: false,
                contains_nan: None,
                lower_bound: Some(value.clone()),
                upper_bound: Some(value),
            }]),
            key_metadata: None,
            first_row_id: None,
        };
        let mut writer = apache_avro::Writer::new(manifest_list_schema_v1(), Vec::new());
        writer.append_ser(entry).unwrap();
        let bytes = writer.into_inner().unwrap();
        let record = apache_avro::Reader::new(&bytes[..])
            .unwrap()
            .next()
            .unwrap();

        avro_value_to_manifest_list_entry_for_format_version(record, &metadata, FormatVersion::V1)
            .unwrap()
            .partitions
            .unwrap()[0]
            .lower_bound
            .clone()
            .unwrap()
    }

    #[test]
    fn expired_snapshot_bounds_follow_iceberg_schema_promotions() {
        assert_eq!(
            decode_historical_bound_after_snapshot_expiration(
                PrimitiveType::Int,
                PrimitiveType::Long,
                Value::Int(42),
                true,
            ),
            Value::LongInt(42)
        );
        assert_eq!(
            decode_historical_bound_after_snapshot_expiration(
                PrimitiveType::Int,
                PrimitiveType::Long,
                Value::Int(42),
                false,
            ),
            Value::LongInt(42)
        );

        let float = Value::try_from_bytes(
            &1.5_f32.to_le_bytes(),
            &Type::Primitive(PrimitiveType::Float),
        )
        .unwrap();
        let expected_double = Value::try_from_bytes(
            &1.5_f64.to_le_bytes(),
            &Type::Primitive(PrimitiveType::Double),
        )
        .unwrap();
        assert_eq!(
            decode_historical_bound_after_snapshot_expiration(
                PrimitiveType::Float,
                PrimitiveType::Double,
                float.clone(),
                true,
            ),
            expected_double
        );
        assert_eq!(
            decode_historical_bound_after_snapshot_expiration(
                PrimitiveType::Float,
                PrimitiveType::Double,
                float,
                false,
            ),
            expected_double
        );

        let decimal = Value::Decimal(decimal_from_i128_with_scale(12_345, 2).unwrap());
        assert_eq!(
            decode_historical_bound_after_snapshot_expiration(
                PrimitiveType::Decimal {
                    precision: 7,
                    scale: 2,
                },
                PrimitiveType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                decimal.clone(),
                true,
            ),
            decimal
        );
    }
}
