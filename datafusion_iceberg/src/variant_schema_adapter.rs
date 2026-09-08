use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, MapArray, StructArray};
use datafusion::arrow::datatypes::{DataType, FieldRef, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion::common::{internal_err, Result};
use datafusion::physical_expr::utils::collect_columns;
use datafusion::physical_expr_adapter::{
    DefaultPhysicalExprAdapter, PhysicalExprAdapter, PhysicalExprAdapterFactory,
};
use datafusion::physical_plan::expressions::Column;
use datafusion::physical_plan::PhysicalExpr;
use datafusion_expr::ColumnarValue;
use parquet_variant_compute::{unshred_variant, VariantArray};

const PARQUET_VARIANT_EXTENSION_NAME: &str = "arrow.parquet.variant";

#[derive(Debug)]
pub(crate) struct IcebergPhysicalExprAdapterFactory;

impl PhysicalExprAdapterFactory for IcebergPhysicalExprAdapterFactory {
    fn create(
        &self,
        logical_file_schema: SchemaRef,
        physical_file_schema: SchemaRef,
    ) -> Result<Arc<dyn PhysicalExprAdapter>> {
        Ok(Arc::new(IcebergPhysicalExprAdapter {
            default: DefaultPhysicalExprAdapter::new(
                Arc::clone(&logical_file_schema),
                Arc::clone(&physical_file_schema),
            ),
            logical_file_schema,
            physical_file_schema,
        }))
    }
}

#[derive(Debug)]
struct IcebergPhysicalExprAdapter {
    default: DefaultPhysicalExprAdapter,
    logical_file_schema: SchemaRef,
    physical_file_schema: SchemaRef,
}

impl IcebergPhysicalExprAdapter {
    fn is_logical_variant_column(&self, name: &str) -> bool {
        self.logical_file_schema
            .field_with_name(name)
            .is_ok_and(|field| field.extension_type_name() == Some(PARQUET_VARIANT_EXTENSION_NAME))
    }

    fn rewrite_variant_column(&self, column: &Column) -> Result<Arc<dyn PhysicalExpr>> {
        let logical_field = self.logical_file_schema.field_with_name(column.name())?;
        let physical_index = self.physical_file_schema.index_of(column.name())?;
        let physical_field = self.physical_file_schema.field(physical_index);

        if !is_variant_storage(physical_field.data_type()) {
            return internal_err!(
                "Iceberg Variant column '{}' has incompatible physical type {}",
                column.name(),
                physical_field.data_type()
            );
        }

        Ok(Arc::new(UnshredVariantExpr::new(
            Arc::new(Column::new(column.name(), physical_index)),
            Arc::new(logical_field.clone()),
        )))
    }

    fn is_logical_map_column(&self, name: &str) -> bool {
        self.logical_file_schema
            .field_with_name(name)
            .is_ok_and(|field| matches!(field.data_type(), DataType::Map(_, _)))
    }

    fn rewrite_map_column(&self, column: &Column) -> Result<Option<Arc<dyn PhysicalExpr>>> {
        let logical_field = self.logical_file_schema.field_with_name(column.name())?;
        let physical_index = self.physical_file_schema.index_of(column.name())?;
        let physical_field = self.physical_file_schema.field(physical_index);

        if logical_field == physical_field {
            return Ok(None);
        }

        let (
            DataType::Map(logical_entries, logical_ordered),
            DataType::Map(physical_entries, physical_ordered),
        ) = (logical_field.data_type(), physical_field.data_type())
        else {
            return Ok(None);
        };
        let (DataType::Struct(logical_fields), DataType::Struct(physical_fields)) =
            (logical_entries.data_type(), physical_entries.data_type())
        else {
            return Ok(None);
        };

        let compatible = logical_ordered == physical_ordered
            && logical_fields.len() == physical_fields.len()
            && logical_fields
                .iter()
                .zip(physical_fields)
                .all(|(logical, physical)| {
                    logical.data_type() == physical.data_type()
                        && logical.is_nullable() == physical.is_nullable()
                });
        if !compatible {
            return Ok(None);
        }

        Ok(Some(Arc::new(ReconcileMapSchemaExpr::new(
            Arc::new(Column::new(column.name(), physical_index)),
            Arc::new(logical_field.clone()),
        ))))
    }
}

impl PhysicalExprAdapter for IcebergPhysicalExprAdapter {
    fn rewrite(&self, expr: Arc<dyn PhysicalExpr>) -> Result<Arc<dyn PhysicalExpr>> {
        let columns = collect_columns(&expr);
        let needs_custom_rewrite = columns.iter().any(|column| {
            self.is_logical_variant_column(column.name())
                || self.is_logical_map_column(column.name())
        });
        if !needs_custom_rewrite {
            return self.default.rewrite(expr);
        }

        expr.transform_up(|expr| {
            let Some(column) = expr.downcast_ref::<Column>() else {
                return Ok(Transformed::no(expr));
            };

            if self.is_logical_variant_column(column.name()) {
                return self.rewrite_variant_column(column).map(Transformed::yes);
            }
            if self.is_logical_map_column(column.name()) {
                if let Some(rewritten) = self.rewrite_map_column(column)? {
                    return Ok(Transformed::yes(rewritten));
                }
            }

            self.default.rewrite(expr).map(Transformed::yes)
        })
        .data()
    }
}

#[derive(Debug, Eq)]
struct ReconcileMapSchemaExpr {
    input: Arc<dyn PhysicalExpr>,
    target_field: FieldRef,
}

impl ReconcileMapSchemaExpr {
    fn new(input: Arc<dyn PhysicalExpr>, target_field: FieldRef) -> Self {
        Self {
            input,
            target_field,
        }
    }
}

impl PartialEq for ReconcileMapSchemaExpr {
    fn eq(&self, other: &Self) -> bool {
        self.input.eq(&other.input) && self.target_field == other.target_field
    }
}

impl Hash for ReconcileMapSchemaExpr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.input.hash(state);
        self.target_field.hash(state);
    }
}

