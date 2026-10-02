//! Buffered replies and the error shapes of the live handlers (Go: `writeLiveError`,
//! `writeRealtimeError`).

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use serde_json::{Value, json};

/// A fully buffered HTTP reply.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

const JSON_UTF8: &str = "application/json; charset=utf-8";

impl Reply {
    pub fn new(status: u16) -> Self {
        Reply { status, headers: HeaderMap::new(), body: Bytes::new() }
    }

    /// Sets (replaces) a header; invalid names or values are ignored.
    pub fn set(&mut self, name: &str, value: &str) {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            self.headers.insert(n, v);
        }
    }

    /// Appends a header value; invalid names or values are ignored.
    pub fn add(&mut self, name: &str, value: &str) {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            self.headers.append(n, v);
        }
    }

    /// `c.JSON(status, value)` for map-shaped payloads: sorted keys, HTML escaped.
    pub fn json(status: u16, value: &Value) -> Self {
        let text = go_json_sorted(value, GoJsonStyle::MARSHAL_USE_NUMBER).unwrap_or_else(|| "null".into());
        let mut reply = Reply::new(status);
        reply.set("content-type", JSON_UTF8);
        reply.body = Bytes::from(text);
        reply
    }

    /// JSON reply from already serialized text.
    pub fn json_text(status: u16, text: String) -> Self {
        let mut reply = Reply::new(status);
        reply.set("content-type", JSON_UTF8);
        reply.body = Bytes::from(text);
        reply
    }

    pub fn into_response(self) -> Response {
        let mut resp = Response::new(Body::from(self.body));
        *resp.status_mut() = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        *resp.headers_mut() = self.headers;
        resp
    }

    /// Body as text (tests and logging).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `writeRealtimeError`: `{"error":{"code","message","param":null,"type"}}`.
pub fn realtime_error(status: u16, message: &str, error_type: &str, code: &str) -> Reply {
    Reply::json(status, &json!({"error": {"message": message, "type": error_type, "param": null, "code": code}}))
}

/// `writeLiveError`: realtime-shaped on `/v1/realtime*` paths, `{"error": message}` otherwise.
pub fn live_error(path: &str, status: u16, message: &str) -> Reply {
    if path.starts_with("/v1/realtime") {
        let mut error_type = "api_error";
        if (400..500).contains(&status) {
            error_type = "invalid_request_error";
        }
        if status == 401 {
            error_type = "authentication_error";
        }
        return realtime_error(status, message, error_type, "realtime_request_failed");
    }
    Reply::json(status, &json!({"error": message}))
}

/// `writeCapabilityNotSupported`.
pub fn capability_not_supported(capability: &str) -> Reply {
    realtime_error(
        501,
        &format!("{capability} are not supported by the ChatGPT/Codex OAuth upstream"),
        "not_supported_error",
        "realtime_capability_not_supported",
    )
}

/// Response header used by the proxy handshake rejection paths.
pub fn content_type_of(headers: &HeaderMap) -> Option<String> {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string)
}
