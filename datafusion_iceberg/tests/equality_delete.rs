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
        values::Value,
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

    // A delete writer may report metrics for non-key columns. They cannot be
    // used to prune an equality delete: the row matched by its key can have a
    // different value in the data file.
    let files = files
        .into_iter()
        .map(|file| {
            let mut lower = file.lower_bounds().clone().unwrap_or_default();
            lower.insert(5, Value::Int(999));
            let mut upper = file.upper_bounds().clone().unwrap_or_default();
            upper.insert(5, Value::Int(999));
            iceberg_rust::spec::manifest::DataFile::builder()
                .with_content(file.content().clone())
                .with_file_path(file.file_path().clone())
                .with_file_format(file.file_format().clone())
                .with_partition(file.partition().clone())
                .with_record_count(*file.record_count())
                .with_file_size_in_bytes(*file.file_size_in_bytes())
                .with_column_sizes(file.column_sizes().clone())
                .with_value_counts(file.value_counts().clone())
                .with_null_value_counts(file.null_value_counts().clone())
                .with_nan_value_counts(file.nan_value_counts().clone())
                .with_distinct_counts(file.distinct_counts().clone())
                .with_lower_bounds(Some(lower))
                .with_upper_bounds(Some(upper))
                .with_key_metadata(file.key_metadata().clone())
                .with_split_offsets(file.split_offsets().clone())
                .with_equality_ids(file.equality_ids().clone())
                .with_sort_order_id(*file.sort_order_id())
                .with_first_row_id(*file.first_row_id())
                .with_referenced_data_file(file.referenced_data_file().clone())
                .with_content_offset(*file.content_offset())
                .with_content_size_in_bytes(*file.content_size_in_bytes())
                .build()
                .unwrap()
        })
        .collect::<Vec<_>>();

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

    let batches = run_query(
        "select id from warehouse.test.orders where amount = 1 order by id",
        &ctx,
    )
    .await;
    assert_batches_eq!(["+----+", "| id |", "+----+", "| 2  |", "+----+"], &batches);

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

async fn run_nullable_key_delete(equality_ids: &[i32], delete_query: &str) -> Vec<RecordBatch> {
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
        .with_struct_field(StructField::new(
            1,
            "id",
            true,
            Type::Primitive(PrimitiveType::Long),
            None,
        ))
        .with_struct_field(StructField::new(
            2,
            "key_a",
            false,
            Type::Primitive(PrimitiveType::Long),
            None,
        ))
        .with_struct_field(StructField::new(
            3,
            "key_b",
            false,
            Type::Primitive(PrimitiveType::Long),
            None,
        ))
        .build()
        .unwrap();
    Table::builder()
        .with_name("nullable_keys")
        .with_location(format!("{}/test/nullable_keys", temp_dir.path().display()))
        .with_schema(schema)
        .build(&["test".to_owned()], catalog.clone())
        .await
        .unwrap();

    let ctx = SessionContext::new();
    ctx.register_catalog(
        "warehouse",
        Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap()),
    );
    run_query(
        "INSERT INTO warehouse.test.nullable_keys VALUES
            (1, NULL, 10),
            (2, NULL, 20),
            (3, 10, NULL),
            (4, 20, NULL),
            (5, NULL, NULL),
            (6, 10, 10),
            (7, 20, 20),
            (8, 30, 30)",
        &ctx,
    )
    .await;
    let delete_rows = run_query(delete_query, &ctx).await;
    let identifier = Identifier::new(&["test".to_string()], "nullable_keys");
    let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("nullable_keys should be an Iceberg table");
    };
    let delete_files = write_equality_deletes_parquet_partitioned(
        &table,
        stream::iter(delete_rows.into_iter().map(Ok::<_, ArrowError>)),
        None,
        equality_ids,
    )
    .await
    .unwrap();
    assert!(!delete_files.is_empty());
    table
        .new_transaction(None)
        .append_delete(delete_files)
        .commit()
        .await
        .unwrap();

    run_query(
        "SELECT id FROM warehouse.test.nullable_keys ORDER BY id",
        &ctx,
    )
    .await
}

