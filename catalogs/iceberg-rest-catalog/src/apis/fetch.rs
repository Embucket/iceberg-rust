use crate::apis::{configuration, ResponseContent};

use super::{Conditional, Error};

use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

pub(crate) async fn fetch_inner<R, E>(
    configuration: &configuration::Configuration,
    method: reqwest::Method,
    prefix: Option<&str>,
    uri_str: &str,
    request: &R,
    headers: Option<HashMap<String, String>>,
    query_params: Option<HashMap<String, String>>,
) -> Result<reqwest::Response, Error<E>>
where
    R: serde::Serialize + ?Sized,
{
    let uri_base = build_uri_base(configuration, prefix);
    let client = &configuration.client;

    let uri = uri_base + uri_str;
    let mut req_builder = client.request(method.clone(), &uri);

    for (key, value) in query_params.unwrap_or_default() {
        req_builder = req_builder.query(&[(key, value)]);
    }

    if let Some(ref aws_v4_key) = configuration.aws_v4_key {
        let body_str = match serde_json::to_value(&request) {
            Ok(serde_json::Value::Null) => "",
            _ => &serde_json::to_string(&request).expect("param should serialize to string"),
        };
        let uri_for_signing = match req_builder.try_clone() {
            Some(cloned_builder) => match cloned_builder.build() {
                Ok(tmp_req) => tmp_req.url().as_str().to_string(),
                Err(_) => uri.clone(),
            },
            None => uri.clone(),
        };
        let new_headers = match aws_v4_key.sign(&uri_for_signing, method.as_str(), body_str) {
            Ok(new_headers) => new_headers,
            Err(err) => return Err(Error::AWSV4SignatureError(err)),
        };
        for (name, value) in new_headers.iter() {
            req_builder = req_builder.header(name.as_str(), value.as_str());
        }
    }
    if let Some(ref user_agent) = configuration.user_agent {
        req_builder = req_builder.header(reqwest::header::USER_AGENT, user_agent.clone());
    }
    if let Some(token) = configuration
        .resolve_oauth_token()
        .await
        .map_err(Error::OAuthToken)?
    {
        req_builder = req_builder.bearer_auth(token);
    };
    if let Some(ref token) = configuration.bearer_access_token {
        req_builder = req_builder.bearer_auth(token.to_owned());
    };
    for (key, value) in headers.unwrap_or_default() {
        req_builder = req_builder.header(key, value);
    }
    if let &reqwest::Method::POST | &reqwest::Method::PUT = &method {
        req_builder = req_builder.json(request);
    }

    let req = req_builder.build()?;
    client.execute(req).await.map_err(Error::from)
}

pub(crate) async fn fetch<R, T, E>(
    configuration: &configuration::Configuration,
    method: reqwest::Method,
    prefix: Option<&str>,
    uri_str: &str,
    request: &R,
    headers: Option<HashMap<String, String>>,
    query_params: Option<HashMap<String, String>>,
) -> Result<T, Error<E>>
where
    R: serde::Serialize + ?Sized,
    T: serde::de::DeserializeOwned,
    E: serde::de::DeserializeOwned,
{
    let resp = fetch_inner(
        configuration,
        method,
        prefix,
        uri_str,
        request,
        headers,
        query_params,
    )
    .await?;
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let content = resp.text().await?;
    if !status.is_client_error() && !status.is_server_error() {
        let body = if content.is_empty() { "null" } else { &content };
        serde_json::from_str(body).map_err(Error::from)
    } else {
        Err(response_error(status, &resp_headers, content))
    }
}

pub(crate) async fn fetch_unit<R, E>(
    configuration: &configuration::Configuration,
    method: reqwest::Method,
    prefix: Option<&str>,
    uri_str: &str,
    request: &R,
    headers: Option<HashMap<String, String>>,
    query_params: Option<HashMap<String, String>>,
) -> Result<(), Error<E>>
where
    R: serde::Serialize + ?Sized,
    E: serde::de::DeserializeOwned,
{
    let response = fetch_inner(
        configuration,
        method,
        prefix,
        uri_str,
        request,
        headers,
        query_params,
    )
    .await?;
    let status = response.status();
    let resp_headers = response.headers().clone();
    let content = response.text().await?;
    unit_response(status, &resp_headers, content)
}

fn unit_response<E>(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    content: String,
) -> Result<(), Error<E>>
where
    E: serde::de::DeserializeOwned,
{
    if status.is_success() {
        return Ok(());
    }
    Err(response_error(status, headers, content))
}

/// Build the [`Error::ResponseError`] for a non-success response, keeping its status, body,
/// decoded error entity (when the body parses as `E`) and `Retry-After` hint.
pub(crate) fn response_error<E>(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    content: String,
) -> Error<E>
where
    E: serde::de::DeserializeOwned,
{
    let entity = serde_json::from_str(&content).ok();
    Error::ResponseError(ResponseContent {
        status,
        content,
        entity,
        retry_after: retry_after(headers, SystemTime::now()),
    })
}

