/*!
 * Implement pruning statistics for Datafusion table
 *
 * Pruning is done on two levels:
 *
 * 1. Prune manifests based on information in manifests lists
 * 2. Prune data files based on information in manifests
 *
 * For the first level the trait [`PruningStatistics`] is implemented for the DataFusionTable. It returns the pruning information for the manifest files
 * and not the final data files.
 *
 * For the second level the trait PruningStatistics is implemented for the Manifest
*/

use std::{any::Any, sync::Arc};

use crate::error::Error as DatafusionIcebergError;
use datafusion::{
    arrow::{
        array::ArrayRef,
        datatypes::{DataType, Schema as ArrowSchema, TimeUnit},
    },
    common::DataFusionError,
    physical_optimizer::pruning::PruningStatistics,
    prelude::Column,
    scalar::ScalarValue,
};
use datafusion_expr::{
    expr::ScalarFunction, BinaryExpr, ColumnarValue, Expr, Operator, ScalarFunctionArgs, ScalarUDF,
    ScalarUDFImpl, Signature, TypeSignature, Volatility,
};
use iceberg_rust::{
    arrow::transform::transform_arrow,
    error::Error,
    spec::{
        decimal::{decimal_mantissa, decimal_scale, Decimal},
        manifest::ManifestEntry,
        manifest_list::ManifestListEntry,
        partition::{BoundPartitionField, Transform},
        schema::Schema,
        types::{PrimitiveType, Type},
        values::Value,
    },
    table::ManifestPath,
};

pub(crate) struct PruneManifests<'table, 'manifests> {
    partition_fields: &'table [BoundPartitionField<'table>],
    partition_spec_id: i32,
    files: &'manifests [ManifestListEntry],
}

impl<'table, 'manifests> PruneManifests<'table, 'manifests> {
    pub(crate) fn new(
        partition_fields: &'table [BoundPartitionField<'table>],
        partition_spec_id: i32,
        files: &'manifests [ManifestListEntry],
    ) -> Self {
        Self {
            partition_fields,
            partition_spec_id,
            files,
        }
    }
}

impl PruningStatistics for PruneManifests<'_, '_> {
    fn min_values(&self, column: &Column) -> Option<ArrayRef> {
        let (index, partition_field) = self
            .partition_fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.name() == column.name())?;
        let data_type = partition_field
            .field_type()
            .tranform(partition_field.transform())
            .ok()?;
        let min_values = self.files.iter().map(|manifest| {
            (manifest.partition_spec_id == self.partition_spec_id)
                .then_some(manifest)
                .and_then(|manifest| manifest.partitions.as_ref())
                .and_then(|partitions| partitions.get(index))
                .and_then(|partition| partition.lower_bound.as_ref())
                .and_then(|min| min.clone().cast(&data_type).ok())
                .map(Value::into_any)
        });
        any_iter_to_array(min_values, &(&data_type).try_into().ok()?).ok()
    }
    fn max_values(&self, column: &Column) -> Option<ArrayRef> {
        let (index, partition_field) = self
            .partition_fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.name() == column.name())?;
        let data_type = partition_field
            .field_type()
            .tranform(partition_field.transform())
            .ok()?;
        let max_values = self.files.iter().map(|manifest| {
            (manifest.partition_spec_id == self.partition_spec_id)
                .then_some(manifest)
                .and_then(|manifest| manifest.partitions.as_ref())
                .and_then(|partitions| partitions.get(index))
                .and_then(|partition| partition.upper_bound.as_ref())
                .and_then(|max| max.clone().cast(&data_type).ok())
                .map(Value::into_any)
        });
        any_iter_to_array(max_values, &(&data_type).try_into().ok()?).ok()
    }
    fn num_containers(&self) -> usize {
        self.files.len()
    }
    fn null_counts(&self, column: &Column) -> Option<ArrayRef> {
        let (index, _) = self
            .partition_fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.source_name() == column.name())?;
        let contains_null = self.files.iter().map(|manifest| {
            (manifest.partition_spec_id == self.partition_spec_id)
                .then_some(manifest)
                .and_then(|manifest| manifest.partitions.as_ref())
                .and_then(|partitions| partitions.get(index))
                .and_then(|partition| (!partition.contains_null).then_some(0))
        });
        ScalarValue::iter_to_array(contains_null.map(ScalarValue::Int32)).ok()
    }
    fn contained(
        &self,
        _column: &Column,
        _values: &std::collections::HashSet<ScalarValue>,
    ) -> Option<datafusion::arrow::array::BooleanArray> {
        None
    }

    fn row_counts(&self) -> Option<ArrayRef> {
        let row_counts = self.files.iter().map(|x| {
            match (
                x.added_rows_count,
                x.existing_rows_count,
                x.deleted_rows_count,
            ) {
                (Some(a), Some(e), Some(d)) => Some(a + e - d),
                _ => None,
            }
        });
        ScalarValue::iter_to_array(row_counts.map(ScalarValue::Int64)).ok()
    }
}

pub(crate) struct PruneDataFiles<'table, 'manifests> {
    schema: &'table Schema,
    arrow_schema: &'table ArrowSchema,
    files: &'manifests [(ManifestPath, ManifestEntry)],
}

impl<'table, 'manifests> PruneDataFiles<'table, 'manifests> {
    pub(crate) fn new(
        schema: &'table Schema,
        arrow_schema: &'table ArrowSchema,
        files: &'manifests [(ManifestPath, ManifestEntry)],
    ) -> Self {
        Self {
            schema,
            arrow_schema,
            files,
        }
    }
}

