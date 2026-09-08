use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayRef, Int32Array, Int64Array, MapArray, StringArray, StructArray,
};
use datafusion::arrow::buffer::OffsetBuffer;
use datafusion::arrow::datatypes::{DataType, Field, Fields, Schema as ArrowSchema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::dataframe::DataFrameWriteOptions;
use datafusion::prelude::SessionContext;
use datafusion_iceberg::catalog::catalog::IcebergCatalog;
use futures::TryStreamExt;
use iceberg_rust::catalog::identifier::Identifier;
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::catalog::Catalog;
use iceberg_rust::object_store::ObjectStoreBuilder;
use iceberg_rust::spec::namespace::Namespace;
use iceberg_rust::spec::schema::Schema;
use iceberg_rust::spec::types::{MapType, PrimitiveType, StructField, Type};
use iceberg_rust::table::Table;
use iceberg_sql_catalog::SqlCatalog;

#[tokio::test]
async fn writes_and_reads_arrow_map() {
    let object_store = ObjectStoreBuilder::memory();
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "test", object_store)
            .await
            .unwrap(),
    );
    catalog
        .create_namespace(&Namespace::try_new(&["public".to_string()]).unwrap(), None)
        .await
        .unwrap();
    let identifier = Identifier::new(&["public".to_string()], "map_roundtrip");
    Table::builder()
        .with_name("map_roundtrip")
        .with_location("/map_roundtrip")
        .with_schema(
            Schema::builder()
                .with_struct_field(StructField::new(
                    1,
                    "id",
                    false,
                    Type::Primitive(PrimitiveType::Int),
                    None,
                ))
                .with_struct_field(StructField::new(
                    2,
                    "attrs",
                    false,
                    Type::Map(MapType {
                        key_id: 3,
                        key: Box::new(Type::Primitive(PrimitiveType::String)),
                        value_id: 4,
                        value: Box::new(Type::Primitive(PrimitiveType::Long)),
                        value_required: false,
                    }),
                    None,
                ))
                .build()
                .unwrap(),
        )
        .build(identifier.namespace(), Arc::clone(&catalog))
        .await
        .unwrap();

    let entry_fields = Fields::from(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Int64, true),
    ]);
    let entries = StructArray::try_new(
        entry_fields.clone(),
        vec![
            Arc::new(StringArray::from(vec!["alpha", "beta"])) as ArrayRef,
            Arc::new(Int64Array::from(vec![10, 20])) as ArrayRef,
        ],
        None,
    )
    .unwrap();
    let entries_field = Arc::new(Field::new("entries", DataType::Struct(entry_fields), false));
    let attrs = Arc::new(
        MapArray::try_new(
            Arc::clone(&entries_field),
            OffsetBuffer::new(vec![0, 2, 2].into()),
            entries,
            None,
            false,
        )
        .unwrap(),
    ) as ArrayRef;
    let batch = RecordBatch::try_new(
        Arc::new(ArrowSchema::new(vec![
            Field::new("id", DataType::Int32, true),
            Field::new("attrs", DataType::Map(entries_field, false), true),
        ])),
        vec![Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef, attrs],
    )
    .unwrap();

    let ctx = SessionContext::new();
    ctx.register_catalog(
        "warehouse",
        Arc::new(
            IcebergCatalog::new(Arc::clone(&catalog), None)
                .await
                .unwrap(),
        ),
    );
    ctx.read_batch(batch)
        .unwrap()
        .write_table(
            "warehouse.public.map_roundtrip",
            DataFrameWriteOptions::default(),
        )
        .await
        .unwrap();

    let Tabular::Table(table) = catalog.clone().load_tabular(&identifier).await.unwrap() else {
        panic!("expected table");
    };
    assert_eq!(table.metadata().last_column_id, 4);
    let manifests = table.manifests(None, None).await.unwrap();
    let data_files = table
        .datafiles(&manifests, None, (None, None))
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        data_files[0]
            .1
            .data_file()
            .null_value_counts()
            .as_ref()
            .unwrap()
            .get(&2),
        None
    );

    let batches = ctx
        .sql("SELECT id, attrs FROM warehouse.public.map_roundtrip ORDER BY id")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let attrs = batches[0]
        .column(1)
        .as_any()
        .downcast_ref::<MapArray>()
        .unwrap();
    assert_eq!(attrs.value_offsets(), &[0, 2, 2]);
    assert_eq!(attrs.null_count(), 0);
}
