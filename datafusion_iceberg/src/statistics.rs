use datafusion::{
    common::stats::Precision,
    physical_plan::{ColumnStatistics, Statistics},
    scalar::ScalarValue,
};
use iceberg_rust::error::Error;
use iceberg_rust::file_format::parquet::estimate_distinct_count;
use iceberg_rust::spec::{
    decimal::{decimal_mantissa, decimal_scale},
    manifest::{AvroMap, ManifestEntry, Status},
    schema::Schema,
    types::{PrimitiveType, Type},
    values::Value,
};

pub(crate) fn statistics_from_datafiles(
    schema: &Schema,
    datafiles: &[(String, ManifestEntry)],
) -> Statistics {
    datafiles
        .iter()
        .filter(|(_, manifest)| !matches!(manifest.status(), Status::Deleted))
        .map(|(_, manifest)| {
            let column_stats = column_statistics(schema, manifest);
            Statistics {
                num_rows: Precision::Exact(*manifest.data_file().record_count() as usize),
                total_byte_size: Precision::Exact(
                    *manifest.data_file().file_size_in_bytes() as usize
                ),
                column_statistics: column_stats
                    .into_iter()
                    .map(|x| ColumnStatistics {
                        null_count: x.null_count,
                        max_value: x.max_value,
                        min_value: x.min_value,
                        distinct_count: x.distinct_count,
                        sum_value: x.sum_value,
                        byte_size: x.byte_size,
                    })
                    .collect(),
            }
        })
        .reduce(|acc, x| Statistics {
            num_rows: acc.num_rows.add(&x.num_rows),
            total_byte_size: acc.total_byte_size.add(&x.total_byte_size),
            column_statistics: acc
                .column_statistics
                .into_iter()
                .zip(x.column_statistics)
                .map(|(acc, x)| {
                    let new_distinct_count = new_distinct_count(&acc, &x);

                    ColumnStatistics {
                        null_count: acc.null_count.add(&x.null_count),
                        max_value: acc.max_value.max(&x.max_value),
                        min_value: acc.min_value.min(&x.min_value),
                        distinct_count: new_distinct_count,
                        sum_value: acc.sum_value.add(&x.sum_value),
                        byte_size: acc.byte_size.add(&x.byte_size),
                    }
                })
                .collect(),
        })
        .unwrap_or_default()
}

fn column_statistics<'a>(
    schema: &'a Schema,
    manifest: &'a ManifestEntry,
) -> impl Iterator<Item = ColumnStatistics> + 'a {
    schema.fields().iter().map(|field| {
        let id = field.id;
        let data_file = &manifest.data_file();
        ColumnStatistics {
            null_count: data_file
                .null_value_counts()
                .as_ref()
                .and_then(|x| x.get(&id))
                .map(|x| Precision::Exact(*x as usize))
                .unwrap_or(Precision::Absent),
            // Iceberg bounds exclude NaN, and NaN sorts above every number (as in Snowflake),
            // so a float column's upper bound only bounds the file when it is known NaN-free.
            max_value: if may_hold_nan(data_file.nan_value_counts().as_ref(), id, &field.field_type)
            {
                Precision::Absent
            } else {
                data_file
                    .upper_bounds()
                    .as_ref()
                    .and_then(|x| x.get(&id))
                    .and_then(|x| bound_statistic(x.clone(), &field.field_type))
                    .unwrap_or(Precision::Absent)
            },
            min_value: data_file
                .lower_bounds()
                .as_ref()
                .and_then(|x| x.get(&id))
                .and_then(|x| bound_statistic(x.clone(), &field.field_type))
                .unwrap_or(Precision::Absent),
            distinct_count: data_file
                .distinct_counts()
                .as_ref()
                .and_then(|x| x.get(&id))
                .map(|x| Precision::Exact(*x as usize))
                .unwrap_or(Precision::Absent),
            sum_value: Precision::Absent,
            byte_size: Precision::Absent,
        }
    })
}

/// Whether a float or double column of a data file may hold NaN: its NaN count is not recorded
/// (writers that skip NaN counting leave it out) or is positive.
pub(crate) fn may_hold_nan(
    nan_value_counts: Option<&AvroMap<i64>>,
    id: i32,
    field_type: &Type,
) -> bool {
    matches!(
        field_type,
        Type::Primitive(PrimitiveType::Float | PrimitiveType::Double)
    ) && nan_value_counts
        .and_then(|counts| counts.get(&id))
        .is_none_or(|count| *count > 0)
}

/// A data file's lower or upper bound as a column statistic.
///
/// String and binary bounds are only bounds: writers truncate them (the Iceberg default
/// metrics mode is `truncate(16)`) to a prefix of the minimum and an incremented prefix of
/// the maximum. They stay usable for pruning but are marked inexact, so `DataFusion` never
/// answers `MIN`/`MAX` from them without scanning. Other bounds are the column's extremes.
fn bound_statistic(value: Value, field_type: &Type) -> Option<Precision<ScalarValue>> {
    let scalar = convert_value_to_scalar_value(value, field_type).ok()?;
    Some(match field_type {
        Type::Primitive(PrimitiveType::String | PrimitiveType::Binary) => {
            Precision::Inexact(scalar)
        }
        _ => Precision::Exact(scalar),
    })
}

