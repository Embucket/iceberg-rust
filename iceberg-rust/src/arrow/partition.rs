//! Arrow-based partitioning implementation for Iceberg tables
//!
//! This module provides functionality to partition Arrow record batches according to Iceberg partition
//! specifications. It includes:
//!
//! * Streaming partition implementation that processes record batches asynchronously
//! * Support for different partition transforms (identity, bucket, truncate)
//! * Efficient handling of distinct partition values
//! * Automatic management of partition streams and channels

use std::{collections::HashSet, hash::Hash};

use arrow::{
    array::{
        as_primitive_array, as_string_array, ArrayRef, BooleanArray, BooleanBufferBuilder,
        PrimitiveArray, Scalar, StringArray,
    },
    compute::{
        and, filter, filter_record_batch,
        kernels::cmp::{distinct, eq},
    },
    datatypes::{
        ArrowPrimitiveType, DataType, Int32Type, Int64Type, TimeUnit, TimestampMicrosecondType,
    },
    error::ArrowError,
    record_batch::RecordBatch,
};
use itertools::{iproduct, Itertools};

use iceberg_rust_spec::{partition::BoundPartitionField, spec::values::Value};

use super::transform::transform_arrow;

/// Partitions a record batch according to the given partition fields.
///
/// This function takes a record batch and partition field specifications, then splits the batch into
/// multiple record batches based on unique combinations of partition values.
///
/// # Arguments
/// * `record_batch` - The input record batch to partition
/// * `partition_fields` - The partition field specifications that define how to split the data
///
/// # Returns
/// An iterator over results containing:
/// * A vector of partition values that identify the partition
/// * The record batch containing only rows matching those partition values
///
/// # Errors
/// Returns an ArrowError if:
/// * Required columns are missing from the record batch
/// * Transformation operations fail
/// * Data type conversions fail
pub fn partition_record_batch<'a>(
    record_batch: &'a RecordBatch,
    partition_fields: &[BoundPartitionField<'_>],
) -> Result<impl Iterator<Item = Result<(Vec<Value>, RecordBatch), ArrowError>> + 'a, ArrowError> {
    let partition_columns: Vec<ArrayRef> = partition_fields
        .iter()
        .map(|field| {
            let array = record_batch
                .column_by_name(field.source_name())
                .ok_or(ArrowError::SchemaError("Column doesn't exist".to_string()))?;
            transform_arrow(array.clone(), field.transform())
        })
        .collect::<Result<_, ArrowError>>()?;
    let distinct_values: Vec<DistinctValues> = partition_columns
        .iter()
        .map(|x| distinct_values(x.clone()))
        .collect::<Result<Vec<_>, ArrowError>>()?;
    let mut true_buffer = BooleanBufferBuilder::new(record_batch.num_rows());
    true_buffer.append_n(record_batch.num_rows(), true);
    let predicates = distinct_values
        .into_iter()
        .zip(partition_columns.iter())
        .map(|(distinct, value)| match distinct {
            DistinctValues::Int(set) => set
                .into_iter()
                .map(|x| {
                    Ok((
                        Value::Int(x),
                        eq(&PrimitiveArray::<Int32Type>::new_scalar(x), value)?,
                    ))
                })
                .collect::<Result<Vec<_>, ArrowError>>(),
            DistinctValues::Long(set) => set
                .into_iter()
                .map(|x| {
                    Ok((
                        Value::LongInt(x),
                        eq(&PrimitiveArray::<Int64Type>::new_scalar(x), value)?,
                    ))
                })
                .collect::<Result<Vec<_>, ArrowError>>(),
            DistinctValues::Timestamp(set, timezone) => set
                .into_iter()
                .map(|x| {
                    let scalar = Scalar::new(
                        PrimitiveArray::<TimestampMicrosecondType>::from(vec![x])
                            .with_timezone_opt(timezone.clone()),
                    );
                    let partition_value = if timezone.is_some() {
                        Value::TimestampTZ(x)
                    } else {
                        Value::Timestamp(x)
                    };
                    Ok((partition_value, eq(&scalar, value)?))
                })
                .collect::<Result<Vec<_>, ArrowError>>(),
            DistinctValues::String(set) => set
                .into_iter()
                .map(|x| {
                    let res = eq(&StringArray::new_scalar(&x), value)?;
                    Ok((Value::String(x), res))
                })
                .collect::<Result<Vec<_>, ArrowError>>(),
        })
        .try_fold(
            vec![(vec![], BooleanArray::new(true_buffer.finish(), None))],
            |acc, predicates| {
                iproduct!(acc, predicates?.iter())
                    .map(|((mut values, x), (value, y))| {
                        values.push(value.clone());
                        Ok((values, and(&x, y)?))
                    })
                    .filter_ok(|x| x.1.true_count() != 0)
                    .collect::<Result<Vec<(Vec<Value>, _)>, ArrowError>>()
            },
        )?;
    Ok(predicates.into_iter().map(move |(values, predicate)| {
        Ok((values, filter_record_batch(record_batch, &predicate)?))
    }))
}

