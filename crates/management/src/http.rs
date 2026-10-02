//! Response and request helpers shared by the handlers.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};

/// A JSON error response: `{"error": msg}` plus optional extra fields.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn new(status: u16, error: impl Into<String>) -> Self {
        Self { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body: json!({"error": error.into()}) }
    }

    /// `{"error": code, "message": detail}`.
    pub fn with_message(status: u16, error: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body: json!({"error": error, "message": message.into()}),
        }
    }

    pub fn bad_request(error: impl Into<String>) -> Self {
        Self::new(400, error)
    }

    pub fn from_body(status: u16, body: Value) -> Self {
        Self { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(self.status.as_u16(), &self.body)
    }
}

pub(crate) type ApiResult<T = Response> = Result<T, ApiError>;

/// JSON response with the given status.
pub(crate) fn json_response<T: Serialize>(status: u16, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"null".to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    resp
}

pub(crate) fn ok_json<T: Serialize>(body: &T) -> Response {
    json_response(200, body)
}

pub(crate) fn no_store(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// A bare status with an empty body.
pub(crate) fn empty(status: u16) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp
}

/// Query pairs in order, repeats preserved (Go: `c.QueryArray`).
pub(crate) fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

/// First value of a query key, `None` when absent (Go: `c.GetQuery`).
pub(crate) fn query_get(uri: &Uri, key: &str) -> Option<String> {
    query_pairs(uri).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Trimmed first value, empty when absent (Go: `strings.TrimSpace(c.Query(key))`).
pub(crate) fn query_trim(uri: &Uri, key: &str) -> String {
    query_get(uri, key).map(|v| v.trim().to_string()).unwrap_or_default()
}

/// Go's `time.Time` JSON: RFC 3339 with the fraction trimmed to what is needed.
pub(crate) fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

pub(crate) fn content_type_is(headers: &axum::http::HeaderMap, essence: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case(essence))
}
