//! Buffered HTTP replies and the SSE response builder shared by every handler.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// A fully buffered response. Handlers build these so the non-streaming keepalive wrapper can
/// either send them as is or, once the status line is already committed, emit only the body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub const JSON_UTF8: &str = "application/json; charset=utf-8";
pub const JSON: &str = "application/json";

impl Reply {
    pub fn new(status: u16) -> Self {
        Reply {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: &str) -> Self {
        if let Ok(v) = HeaderValue::from_str(value) {
            self.headers.insert(name, v);
        }
        self
    }

    pub fn content_type(self, value: &str) -> Self {
        self.with_header(header::CONTENT_TYPE, value)
    }

    /// `c.JSON(status, body)`: `application/json; charset=utf-8`.
    pub fn json(status: u16, body: impl Into<Bytes>) -> Self {
        Reply::new(status).content_type(JSON_UTF8).with_body(body)
    }

    /// `c.JSON(status, value)` for map-shaped Go payloads: keys sorted, HTML escaped.
    pub fn json_value(status: u16, value: &Value) -> Self {
        let text = go_json_sorted(value, GoJsonStyle::MARSHAL_USE_NUMBER).unwrap_or_else(|| "null".into());
        Reply::json(status, text.into_bytes())
    }

    pub fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = Response::new(Body::from(self.body));
        *resp.status_mut() = status;
        *resp.headers_mut() = self.headers;
        resp
    }
}

/// Headers common to every SSE endpoint, set only once the first chunk is ready.
pub fn set_sse_headers(headers: &mut HeaderMap) {
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
}

/// Streaming body fed from a channel; dropping the response drops the receiver, which the
/// forwarding task observes as a client disconnect.
pub fn streaming_response(status: u16, headers: HeaderMap, rx: mpsc::Receiver<Bytes>) -> Response {
    let stream = futures_util::StreamExt::map(ReceiverStream::new(rx), Ok::<Bytes, std::convert::Infallible>);
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    *resp.headers_mut() = headers;
    resp
}

/// Response with headers and an already-complete body (e.g. the empty-stream footer).
pub fn sse_reply(headers: HeaderMap, body: &'static str) -> Reply {
    Reply {
        status: 200,
        headers,
        body: Bytes::from_static(body.as_bytes()),
    }
}
