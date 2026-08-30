/*!
 * Convert between datafusion and iceberg schema
*/

use std::{collections::HashMap, convert::TryInto, sync::Arc};

use crate::{
    spec::types::{PrimitiveType, StructField, StructType, Type, VariantType},
    types::ListType,
};
use arrow_schema::{DataType, Field, Fields, Schema as ArrowSchema, TimeUnit};

use crate::error::Error;

pub const PARQUET_FIELD_ID_META_KEY: &str = "PARQUET:field_id";
pub const ARROW_EXTENSION_TYPE_NAME_KEY: &str = "ARROW:extension:name";
pub const PARQUET_VARIANT_EXTENSION_NAME: &str = "arrow.parquet.variant";

fn arrow_field(
    name: &str,
    field_type: &Type,
    nullable: bool,
    mut metadata: HashMap<String, String>,
) -> Result<Field, Error> {
    if matches!(field_type, Type::Variant(_)) {
        metadata.insert(
            ARROW_EXTENSION_TYPE_NAME_KEY.to_string(),
            PARQUET_VARIANT_EXTENSION_NAME.to_string(),
        );
    }
    Ok(Field::new(name, field_type.try_into()?, nullable).with_metadata(metadata))
}

fn iceberg_type(field: &Field) -> Result<Type, Error> {
    if field.extension_type_name() == Some(PARQUET_VARIANT_EXTENSION_NAME) {
        if !matches!(field.data_type(), DataType::Struct(_)) {
            return Err(Error::NotSupported(format!(
                "Arrow {PARQUET_VARIANT_EXTENSION_NAME} extension requires Struct storage"
            )));
        }
        Ok(Type::Variant(VariantType))
    } else {
        field.data_type().try_into()
    }
}

impl TryInto<ArrowSchema> for &StructType {
    type Error = Error;

    fn try_into(self) -> Result<ArrowSchema, Self::Error> {
        let fields = self.try_into()?;
        let metadata = HashMap::new();
        Ok(ArrowSchema { fields, metadata })
    }
}

impl TryInto<Fields> for &StructType {
    type Error = Error;

    fn try_into(self) -> Result<Fields, Self::Error> {
        let fields = self
            .iter()
            .map(|field| {
                arrow_field(
                    &field.name,
                    &field.field_type,
                    !field.required,
                    HashMap::from_iter(vec![(
                        PARQUET_FIELD_ID_META_KEY.to_string(),
                        field.id.to_string(),
                    )]),
                )
            })
            .collect::<Result<_, Error>>()?;
        Ok(fields)
    }
}

impl TryFrom<&ArrowSchema> for StructType {
    type Error = Error;

    fn try_from(value: &ArrowSchema) -> Result<Self, Self::Error> {
        value.fields().try_into()
    }
}

impl TryFrom<&Fields> for StructType {
    type Error = Error;

    fn try_from(value: &Fields) -> Result<Self, Self::Error> {
        let fields = value
            .iter()
            .map(|field| {
                Ok(StructField {
                    id: get_field_id(field)?,
                    name: field.name().to_owned(),
                    required: !field.is_nullable(),
                    field_type: iceberg_type(field)?,
                    doc: None,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(StructType::new(fields))
    }
}

impl TryFrom<&Type> for DataType {
    type Error = Error;

    fn try_from(value: &Type) -> Result<Self, Self::Error> {
        match value {
            Type::Primitive(primitive) => match primitive {
                PrimitiveType::Boolean => Ok(DataType::Boolean),
                PrimitiveType::Int => Ok(DataType::Int32),
                PrimitiveType::Long => Ok(DataType::Int64),
                PrimitiveType::Float => Ok(DataType::Float32),
                PrimitiveType::Double => Ok(DataType::Float64),
                PrimitiveType::Decimal { precision, scale } => {
                    Ok(DataType::Decimal128(*precision as u8, *scale as i8))
                }
                PrimitiveType::Date => Ok(DataType::Date32),
                PrimitiveType::Time => Ok(DataType::Time64(TimeUnit::Microsecond)),
                PrimitiveType::Timestamp => Ok(DataType::Timestamp(TimeUnit::Microsecond, None)),
                PrimitiveType::Timestamptz => Ok(DataType::Timestamp(
                    TimeUnit::Microsecond,
                    Some(Arc::from("UTC")),
                )),
                PrimitiveType::String => Ok(DataType::Utf8),
                PrimitiveType::Uuid => Ok(DataType::Utf8),
                PrimitiveType::Fixed(len) => Ok(DataType::FixedSizeBinary(*len as i32)),
                PrimitiveType::Binary => Ok(DataType::Binary),
            },
            Type::List(list) => Ok(DataType::List(Arc::new(arrow_field(
                "item",
                &list.element,
                !list.element_required,
                HashMap::from_iter(vec![(
                    PARQUET_FIELD_ID_META_KEY.to_string(),
                    list.element_id.to_string(),
                )]),
            )?))),
            Type::Struct(struc) => Ok(DataType::Struct(struc.try_into()?)),
            Type::Map(map) => Ok(DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        arrow_field(
                            "key",
                            &map.key,
                            false,
                            HashMap::from_iter(vec![(
                                PARQUET_FIELD_ID_META_KEY.to_string(),
                                map.key_id.to_string(),
                            )]),
                        )?,
                        arrow_field(
                            "value",
                            &map.value,
                            !map.value_required,
                            HashMap::from_iter(vec![(
                                PARQUET_FIELD_ID_META_KEY.to_string(),
                                map.value_id.to_string(),
                            )]),
                        )?,
                    ])),
                    false,
                )),
                false,
            )),
            Type::Variant(_) => Ok(DataType::Struct(Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, true),
            ]))),
        }
    }
}

