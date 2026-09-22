use std::collections::HashMap;
use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{Array, BooleanBuilder, Int64Array, RecordBatch, StringArray};
use datafusion::arrow::datatypes::{DataType, Schema};
use datafusion::common::{exec_err, Result};
use datafusion::physical_plan::PhysicalExpr;
use datafusion::scalar::ScalarValue;
use datafusion_expr::ColumnarValue;
use iceberg_rust::spec::deletion_vector::DeletionVector;

/// Batch predicate backed by compact roaring deletion vectors.
///
/// DataFusion's file stream emits each record batch from one file, so the Iceberg path
/// partition column is constant within a batch. Looking it up once keeps the hot loop to one
/// row-position read and one roaring membership test per row.
#[derive(Debug)]
pub(crate) struct DeletionVectorPredicate {
    file_path: Arc<dyn PhysicalExpr>,
    row_position: Arc<dyn PhysicalExpr>,
    vectors: Arc<HashMap<String, DeletionVector>>,
}

impl DeletionVectorPredicate {
    pub(crate) fn new(
        file_path: Arc<dyn PhysicalExpr>,
        row_position: Arc<dyn PhysicalExpr>,
        vectors: HashMap<String, DeletionVector>,
    ) -> Self {
        Self {
            file_path,
            row_position,
            vectors: Arc::new(vectors),
        }
    }
}

impl PartialEq for DeletionVectorPredicate {
    fn eq(&self, other: &Self) -> bool {
        self.file_path.eq(&other.file_path)
            && self.row_position.eq(&other.row_position)
            && Arc::ptr_eq(&self.vectors, &other.vectors)
    }
}

impl Eq for DeletionVectorPredicate {}

impl Hash for DeletionVectorPredicate {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.file_path.hash(state);
        self.row_position.hash(state);
        Arc::as_ptr(&self.vectors).hash(state);
    }
}

impl Display for DeletionVectorPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "iceberg_deletion_vector_keep({}, {})",
            self.file_path, self.row_position
        )
    }
}

impl PhysicalExpr for DeletionVectorPredicate {
    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(DataType::Boolean)
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(false)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        if batch.num_rows() == 0 {
            return Ok(ColumnarValue::Array(Arc::new(
                datafusion::arrow::array::BooleanArray::from(Vec::<bool>::new()),
            )));
        }

        let paths = self
            .file_path
            .evaluate(batch)?
            .into_array(batch.num_rows())?;
        let Some(paths) = paths.as_any().downcast_ref::<StringArray>() else {
            return exec_err!(
                "Iceberg deletion-vector file path must be Utf8, got {}",
                paths.data_type()
            );
        };
        if paths.is_null(0) {
            return exec_err!("Iceberg deletion-vector file path cannot be null");
        }
        let Some(vector) = self.vectors.get(paths.value(0)) else {
            return Ok(ColumnarValue::Scalar(ScalarValue::Boolean(Some(true))));
        };

        let positions = self
            .row_position
            .evaluate(batch)?
            .into_array(batch.num_rows())?;
        let Some(positions) = positions.as_any().downcast_ref::<Int64Array>() else {
            return exec_err!(
                "Iceberg deletion-vector row position must be Int64, got {}",
                positions.data_type()
            );
        };

        let mut keep = BooleanBuilder::with_capacity(batch.num_rows());
        for position in positions.iter() {
            let Some(position) = position else {
                return exec_err!("Iceberg deletion-vector row position cannot be null");
            };
            let position = u64::try_from(position).map_err(|_| {
                datafusion::common::DataFusionError::Execution(format!(
                    "Iceberg deletion-vector row position cannot be negative: {position}"
                ))
            })?;
            keep.append_value(!vector.contains(position));
        }
        Ok(ColumnarValue::Array(Arc::new(keep.finish())))
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.file_path, &self.row_position]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        let [file_path, row_position]: [Arc<dyn PhysicalExpr>; 2] =
            children.try_into().map_err(|children: Vec<_>| {
                datafusion::common::DataFusionError::Internal(format!(
                    "Iceberg deletion-vector predicate requires 2 children, got {}",
                    children.len()
                ))
            })?;
        Ok(Arc::new(Self {
            file_path,
            row_position,
            vectors: Arc::clone(&self.vectors),
        }))
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::{ArrayRef, Int64Array, StringArray};
    use datafusion::arrow::datatypes::{Field, Schema};
    use datafusion::physical_plan::expressions::Column;
    use roaring::RoaringTreemap;

    use super::*;

    #[test]
    fn filters_a_batch_without_materializing_delete_rows() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("path", DataType::Utf8, false),
            Field::new("pos", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(StringArray::from(vec!["a.parquet"; 5])) as ArrayRef,
                Arc::new(Int64Array::from_iter_values(0..5)) as ArrayRef,
            ],
        )
        .unwrap();
        let mut vectors = HashMap::new();
        vectors.insert(
            "a.parquet".to_string(),
            DeletionVector::new([1, 3].into_iter().collect::<RoaringTreemap>()),
        );
        let predicate = DeletionVectorPredicate::new(
            Arc::new(Column::new("path", 0)),
            Arc::new(Column::new("pos", 1)),
            vectors,
        );

        let result = predicate.evaluate(&batch).unwrap().into_array(5).unwrap();
        let result = result
            .as_any()
            .downcast_ref::<datafusion::arrow::array::BooleanArray>()
            .unwrap();
        assert_eq!(
            result.iter().collect::<Vec<_>>(),
            vec![Some(true), Some(false), Some(true), Some(false), Some(true)]
        );
    }
}
