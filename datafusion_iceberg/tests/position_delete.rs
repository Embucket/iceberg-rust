use std::{collections::HashMap, fs::File, sync::Arc};

use datafusion::{
    arrow::{
        array::{Int64Array, StringArray},
        datatypes::{DataType, Field, Schema as ArrowSchema},
        error::ArrowError,
        record_batch::RecordBatch,
    },
    assert_batches_eq,
    parquet::{
        arrow::{ArrowWriter, PARQUET_FIELD_ID_META_KEY},
        file::properties::WriterProperties,
    },
    prelude::SessionContext,
};
use datafusion_iceberg::{
    catalog::catalog::IcebergCatalog,
    table::{DataFusionTable, DataFusionTableConfigBuilder},
};
use futures::{stream, TryStreamExt};
use iceberg_rust::{
    arrow::write::write_equality_deletes_parquet_partitioned,
    catalog::{identifier::Identifier, tabular::Tabular, Catalog},
    object_store::ObjectStoreBuilder,
    spec::{
        manifest::{Content, DataFile, FileFormat, Status},
        namespace::Namespace,
        partition::{PartitionField, PartitionSpec, Transform},
        puffin::{Blob, PuffinWriter, STANDARD_BLOB_TYPE_DELETION_VECTOR_V1},
        schema::Schema,
        types::{PrimitiveType, StructField, Type},
        values::{Struct, Value},
    },
    table::Table,
};
use iceberg_sql_catalog::SqlCatalog;
use object_store::local::LocalFileSystem;
use roaring::RoaringTreemap;
use tempfile::TempDir;

const FILE_PATH_FIELD_ID: i32 = i32::MAX - 101;
const POS_FIELD_ID: i32 = i32::MAX - 102;

async fn run_query(query: &str, ctx: &SessionContext) -> Vec<RecordBatch> {
    ctx.sql(query)
        .await
        .expect("query planning failed")
        .collect()
        .await
        .expect("query execution failed")
}

fn write_position_delete_file(
    path: &str,
    data_file_path: &str,
    partition: Struct,
    positions: &[i64],
) -> DataFile {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("file_path", DataType::Utf8, false).with_metadata(HashMap::from([(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            FILE_PATH_FIELD_ID.to_string(),
        )])),
        Field::new("pos", DataType::Int64, false).with_metadata(HashMap::from([(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            POS_FIELD_ID.to_string(),
        )])),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![data_file_path; positions.len()])),
            Arc::new(Int64Array::from(positions.to_vec())),
        ],
    )
    .unwrap();

    let file = File::create(path).unwrap();
    let mut writer =
        ArrowWriter::try_new(file, schema, Some(WriterProperties::builder().build())).unwrap();
    writer.write(&batch).unwrap();
    let metadata = writer.close().unwrap();
    let file_size = std::fs::metadata(path).unwrap().len();

    DataFile::builder()
        .with_content(Content::PositionDeletes)
        .with_file_path(path.to_string())
        .with_file_format(FileFormat::Parquet)
        .with_partition(partition)
        .with_record_count(metadata.file_metadata().num_rows())
        .with_file_size_in_bytes(i64::try_from(file_size).unwrap())
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

fn encode_deletion_vector(positions: impl IntoIterator<Item = u64>) -> Vec<u8> {
    const MAGIC: [u8; 4] = [0xD1, 0xD3, 0x39, 0x64];
    let positions = positions.into_iter().collect::<RoaringTreemap>();
    let mut roaring = Vec::with_capacity(positions.serialized_size());
    positions.serialize_into(&mut roaring).unwrap();
    let mut body = MAGIC.to_vec();
    body.extend_from_slice(&roaring);

    let mut blob = Vec::with_capacity(4 + body.len() + 4);
    blob.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
    blob.extend_from_slice(&body);
    blob.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
    blob
}

fn write_deletion_vector_file(path: &str, data_file_path: &str, positions: &[u64]) -> DataFile {
    let vector = encode_deletion_vector(positions.iter().copied());
    let mut writer = PuffinWriter::new();
    writer
        .write_blob(Blob {
            blob_type: STANDARD_BLOB_TYPE_DELETION_VECTOR_V1.to_string(),
            fields: Vec::new(),
            snapshot_id: -1,
            sequence_number: -1,
            compression_codec: None,
            properties: HashMap::new(),
            payload: &vector,
        })
        .unwrap();
    let puffin = writer.finish().unwrap();
    std::fs::write(path, &puffin).unwrap();

    DataFile::builder()
        .with_content(Content::PositionDeletes)
        .with_file_path(path.to_string())
        .with_file_format(FileFormat::Puffin)
        .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
        .with_record_count(i64::try_from(positions.len()).unwrap())
        .with_file_size_in_bytes(i64::try_from(puffin.len()).unwrap())
        .with_column_sizes(None)
        .with_value_counts(None)
        .with_null_value_counts(None)
        .with_nan_value_counts(None)
        .with_distinct_counts(None)
        .with_lower_bounds(None)
        .with_upper_bounds(None)
        .with_referenced_data_file(Some(data_file_path.to_string()))
        .with_content_offset(Some(4))
        .with_content_size_in_bytes(Some(i64::try_from(vector.len()).unwrap()))
        .build()
        .unwrap()
}

