//! Public overwrite target validation using synthetic file metadata only.
//! No Parquet bytes are written or read; these tests cover metadata commits, not row deletion.

use std::collections::HashMap;
use std::sync::Arc;

use futures::TryStreamExt;
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::catalog::Catalog;
use iceberg_rust::error::Error;
use iceberg_rust::object_store::ObjectStoreBuilder;
use iceberg_rust::table::Table;
use iceberg_rust_spec::manifest::{Content, DataFile, FileFormat, Status};
use iceberg_rust_spec::manifest_list::Content as ManifestContent;
use iceberg_rust_spec::schema::Schema;
use iceberg_rust_spec::table_metadata::FormatVersion;
use iceberg_rust_spec::types::{PrimitiveType, StructField, Type};
use iceberg_rust_spec::values::{Struct, Value};
use iceberg_sql_catalog::SqlCatalog;

fn file(table: &Table, name: &str, content: Content) -> DataFile {
    let equality_ids = matches!(&content, Content::EqualityDeletes).then(|| vec![1]);
    DataFile::builder()
        .with_content(content)
        .with_file_path(format!("{}/data/{name}.parquet", table.metadata().location))
        .with_file_format(FileFormat::Parquet)
        .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
        .with_record_count(1)
        .with_file_size_in_bytes(100)
        .with_equality_ids(equality_ids)
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
        panic!("overwrite fixture must remain a table");
    };
    loaded
}

async fn active_files(table: &Table) -> Vec<(String, Content)> {
    let manifests = table.manifests(None, None).await.unwrap();
    let entries = table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let mut files = entries
        .into_iter()
        .filter(|(_, entry)| *entry.status() != Status::Deleted)
        .map(|(_, entry)| {
            (
                entry.data_file().file_path().clone(),
                entry.data_file().content().clone(),
            )
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

async fn assert_delete_target_rejected(version: u8, with_replacement: bool) {
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
    let name = format!("overwrite_delete_v{version}_{with_replacement}");
    let mut table = Table::builder()
        .with_name(name.as_str())
        .with_location(format!("/test/{name}"))
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

    let data = file(&table, "original", Content::Data);
    let deletes = file(&table, "deletes", Content::EqualityDeletes);
    assert_eq!(deletes.equality_ids().as_deref(), Some(&[1][..]));
    table
        .new_transaction(None)
        .append_data(vec![data.clone()])
        .append_delete(vec![deletes.clone()])
        .commit()
        .await
        .unwrap();
    let mut expected_files = vec![
        (data.file_path().clone(), Content::Data),
        (deletes.file_path().clone(), Content::EqualityDeletes),
    ];
    expected_files.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(active_files(&table).await, expected_files);

    let manifests = table.manifests(None, None).await.unwrap();
    let delete_manifests = manifests
        .iter()
        .filter(|manifest| manifest.content == ManifestContent::Deletes)
        .collect::<Vec<_>>();
    assert_eq!(delete_manifests.len(), 1);
    let target = delete_manifests[0].manifest_path.clone();
    let overwritten = HashMap::from([(target.clone(), vec![deletes.file_path().clone()])]);
    let replacements = if with_replacement {
        vec![file(&table, "replacement", Content::Data)]
    } else {
        Vec::new()
    };

    let before = serde_json::to_value(load(&catalog, &table).await.metadata()).unwrap();
    assert_eq!(serde_json::to_value(table.metadata()).unwrap(), before);
    let error = table
        .new_transaction(None)
        .overwrite(replacements, overwritten)
        .commit()
        .await
        .expect_err("data-file overwrite must reject a delete-manifest target");
    assert!(matches!(
        error,
        Error::NotSupported(message) if message.contains(&target)
    ));
    assert_eq!(serde_json::to_value(table.metadata()).unwrap(), before);
    let loaded = load(&catalog, &table).await;
    assert_eq!(serde_json::to_value(loaded.metadata()).unwrap(), before);
    assert_eq!(active_files(&loaded).await, expected_files);
}

#[tokio::test]
async fn v2_delete_target_without_replacements_is_rejected() {
    assert_delete_target_rejected(2, false).await;
}

#[tokio::test]
async fn v2_delete_target_with_replacements_is_rejected() {
    assert_delete_target_rejected(2, true).await;
}

#[tokio::test]
async fn v3_delete_target_without_replacements_is_rejected() {
    assert_delete_target_rejected(3, false).await;
}

#[tokio::test]
async fn v3_delete_target_with_replacements_is_rejected() {
    assert_delete_target_rejected(3, true).await;
}
