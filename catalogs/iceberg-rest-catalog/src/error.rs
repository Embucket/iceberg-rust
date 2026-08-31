use iceberg_rust::error::Error;
use reqwest::StatusCode;

use crate::apis::{self, catalog_api_api::CreateNamespaceError, ResponseContent};

/**
Error conversion
*/
impl<T> From<apis::Error<T>> for Error {
    fn from(val: apis::Error<T>) -> Self {
        match val {
            apis::Error::Reqwest(err) => Error::External(err.into()),
            apis::Error::Serde(err) => Error::JSONSerde(err),
            apis::Error::Io(err) => Error::IO(err),
            apis::Error::ResponseError(ResponseContent {
                status: StatusCode::NOT_FOUND,
                content,
                entity: _,
            }) => Error::NotFound(content),
            apis::Error::ResponseError(ResponseContent {
                status: StatusCode::CONFLICT,
                content,
                entity: _,
            }) => Error::CommitConflict(content),
            apis::Error::ResponseError(err) => Error::InvalidFormat(format!(
                "Response status: {}, Response content: {}",
                err.status, err.content
            )),
            apis::Error::AWSV4SignatureError(err) => Error::External(Box::new(err)),
            apis::Error::OAuthToken(err) => err,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_conflict_is_structured_commit_conflict() {
        let error: Error = apis::Error::<()>::ResponseError(ResponseContent {
            status: StatusCode::CONFLICT,
            content: "concurrent commit".to_string(),
            entity: None,
        })
        .into();

        assert!(matches!(error, Error::CommitConflict(message) if message == "concurrent commit"));
    }
}
