//! Websocket error frames and transport error mapping (Go: codex_websockets_errors.go and the
//! error helpers of codex_websockets_connection.go).

use std::time::Duration;

use bytes::Bytes;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ErrorCode, ExecError};
use http::{HeaderMap, HeaderName, HeaderValue};

use super::conn::{CLOSE_MESSAGE_TOO_BIG, ReadError};
use crate::codex::reasoning::{ReplayScope, clear_replay_on_invalid_signature};
use crate::codex::terminal::{is_usage_limit_error, parse_retry_after, status_error, status_text};

const MESSAGE_TOO_BIG_BODY: &str = r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#;

/// Status of an `error` frame: `status`, else `status_code`; 0 when neither is positive.
fn frame_status(frame: &Value) -> i64 {
    let status = frame.g("status").int();
    if status == 0 { frame.g("status_code").int() } else { status }
}

/// An `error` frame with a positive status as an error carrying the frame's headers (Go:
/// parseCodexWebsocketErrorWithCooling). Frames without a status are not errors here.
pub fn parse_error_frame(frame: &Value, model_level_cooling: bool) -> Option<ExecError> {
    if frame.g("type").str().trim() != "error" {
        return None;
    }
    let status = frame_status(frame);
    if status <= 0 {
        return None;
    }
    let out = build_error_payload(frame, status);
    let out_bytes = cpa_json::to_vec(&out);
    let usage_limit = is_usage_limit_error(&out_bytes);
    // ExecError carries a u16; an out-of-range frame status is still an error (clamped).
    let status = u16::try_from(status).unwrap_or(u16::MAX);
    let mut err = status_error(status, String::from_utf8_lossy(&out_bytes).into_owned());
    err.credential_scoped = usage_limit && !model_level_cooling;
    if let Some(retry) = parse_retry_after(status, &out_bytes, std::time::SystemTime::now()) {
        err.retry_after = Some(retry);
    } else if is_connection_limit_error(frame) {
        err.retry_after = Some(Duration::ZERO);
    }
    err.headers = error_headers(frame);
    Some(err)
}

/// Drops the reasoning replay state when the frame reports an invalid thinking signature.
pub fn clear_replay_on_error_frame(scope: &ReplayScope, frame: &Value) -> Result<(), ExecError> {
    let status = frame_status(frame);
    if status <= 0 {
        return Ok(());
    }
    let payload = cpa_json::to_vec(&build_error_payload(frame, status));
    clear_replay_on_invalid_signature(scope, u16::try_from(status).unwrap_or(u16::MAX), &payload)
}

fn build_error_payload(frame: &Value, status: i64) -> Value {
    let mut out = cpa_json::parse_str("{}");
    cpa_json::set(&mut out, "status", status);
    let body = frame.g("body");
    if body.exists() {
        cpa_json::set(&mut out, "body", body.value());
        let body_error = body.g("error");
        if body_error.exists() {
            cpa_json::set(&mut out, "error", body_error.value());
            return out;
        }
    }
    let error = frame.g("error");
    if error.exists() {
        cpa_json::set(&mut out, "error", error.value());
        return out;
    }
    cpa_json::set(&mut out, "error.type", "server_error");
    cpa_json::set(&mut out, "error.message", u16::try_from(status).map_or("", status_text));
    out
}

fn is_connection_limit_error(frame: &Value) -> bool {
    ["error.code", "error.type", "body.error.code", "body.error.type", "code", "error"]
        .iter()
        .any(|path| frame.g(path).str().trim() == "websocket_connection_limit_reached")
}

/// The frame's `headers` object as response headers (strings, numbers and booleans only).
fn error_headers(frame: &Value) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    let node = frame.g("headers");
    if !node.is_object() {
        return mapped;
    }
    for (key, value) in node.entries() {
        let name = key.trim();
        if name.is_empty() {
            continue;
        }
        let text = match value.v() {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Number(_) | Value::Bool(_)) => value.raw().trim().to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&text)) {
            mapped.insert(name, value);
        }
    }
    mapped
}

