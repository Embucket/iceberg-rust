use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, Int64Array, RecordBatch};
use datafusion::arrow::compute::is_not_null;
use datafusion::arrow::compute::kernels::{numeric::add, zip::zip};
use datafusion::arrow::datatypes::{DataType, Field, FieldRef, Schema};
use datafusion::common::{exec_err, Result};
use datafusion::physical_plan::PhysicalExpr;
use datafusion_expr::ColumnarValue;
use iceberg_rust::spec::arrow::schema::PARQUET_FIELD_ID_META_KEY;
pub use iceberg_rust::spec::row_lineage::{
    LAST_UPDATED_SEQUENCE_NUMBER_COLUMN_NAME as LAST_UPDATED_SEQUENCE_NUMBER_COLUMN,
    LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID, ROW_ID_COLUMN_NAME as ROW_ID_COLUMN, ROW_ID_FIELD_ID,
};

pub(crate) const PHYSICAL_ROW_ID_COLUMN: &str = "__iceberg_physical_row_id";
pub(crate) const PHYSICAL_LAST_UPDATED_SEQUENCE_NUMBER_COLUMN: &str =
    "__iceberg_physical_last_updated_sequence_number";
pub(crate) const FIRST_ROW_ID_COLUMN: &str = "__iceberg_first_row_id";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RowLineageKind {
    RowId,
    LastUpdatedSequenceNumber,
}

#[derive(Debug, Eq)]
pub(crate) struct RowLineageExpr {
    kind: RowLineageKind,
    physical: Arc<dyn PhysicalExpr>,
    first_row_id: Arc<dyn PhysicalExpr>,
    fallback: Arc<dyn PhysicalExpr>,
}

impl RowLineageExpr {
    pub(crate) fn new(
        kind: RowLineageKind,
        physical: Arc<dyn PhysicalExpr>,
        first_row_id: Arc<dyn PhysicalExpr>,
        fallback: Arc<dyn PhysicalExpr>,
    ) -> Self {
        Self {
            kind,
            physical,
            first_row_id,
            fallback,
        }
    }

    fn output_field(&self) -> FieldRef {
        let (name, field_id) = match self.kind {
            RowLineageKind::RowId => (ROW_ID_COLUMN, ROW_ID_FIELD_ID),
            RowLineageKind::LastUpdatedSequenceNumber => (
                LAST_UPDATED_SEQUENCE_NUMBER_COLUMN,
                LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID,
            ),
        };
        Arc::new(
            Field::new(name, DataType::Int64, true).with_metadata(
                [(PARQUET_FIELD_ID_META_KEY.to_owned(), field_id.to_string())].into(),
            ),
        )
    }

    fn evaluate_int64(
        expr: &Arc<dyn PhysicalExpr>,
        batch: &RecordBatch,
        name: &str,
    ) -> Result<ArrayRef> {
        let array = expr.evaluate(batch)?.into_array(batch.num_rows())?;
        if array.data_type() != &DataType::Int64 {
            return exec_err!(
                "Iceberg row lineage {name} must be Int64, got {}",
                array.data_type()
            );
        }
        Ok(array)
    }
}

impl PartialEq for RowLineageExpr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.physical.eq(&other.physical)
            && self.first_row_id.eq(&other.first_row_id)
            && self.fallback.eq(&other.fallback)
    }
}

impl Hash for RowLineageExpr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.physical.hash(state);
        self.first_row_id.hash(state);
        self.fallback.hash(state);
    }
}

impl Display for RowLineageExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "iceberg_{:?}", self.kind)
    }
}

impl PhysicalExpr for RowLineageExpr {
    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(DataType::Int64)
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(true)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let physical = Self::evaluate_int64(&self.physical, batch, "physical column")?;
        let first_row_id = Self::evaluate_int64(&self.first_row_id, batch, "first_row_id")?;
        let fallback_source = Self::evaluate_int64(&self.fallback, batch, "fallback")?;