/// Parse the `Retry-After` response header (RFC 9110 section 10.2.3) relative to `now`.
///
/// Accepts delta-seconds and the three HTTP-date formats (IMF-fixdate, RFC 850, asctime).
/// An HTTP-date at or before `now` yields [`Duration::ZERO`]; a missing or malformed header
/// yields `None`.
fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let retry_at = parse_http_date(value)?;
    let retry_at =
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(retry_at).ok()?))?;
    Some(retry_at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Parse an HTTP-date (RFC 9110 section 5.6.7) into seconds since the Unix epoch.
fn parse_http_date(value: &str) -> Option<i64> {
    use chrono::NaiveDateTime;

    const FORMATS: [&str; 3] = [
        // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
        "%a, %d %b %Y %H:%M:%S GMT",
        // obsolete RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
        "%A, %d-%b-%y %H:%M:%S GMT",
        // obsolete asctime: Sun Nov  6 08:49:37 1994
        "%a %b %e %H:%M:%S %Y",
    ];
    FORMATS.iter().find_map(|format| {
        NaiveDateTime::parse_from_str(value, format)
            .ok()
            .map(|date| date.and_utc().timestamp())
    })
}

pub(crate) async fn fetch_conditional<R, T, E>(
    configuration: &configuration::Configuration,
    method: reqwest::Method,
    prefix: Option<&str>,
    uri_str: &str,
    request: &R,
    headers: Option<HashMap<String, String>>,
    query_params: Option<HashMap<String, String>>,
) -> Result<Conditional<T>, Error<E>>
where
    R: serde::Serialize + ?Sized,
    T: serde::de::DeserializeOwned,
    E: serde::de::DeserializeOwned,
{
    let resp = fetch_inner(
        configuration,
        method,
        prefix,
        uri_str,
        request,
        headers,
        query_params,
    )
    .await?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Conditional::NotModified);
    }
    let resp_headers = resp.headers().clone();
    let status = resp.status();
    let content = resp.text().await?;
    if !status.is_client_error() && !status.is_server_error() {
        let body = if content.is_empty() { "null" } else { &content };
        let value = serde_json::from_str(body).map_err(Error::from)?;
        Ok(Conditional::Modified {
            value,
            headers: resp_headers,
        })
    } else {
        Err(response_error(status, &resp_headers, content))
    }
}

/// Build the base URI for REST API calls with proper prefix encoding
fn build_uri_base(configuration: &configuration::Configuration, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => {
            // Split prefix by '/' and URL-encode each segment individually
            // This allows paths like "catalogs/warehouse_name" while still protecting
            // against path traversal attacks (e.g., "../../../etc")
            let encoded_segments: Vec<String> = prefix
                .split('/')
                .map(|segment| crate::apis::urlencode(segment))
                .collect();
            format!(
                "{}/v1/{}/",
                configuration.base_path,
                encoded_segments.join("/")
            )
        }
        None => format!("{}/v1/", configuration.base_path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_response_accepts_empty_and_nonempty_success_bodies() {
        for (status, body) in [
            (reqwest::StatusCode::NO_CONTENT, ""),
            (reqwest::StatusCode::OK, "null"),
            (reqwest::StatusCode::OK, "\"NO_CONTENT\""),
        ] {
            assert!(unit_response::<serde_json::Value>(
                status,
                &HeaderMap::new(),
                body.to_string()
            )
            .is_ok());
        }
    }

    #[test]
    fn unit_response_keeps_error_status_and_body() {
        let body = r#"{"error":"invalid rename"}"#;
        let result = unit_response::<serde_json::Value>(
            reqwest::StatusCode::BAD_REQUEST,
            &HeaderMap::new(),
            body.to_string(),
        );
        match result {
            Err(Error::ResponseError(response)) => {
                assert_eq!(response.status, reqwest::StatusCode::BAD_REQUEST);
                assert_eq!(response.content, body);
                assert_eq!(response.entity, serde_json::from_str(body).ok());
            }
            _ => panic!("expected REST response error"),
        }
    }

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    // 2015-10-21T07:28:00Z
    const DATE_EPOCH_SECS: u64 = 1_445_412_480;

    #[test]
    fn retry_after_parses_delta_seconds() {
        let now = SystemTime::now();
        assert_eq!(
            retry_after(&headers_with_retry_after("2"), now),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            retry_after(&headers_with_retry_after(" 120 "), now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after(&headers_with_retry_after("0"), now),
            Some(Duration::ZERO)
        );
        for invalid in ["", "-1", "1.5", "soon", "2 seconds"] {
            assert_eq!(
                retry_after(&headers_with_retry_after(invalid), now),
                None,
                "{invalid:?}"
            );
        }
        assert_eq!(retry_after(&HeaderMap::new(), now), None);
    }

    #[test]
    fn retry_after_parses_http_dates_relative_to_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(DATE_EPOCH_SECS - 30);
        for date in [
            // IMF-fixdate, obsolete RFC 850 and asctime formats (RFC 9110 section 5.6.7).
            "Wed, 21 Oct 2015 07:28:00 GMT",
            "Wednesday, 21-Oct-15 07:28:00 GMT",
            "Wed Oct 21 07:28:00 2015",
        ] {
            assert_eq!(
                retry_after(&headers_with_retry_after(date), now),
                Some(Duration::from_secs(30)),
                "{date:?}"
            );
        }
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(DATE_EPOCH_SECS + 30);
        assert_eq!(
            retry_after(
                &headers_with_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
                later
            ),
            Some(Duration::ZERO),
            "an HTTP-date in the past means retry now"
        );
        assert_eq!(
            retry_after(
                &headers_with_retry_after("Wed, 32 Oct 2015 07:28:00 GMT"),
                now
            ),
            None
        );
    }

    #[test]
    fn response_error_keeps_retry_after() {
        let error = response_error::<serde_json::Value>(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &headers_with_retry_after("2"),
            "slow down".to_string(),
        );
        assert_eq!(error.status(), Some(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
    }
}
