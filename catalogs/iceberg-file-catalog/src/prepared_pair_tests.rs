use super::*;
use iceberg_rust::catalog::commit::TableRequirement;
use iceberg_rust::catalog::create::CreateTableBuilder;
use iceberg_rust::spec::{
    manifest::{Content, DataFile, FileFormat},
    schema::Schema,
    types::{PrimitiveType, StructField, Type},
    values::{Struct, Value},
};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Fixture {
    root: tempfile::TempDir,
    catalog: Arc<FileCatalog>,
    tables: [Table; 2],
    commits: [CommitTable; 2],
}

impl Fixture {
    async fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let catalog = Arc::new(
            FileCatalog::new(
                "file:///catalog",
                ObjectStoreBuilder::filesystem(root.path()),
            )
            .await?,
        );
        let schema = Schema::builder()
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_owned(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()?;
        let namespace = Namespace::try_new(&["APP".to_owned()])?;
        catalog.create_namespace(&namespace, None).await?;
        let mut tables = Vec::new();
        let mut commits = Vec::new();
        for name in ["A", "B"] {
            let mut builder = CreateTableBuilder::default();
            let mut table = builder
                .with_name(name)
                .with_schema(schema.clone())
                .build(&namespace, Arc::clone(&catalog) as Arc<dyn Catalog>)
                .await?;
            // The publisher tests metadata/manifest publication. This is deliberately
            // not a Parquet decoding or SQL row acceptance fixture.
            let path = format!("{}/data/input.parquet", table.metadata().location);
            table
                .object_store()
                .put(&strip_prefix(&path).into(), b"PAR1".to_vec().into())
                .await?;
            let file = DataFile::builder()
                .with_content(Content::Data)
                .with_file_path(path)
                .with_file_format(FileFormat::Parquet)
                .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
                .with_record_count(1)
                .with_file_size_in_bytes(4)
                .with_column_sizes(None)
                .with_value_counts(None)
                .with_null_value_counts(None)
                .with_nan_value_counts(None)
                .with_distinct_counts(None)
                .with_lower_bounds(None)
                .with_upper_bounds(None)
                .build()?;
            commits.push(
                table
                    .new_transaction(None)
                    .append_data(vec![file])
                    .prepare_commit()
                    .await?
                    .ok_or("prepared commit")?,
            );
            tables.push(table);
        }
        Ok(Self {
            root,
            catalog,
            tables: tables.try_into().map_err(|_| "two tables")?,
            commits: commits.try_into().map_err(|_| "two commits")?,
        })
    }

    async fn snapshots(&self) -> TestResult<[Option<i64>; 2]> {
        let scope = self.catalog.read_scope().await?;
        let mut snapshots = Vec::new();
        for table in &self.tables {
            let Tabular::Table(table) = scope.load(table.identifier()).await? else {
                return Err("table".into());
            };
            snapshots.push(table.metadata().current_snapshot_id);
        }
        Ok(snapshots.try_into().map_err(|_| "two snapshots")?)
    }
}

#[tokio::test]
async fn publishes_both_native_prepared_heads_and_reopens() -> TestResult {
    let fixture = Fixture::new().await?;
    assert_eq!(fixture.snapshots().await?, [None, None]);
    let prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    let receipt = prepared.publish().await?;
    assert!(!receipt.operation_id.is_nil());
    let snapshots = fixture.snapshots().await?;
    assert!(snapshots.iter().all(Option::is_some));
    for (index, (identifier, metadata)) in receipt.tables.iter().enumerate() {
        assert_eq!(identifier, fixture.tables[index].identifier());
        assert_eq!(
            metadata.table_uuid,
            fixture.tables[index].metadata().table_uuid
        );
        assert_eq!(metadata.current_snapshot_id, snapshots[index]);
    }
    let reopened = Arc::new(
        FileCatalog::new(
            "file:///catalog",
            ObjectStoreBuilder::filesystem(fixture.root.path()),
        )
        .await?,
    );
    let scope = reopened.read_scope().await?;
    for (index, table) in fixture.tables.iter().enumerate() {
        let Tabular::Table(current) = scope.load(table.identifier()).await? else {
            return Err("table".into());
        };
        assert_eq!(current.metadata().current_snapshot_id, snapshots[index]);
        assert_eq!(current.metadata().table_uuid, table.metadata().table_uuid);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_second_requirement_never_publishes_first_head() -> TestResult {
    let fixture = Fixture::new().await?;
    let mut commits = fixture.commits.clone();
    commits[1]
        .requirements
        .push(TableRequirement::AssertTableUuid {
            uuid: Uuid::new_v4(),
        });
    assert!(fixture.catalog.prepare_append_pair(commits).await.is_err());
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn dropping_a_prepared_pair_changes_no_head() -> TestResult {
    let fixture = Fixture::new().await?;
    let prepared = fixture
        .catalog
        .prepare_append_pair(fixture.commits.clone())
        .await?;
    drop(prepared);
    assert_eq!(fixture.snapshots().await?, [None, None]);
    Ok(())
}

#[tokio::test]
async fn reader_scope_excludes_pair_preparation() -> TestResult {
    let fixture = Fixture::new().await?;
    let scope = fixture.catalog.read_scope().await?;
    let catalog = Arc::clone(&fixture.catalog);
    let commits = fixture.commits.clone();
    let mut pending = tokio::spawn(async move { catalog.prepare_append_pair(commits).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    for table in &fixture.tables {
        let Tabular::Table(current) = scope.load(table.identifier()).await? else {
            return Err("table".into());
        };
        assert_eq!(current.metadata().current_snapshot_id, None);
    }
    drop(scope);
    let prepared = tokio::time::timeout(Duration::from_secs(5), pending).await???;
    prepared.publish().await?;
    assert!(fixture.snapshots().await?.iter().all(Option::is_some));
    Ok(())
}