        let fallback = match self.kind {
            RowLineageKind::RowId => add(&first_row_id, &fallback_source)?,
            RowLineageKind::LastUpdatedSequenceNumber => fallback_source,
        };
        let value = zip(&is_not_null(&physical)?, &physical, &fallback)?;
        let nulls: ArrayRef = Arc::new(Int64Array::new_null(batch.num_rows()));
        let value = zip(&is_not_null(&first_row_id)?, &value, &nulls)?;
        Ok(ColumnarValue::Array(value))
    }

    fn return_field(&self, _input_schema: &Schema) -> Result<FieldRef> {
        Ok(self.output_field())
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.physical, &self.first_row_id, &self.fallback]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        let [physical, first_row_id, fallback]: [Arc<dyn PhysicalExpr>; 3] =
            children.try_into().map_err(|children: Vec<_>| {
                datafusion::common::DataFusionError::Internal(format!(
                    "Iceberg row lineage expression requires 3 children, got {}",
                    children.len()
                ))
            })?;
        Ok(Arc::new(Self::new(
            self.kind,
            physical,
            first_row_id,
            fallback,
        )))
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::Array;
    use datafusion::arrow::datatypes::SchemaRef;
    use datafusion::physical_plan::expressions::Column;

    use super::*;

    fn batch(
        physical: Vec<Option<i64>>,
        first_row_id: Vec<Option<i64>>,
        fallback: Vec<Option<i64>>,
    ) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("physical", DataType::Int64, true),
            Field::new("first_row_id", DataType::Int64, true),
            Field::new("fallback", DataType::Int64, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(physical)),
                Arc::new(Int64Array::from(first_row_id)),
                Arc::new(Int64Array::from(fallback)),
            ],
        )
        .expect("build row lineage test batch")
    }

    fn values(kind: RowLineageKind, batch: &RecordBatch) -> Vec<Option<i64>> {
        let expr = RowLineageExpr::new(
            kind,
            Arc::new(Column::new("physical", 0)),
            Arc::new(Column::new("first_row_id", 1)),
            Arc::new(Column::new("fallback", 2)),
        );
        let ColumnarValue::Array(result) = expr.evaluate(batch).expect("evaluate lineage") else {
            panic!("row lineage expression must return an array");
        };
        let result = result
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Int64 row lineage result");
        (0..result.len())
            .map(|index| (!result.is_null(index)).then(|| result.value(index)))
            .collect()
    }

    #[test]
    fn row_id_is_vectorized_and_coalesces_physical_values() {
        let input = batch(
            vec![None, Some(7), Some(8), None],
            vec![Some(100), Some(100), None, None],
            vec![Some(0), Some(1), Some(2), Some(3)],
        );
        assert_eq!(
            values(RowLineageKind::RowId, &input),
            vec![Some(100), Some(7), None, None]
        );
    }

    #[test]
    fn last_updated_sequence_is_gated_by_first_row_id() {
        let input = batch(
            vec![None, Some(7), Some(8), None],
            vec![Some(100), Some(100), None, None],
            vec![Some(4), Some(4), Some(5), Some(5)],
        );
        assert_eq!(
            values(RowLineageKind::LastUpdatedSequenceNumber, &input),
            vec![Some(4), Some(7), None, None]
        );
    }

    #[test]
    fn output_fields_use_reserved_iceberg_ids() {
        let schema: SchemaRef = Arc::new(Schema::empty());
        for (kind, name, id) in [
            (RowLineageKind::RowId, ROW_ID_COLUMN, ROW_ID_FIELD_ID),
            (
                RowLineageKind::LastUpdatedSequenceNumber,
                LAST_UPDATED_SEQUENCE_NUMBER_COLUMN,
                LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID,
            ),
        ] {
            let expr = RowLineageExpr::new(
                kind,
                Arc::new(Column::new("physical", 0)),
                Arc::new(Column::new("first_row_id", 1)),
                Arc::new(Column::new("fallback", 2)),
            );
            let field = expr.return_field(schema.as_ref()).expect("lineage field");
            assert_eq!(field.name(), name);
            assert_eq!(
                field.metadata().get(PARQUET_FIELD_ID_META_KEY),
                Some(&id.to_string())
            );
        }
    }
}
