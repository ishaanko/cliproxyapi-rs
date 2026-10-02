//! Dialect handlers (Go: sdk/api/handlers/{openai,claude,gemini}).

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use bytes::Bytes;

use crate::body::decode_request_body;
use crate::error::error_response_json;
use crate::exec::ExecOk;
use crate::headers::write_upstream_headers;
use crate::reply::{JSON, Reply};
use crate::req::ReqInfo;

pub mod claude;
pub mod gemini;
pub mod images;
pub mod openai;
pub mod responses;

pub const TRACE_ID_HEADER: &str = "x-cpa-trace-id";

/// `ReadRequestBody`: raw body plus `Content-Encoding` decoding; failures become the 400 reply.
pub fn read_request_body(info: &ReqInfo, raw: Bytes) -> Result<Bytes, Reply> {
    decode_request_body(&info.headers, raw).map_err(invalid_request)
}

/// `{"error":{"message":"Invalid request: <err>","type":"invalid_request_error"}}` (400).
pub fn invalid_request(err: impl std::fmt::Display) -> Reply {
    Reply::json(400, error_response_json(&format!("Invalid request: {err}"), "invalid_request_error"))
}

/// 400 with a plain `invalid_request_error` message.
pub fn bad_request_message(message: &str) -> Reply {
    Reply::json(400, error_response_json(message, "invalid_request_error"))
}

/// `X-CPA-TRACE-ID: <YYYYMMDDHHMMSS>-<authIndex>-<requestID>` once a credential was selected.
pub fn trace_id(auth_index: &str, request_id: &str) -> Option<String> {
    let auth_index = auth_index.trim();
    let request_id = request_id.trim();
    if auth_index.is_empty() || request_id.is_empty() {
        return None;
    }
    Some(format!("{}-{auth_index}-{request_id}", chrono::Local::now().format("%Y%m%d%H%M%S")))
}

/// 200 reply for a successful non-stream execution: `Content-Type: application/json`, upstream
/// headers where not already set, trace id when known.
pub fn ok_reply(info: &ReqInfo, ok: ExecOk, body: Bytes) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    write_upstream_headers(&mut headers, &ok.headers);
    if let Some(index) = &ok.auth_index
        && let Some(id) = trace_id(index, &info.request_id)
        && let Ok(v) = HeaderValue::from_str(&id)
    {
        headers.insert(HeaderName::from_static(TRACE_ID_HEADER), v);
    }
    Reply {
        status: 200,
        headers,
        body,
    }
}
