use iceberg_rust::error::Error as IcebergError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("prepared-pair recovery: {0}")]
    PairRecovery(#[source] IcebergError),
    #[error(transparent)]
    ParseError(#[from] url::ParseError),
    #[error(transparent)]
    ParseIntError(#[from] std::num::ParseIntError),
}

impl From<Error> for IcebergError {
    fn from(value: Error) -> Self {
        IcebergError::External(Box::new(value))
    }
}
