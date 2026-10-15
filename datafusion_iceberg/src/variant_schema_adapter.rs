use std::collections::HashMap;
use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{
    new_null_array, Array, ArrayRef, FixedSizeListArray, LargeListArray, ListArray, MapArray,
    StructArray,
};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, FieldRef, Fields, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion::common::{exec_err, internal_err, Result, ScalarValue};
use datafusion::physical_expr::utils::collect_columns;
use datafusion::physical_expr_adapter::{
    DefaultPhysicalExprAdapter, PhysicalExprAdapter, PhysicalExprAdapterFactory,
};
use datafusion::physical_plan::expressions::{CastExpr, Column, Literal};
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
        let physical_fields_by_id = physical_file_schema
            .fields()
            .iter()
            .enumerate()
            .filter_map(|(index, field)| {
                field_id(field).map(|field_id| (field_id, (index, Arc::clone(field))))
            })
            .collect();
        Ok(Arc::new(IcebergPhysicalExprAdapter {
            default: DefaultPhysicalExprAdapter::new(
                Arc::clone(&logical_file_schema),
                Arc::clone(&physical_file_schema),
            ),
            logical_file_schema,
            physical_file_schema,
            physical_fields_by_id,
        }))
    }
}

#[derive(Debug)]
struct IcebergPhysicalExprAdapter {
    default: DefaultPhysicalExprAdapter,
    logical_file_schema: SchemaRef,
    physical_file_schema: SchemaRef,
    physical_fields_by_id: HashMap<i32, (usize, FieldRef)>,
}

impl IcebergPhysicalExprAdapter {
    fn physical_field_by_id(
        &self,
        field_id: i32,
    ) -> Option<(usize, datafusion::arrow::datatypes::FieldRef)> {
        self.physical_fields_by_id
            .get(&field_id)
            .map(|(index, field)| (*index, Arc::clone(field)))
    }

    fn field_id_mapping(
        &self,
        name: &str,
    ) -> Option<(
        datafusion::arrow::datatypes::FieldRef,
        usize,
        datafusion::arrow::datatypes::FieldRef,
    )> {
        let logical = self.logical_file_schema.field_with_name(name).ok()?;
        let field_id = field_id(logical)?;
        let (physical_index, physical) = self.physical_field_by_id(field_id)?;
        Some((Arc::new(logical.clone()), physical_index, physical))
    }

    fn reused_name_with_different_id(&self, name: &str) -> Option<FieldRef> {
        let logical = self.logical_file_schema.field_with_name(name).ok()?;
        let logical_id = field_id(logical)?;
        if self.physical_field_by_id(logical_id).is_some() {
            return None;
        }
        let physical = self.physical_file_schema.field_with_name(name).ok()?;
        (field_id(physical).is_some_and(|id| id != logical_id)).then(|| Arc::new(logical.clone()))
    }

    fn is_logical_variant_column(&self, name: &str) -> bool {
        self.logical_file_schema
            .field_with_name(name)
            .is_ok_and(|field| field.extension_type_name() == Some(PARQUET_VARIANT_EXTENSION_NAME))
    }

    fn rewrite_variant_column(&self, column: &Column) -> Result<Arc<dyn PhysicalExpr>> {
        let mapping = self.field_id_mapping(column.name()).or_else(|| {
            let logical = self
                .logical_file_schema
                .field_with_name(column.name())
                .ok()?;
            let physical_index = self.physical_file_schema.index_of(column.name()).ok()?;
            Some((
                Arc::new(logical.clone()),
                physical_index,
                Arc::clone(&self.physical_file_schema.fields()[physical_index]),
            ))
        });
        let Some((logical_field, physical_index, physical_field)) = mapping else {
            return self.default.rewrite(Arc::new(column.clone()));
        };

        if !is_variant_storage(physical_field.data_type()) {
            return internal_err!(
                "Iceberg Variant column '{}' has incompatible physical type {}",
                column.name(),
                physical_field.data_type()
            );
        }

        Ok(Arc::new(UnshredVariantExpr::new(
            Arc::new(Column::new(physical_field.name(), physical_index)),
            logical_field,
        )))
    }

