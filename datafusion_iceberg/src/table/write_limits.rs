use std::collections::HashMap;

use datafusion::arrow::array::{
    Array, AsArray, GenericBinaryArray, GenericStringArray, OffsetSizeTrait,
};
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::DataFusionError;
use iceberg_rust::spec::schema::Schema;
use iceberg_rust::spec::types::{PrimitiveType, Type};

use crate::error::Error;

/// Optional Rustice/Snowflake VARCHAR character limit, keyed by Iceberg field ID.
pub const VARCHAR_LENGTH_PROPERTY_PREFIX: &str = "embucket.schema.varchar-length.";
/// Optional Rustice/Snowflake BINARY byte limit, keyed by Iceberg field ID.
pub const BINARY_LENGTH_PROPERTY_PREFIX: &str = "embucket.schema.binary-length.";

#[derive(Clone, Copy, Debug)]
enum Kind {
    Varchar,
    Binary,
}

impl Kind {
    const fn sql_name(self) -> &'static str {
        match self {
            Self::Varchar => "VARCHAR",
            Self::Binary => "BINARY",
        }
    }
}

#[derive(Debug)]
struct Limit {
    index: usize,
    name: String,
    kind: Kind,
    max: usize,
}

#[derive(Default, Debug)]
pub(super) struct WriteLimits(Vec<Limit>);

impl WriteLimits {
    pub(super) fn from_schema(
        schema: &Schema,
        properties: &HashMap<String, String>,
    ) -> Result<Self, DataFusionError> {
        if !properties.keys().any(|key| {
            key.starts_with(VARCHAR_LENGTH_PROPERTY_PREFIX)
                || key.starts_with(BINARY_LENGTH_PROPERTY_PREFIX)
        }) {
            return Ok(Self::default());
        }
        let mut limits = Vec::new();
        for (index, field) in schema.fields().iter().enumerate() {
            let (prefix, kind) = match &field.field_type {
                Type::Primitive(PrimitiveType::String) => {
                    (VARCHAR_LENGTH_PROPERTY_PREFIX, Kind::Varchar)
                }
                Type::Primitive(PrimitiveType::Binary) => {
                    (BINARY_LENGTH_PROPERTY_PREFIX, Kind::Binary)
                }
                _ => continue,
            };
            let key = format!("{prefix}{}", field.id);
            let Some(value) = properties.get(&key) else {
                continue;
            };
            let max = value.parse::<usize>().map_err(|_| {
                DataFusionError::Plan(format!("Invalid declared column length {key}={value}"))
            })?;
            limits.push(Limit {
                index,
                name: field.name.clone(),
                kind,
                max,
            });
        }
        Ok(Self(limits))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn validate_batch(&self, batch: &RecordBatch) -> Result<(), DataFusionError> {
        for limit in &self.0 {
            let array = batch.column(limit.index);
            let preview = match (limit.kind, array.data_type()) {
                (Kind::Varchar, DataType::Utf8) => {
                    first_overlong_string(array.as_string::<i32>(), limit.max).map(string_preview)
                }
                (Kind::Varchar, DataType::LargeUtf8) => {
                    first_overlong_string(array.as_string::<i64>(), limit.max).map(string_preview)
                }
                (Kind::Varchar, DataType::Utf8View) => {
                    let array = array.as_string_view();
                    (0..array.len())
                        .find(|&row| {
                            !array.is_null(row)
                                && view_byte_length(array.views()[row]) > limit.max
                                && string_overflows(array.value(row), limit.max)
                        })
                        .map(|row| string_preview(array.value(row)))
                }
                (Kind::Binary, DataType::Binary) => {
                    first_overlong_binary(array.as_binary::<i32>(), limit.max).map(binary_preview)
                }
                (Kind::Binary, DataType::LargeBinary) => {
                    first_overlong_binary(array.as_binary::<i64>(), limit.max).map(binary_preview)
                }
                (Kind::Binary, DataType::BinaryView) => {
                    let array = array.as_binary_view();
                    (0..array.len())
                        .find(|&row| {
                            !array.is_null(row) && view_byte_length(array.views()[row]) > limit.max
                        })
                        .map(|row| binary_preview(array.value(row)))
                }
                (_, actual) => {
                    return Err(DataFusionError::Internal(format!(
                        "Declared {} length for column {} cannot validate {actual}",
                        limit.kind.sql_name(),
                        limit.name
                    )));
                }
            };
            if let Some(preview) = preview {
                return Err(Error::DeclaredColumnLengthExceeded {
                    column: limit.name.clone(),
                    kind: match limit.kind {
                        Kind::Varchar => "String",
                        Kind::Binary => "Binary value",
                    },
                    limit: limit.max,
                    preview,
                }
                .into());
            }
        }
        Ok(())
    }
}

fn first_overlong_string<O: OffsetSizeTrait>(
    array: &GenericStringArray<O>,
    max: usize,
) -> Option<&str> {
    if array.value_data().len() <= max {
        return None;
    }
    (0..array.len())
        .filter(|&row| !array.is_null(row))
        .map(|row| array.value(row))
        .find(|value| string_overflows(value, max))
}

fn first_overlong_binary<O: OffsetSizeTrait>(
    array: &GenericBinaryArray<O>,
    max: usize,
) -> Option<&[u8]> {
    if array.value_data().len() <= max {
        return None;
    }
    (0..array.len())
        .filter(|&row| !array.is_null(row))
        .map(|row| array.value(row))
        .find(|value| value.len() > max)
}

fn string_overflows(value: &str, max: usize) -> bool {
    value.len() > max && value.chars().nth(max).is_some()
}

fn string_preview(value: &str) -> String {
    value.chars().take(128).collect()
}

fn binary_preview(value: &[u8]) -> String {
    value
        .iter()
        .take(64)
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

fn view_byte_length(view: u128) -> usize {
    usize::try_from(view & u128::from(u32::MAX)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{
        ArrayRef, BinaryArray, BinaryViewArray, StringArray, StringViewArray,
    };
    use datafusion::arrow::datatypes::{Field, Schema as ArrowSchema};
    use iceberg_rust::spec::types::{StructField, StructType};

    use super::*;

    fn table_schema() -> Schema {
        Schema::from_struct_type(
            StructType::new(vec![
                StructField::new(
                    7,
                    "text",
                    false,
                    Type::Primitive(PrimitiveType::String),
                    None,
                ),
                StructField::new(
                    9,
                    "bytes",
                    false,
                    Type::Primitive(PrimitiveType::Binary),
                    None,
                ),
            ]),
            0,
            None,
        )
    }

    fn batch(text: ArrayRef, bytes: ArrayRef) -> RecordBatch {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("text", text.data_type().clone(), true),
            Field::new("bytes", bytes.data_type().clone(), true),
        ]));
        RecordBatch::try_new(schema, vec![text, bytes]).expect("valid batch")
    }

