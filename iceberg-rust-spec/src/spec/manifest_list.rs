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
    fn preferred_schema_id(table_metadata: &TableMetadata, added_snapshot_id: i64) -> i32 {
        table_metadata
            .snapshots
            .get(&added_snapshot_id)
            .and_then(|snapshot| *snapshot.schema_id())
            .unwrap_or(table_metadata.current_schema_id)
    }

    fn partition_type_candidates(
        table_metadata: &TableMetadata,
        partition_spec_id: i32,
        preferred_schema_id: i32,
    ) -> Result<Vec<Vec<Type>>, Error> {
        let partition_spec = table_metadata
            .partition_specs
            .get(&partition_spec_id)
            .ok_or_else(|| {
                Error::NotFound(format!("Partition spec with id {partition_spec_id}"))
            })?;
        let mut schema_ids = table_metadata.schemas.keys().copied().collect::<Vec<_>>();
        schema_ids.sort_unstable_by(|left, right| right.cmp(left));
        schema_ids.retain(|schema_id| *schema_id != preferred_schema_id);
        schema_ids.insert(0, preferred_schema_id);

        let mut candidates: Option<Vec<Vec<Type>>> = None;
        for schema_id in schema_ids {
            let Some(schema) = table_metadata.schemas.get(&schema_id) else {
                continue;
            };
            let Ok(types) = partition_spec.data_types(schema.fields()) else {
                continue;
            };
            let candidates = candidates.get_or_insert_with(|| vec![Vec::new(); types.len()]);
            for (field_candidates, data_type) in candidates.iter_mut().zip(types) {
                if !field_candidates.contains(&data_type) {
                    field_candidates.push(data_type);
                }
            }
        }

        Ok(candidates.unwrap_or_default())
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
        let preferred_schema_id =
            Self::preferred_schema_id(table_metadata, entry.added_snapshot_id);
        let partition_types = Self::partition_type_candidates(
            table_metadata,
            entry.partition_spec_id,
            preferred_schema_id,
        )?;
        Self::try_from_v3_with_partition_types(entry, &partition_types)
    }

    fn try_from_v3_with_partition_types(
        entry: _serde::ManifestListEntryV3,
        partition_types: &[Vec<Type>],
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
            partitions: entry
                .partitions
                .filter(|partitions| partitions.len() == partition_types.len())
                .map(|v| {
                    v.into_iter()
                        .zip(partition_types.iter())
                        .map(|(x, candidates)| FieldSummary::try_from(x, candidates))
                        .collect::<Result<Vec<_>, Error>>()
                })
                .transpose()?,
            key_metadata: entry.key_metadata,
            first_row_id: entry.first_row_id,
        })
    }

    pub fn try_from_v2(
        entry: _serde::ManifestListEntryV2,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        let preferred_schema_id =
            Self::preferred_schema_id(table_metadata, entry.added_snapshot_id);
        let partition_types = Self::partition_type_candidates(
            table_metadata,
            entry.partition_spec_id,
            preferred_schema_id,
        )?;
        Self::try_from_v2_with_partition_types(entry, &partition_types)
    }

    fn try_from_v2_with_partition_types(
        entry: _serde::ManifestListEntryV2,
        partition_types: &[Vec<Type>],
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
            partitions: entry
                .partitions
                .filter(|partitions| partitions.len() == partition_types.len())
                .map(|v| {
                    v.into_iter()
                        .zip(partition_types.iter())
                        .map(|(x, candidates)| FieldSummary::try_from(x, candidates))
                        .collect::<Result<Vec<_>, Error>>()
                })
                .transpose()?,
            key_metadata: entry.key_metadata,
            first_row_id: None,
        })
    }

    pub fn try_from_v1(
        entry: _serde::ManifestListEntryV1,
        table_metadata: &TableMetadata,
    ) -> Result<ManifestListEntry, Error> {
        let preferred_schema_id =
            Self::preferred_schema_id(table_metadata, entry.added_snapshot_id);
        let partition_types = Self::partition_type_candidates(
            table_metadata,
            entry.partition_spec_id,
            preferred_schema_id,
        )?;
        Self::try_from_v1_with_partition_types(entry, &partition_types)
    }

    fn try_from_v1_with_partition_types(
        entry: _serde::ManifestListEntryV1,
        partition_types: &[Vec<Type>],
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
            partitions: entry
                .partitions
                .filter(|partitions| partitions.len() == partition_types.len())
                .map(|v| {
                    v.into_iter()
                        .zip(partition_types.iter())
                        .map(|(x, candidates)| FieldSummary::try_from(x, candidates))
                        .collect::<Result<Vec<_>, Error>>()
                })
                .transpose()?,
            key_metadata: entry.key_metadata,
            first_row_id: None,
        })
    }
}

