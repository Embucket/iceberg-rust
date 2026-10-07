use datafusion::common::test_util::batches_to_string;
use datafusion::{
    arrow::{error::ArrowError, record_batch::RecordBatch},
    assert_batches_eq,
    catalog::TableProvider,
    physical_plan::empty::EmptyExec,
    prelude::SessionContext,
};
use datafusion_iceberg::catalog::catalog::IcebergCatalog;
use datafusion_iceberg::table::{DataFusionTable, DataFusionTableConfigBuilder};
use futures::{stream, TryStreamExt};
use iceberg_rust::catalog::identifier::Identifier;
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::spec::namespace::Namespace;
use iceberg_rust::{
    arrow::write::write_equality_deletes_parquet_partitioned,
    catalog::Catalog,
    object_store::ObjectStoreBuilder,
    spec::{
        manifest::{Content, Status},
        partition::{PartitionField, PartitionSpec, Transform},
        schema::Schema,
        types::{PrimitiveType, StructField, Type},
    },
    table::Table,
};
use iceberg_sql_catalog::SqlCatalog;
use object_store::local::LocalFileSystem;
use std::{collections::HashSet, sync::Arc};
use tempfile::TempDir;

async fn run_query(query: &str, ctx: &SessionContext) -> Vec<RecordBatch> {
    ctx.sql(query)
        .await
        .expect("Failed to create plan for query")
        .collect()
        .await
        .expect("Failed to execute query")
}

#[tokio::test]
pub async fn test_equality_delete() {
    let temp_dir = TempDir::new().unwrap();
    let table_dir = format!("{}/test/orders", temp_dir.path().to_str().unwrap());
    let object_store = ObjectStoreBuilder::Filesystem(Arc::new(LocalFileSystem::new()));

    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "warehouse", object_store.clone())
            .await
            .unwrap(),
    );

    catalog
        .create_namespace(&Namespace::try_new(&["test".to_string()]).unwrap(), None)
        .await
        .unwrap();

    let schema = Schema::builder()
        .with_struct_field(StructField {
            id: 1,
            name: "id".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 2,
            name: "customer_id".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 3,
            name: "product_id".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 4,
            name: "date".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Date),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 5,
            name: "amount".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Int),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .build()
        .unwrap();

    let partition_spec = PartitionSpec::builder()
        .with_partition_field(PartitionField::new(4, 1000, "date_day", Transform::Day))
        .build()
        .expect("Failed to create partition spec");

    let table = Table::builder()
        .with_name("orders")
        .with_location(&table_dir)
        .with_schema(schema)
        .with_partition_spec(partition_spec)
        .build(&["test".to_owned()], catalog.clone())
        .await
        .expect("Failed to create table");

    let ctx = SessionContext::new();

    let datafusion_catalog = Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap());

    ctx.register_catalog("warehouse", datafusion_catalog);

    run_query(
        "INSERT INTO warehouse.test.orders (id, customer_id, product_id, date, amount) VALUES
                (1, 1, 1, '2020-01-01', 1),
                (2, 2, 1, '2020-01-01', 1),
                (3, 3, 1, '2020-01-01', 3),
                (4, 1, 2, '2020-02-02', 1),
                (5, 1, 1, '2020-02-02', 2),
                (6, 3, 3, '2020-02-02', 3);",
        &ctx,
    )
    .await;

    let batches = run_query("select * from warehouse.test.orders order by id", &ctx).await;

    let expected = [
        "+----+-------------+------------+------------+--------+",
        "| id | customer_id | product_id | date       | amount |",
        "+----+-------------+------------+------------+--------+",
        "| 1  | 1           | 1          | 2020-01-01 | 1      |",
        "| 2  | 2           | 1          | 2020-01-01 | 1      |",
        "| 3  | 3           | 1          | 2020-01-01 | 3      |",
        "| 4  | 1           | 2          | 2020-02-02 | 1      |",
        "| 5  | 1           | 1          | 2020-02-02 | 2      |",
        "| 6  | 3           | 3          | 2020-02-02 | 3      |",
        "+----+-------------+------------+------------+--------+",
    ];
    assert_batches_eq!(expected, &batches);

    let batches = run_query("SELECT id, customer_id, product_id, date FROM warehouse.test.orders WHERE customer_id = 1 order by id", &ctx).await;

    let expected = [
        "+----+-------------+------------+------------+",
        "| id | customer_id | product_id | date       |",
        "+----+-------------+------------+------------+",
        "| 1  | 1           | 1          | 2020-01-01 |",
        "| 4  | 1           | 2          | 2020-02-02 |",
        "| 5  | 1           | 1          | 2020-02-02 |",
        "+----+-------------+------------+------------+",
    ];
    assert_batches_eq!(expected, &batches);

    let files = write_equality_deletes_parquet_partitioned(
        &table,
        stream::iter(batches.into_iter().map(Ok::<_, ArrowError>)),
        None,
        &[1, 2, 3, 4],
    )
    .await
    .unwrap();

    // Load the latest table version, which includes the inserted rows
    let Tabular::Table(mut table) = catalog
        .clone()
        .load_tabular(&Identifier::new(&["test".to_string()], "orders"))
        .await
        .unwrap()
    else {
        panic!("Tabular should be a table");
    };

    table
        .new_transaction(None)
        .append_delete(files)
        .commit()
        .await
        .unwrap();

    let batches = run_query("select * from warehouse.test.orders order by id", &ctx).await;

    let expected = [
        "+----+-------------+------------+------------+--------+",
        "| id | customer_id | product_id | date       | amount |",
        "+----+-------------+------------+------------+--------+",
        "| 2  | 2           | 1          | 2020-01-01 | 1      |",
        "| 3  | 3           | 1          | 2020-01-01 | 3      |",
        "| 6  | 3           | 3          | 2020-02-02 | 3      |",
        "+----+-------------+------------+------------+--------+",
    ];
    assert_batches_eq!(expected, &batches);

    // Test that projecting a column that is not included in equality deletes works
    run_query(
        "INSERT INTO warehouse.test.orders (id, customer_id, product_id, date, amount) VALUES
                (7, 3, 2, '2020-01-01', 2),
                (8, 2, 1, '2020-02-02', 3),
                (9, 1, 3, '2020-01-01', 1);",
        &ctx,
    )
    .await;

    let batches = run_query("select sum(amount) from warehouse.test.orders", &ctx).await;

    let expected = [
        "+-----------------------------------+",
        "| sum(warehouse.test.orders.amount) |",
        "+-----------------------------------+",
        "| 13                                |",
        "+-----------------------------------+",
    ];
    assert_batches_eq!(expected, &batches);

    // Test that using a filter on a column that is not included in equality deletes works
    let query = "select count(*) from warehouse.test.orders where product_id = 1 and (amount = 1 or customer_id = 3)";
    let batches = run_query(query, &ctx).await;
    let expected = [
        "+----------+",
        "| count(*) |",
        "+----------+",
        "| 2        |",
        "+----------+",
    ];
    assert_batches_eq!(expected, &batches);

    // Ensure we only pushed down predicates that have matching columns with delete file schemas (i.e. amount was not pushed down).
    let batches = run_query(&format!("explain {query}"), &ctx).await;
    assert!(batches_to_string(&batches).contains("projection=[id, customer_id, product_id, date], file_type=parquet, predicate=product_id@2 = 1,"));
}