#[tokio::test]
async fn applies_v2_position_deletes() {
    let temp_dir = TempDir::new().unwrap();
    let table_dir = format!("{}/test/orders", temp_dir.path().display());
    let object_store = ObjectStoreBuilder::Filesystem(Arc::new(LocalFileSystem::new()));
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "warehouse", object_store)
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
            name: "payload".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::String),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 3,
            name: "__data_file_path".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::Long),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 4,
            name: "__iceberg_file_row_position".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::String),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 5,
            name: "__iceberg_data_sequence_number".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::String),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .with_struct_field(StructField {
            id: 6,
            name: "category".to_string(),
            required: true,
            field_type: Type::Primitive(PrimitiveType::String),
            doc: None,
            initial_default: None,
            write_default: None,
        })
        .build()
        .unwrap();

    let partition_spec = PartitionSpec::builder()
        .with_partition_field(PartitionField::new(
            6,
            1000,
            "category",
            Transform::Identity,
        ))
        .build()
        .unwrap();

    Table::builder()
        .with_name("orders")
        .with_location(&table_dir)
        .with_schema(schema)
        .with_partition_spec(partition_spec)
        .build(&["test".to_owned()], catalog.clone())
        .await
        .unwrap();

    let ctx = SessionContext::new();
    ctx.register_catalog(
        "warehouse",
        Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap()),
    );

    run_query(
        "INSERT INTO warehouse.test.orders VALUES
            (1, 'one', 101, 'row-one', 'seq-one', 'a'),
            (2, 'two', 102, 'row-two', 'seq-two', 'a'),
            (3, 'three', 103, 'row-three', 'seq-three', 'a'),
            (4, 'four', 104, 'row-four', 'seq-four', 'a'),
            (5, 'five', 105, 'row-five', 'seq-five', 'a'),
            (6, 'six', 106, 'row-six', 'seq-six', 'a')",
        &ctx,
    )
    .await;

    let identifier = Identifier::new(&["test".to_string()], "orders");
    let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("orders should be an Iceberg table");
    };
    let manifests = table.manifests(None, None).await.unwrap();
    let data_files = table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let (data_manifest_path, data_manifest_entry) = data_files
        .iter()
        .find(|(_, entry)| {
            entry.status() != &Status::Deleted && entry.data_file().content() == &Content::Data
        })
        .unwrap();
    let data_file_path = data_manifest_entry.data_file().file_path().clone();
    let partition = data_manifest_entry.data_file().partition().clone();

    let delete_dir = format!("{table_dir}/data");
    std::fs::create_dir_all(&delete_dir).unwrap();
    let delete_files = vec![
        write_position_delete_file(
            &format!("{delete_dir}/position-delete-1.parquet"),
            &data_file_path,
            partition.clone(),
            &[1, 4],
        ),
        write_position_delete_file(
            &format!("{delete_dir}/position-delete-2.parquet"),
            &data_file_path,
            partition,
            &[4, 5],
        ),
    ];

    table
        .new_transaction(None)
        .append_delete(delete_files)
        .commit()
        .await
        .unwrap();

    let Tabular::Table(table_with_manifest_metadata) =
        catalog.clone().load_tabular(&identifier).await.unwrap()
    else {
        panic!("orders should be an Iceberg table");
    };
    let metadata_config = DataFusionTableConfigBuilder::default()
        .enable_data_file_path_column(false)
        .enable_data_file_row_position_column(false)
        .enable_manifest_file_path_column(true)
        .build()
        .unwrap();
    let metadata_ctx = SessionContext::new();
    metadata_ctx
        .register_table(
            "orders_with_manifest_metadata",
            Arc::new(DataFusionTable::new_with_config(
                Tabular::Table(table_with_manifest_metadata),
                None,
                None,
                None,
                Some(metadata_config),
            )),
        )
        .unwrap();
    let metadata_batches = run_query(
        "SELECT __manifest_file_path FROM orders_with_manifest_metadata LIMIT 1",
        &metadata_ctx,
    )
    .await;
    let manifest_paths = metadata_batches[0]
        .column_by_name("__manifest_file_path")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(manifest_paths.value(0), data_manifest_path);

    let metadata_batches = run_query(
        "SELECT id, payload FROM orders_with_manifest_metadata ORDER BY id",
        &metadata_ctx,
    )
    .await;
    assert_batches_eq!(
        [
            "+----+---------+",
            "| id | payload |",
            "+----+---------+",
            "| 1  | one     |",
            "| 3  | three   |",
            "| 4  | four    |",
            "+----+---------+",
        ],
        &metadata_batches
    );

    let batches = run_query(
        "SELECT id, payload FROM warehouse.test.orders ORDER BY id",
        &ctx,
    )
    .await;
    assert_batches_eq!(
        [
            "+----+---------+",
            "| id | payload |",
            "+----+---------+",
            "| 1  | one     |",
            "| 3  | three   |",
            "| 4  | four    |",
            "+----+---------+",
        ],
        &batches
    );

    let batches = run_query(
        "SELECT __iceberg_data_sequence_number, id, __data_file_path,
                __iceberg_file_row_position
         FROM warehouse.test.orders ORDER BY id",
        &ctx,
    )
    .await;
    assert_batches_eq!(
        [
            "+--------------------------------+----+------------------+-----------------------------+",
            "| __iceberg_data_sequence_number | id | __data_file_path | __iceberg_file_row_position |",
            "+--------------------------------+----+------------------+-----------------------------+",
            "| seq-one                        | 1  | 101              | row-one                     |",
            "| seq-three                      | 3  | 103              | row-three                   |",
            "| seq-four                       | 4  | 104              | row-four                    |",
            "+--------------------------------+----+------------------+-----------------------------+",
        ],
        &batches
    );

    let batches = run_query(
        "SELECT id FROM warehouse.test.orders WHERE id IN (2, 3, 5) ORDER BY id",
        &ctx,
    )
    .await;
    assert_batches_eq!(
        ["+----+", "| id |", "+----+", "| 3  |", "+----+",],
        &batches
    );

    run_query(
        "INSERT INTO warehouse.test.orders VALUES
            (7, 'seven', 107, 'row-seven', 'seq-seven', 'a'),
            (8, 'eight', 108, 'row-eight', 'seq-eight', 'a')",
        &ctx,
    )
    .await;
    let batches = run_query(
        "SELECT id FROM warehouse.test.orders WHERE id >= 5 ORDER BY id",
        &ctx,
    )
    .await;
    assert_batches_eq!(
        ["+----+", "| id |", "+----+", "| 7  |", "| 8  |", "+----+",],
        &batches
    );

    let equality_rows = run_query(
        "SELECT id, category FROM warehouse.test.orders WHERE id IN (3, 7)",
        &ctx,
    )
    .await;
    let equality_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("id", DataType::Int64, false).with_metadata(HashMap::from([(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            "1".to_string(),
        )])),
        Field::new("category", DataType::Utf8, false).with_metadata(HashMap::from([(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            "6".to_string(),
        )])),
    ]));
    let equality_rows = equality_rows
        .into_iter()
        .map(|batch| RecordBatch::try_new(equality_schema.clone(), batch.columns().to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("orders should be an Iceberg table");
    };
    let equality_files = write_equality_deletes_parquet_partitioned(
        &table,
        stream::iter(equality_rows.into_iter().map(Ok::<_, ArrowError>)),
        None,
        &[1, 6],
    )
    .await
    .unwrap();
    table
        .new_transaction(None)
        .append_delete(equality_files)
        .commit()
        .await
        .unwrap();

    let batches = run_query("SELECT id FROM warehouse.test.orders ORDER BY id", &ctx).await;
    assert_batches_eq!(
        ["+----+", "| id |", "+----+", "| 1  |", "| 4  |", "| 8  |", "+----+",],
        &batches
    );
}