/// Extracts distinct values from an Arrow array into a DistinctValues enum
///
/// # Arguments
/// * `array` - The Arrow array to extract distinct values from
///
/// # Returns
/// * `Ok(DistinctValues)` - An enum containing a HashSet of the distinct values
/// * `Err(ArrowError)` - If the array's data type is not supported
///
/// # Supported Data Types
/// * Int32 - Converted to DistinctValues::Int
/// * Int64 - Converted to DistinctValues::Long
/// * Utf8 - Converted to DistinctValues::String
fn distinct_values(array: ArrayRef) -> Result<DistinctValues, ArrowError> {
    if array.null_count() != 0 {
        return Err(ArrowError::ComputeError(
            "Null partition values are not supported".to_owned(),
        ));
    }
    match array.data_type() {
        DataType::Int32 => Ok(DistinctValues::Int(distinct_values_primitive::<
            i32,
            Int32Type,
        >(array)?)),
        DataType::Int64 => Ok(DistinctValues::Long(distinct_values_primitive::<
            i64,
            Int64Type,
        >(array)?)),
        DataType::Timestamp(TimeUnit::Microsecond, timezone) => Ok(DistinctValues::Timestamp(
            distinct_values_primitive::<i64, TimestampMicrosecondType>(array.clone())?,
            timezone.clone(),
        )),
        DataType::Utf8 => Ok(DistinctValues::String(distinct_values_string(array)?)),
        _ => Err(ArrowError::ComputeError(
            "Datatype not supported for transform.".to_string(),
        )),
    }
}

/// Extracts distinct primitive values from an Arrow array into a HashSet
///
/// # Type Parameters
/// * `T` - The Rust native type that implements Eq + Hash
/// * `P` - The Arrow primitive type corresponding to T
///
/// # Arguments
/// * `array` - The Arrow array to extract distinct values from
///
/// # Returns
/// A HashSet containing all unique values from the array
fn distinct_values_primitive<T: Eq + Hash, P: ArrowPrimitiveType<Native = T>>(
    array: ArrayRef,
) -> Result<HashSet<P::Native>, ArrowError> {
    let array = as_primitive_array::<P>(&array);

    let first = array.value(0);

    let slice_len = array.len() - 1;

    if slice_len == 0 {
        return Ok(HashSet::from_iter([first]));
    }

    let v1 = array.slice(0, slice_len);
    let v2 = array.slice(1, slice_len);

    // Which consecutive entries are different
    let mask = distinct(&v1, &v2)?;

    let unique = filter(&v2, &mask)?;

    let unique = as_primitive_array::<P>(&unique);

    let set = unique
        .iter()
        .fold(HashSet::from_iter([first]), |mut acc, x| {
            if let Some(x) = x {
                acc.insert(x);
            }
            acc
        });
    Ok(set)
}

