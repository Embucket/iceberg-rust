//! Iceberg v3 row-lineage metadata columns.

/// Logical metadata column carrying a row's stable table-wide identifier.
pub const ROW_ID_COLUMN_NAME: &str = "_row_id";
/// Logical metadata column carrying the sequence number of a row's last update.
pub const LAST_UPDATED_SEQUENCE_NUMBER_COLUMN_NAME: &str = "_last_updated_sequence_number";

/// Reserved Iceberg field ID for [`ROW_ID_COLUMN_NAME`].
pub const ROW_ID_FIELD_ID: i32 = i32::MAX - 107;
/// Reserved Iceberg field ID for [`LAST_UPDATED_SEQUENCE_NUMBER_COLUMN_NAME`].
pub const LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID: i32 = i32::MAX - 108;

/// Returns whether `field_id` identifies an Iceberg row-lineage metadata column.
#[must_use]
pub const fn is_row_lineage_field_id(field_id: i32) -> bool {
    field_id == ROW_ID_FIELD_ID || field_id == LAST_UPDATED_SEQUENCE_NUMBER_FIELD_ID
}
