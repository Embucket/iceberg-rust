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
        as_primitive_array, as_string_array, Array, ArrayRef, BooleanArray, BooleanBufferBuilder,
        PrimitiveArray, Scalar, StringArray,
    },
    compute::{
        and, filter, filter_record_batch, is_not_null, is_null,
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

/// A record batch and the partition values shared by all its rows.
pub type PartitionedRecordBatch = (Vec<Option<Value>>, RecordBatch);

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
) -> Result<impl Iterator<Item = Result<PartitionedRecordBatch, ArrowError>> + 'a, ArrowError> {
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
        .map(
            |(distinct, value)| -> Result<Vec<(Option<Value>, BooleanArray)>, ArrowError> {
                let mut predicates = match distinct {
                    DistinctValues::Int(set) => set
                        .into_iter()
                        .map(|x| {
                            Ok((
                                Some(Value::Int(x)),
                                eq(&PrimitiveArray::<Int32Type>::new_scalar(x), value)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, ArrowError>>(),
                    DistinctValues::Long(set) => set
                        .into_iter()
                        .map(|x| {
                            Ok((
                                Some(Value::LongInt(x)),
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
                            Ok((Some(partition_value), eq(&scalar, value)?))
                        })
                        .collect::<Result<Vec<_>, ArrowError>>(),
                    DistinctValues::String(set) => set
                        .into_iter()
                        .map(|x| {
                            let res = eq(&StringArray::new_scalar(&x), value)?;
                            Ok((Some(Value::String(x)), res))
                        })
                        .collect::<Result<Vec<_>, ArrowError>>(),
                }?;
                if value.null_count() != 0 {
                    predicates.push((None, is_null(value.as_ref())?));
                }
                Ok(predicates)
            },
        )
        .try_fold(
            vec![(vec![], BooleanArray::new(true_buffer.finish(), None))],
            |acc, predicates| {
                iproduct!(acc, predicates?.iter())
                    .map(|((mut values, x), (value, y))| {
                        values.push(value.clone());
                        Ok((values, and(&x, y)?))
                    })
                    .filter_ok(|x| x.1.true_count() != 0)
                    .collect::<Result<Vec<(Vec<Option<Value>>, _)>, ArrowError>>()
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
    let array = if array.null_count() == 0 {
        array
    } else {
        filter(array.as_ref(), &is_not_null(array.as_ref())?)?
    };
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

    if array.is_empty() {
        return Ok(HashSet::new());
    }

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
    let array = as_string_array(&array);

    if array.is_empty() {
        return Ok(HashSet::new());
    }

    let slice_len = array.len() - 1;

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
                vec![Some(Value::TimestampTZ(10)), Some(Value::TimestampTZ(20))]
            } else {
                vec![Some(Value::Timestamp(10)), Some(Value::Timestamp(20))]
            };
            assert_eq!(values, expected);
        }
        Ok(())
    }

    #[test]
    fn nullable_partition_values_keep_every_row() -> Result<(), ArrowError> {
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
            let mut groups = partition_record_batch(&batch, &[bound])?
                .map(|group| group.map(|(values, rows)| (values[0].clone(), rows.num_rows())))
                .collect::<Result<Vec<_>, ArrowError>>()?;
            groups.sort();
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0], (None, 1));
            assert_eq!(groups[1].1, 1);
        }
        Ok(())
    }

    #[test]
    fn all_null_and_empty_partition_batches() -> Result<(), ArrowError> {
        for values in [vec![None, None], vec![]] {
            let array: ArrayRef = Arc::new(arrow::array::Int64Array::from(values.clone()));
            let batch = RecordBatch::try_from_iter(vec![("id", array)])?;
            let source =
                StructField::new(1, "id", false, Type::Primitive(PrimitiveType::Long), None);
            let partition = PartitionField::new(1, 1000, "id", Transform::Identity);
            let bound = BoundPartitionField::new(&partition, &source);
            let groups = partition_record_batch(&batch, &[bound])?
                .collect::<Result<Vec<_>, ArrowError>>()?;
            if values.is_empty() {
                assert!(groups.is_empty());
            } else {
                assert_eq!(groups.len(), 1);
                assert_eq!(groups[0].0, vec![None]);
                assert_eq!(groups[0].1.num_rows(), values.len());
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "local partition-key benchmark; run with --ignored --nocapture"]
    fn benchmark_identity_partition_key_extraction() -> Result<(), ArrowError> {
        use std::hint::black_box;
        use std::time::Instant;

        const ROWS: u32 = 100_000;
        const REPEATS: u32 = 100;
        let values = (0..ROWS)
            .map(|index| i64::from(index % 1024))
            .collect::<Vec<_>>();
        let columns: [(&str, ArrayRef); 2] = [
            (
                "int64",
                Arc::new(arrow::array::Int64Array::from(values.clone())) as ArrayRef,
            ),
            (
                "timestamptz",
                Arc::new(TimestampMicrosecondArray::from(values).with_timezone("UTC")) as ArrayRef,
            ),
        ];
        for (name, column) in columns {
            let start = Instant::now();
            for _ in 0..REPEATS {
                black_box(distinct_values(Arc::clone(&column))?);
            }
            let elapsed = start.elapsed().as_secs_f64();
            eprintln!(
                "{name}: {:.1} million rows/s",
                f64::from(ROWS) * f64::from(REPEATS) / elapsed / 1_000_000.0
            );
        }
        Ok(())
    }
}