impl TryFrom<&DataType> for Type {
    type Error = Error;

    fn try_from(value: &DataType) -> Result<Self, Self::Error> {
        match value {
            DataType::Boolean => Ok(Type::Primitive(PrimitiveType::Boolean)),
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::UInt32 => {
                Ok(Type::Primitive(PrimitiveType::Int))
            }
            DataType::Int64 | DataType::UInt64 => Ok(Type::Primitive(PrimitiveType::Long)),
            DataType::Float32 => Ok(Type::Primitive(PrimitiveType::Float)),
            DataType::Float64 => Ok(Type::Primitive(PrimitiveType::Double)),
            DataType::Decimal128(precision, scale) => Ok(Type::Primitive(PrimitiveType::Decimal {
                precision: *precision as u32,
                scale: *scale as u32,
            })),
            DataType::Date32 => Ok(Type::Primitive(PrimitiveType::Date)),
            DataType::Time64(_) => Ok(Type::Primitive(PrimitiveType::Time)),
            DataType::Timestamp(_, _) => Ok(Type::Primitive(PrimitiveType::Timestamp)),
            DataType::Utf8 => Ok(Type::Primitive(PrimitiveType::String)),
            DataType::Utf8View => Ok(Type::Primitive(PrimitiveType::String)),
            DataType::FixedSizeBinary(len) => {
                Ok(Type::Primitive(PrimitiveType::Fixed(*len as u64)))
            }
            DataType::Binary => Ok(Type::Primitive(PrimitiveType::Binary)),
            DataType::Struct(fields) => Ok(Type::Struct(fields.try_into()?)),
            DataType::List(field) => Ok(Type::List(ListType {
                element_id: get_field_id(field)?,
                element_required: !field.is_nullable(),
                element: Box::new(iceberg_type(field)?),
            })),
            x => Err(Error::NotSupported(format!(
                "Arrow datatype {x} is not supported."
            ))),
        }
    }
}

fn get_field_id(field: &Field) -> Result<i32, Error> {
    field
        .metadata()
        .get(PARQUET_FIELD_ID_META_KEY)
        .ok_or(Error::NotFound(format!(
            "Parquet field id of field {field}"
        )))
        .and_then(|x| x.parse().map_err(Error::from))
}

