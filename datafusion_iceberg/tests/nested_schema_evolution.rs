use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, Int32Array, ListArray, MapArray, StringArray, StructArray,
};
use datafusion::arrow::buffer::OffsetBuffer;
use datafusion::arrow::datatypes::{DataType, Field, Fields, Schema as ArrowSchema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::dataframe::DataFrameWriteOptions;
use datafusion::prelude::SessionContext;
use datafusion_iceberg::catalog::catalog::IcebergCatalog;
use iceberg_rust::catalog::commit::{CommitTable, TableRequirement, TableUpdate};
use iceberg_rust::catalog::identifier::Identifier;
use iceberg_rust::catalog::tabular::Tabular;
use iceberg_rust::catalog::Catalog;
use iceberg_rust::object_store::ObjectStoreBuilder;
use iceberg_rust::spec::namespace::Namespace;
use iceberg_rust::spec::schema::Schema;
use iceberg_rust::spec::types::{ListType, MapType, PrimitiveType, StructField, StructType, Type};
use iceberg_rust::table::Table;
use iceberg_sql_catalog::SqlCatalog;

fn nested_fields(old_names: bool, extra_id: i32) -> StructType {
    let mut fields = vec![
        StructField::new(
            extra_id - 2,
            if old_names { "old" } else { "renamed" },
            true,
            Type::Primitive(PrimitiveType::Int),
            None,
        ),
        StructField::new(
            extra_id - 1,
            if old_names { "other" } else { "other_renamed" },
            true,
            Type::Primitive(PrimitiveType::Int),
            None,
        ),
    ];
    if !old_names {
        fields.reverse();
        fields.push(StructField::new(
            extra_id,
            "added",
            false,
            Type::Primitive(PrimitiveType::Int),
            None,
        ));
    }
    StructType::new(fields)
}

fn iceberg_schema(old_names: bool) -> Schema {
    let fields = StructType::new(vec![
        StructField::new(1, "id", true, Type::Primitive(PrimitiveType::Int), None),
        StructField::new(
            2,
            "items",
            true,
            Type::List(ListType {
                element_id: 3,
                element_required: true,
                element: Box::new(Type::Struct(nested_fields(old_names, 6))),
            }),
            None,
        ),
        StructField::new(
            7,
            "attrs",
            true,
            Type::Map(MapType {
                key_id: 8,
                key: Box::new(Type::Primitive(PrimitiveType::String)),
                value_id: 9,
                value_required: true,
                value: Box::new(Type::Struct(nested_fields(old_names, 12))),
            }),
            None,
        ),
    ]);
    Schema::from_struct_type(fields, if old_names { 0 } else { 1 }, None)
}

fn old_batch() -> RecordBatch {
    let list_fields = Fields::from(vec![
        Field::new("old", DataType::Int32, false),
        Field::new("other", DataType::Int32, false),
    ]);
    let list_values = StructArray::try_new(
        list_fields.clone(),
        vec![
            Arc::new(Int32Array::from(vec![1, 2])),
            Arc::new(Int32Array::from(vec![10, 20])),
        ],
        None,
    )
    .unwrap();
    let element = Arc::new(Field::new("element", DataType::Struct(list_fields), false));
    let items = Arc::new(
        ListArray::try_new(
            Arc::clone(&element),
            OffsetBuffer::from_lengths([2]),
            Arc::new(list_values),
            None,
        )
        .unwrap(),
    ) as ArrayRef;

    let map_value_fields = Fields::from(vec![
        Field::new("old", DataType::Int32, false),
        Field::new("other", DataType::Int32, false),
    ]);
    let map_values = StructArray::try_new(
        map_value_fields.clone(),
        vec![
            Arc::new(Int32Array::from(vec![3])),
            Arc::new(Int32Array::from(vec![30])),
        ],
        None,
    )
    .unwrap();
    let entries_fields = Fields::from(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Struct(map_value_fields), false),
    ]);
    let entries = StructArray::try_new(
        entries_fields.clone(),
        vec![Arc::new(StringArray::from(vec!["k"])), Arc::new(map_values)],
        None,
    )
    .unwrap();
    let entry = Arc::new(Field::new(
        "entries",
        DataType::Struct(entries_fields),
        false,
    ));
    let attrs = Arc::new(
        MapArray::try_new(
            Arc::clone(&entry),
            OffsetBuffer::from_lengths([1]),
            entries,
            None,
            false,
        )
        .unwrap(),
    ) as ArrayRef;

    RecordBatch::try_new(
        Arc::new(ArrowSchema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("items", DataType::List(element), false),
            Field::new("attrs", DataType::Map(entry, false), false),
        ])),
        vec![
            Arc::new(Int32Array::from(vec![1])) as ArrayRef,
            items,
            attrs,
        ],
    )
    .unwrap()
}

#[tokio::test]
async fn parquet_backed_list_and_map_follow_nested_field_ids() {
    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "test", ObjectStoreBuilder::memory())
            .await
            .unwrap(),
    );
    catalog
        .create_namespace(&Namespace::try_new(&["public".to_owned()]).unwrap(), None)
        .await
        .unwrap();
    let identifier = Identifier::new(&["public".to_owned()], "nested_evolution");
    Table::builder()
        .with_name("nested_evolution")
        .with_location("/nested_evolution")
        .with_schema(iceberg_schema(true))
        .build(identifier.namespace(), Arc::clone(&catalog))
        .await
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
    ctx.read_batch(old_batch())
        .unwrap()
        .write_table(
            "warehouse.public.nested_evolution",
            DataFrameWriteOptions::default(),
        )
        .await
        .unwrap();

    let Tabular::Table(table) = Arc::clone(&catalog)
        .load_tabular(&identifier)
        .await
        .unwrap()
    else {
        panic!("expected table");
    };
    catalog
        .update_table(CommitTable {
            identifier,
            requirements: vec![TableRequirement::AssertCurrentSchemaId {
                current_schema_id: *table.current_schema().unwrap().schema_id(),
            }],
            updates: vec![
                TableUpdate::AddSchema {
                    schema: iceberg_schema(false),
                    last_column_id: Some(12),
                },
                TableUpdate::SetCurrentSchema { schema_id: -1 },
            ],
        })
        .await
        .unwrap();

    let batches = ctx
        .sql("SELECT items, attrs FROM warehouse.public.nested_evolution")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 1);
    let items = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap();
    assert_eq!(items.value_offsets(), &[0, 2]);
    let list_values = items.values();
    let list_values = list_values.as_any().downcast_ref::<StructArray>().unwrap();
    assert_eq!(
        list_values.column_names(),
        vec!["other_renamed", "renamed", "added"]
    );
    assert_eq!(
        list_values
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .as_ref(),
        &[10, 20]
    );
    assert_eq!(
        list_values
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .values()
            .as_ref(),
        &[1, 2]
    );
    assert_eq!(list_values.column(2).null_count(), 2);

    let attrs = batches[0]
        .column(1)
        .as_any()
        .downcast_ref::<MapArray>()
        .unwrap();
    assert_eq!(attrs.value_offsets(), &[0, 1]);
    assert_eq!(
        attrs
            .entries()
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0),
        "k"
    );
    let map_values = attrs
        .entries()
        .column(1)
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    assert_eq!(
        map_values.column_names(),
        vec!["other_renamed", "renamed", "added"]
    );
    assert_eq!(
        map_values
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0),
        30
    );
    assert_eq!(
        map_values
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0),
        3
    );
    assert_eq!(map_values.column(2).null_count(), 1);
}
