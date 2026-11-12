//! Whole-table rewrite contracts using synthetic file metadata and native Avro manifests.
//! Data/Puffin objects contain sentinel bytes, not valid Parquet or DV payloads. These
//! tests prove metadata publication and file retention, not row-level backfill correctness.

use std::collections::HashMap;
use std::sync::Arc;

use futures::TryStreamExt;
use iceberg_rust::catalog::commit::{CommitTable, TableRequirement, TableUpdate};
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::catalog::Catalog;
use iceberg_rust::error::Error;
use iceberg_rust::object_store::ObjectStoreBuilder;
use iceberg_rust::table::Table;
use iceberg_rust_spec::manifest::{
    Content, DataFile, DataFileBuilder, FileFormat, ManifestEntry, Status,
};
use iceberg_rust_spec::manifest_list::{Content as ManifestContent, ManifestListEntry};
use iceberg_rust_spec::partition::PartitionSpec;
use iceberg_rust_spec::schema::Schema;
use iceberg_rust_spec::snapshot::{
    Operation as SnapshotOperation, Snapshot, SnapshotBuilder, SnapshotReference, SnapshotRetention,
};
use iceberg_rust_spec::table_metadata::FormatVersion;
use iceberg_rust_spec::types::{PrimitiveType, StructField, Type};
use iceberg_rust_spec::util::strip_prefix;
use iceberg_rust_spec::values::{Struct, Value};
use iceberg_sql_catalog::SqlCatalog;
use object_store::ObjectStoreExt;

async fn table(version: u8) -> (Arc<dyn Catalog>, Table) {
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "test", ObjectStoreBuilder::memory())
            .await
            .unwrap(),
    );
    let schema = Schema::builder()
        .with_struct_field(StructField::new(
            1,
            "id",
            true,
            Type::Primitive(PrimitiveType::Long),
            None,
        ))
        .build()
        .unwrap();
    let table = Table::builder()
        .with_name("rewrite_all")
        .with_location("/test/rewrite_all")
        .with_schema(schema)
        .with_property(("format-version".to_owned(), version.to_string()))
        .build(&["test".to_owned()], Arc::clone(&catalog))
        .await
        .unwrap();
    assert_eq!(
        table.metadata().format_version,
        if version == 3 {
            FormatVersion::V3
        } else {
            FormatVersion::V2
        }
    );
    (catalog, table)
}

fn file_builder(table: &Table, name: &str, content: Content, rows: i64) -> DataFileBuilder {
    let equality_ids = (content == Content::EqualityDeletes).then(|| vec![1]);
    let mut builder = DataFile::builder();
    builder
        .with_content(content)
        .with_file_path(format!("{}/data/{name}.parquet", table.metadata().location))
        .with_file_format(FileFormat::Parquet)
        .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
        .with_record_count(rows)
        .with_file_size_in_bytes(100)
        .with_equality_ids(equality_ids)
        .with_column_sizes(None)
        .with_value_counts(None)
        .with_null_value_counts(None)
        .with_nan_value_counts(None)
        .with_distinct_counts(None)
        .with_lower_bounds(None)
        .with_upper_bounds(None);
    builder
}

fn data_file(table: &Table, name: &str, rows: i64) -> DataFile {
    file_builder(table, name, Content::Data, rows)
        .build()
        .unwrap()
}

fn replacement_files(table: &Table, empty: bool) -> Vec<DataFile> {
    if empty {
        Vec::new()
    } else {
        vec![data_file(table, "new-a", 2), data_file(table, "new-b", 3)]
    }
}

async fn load(catalog: &Arc<dyn Catalog>, table: &Table) -> Table {
    let Tabular::Table(loaded) = catalog
        .clone()
        .load_tabular(table.identifier())
        .await
        .unwrap()
    else {
        panic!("rewrite fixture must remain a table");
    };
    loaded
}