pub fn new_fields_with_ids(fields: &Fields, index: &mut i32) -> Fields {
    fields
        .into_iter()
        .map(|field| {
            *index += 1;
            let field_id = *index;
            let with_field_id = |field: Field, source: &Field, id: i32| {
                let mut metadata = source.metadata().clone();
                metadata.insert(PARQUET_FIELD_ID_META_KEY.to_string(), id.to_string());
                field.with_metadata(metadata)
            };

            if field.extension_type_name() == Some(PARQUET_VARIANT_EXTENSION_NAME) {
                return with_field_id(field.as_ref().clone(), field, field_id);
            }

            match field.data_type() {
                DataType::Struct(fields) => {
                    let new_field = Field::new(
                        field.name(),
                        DataType::Struct(new_fields_with_ids(fields, index)),
                        field.is_nullable(),
                    );
                    with_field_id(new_field, field, field_id)
                }
                DataType::List(list_field) => {
                    *index += 1;
                    let element_id = *index;
                    let element =
                        with_field_id(list_field.as_ref().clone(), list_field, element_id);
                    let new_field = Field::new(
                        field.name(),
                        DataType::List(Arc::new(element)),
                        field.is_nullable(),
                    );
                    with_field_id(new_field, field, field_id)
                }
                _ => with_field_id(field.as_ref().clone(), field, field_id),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::types::MapType;

    #[test]
    fn test_struct_type_to_arrow_schema_simple() {
        let struct_type = StructType::new(vec![
            StructField::new(1, "field1", true, Type::Primitive(PrimitiveType::Int), None),
            StructField::new(
                2,
                "field2",
                false,
                Type::Primitive(PrimitiveType::String),
                None,
            ),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        assert_eq!(arrow_schema.fields().len(), 2);
        assert_eq!(arrow_schema.field(0).name(), "field1");
        assert_eq!(get_field_id(arrow_schema.field(0)).unwrap(), 1);
        assert_eq!(arrow_schema.field(0).data_type(), &DataType::Int32);
        assert!(!arrow_schema.field(0).is_nullable());
        assert_eq!(arrow_schema.field(1).name(), "field2");
        assert_eq!(get_field_id(arrow_schema.field(1)).unwrap(), 2);
        assert_eq!(arrow_schema.field(1).data_type(), &DataType::Utf8);
        assert!(arrow_schema.field(1).is_nullable());
    }

    #[test]
    fn test_struct_type_to_arrow_schema_nested() {
        let nested_struct = StructType::new(vec![
            StructField::new(
                3,
                "nested1",
                true,
                Type::Primitive(PrimitiveType::Long),
                None,
            ),
            StructField::new(
                4,
                "nested2",
                false,
                Type::Primitive(PrimitiveType::Boolean),
                None,
            ),
        ]);

        let struct_type = StructType::new(vec![
            StructField::new(1, "field1", true, Type::Primitive(PrimitiveType::Int), None),
            StructField::new(2, "field2", false, Type::Struct(nested_struct), None),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        assert_eq!(arrow_schema.fields().len(), 2);
        assert_eq!(arrow_schema.field(0).name(), "field1");
        assert_eq!(get_field_id(arrow_schema.field(0)).unwrap(), 1);
        assert_eq!(arrow_schema.field(0).data_type(), &DataType::Int32);
        assert!(!arrow_schema.field(0).is_nullable());

        let nested_field = arrow_schema.field(1);
        assert_eq!(nested_field.name(), "field2");
        assert_eq!(get_field_id(nested_field).unwrap(), 2);
        assert!(nested_field.is_nullable());

        if let DataType::Struct(nested_fields) = nested_field.data_type() {
            assert_eq!(nested_fields.len(), 2);
            assert_eq!(nested_fields[0].name(), "nested1");
            assert_eq!(get_field_id(&nested_fields[0]).unwrap(), 3);
            assert_eq!(nested_fields[0].data_type(), &DataType::Int64);
            assert!(!nested_fields[0].is_nullable());
            assert_eq!(nested_fields[1].name(), "nested2");
            assert_eq!(get_field_id(&nested_fields[1]).unwrap(), 4);
            assert_eq!(nested_fields[1].data_type(), &DataType::Boolean);
            assert!(nested_fields[1].is_nullable());
        } else {
            panic!("Expected nested field to be a struct");
        }
    }

    #[test]
    fn test_struct_type_to_arrow_schema_list() {
        let list_type = Type::List(ListType {
            element_id: 3,
            element_required: false,
            element: Box::new(Type::Primitive(PrimitiveType::Double)),
        });

        let struct_type = StructType::new(vec![
            StructField::new(1, "field1", true, Type::Primitive(PrimitiveType::Int), None),
            StructField::new(2, "field2", false, list_type, None),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        assert_eq!(arrow_schema.fields().len(), 2);
        assert_eq!(arrow_schema.field(0).name(), "field1");
        assert_eq!(get_field_id(arrow_schema.field(0)).unwrap(), 1);
        assert_eq!(arrow_schema.field(0).data_type(), &DataType::Int32);
        assert!(!arrow_schema.field(0).is_nullable());

        let list_field = arrow_schema.field(1);
        assert_eq!(list_field.name(), "field2");
        assert_eq!(get_field_id(list_field).unwrap(), 2);
        assert!(list_field.is_nullable());

        if let DataType::List(element_field) = list_field.data_type() {
            assert_eq!(element_field.data_type(), &DataType::Float64);
            assert_eq!(get_field_id(element_field).unwrap(), 3);
            assert!(element_field.is_nullable());
        } else {
            panic!("Expected list field");
        }
    }

    #[test]
    fn test_struct_type_to_arrow_schema_map() {
        let map_type = Type::Map(MapType {
            key_id: 3,
            value_id: 4,
            value_required: false,
            key: Box::new(Type::Primitive(PrimitiveType::String)),
            value: Box::new(Type::Primitive(PrimitiveType::Int)),
        });

        let struct_type = StructType::new(vec![
            StructField::new(1, "field1", true, Type::Primitive(PrimitiveType::Int), None),
            StructField::new(2, "field2", false, map_type, None),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        assert_eq!(arrow_schema.fields().len(), 2);
        assert_eq!(arrow_schema.field(0).name(), "field1");
        assert_eq!(get_field_id(arrow_schema.field(0)).unwrap(), 1);
        assert_eq!(arrow_schema.field(0).data_type(), &DataType::Int32);
        assert!(!arrow_schema.field(0).is_nullable());

        let map_field = arrow_schema.field(1);
        assert_eq!(map_field.name(), "field2");
        assert_eq!(get_field_id(map_field).unwrap(), 2);
        assert!(map_field.is_nullable());

        if let DataType::Map(entries_field, _) = map_field.data_type() {
            if let DataType::Struct(entry_fields) = entries_field.data_type() {
                assert_eq!(entry_fields.len(), 2);
                assert_eq!(entry_fields[0].name(), "key");
                assert_eq!(get_field_id(&entry_fields[0]).unwrap(), 3);
                assert_eq!(entry_fields[0].data_type(), &DataType::Utf8);
                assert!(!entry_fields[0].is_nullable());
                assert_eq!(entry_fields[1].name(), "value");
                assert_eq!(get_field_id(&entry_fields[1]).unwrap(), 4);
                assert_eq!(entry_fields[1].data_type(), &DataType::Int32);
                assert!(entry_fields[1].is_nullable());
            } else {
                panic!("Expected struct field for map entries");
            }
        } else {
            panic!("Expected map field");
        }
    }

    #[test]
    fn test_struct_type_to_arrow_schema_complex() {
        let nested_struct = StructType::new(vec![
            StructField::new(
                4,
                "nested1",
                true,
                Type::Primitive(PrimitiveType::Long),
                None,
            ),
            StructField::new(
                5,
                "nested2",
                false,
                Type::Primitive(PrimitiveType::Boolean),
                None,
            ),
        ]);

        let list_type = Type::List(ListType {
            element_id: 3,
            element_required: true,
            element: Box::new(Type::Struct(nested_struct)),
        });

        let map_type = Type::Map(MapType {
            key_id: 7,
            value_id: 8,
            value_required: false,
            key: Box::new(Type::Primitive(PrimitiveType::String)),
            value: Box::new(Type::Primitive(PrimitiveType::Date)),
        });

        let struct_type = StructType::new(vec![
            StructField::new(1, "field1", true, Type::Primitive(PrimitiveType::Int), None),
            StructField::new(2, "field2", false, list_type, None),
            StructField::new(6, "field3", true, map_type, None),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        assert_eq!(arrow_schema.fields().len(), 3);
        // Assertions for field1 (simple int)
        assert_eq!(arrow_schema.field(0).name(), "field1");
        assert_eq!(get_field_id(arrow_schema.field(0)).unwrap(), 1);
        assert_eq!(arrow_schema.field(0).data_type(), &DataType::Int32);
        assert!(!arrow_schema.field(0).is_nullable());

        // Assertions for field2 (list of structs)
        let list_field = arrow_schema.field(1);
        assert_eq!(list_field.name(), "field2");
        assert_eq!(get_field_id(list_field).unwrap(), 2);
        assert!(list_field.is_nullable());
        if let DataType::List(element_field) = list_field.data_type() {
            if let DataType::Struct(nested_fields) = element_field.data_type() {
                assert_eq!(nested_fields.len(), 2);
                assert_eq!(nested_fields[0].name(), "nested1");
                assert_eq!(get_field_id(&nested_fields[0]).unwrap(), 4);
                assert_eq!(nested_fields[0].data_type(), &DataType::Int64);
                assert!(!nested_fields[0].is_nullable());
                assert_eq!(nested_fields[1].name(), "nested2");
                assert_eq!(get_field_id(&nested_fields[1]).unwrap(), 5);
                assert_eq!(nested_fields[1].data_type(), &DataType::Boolean);
                assert!(nested_fields[1].is_nullable());
            } else {
                panic!("Expected struct as list element");
            }
        } else {
            panic!("Expected list field");
        }

        // Assertions for field3 (map of string to list of structs)
        let map_field = arrow_schema.field(2);
        assert_eq!(map_field.name(), "field3");
        assert_eq!(get_field_id(map_field).unwrap(), 6);
        assert!(!map_field.is_nullable());
        if let DataType::Map(entries_field, _) = map_field.data_type() {
            if let DataType::Struct(entry_fields) = entries_field.data_type() {
                assert_eq!(entry_fields.len(), 2);
                assert_eq!(entry_fields[0].name(), "key");
                assert_eq!(get_field_id(&entry_fields[0]).unwrap(), 7);
                assert_eq!(entry_fields[0].data_type(), &DataType::Utf8);
                assert!(!entry_fields[0].is_nullable());

                // Check the value (list of structs)
                assert_eq!(entry_fields[1].name(), "value");
                assert_eq!(get_field_id(&entry_fields[1]).unwrap(), 8);
                assert!(entry_fields[1].is_nullable());
            } else {
                panic!("Expected struct field for map entries");
            }
        } else {
            panic!("Expected map field");
        }
    }

    #[test]
    fn test_struct_type_to_arrow_schema_empty() {
        let struct_type = StructType::new(vec![]);
        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();
        assert_eq!(arrow_schema.fields().len(), 0);
    }

    #[test]
    fn test_struct_type_to_arrow_schema_metadata() {
        let struct_type = StructType::new(vec![StructField::new(
            1,
            "field1",
            true,
            Type::Primitive(PrimitiveType::Int),
            None,
        )]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();

        // Check that the PARQUET:field_id metadata is set correctly
        let field_metadata = arrow_schema.field(0).metadata();
        assert_eq!(
            field_metadata.get(PARQUET_FIELD_ID_META_KEY),
            Some(&"1".to_string())
        );
    }

    use arrow_schema::DataType;
    use std::sync::Arc;

    #[test]
    fn test_arrow_schema_to_struct_type_simple() {
        let arrow_schema = ArrowSchema::new(vec![
            Field::new("field1", DataType::Int32, false).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "1".to_string(),
            )])),
            Field::new("field2", DataType::Utf8, true).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "2".to_string(),
            )])),
            Field::new("field3", DataType::Int16, true).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "3".to_string(),
            )])),
        ]);

        let struct_type: StructType = (&arrow_schema).try_into().unwrap();

        assert_eq!(struct_type[0].id, 1);
        assert_eq!(struct_type[0].name, "field1");
        assert!(struct_type[0].required);
        assert_eq!(
            struct_type[0].field_type,
            Type::Primitive(PrimitiveType::Int)
        );
        assert_eq!(struct_type[1].id, 2);
        assert_eq!(struct_type[1].name, "field2");
        assert!(!struct_type[1].required);
        assert_eq!(
            struct_type[1].field_type,
            Type::Primitive(PrimitiveType::String)
        );
        assert_eq!(struct_type[2].id, 3);
        assert_eq!(struct_type[2].name, "field3");
        assert!(!struct_type[2].required);
        assert_eq!(
            struct_type[2].field_type,
            Type::Primitive(PrimitiveType::Int)
        );
    }

    #[test]
    fn test_arrow_schema_to_struct_type_nested() {
        let nested_fields = Fields::from(vec![
            Field::new("nested1", DataType::Int64, true).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "3".to_string(),
            )])),
            Field::new("nested2", DataType::Boolean, false).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "4".to_string(),
            )])),
        ]);

        let arrow_schema = ArrowSchema::new(vec![
            Field::new("field1", DataType::Int32, false).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "1".to_string(),
            )])),
            Field::new("field2", DataType::Struct(nested_fields), true).with_metadata(
                HashMap::from([(PARQUET_FIELD_ID_META_KEY.to_string(), "2".to_string())]),
            ),
        ]);

        let struct_type: StructType = (&arrow_schema).try_into().unwrap();

        assert_eq!(struct_type[0].id, 1);
        assert_eq!(struct_type[0].name, "field1");
        assert!(struct_type[0].required);
        assert_eq!(
            struct_type[0].field_type,
            Type::Primitive(PrimitiveType::Int)
        );

        match &struct_type[1].field_type {
            Type::Struct(nested_struct) => {
                assert_eq!(nested_struct[0].id, 3);
                assert_eq!(nested_struct[0].name, "nested1");
                assert!(!nested_struct[0].required);
                assert_eq!(
                    nested_struct[0].field_type,
                    Type::Primitive(PrimitiveType::Long)
                );
                assert_eq!(nested_struct[1].id, 4);
                assert_eq!(nested_struct[1].name, "nested2");
                assert!(nested_struct[1].required);
                assert_eq!(
                    nested_struct[1].field_type,
                    Type::Primitive(PrimitiveType::Boolean)
                );
            }
            _ => panic!("Expected nested struct"),
        }
    }

    #[test]
    fn test_arrow_schema_to_struct_type_list() {
        let arrow_schema = ArrowSchema::new(vec![
            Field::new("field1", DataType::Int32, false).with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "1".to_string(),
            )])),
            Field::new(
                "field2",
                DataType::List(Arc::new(
                    Field::new("item", DataType::Float64, true).with_metadata(HashMap::from([(
                        PARQUET_FIELD_ID_META_KEY.to_string(),
                        "3".to_string(),
                    )])),
                )),
                true,
            )
            .with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "2".to_string(),
            )])),
        ]);

        let struct_type: StructType = (&arrow_schema).try_into().unwrap();

        assert_eq!(struct_type[0].id, 1);
        assert_eq!(struct_type[0].name, "field1");
        assert!(struct_type[0].required);
        assert_eq!(
            struct_type[0].field_type,
            Type::Primitive(PrimitiveType::Int)
        );

        match &struct_type[1].field_type {
            Type::List(list_type) => {
                assert_eq!(list_type.element_id, 3);
                assert!(!list_type.element_required);
                assert_eq!(*list_type.element, Type::Primitive(PrimitiveType::Double));
            }
            _ => panic!("Expected list type"),
        }
    }

    // #[test]
    // fn test_arrow_schema_to_struct_type_map() {
    //     let arrow_schema = ArrowSchema::new(vec![
    //         Field::new("field1", DataType::Int32, false).with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "1".to_string(),
    //         )])),
    //         Field::new(
    //             "field2",
    //             DataType::Map(
    //                 Arc::new(Field::new(
    //                     "entries",
    //                     DataType::Struct(Fields::from(vec![
    //                         Field::new("key", DataType::Utf8, false).with_metadata(HashMap::from(
    //                             [(PARQUET_FIELD_ID_META_KEY.to_string(), "3".to_string())],
    //                         )),
    //                         Field::new("value", DataType::Int32, true).with_metadata(
    //                             HashMap::from([(
    //                                 PARQUET_FIELD_ID_META_KEY.to_string(),
    //                                 "4".to_string(),
    //                             )]),
    //                         ),
    //                     ])),
    //                     false,
    //                 )),
    //                 false,
    //             ),
    //             true,
    //         )
    //         .with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "2".to_string(),
    //         )])),
    //     ]);

    //     let struct_type: StructType = (&arrow_schema).try_into().unwrap();

    //     assert_eq!(struct_type[0].id, 1);
    //     assert_eq!(struct_type[0].name, "field1");
    //     assert_eq!(struct_type[0].required, true);
    //     assert_eq!(
    //         struct_type[0].field_type,
    //         Type::Primitive(PrimitiveType::Int)
    //     );

    //     match &struct_type[1].field_type {
    //         Type::Map(map_type) => {
    //             assert_eq!(map_type.key_id, 3);
    //             assert_eq!(map_type.value_id, 4);
    //             assert_eq!(map_type.value_required, false);
    //             assert_eq!(*map_type.key, Type::Primitive(PrimitiveType::String));
    //             assert_eq!(*map_type.value, Type::Primitive(PrimitiveType::Int));
    //         }
    //         _ => panic!("Expected map type"),
    //     }
    // }

    // #[test]
    // fn test_arrow_schema_to_struct_type_complex() {
    //     let nested_fields = Fields::from(vec![
    //         Field::new("nested1", DataType::Int64, true).with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "4".to_string(),
    //         )])),
    //         Field::new("nested2", DataType::Boolean, false).with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "5".to_string(),
    //         )])),
    //     ]);

    //     let arrow_schema = ArrowSchema::new(vec![
    //         Field::new("field1", DataType::Int32, false).with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "1".to_string(),
    //         )])),
    //         Field::new(
    //             "field2",
    //             DataType::List(Arc::new(
    //                 Field::new("item", DataType::Struct(nested_fields), false).with_metadata(
    //                     HashMap::from([(PARQUET_FIELD_ID_META_KEY.to_string(), "3".to_string())]),
    //                 ),
    //             )),
    //             true,
    //         )
    //         .with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "2".to_string(),
    //         )])),
    //         Field::new(
    //             "field3",
    //             DataType::Map(
    //                 Arc::new(Field::new(
    //                     "entries",
    //                     DataType::Struct(Fields::from(vec![
    //                         Field::new("key", DataType::Utf8, false).with_metadata(HashMap::from(
    //                             [(PARQUET_FIELD_ID_META_KEY.to_string(), "7".to_string())],
    //                         )),
    //                         Field::new("value", DataType::Date32, true).with_metadata(
    //                             HashMap::from([(
    //                                 PARQUET_FIELD_ID_META_KEY.to_string(),
    //                                 "8".to_string(),
    //                             )]),
    //                         ),
    //                     ])),
    //                     false,
    //                 )),
    //                 false,
    //             ),
    //             false,
    //         )
    //         .with_metadata(HashMap::from([(
    //             PARQUET_FIELD_ID_META_KEY.to_string(),
    //             "6".to_string(),
    //         )])),
    //     ]);

    //     let struct_type: StructType = (&arrow_schema).try_into().unwrap();

    //     // Check field1
    //     assert_eq!(struct_type[0].id, 1);
    //     assert_eq!(struct_type[0].name, "field1");
    //     assert_eq!(struct_type[0].required, true);
    //     assert_eq!(
    //         struct_type[0].field_type,
    //         Type::Primitive(PrimitiveType::Int)
    //     );

    //     // Check field2 (list of structs)
    //     match &struct_type[1].field_type {
    //         Type::List(list_type) => {
    //             assert_eq!(list_type.element_id, 3);
    //             assert_eq!(list_type.element_required, true);
    //             match &*list_type.element {
    //                 Type::Struct(nested_struct) => {
    //                     assert_eq!(nested_struct[0].id, 4);
    //                     assert_eq!(nested_struct[0].name, "nested1");
    //                     assert_eq!(nested_struct[0].required, false);
    //                     assert_eq!(
    //                         nested_struct[0].field_type,
    //                         Type::Primitive(PrimitiveType::Long)
    //                     );
    //                     assert_eq!(nested_struct[1].id, 5);
    //                     assert_eq!(nested_struct[1].name, "nested2");
    //                     assert_eq!(nested_struct[1].required, true);
    //                     assert_eq!(
    //                         nested_struct[1].field_type,
    //                         Type::Primitive(PrimitiveType::Boolean)
    //                     );
    //                 }
    //                 _ => panic!("Expected nested struct in list"),
    //             }
    //         }
    //         _ => panic!("Expected list type"),
    //     }

    //     // Check field3 (map)
    //     match &struct_type[2].field_type {
    //         Type::Map(map_type) => {
    //             assert_eq!(map_type.key_id, 7);
    //             assert_eq!(map_type.value_id, 8);
    //             assert_eq!(map_type.value_required, false);
    //             assert_eq!(*map_type.key, Type::Primitive(PrimitiveType::String));
    //             assert_eq!(*map_type.value, Type::Primitive(PrimitiveType::Date));
    //         }
    //         _ => panic!("Expected map type"),
    //     }
    // }

    #[test]
    fn test_arrow_schema_to_struct_type_missing_field_id() {
        let arrow_schema = ArrowSchema::new(vec![Field::new("field1", DataType::Int32, false)]);

        let result: Result<StructType, Error> = (&arrow_schema).try_into();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::NotFound(_)));
    }

    #[test]
    fn test_arrow_schema_to_struct_type_invalid_field_id() {
        let arrow_schema = ArrowSchema::new(vec![Field::new("field1", DataType::Int32, false)
            .with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "invalid".to_string(),
            )]))]);

        let result: Result<StructType, Error> = (&arrow_schema).try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_arrow_schema_to_struct_type_unsupported_datatype() {
        let arrow_schema = ArrowSchema::new(vec![Field::new("field1", DataType::UInt8, false)
            .with_metadata(HashMap::from([(
                PARQUET_FIELD_ID_META_KEY.to_string(),
                "1".to_string(),
            )]))]);

        let result: Result<StructType, Error> = (&arrow_schema).try_into();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::NotSupported(_)));
    }

    #[test]
    fn test_nested_field_name() {
        let schema = crate::schema::Schema::builder()
            .with_schema_id(1)
            .with_struct_field(StructField::new(
                1,
                "nested_object",
                true,
                Type::Struct(StructType::new(vec![
                    StructField::new(
                        2,
                        "key1",
                        true,
                        Type::Primitive(PrimitiveType::String),
                        None,
                    ),
                    StructField::new(3, "key2", true, Type::Primitive(PrimitiveType::Int), None),
                ])),
                None,
            ))
            .build()
            .unwrap();

        let field_name = schema.get_name("nested_object.key1");
        assert!(field_name.is_some());
    }

    #[test]
    fn test_variant_arrow_schema_roundtrip() {
        let struct_type = StructType::new(vec![
            StructField::new(1, "payload", false, Type::Variant(VariantType), None),
            StructField::new(
                2,
                "items",
                false,
                Type::List(ListType {
                    element_id: 3,
                    element_required: false,
                    element: Box::new(Type::Variant(VariantType)),
                }),
                None,
            ),
        ]);

        let arrow_schema: ArrowSchema = (&struct_type).try_into().unwrap();
        let payload = arrow_schema.field_with_name("payload").unwrap();
        assert_eq!(
            payload.extension_type_name(),
            Some(PARQUET_VARIANT_EXTENSION_NAME)
        );
        assert_eq!(
            payload.data_type(),
            &DataType::Struct(Fields::from(vec![
                Field::new("metadata", DataType::BinaryView, false),
                Field::new("value", DataType::BinaryView, true),
            ]))
        );

        let DataType::List(element) = arrow_schema.field_with_name("items").unwrap().data_type()
        else {
            panic!("expected list");
        };
        assert_eq!(
            element.extension_type_name(),
            Some(PARQUET_VARIANT_EXTENSION_NAME)
        );

        let roundtrip = StructType::try_from(&arrow_schema).unwrap();
        assert_eq!(roundtrip, struct_type);
    }

    #[test]
    fn test_new_fields_with_ids_preserves_variant_extension() {
        let storage = DataType::Struct(Fields::from(vec![
            Field::new("metadata", DataType::BinaryView, false),
            Field::new("value", DataType::BinaryView, true),
        ]));
        let variant = Field::new("payload", storage, true).with_metadata(HashMap::from([(
            ARROW_EXTENSION_TYPE_NAME_KEY.to_string(),
            PARQUET_VARIANT_EXTENSION_NAME.to_string(),
        )]));

        let fields = new_fields_with_ids(&Fields::from(vec![variant]), &mut 0);
        let payload = &fields[0];
        assert_eq!(get_field_id(payload).unwrap(), 1);
        assert_eq!(
            payload.extension_type_name(),
            Some(PARQUET_VARIANT_EXTENSION_NAME)
        );

        let DataType::Struct(storage_fields) = payload.data_type() else {
            panic!("expected Variant struct storage");
        };
        assert!(storage_fields
            .iter()
            .all(|field| { !field.metadata().contains_key(PARQUET_FIELD_ID_META_KEY) }));
    }
}