impl Display for ReconcileMapSchemaExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reconcile_map_schema({})", self.input)
    }
}

impl PhysicalExpr for ReconcileMapSchemaExpr {
    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(self.target_field.data_type().clone())
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(self.target_field.is_nullable())
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let ColumnarValue::Array(input) = self.input.evaluate(batch)? else {
            return internal_err!("Map file column unexpectedly evaluated to a scalar");
        };
        let input = input.as_any().downcast_ref::<MapArray>().ok_or_else(|| {
            datafusion::common::DataFusionError::Internal(
                "Map file column did not contain a MapArray".to_string(),
            )
        })?;
        let DataType::Map(target_entries, ordered) = self.target_field.data_type() else {
            return internal_err!("Map target field must use Map storage");
        };
        let DataType::Struct(target_entry_fields) = target_entries.data_type() else {
            return internal_err!("Map target entries must use Struct storage");
        };

        let entries = StructArray::try_new(
            target_entry_fields.clone(),
            input.entries().columns().to_vec(),
            input.entries().nulls().cloned(),
        )?;
        let output = MapArray::try_new(
            Arc::clone(target_entries),
            input.offsets().clone(),
            entries,
            input.nulls().cloned(),
            *ordered,
        )?;
        Ok(ColumnarValue::Array(Arc::new(output) as ArrayRef))
    }

    fn return_field(&self, _input_schema: &Schema) -> Result<FieldRef> {
        Ok(Arc::clone(&self.target_field))
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.input]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        if children.len() != 1 {
            return internal_err!("ReconcileMapSchemaExpr requires exactly one child");
        }
        Ok(Arc::new(Self::new(
            Arc::clone(&children[0]),
            Arc::clone(&self.target_field),
        )))
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

fn is_variant_storage(data_type: &DataType) -> bool {
    let DataType::Struct(fields) = data_type else {
        return false;
    };

    let has_metadata = fields.iter().any(|field| {
        field.name() == "metadata"
            && matches!(
                field.data_type(),
                DataType::Binary | DataType::LargeBinary | DataType::BinaryView
            )
    });
    let has_value = fields.iter().any(|field| field.name() == "value");
    let has_typed_value = fields.iter().any(|field| field.name() == "typed_value");
    has_metadata && (has_value || has_typed_value)
}

#[derive(Debug, Eq)]
struct UnshredVariantExpr {
    input: Arc<dyn PhysicalExpr>,
    target_field: FieldRef,
}

impl UnshredVariantExpr {
    fn new(input: Arc<dyn PhysicalExpr>, target_field: FieldRef) -> Self {
        Self {
            input,
            target_field,
        }
    }
}

impl PartialEq for UnshredVariantExpr {
    fn eq(&self, other: &Self) -> bool {
        self.input.eq(&other.input) && self.target_field == other.target_field
    }
}

impl Hash for UnshredVariantExpr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.input.hash(state);
        self.target_field.hash(state);
    }
}

impl Display for UnshredVariantExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unshred_variant({})", self.input)
    }
}

