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
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body: json!({"error": error.into()}),
        }
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
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(self.status.as_u16(), &self.body)
    }
}

pub(crate) type ApiResult<T = Response> = Result<T, ApiError>;

/// JSON response with the given status. Top-level object keys are sorted: Go builds nearly every
/// management answer from a `gin.H` map, which `encoding/json` writes in key order. Nested values
/// keep their own order (they are structs in Go). Use [`json_struct`] for a struct body.
pub(crate) fn json_response<T: Serialize>(status: u16, body: &T) -> Response {
    let value = serde_json::to_value(body).unwrap_or(Value::Null);
    json_struct(status, &sort_top(value))
}

/// Like [`json_response`] but keeps the field order of the body (Go struct responses).
pub(crate) fn json_struct<T: Serialize>(status: u16, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"null".to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

/// Sorts the keys of a top-level JSON object (`map[string]any` marshalling).
pub(crate) fn sort_top(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        other => other,
    }
}

pub(crate) fn ok_json<T: Serialize>(body: &T) -> Response {
    json_response(200, body)
}

/// 200 with a struct body (field order kept).
pub(crate) fn ok_struct<T: Serialize>(body: &T) -> Response {
    json_struct(200, body)
}

pub(crate) fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// First value of a query key, `None` when absent (Go: `c.GetQuery`).
pub(crate) fn query_get(uri: &Uri, key: &str) -> Option<String> {
    query_pairs(uri)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// Trimmed first value, empty when absent (Go: `strings.TrimSpace(c.Query(key))`).
pub(crate) fn query_trim(uri: &Uri, key: &str) -> String {
    query_get(uri, key)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// Go's `time.Time` JSON: RFC 3339 with the fraction trimmed to what is needed.
pub(crate) fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

pub(crate) fn content_type_is(headers: &axum::http::HeaderMap, essence: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(essence)
        })
}

/// Runs `fut` to completion even when the client disconnects (axum drops the handler future
/// then). Mutating handlers use it so a write, its config reload and its registry update are
/// never abandoned half way.
pub(crate) async fn detached<F>(fut: F) -> ApiResult
where
    F: std::future::Future<Output = ApiResult> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("management task failed: {e}");
            Err(ApiError::new(500, "internal error"))
        }
    }
}

/// Reads a request body up to [`crate::MAX_BODY_BYTES`].
pub(crate) async fn read_body(body: axum::body::Body) -> Result<bytes::Bytes, ()> {
    axum::body::to_bytes(body, crate::MAX_BODY_BYTES)
        .await
        .map_err(|_| ())
}

/// Runs blocking filesystem work off the async workers.
pub(crate) async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    F: FnOnce() -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(500, format!("task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn detached_work_finishes_after_the_caller_is_cancelled() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let handler = tokio::spawn(detached(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
            Ok(empty(200))
        }));
        tokio::task::yield_now().await; // the handler is running...
        handler.abort(); // ...when the client disconnects
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(done.load(Ordering::SeqCst));
    }
}
