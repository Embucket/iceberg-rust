use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, StructArray};
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
use iceberg_rust::spec::arrow::schema::PARQUET_FIELD_ID_META_KEY;
use parquet_variant_compute::{unshred_variant, VariantArray};

use crate::row_lineage::{
    LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID, PHYSICAL_LAST_UPDATED_SEQUENCE_NUMBER_COLUMN,
    PHYSICAL_ROW_ID_COLUMN, ROW_ID_FIELD_ID,
};

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

    fn row_lineage_field_id(name: &str) -> Option<i32> {
        match name {
            PHYSICAL_ROW_ID_COLUMN => Some(ROW_ID_FIELD_ID),
            PHYSICAL_LAST_UPDATED_SEQUENCE_NUMBER_COLUMN => {
                Some(LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID)
            }
            _ => None,
        }
    }

    fn rewrite_row_lineage_column(
        &self,
        _column: &Column,
        field_id: i32,
    ) -> Result<Option<Arc<dyn PhysicalExpr>>> {
        let physical = self
            .physical_file_schema
            .fields()
            .iter()
            .enumerate()
            .find(|(_, field)| {
                field
                    .metadata()
                    .get(PARQUET_FIELD_ID_META_KEY)
                    .and_then(|id| id.parse::<i32>().ok())
                    == Some(field_id)
            });
        let Some((index, field)) = physical else {
            return Ok(None);
        };
        if field.data_type() != &DataType::Int64 {
            return internal_err!(
                "Iceberg row lineage field id {field_id} must be Int64, got {}",
                field.data_type()
            );
        }
        Ok(Some(Arc::new(Column::new(field.name(), index))))
    }
}

impl PhysicalExprAdapter for IcebergPhysicalExprAdapter {
    fn rewrite(&self, expr: Arc<dyn PhysicalExpr>) -> Result<Arc<dyn PhysicalExpr>> {
        let columns = collect_columns(&expr);
        let requires_iceberg_rewrite = columns.iter().any(|column| {
            self.is_logical_variant_column(column.name())
                || Self::row_lineage_field_id(column.name()).is_some()
        });
        if !requires_iceberg_rewrite {
            return self.default.rewrite(expr);
        }

        expr.transform_up(|expr| {
            let Some(column) = expr.downcast_ref::<Column>() else {
                return Ok(Transformed::no(expr));
            };

            if let Some(field_id) = Self::row_lineage_field_id(column.name()) {
                if let Some(physical) = self.rewrite_row_lineage_column(column, field_id)? {
                    return Ok(Transformed::yes(physical));
                }
                return self.default.rewrite(expr).map(Transformed::yes);
            }

            if self.is_logical_variant_column(column.name()) {
                return self.rewrite_variant_column(column).map(Transformed::yes);
            }

            self.default.rewrite(expr).map(Transformed::yes)
        })
        .data()
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
    use datafusion::arrow::array::{BinaryViewArray, BooleanArray};
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
    fn maps_physical_row_lineage_by_reserved_field_id() -> Result<()> {
        let logical_schema = Arc::new(Schema::new(vec![Arc::new(
            Field::new(PHYSICAL_ROW_ID_COLUMN, DataType::Int64, true).with_metadata(
                [(
                    PARQUET_FIELD_ID_META_KEY.to_owned(),
                    ROW_ID_FIELD_ID.to_string(),
                )]
                .into(),
            ),
        )]));
        let physical_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("writer_specific_row_id", DataType::Int64, true).with_metadata(
                [(
                    PARQUET_FIELD_ID_META_KEY.to_owned(),
                    ROW_ID_FIELD_ID.to_string(),
                )]
                .into(),
            ),
        ]));
        let adapter = IcebergPhysicalExprAdapterFactory.create(logical_schema, physical_schema)?;
        let rewritten = adapter.rewrite(Arc::new(Column::new(PHYSICAL_ROW_ID_COLUMN, 0)))?;
        let column = rewritten
            .downcast_ref::<Column>()
            .expect("physical lineage column");
        assert_eq!(column.name(), "writer_specific_row_id");
        assert_eq!(column.index(), 1);
        Ok(())
    }
}
