/*!
Error type for iceberg
*/

use datafusion::{arrow::array::RecordBatch, error::DataFusionError};
use iceberg_rust::error::Error as IcebergError;
use thiserror::Error;

/// Declared Snowflake-compatible column type for a write-length failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclaredColumnKind {
    /// Length is measured in Unicode characters.
    Varchar,
    /// Length is measured in bytes.
    Binary,
}

impl DeclaredColumnKind {
    pub(crate) const fn sql_name(self) -> &'static str {
        match self {
            Self::Varchar => "VARCHAR",
            Self::Binary => "BINARY",
        }
    }
}

impl std::fmt::Display for DeclaredColumnKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Varchar => f.write_str("String"),
            Self::Binary => f.write_str("Binary value"),
        }
    }
}

#[derive(Error, Debug)]
/// Iceberg error
pub enum Error {
    /// A value exceeds a Snowflake-compatible declared column length.
    #[error("{kind} '{preview}' is too long and would be truncated")]
    DeclaredColumnLengthExceeded {
        /// Name of the column being written.
        column: String,
        /// Snowflake display name of the value type.
        kind: DeclaredColumnKind,
        /// Maximum number of characters or bytes.
        limit: usize,
        /// Bounded value preview, formatted like Snowflake's error.
        preview: String,
    },
    /// Datafusion error
    #[error(transparent)]
    Datafusion(#[from] datafusion::error::DataFusionError),
    /// Arrow error
    #[error(transparent)]
    Arrow(#[from] datafusion::arrow::error::ArrowError),
    /// sql parser error
    #[error(transparent)]
    SQLParser(#[from] datafusion::sql::sqlparser::parser::ParserError),
    /// Iceberg error
    #[error(transparent)]
    Iceberg(#[from] iceberg_rust::error::Error),
    /// Iceberg error
    #[error(transparent)]
    IcebergSpec(#[from] iceberg_rust::spec::error::Error),
    /// Serde json
    #[error(transparent)]
    JSONSerde(#[from] serde_json::Error),
    /// Chrono parse
    #[error(transparent)]
    Chrono(#[from] chrono::ParseError),
    /// Io error
    #[error(transparent)]
    IO(#[from] std::io::Error),
    /// Channel error
    #[error(transparent)]
    FuturesChannel(#[from] futures::channel::mpsc::SendError),
    /// Objectstore error
    #[error(transparent)]
    ObjectStore(#[from] object_store::Error),
    /// Try from slice error
    #[error(transparent)]
    TryFromSlice(#[from] std::array::TryFromSliceError),
    /// Try from int error
    #[error(transparent)]
    TryFromInt(#[from] std::num::TryFromIntError),
    /// Utf8 error
    #[error(transparent)]
    UTF8(#[from] std::str::Utf8Error),
    /// from utf8 error
    #[error(transparent)]
    FromUTF8(#[from] std::string::FromUtf8Error),
    /// parse int error
    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),
    /// Tokio error
    #[error(transparent)]
    TokioSend(
        #[from]
        tokio::sync::mpsc::error::SendError<(
            object_store::path::Path,
            tokio::sync::mpsc::Receiver<RecordBatch>,
        )>,
    ),
    /// parse int error
    #[error(transparent)]
    DeriveBuilder(#[from] derive_builder::UninitializedFieldError),
}

impl From<Error> for DataFusionError {
    fn from(value: Error) -> Self {
        DataFusionError::External(Box::new(value))
    }
}

impl From<Error> for IcebergError {
    fn from(value: Error) -> Self {
        IcebergError::External(Box::new(value))
    }
}
