//! Dialect handlers (Go: sdk/api/handlers/{openai,claude,gemini}).

use axum::http::{HeaderMap, HeaderValue, header};
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

/// 200 reply for a successful non-stream execution: `Content-Type: application/json` plus
/// upstream headers where not already set.
pub fn ok_reply(ok: ExecOk, body: Bytes) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    write_upstream_headers(&mut headers, &ok.headers);
    Reply {
        status: 200,
        headers,
        body,
    }
}