    fn rewrite_field_id_column(&self, column: &Column) -> Result<Option<Arc<dyn PhysicalExpr>>> {
        let Some((logical_field, physical_index, physical_field)) =
            self.field_id_mapping(column.name())
        else {
            return Ok(None);
        };
        let nested_change =
            needs_nested_projection(physical_field.data_type(), logical_field.data_type());
        if logical_field.name() == physical_field.name() && !nested_change {
            return Ok(None);
        }

        let physical_column: Arc<dyn PhysicalExpr> =
            Arc::new(Column::new(physical_field.name(), physical_index));
        if nested_change {
            return Ok(Some(Arc::new(FieldIdColumnExpr::with_nested_projection(
                physical_column,
                logical_field,
                physical_field.data_type(),
            )?)));
        }
        let input = if logical_field.data_type() == physical_field.data_type() {
            physical_column
        } else {
            Arc::new(CastExpr::new_with_target_field(
                physical_column,
                Arc::clone(&logical_field),
                None,
            ))
        };
        Ok(Some(Arc::new(FieldIdColumnExpr::new(input, logical_field))))
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

fn field_id(field: &datafusion::arrow::datatypes::Field) -> Option<i32> {
    field
        .metadata()
        .get(PARQUET_FIELD_ID_META_KEY)
        .and_then(|id| id.parse().ok())
}

fn needs_nested_projection(source: &DataType, target: &DataType) -> bool {
    source != target
        && matches!(
            (source, target),
            (DataType::Struct(_), DataType::Struct(_))
                | (DataType::List(_), DataType::List(_))
                | (DataType::LargeList(_), DataType::LargeList(_))
                | (DataType::FixedSizeList(_, _), DataType::FixedSizeList(_, _))
                | (DataType::Map(_, _), DataType::Map(_, _))
        )
}

impl PhysicalExprAdapter for IcebergPhysicalExprAdapter {
    fn rewrite(&self, expr: Arc<dyn PhysicalExpr>) -> Result<Arc<dyn PhysicalExpr>> {
        let columns = collect_columns(&expr);
        let requires_iceberg_rewrite = columns.iter().any(|column| {
            self.is_logical_variant_column(column.name())
                || Self::row_lineage_field_id(column.name()).is_some()
                || self.reused_name_with_different_id(column.name()).is_some()
                || self
                    .field_id_mapping(column.name())
                    .is_some_and(|(logical, _, physical)| {
                        logical.name() != physical.name()
                            || needs_nested_projection(physical.data_type(), logical.data_type())
                    })
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

            if let Some(field) = self.reused_name_with_different_id(column.name()) {
                if !field.is_nullable() {
                    return exec_err!(
                        "Required Iceberg field '{}' is absent from the data file",
                        field.name()
                    );
                }
                let value = ScalarValue::Null.cast_to(field.data_type())?;
                return Ok(Transformed::yes(Arc::new(Literal::new_with_metadata(
                    value,
                    Some(datafusion::common::metadata::FieldMetadata::from(
                        field.as_ref(),
                    )),
                ))));
            }

            if self.is_logical_variant_column(column.name()) {
                return self.rewrite_variant_column(column).map(Transformed::yes);
            }

            if let Some(physical) = self.rewrite_field_id_column(column)? {
                return Ok(Transformed::yes(physical));
            }

            self.default.rewrite(expr).map(Transformed::yes)
        })
        .data()
    }
}

#[derive(Debug, Eq)]
struct FieldIdColumnExpr {
    input: Arc<dyn PhysicalExpr>,
    target_field: FieldRef,
    nested_projection: Option<NestedProjection>,
}

impl FieldIdColumnExpr {
    fn new(input: Arc<dyn PhysicalExpr>, target_field: FieldRef) -> Self {
        Self {
            input,
            target_field,
            nested_projection: None,
        }
    }

    fn with_nested_projection(
        input: Arc<dyn PhysicalExpr>,
        target_field: FieldRef,
        physical_type: &DataType,
    ) -> Result<Self> {
        let Some(nested_projection) =
            NestedProjection::new(physical_type, target_field.data_type())?
        else {
            return internal_err!("Iceberg nested field projection requires container types");
        };
        Ok(Self {
            input,
            target_field,
            nested_projection: Some(nested_projection),
        })
    }
}

impl PartialEq for FieldIdColumnExpr {
    fn eq(&self, other: &Self) -> bool {
        self.input.eq(&other.input)
            && self.target_field == other.target_field
            && self.nested_projection == other.nested_projection
    }
}

impl Hash for FieldIdColumnExpr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.input.hash(state);
        self.target_field.hash(state);
        self.nested_projection.hash(state);
    }
}