/// Extracts distinct string values from an Arrow array into a HashSet
///
/// # Arguments
/// * `array` - The Arrow array to extract distinct values from
///
/// # Returns
/// A HashSet containing all unique string values from the array
fn distinct_values_string(array: ArrayRef) -> Result<HashSet<String>, ArrowError> {
    let slice_len = array.len() - 1;

    let array = as_string_array(&array);

    let first = array.value(0).to_owned();

    if slice_len == 0 {
        return Ok(HashSet::from_iter([first]));
    }

    let v1 = array.slice(0, slice_len);
    let v2 = array.slice(1, slice_len);

    // Which consecutive entries are different
    let mask = distinct(&v1, &v2)?;

    let unique = filter(&v2, &mask)?;

    let unique = as_string_array(&unique);

    let set = unique
        .iter()
        .fold(HashSet::from_iter([first]), |mut acc, x| {
            if let Some(x) = x {
                acc.insert(x.to_owned());
            }
            acc
        });
    Ok(set)
}

/// Represents distinct values found in Arrow arrays during partitioning
///
/// This enum stores unique values from different Arrow array types:
/// * `Int` - Distinct 32-bit integer values
/// * `Long` - Distinct 64-bit integer values  
/// * `Timestamp` - Distinct microsecond timestamps and their optional timezone
/// * `String` - Distinct string values
enum DistinctValues {
    Int(HashSet<i32>),
    Long(HashSet<i64>),
    Timestamp(HashSet<i64>, Option<std::sync::Arc<str>>),
    String(HashSet<String>),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, TimestampMicrosecondArray};
    use iceberg_rust_spec::spec::{
        partition::{PartitionField, Transform},
        types::{PrimitiveType, StructField, Type},
    };

    use super::*;

    #[test]
    fn identity_partition_preserves_timestamp_kind() -> Result<(), ArrowError> {
        for timezone in [None, Some("UTC")] {
            let array: ArrayRef = Arc::new(
                TimestampMicrosecondArray::from(vec![10_i64, 20_i64]).with_timezone_opt(timezone),
            );
            let batch = RecordBatch::try_from_iter(vec![("ts", array)])?;
            let source_type = if timezone.is_some() {
                PrimitiveType::Timestamptz
            } else {
                PrimitiveType::Timestamp
            };
            let source = StructField::new(1, "ts", true, Type::Primitive(source_type), None);
            let partition = PartitionField::new(1, 1000, "ts", Transform::Identity);
            let bound = BoundPartitionField::new(&partition, &source);
            let mut values = partition_record_batch(&batch, &[bound])?
                .map(|result| {
                    result.map(|(values, rows)| {
                        assert_eq!(rows.num_rows(), 1);
                        values[0].clone()
                    })
                })
                .collect::<Result<Vec<_>, ArrowError>>()?;
            values.sort();
            let expected = if timezone.is_some() {
                vec![Value::TimestampTZ(10), Value::TimestampTZ(20)]
            } else {
                vec![Value::Timestamp(10), Value::Timestamp(20)]
            };
            assert_eq!(values, expected);
        }
        Ok(())
    }

    #[test]
    fn null_partition_values_fail_instead_of_dropping_rows() -> Result<(), ArrowError> {
        for (array, source_type) in [
            (
                Arc::new(arrow::array::Int64Array::from(vec![Some(10), None])) as ArrayRef,
                PrimitiveType::Long,
            ),
            (
                Arc::new(TimestampMicrosecondArray::from(vec![Some(10), None]).with_timezone("UTC"))
                    as ArrayRef,
                PrimitiveType::Timestamptz,
            ),
        ] {
            let batch = RecordBatch::try_from_iter(vec![("ts", array)])?;
            let source = StructField::new(1, "ts", false, Type::Primitive(source_type), None);
            let partition = PartitionField::new(1, 1000, "ts", Transform::Identity);
            let bound = BoundPartitionField::new(&partition, &source);
            let result = partition_record_batch(&batch, &[bound]);
            assert!(
                matches!(result, Err(ArrowError::ComputeError(message)) if message.contains("Null partition values"))
            );
        }
        Ok(())
    }
}