impl PhysicalExpr for UnshredVariantExpr {
    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(self.target_field.data_type().clone())
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(self.target_field.is_nullable())
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let ColumnarValue::Array(input) = self.input.evaluate(batch)? else {
            return internal_err!("Variant file column unexpectedly evaluated to a scalar");
        };
        let variant = VariantArray::try_new(input.as_ref())?;
        let unshredded = unshred_variant(&variant)?.into_inner();
        let DataType::Struct(target_fields) = self.target_field.data_type() else {
            return internal_err!("Variant target field must use Struct storage");
        };
        let output = StructArray::try_new(
            target_fields.clone(),
            unshredded.columns().to_vec(),
            unshredded.nulls().cloned(),
        )?;
        Ok(ColumnarValue::Array(Arc::new(output) as ArrayRef))
    }

    fn return_field(&self, _input_schema: &Schema) -> Result<FieldRef> {
        Ok(Arc::clone(&self.target_field))
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.input]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        if children.len() != 1 {
            return internal_err!("UnshredVariantExpr requires exactly one child");
        }
        Ok(Arc::new(Self::new(
            Arc::clone(&children[0]),
            Arc::clone(&self.target_field),
        )))
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::{BinaryViewArray, BooleanArray, Int64Array, StringArray};
    use datafusion::arrow::buffer::OffsetBuffer;
    use datafusion::arrow::datatypes::{Field, Fields};

    use super::*;

    fn variant_field(name: &str, fields: Fields) -> FieldRef {
        Arc::new(
            Field::new(name, DataType::Struct(fields), true).with_metadata(
                [(
                    "ARROW:extension:name".to_string(),
                    PARQUET_VARIANT_EXTENSION_NAME.to_string(),
                )]
                .into(),
            ),
        )
    }

    #[test]
    fn unshreds_snowflake_boolean_variant() -> Result<()> {
        let physical_fields = Fields::from(vec![
            Field::new("metadata", DataType::BinaryView, false),
            Field::new("value", DataType::BinaryView, true),
            Field::new("typed_value", DataType::Boolean, true),
        ]);
        let payload = StructArray::try_new(
            physical_fields.clone(),
            vec![
                Arc::new(BinaryViewArray::from_iter_values([&[1, 0, 0]])),
                Arc::new(BinaryViewArray::from(vec![None::<&[u8]>])),
                Arc::new(BooleanArray::from(vec![Some(true)])),
            ],
            None,
        )?;
        let physical_schema =
            Arc::new(Schema::new(vec![variant_field("payload", physical_fields)]));
        let batch = RecordBatch::try_new(physical_schema, vec![Arc::new(payload)])?;
        let target_field = variant_field(
            "payload",
            Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, true),
            ]),
        );
        let expr = UnshredVariantExpr::new(
            Arc::new(Column::new("payload", 0)),
            Arc::clone(&target_field),
        );

        let ColumnarValue::Array(output) = expr.evaluate(&batch)? else {
            return internal_err!("expected array output");
        };
        assert_eq!(output.data_type(), target_field.data_type());
        let variant = VariantArray::try_new(output.as_ref())?;
        assert!(variant.typed_value_column().is_none());
        assert_eq!(format!("{:?}", variant.try_value(0)?), "BooleanTrue");
        Ok(())
    }

    #[test]
    fn reconciles_parquet_map_entry_field_name() -> Result<()> {
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
        )?;
        let physical_entries = Arc::new(Field::new(
            "attrs",
            DataType::Struct(entry_fields.clone()),
            false,
        ));
        let physical_field = Arc::new(Field::new(
            "attrs",
            DataType::Map(Arc::clone(&physical_entries), false),
            true,
        ));
        let map = MapArray::try_new(
            physical_entries,
            OffsetBuffer::new(vec![0, 2, 2].into()),
            entries,
            None,
            false,
        )?;
        let physical_schema = Arc::new(Schema::new(vec![physical_field]));
        let batch = RecordBatch::try_new(Arc::clone(&physical_schema), vec![Arc::new(map)])?;

        let logical_entries =
            Arc::new(Field::new("entries", DataType::Struct(entry_fields), false));
        let logical_field = Arc::new(Field::new(
            "attrs",
            DataType::Map(logical_entries, false),
            true,
        ));
        let logical_schema = Arc::new(Schema::new(vec![Arc::clone(&logical_field)]));
        let adapter = IcebergPhysicalExprAdapterFactory.create(logical_schema, physical_schema)?;
        let expr = adapter.rewrite(Arc::new(Column::new("attrs", 0)))?;

        let ColumnarValue::Array(output) = expr.evaluate(&batch)? else {
            return internal_err!("expected array output");
        };
        assert_eq!(output.data_type(), logical_field.data_type());
        let output = output.as_any().downcast_ref::<MapArray>().unwrap();
        assert_eq!(output.value_offsets(), &[0, 2, 2]);
        assert_eq!(output.null_count(), 0);
        Ok(())
    }
}