impl PruningStatistics for PruneDataFiles<'_, '_> {
    fn min_values(&self, column: &Column) -> Option<ArrayRef> {
        let field = self.schema.fields().get_name(&column.name)?;
        let column_id = field.id;
        let datatype = self
            .arrow_schema
            .field_with_name(&column.name)
            .ok()?
            .data_type();
        let min_values =
            self.files
                .iter()
                .map(|manifest| match &manifest.1.data_file().lower_bounds() {
                    Some(map) => map.get(&column_id).and_then(|value| {
                        value
                            .clone()
                            .cast(&field.field_type)
                            .ok()
                            .map(Value::into_any)
                    }),
                    None => None,
                });
        any_iter_to_array(min_values, datatype).ok()
    }
    fn max_values(&self, column: &Column) -> Option<ArrayRef> {
        let field = self.schema.fields().get_name(&column.name)?;
        let column_id = field.id;
        let datatype = self
            .arrow_schema
            .field_with_name(&column.name)
            .ok()?
            .data_type();
        let max_values =
            self.files
                .iter()
                .map(|manifest| match &manifest.1.data_file().upper_bounds() {
                    Some(map) => map.get(&column_id).and_then(|value| {
                        value
                            .clone()
                            .cast(&field.field_type)
                            .ok()
                            .map(Value::into_any)
                    }),
                    None => None,
                });
        any_iter_to_array(max_values, datatype).ok()
    }
    fn num_containers(&self) -> usize {
        self.files.len()
    }
    fn null_counts(&self, column: &Column) -> Option<ArrayRef> {
        let column_id = self.schema.fields().get_name(&column.name)?.id;
        let null_counts =
            self.files.iter().map(
                |manifest| match &manifest.1.data_file().null_value_counts() {
                    Some(map) => map.get(&{ column_id }).copied(),
                    None => None,
                },
            );
        ScalarValue::iter_to_array(null_counts.map(ScalarValue::Int64)).ok()
    }
    fn contained(
        &self,
        _column: &Column,
        _values: &std::collections::HashSet<ScalarValue>,
    ) -> Option<datafusion::arrow::array::BooleanArray> {
        None
    }

    fn row_counts(&self) -> Option<ArrayRef> {
        let row_counts = self
            .files
            .iter()
            .map(|manifest| Some(*manifest.1.data_file().record_count()));
        ScalarValue::iter_to_array(row_counts.map(ScalarValue::Int64)).ok()
    }
}

fn any_iter_to_array(
    iter: impl Iterator<Item = Option<Box<dyn Any>>>,
    datatype: &DataType,
) -> Result<ArrayRef, DataFusionError> {
    match datatype {
        DataType::Boolean => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Boolean(opt.and_then(|value| Some(*value.downcast::<bool>().ok()?)))
        })),
        DataType::Int32 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Int32(opt.and_then(|value| Some(*value.downcast::<i32>().ok()?)))
        })),
        DataType::Int64 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Int64(opt.and_then(|value| Some(*value.downcast::<i64>().ok()?)))
        })),
        DataType::Float32 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Float32(opt.and_then(|value| Some(*value.downcast::<f32>().ok()?)))
        })),
        DataType::Float64 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Float64(opt.and_then(|value| Some(*value.downcast::<f64>().ok()?)))
        })),
        DataType::Date32 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Date32(opt.and_then(|value| Some(*value.downcast::<i32>().ok()?)))
        })),
        DataType::Date64 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Date64(opt.and_then(|value| Some(*value.downcast::<i64>().ok()?)))
        })),
        DataType::Time64(_) => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Time64Microsecond(
                opt.and_then(|value| Some(*value.downcast::<i64>().ok()?)),
            )
        })),
        DataType::Timestamp(_, tz) => {
            // Make sure to preserve the column's timezone for the sake of comparisons.
            ScalarValue::iter_to_array(iter.map(move |opt| {
                ScalarValue::TimestampMicrosecond(
                    opt.and_then(|value| Some(*value.downcast::<i64>().ok()?)),
                    tz.clone(),
                )
            }))
        }
        DataType::Utf8 => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Utf8(opt.and_then(|value| Some(*value.downcast::<String>().ok()?)))
        })),
        DataType::FixedSizeBinary(_) => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Binary(opt.and_then(|value| Some(*value.downcast::<Vec<u8>>().ok()?)))
        })),
        DataType::Binary => ScalarValue::iter_to_array(iter.map(|opt| {
            ScalarValue::Binary(opt.and_then(|value| Some(*value.downcast::<Vec<u8>>().ok()?)))
        })),
        // Only prune when the stored scale matches the column's scale.
        DataType::Decimal128(precision, scale) => {
            let (precision, scale) = (*precision, *scale);
            ScalarValue::iter_to_array(iter.map(move |opt| {
                ScalarValue::Decimal128(
                    opt.and_then(|value| {
                        let d = *value.downcast::<Decimal>().ok()?;
                        if decimal_scale(&d) == scale as u32 {
                            decimal_mantissa(&d).ok()
                        } else {
                            None
                        }
                    }),
                    precision,
                    scale,
                )
            }))
        }
        _ => Err(DataFusionError::Internal(
            "Arrow datatype not supported for pruning.".to_string(),
        )),
    }
}

