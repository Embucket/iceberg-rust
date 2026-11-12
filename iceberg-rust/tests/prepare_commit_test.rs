use std::collections::HashMap;
use std::sync::Arc;

use futures::TryStreamExt;
use iceberg_rust::catalog::commit::{apply_table_updates, TableRequirement, TableUpdate};
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::catalog::Catalog;
use iceberg_rust::error::Error;
use iceberg_rust::object_store::ObjectStoreBuilder;
use iceberg_rust::table::Table;
use iceberg_rust_spec::spec::manifest::{Content, DataFile, FileFormat, Status};
use iceberg_rust_spec::spec::schema::Schema;
use iceberg_rust_spec::spec::table_metadata::FormatVersion;
use iceberg_rust_spec::spec::types::{PrimitiveType, StructField, Type};
use iceberg_rust_spec::spec::values::{Struct, Value};
use iceberg_sql_catalog::SqlCatalog;

async fn table(name: &str, version: u8) -> (Arc<dyn Catalog>, Table) {
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
        .with_name(name)
        .with_location(format!("/test/{name}"))
        .with_schema(schema)
        .with_property(("format-version".to_owned(), version.to_string()))
        .build(&["test".to_owned()], Arc::clone(&catalog))
        .await
        .unwrap();
    (catalog, table)
}

fn data_file(name: &str, count: i64) -> DataFile {
    DataFile::builder()
        .with_content(Content::Data)
        .with_file_path(format!("s3://tests/{name}.parquet"))
        .with_file_format(FileFormat::Parquet)
        .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
        .with_record_count(count)
        .with_file_size_in_bytes(100)
        .with_column_sizes(None)
        .with_value_counts(None)
        .with_null_value_counts(None)
        .with_nan_value_counts(None)
        .with_distinct_counts(None)
        .with_lower_bounds(None)
        .with_upper_bounds(None)
        .build()
        .unwrap()
}

async fn load(catalog: &Arc<dyn Catalog>, table: &Table) -> Table {
    let Tabular::Table(loaded) = catalog
        .clone()
        .load_tabular(table.identifier())
        .await
        .unwrap()
    else {
        panic!("not a table");
    };
    loaded
}

#[tokio::test]
async fn prepare_leaves_the_handle_and_catalog_unchanged_until_explicit_publication() {
    let (catalog, mut table) = table("prepare_properties", 2).await;
    let before = serde_json::to_value(table.metadata()).unwrap();
    assert!(table
        .new_transaction(None)
        .prepare_commit()
        .await
        .unwrap()
        .is_none());
    let prepared = table
        .new_transaction(None)
        .update_properties(vec![("prepared".to_owned(), "yes".to_owned())])
        .prepare_commit()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(table.metadata()).unwrap(), before);
    assert_eq!(
        serde_json::to_value(load(&catalog, &table).await.metadata()).unwrap(),
        before
    );
    assert!(prepared.requirements.iter().any(|requirement| matches!(
        requirement,
        TableRequirement::AssertTableUuid { uuid } if *uuid == table.metadata().table_uuid
    )));
    let published = catalog.update_table(prepared).await.unwrap();
    assert_eq!(published.metadata().properties["prepared"], "yes");
    assert!(!table.metadata().properties.contains_key("prepared"));
    table = published;
    assert_eq!(table.metadata().properties["prepared"], "yes");
}

