use std::error::Error;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, StringArray};
use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::variant::{json_to_variant, VariantArray, VariantType};
use parquet_variant_json::VariantToJson;

#[test]
fn parquet_variant_roundtrip_preserves_values_and_logical_type() -> Result<(), Box<dyn Error>> {
    let input: ArrayRef = Arc::new(StringArray::from(vec![
        Some(r#"{"name":"Alice","age":30,"active":true}"#),
        Some(r#"[1,"two",null,{"nested":3.5}]"#),
        None,
    ]));
    let variant = json_to_variant(&input)?;
    let schema = Arc::new(Schema::new(vec![variant.field("payload")]));
    let batch = RecordBatch::try_new(schema, vec![ArrayRef::from(variant)])?;

    let mut parquet = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut parquet, batch.schema(), None)?;
    writer.write(&batch)?;
    writer.close()?;

    let mut reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(parquet))?.build()?;
    let output = reader.next().transpose()?.ok_or("missing record batch")?;
    output
        .schema()
        .field_with_name("payload")?
        .try_extension_type::<VariantType>()?;

    let variant = VariantArray::try_new(output.column(0).as_ref())?;
    for row in [0, 1] {
        let expected: serde_json::Value = serde_json::from_str(
            input
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or("expected string input")?
                .value(row),
        )?;
        let actual: serde_json::Value =
            serde_json::from_str(&variant.value(row).to_json_string()?)?;
        assert_eq!(actual, expected);
    }
    assert!(variant.is_null(2));
    Ok(())
}
