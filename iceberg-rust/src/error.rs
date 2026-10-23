/*!
Error type for iceberg
*/

use std::{fmt, time::Duration};

use arrow::error::ArrowError;
use thiserror::Error;

/// Error response returned by a remote (e.g. REST) catalog over HTTP.
///
/// Carries the HTTP status code and the parsed `Retry-After` hint so callers can classify
/// failures (authentication vs. throttling vs. server errors) without parsing message text.
/// Its `Display` output matches the legacy [`Error::InvalidFormat`] text that these errors
/// were reported with before, so existing log and error messages are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponseError {
    status: u16,
    retry_after: Option<Duration>,
    message: String,
}

impl HttpResponseError {
    /// Create a response error from its HTTP status, optional `Retry-After` delay and
    /// human-readable message (status line and response body).
    pub fn new(status: u16, retry_after: Option<Duration>, message: impl Into<String>) -> Self {
        Self {
            status,
            retry_after,
            message: message.into(),
        }
    }

    /// HTTP status code of the response.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Delay requested by the server's `Retry-After` header, if present and valid.
    /// An HTTP-date in the past is reported as [`Duration::ZERO`]. The delay is the server's
    /// value, unclamped (it may be up to `u64::MAX` seconds), so bound it before adding it
    /// to an `Instant`.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Human-readable message (status line and response body).
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HttpResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same text as `Error::InvalidFormat(message)`, which carried these errors before.
        write!(f, "{} doesn't have the right format", self.message)
    }
}

#[derive(Error, Debug)]
/// Iceberg error
pub enum Error {
    /// Invalid format
    #[error("{0} doesn't have the right format")]
    InvalidFormat(String),
    /// Type error
    #[error("Value {0} doesn't have the {1} type.")]
    Type(String, String),
    /// Schema error
    #[error("Column {0} not in schema {1}.")]
    Schema(String, String),
    /// Conversion error
    #[error("Failed to convert {0} to {1}.")]
    Conversion(String, String),
    /// Failed to decompress gzip data
    #[error("Failed to decompress gzip data: {0}")]
    Decompress(String),
    /// Not found
    #[error("{0} not found.")]
    NotFound(String),
    /// Not supported
    #[error("Feature {0} is not supported.")]
    NotSupported(String),
    /// Not found in catalog
    #[error("Entity not found in catalog")]
    CatalogNotFound,
    /// A concurrent commit superseded the base metadata this commit was built on
    #[error(
        "Commit to {0} conflicted with a concurrent commit; the base metadata is no longer current"
    )]
    CommitConflict(String),
    /// HTTP error response from a remote catalog (status codes other than those mapped to
    /// [`Error::NotFound`] and [`Error::CommitConflict`])
    #[error("{0}")]
    HttpResponse(HttpResponseError),
    /// External error
    #[error(transparent)]
    External(Box<dyn std::error::Error + Send + Sync>),
    /// Iceberg spec error
    #[error(transparent)]
    Iceberg(#[from] iceberg_rust_spec::error::Error),
    /// Arrow error
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    /// Parquet error
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Avro error
    #[error(transparent)]
    Avro(Box<apache_avro::Error>),
    /// Thrift error
    #[error(transparent)]
    Thrift(#[from] thrift::Error),
    /// sql parser error
    #[error(transparent)]
    SQLParser(#[from] sqlparser::parser::ParserError),
    /// Serde json
    #[error(transparent)]
    JSONSerde(#[from] serde_json::Error),
    /// Chrono parse
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
    /// Url parse
    #[error(transparent)]
    Url(#[from] url::ParseError),
    /// Io error
    #[error(transparent)]
    IO(#[from] std::io::Error),
    /// Channel error
    #[error(transparent)]
    FuturesChannel(#[from] futures::channel::mpsc::SendError),
    /// Tokio error
    #[error(transparent)]
    TokioJoinError(#[from] tokio::task::JoinError),
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
    /// derive builder
    #[error(transparent)]
    DeriveBuilder(#[from] derive_builder::UninitializedFieldError),
}

impl Error {
    /// HTTP status code of a remote catalog error response, if this error carries one.
    ///
    /// Only [`Error::HttpResponse`] carries a status; responses mapped to
    /// [`Error::NotFound`] (404) or [`Error::CommitConflict`] (409) keep those variants and
    /// return `None` here.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Error::HttpResponse(error) => Some(error.status()),
            _ => None,
        }
    }

    /// Delay requested by a remote catalog's `Retry-After` response header, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::HttpResponse(error) => error.retry_after(),
            _ => None,
        }
    }
}

impl From<apache_avro::Error> for Error {
    fn from(err: apache_avro::Error) -> Self {
        Error::Avro(Box::new(err))
    }
}

impl From<Error> for ArrowError {
    fn from(value: Error) -> Self {
        ArrowError::from_external_error(Box::new(value))
    }
}