    #[test]
    fn enforces_field_id_limits_without_treating_unicode_bytes_as_characters() {
        let properties = HashMap::from([
            (format!("{VARCHAR_LENGTH_PROPERTY_PREFIX}7"), "3".to_owned()),
            (format!("{BINARY_LENGTH_PROPERTY_PREFIX}9"), "2".to_owned()),
        ]);
        let limits = WriteLimits::from_schema(&table_schema(), &properties).expect("limits");
        limits
            .validate_batch(&batch(
                Arc::new(StringArray::from(vec![Some("ééé"), None])),
                Arc::new(BinaryArray::from(vec![Some(&b"ab"[..]), None])),
            ))
            .expect("within limits");
        let text_error = limits
            .validate_batch(&batch(
                Arc::new(StringViewArray::from(vec!["éééé"])),
                Arc::new(BinaryViewArray::from(vec![&b"ab"[..]])),
            ))
            .expect_err("four code points exceed VARCHAR(3)");
        assert!(text_error.to_string().contains("String 'éééé' is too long"));
        let binary_error = limits
            .validate_batch(&batch(
                Arc::new(StringArray::from(vec!["abc"])),
                Arc::new(BinaryViewArray::from(vec![&b"abc"[..]])),
            ))
            .expect_err("three bytes exceed BINARY(2)");
        assert!(binary_error
            .to_string()
            .contains("Binary value '616263' is too long"));
    }

    #[test]
    fn tables_without_limits_take_the_empty_fast_path() {
        assert!(WriteLimits::from_schema(&table_schema(), &HashMap::new())
            .expect("no limits")
            .is_empty());
    }
}