pub(crate) fn manifest_statistics(schema: &Schema, manifest: &ManifestEntry) -> Statistics {
    Statistics {
        num_rows: Precision::Exact(*manifest.data_file().record_count() as usize),
        total_byte_size: Precision::Exact(*manifest.data_file().file_size_in_bytes() as usize),
        column_statistics: column_statistics(schema, manifest).collect(),
    }
}

pub(crate) fn cast_iceberg_bound(value: Value, field_type: &Type) -> Result<Value, Error> {
    if matches!(
        field_type,
        Type::Primitive(PrimitiveType::Decimal {
            precision: 0,
            scale: 0
        })
    ) {
        if !matches!(&value, Value::Decimal(decimal) if decimal_mantissa(decimal)? == 0) {
            return Err(Error::InvalidFormat(
                "Iceberg decimal(0,0) bound must be zero".to_string(),
            ));
        }
        return Ok(value.cast(&Type::Primitive(PrimitiveType::Decimal {
            precision: 1,
            scale: 0,
        }))?);
    }
    Ok(value.cast(field_type)?)
}

fn convert_value_to_scalar_value(value: Value, field_type: &Type) -> Result<ScalarValue, Error> {
    let value = cast_iceberg_bound(value, field_type)?;
    match value {
        Value::Boolean(b) => Ok(ScalarValue::Boolean(Some(b))),
        Value::Int(i) => Ok(ScalarValue::Int32(Some(i))),
        Value::LongInt(l) => Ok(ScalarValue::Int64(Some(l))),
        Value::Float(f) => Ok(ScalarValue::Float32(Some(f.0))),
        Value::Double(d) => Ok(ScalarValue::Float64(Some(d.0))),
        Value::Date(d) => Ok(ScalarValue::Date32(Some(d))),
        Value::Time(t) => Ok(ScalarValue::Time64Microsecond(Some(t))),
        Value::Timestamp(ts) => Ok(ScalarValue::TimestampMicrosecond(Some(ts), None)),
        Value::TimestampTZ(ts) => Ok(ScalarValue::TimestampMicrosecond(
            Some(ts),
            Some("UTC".into()),
        )),
        Value::TimestampNano(ts) => Ok(ScalarValue::TimestampNanosecond(Some(ts), None)),
        Value::TimestampTZNano(ts) => Ok(ScalarValue::TimestampNanosecond(
            Some(ts),
            Some("UTC".into()),
        )),
        Value::String(s) => Ok(ScalarValue::Utf8(Some(s))),
        Value::UUID(u) => Ok(ScalarValue::FixedSizeBinary(
            16,
            Some(u.into_bytes().into()),
        )),
        Value::Fixed(size, data) => Ok(ScalarValue::FixedSizeBinary(size as i32, Some(data))),
        Value::Binary(data) => Ok(ScalarValue::Binary(Some(data))),
        Value::Decimal(decimal) => {
            // Extract precision and scale from the field type
            let (precision, scale) = match field_type {
                Type::Primitive(PrimitiveType::Decimal { precision, scale }) => {
                    ((*precision).max(1) as u8, *scale as i8)
                }
                _ => {
                    // Fallback: use the decimal's own scale and assume max precision
                    // This matches the behavior in Value::datatype()
                    (38, decimal_scale(&decimal) as i8)
                }
            };
            Ok(ScalarValue::Decimal128(
                Some(decimal_mantissa(&decimal)?),
                precision,
                scale,
            ))
        }
        x => Err(Error::Conversion(
            "Iceberg value".to_string(),
            format!("{x:?}"),
        )),
    }
}

