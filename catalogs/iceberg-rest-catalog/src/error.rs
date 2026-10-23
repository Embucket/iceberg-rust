use iceberg_rust::error::{Error, HttpResponseError};
use reqwest::StatusCode;

use crate::apis::{self, catalog_api_api::CreateNamespaceError, ResponseContent};

/**
Error conversion

404 maps to [`Error::NotFound`] and 409 to [`Error::CommitConflict`]; every other error status
maps to [`Error::HttpResponse`], which keeps the status code and `Retry-After` delay
(see [`Error::http_status`] and [`Error::retry_after`]).
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
                ..
            }) => Error::NotFound(content),
            apis::Error::ResponseError(ResponseContent {
                status: StatusCode::CONFLICT,
                content,
                ..
            }) => Error::CommitConflict(content),
            apis::Error::ResponseError(err) => Error::HttpResponse(HttpResponseError::new(
                err.status.as_u16(),
                err.retry_after,
                format!(
                    "Response status: {}, Response content: {}",
                    err.status, err.content
                ),
            )),
            apis::Error::AWSV4SignatureError(err) => Error::External(Box::new(err)),
            apis::Error::OAuthToken(err) => err,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn response_conflict_is_structured_commit_conflict() {
        let error: Error = apis::Error::<()>::ResponseError(ResponseContent {
            status: StatusCode::CONFLICT,
            content: "concurrent commit".to_string(),
            entity: None,
            retry_after: None,
        })
        .into();

        assert!(matches!(error, Error::CommitConflict(message) if message == "concurrent commit"));
    }

    fn response(status: StatusCode, retry_after: Option<Duration>) -> apis::Error<()> {
        apis::Error::ResponseError(ResponseContent {
            status,
            content: "body".to_string(),
            entity: None,
            retry_after,
        })
    }

    #[test]
    fn response_not_found_and_conflict_keep_their_variants() {
        let not_found: Error = response(StatusCode::NOT_FOUND, None).into();
        assert!(matches!(not_found, Error::NotFound(ref message) if message == "body"));
        assert_eq!(not_found.http_status(), None);

        let conflict: Error = response(StatusCode::CONFLICT, None).into();
        assert!(matches!(conflict, Error::CommitConflict(_)));
        assert_eq!(conflict.http_status(), None);
    }

    #[test]
    fn response_error_carries_status_and_retry_after() {
        for (status, retry_after) in [
            (StatusCode::TOO_MANY_REQUESTS, Some(Duration::from_secs(2))),
            (StatusCode::SERVICE_UNAVAILABLE, Some(Duration::ZERO)),
            (StatusCode::UNAUTHORIZED, None),
            (StatusCode::FORBIDDEN, None),
            (StatusCode::INTERNAL_SERVER_ERROR, None),
        ] {
            let error: Error = response(status, retry_after).into();
            assert!(
                matches!(error, Error::HttpResponse(_)),
                "{status}: {error:?}"
            );
            assert_eq!(error.http_status(), Some(status.as_u16()));
            assert_eq!(error.retry_after(), retry_after);
            // Display text is unchanged from the former `InvalidFormat` mapping.
            assert_eq!(
                error.to_string(),
                format!("Response status: {status}, Response content: body doesn't have the right format")
            );
        }
    }
}