fn snapshot<'a>(table: &'a Table, branch: Option<&str>) -> &'a Snapshot {
    table.metadata().current_snapshot(branch).unwrap().unwrap()
}

async fn entries(table: &Table, branch: Option<&str>) -> Vec<ManifestEntry> {
    let manifests = manifests(table, branch).await;
    table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, entry)| entry)
        .collect()
}

async fn manifests(table: &Table, branch: Option<&str>) -> Vec<ManifestListEntry> {
    match table.metadata().current_snapshot(branch).unwrap() {
        Some(snapshot) => table
            .manifests(None, Some(*snapshot.snapshot_id()))
            .await
            .unwrap(),
        None => Vec::new(),
    }
}

async fn active_files(table: &Table, branch: Option<&str>) -> Vec<DataFile> {
    let mut files = entries(table, branch)
        .await
        .into_iter()
        .filter(|entry| *entry.status() != Status::Deleted)
        .map(|entry| entry.data_file().clone())
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.file_path().cmp(right.file_path()));
    files
}

fn assert_original_fences(commit: &CommitTable, table: &Table, branch: Option<&str>) {
    assert!(commit
        .requirements
        .contains(&TableRequirement::AssertTableUuid {
            uuid: table.metadata().table_uuid,
        }));
    assert!(commit
        .requirements
        .contains(&TableRequirement::AssertRefSnapshotId {
            r#ref: branch.unwrap_or("main").to_owned(),
            snapshot_id: table
                .metadata()
                .current_snapshot(branch)
                .unwrap()
                .map(|snapshot| *snapshot.snapshot_id()),
        }));
    for (name, reference) in &table.metadata().refs {
        assert!(commit
            .requirements
            .contains(&TableRequirement::AssertRefSnapshotId {
                r#ref: name.clone(),
                snapshot_id: Some(reference.snapshot_id),
            }));
    }
}

fn added_snapshot(commit: &CommitTable) -> &Snapshot {
    assert_eq!(
        commit.updates.len(),
        2,
        "no intermediate empty snapshot or metadata removals"
    );
    let [TableUpdate::AddSnapshot { snapshot }, TableUpdate::SetSnapshotRef {
        snapshot_reference, ..
    }] = commit.updates.as_slice()
    else {
        panic!("rewrite must publish exactly one snapshot and its ref");
    };
    assert_eq!(snapshot_reference.snapshot_id, *snapshot.snapshot_id());
    snapshot
}