pub(crate) fn transform_predicate(
    expr: Expr,
    partition_fields: &[BoundPartitionField],
) -> Option<Expr> {
    let expr = match expr {
        Expr::BinaryExpr(BinaryExpr { left, op, right })
            if matches!(op, Operator::And | Operator::Or) =>
        {
            let left = transform_predicate(*left, partition_fields);
            let right = transform_predicate(*right, partition_fields);
            return match (left, right, op) {
                (Some(left), Some(right), op) => Some(Expr::BinaryExpr(BinaryExpr::new(
                    Box::new(left),
                    op,
                    Box::new(right),
                ))),
                (left, right, Operator::And) => left.or(right),
                _ => None,
            };
        }
        expr => expr,
    };
    if expr.column_refs().iter().all(|column| {
        partition_fields.iter().any(|field| {
            field.source_name() == column.name()
                && field.name() == column.name()
                && field.transform() == &Transform::Identity
        })
    }) {
        return Some(expr);
    }

    let Expr::BinaryExpr(BinaryExpr { left, op, right }) = expr else {
        return None;
    };
    let (column, literal, column_on_left) = match (*left, *right) {
        (Expr::Column(column), literal @ Expr::Literal(..)) => (column, literal, true),
        (literal @ Expr::Literal(..), Expr::Column(column)) => (column, literal, false),
        _ => return None,
    };
    let field = partition_fields
        .iter()
        .find(|field| field.source_name() == column.name())?;
    if field.transform() != &Transform::Identity {
        let Expr::Literal(value, _) = &literal else {
            return None;
        };
        if !literal_matches_source(value, field) {
            return None;
        }
        if matches!(field.transform(), Transform::Day | Transform::Hour)
            && matches!(value, ScalarValue::TimestampMicrosecond(Some(value), _) if *value < 0)
        {
            return None;
        }
    }
    let op = if column_on_left {
        op
    } else {
        match op {
            Operator::Lt => Operator::Gt,
            Operator::LtEq => Operator::GtEq,
            Operator::Gt => Operator::Lt,
            Operator::GtEq => Operator::LtEq,
            Operator::Eq | Operator::NotEq => op,
            _ => return None,
        }
    };
    let op = match field.transform() {
        Transform::Identity => op,
        // Legacy numeric manifests and truncated String min/max statistics can
        // record a bucket unrelated to a matching row. Keep the row predicate,
        // but do not use bucket metadata to prune manifests.
        Transform::Bucket(_) => return None,
        Transform::Year
        | Transform::Month
        | Transform::Day
        | Transform::Hour
        | Transform::Truncate(_) => match op {
            Operator::Eq | Operator::LtEq | Operator::GtEq => op,
            Operator::Lt => Operator::LtEq,
            Operator::Gt => Operator::GtEq,
            _ => return None,
        },
        _ => return None,
    };
    let literal = transform_literal(literal, field.transform())?;
    let column = Expr::Column(Column::new(column.relation, field.name().to_owned()));
    if field.transform() == &Transform::Month {
        let legacy = Expr::BinaryExpr(BinaryExpr::new(
            Box::new(literal.clone()),
            Operator::Plus,
            Box::new(Expr::Literal(ScalarValue::Int32(Some(1)), None)),
        ));
        return match op {
            Operator::Eq => Some(Expr::BinaryExpr(BinaryExpr::new(
                Box::new(partition_comparison(
                    column.clone(),
                    Operator::GtEq,
                    literal,
                )),
                Operator::And,
                Box::new(partition_comparison(column, Operator::LtEq, legacy)),
            ))),
            Operator::LtEq => Some(partition_comparison(column, op, legacy)),
            Operator::GtEq => Some(partition_comparison(column, op, literal)),
            _ => None,
        };
    }
    Some(partition_comparison(column, op, literal))
}

fn partition_comparison(column: Expr, op: Operator, literal: Expr) -> Expr {
    Expr::BinaryExpr(BinaryExpr::new(Box::new(column), op, Box::new(literal)))
}

fn literal_matches_source(value: &ScalarValue, field: &BoundPartitionField<'_>) -> bool {
    matches!(
        (field.field_type(), value),
        (
            Type::Primitive(PrimitiveType::Int),
            ScalarValue::Int32(Some(_))
        ) | (
            Type::Primitive(PrimitiveType::Long),
            ScalarValue::Int64(Some(_))
        ) | (
            Type::Primitive(PrimitiveType::Date),
            ScalarValue::Date32(Some(_))
        ) | (
            Type::Primitive(PrimitiveType::Time),
            ScalarValue::Time64Microsecond(Some(_))
        ) | (
            Type::Primitive(PrimitiveType::String),
            ScalarValue::Utf8(Some(_))
        ) | (
            Type::Primitive(PrimitiveType::Timestamp),
            ScalarValue::TimestampMicrosecond(Some(_), None)
        )
    ) || matches!(
        (field.field_type(), value),
        (
            Type::Primitive(PrimitiveType::Timestamptz),
            ScalarValue::TimestampMicrosecond(Some(_), Some(tz))
        ) if tz.as_ref() == "UTC"
    )
}