#[tokio::test]
async fn test_equality_delete_null_key() {
    let batches = run_nullable_key_delete(
        &[2],
        "SELECT key_a FROM warehouse.test.nullable_keys WHERE id = 1",
    )
    .await;
    assert_batches_eq!(
        [
            "+----+", "| id |", "+----+", "| 3  |", "| 4  |", "| 6  |", "| 7  |", "| 8  |",
            "+----+",
        ],
        &batches
    );
}

#[tokio::test]
async fn test_equality_delete_composite_null_keys() {
    let batches = run_nullable_key_delete(
        &[2, 3],
        "SELECT key_a, key_b FROM warehouse.test.nullable_keys WHERE id IN (1, 3, 5, 7)",
    )
    .await;
    assert_batches_eq!(
        ["+----+", "| id |", "+----+", "| 2  |", "| 4  |", "| 6  |", "| 8  |", "+----+",],
        &batches
    );
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

#[tokio::test]
#[ignore = "manual equality-delete benchmark; run alone with --ignored --nocapture --test-threads=1"]
async fn benchmark_equality_delete_null_comparison() {
    use datafusion::{
        arrow::{
            array::{Array, Int64Array, StringArray},
            datatypes::{DataType, Field, Schema as ArrowSchema},
        },
        datasource::MemTable,
        physical_plan::{collect, ExecutionPlan},
        prelude::SessionConfig,
    };
    use std::time::Instant;

    const ROWS: i64 = 100_000;
    const WARMUPS: usize = 2;
    const SAMPLES: usize = 7;

    fn metric_sum(plan: &dyn ExecutionPlan, name: &str) -> usize {
        plan.metrics()
            .and_then(|metrics| metrics.sum_by_name(name))
            .map_or(0, |value| value.as_usize())
            + plan
                .children()
                .into_iter()
                .map(|child| metric_sum(child.as_ref(), name))
                .sum::<usize>()
    }

    let assert_count = |batches: &[RecordBatch], expected: i64| {
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_columns(), 1);
        assert_eq!(batches[0].num_rows(), 1);
        let count = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("COUNT(*) must return Int64");
        assert_eq!(count.null_count(), 0);
        assert_eq!(count.value(0), expected);
    };
    let data_cache_mb = std::env::var("ICEBERG_PARQUET_DATA_CACHE_MB")
        .ok()
        .map(|value| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    assert_eq!(data_cache_mb, 0, "benchmark requires the data cache off");

    for (null_percent, expected_count, expected_delete_keys) in
        [(0, 99_000_i64, 100_usize), (5, 99_050, 95)]
    {
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
            .with_struct_field(StructField::new(
                1,
                "id",
                true,
                Type::Primitive(PrimitiveType::Long),
                None,
            ))
            .with_struct_field(StructField::new(
                2,
                "delete_key",
                false,
                Type::Primitive(PrimitiveType::Long),
                None,
            ))
            .build()
            .unwrap();
        Table::builder()
            .with_name("equality_bench")
            .with_location(format!("{}/test/equality_bench", temp_dir.path().display()))
            .with_schema(schema)
            .build(&["test".to_owned()], catalog.clone())
            .await
            .unwrap();
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        ctx.register_catalog(
            "warehouse",
            Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap()),
        );

        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("delete_key", DataType::Int64, true),
        ]));
        let keys = Int64Array::from_iter((0..ROWS).map(|id| {
            if null_percent == 5 && id % 20 == 0 {
                None
            } else {
                Some(id % 10_000)
            }
        }));
        assert_eq!(keys.null_count(), if null_percent == 5 { 5_000 } else { 0 });
        let batch = RecordBatch::try_new(
            arrow_schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..ROWS)),
                Arc::new(keys),
            ],
        )
        .unwrap();
        ctx.register_table(
            "benchmark_input",
            Arc::new(MemTable::try_new(arrow_schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        run_query(
            "INSERT INTO warehouse.test.equality_bench
             SELECT id, delete_key FROM benchmark_input",
            &ctx,
        )
        .await;
        ctx.deregister_table("benchmark_input").unwrap();
        let query = "SELECT COUNT(*) AS row_count FROM warehouse.test.equality_bench";
        assert_count(&run_query(query, &ctx).await, ROWS);

        let delete_rows = run_query(
            "SELECT DISTINCT delete_key FROM warehouse.test.equality_bench
             WHERE delete_key < 100 AND delete_key IS NOT NULL",
            &ctx,
        )
        .await;
        assert_eq!(
            delete_rows.iter().map(RecordBatch::num_rows).sum::<usize>(),
            expected_delete_keys,
        );
        for batch in &delete_rows {
            let keys = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!(keys.null_count(), 0);
            assert!(keys.values().iter().all(|key| (0..100).contains(key)));
        }
        let identifier = Identifier::new(&["test".to_string()], "equality_bench");
        let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap()
        else {
            panic!("equality_bench should be an Iceberg table");
        };
        let delete_files = write_equality_deletes_parquet_partitioned(
            &table,
            stream::iter(delete_rows.into_iter().map(Ok::<_, ArrowError>)),
            None,
            &[2],
        )
        .await
        .unwrap();
        assert!(!delete_files.is_empty());
        table
            .new_transaction(None)
            .append_delete(delete_files)
            .commit()
            .await
            .unwrap();

        let explain = run_query(&format!("EXPLAIN {query}"), &ctx).await;
        let physical_plan = explain
            .iter()
            .find_map(|batch| {
                let kinds = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                let plans = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                (0..batch.num_rows())
                    .find(|&row| kinds.value(row) == "physical_plan")
                    .map(|row| plans.value(row).to_owned())
            })
            .expect("EXPLAIN must include a physical plan");
        assert!(
            physical_plan.lines().any(|line| {
                line.contains("HashJoinExec:") && line.contains("join_type=RightAnti")
            }),
            "benchmark must execute the native equality-delete join: {physical_plan}",
        );

        let mut samples_ns = Vec::with_capacity(SAMPLES);
        let mut metadata_hits = Vec::with_capacity(SAMPLES);
        for iteration in 0..WARMUPS + SAMPLES {
            let plan = ctx
                .sql(query)
                .await
                .unwrap()
                .create_physical_plan()
                .await
                .unwrap();
            let execution_plan = plan.clone();
            let task_ctx = ctx.task_ctx();
            let start = Instant::now();
            let batches = collect(execution_plan, task_ctx).await.unwrap();
            let elapsed = start.elapsed();
            assert_count(&batches, expected_count);
            if iteration < WARMUPS {
                continue;
            }
            let hits = metric_sum(plan.as_ref(), "parquet_metadata_cache_hits");
            assert!(hits >= 2, "both data and delete footers must be cached");
            assert_eq!(
                metric_sum(plan.as_ref(), "parquet_metadata_cache_misses"),
                0
            );
            assert_eq!(metric_sum(plan.as_ref(), "parquet_data_cache_hits"), 0);
            assert_eq!(metric_sum(plan.as_ref(), "parquet_data_cache_misses"), 0);
            samples_ns.push(u64::try_from(elapsed.as_nanos()).unwrap());
            metadata_hits.push(hits);
        }
        println!(
            "{}",
            serde_json::json!({
                "benchmark": "equality_delete_null_comparison",
                "input_rows": ROWS,
                "null_percent": null_percent,
                "delete_keys": expected_delete_keys,
                "expected_count": expected_count,
                "warmups": WARMUPS,
                "samples_ns": samples_ns,
                "timing_scope": "physical_execution_only_fresh_plan",
                "target_partitions": 1,
                "data_cache_mb": data_cache_mb,
                "metadata_cache_hits": metadata_hits,
                "physical_plan": physical_plan,
            })
        );
    }
}