#[tokio::test]
async fn excluded_data_files_do_not_scan_equality_deletes() {
    let temp_dir = TempDir::new().unwrap();
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new(
            "sqlite://",
            "warehouse",
            ObjectStoreBuilder::Filesystem(Arc::new(LocalFileSystem::new())),
        )
        .await
        .unwrap(),
    );
    catalog
        .create_namespace(&Namespace::try_new(&["test".to_string()]).unwrap(), None)
        .await
        .unwrap();
    let schema = Schema::builder()
        .with_struct_field(StructField {
            id: 1,
            name: "id".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .build()
        .unwrap();
    Table::builder()
        .with_name("excluded")
        .with_location(format!("{}/test/excluded", temp_dir.path().display()))
        .with_schema(schema)
        .build(&["test".to_owned()], catalog.clone())
        .await
        .unwrap();

    let ctx = SessionContext::new();
    ctx.register_catalog(
        "warehouse",
        Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap()),
    );
    run_query("INSERT INTO warehouse.test.excluded VALUES (1), (2)", &ctx).await;
    let delete_rows = run_query("SELECT id FROM warehouse.test.excluded WHERE id = 1", &ctx).await;
    let identifier = Identifier::new(&["test".to_string()], "excluded");
    let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("excluded should be an Iceberg table");
    };
    let delete_files = write_equality_deletes_parquet_partitioned(
        &table,
        stream::iter(delete_rows.into_iter().map(Ok::<_, ArrowError>)),
        None,
        &[1],
    )
    .await
    .unwrap();
    table
        .new_transaction(None)
        .append_delete(delete_files)
        .commit()
        .await
        .unwrap();

    let manifests = table.manifests(None, None).await.unwrap();
    let entries = table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let excluded_paths = entries
        .iter()
        .filter(|(_, entry)| {
            entry.status() != &Status::Deleted && entry.data_file().content() == &Content::Data
        })
        .map(|(_, entry)| entry.data_file().file_path().clone())
        .collect::<HashSet<_>>();
    assert!(!excluded_paths.is_empty());
    let config = DataFusionTableConfigBuilder::default()
        .enable_data_file_path_column(false)
        .enable_data_file_row_position_column(false)
        .enable_manifest_file_path_column(false)
        .excluded_data_file_paths(Arc::new(excluded_paths))
        .build()
        .unwrap();
    let provider = Arc::new(DataFusionTable::new_with_config(
        Tabular::Table(table),
        None,
        None,
        None,
        Some(config),
    ));
    let excluded_ctx = SessionContext::new();
    excluded_ctx
        .register_table("excluded", provider.clone())
        .unwrap();
    let plan = provider
        .scan(&excluded_ctx.state(), None, &[], None)
        .await
        .unwrap();
    assert!(plan.downcast_ref::<EmptyExec>().is_some());
    let batches = run_query("SELECT id FROM excluded", &excluded_ctx).await;
    assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 0);
}
