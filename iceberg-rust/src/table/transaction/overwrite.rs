use std::{cmp::Ordering, collections::HashSet};

use iceberg_rust_spec::{
    manifest_list::{Content, ManifestListEntry},
    table_metadata::TableMetadata,
};

use crate::{
    error::Error,
    table::manifest_list::{
        append_manifest, ManifestListReader, ManifestRewriteCompatibility, RowIdAssigner,
    },
    util::{summary_to_rectangle, Rectangle},
};

pub(crate) struct OverwriteManifest {
    pub manifest: Option<ManifestListEntry>,
    pub file_count_all_entries: usize,
    pub manifests_to_overwrite: Vec<ManifestListEntry>,
}

/// Select the manifest that yields the smallest bounding rectangle after the
/// bounding rectangle of the new values has been added.
pub(crate) fn select_manifest_without_overwrites_partitioned(
    manifest_list_reader: ManifestListReader<&[u8]>,
    manifest_list_writer: &mut apache_avro::Writer<Vec<u8>>,
    mut row_id_assigner: Option<&mut RowIdAssigner>,
    bounding_partition_values: &Rectangle,
    overwrites: &HashSet<String>,
    table_metadata: &TableMetadata,
) -> Result<OverwriteManifest, Error> {
    let mut compatibility = ManifestRewriteCompatibility::new(table_metadata)?;
    let mut selected_state = None;
    let mut file_count_all_entries = 0;
    let mut manifests_to_overwrite = Vec::new();
    for manifest_res in manifest_list_reader {
        let manifest = manifest_res?;

        if manifest.content == Content::Deletes {
            if overwrites.contains(&manifest.manifest_path) {
                return Err(Error::NotSupported(format!(
                    "Overwriting delete manifest {} through a data-file overwrite is not supported",
                    manifest.manifest_path
                )));
            }
            append_manifest(
                manifest_list_writer,
                row_id_assigner.as_deref_mut(),
                manifest,
            )?;
            continue;
        }

        file_count_all_entries += manifest.added_files_count.unwrap_or(0) as usize;
        if !compatibility.allows(&manifest, table_metadata) {
            if overwrites.contains(&manifest.manifest_path) {
                manifests_to_overwrite.push(manifest);
            } else {
                append_manifest(
                    manifest_list_writer,
                    row_id_assigner.as_deref_mut(),
                    manifest,
                )?;
            }
            continue;
        }

        let mut bounds =
            summary_to_rectangle(manifest.partitions.as_ref().ok_or(Error::NotFound(format!(
                "Partition struct in manifest {}",
                manifest.manifest_path
            )))?)?;

        bounds.expand(bounding_partition_values);

        let Some((selected_bounds, _)) = &selected_state else {
            selected_state = Some((bounds, manifest));
            continue;
        };

        match selected_bounds.cmp_with_priority(&bounds)? {
            Ordering::Greater => {
                let old = selected_state.replace((bounds, manifest));
                if let Some((_, old)) = old {
                    if !overwrites.contains(&old.manifest_path) {
                        append_manifest(manifest_list_writer, row_id_assigner.as_deref_mut(), old)?;
                    } else {
                        manifests_to_overwrite.push(old);
                    }
                }
                continue;
            }
            _ => {
                if !overwrites.contains(&manifest.manifest_path) {
                    append_manifest(
                        manifest_list_writer,
                        row_id_assigner.as_deref_mut(),
                        manifest,
                    )?;
                } else {
                    manifests_to_overwrite.push(manifest);
                }

                continue;
            }
        }
    }
    Ok(OverwriteManifest {
        manifest: selected_state.map(|(_, entry)| entry),
        file_count_all_entries,
        manifests_to_overwrite,
    })
}