impl Display for FieldIdColumnExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.input, f)
    }
}

impl PhysicalExpr for FieldIdColumnExpr {
    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(self.target_field.data_type().clone())
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(self.target_field.is_nullable())
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let value = self.input.evaluate(batch)?;
        let Some(projection) = &self.nested_projection else {
            return Ok(value);
        };
        let ColumnarValue::Array(value) = value else {
            return internal_err!("Iceberg nested column unexpectedly evaluated to a scalar");
        };
        Ok(ColumnarValue::Array(projection.apply(&value)?))
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
            return internal_err!("FieldIdColumnExpr requires exactly one child");
        }
        Ok(Arc::new(Self {
            input: Arc::clone(&children[0]),
            target_field: Arc::clone(&self.target_field),
            nested_projection: self.nested_projection.clone(),
        }))
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NestedProjection {
    Struct(StructProjection),
    List {
        field: FieldRef,
        values: Option<Box<Self>>,
    },
    LargeList {
        field: FieldRef,
        values: Option<Box<Self>>,
    },
    FixedSizeList {
        field: FieldRef,
        size: i32,
        values: Option<Box<Self>>,
    },
    Map {
        field: FieldRef,
        ordered: bool,
        entries: Option<Box<Self>>,
    },
}

impl NestedProjection {
    fn new(source: &DataType, target: &DataType) -> Result<Option<Self>> {
        if source == target {
            return Ok(None);
        }
        let projection = match (source, target) {
            (DataType::Struct(source), DataType::Struct(target)) => {
                Self::Struct(StructProjection::new(source, target)?)
            }
            (DataType::List(source), DataType::List(target)) => {
                verify_container_child_id(source, target)?;
                Self::List {
                    field: Arc::clone(target),
                    values: Self::new(source.data_type(), target.data_type())?.map(Box::new),
                }
            }
            (DataType::LargeList(source), DataType::LargeList(target)) => {
                verify_container_child_id(source, target)?;
                Self::LargeList {
                    field: Arc::clone(target),
                    values: Self::new(source.data_type(), target.data_type())?.map(Box::new),
                }
            }
            (
                DataType::FixedSizeList(source, source_size),
                DataType::FixedSizeList(target, size),
            ) => {
                if source_size != size {
                    return exec_err!("Iceberg fixed-size list width changed across schemas");
                }
                verify_container_child_id(source, target)?;
                Self::FixedSizeList {
                    field: Arc::clone(target),
                    size: *size,
                    values: Self::new(source.data_type(), target.data_type())?.map(Box::new),
                }
            }
            (DataType::Map(source, _), DataType::Map(target, ordered)) => {
                verify_container_child_id(source, target)?;
                Self::Map {
                    field: Arc::clone(target),
                    ordered: *ordered,
                    entries: Self::new(source.data_type(), target.data_type())?.map(Box::new),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(projection))
    }

    fn apply(&self, input: &ArrayRef) -> Result<ArrayRef> {
        match self {
            Self::Struct(projection) => projection.apply(input),
            Self::List { field, values } => {
                let Some(input) = input.as_any().downcast_ref::<ListArray>() else {
                    return internal_err!("Iceberg nested projection expected a list array");
                };
                let values = project_or_cast(input.values(), field.data_type(), values.as_deref())?;
                Ok(Arc::new(ListArray::try_new(
                    Arc::clone(field),
                    input.offsets().clone(),
                    values,
                    input.nulls().cloned(),
                )?))
            }
            Self::LargeList { field, values } => {
                let Some(input) = input.as_any().downcast_ref::<LargeListArray>() else {
                    return internal_err!("Iceberg nested projection expected a large list array");
                };
                let values = project_or_cast(input.values(), field.data_type(), values.as_deref())?;
                Ok(Arc::new(LargeListArray::try_new(
                    Arc::clone(field),
                    input.offsets().clone(),
                    values,
                    input.nulls().cloned(),
                )?))
            }
            Self::FixedSizeList {
                field,
                size,
                values,
            } => {
                let Some(input) = input.as_any().downcast_ref::<FixedSizeListArray>() else {
                    return internal_err!(
                        "Iceberg nested projection expected a fixed-size list array"
                    );
                };
                let values = project_or_cast(input.values(), field.data_type(), values.as_deref())?;
                Ok(Arc::new(FixedSizeListArray::try_new_with_length(
                    Arc::clone(field),
                    *size,
                    values,
                    input.nulls().cloned(),
                    input.len(),
                )?))
            }
            Self::Map {
                field,
                ordered,
                entries,
            } => {
                let Some(input) = input.as_any().downcast_ref::<MapArray>() else {
                    return internal_err!("Iceberg nested projection expected a map array");
                };
                let input_entries: ArrayRef = Arc::new(input.entries().clone());
                let projected =
                    project_or_cast(&input_entries, field.data_type(), entries.as_deref())?;
                let Some(projected) = projected.as_any().downcast_ref::<StructArray>() else {
                    return internal_err!("Iceberg map entries must be a struct array");
                };
                Ok(Arc::new(MapArray::try_new(
                    Arc::clone(field),
                    input.offsets().clone(),
                    projected.clone(),
                    input.nulls().cloned(),
                    *ordered,
                )?))
            }
        }
    }
}

fn verify_container_child_id(source: &FieldRef, target: &FieldRef) -> Result<()> {
    if let (Some(source_id), Some(target_id)) = (field_id(source), field_id(target)) {
        if source_id != target_id {
            return exec_err!(
                "Iceberg container child '{}' changed field ID from {source_id} to {target_id}",
                target.name()
            );
        }
    }
    Ok(())
}

fn project_or_cast(
    source: &ArrayRef,
    target: &DataType,
    projection: Option<&NestedProjection>,
) -> Result<ArrayRef> {
    if let Some(projection) = projection {
        projection.apply(source)
    } else if source.data_type() == target {
        Ok(Arc::clone(source))
    } else {
        Ok(cast(source.as_ref(), target)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StructProjection {
    target_fields: Fields,
    children: Vec<StructChildProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StructChildProjection {
    source_index: Option<usize>,
    target: FieldRef,
    nested: Option<Box<NestedProjection>>,
}

impl StructProjection {
    fn new(source: &Fields, target: &Fields) -> Result<Self> {
        let source_has_ids = source.iter().any(|field| field_id(field).is_some());
        let children = target
            .iter()
            .map(|target_field| {
                let source_index = match (source_has_ids, field_id(target_field)) {
                    (true, Some(id)) => source.iter().position(|field| field_id(field) == Some(id)),
                    _ => source
                        .iter()
                        .position(|field| field.name() == target_field.name()),
                };
                if source_index.is_none() && !target_field.is_nullable() {
                    return exec_err!(
                        "Required Iceberg struct field '{}' is absent from the data file",
                        target_field.name()
                    );
                }
                let nested = source_index
                    .map(|index| {
                        NestedProjection::new(source[index].data_type(), target_field.data_type())
                    })
                    .transpose()?
                    .flatten()
                    .map(Box::new);
                Ok(StructChildProjection {
                    source_index,
                    target: Arc::clone(target_field),
                    nested,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            target_fields: target.clone(),
            children,
        })
    }

    fn apply(&self, input: &ArrayRef) -> Result<ArrayRef> {
        let Some(input) = input.as_any().downcast_ref::<StructArray>() else {
            return internal_err!("Iceberg nested field projection expected a struct array");
        };
        let columns = self
            .children
            .iter()
            .map(|child| {
                let Some(index) = child.source_index else {
                    return Ok(new_null_array(child.target.data_type(), input.len()));
                };
                let source = input.column(index);
                project_or_cast(source, child.target.data_type(), child.nested.as_deref())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(StructArray::try_new(
            self.target_fields.clone(),
            columns,
            input.nulls().cloned(),
        )?))
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
        if unshredded.columns().len() != target_fields.len() {
            return internal_err!("Unshredded Variant columns do not match the target schema");
        }
        // Parquet can yield Binary even when the logical Variant schema uses BinaryView.
        let columns = unshredded
            .columns()
            .iter()
            .zip(target_fields.iter())
            .map(|(column, field)| {
                if column.data_type() == field.data_type() {
                    Ok(Arc::clone(column))
                } else {
                    Ok(cast(column.as_ref(), field.data_type())?)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let output =
            StructArray::try_new(target_fields.clone(), columns, unshredded.nulls().cloned())?;
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
    use datafusion::arrow::array::{
        BinaryArray, BinaryViewArray, BooleanArray, Decimal128Array, Int64Array, StringArray,
    };
    use datafusion::arrow::datatypes::{Field, Fields};
    use parquet::variant::Variant;
    use parquet_variant_compute::VariantArrayBuilder;

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
    fn unshreds_snowflake_decimal_variant_without_losing_type_or_scale() -> Result<()> {
        let physical_fields = Fields::from(vec![
            Field::new("metadata", DataType::Binary, false),
            Field::new("value", DataType::Binary, true),
            Field::new("typed_value", DataType::Decimal128(10, 2), true),
        ]);
        let payload = StructArray::try_new(
            physical_fields.clone(),
            vec![
                Arc::new(BinaryArray::from_iter_values([&[1, 0, 0]])),
                Arc::new(BinaryArray::from(vec![None::<&[u8]>])),
                Arc::new(
                    Decimal128Array::from(vec![Some(12_345)]).with_precision_and_scale(10, 2)?,
                ),
            ],
            None,
        )?;
        let with_field_id = |field: FieldRef| {
            let mut metadata = field.metadata().clone();
            metadata.insert(PARQUET_FIELD_ID_META_KEY.to_owned(), "2".to_owned());
            Arc::new(field.as_ref().clone().with_metadata(metadata))
        };
        let physical_field = with_field_id(variant_field("writer_payload", physical_fields));
        let physical_schema = Arc::new(Schema::new(vec![physical_field]));
        let batch = RecordBatch::try_new(Arc::clone(&physical_schema), vec![Arc::new(payload)])?;
        let logical_field = with_field_id(variant_field(
            "payload",
            Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, true),
            ]),
        ));
        let logical_schema = Arc::new(Schema::new(vec![Arc::clone(&logical_field)]));
        let adapter = IcebergPhysicalExprAdapterFactory.create(logical_schema, physical_schema)?;
        let expr = adapter.rewrite(Arc::new(Column::new("payload", 0)))?;

        let ColumnarValue::Array(output) = expr.evaluate(&batch)? else {
            return internal_err!("expected array output");
        };
        assert_eq!(expr.return_field(batch.schema().as_ref())?, logical_field);
        assert_eq!(output.data_type(), logical_field.data_type());
        let variant = VariantArray::try_new(output.as_ref())?;
        let Some(decimal) = variant.try_value(0)?.as_decimal16() else {
            return internal_err!("expected decimal VARIANT");
        };
        assert_eq!(decimal.integer(), 12_345);
        assert_eq!(decimal.scale(), 2);
        Ok(())
    }

    #[test]
    fn adapts_unshredded_binary_variant_columns_and_nulls() -> Result<()> {
        let mut builder = VariantArrayBuilder::new(2);
        builder.append_variant(Variant::BooleanTrue);
        builder.append_null();
        let source = builder.build().into_inner();
        let physical_fields = Fields::from(vec![
            Field::new("metadata", DataType::Binary, false),
            Field::new("value", DataType::Binary, false),
        ]);
        let physical_columns: Vec<ArrayRef> = source
            .columns()
            .iter()
            .map(|column| cast(column.as_ref(), &DataType::Binary))
            .collect::<std::result::Result<_, _>>()?;
        let physical =
            StructArray::try_new(physical_fields, physical_columns, source.nulls().cloned())?;
        let physical_schema = Arc::new(Schema::new(vec![Field::new(
            "payload",
            physical.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(physical_schema, vec![Arc::new(physical)])?;
        let target_field = variant_field(
            "payload",
            Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, false),
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
        assert_eq!(format!("{:?}", variant.try_value(0)?), "BooleanTrue");
        assert!(variant.is_null(1));
        Ok(())
    }

    #[test]
    #[ignore = "manual Variant binary-width adaptation benchmark"]
    fn benchmark_variant_binary_width_adaptation() -> Result<()> {
        use std::hint::black_box;
        use std::time::Instant;

        let target_field = variant_field(
            "payload",
            Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, false),
            ]),
        );
        for (row_count, iterations) in [(1_usize, 1000_u32), (4096, 100)] {
            let mut builder = VariantArrayBuilder::new(row_count);
            for _ in 0..row_count {
                builder.append_variant(Variant::BooleanTrue);
            }
            let source = builder.build().into_inner();
            for binary_storage in [false, true] {
                let physical = if binary_storage {
                    let columns = source
                        .columns()
                        .iter()
                        .map(|column| cast(column.as_ref(), &DataType::Binary))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    StructArray::try_new(
                        Fields::from(vec![
                            Field::new("metadata", DataType::Binary, false),
                            Field::new("value", DataType::Binary, false),
                        ]),
                        columns,
                        source.nulls().cloned(),
                    )?
                } else {
                    source.clone()
                };
                let schema = Arc::new(Schema::new(vec![Field::new(
                    "payload",
                    physical.data_type().clone(),
                    true,
                )]));
                let batch = RecordBatch::try_new(schema, vec![Arc::new(physical)])?;
                let expr = UnshredVariantExpr::new(
                    Arc::new(Column::new("payload", 0)),
                    Arc::clone(&target_field),
                );
                let start = Instant::now();
                for _ in 0..iterations {
                    black_box(expr.evaluate(black_box(&batch))?);
                }
                eprintln!(
                    "rows={row_count} binary_storage={binary_storage} average_ns_per_batch={}",
                    (start.elapsed() / iterations).as_nanos()
                );
            }
        }
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

    #[test]
    fn maps_renamed_iceberg_column_by_field_id_without_copying() -> Result<()> {
        let field_id = |id: i32| [(PARQUET_FIELD_ID_META_KEY.to_owned(), id.to_string())].into();
        let logical_schema = Arc::new(Schema::new(vec![Arc::new(
            Field::new("display_name", DataType::Utf8, false).with_metadata(field_id(2)),
        )]));
        let physical_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false).with_metadata(field_id(1)),
            Field::new("old_name", DataType::Utf8, false).with_metadata(field_id(2)),
        ]));
        let adapter = IcebergPhysicalExprAdapterFactory
            .create(Arc::clone(&logical_schema), Arc::clone(&physical_schema))?;
        let rewritten = adapter.rewrite(Arc::new(Column::new("display_name", 0)))?;
        let target = rewritten.return_field(&physical_schema)?;
        assert_eq!(target.name(), "display_name");
        assert_eq!(target.metadata(), logical_schema.field(0).metadata());

        let ids: ArrayRef = Arc::new(Int64Array::from(vec![1, 2]));
        let names: ArrayRef = Arc::new(StringArray::from(vec!["one", "two"]));
        let batch = RecordBatch::try_new(physical_schema, vec![ids, Arc::clone(&names)])?;
        let ColumnarValue::Array(output) = rewritten.evaluate(&batch)? else {
            return internal_err!("renamed field unexpectedly evaluated to a scalar");
        };
        assert!(Arc::ptr_eq(&output, &names));
        Ok(())
    }

    #[test]
    fn reused_iceberg_column_name_does_not_read_old_field_id() -> Result<()> {
        let field_id = |id: i32| [(PARQUET_FIELD_ID_META_KEY.to_owned(), id.to_string())].into();
        let logical_schema = Arc::new(Schema::new(vec![
            Field::new("x", DataType::Int64, true).with_metadata(field_id(2))
        ]));
        let physical_schema = Arc::new(Schema::new(vec![
            Field::new("x", DataType::Int64, true).with_metadata(field_id(1))
        ]));
        let adapter = IcebergPhysicalExprAdapterFactory
            .create(logical_schema, Arc::clone(&physical_schema))?;
        let expr = adapter.rewrite(Arc::new(Column::new("x", 0)))?;
        let batch = RecordBatch::try_new(
            physical_schema,
            vec![Arc::new(Int64Array::from(vec![Some(7), Some(8)]))],
        )?;
        let result = expr.evaluate(&batch)?.into_array(batch.num_rows())?;
        assert_eq!(result.null_count(), 2);
        Ok(())
    }

    #[test]
    fn maps_renamed_nested_iceberg_field_by_id() -> Result<()> {
        use datafusion::arrow::array::Int32Array;

        let field_id = |id: i32| [(PARQUET_FIELD_ID_META_KEY.to_owned(), id.to_string())].into();
        let physical_nested =
            Field::new("old_name", DataType::Int32, false).with_metadata(field_id(3));
        let physical_other =
            Field::new("old_other", DataType::Int32, false).with_metadata(field_id(4));
        let logical_nested =
            Field::new("new_name", DataType::Int32, false).with_metadata(field_id(3));
        let logical_other =
            Field::new("new_other", DataType::Int32, false).with_metadata(field_id(4));
        let logical_added = Field::new("added", DataType::Int32, true).with_metadata(field_id(5));
        let physical_fields = Fields::from(vec![physical_nested, physical_other]);
        let physical = StructArray::try_new(
            physical_fields.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(Int32Array::from(vec![10, 20])),
            ],
            None,
        )?;
        let physical_schema = Arc::new(Schema::new(vec![Field::new(
            "obj",
            DataType::Struct(physical_fields),
            false,
        )
        .with_metadata(field_id(2))]));
        let logical_schema = Arc::new(Schema::new(vec![Field::new(
            "obj",
            DataType::Struct(Fields::from(vec![
                logical_other,
                logical_nested,
                logical_added,
            ])),
            false,
        )
        .with_metadata(field_id(2))]));
        let batch = RecordBatch::try_new(Arc::clone(&physical_schema), vec![Arc::new(physical)])?;
        let adapter = IcebergPhysicalExprAdapterFactory
            .create(Arc::clone(&logical_schema), Arc::clone(&physical_schema))?;
        let expr = adapter.rewrite(Arc::new(Column::new("obj", 0)))?;
        let ColumnarValue::Array(output) = expr.evaluate(&batch)? else {
            return internal_err!("nested field unexpectedly evaluated to a scalar");
        };
        let output = output
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("struct output");
        assert_eq!(output.data_type(), logical_schema.field(0).data_type());
        let other = output
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("Int32 child");
        assert_eq!(other.values().as_ref(), &[10, 20]);
        let values = output
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("Int32 child");
        assert_eq!(values.values().as_ref(), &[1, 2]);
        assert_eq!(output.column(2).null_count(), 2);
        Ok(())
    }

    #[test]
    fn maps_nested_iceberg_fields_inside_list_and_map_by_id() -> Result<()> {
        use datafusion::arrow::array::Int32Array;
        use datafusion::arrow::buffer::OffsetBuffer;

        let field_id = |id: i32| [(PARQUET_FIELD_ID_META_KEY.to_owned(), id.to_string())].into();
        let old_list_fields = Fields::from(vec![
            Field::new("old", DataType::Int32, false).with_metadata(field_id(4)),
            Field::new("other", DataType::Int32, false).with_metadata(field_id(9)),
        ]);
        let new_list_fields = Fields::from(vec![
            Field::new("other_new", DataType::Int32, false).with_metadata(field_id(9)),
            Field::new("new", DataType::Int32, false).with_metadata(field_id(4)),
        ]);
        let list_values = StructArray::try_new(
            old_list_fields.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(Int32Array::from(vec![11, 22])),
            ],
            None,
        )?;
        let old_element = Arc::new(
            Field::new("element", DataType::Struct(old_list_fields), false)
                .with_metadata(field_id(3)),
        );
        let new_element = Arc::new(
            Field::new("element", DataType::Struct(new_list_fields), false)
                .with_metadata(field_id(3)),
        );
        let list = ListArray::try_new(
            Arc::clone(&old_element),
            OffsetBuffer::from_lengths([2]),
            Arc::new(list_values),
            None,
        )?;

        let old_value_fields = Fields::from(vec![
            Field::new("old", DataType::Int32, false).with_metadata(field_id(8)),
            Field::new("other", DataType::Int32, false).with_metadata(field_id(10)),
        ]);
        let new_value_fields = Fields::from(vec![
            Field::new("other_new", DataType::Int32, false).with_metadata(field_id(10)),
            Field::new("new", DataType::Int32, false).with_metadata(field_id(8)),
        ]);
        let map_values = StructArray::try_new(
            old_value_fields.clone(),
            vec![
                Arc::new(Int32Array::from(vec![9])),
                Arc::new(Int32Array::from(vec![19])),
            ],
            None,
        )?;
        let old_entries_fields = Fields::from(vec![
            Field::new("key", DataType::Utf8, false).with_metadata(field_id(6)),
            Field::new("value", DataType::Struct(old_value_fields), false)
                .with_metadata(field_id(7)),
        ]);
        let new_entries_fields = Fields::from(vec![
            Field::new("key", DataType::Utf8, false).with_metadata(field_id(6)),
            Field::new("value", DataType::Struct(new_value_fields), false)
                .with_metadata(field_id(7)),
        ]);
        let entries = StructArray::try_new(
            old_entries_fields.clone(),
            vec![Arc::new(StringArray::from(vec!["k"])), Arc::new(map_values)],
            None,
        )?;
        let old_entries = Arc::new(Field::new(
            "entries",
            DataType::Struct(old_entries_fields),
            false,
        ));
        let new_entries = Arc::new(Field::new(
            "entries",
            DataType::Struct(new_entries_fields),
            false,
        ));
        let map = MapArray::try_new(
            Arc::clone(&old_entries),
            OffsetBuffer::from_lengths([1]),
            entries,
            None,
            false,
        )?;

        let physical_schema = Arc::new(Schema::new(vec![
            Field::new("items", DataType::List(old_element), false).with_metadata(field_id(2)),
            Field::new("attrs", DataType::Map(old_entries, false), false)
                .with_metadata(field_id(5)),
        ]));
        let logical_schema = Arc::new(Schema::new(vec![
            Field::new("items", DataType::List(new_element), false).with_metadata(field_id(2)),
            Field::new("attrs", DataType::Map(new_entries, false), false)
                .with_metadata(field_id(5)),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&physical_schema),
            vec![Arc::new(list), Arc::new(map)],
        )?;
        let adapter = IcebergPhysicalExprAdapterFactory
            .create(Arc::clone(&logical_schema), physical_schema)?;

        let list_expr = adapter.rewrite(Arc::new(Column::new("items", 0)))?;
        let ColumnarValue::Array(list_output) = list_expr.evaluate(&batch)? else {
            return internal_err!("list unexpectedly evaluated to a scalar");
        };
        let list_output = list_output.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(list_output.data_type(), logical_schema.field(0).data_type());
        let list_values = list_output
            .values()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        assert_eq!(list_values.column_names(), vec!["other_new", "new"]);
        let values = list_values
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(values.values().as_ref(), &[11, 22]);
        let values = list_values
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(values.values().as_ref(), &[1, 2]);

        let map_expr = adapter.rewrite(Arc::new(Column::new("attrs", 1)))?;
        let ColumnarValue::Array(map_output) = map_expr.evaluate(&batch)? else {
            return internal_err!("map unexpectedly evaluated to a scalar");
        };
        let map_output = map_output.as_any().downcast_ref::<MapArray>().unwrap();
        assert_eq!(map_output.data_type(), logical_schema.field(1).data_type());
        let map_values = map_output
            .entries()
            .column(1)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        assert_eq!(map_values.column_names(), vec!["other_new", "new"]);
        let values = map_values
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(values.values().as_ref(), &[19]);
        let values = map_values
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(values.values().as_ref(), &[9]);
        Ok(())
    }
}