#[tokio::test]
async fn prepared_overwrite_composes_with_atomic_schema_activation_in_v2_and_v3() {
    for version in [2, 3] {
        let (catalog, mut table) = table("prepare_schema", version).await;
        table
            .new_transaction(None)
            .append_data(vec![data_file("old", 2)])
            .commit()
            .await
            .unwrap();
        let before = serde_json::to_value(table.metadata()).unwrap();
        let base_schema = table.metadata().current_schema_id;
        let base_snapshot = *table
            .metadata()
            .current_snapshot(None)
            .unwrap()
            .unwrap()
            .snapshot_id();
        let manifests = table.manifests(None, None).await.unwrap();
        let entries = table
            .datafiles(&manifests, None, (None, None))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let mut overwritten: HashMap<String, Vec<String>> = HashMap::new();
        for (manifest, entry) in entries {
            if *entry.status() != Status::Deleted && *entry.data_file().content() == Content::Data {
                overwritten
                    .entry(manifest)
                    .or_default()
                    .push(entry.data_file().file_path().clone());
            }
        }
        assert_eq!(overwritten.values().map(Vec::len).sum::<usize>(), 1);
        let schema = Schema::builder()
            .with_schema_id(base_schema + 1)
            .with_struct_field(StructField::new(
                1,
                "id",
                true,
                Type::Primitive(PrimitiveType::Long),
                None,
            ))
            .with_struct_field(StructField::new(
                2,
                "added",
                false,
                Type::Primitive(PrimitiveType::Long),
                None,
            ))
            .build()
            .unwrap();
        let schema_updates = vec![
            TableUpdate::AddSchema {
                schema,
                last_column_id: Some(2),
            },
            TableUpdate::SetCurrentSchema { schema_id: -1 },
        ];
        let mut staged_metadata = table.metadata().clone();
        apply_table_updates(&mut staged_metadata, schema_updates.clone()).unwrap();
        let mut staged = Table::new(
            table.identifier().clone(),
            Arc::clone(&catalog),
            table.object_store(),
            staged_metadata,
        )
        .await
        .unwrap();
        let mut prepared = staged
            .new_transaction(None)
            .overwrite(vec![data_file("rewritten", 2)], overwritten)
            .prepare_commit()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(load(&catalog, &table).await.metadata()).unwrap(),
            before
        );
        prepared.requirements.extend([
            TableRequirement::AssertCurrentSchemaId {
                current_schema_id: base_schema,
            },
            TableRequirement::AssertLastAssignedFieldId {
                last_assigned_field_id: 1,
            },
        ]);
        prepared.updates.splice(0..0, schema_updates);
        prepared.updates.push(TableUpdate::SetProperties {
            updates: HashMap::from([("default.2".to_owned(), "7".to_owned())]),
        });
        let published = catalog.update_table(prepared).await.unwrap();
        assert_eq!(published.metadata().current_schema_id, base_schema + 1);
        assert_eq!(published.metadata().properties["default.2"], "7");
        let snapshot = published
            .metadata()
            .current_snapshot(None)
            .unwrap()
            .unwrap();
        assert_eq!(*snapshot.parent_snapshot_id(), Some(base_snapshot));
        assert_eq!(*snapshot.schema_id(), Some(base_schema + 1));
        assert_eq!(
            published.metadata().format_version,
            if version == 3 {
                FormatVersion::V3
            } else {
                FormatVersion::V2
            }
        );
        let manifests = published.manifests(None, None).await.unwrap();
        let entries = published
            .datafiles(&manifests, None, (None, None))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let live = entries
            .iter()
            .filter(|(_, entry)| *entry.status() != Status::Deleted)
            .map(|(_, entry)| entry.data_file().file_path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(live, vec!["s3://tests/rewritten.parquet"]);
    }
}

#[tokio::test]
async fn prepared_commit_preserves_the_optimistic_snapshot_fence() {
    let (catalog, mut table) = table("prepare_conflict", 2).await;
    let mut stale = table.clone();
    let prepared = table
        .new_transaction(None)
        .append_data(vec![data_file("prepared", 2)])
        .prepare_commit()
        .await
        .unwrap()
        .unwrap();
    table
        .new_transaction(None)
        .append_data(vec![data_file("concurrent", 1)])
        .commit()
        .await
        .unwrap();
    assert!(matches!(
        catalog.clone().update_table(prepared).await,
        Err(Error::CommitConflict(_))
    ));
    assert!(matches!(
        stale
            .new_transaction(None)
            .append_data(vec![data_file("stale_ordinary", 2)])
            .commit()
            .await,
        Err(Error::CommitConflict(_))
    ));
    let loaded = load(&catalog, &table).await;
    let snapshot = loaded.metadata().current_snapshot(None).unwrap().unwrap();
    assert_eq!(snapshot.summary().other["total-records"], "1");
}