/// Stateful manifest-list decoder that reuses partition type candidates across entries.
pub struct ManifestListEntryDecoder<'a> {
    table_metadata: &'a TableMetadata,
    partition_type_candidates: HashMap<(i32, i32), Vec<Vec<Type>>>,
}

impl<'a> ManifestListEntryDecoder<'a> {
    pub fn new(table_metadata: &'a TableMetadata) -> Self {
        Self {
            table_metadata,
            partition_type_candidates: HashMap::new(),
        }
    }

    fn partition_type_candidates(
        &mut self,
        partition_spec_id: i32,
        added_snapshot_id: i64,
    ) -> Result<&[Vec<Type>], Error> {
        let preferred_schema_id =
            ManifestListEntry::preferred_schema_id(self.table_metadata, added_snapshot_id);
        let key = (partition_spec_id, preferred_schema_id);
        if !self.partition_type_candidates.contains_key(&key) {
            let candidates = ManifestListEntry::partition_type_candidates(
                self.table_metadata,
                partition_spec_id,
                preferred_schema_id,
            )?;
            self.partition_type_candidates.insert(key, candidates);
        }
        Ok(self.partition_type_candidates.get(&key).unwrap())
    }

    pub fn decode(
        &mut self,
        value: Result<AvroValue, apache_avro::Error>,
        format_version: FormatVersion,
    ) -> Result<ManifestListEntry, Error> {
        let entry = value?;
        match format_version {
            FormatVersion::V1 => {
                let entry = apache_avro::from_value::<_serde::ManifestListEntryV1>(&entry)?;
                let partition_types = self
                    .partition_type_candidates(entry.partition_spec_id, entry.added_snapshot_id)?;
                ManifestListEntry::try_from_v1_with_partition_types(entry, partition_types)
            }
            FormatVersion::V2 => {
                let entry = apache_avro::from_value::<_serde::ManifestListEntryV2>(&entry)?;
                let partition_types = self
                    .partition_type_candidates(entry.partition_spec_id, entry.added_snapshot_id)?;
                ManifestListEntry::try_from_v2_with_partition_types(entry, partition_types)
            }
            FormatVersion::V3 => {
                let entry = apache_avro::from_value::<_serde::ManifestListEntryV3>(&entry)?;
                let partition_types = self
                    .partition_type_candidates(entry.partition_spec_id, entry.added_snapshot_id)?;
                ManifestListEntry::try_from_v3_with_partition_types(entry, partition_types)
            }
        }
    }
}

impl FieldSummary {
    fn try_from(
        value: _serde::FieldSummarySerde,
        data_type_candidates: &[Type],
    ) -> Result<Self, Error> {
        Ok(FieldSummary {
            contains_null: value.contains_null,
            contains_nan: value.contains_nan,
            lower_bound: value
                .lower_bound
                .map(|x| Self::decode_bound(&x, data_type_candidates))
                .transpose()?,
            upper_bound: value
                .upper_bound
                .map(|x| Self::decode_bound(&x, data_type_candidates))
                .transpose()?,
        })
    }

    fn decode_bound(bytes: &[u8], data_type_candidates: &[Type]) -> Result<Value, Error> {
        let target_type = data_type_candidates
            .first()
            .ok_or_else(|| Error::InvalidFormat("partition field type candidates".to_string()))?;
        let mut last_error = None;
        for data_type in data_type_candidates {
            match super::manifest::decode_bound(bytes, data_type) {
                Ok(value) if data_type == target_type => return Ok(value),
                Ok(value) => match value.promote_iceberg(data_type, target_type) {
                    Ok(value) => return Ok(value),
                    Err(error) => last_error = Some(error),
                },
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| Error::InvalidFormat("partition field bound".to_string())))
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
    let entry = value?;
    match format_version {
        FormatVersion::V1 => ManifestListEntry::try_from_v1(
            apache_avro::from_value::<_serde::ManifestListEntryV1>(&entry)?,
            table_metadata,
        ),
        FormatVersion::V2 => ManifestListEntry::try_from_v2(
            apache_avro::from_value::<_serde::ManifestListEntryV2>(&entry)?,
            table_metadata,
        ),
        FormatVersion::V3 => ManifestListEntry::try_from_v3(
            apache_avro::from_value::<_serde::ManifestListEntryV3>(&entry)?,
            table_metadata,
        ),
    }
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use super::*;

    use crate::spec::{
        decimal::decimal_from_i128_with_scale,
        partition::{PartitionField, PartitionSpec, Transform},
        schema::Schema,
        snapshot::{SnapshotBuilder, Summary},
        table_metadata::TableMetadataBuilder,
        types::{PrimitiveType, StructField},
    };

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