#[tokio::test]
async fn applies_v3_puffin_deletion_vector() {
    let temp_dir = TempDir::new().unwrap();
    let table_dir = format!("{}/test/orders_v3", temp_dir.path().display());
    let object_store = ObjectStoreBuilder::Filesystem(Arc::new(LocalFileSystem::new()));
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "warehouse", object_store)
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
        .with_name("orders_v3")
        .with_location(&table_dir)
        .with_schema(schema)
        .with_property(("format-version".to_string(), "3".to_string()))
        .build(&["test".to_owned()], catalog.clone())
        .await
        .unwrap();

    let ctx = SessionContext::new();
    ctx.register_catalog(
        "warehouse",
        Arc::new(IcebergCatalog::new(catalog.clone(), None).await.unwrap()),
    );
    run_query(
        "INSERT INTO warehouse.test.orders_v3 VALUES (10), (20), (30), (40), (50)",
        &ctx,
    )
    .await;

    let identifier = Identifier::new(&["test".to_string()], "orders_v3");
    let Tabular::Table(mut table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("orders_v3 should be an Iceberg table");
    };
    let manifests = table.manifests(None, None).await.unwrap();
    let data_files = table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let data_file_path = data_files
        .iter()
        .find(|(_, entry)| {
            entry.status() != &Status::Deleted && entry.data_file().content() == &Content::Data
        })
        .unwrap()
        .1
        .data_file()
        .file_path()
        .clone();

    let delete_dir = format!("{table_dir}/data");
    std::fs::create_dir_all(&delete_dir).unwrap();
    let deletion_vector = write_deletion_vector_file(
        &format!("{delete_dir}/deletion-vector.puffin"),
        &data_file_path,
        &[1, 3],
    );
    table
        .new_transaction(None)
        .append_delete(vec![deletion_vector])
        .commit()
        .await
        .unwrap();

    let batches = run_query("SELECT id FROM warehouse.test.orders_v3 ORDER BY id", &ctx).await;
    assert_batches_eq!(
        ["+----+", "| id |", "+----+", "| 10 |", "| 30 |", "| 50 |", "+----+",],
        &batches
    );
}