/// Upstream closed with 1009: request-scoped 413.
pub fn message_too_big_error() -> ExecError {
    let mut err = status_error(413, MESSAGE_TOO_BIG_BODY).with_code(ErrorCode::RequestScoped);
    err.body = Some(Bytes::from_static(MESSAGE_TOO_BIG_BODY.as_bytes()));
    err
}

/// Maps a read failure: a 1009 close becomes the message-too-big error (Go: mapCodexWebsocketReadError).
pub fn map_read_error(err: &ReadError) -> ExecError {
    match err {
        ReadError::Close(close) if close.code == CLOSE_MESSAGE_TOO_BIG => message_too_big_error(),
        other => ExecError::new(0, other.to_string()),
    }
}

/// Maps a write failure; a peer 1009 close seen on the connection wins (Go: mapCodexWebsocketWriteError).
pub fn map_write_error(text: String, close: Option<super::conn::CloseInfo>) -> ExecError {
    match close {
        Some(info) if info.code == CLOSE_MESSAGE_TOO_BIG => message_too_big_error(),
        _ => ExecError::new(0, text),
    }
}

/// Whether a failed first send may be retried on a fresh connection.
pub fn should_retry_send(err: &ExecError) -> bool {
    !err.is_request_scoped()
}

/// A frame as an SSE `data:` line for the translators.
pub fn encode_as_sse(payload: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(payload.len() + 6);
    line.extend_from_slice(b"data: ");
    line.extend_from_slice(payload);
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(s: &str) -> Value {
        cpa_json::parse(s.as_bytes())
    }

    #[test]
    fn error_frame_carries_status_body_error_and_headers() {
        let err = parse_error_frame(
            &frame(r#"{"type":"error","status":429,"body":{"error":{"type":"usage_limit_reached","resets_in_seconds":60}},"headers":{"x-codex-primary-used-percent":100,"retry-after":" 5 ","skip":null}}"#),
            false,
        )
        .unwrap();
        assert_eq!(err.status, 429);
        assert!(err.credential_scoped);
        assert_eq!(err.retry_after, Some(Duration::from_secs(60)));
        assert_eq!(err.headers.get("x-codex-primary-used-percent").unwrap(), "100");
        assert_eq!(err.headers.get("retry-after").unwrap(), "5");
        assert!(err.headers.get("skip").is_none());
        assert_eq!(
            err.message,
            r#"{"status":429,"body":{"error":{"type":"usage_limit_reached","resets_in_seconds":60}},"error":{"type":"usage_limit_reached","resets_in_seconds":60}}"#
        );
    }

    #[test]
    fn frames_without_status_are_not_errors_and_default_body_is_server_error() {
        assert!(parse_error_frame(&frame(r#"{"type":"error","error":{"message":"x"}}"#), false).is_none());
        let err = parse_error_frame(&frame(r#"{"type":"error","status_code":502}"#), false).unwrap();
        assert_eq!(err.message, r#"{"status":502,"error":{"type":"server_error","message":"Bad Gateway"}}"#);
    }

    #[test]
    fn out_of_range_status_is_still_an_error() {
        let err = parse_error_frame(&frame(r#"{"type":"error","status":70000}"#), false).unwrap();
        assert_eq!(err.status, u16::MAX);
        assert!(err.message.contains(r#""status":70000"#));
    }

    #[test]
    fn connection_limit_retries_immediately() {
        let err = parse_error_frame(&frame(r#"{"type":"error","status":429,"error":{"code":"websocket_connection_limit_reached"}}"#), false).unwrap();
        assert_eq!(err.retry_after, Some(Duration::ZERO));
    }

    #[test]
    fn close_1009_maps_to_request_scoped_413() {
        let err = map_read_error(&ReadError::Close(super::super::conn::CloseInfo { code: 1009, text: "big".into() }));
        assert_eq!(err.status, 413);
        assert!(err.is_request_scoped());
        assert!(err.message.contains("message_too_big"));
        assert_eq!(map_read_error(&ReadError::Other("eof".into())).status, 0);
        assert!(!should_retry_send(&err));
    }
}