fn transform_literal(expr: Expr, transform: &Transform) -> Option<Expr> {
    let Expr::Literal(value, _) = &expr else {
        return None;
    };
    if value.is_null() {
        return None;
    }
    match transform {
        Transform::Year => Some(Expr::ScalarFunction(ScalarFunction::new_udf(
            Arc::new(ScalarUDF::new_from_impl(DateTransform::new())),
            vec![Expr::Literal(ScalarValue::new_utf8("year"), None), expr],
        ))),
        Transform::Month => Some(Expr::ScalarFunction(ScalarFunction::new_udf(
            Arc::new(ScalarUDF::new_from_impl(DateTransform::new())),
            vec![Expr::Literal(ScalarValue::new_utf8("month"), None), expr],
        ))),
        Transform::Day => Some(Expr::ScalarFunction(ScalarFunction::new_udf(
            Arc::new(ScalarUDF::new_from_impl(DateTransform::new())),
            vec![Expr::Literal(ScalarValue::new_utf8("day"), None), expr],
        ))),
        Transform::Hour => Some(Expr::ScalarFunction(ScalarFunction::new_udf(
            Arc::new(ScalarUDF::new_from_impl(DateTransform::new())),
            vec![Expr::Literal(ScalarValue::new_utf8("hour"), None), expr],
        ))),
        Transform::Identity => Some(expr),
        Transform::Truncate(width) if *width == 0 => None,
        Transform::Truncate(width) => {
            let value = match value {
                ScalarValue::Int16(Some(value)) if *width <= i16::MAX as u32 => {
                    ScalarValue::Int16(Some(value.checked_sub(value.rem_euclid(*width as i16))?))
                }
                ScalarValue::Int32(Some(value)) if *width <= i32::MAX as u32 => {
                    ScalarValue::Int32(Some(value.checked_sub(value.rem_euclid(*width as i32))?))
                }
                ScalarValue::Int64(Some(value)) => ScalarValue::Int64(Some(
                    value.checked_sub(value.rem_euclid(i64::from(*width)))?,
                )),
                _ => return None,
            };
            Some(Expr::Literal(value, None))
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct DateTransform {
    signature: Signature,
}

impl DateTransform {
    fn new() -> Self {
        let signature = Signature {
            type_signature: TypeSignature::OneOf(vec![
                TypeSignature::Exact(vec![DataType::Utf8, DataType::Date32]),
                TypeSignature::Exact(vec![
                    DataType::Utf8,
                    DataType::Timestamp(TimeUnit::Microsecond, None),
                ]),
                // Iceberg `timestamptz` is always UTC microseconds, mapped to
                // Timestamp(Microsecond, Some("UTC")) in iceberg-rust-spec/src/arrow/schema.rs.
                // Arrow allows arbitrary tz strings [1] but we only accept "UTC".
                // [1] https://github.com/apache/arrow/blob/apache-arrow-23.0.1/format/Schema.fbs#L385
                TypeSignature::Exact(vec![
                    DataType::Utf8,
                    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                ]),
            ]),
            volatility: Volatility::Immutable,
            parameter_names: None,
        };
        Self { signature }
    }
}

impl ScalarUDFImpl for DateTransform {
    fn name(&self) -> &str {
        "date_transform"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Int32)
    }

    fn invoke_with_args(
        &self,
        args: ScalarFunctionArgs,
    ) -> datafusion::error::Result<ColumnarValue> {
        let args = args.args;
        let transform = &args[0];
        let array = &args[1];
        let ColumnarValue::Scalar(ScalarValue::Utf8(Some(transform))) = transform else {
            return Err(DataFusionError::External(Box::new(Error::InvalidFormat(
                "Partition transform".to_owned(),
            ))));
        };
        let transform = match transform.as_str() {
            "year" => Ok(Transform::Year),
            "month" => Ok(Transform::Month),
            "day" => Ok(Transform::Day),
            "hour" => Ok(Transform::Hour),
            _ => Err(DataFusionError::External(Box::new(Error::InvalidFormat(
                "Partition transform".to_owned(),
            )))),
        }?;
        match array {
            ColumnarValue::Array(array) => Ok(ColumnarValue::Array(transform_arrow(
                array.clone(),
                &transform,
            )?)),
            ColumnarValue::Scalar(scalar) => Ok(ColumnarValue::Scalar(
                value_to_scalarvalue(
                    scalarvalue_to_value(scalar)
                        .map_err(DatafusionIcebergError::from)?
                        .transform(&transform)
                        .map_err(DatafusionIcebergError::from)?,
                )
                .map_err(DatafusionIcebergError::from)?,
            )),
        }
    }
}

fn scalarvalue_to_value(scalar: &ScalarValue) -> Result<Value, Error> {
    match scalar {
        ScalarValue::Boolean(x) => Ok(Value::Boolean(x.ok_or(Error::InvalidFormat(
            "Value can't be null when converting to iceberg value".to_owned(),
        ))?)),
        ScalarValue::Int32(x) => Ok(Value::Int(x.ok_or(Error::InvalidFormat(
            "Value can't be null when converting to iceberg value".to_owned(),
        ))?)),
        ScalarValue::Int64(x) => Ok(Value::LongInt(x.ok_or(Error::InvalidFormat(
            "Value can't be null when converting to iceberg value".to_owned(),
        ))?)),
        ScalarValue::Date32(x) => Ok(Value::Date(x.ok_or(Error::InvalidFormat(
            "Value can't be null when converting to iceberg value".to_owned(),
        ))?)),
        ScalarValue::Time64Microsecond(x) => Ok(Value::Time(x.ok_or(Error::InvalidFormat(
            "Value can't be null when converting to iceberg value".to_owned(),
        ))?)),
        ScalarValue::TimestampMicrosecond(x, Some(tz)) if tz == &Arc::from("UTC") => {
            Ok(Value::TimestampTZ(x.ok_or(Error::InvalidFormat(
                "Value can't be null when converting to iceberg value".to_owned(),
            ))?))
        }
        ScalarValue::TimestampMicrosecond(x, None) => Ok(Value::Timestamp(x.ok_or(
            Error::InvalidFormat("Value can't be null when converting to iceberg value".to_owned()),
        )?)),
        x => Err(Error::NotSupported(format!(
            "Transforming {x} to iceberg value"
        ))),
    }
}

fn value_to_scalarvalue(value: Value) -> Result<ScalarValue, Error> {
    match value {
        Value::Boolean(x) => Ok(ScalarValue::Boolean(Some(x))),
        Value::Int(x) => Ok(ScalarValue::Int32(Some(x))),
        Value::LongInt(x) => Ok(ScalarValue::Int64(Some(x))),
        Value::Date(x) => Ok(ScalarValue::Date32(Some(x))),
        Value::Time(x) => Ok(ScalarValue::Time64Microsecond(Some(x))),
        Value::Timestamp(x) => Ok(ScalarValue::TimestampMicrosecond(Some(x), None)),
        Value::TimestampTZ(x) => Ok(ScalarValue::TimestampMicrosecond(
            Some(x),
            Some("UTC".into()),
        )),
        x => Err(Error::NotSupported(format!(
            "Transforming {x} to iceberg value"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{
        Array, Date32Array, Decimal128Array, Int64Array, TimestampMicrosecondArray,
    };
    use datafusion::arrow::datatypes::Field;
    use datafusion::common::config::ConfigOptions;
    use datafusion::execution::context::SessionContext;
    use datafusion::logical_expr::physical_planning_context::PhysicalPlanningContext;
    use datafusion::physical_expr::create_physical_expr;
    use datafusion::physical_optimizer::pruning::PruningPredicateBuilder;
    use iceberg_rust::spec::decimal::decimal_from_i128_with_scale;
    use iceberg_rust::spec::{
        manifest::{Content, DataFile, FileFormat, Status},
        manifest_list::{Content as ManifestContent, FieldSummary},
        partition::PartitionField,
        table_metadata::FormatVersion,
        types::{PrimitiveType, StructField, StructType, Type},
        values::Struct,
    };
    use std::sync::Arc;

    #[test]
    fn manifest_pruning_does_not_compare_different_partition_specs() {
        let source = StructField::new(2, "b", false, Type::Primitive(PrimitiveType::Long), None);
        let partition = PartitionField::new(2, 1000, "b", Transform::Identity);
        let fields = [BoundPartitionField::new(&partition, &source)];
        let entry = |spec_id, lower: Value| ManifestListEntry {
            format_version: FormatVersion::V2,
            manifest_path: format!("/{spec_id}.avro"),
            manifest_length: 1,
            partition_spec_id: spec_id,
            content: ManifestContent::Data,
            sequence_number: 1,
            min_sequence_number: 1,
            added_snapshot_id: 1,
            added_files_count: Some(1),
            existing_files_count: Some(0),
            deleted_files_count: Some(0),
            added_rows_count: Some(1),
            existing_rows_count: Some(0),
            deleted_rows_count: Some(0),
            partitions: Some(vec![FieldSummary {
                contains_null: false,
                contains_nan: None,
                lower_bound: Some(lower.clone()),
                upper_bound: Some(lower),
            }]),
            key_metadata: None,
            first_row_id: None,
        };
        let manifests = vec![
            entry(0, Value::Int(1000)),
            entry(1, Value::LongInt(3)),
            entry(1, Value::Int(-42)),
        ];
        let pruning = PruneManifests::new(&fields, 1, &manifests);
        let minimums = pruning.min_values(&Column::from_name("b")).unwrap();
        let minimums = minimums.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(minimums.len(), 3);
        assert!(minimums.is_null(0));
        assert_eq!(minimums.value(1), 3);
        assert_eq!(minimums.value(2), -42);
        let maximums = pruning.max_values(&Column::from_name("b")).unwrap();
        let maximums = maximums.as_any().downcast_ref::<Int64Array>().unwrap();
        assert!(maximums.is_null(0));
        assert_eq!(maximums.value(2), -42);
        let null_counts = pruning.null_counts(&Column::from_name("b")).unwrap();
        assert!(null_counts.is_null(0));
    }

    #[test]
    fn month_projection_keeps_legacy_and_spec_manifests() {
        let source = StructField::new(1, "d", false, Type::Primitive(PrimitiveType::Date), None);
        let partition = PartitionField::new(1, 1000, "month", Transform::Month);
        let fields = [BoundPartitionField::new(&partition, &source)];
        let predicate = transform_predicate(
            partition_comparison(
                Expr::Column(Column::from_name("d")),
                Operator::Eq,
                Expr::Literal(ScalarValue::Date32(Some(0)), None),
            ),
            &fields,
        )
        .unwrap();
        let schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "month",
            DataType::Int32,
            true,
        )]));
        let session = SessionContext::new();
        let state = session.state();
        let physical = create_physical_expr(
            &predicate,
            &schema.clone().try_into().unwrap(),
            state.execution_props(),
            &PhysicalPlanningContext::default(),
        )
        .unwrap();
        let pruning = PruningPredicateBuilder::new()
            .with_file_schema(schema)
            .try_build(physical)
            .unwrap();
        let manifests: Vec<_> = [-1, 0, 1, 2]
            .into_iter()
            .map(|month| ManifestListEntry {
                format_version: FormatVersion::V2,
                manifest_path: format!("/{month}.avro"),
                manifest_length: 1,
                partition_spec_id: 1,
                content: ManifestContent::Data,
                sequence_number: 1,
                min_sequence_number: 1,
                added_snapshot_id: 1,
                added_files_count: Some(1),
                existing_files_count: Some(0),
                deleted_files_count: Some(0),
                added_rows_count: Some(1),
                existing_rows_count: Some(0),
                deleted_rows_count: Some(0),
                partitions: Some(vec![FieldSummary {
                    contains_null: false,
                    contains_nan: None,
                    lower_bound: Some(Value::Int(month)),
                    upper_bound: Some(Value::Int(month)),
                }]),
                key_metadata: None,
                first_row_id: None,
            })
            .collect();
        let selected = pruning
            .prune(&PruneManifests::new(&fields, 1, &manifests))
            .unwrap();
        assert_eq!(selected, vec![false, true, true, false]);
    }

    #[test]
    fn data_file_pruning_promotes_old_numeric_bounds_and_keeps_unknowns() {
        let schema = Schema::from_struct_type(
            StructType::new(vec![StructField::new(
                1,
                "id",
                false,
                Type::Primitive(PrimitiveType::Long),
                None,
            )]),
            1,
            None,
        );
        let arrow_schema = ArrowSchema::new(vec![Field::new("id", DataType::Int64, true)]);
        let entry = |lower: Value| {
            let file = DataFile::builder()
                .with_content(Content::Data)
                .with_file_path("/data.parquet".into())
                .with_file_format(FileFormat::Parquet)
                .with_partition(Struct::from_iter(Vec::<(String, Option<Value>)>::new()))
                .with_record_count(1)
                .with_file_size_in_bytes(1)
                .with_column_sizes(None)
                .with_value_counts(None)
                .with_null_value_counts(None)
                .with_nan_value_counts(None)
                .with_distinct_counts(None)
                .with_lower_bounds(Some(std::collections::HashMap::from([(1, lower)])))
                .with_upper_bounds(None)
                .build()
                .unwrap();
            ManifestEntry::builder()
                .with_format_version(FormatVersion::V2)
                .with_status(Status::Added)
                .with_data_file(file)
                .build()
                .unwrap()
        };
        let files = vec![
            ("old".into(), entry(Value::Int(-42))),
            ("unknown".into(), entry(Value::String("invalid".into()))),
        ];
        let pruning = PruneDataFiles::new(&schema, &arrow_schema, &files);
        let min_values = pruning.min_values(&Column::from_name("id")).unwrap();
        let min_values = min_values.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(min_values.len(), 2);
        assert_eq!(min_values.value(0), -42);
        assert!(min_values.is_null(1));
    }

    /// Helper: invoke `DateTransform` directly with a transform name and scalar value.
    fn invoke_date_transform(
        transform_name: &str,
        scalar: ScalarValue,
    ) -> datafusion::error::Result<ColumnarValue> {
        let dt = DateTransform::new();
        let value_type = scalar.data_type();
        dt.invoke_with_args(ScalarFunctionArgs {
            args: vec![
                ColumnarValue::Scalar(ScalarValue::new_utf8(transform_name)),
                ColumnarValue::Scalar(scalar),
            ],
            arg_fields: vec![
                Arc::new(Field::new("transform", DataType::Utf8, false)),
                Arc::new(Field::new("value", value_type, true)),
            ],
            number_rows: 1,
            return_field: Arc::new(Field::new("result", DataType::Int32, true)),
            config_options: Arc::new(ConfigOptions::default()),
        })
    }

    /// Extract Int32 from a ColumnarValue, panicking with a clear message on mismatch.
    fn unwrap_int32(result: ColumnarValue) -> i32 {
        match result {
            ColumnarValue::Scalar(ScalarValue::Int32(Some(v))) => v,
            other => panic!("expected ScalarValue::Int32, got {other:?}"),
        }
    }

    // 2024-03-15T10:30:00Z in microseconds since epoch
    const TS_MICROS: i64 = 1_710_498_600_000_000;

    // -- invoke DateTransform with Date32 (19797 days since epoch = 2024-03-15) --

    #[test]
    fn year_on_date32() {
        let result = invoke_date_transform("year", ScalarValue::Date32(Some(19797))).unwrap();
        // 2024 - 1970 = 54
        assert_eq!(unwrap_int32(result), 54);
    }

    #[test]
    fn month_on_date32() {
        let result = invoke_date_transform("month", ScalarValue::Date32(Some(19797))).unwrap();
        assert_eq!(unwrap_int32(result), 650);
    }

    #[test]
    fn day_on_date32() {
        let result = invoke_date_transform("day", ScalarValue::Date32(Some(19797))).unwrap();
        assert_eq!(unwrap_int32(result), 19797);
    }

    #[test]
    fn hour_on_date32_is_rejected() {
        // Date32 has no time component — hour transform is not supported
        let result = invoke_date_transform("hour", ScalarValue::Date32(Some(19797)));
        assert!(
            result.is_err(),
            "hour transform should not be supported for Date32"
        );
    }

    // -- invoke DateTransform directly with Timestamp (no TZ) --

    #[test]
    fn year_on_timestamp() {
        let result = invoke_date_transform(
            "year",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), None),
        )
        .unwrap();
        // 2024 - 1970 = 54
        assert_eq!(unwrap_int32(result), 54);
    }

    #[test]
    fn month_on_timestamp() {
        let result = invoke_date_transform(
            "month",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), None),
        )
        .unwrap();
        assert_eq!(unwrap_int32(result), 650);
    }

    #[test]
    fn day_on_timestamp() {
        let result = invoke_date_transform(
            "day",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), None),
        )
        .unwrap();
        // 2024-03-15 is day 19797 since epoch
        assert_eq!(unwrap_int32(result), 19797);
    }

    #[test]
    fn hour_on_timestamp() {
        let result = invoke_date_transform(
            "hour",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), None),
        )
        .unwrap();
        // 19797 * 24 + 10 = 475138
        assert_eq!(unwrap_int32(result), 475138);
    }

    // -- invoke DateTransform with Timestamp(UTC) --

    #[test]
    fn year_on_timestamp_with_utc() {
        let result = invoke_date_transform(
            "year",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), Some("UTC".into())),
        )
        .unwrap();
        assert_eq!(unwrap_int32(result), 54);
    }

    #[test]
    fn month_on_timestamp_with_utc() {
        let result = invoke_date_transform(
            "month",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), Some("UTC".into())),
        )
        .unwrap();
        assert_eq!(unwrap_int32(result), 650);
    }

    #[test]
    fn day_on_timestamp_with_utc() {
        let result = invoke_date_transform(
            "day",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), Some("UTC".into())),
        )
        .unwrap();
        assert_eq!(unwrap_int32(result), 19797);
    }

    #[test]
    fn hour_on_timestamp_with_utc() {
        let result = invoke_date_transform(
            "hour",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), Some("UTC".into())),
        )
        .unwrap();
        assert_eq!(unwrap_int32(result), 475138);
    }

    // -- edge cases --

    #[test]
    fn epoch_zero_transforms() {
        // 1970-01-01T00:00:00Z
        let cases = vec![
            ("year", 0), // 1970 - 1970 = 0
            ("month", 0),
            ("day", 0),  // day 0 since epoch
            ("hour", 0), // hour 0 since epoch
        ];
        for (name, expected) in cases {
            let result =
                invoke_date_transform(name, ScalarValue::TimestampMicrosecond(Some(0), None))
                    .unwrap();
            assert_eq!(
                unwrap_int32(result),
                expected,
                "epoch zero: {name} transform"
            );
        }
        for (name, expected) in [("month", -1), ("day", -1), ("hour", -1)] {
            let result =
                invoke_date_transform(name, ScalarValue::TimestampMicrosecond(Some(-1), None))
                    .unwrap();
            assert_eq!(unwrap_int32(result), expected, "pre-epoch {name}");
        }
    }

    #[test]
    fn invalid_transform_name_is_rejected() {
        let result = invoke_date_transform(
            "century",
            ScalarValue::TimestampMicrosecond(Some(TS_MICROS), None),
        );
        assert!(result.is_err(), "unknown transform name should be rejected");
    }

    #[test]
    fn signature_rejects_non_utc_timezone() {
        // Iceberg only maps timestamptz to Timestamp(Microsecond, Some("UTC"))
        // per iceberg-rust-spec/src/arrow/schema.rs. The DateTransform signature
        // enforces this — non-UTC tz strings are not accepted.
        let udf = ScalarUDF::new_from_impl(DateTransform::new());
        let sig = &udf.signature().type_signature;
        let non_utc = vec![
            Some("+00:00".into()),
            Some("Etc/UTC".into()),
            Some("America/New_York".into()),
        ];
        for tz in non_utc {
            let args = vec![
                DataType::Utf8,
                DataType::Timestamp(TimeUnit::Microsecond, tz.clone()),
            ];
            let accepts = match sig {
                TypeSignature::OneOf(variants) => variants.iter().any(|v| match v {
                    TypeSignature::Exact(expected) => expected == &args,
                    _ => false,
                }),
                _ => false,
            };
            assert!(
                !accepts,
                "signature should reject Timestamp(Microsecond, {tz:?})"
            );
        }
    }

    // -- transform_literal wiring --

    #[test]
    fn transform_literal_identity_passes_through() {
        let input = Expr::Literal(ScalarValue::TimestampMicrosecond(Some(42), None), None);
        let result = transform_literal(input.clone(), &Transform::Identity)
            .expect("identity should pass through");
        assert_eq!(result, input);
    }

    fn project_id_predicate(
        transform: Transform,
        source_type: PrimitiveType,
        op: Operator,
        value: ScalarValue,
    ) -> Option<Expr> {
        let source = StructField::new(1, "id", false, Type::Primitive(source_type), None);
        let partition = PartitionField::new(1, 1000, "partition_id", transform);
        let fields = [BoundPartitionField::new(&partition, &source)];
        transform_predicate(
            Expr::BinaryExpr(BinaryExpr::new(
                Box::new(Expr::Column(Column::from_name("id"))),
                op,
                Box::new(Expr::Literal(value, None)),
            )),
            &fields,
        )
    }

    #[test]
    fn bucket_predicates_do_not_prune_manifests() {
        for (source_type, value) in [
            (PrimitiveType::Int, ScalarValue::Int32(Some(2))),
            (PrimitiveType::Long, ScalarValue::Int64(Some(2))),
            (PrimitiveType::Date, ScalarValue::Date32(Some(2))),
            (
                PrimitiveType::String,
                ScalarValue::Utf8(Some("iceberg".to_owned())),
            ),
            (PrimitiveType::Time, ScalarValue::Time64Microsecond(Some(2))),
        ] {
            assert!(
                project_id_predicate(Transform::Bucket(16), source_type, Operator::Eq, value)
                    .is_none()
            );
        }
    }

    #[test]
    fn promoted_int_bucket_skips_manifest_pruning() {
        // A bucket[10] Int file containing 6, 9, 70 was grouped in signed
        // bucket 9 but its legacy min/max metadata recorded bucket 2. Iceberg
        // permits the source field to become Long under the same spec ID.
        assert!(project_id_predicate(
            Transform::Bucket(10),
            PrimitiveType::Int,
            Operator::Eq,
            ScalarValue::Int32(Some(9)),
        )
        .is_none());
        assert!(project_id_predicate(
            Transform::Bucket(10),
            PrimitiveType::Long,
            Operator::Eq,
            ScalarValue::Int64(Some(9)),
        )
        .is_none());
    }

    #[test]
    fn monotone_partition_strict_bound_includes_boundary_partition() {
        let projected = project_id_predicate(
            Transform::Truncate(10),
            PrimitiveType::Long,
            Operator::Lt,
            ScalarValue::Int64(Some(27)),
        )
        .expect("truncated range can prune");
        let Expr::BinaryExpr(BinaryExpr { left, op, right }) = projected else {
            panic!("expected comparison");
        };
        assert_eq!(op, Operator::LtEq);
        assert_eq!(*left, Expr::Column(Column::from_name("partition_id")));
        assert_eq!(*right, Expr::Literal(ScalarValue::Int64(Some(20)), None));

        let projected = project_id_predicate(
            Transform::Year,
            PrimitiveType::Date,
            Operator::Gt,
            ScalarValue::Date32(Some(18_628)),
        )
        .expect("year range can prune");
        let Expr::BinaryExpr(BinaryExpr { op, .. }) = projected else {
            panic!("expected comparison");
        };
        assert_eq!(op, Operator::GtEq);
        assert!(project_id_predicate(
            Transform::Truncate(10),
            PrimitiveType::Long,
            Operator::Eq,
            ScalarValue::Int64(Some(i64::MIN)),
        )
        .is_none());
    }

    #[test]
    fn month_projection_accepts_old_and_new_partition_encodings() {
        let projected = project_id_predicate(
            Transform::Month,
            PrimitiveType::Date,
            Operator::Eq,
            ScalarValue::Date32(Some(0)),
        )
        .expect("month equality can prune both encodings");
        let Expr::BinaryExpr(BinaryExpr { left, op, right }) = projected else {
            panic!("expected interval");
        };
        assert_eq!(op, Operator::And);
        assert!(matches!(
            *left,
            Expr::BinaryExpr(BinaryExpr {
                op: Operator::GtEq,
                ..
            })
        ));
        assert!(matches!(
            *right,
            Expr::BinaryExpr(BinaryExpr {
                op: Operator::LtEq,
                ..
            })
        ));

        let source = StructField::new(1, "id", false, Type::Primitive(PrimitiveType::Date), None);
        let partition = PartitionField::new(1, 1000, "partition_id", Transform::Month);
        let fields = [BoundPartitionField::new(&partition, &source)];
        let literal_left = Expr::BinaryExpr(BinaryExpr::new(
            Box::new(Expr::Literal(ScalarValue::Date32(Some(0)), None)),
            Operator::Lt,
            Box::new(Expr::Column(Column::from_name("id"))),
        ));
        let projected = transform_predicate(literal_left, &fields).unwrap();
        assert!(matches!(
            projected,
            Expr::BinaryExpr(BinaryExpr {
                op: Operator::GtEq,
                ..
            })
        ));
    }

    #[test]
    fn negative_timestamp_day_and_hour_skip_legacy_unsafe_pruning() {
        for transform in [Transform::Day, Transform::Hour] {
            assert!(project_id_predicate(
                transform,
                PrimitiveType::Timestamp,
                Operator::Eq,
                ScalarValue::TimestampMicrosecond(Some(-1), None),
            )
            .is_none());
        }
        assert!(project_id_predicate(
            Transform::Day,
            PrimitiveType::Date,
            Operator::Eq,
            ScalarValue::Date32(Some(-1)),
        )
        .is_some());
    }

    #[test]
    fn unsupported_partition_predicates_do_not_affect_safe_conjunctions() {
        let source = StructField::new(1, "id", false, Type::Primitive(PrimitiveType::Long), None);
        let partition = PartitionField::new(1, 1000, "partition_id", Transform::Truncate(10));
        let fields = [BoundPartitionField::new(&partition, &source)];
        let compare = |op| {
            Expr::BinaryExpr(BinaryExpr::new(
                Box::new(Expr::Column(Column::from_name("id"))),
                op,
                Box::new(Expr::Literal(ScalarValue::Int64(Some(123)), None)),
            ))
        };
        let safe = transform_predicate(compare(Operator::Eq), &fields).unwrap();
        let conjunction = Expr::BinaryExpr(BinaryExpr::new(
            Box::new(compare(Operator::Eq)),
            Operator::And,
            Box::new(compare(Operator::NotEq)),
        ));
        assert_eq!(transform_predicate(conjunction, &fields), Some(safe));
        let disjunction = Expr::BinaryExpr(BinaryExpr::new(
            Box::new(compare(Operator::Eq)),
            Operator::Or,
            Box::new(compare(Operator::NotEq)),
        ));
        assert!(transform_predicate(disjunction, &fields).is_none());
    }

    #[test]
    fn any_iter_to_array_date32() {
        let iter = vec![Some(Value::Date(19797).into_any()), None].into_iter();
        let array = any_iter_to_array(iter, &DataType::Date32).unwrap();
        let dates = array.as_any().downcast_ref::<Date32Array>().unwrap();
        assert_eq!(dates.value(0), 19797);
        assert!(dates.is_null(1));
    }

    #[test]
    fn any_iter_to_array_decimal128() {
        let iter = vec![
            Some(Value::Decimal(decimal_from_i128_with_scale(12345, 2).unwrap()).into_any()),
            None,
        ]
        .into_iter();
        let array = any_iter_to_array(iter, &DataType::Decimal128(10, 2)).unwrap();
        let dec = array.as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert_eq!(dec.value(0), 12345);
        assert!(dec.is_null(1));
        assert_eq!(dec.precision(), 10);
        assert_eq!(dec.scale(), 2);
    }

    #[test]
    fn any_iter_to_array_decimal128_scale_mismatch_is_null() {
        // Stored scale (2) != column scale (4): emit null rather than misread the mantissa.
        let iter = std::iter::once(Some(
            Value::Decimal(decimal_from_i128_with_scale(12345, 2).unwrap()).into_any(),
        ));
        let array = any_iter_to_array(iter, &DataType::Decimal128(10, 4)).unwrap();
        let dec = array.as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert!(dec.is_null(0));
    }

    #[test]
    fn any_iter_to_array_preserves_timezone() {
        for val in [Value::Timestamp(TS_MICROS), Value::TimestampTZ(TS_MICROS)] {
            let iter = vec![Some(val.into_any()), None].into_iter();
            let dt = DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));

            let array = any_iter_to_array(iter, &dt).unwrap();

            assert_eq!(array.data_type(), &dt);
            let ts = array
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap();
            assert_eq!(ts.value(0), TS_MICROS);
            assert!(ts.is_null(1));
        }
    }

    #[test]
    fn scalar_value_roundtrip_preserves_timezone() {
        for val in [Value::Timestamp(TS_MICROS), Value::TimestampTZ(TS_MICROS)] {
            let scalar = value_to_scalarvalue(val.clone()).unwrap();
            assert_eq!(scalarvalue_to_value(&scalar).unwrap(), val);
        }
    }
}