fn new_distinct_count(acc: &ColumnStatistics, x: &ColumnStatistics) -> Precision<usize> {
    match (
        &acc.distinct_count,
        &x.distinct_count,
        &acc.min_value,
        &acc.max_value,
        &x.min_value,
        &x.max_value,
    ) {
        (
            Precision::Exact(old_count),
            Precision::Exact(new_count),
            Precision::Exact(ScalarValue::Int32(Some(old_min))),
            Precision::Exact(ScalarValue::Int32(Some(old_max))),
            Precision::Exact(ScalarValue::Int32(Some(new_min))),
            Precision::Exact(ScalarValue::Int32(Some(new_max))),
        ) => {
            let estimated = estimate_distinct_count(
                &[old_min, old_max],
                &[new_min, new_max],
                *old_count as i64,
                *new_count as i64,
            );
            Precision::Inexact(*old_count + estimated as usize)
        }
        (
            Precision::Exact(old_count),
            Precision::Exact(new_count),
            Precision::Exact(ScalarValue::Int64(Some(old_min))),
            Precision::Exact(ScalarValue::Int64(Some(old_max))),
            Precision::Exact(ScalarValue::Int64(Some(new_min))),
            Precision::Exact(ScalarValue::Int64(Some(new_max))),
        ) => {
            let estimated = estimate_distinct_count(
                &[old_min, old_max],
                &[new_min, new_max],
                *old_count as i64,
                *new_count as i64,
            );
            Precision::Inexact(*old_count + estimated as usize)
        }
        (Precision::Absent, Precision::Exact(_), _, _, _, _) => x.distinct_count,
        _ => acc.distinct_count.add(&x.distinct_count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceberg_rust::spec::decimal::decimal_from_i128_with_scale;

    #[test]
    fn float_upper_bound_needs_a_zero_nan_count() {
        use std::collections::HashMap;
        let double = Type::Primitive(PrimitiveType::Double);
        let float = Type::Primitive(PrimitiveType::Float);
        let long = Type::Primitive(PrimitiveType::Long);
        let counts = |count: i64| AvroMap(HashMap::from([(1, count)]));
        // Not recorded (the writer skips NaN counting) or positive: the bound may exclude NaN rows.
        assert!(may_hold_nan(None, 1, &double));
        assert!(may_hold_nan(Some(&counts(0)), 2, &double));
        assert!(may_hold_nan(Some(&counts(3)), 1, &float));
        // Recorded as zero: the upper bound bounds every row.
        assert!(!may_hold_nan(Some(&counts(0)), 1, &double));
        // Only float types can hold NaN.
        assert!(!may_hold_nan(None, 1, &long));
    }

    #[test]
    fn truncatable_bounds_are_inexact_statistics() {
        let string = Type::Primitive(PrimitiveType::String);
        let binary = Type::Primitive(PrimitiveType::Binary);
        let long = Type::Primitive(PrimitiveType::Long);
        // A `truncate(16)` upper bound: the prefix with its last character incremented.
        assert_eq!(
            bound_statistic(Value::String("snowflake_unloae".to_owned()), &string),
            Some(Precision::Inexact(ScalarValue::Utf8(Some(
                "snowflake_unloae".to_owned()
            ))))
        );
        assert!(matches!(
            bound_statistic(Value::Binary(vec![1, 2, 3]), &binary),
            Some(Precision::Inexact(_))
        ));
        assert_eq!(
            bound_statistic(Value::LongInt(7), &long),
            Some(Precision::Exact(ScalarValue::Int64(Some(7))))
        );
    }

    #[test]
    fn converts_precision_38_decimal_bound_to_datafusion() {
        let mantissa = 99_999_999_999_999_999_999_999_999_999_999_999_999_i128;
        let field_type = Type::Primitive(PrimitiveType::Decimal {
            precision: 38,
            scale: 0,
        });

        let scalar = convert_value_to_scalar_value(
            Value::Decimal(decimal_from_i128_with_scale(mantissa, 0).unwrap()),
            &field_type,
        )
        .unwrap();

        assert_eq!(scalar, ScalarValue::Decimal128(Some(mantissa), 38, 0));
    }

    #[test]
    fn promotes_numeric_bounds_to_scan_schema() {
        assert_eq!(
            convert_value_to_scalar_value(Value::Int(-42), &Type::Primitive(PrimitiveType::Long))
                .unwrap(),
            ScalarValue::Int64(Some(-42))
        );
        assert_eq!(
            convert_value_to_scalar_value(
                Value::try_from_bytes(
                    &1.25_f32.to_le_bytes(),
                    &Type::Primitive(PrimitiveType::Float),
                )
                .unwrap(),
                &Type::Primitive(PrimitiveType::Double),
            )
            .unwrap(),
            ScalarValue::Float64(Some(1.25))
        );
    }

    #[test]
    fn preserves_decimal_bound_with_declared_precision() {
        let field_type = Type::Primitive(PrimitiveType::Decimal {
            precision: 10,
            scale: 2,
        });
        let value = Value::Decimal(decimal_from_i128_with_scale(1234, 2).unwrap());
        assert_eq!(
            convert_value_to_scalar_value(value, &field_type).unwrap(),
            ScalarValue::Decimal128(Some(1234), 10, 2)
        );
    }

    #[test]
    fn zero_precision_decimal_bound_uses_physical_precision_one() {
        let field_type = Type::Primitive(PrimitiveType::Decimal {
            precision: 0,
            scale: 0,
        });
        let zero = Value::Decimal(decimal_from_i128_with_scale(0, 0).unwrap());
        assert_eq!(
            convert_value_to_scalar_value(zero, &field_type).unwrap(),
            ScalarValue::Decimal128(Some(0), 1, 0)
        );
        let nonzero = Value::Decimal(decimal_from_i128_with_scale(1, 0).unwrap());
        assert!(convert_value_to_scalar_value(nonzero, &field_type).is_err());
    }
}