async fn seed(table: &mut Table) {
    let old_a = data_file(table, "old-a", 3);
    let old_b = data_file(table, "old-b", 4);
    table
        .new_transaction(None)
        .append_data(vec![old_a.clone(), old_b.clone()])
        .commit()
        .await
        .unwrap();
    let equality = file_builder(table, "old-equality", Content::EqualityDeletes, 1)
        .build()
        .unwrap();
    let mut position = file_builder(table, "old-position", Content::PositionDeletes, 1);
    if table.metadata().format_version == FormatVersion::V3 {
        // A fresh V3 position delete must be a DV, not a Parquet delete file.
        // The pointer is valid synthetic metadata; its payload is not decoded here.
        position
            .with_file_path(format!("{}/data/old-dv.puffin", table.metadata().location))
            .with_file_format(FileFormat::Puffin)
            .with_referenced_data_file(Some(old_a.file_path().clone()))
            .with_content_offset(Some(4))
            .with_content_size_in_bytes(Some(32));
    }
    let position = position.build().unwrap();
    table
        .new_transaction(None)
        .append_delete(vec![equality.clone(), position.clone()])
        .commit()
        .await
        .unwrap();
    let old_id = *snapshot(table, None).snapshot_id();
    table
        .new_transaction(None)
        .set_snapshot_ref((
            "audit".to_owned(),
            SnapshotReference {
                snapshot_id: old_id,
                retention: SnapshotRetention::default(),
            },
        ))
        .commit()
        .await
        .unwrap();
    let main_only = data_file(table, "main-only", 2);
    table
        .new_transaction(None)
        .append_data(vec![main_only.clone()])
        .commit()
        .await
        .unwrap();
    for file in [old_a, old_b, equality, position, main_only] {
        table
            .object_store()
            .put(
                &strip_prefix(file.file_path()).into(),
                format!("synthetic file sentinel: {}", file.file_path()).into(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn rewrite_replaces_all_manifests_and_retains_history_in_v2_and_v3() {
    for version in [2, 3] {
        for empty in [false, true] {
            for branch in [None, Some("main"), Some("audit")] {
                let (catalog, mut table) = table(version).await;
                seed(&mut table).await;
                let base = table.metadata().clone();
                let parent = snapshot(&table, branch).clone();
                let old_manifests = manifests(&table, branch).await;
                assert!(old_manifests
                    .iter()
                    .any(|m| m.content == ManifestContent::Deletes));
                let old_files = active_files(&table, branch).await;
                assert!(old_files
                    .iter()
                    .any(|file| *file.content() == Content::EqualityDeletes));
                let position = old_files
                    .iter()
                    .find(|file| *file.content() == Content::PositionDeletes)
                    .unwrap();
                if version == 3 {
                    assert_eq!(*position.file_format(), FileFormat::Puffin);
                    assert!(position.referenced_data_file().is_some());
                    assert_eq!(*position.content_offset(), Some(4));
                    assert_eq!(*position.content_size_in_bytes(), Some(32));
                }
                let old_objects = table
                    .object_store()
                    .list(None)
                    .map_ok(|object| object.location)
                    .try_collect::<Vec<_>>()
                    .await
                    .unwrap();
                let mut old_bytes = Vec::new();
                for path in old_objects {
                    if path.as_ref().ends_with("/version-hint.text") {
                        continue;
                    }
                    let bytes = table
                        .object_store()
                        .get(&path)
                        .await
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap();
                    old_bytes.push((path, bytes));
                }
                let files = replacement_files(&table, empty);
                let mut expected_paths = files
                    .iter()
                    .map(|file| file.file_path().clone())
                    .collect::<Vec<_>>();
                expected_paths.sort();
                let prepared = table
                    .new_transaction(branch)
                    .rewrite_all(files)
                    .prepare_commit()
                    .await
                    .unwrap()
                    .unwrap();
                assert_original_fences(&prepared, &table, branch);
                let added = added_snapshot(&prepared);
                let added_id = *added.snapshot_id();
                assert_eq!(*added.parent_snapshot_id(), Some(*parent.snapshot_id()));
                assert_eq!(*added.schema_id(), Some(base.current_schema_id));
                assert_eq!(*added.sequence_number(), base.last_sequence_number + 1);
                assert_eq!(added.summary().operation, SnapshotOperation::Overwrite);
                let rows = if empty { 0 } else { 5 };
                assert_eq!(added.summary().other["total-records"], rows.to_string());
                assert_eq!(
                    added.summary().other["total-data-files"],
                    if empty { "0" } else { "2" }
                );
                assert_eq!(added.summary().other["total-delete-files"], "0");
                if version == 3 {
                    assert_eq!(*added.first_row_id(), Some(base.next_row_id));
                    assert_eq!(*added.added_rows(), Some(rows));
                } else {
                    assert_eq!(*added.first_row_id(), None);
                    assert_eq!(*added.added_rows(), None);
                }
                assert_eq!(
                    serde_json::to_value(table.metadata()).unwrap(),
                    serde_json::to_value(&base).unwrap()
                );
                assert_eq!(
                    serde_json::to_value(load(&catalog, &table).await.metadata()).unwrap(),
                    serde_json::to_value(&base).unwrap()
                );

                let published = catalog.update_table(prepared).await.unwrap();
                assert_eq!(
                    published.metadata().snapshots.len(),
                    base.snapshots.len() + 1
                );
                assert_eq!(
                    published.metadata().snapshot_log.len(),
                    base.snapshot_log.len() + 1
                );
                for (id, old) in &base.snapshots {
                    assert_eq!(published.metadata().snapshots.get(id), Some(old));
                }
                assert_eq!(
                    published.metadata().next_row_id,
                    base.next_row_id + if version == 3 { rows } else { 0 }
                );
                for (name, reference) in &base.refs {
                    if name != branch.unwrap_or("main") {
                        assert_eq!(published.metadata().refs.get(name), Some(reference));
                    }
                }
                if branch == Some("audit") {
                    assert_eq!(
                        published.metadata().current_snapshot_id,
                        base.current_snapshot_id
                    );
                    assert!(*parent.sequence_number() < base.last_sequence_number);
                }
                let manifests = manifests(&published, branch).await;
                assert_eq!(manifests.is_empty(), empty);
                for manifest in &manifests {
                    assert_eq!(manifest.content, ManifestContent::Data);
                    assert_eq!(manifest.added_snapshot_id, added_id);
                    assert_eq!(manifest.sequence_number, base.last_sequence_number + 1);
                    assert!(!old_manifests
                        .iter()
                        .any(|old| old.manifest_path == manifest.manifest_path));
                }
                let live = active_files(&published, branch).await;
                assert!(live.iter().all(|file| *file.content() == Content::Data));
                if version == 3 {
                    let mut ranges = live
                        .iter()
                        .map(|file| (file.first_row_id().unwrap(), *file.record_count()))
                        .collect::<Vec<_>>();
                    ranges.sort_unstable();
                    let mut next_row_id = base.next_row_id;
                    for (first_row_id, count) in ranges {
                        assert_eq!(first_row_id, next_row_id);
                        next_row_id += count;
                    }
                    assert_eq!(next_row_id, published.metadata().next_row_id);
                }
                assert_eq!(
                    live.iter()
                        .map(|file| file.file_path().clone())
                        .collect::<Vec<_>>(),
                    expected_paths
                );
                for entry in entries(&published, branch).await {
                    assert_eq!(*entry.status(), Status::Added);
                    assert_eq!(*entry.snapshot_id(), Some(added_id));
                    assert_eq!(
                        *entry.sequence_number(),
                        Some(base.last_sequence_number + 1)
                    );
                }
                for (path, bytes) in old_bytes {
                    assert_eq!(
                        published
                            .object_store()
                            .get(&path)
                            .await
                            .unwrap()
                            .bytes()
                            .await
                            .unwrap(),
                        bytes
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn rewrite_new_table_or_absent_branch_has_no_parent_and_one_snapshot() {
    for version in [2, 3] {
        for empty in [false, true] {
            for branch in [None, Some("new-branch")] {
                let (catalog, mut table) = table(version).await;
                if branch.is_some() {
                    seed(&mut table).await;
                }
                let base = table.metadata().clone();
                let files = replacement_files(&table, empty);
                let prepared = table
                    .new_transaction(branch)
                    .rewrite_all(files)
                    .prepare_commit()
                    .await
                    .unwrap()
                    .unwrap();
                assert_original_fences(&prepared, &table, branch);
                assert_eq!(*added_snapshot(&prepared).parent_snapshot_id(), None);
                let published = catalog.update_table(prepared).await.unwrap();
                assert_eq!(
                    published.metadata().snapshots.len(),
                    base.snapshots.len() + 1
                );
                assert_eq!(
                    *snapshot(&published, branch).sequence_number(),
                    base.last_sequence_number + 1
                );
                assert_eq!(
                    active_files(&published, branch).await.len(),
                    if empty { 0 } else { 2 }
                );
                if version == 3 {
                    assert_eq!(
                        *snapshot(&published, branch).first_row_id(),
                        Some(base.next_row_id)
                    );
                    assert_eq!(
                        published.metadata().next_row_id,
                        base.next_row_id + if empty { 0 } else { 5 }
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn stale_rewrite_conflicts_without_losing_winning_file_metadata() {
    for version in [2, 3] {
        for empty in [false, true] {
            for winning_branch in [None, Some("audit"), Some("new-branch")] {
                let (catalog, mut table) = table(version).await;
                seed(&mut table).await;
                let mut stale = table.clone();
                let files = replacement_files(&table, empty);
                let prepared = table
                    .new_transaction(None)
                    .rewrite_all(files.clone())
                    .prepare_commit()
                    .await
                    .unwrap()
                    .unwrap();
                assert_original_fences(&prepared, &table, None);
                let winner = data_file(&table, "winner", 11);
                table
                    .new_transaction(winning_branch)
                    .append_data(vec![winner.clone()])
                    .commit()
                    .await
                    .unwrap();
                let winning_metadata = serde_json::to_value(table.metadata()).unwrap();
                let winning_files = active_files(&table, winning_branch).await;
                assert!(winning_files
                    .iter()
                    .any(|file| file.file_path() == winner.file_path()));
                assert!(matches!(
                    catalog.clone().update_table(prepared).await,
                    Err(Error::CommitConflict(_))
                ));
                assert!(matches!(
                    stale
                        .new_transaction(None)
                        .rewrite_all(files)
                        .commit()
                        .await,
                    Err(Error::CommitConflict(_))
                ));
                let loaded = load(&catalog, &table).await;
                assert_eq!(
                    serde_json::to_value(loaded.metadata()).unwrap(),
                    winning_metadata
                );
                assert_eq!(active_files(&loaded, winning_branch).await, winning_files);
            }
        }
    }
}

#[tokio::test]
async fn rewrite_fences_ref_moves_and_target_creation_without_a_new_sequence_number() {
    for version in [2, 3] {
        for empty in [false, true] {
            for target in [None, Some("new-branch")] {
                let (catalog, mut table) = table(version).await;
                seed(&mut table).await;
                let files = replacement_files(&table, empty);
                let prepared = table
                    .new_transaction(target)
                    .rewrite_all(files)
                    .prepare_commit()
                    .await
                    .unwrap()
                    .unwrap();
                assert_original_fences(&prepared, &table, target);
                let sequence = table.metadata().last_sequence_number;
                let main_id = *snapshot(&table, None).snapshot_id();
                let moved_ref = target.unwrap_or("audit");
                table
                    .new_transaction(None)
                    .set_snapshot_ref((
                        moved_ref.to_owned(),
                        SnapshotReference {
                            snapshot_id: main_id,
                            retention: SnapshotRetention::default(),
                        },
                    ))
                    .commit()
                    .await
                    .unwrap();
                assert_eq!(table.metadata().last_sequence_number, sequence);
                assert!(matches!(
                    catalog.clone().update_table(prepared).await,
                    Err(Error::CommitConflict(_))
                ));
                assert_eq!(
                    load(&catalog, &table).await.metadata().refs[moved_ref].snapshot_id,
                    main_id
                );
            }
        }
    }
}

#[tokio::test]
async fn rewrite_rejects_other_data_operations_and_delete_inputs_before_writing() {
    let (catalog, mut table) = table(3).await;
    seed(&mut table).await;
    let before = serde_json::to_value(table.metadata()).unwrap();
    let object_count = table
        .object_store()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .len();
    for empty in [false, true] {
        for operation in 0..6 {
            let files = replacement_files(&table, empty);
            let data = data_file(&table, "conflicting", 1);
            let transaction = table.new_transaction(None).rewrite_all(files);
            let transaction = match operation {
                0 => transaction.append_data(vec![data]),
                1 => transaction.append_delete(Vec::new()),
                2 => transaction.append_sequence_group(Vec::new(), Vec::new()),
                3 => transaction.replace(Vec::new()),
                4 => transaction.overwrite(Vec::new(), HashMap::new()),
                5 => transaction.truncate(),
                _ => unreachable!(),
            };
            assert!(matches!(
                transaction.prepare_commit().await,
                Err(Error::NotSupported(_))
            ));
        }
    }
    for delete in active_files(&table, None)
        .await
        .into_iter()
        .filter(|file| *file.content() != Content::Data)
    {
        assert!(matches!(
            table
                .new_transaction(None)
                .rewrite_all(vec![delete])
                .prepare_commit()
                .await,
            Err(Error::NotSupported(_))
        ));
    }
    assert_eq!(
        table
            .object_store()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        object_count
    );
    assert_eq!(serde_json::to_value(table.metadata()).unwrap(), before);
    assert_eq!(
        serde_json::to_value(load(&catalog, &table).await.metadata()).unwrap(),
        before
    );
}

#[tokio::test]
async fn rewrite_preserves_partition_spec_write_guards_and_combines_replacement_calls() {
    let (catalog, mut table) = table(2).await;
    seed(&mut table).await;
    table = catalog
        .clone()
        .update_table(CommitTable {
            identifier: table.identifier().clone(),
            requirements: Vec::new(),
            updates: vec![TableUpdate::AddSpec {
                spec: PartitionSpec::builder()
                    .with_spec_id(1)
                    .with_fields(Vec::new())
                    .build()
                    .unwrap(),
            }],
        })
        .await
        .unwrap();
    let a = data_file(&table, "new-a", 2);
    let b = data_file(&table, "new-b", 3);
    assert!(matches!(
        table
            .new_transaction(None)
            .set_default_spec(1)
            .rewrite_all(vec![a.clone()])
            .prepare_commit()
            .await,
        Err(Error::NotSupported(_))
    ));
    let prepared = table
        .new_transaction(None)
        .set_default_spec(0)
        .rewrite_all(vec![a])
        .rewrite_all(vec![b])
        .prepare_commit()
        .await
        .unwrap()
        .unwrap();
    assert_original_fences(&prepared, &table, None);
    let published = catalog.update_table(prepared).await.unwrap();
    assert_eq!(active_files(&published, None).await.len(), 2);
    assert_eq!(
        snapshot(&published, None).summary().other["total-records"],
        "5"
    );
    // Empty output follows TRUNCATE's existing allowance for a spec-only change.
    let mut published = published;
    published
        .new_transaction(None)
        .set_default_spec(1)
        .rewrite_all(Vec::new())
        .commit()
        .await
        .unwrap();
    assert_eq!(published.metadata().default_spec_id, 1);
    assert!(published.manifests(None, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn rewrite_does_not_need_old_manifest_objects() {
    for version in [2, 3] {
        for empty in [false, true] {
            let (catalog, table) = table(version).await;
            let mut metadata = table.metadata().clone();
            let mut builder = SnapshotBuilder::default();
            builder
                .with_snapshot_id(101)
                .with_sequence_number(40)
                .with_schema_id(metadata.current_schema_id)
                .with_manifest_list("/missing/old-manifest-list.avro".to_owned());
            if version == 3 {
                builder.with_first_row_id(0).with_added_rows(17);
                metadata.next_row_id = 17;
            }
            metadata.snapshots.insert(101, builder.build().unwrap());
            metadata.refs.insert(
                "main".to_owned(),
                SnapshotReference {
                    snapshot_id: 101,
                    retention: SnapshotRetention::default(),
                },
            );
            metadata.current_snapshot_id = Some(101);
            metadata.last_sequence_number = 40;
            let mut staged = Table::new(
                table.identifier().clone(),
                catalog,
                table.object_store(),
                metadata,
            )
            .await
            .unwrap();
            let files = replacement_files(&staged, empty);
            let prepared = staged
                .new_transaction(None)
                .rewrite_all(files)
                .prepare_commit()
                .await
                .unwrap()
                .unwrap();
            assert_original_fences(&prepared, &staged, None);
            let added = added_snapshot(&prepared);
            assert_eq!(*added.parent_snapshot_id(), Some(101));
            assert_eq!(*added.sequence_number(), 41);
        }
    }
}