/// Select the manifest with the smallest number of rows.
pub(crate) fn select_manifest_without_overwrites_unpartitioned(
    manifest_list_reader: ManifestListReader<&[u8]>,
    manifest_list_writer: &mut apache_avro::Writer<Vec<u8>>,
    mut row_id_assigner: Option<&mut RowIdAssigner>,
    overwrites: &HashSet<String>,
    table_metadata: &TableMetadata,
) -> Result<OverwriteManifest, Error> {
    let mut compatibility = ManifestRewriteCompatibility::new(table_metadata)?;
    let mut selected_state = None;
    let mut file_count_all_entries = 0;
    let mut manifests_to_overwrite = Vec::new();
    for manifest_res in manifest_list_reader {
        let manifest = manifest_res?;
        if manifest.content == Content::Deletes {
            if overwrites.contains(&manifest.manifest_path) {
                return Err(Error::NotSupported(format!(
                    "Overwriting delete manifest {} through a data-file overwrite is not supported",
                    manifest.manifest_path
                )));
            }
            append_manifest(
                manifest_list_writer,
                row_id_assigner.as_deref_mut(),
                manifest,
            )?;
            continue;
        }

        // TODO: should this also account for existing_rows_count / existing_files_count?
        let row_count = manifest.added_rows_count;
        file_count_all_entries += manifest.added_files_count.unwrap_or(0) as usize;

        if !compatibility.allows(&manifest, table_metadata) {
            if overwrites.contains(&manifest.manifest_path) {
                manifests_to_overwrite.push(manifest);
            } else {
                append_manifest(
                    manifest_list_writer,
                    row_id_assigner.as_deref_mut(),
                    manifest,
                )?;
            }
            continue;
        }

        let Some((selected_row_count, _)) = &selected_state else {
            selected_state = Some((row_count, manifest));
            continue;
        };

        // If the file doesn't have any rows, we select it
        let Some(row_count) = row_count else {
            selected_state = Some((row_count, manifest));
            continue;
        };

        if selected_row_count.is_some_and(|x| x > row_count) {
            let old = selected_state.replace((Some(row_count), manifest));
            if let Some((_, old)) = old {
                if !overwrites.contains(&old.manifest_path) {
                    append_manifest(manifest_list_writer, row_id_assigner.as_deref_mut(), old)?;
                } else {
                    manifests_to_overwrite.push(old);
                }
            }
            continue;
        } else {
            if !overwrites.contains(&manifest.manifest_path) {
                append_manifest(
                    manifest_list_writer,
                    row_id_assigner.as_deref_mut(),
                    manifest,
                )?;
            } else {
                manifests_to_overwrite.push(manifest);
            }
            continue;
        }
    }
    Ok(OverwriteManifest {
        manifest: selected_state.map(|(_, entry)| entry),
        file_count_all_entries,
        manifests_to_overwrite,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use iceberg_rust_spec::{
        manifest_list::{manifest_list_schema_v2, FieldSummary},
        partition::{PartitionField, PartitionSpec, Transform},
        schema::SchemaBuilder,
        snapshot::{SnapshotBuilder, SnapshotReference, SnapshotRetention, Summary},
        table_metadata::{FormatVersion, TableMetadataBuilder},
        types::{PrimitiveType, StructField, Type},
        values::Value,
    };

    use super::*;

    fn metadata(partitioned: bool) -> TableMetadata {
        let schema = SchemaBuilder::default()
            .with_schema_id(0)
            .with_struct_field(StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Long),
                None,
            ))
            .build()
            .unwrap();
        let spec = if partitioned {
            PartitionSpec::builder()
                .with_spec_id(0)
                .with_partition_field(PartitionField::new(1, 1000, "id", Transform::Identity))
                .build()
                .unwrap()
        } else {
            PartitionSpec::default()
        };
        let snapshot = SnapshotBuilder::default()
            .with_snapshot_id(7)
            .with_sequence_number(7)
            .with_timestamp_ms(7)
            .with_manifest_list("s3://tests/table/metadata/snapshot-7.avro".to_owned())
            .with_schema_id(0)
            .with_summary(Summary::default())
            .build()
            .unwrap();
        TableMetadataBuilder::default()
            .format_version(FormatVersion::V2)
            .location("s3://tests/table".to_owned())
            .last_sequence_number(7)
            .last_column_id(1)
            .current_schema_id(0)
            .schemas(HashMap::from([(0, schema)]))
            .default_spec_id(0)
            .partition_specs(HashMap::from([(0, spec)]))
            .last_partition_id(if partitioned { 1000 } else { 0 })
            .current_snapshot_id(Some(7))
            .snapshots(HashMap::from([(7, snapshot)]))
            .refs(HashMap::from([(
                "main".to_owned(),
                SnapshotReference {
                    snapshot_id: 7,
                    retention: SnapshotRetention::default(),
                },
            )]))
            .build()
            .unwrap()
    }

    fn partition_summary(lower: i64, upper: i64) -> FieldSummary {
        FieldSummary {
            contains_null: true,
            contains_nan: Some(false),
            lower_bound: Some(Value::LongInt(lower)),
            upper_bound: Some(Value::LongInt(upper)),
        }
    }

    fn manifest(path: &str, content: Content, partitioned: bool) -> ManifestListEntry {
        ManifestListEntry {
            format_version: FormatVersion::V2,
            manifest_path: format!("s3://tests/table/metadata/{path}.avro"),
            manifest_length: 123,
            partition_spec_id: 0,
            content,
            sequence_number: 7,
            min_sequence_number: 3,
            added_snapshot_id: 7,
            added_files_count: Some(3),
            existing_files_count: Some(5),
            deleted_files_count: Some(7),
            added_rows_count: Some(100),
            existing_rows_count: Some(200),
            deleted_rows_count: Some(300),
            partitions: Some(if partitioned {
                vec![partition_summary(10, 20)]
            } else {
                Vec::new()
            }),
            key_metadata: Some(vec![1, 2, 3].into()),
            first_row_id: None,
        }
    }

    fn select(
        partitioned: bool,
        entries: &[ManifestListEntry],
        overwrites: &HashSet<String>,
    ) -> Result<(OverwriteManifest, Vec<ManifestListEntry>), Error> {
        let metadata = metadata(partitioned);
        let mut compatibility = ManifestRewriteCompatibility::new(&metadata).unwrap();
        let mut input = apache_avro::Writer::new(manifest_list_schema_v2(), Vec::new());
        for entry in entries {
            // Keep content as the only reason a delete cannot be a data candidate.
            assert!(compatibility.allows(entry, &metadata));
            input.append_ser(entry).unwrap();
        }
        let input = input.into_inner().unwrap();
        let reader = ManifestListReader::new(input.as_slice(), &metadata).unwrap();
        let mut output = apache_avro::Writer::new(manifest_list_schema_v2(), Vec::new());
        let selected = if partitioned {
            select_manifest_without_overwrites_partitioned(
                reader,
                &mut output,
                None,
                &summary_to_rectangle(&[partition_summary(0, 0)]).unwrap(),
                overwrites,
                &metadata,
            )
        } else {
            select_manifest_without_overwrites_unpartitioned(
                reader,
                &mut output,
                None,
                overwrites,
                &metadata,
            )
        }?;
        let output = output.into_inner().unwrap();
        let preserved = ManifestListReader::new(output.as_slice(), &metadata)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Ok((selected, preserved))
    }

    fn manifests(partitioned: bool) -> [ManifestListEntry; 3] {
        let selected = manifest("selected-data", Content::Data, partitioned);
        let mut other = manifest("other-data", Content::Data, partitioned);
        other.added_files_count = Some(4);
        other.added_rows_count = Some(200);
        if partitioned {
            other.partitions = Some(vec![partition_summary(30, 50)]);
        }
        let mut deletes = manifest("deletes", Content::Deletes, partitioned);
        deletes.added_files_count = Some(11);
        deletes.added_rows_count = Some(1);
        if partitioned {
            deletes.partitions = Some(vec![partition_summary(0, 0)]);
        }
        [selected, other, deletes]
    }

    #[test]
    fn delete_manifests_never_compete_with_data_in_any_input_order() {
        for partitioned in [false, true] {
            let [data, other, deletes] = manifests(partitioned);
            for (entries, expected_preserved) in [
                (
                    vec![data.clone(), deletes.clone(), other.clone()],
                    vec![deletes.clone(), other.clone()],
                ),
                (
                    vec![deletes.clone(), data.clone(), other.clone()],
                    vec![deletes.clone(), other.clone()],
                ),
                (
                    vec![data.clone(), other.clone(), deletes.clone()],
                    vec![other.clone(), deletes.clone()],
                ),
                (
                    vec![other.clone(), deletes.clone(), data.clone()],
                    vec![deletes.clone(), other.clone()],
                ),
                (
                    vec![deletes.clone(), other.clone(), data.clone()],
                    vec![deletes.clone(), other.clone()],
                ),
                (
                    vec![other.clone(), data.clone(), deletes.clone()],
                    vec![other.clone(), deletes.clone()],
                ),
            ] {
                let (selected, preserved) = select(partitioned, &entries, &HashSet::new()).unwrap();
                assert_eq!(selected.manifest.as_ref(), Some(&data));
                assert_eq!(selected.file_count_all_entries, 7);
                assert!(selected.manifests_to_overwrite.is_empty());
                assert_eq!(preserved, expected_preserved);
            }
        }
    }

    #[test]
    fn delete_only_lists_are_preserved_without_data_candidates_or_counts() {
        for partitioned in [false, true] {
            let [_, _, first] = manifests(partitioned);
            let mut second = first.clone();
            second.manifest_path = "s3://tests/table/metadata/other-deletes.avro".to_owned();
            second.manifest_length = 456;
            second.added_rows_count = Some(2);
            for entries in [vec![first.clone(), second.clone()], vec![second, first]] {
                let (selected, preserved) = select(partitioned, &entries, &HashSet::new()).unwrap();
                assert!(selected.manifest.is_none());
                assert_eq!(selected.file_count_all_entries, 0);
                assert!(selected.manifests_to_overwrite.is_empty());
                assert_eq!(preserved, entries);
            }
        }
    }

    #[test]
    fn unaffected_delete_manifests_survive_data_overwrite_maps() {
        for partitioned in [false, true] {
            let [data, other, deletes] = manifests(partitioned);
            let overwrites = HashSet::from([other.manifest_path.clone()]);
            let entries = [deletes.clone(), data.clone(), other.clone()];
            let (selected, preserved) = select(partitioned, &entries, &overwrites).unwrap();
            assert_eq!(selected.manifest.as_ref(), Some(&data));
            assert_eq!(selected.file_count_all_entries, 7);
            assert_eq!(selected.manifests_to_overwrite, vec![other]);
            assert_eq!(preserved, vec![deletes]);
        }
    }

    #[test]
    fn explicitly_targeted_delete_manifests_are_rejected() {
        for partitioned in [false, true] {
            let [data, other, deletes] = manifests(partitioned);
            let overwrites =
                HashSet::from([deletes.manifest_path.clone(), other.manifest_path.clone()]);
            for entries in [
                vec![deletes.clone()],
                vec![deletes.clone(), data.clone(), other.clone()],
                vec![other.clone(), data.clone(), deletes.clone()],
            ] {
                assert!(matches!(
                    select(partitioned, &entries, &overwrites),
                    Err(Error::NotSupported(message)) if message.contains(&deletes.manifest_path)
                ));
            }
        }
    }
}
