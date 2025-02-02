/*!
 * Schemas
*/
use std::{fmt, ops::Deref, str};

use derive_getters::Getters;
use serde::{Deserialize, Serialize};
use super::types::{StructType, StructTypeBuilder};

use crate::error::Error;

use super::types::StructType;

pub static DEFAULT_SCHEMA_ID: i32 = 0;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Getters)]
#[serde(rename_all = "kebab-case")]
/// Names and types of fields in a table.
pub struct Schema {
    /// Identifier of the schema
    #[builder(default = "DEFAULT_SCHEMA_ID")]
    schema_id: i32,
    /// Set of primitive fields that identify rows in a table.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[builder(setter(into, strip_option), default)]
    identifier_field_ids: Option<Vec<i32>>,

    #[serde(flatten)]
    /// The struct fields
    fields: StructType,
}

impl Deref for Schema {
    type Target = StructType;
    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

#[derive(Default)]
pub struct SchemaBuilder {
    schema_id: Option<i32>,
    identifier_field_ids: Option<Vec<i32>>,
    fields: Option<StructTypeBuilder>,
}

impl SchemaBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_schema_id(&mut self, schema_id: i32) -> &mut Self {
        self.schema_id = Some(schema_id);
        self
    }

    pub fn with_identifier_field_ids(&mut self, ids: Vec<i32>) -> &mut Self {
        self.identifier_field_ids = Some(ids);
        self
    }

    pub fn with_fields(&mut self, fields: StructTypeBuilder) -> &mut Self {
        self.fields = Some(fields);
        self
    }

    pub fn build(&mut self) -> Result<Schema, Error> {
        let fields = self.fields
            .take()
            .ok_or_else(|| Error::InvalidFormat("Schema fields must be set".to_string()))?
            .build()?;

        Ok(Schema {
            schema_id: self.schema_id.unwrap_or(DEFAULT_SCHEMA_ID),
            identifier_field_ids: self.identifier_field_ids.take(),
            fields,
        })
    }
}

impl Schema {
    pub fn builder() -> SchemaBuilder {
        SchemaBuilder::new()
    }

    pub fn project(&self, ids: &[i32]) -> Schema {
        Schema {
            schema_id: self.schema_id,
            identifier_field_ids: self.identifier_field_ids.as_ref().map(|x| {
                x.iter()
                    .filter(|x| ids.contains(x))
                    .map(ToOwned::to_owned)
                    .collect()
            }),
            fields: StructType::new(
                self.fields()
                    .iter()
                    .filter(|x| ids.contains(&x.id))
                    .map(ToOwned::to_owned)
                    .collect(),
            ),
        }
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            &serde_json::to_string(self).map_err(|_| fmt::Error)?,
        )
    }
}

impl str::FromStr for Schema {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s).map_err(Error::from)
    }
}

impl TryFrom<SchemaV2> for Schema {
    type Error = Error;
    fn try_from(value: SchemaV2) -> Result<Self, Self::Error> {
        Ok(Schema {
            schema_id: value.schema_id,
            identifier_field_ids: value.identifier_field_ids,
            fields: value.fields,
        })
    }
}

impl TryFrom<SchemaV1> for Schema {
    type Error = Error;
    fn try_from(value: SchemaV1) -> Result<Self, Self::Error> {
        Ok(Schema {
            schema_id: value.schema_id.unwrap_or(0),
            identifier_field_ids: value.identifier_field_ids,
            fields: value.fields,
        })
    }
}

impl From<Schema> for SchemaV2 {
    fn from(value: Schema) -> Self {
        SchemaV2 {
            schema_id: value.schema_id,
            identifier_field_ids: value.identifier_field_ids,
            fields: value.fields,
        }
    }
}

impl From<Schema> for SchemaV1 {
    fn from(value: Schema) -> Self {
        SchemaV1 {
            schema_id: Some(value.schema_id),
            identifier_field_ids: value.identifier_field_ids,
            fields: value.fields,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "kebab-case")]
/// Names and types of fields in a table.
pub struct SchemaV2 {
    /// Identifier of the schema
    pub schema_id: i32,
    /// Set of primitive fields that identify rows in a table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier_field_ids: Option<Vec<i32>>,

    #[serde(flatten)]
    /// The struct fields
    pub fields: StructType,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "kebab-case")]
/// Names and types of fields in a table.
pub struct SchemaV1 {
    /// Identifier of the schema
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<i32>,
    /// Set of primitive fields that identify rows in a table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier_field_ids: Option<Vec<i32>>,

    #[serde(flatten)]
    /// The struct fields
    pub fields: StructType,
}

impl From<SchemaV1> for SchemaV2 {
    fn from(v1: SchemaV1) -> Self {
        SchemaV2 {
            schema_id: v1.schema_id.unwrap_or(0),
            identifier_field_ids: v1.identifier_field_ids,
            fields: v1.fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::spec::types::{PrimitiveType, Type};

    use super::*;

    #[test]
    fn schema() {
        let record = r#"
        {
            "type": "struct",
            "schema-id": 1,
            "fields": [ {
            "id": 1,
            "name": "id",
            "required": true,
            "type": "uuid"
            }, {
            "id": 2,
            "name": "data",
            "required": false,
            "type": "int"
            } ]
            }
        "#;

        let result: SchemaV2 = serde_json::from_str(record).unwrap();
        assert_eq!(1, result.schema_id);
        assert_eq!(
            Type::Primitive(PrimitiveType::Uuid),
            result.fields[0].field_type
        );
        assert_eq!(1, result.fields[0].id);
        assert!(result.fields[0].required);

        assert_eq!(
            Type::Primitive(PrimitiveType::Int),
            result.fields[1].field_type
        );
        assert_eq!(2, result.fields[1].id);
        assert!(!result.fields[1].required);
    }
}
