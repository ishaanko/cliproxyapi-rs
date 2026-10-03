//! Websocket error frames and transport error mapping for the xAI transport (Go:
//! parseXAIWebsocketError, parseCodexWebsocketError and the codex read/write error mappers).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ErrorCode, ExecError};
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::codex::ws::conn::{CLOSE_MESSAGE_TOO_BIG, CloseInfo, ReadError};
use crate::helps::status::status_err;
use crate::xai::response::status_err_for_body;
use crate::xai::util::s;

const MESSAGE_TOO_BIG_BODY: &str = r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#;

fn status_text(status: u16) -> &'static str {
    http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("")
}

/// Status of an `error` frame: `status`, else `status_code`; 0 when neither is set.
fn frame_status(frame: &Value) -> i64 {
    let status = frame.g("status").int();
    if status == 0 { frame.g("status_code").int() } else { status }
}

/// Go: buildCodexWebsocketErrorPayload.
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

/// Go: parseCodexWebsocketErrorHeaders. Strings, numbers and booleans only.
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

/// Go: isCodexUsageLimitError.
fn is_usage_limit_error(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let parsed = cpa_json::parse(body);
    [parsed.g("error.type").str(), parsed.g("type").str()].iter().any(|c| c.trim().eq_ignore_ascii_case("usage_limit_reached"))
}

/// Go: parseCodexRetryAfter. Cooldown for usage-limit 429s from `resets_at` / `resets_in_seconds`.
fn codex_retry_after(status: u16, body: &[u8], now: SystemTime) -> Option<Duration> {
    if status != 429 || body.is_empty() {
        return None;
    }
    let parsed = cpa_json::parse(body);
    let error = parsed.g("error").value();
    for quota in [&error, &parsed] {
        if !quota.g("type").str().trim().eq_ignore_ascii_case("usage_limit_reached") {
            continue;
        }
        let resets_at = quota.g("resets_at").int();
        if resets_at > 0
            && let Ok(delta) = (UNIX_EPOCH + Duration::from_secs(resets_at as u64)).duration_since(now)
            && !delta.is_zero()
        {
            return Some(delta);
        }
        let in_seconds = quota.g("resets_in_seconds").int();
        if in_seconds > 0 {
            return Some(Duration::from_secs(in_seconds as u64));
        }
    }
    None
}

/// Go: parseCodexWebsocketError. An `error` frame with a positive status, carrying the frame's
/// headers; `None` for anything else.
fn parse_codex_error(frame: &Value) -> Option<ExecError> {
    if frame.g("type").str().trim() != "error" {
        return None;
    }
    let status = frame_status(frame);
    if status <= 0 {
        return None;
    }
    let out = cpa_json::to_vec(&build_error_payload(frame, status));
    let usage_limit = is_usage_limit_error(&out);
    let code = u16::try_from(status).unwrap_or(u16::MAX);
    let mut err = status_err(code, String::from_utf8_lossy(&out).into_owned());
    err.credential_scoped = usage_limit;
    if let Some(retry) = codex_retry_after(code, &out, SystemTime::now()) {
        err.retry_after = Some(retry);
    } else if is_connection_limit_error(frame) {
        err.retry_after = Some(Duration::ZERO);
    }
    err.headers = error_headers(frame);
    Some(err)
}

/// Go: xaiBareWebsocketErrorStatus.
fn bare_error_status(frame: &Value) -> i64 {
    for path in ["error.code", "error.status", "code"] {
        let raw = frame.g(path).str().trim().to_string();
        if raw.is_empty() {
            continue;
        }
        if let Ok(status) = raw.parse::<i64>()
            && status > 0
        {
            return status;
        }
    }
    let message = s(frame, "error.message");
    let message = message.trim();
    if message.contains(r#""code":"400""#) || message.contains("Request validation error") {
        return 400;
    }
    500
}

/// Go: parseXAIWebsocketError. `payload` is the raw frame text and `frame` its parse.
pub fn parse_error_frame(payload: &[u8], frame: &Value) -> Option<ExecError> {
    if let Some(mut ws_err) = parse_codex_error(frame) {
        // Apply the normalized status (403 bad-credentials becomes 401) and the provider retry
        // hint while keeping the websocket headers.
        let xai = status_err_for_body(ws_err.status, payload);
        ws_err.status = xai.status;
        if xai.retry_after.is_some() {
            ws_err.retry_after = xai.retry_after;
        }
        return Some(ws_err);
    }
    if payload.is_empty() || !frame.g("error").exists() {
        return None;
    }
    let mut status = frame.g("status").int();
    if status <= 0 {
        status = frame.g("status_code").int();
    }
    if status <= 0 {
        status = bare_error_status(frame);
    }
    let mut out = cpa_json::parse_str("{}");
    cpa_json::set(&mut out, "type", "error");
    cpa_json::set(&mut out, "status", status);
    cpa_json::set(&mut out, "error", frame.g("error").value());
    let code = u16::try_from(status).unwrap_or(u16::MAX);
    Some(status_err_for_body(code, &cpa_json::to_vec(&out)))
}

/// Upstream closed with 1009: request-scoped 413.
pub fn message_too_big_error() -> ExecError {
    let mut err = status_err(413, MESSAGE_TOO_BIG_BODY).with_code(ErrorCode::RequestScoped);
    err.body = Some(Bytes::from_static(MESSAGE_TOO_BIG_BODY.as_bytes()));
    err
}

/// Go: mapXAIWebsocketReadError. A 1009 close becomes the message-too-big error.
pub fn map_read_error(err: &ReadError) -> ExecError {
    match err {
        ReadError::Close(close) if close.code == CLOSE_MESSAGE_TOO_BIG => message_too_big_error(),
        other => ExecError::new(0, other.to_string()),
    }
}

/// Go: mapXAIWebsocketWriteError. A peer 1009 close seen on the connection wins.
pub fn map_write_error(text: String, close: Option<CloseInfo>) -> ExecError {
    match close {
        Some(info) if info.code == CLOSE_MESSAGE_TOO_BIG => message_too_big_error(),
        _ => ExecError::new(0, text),
    }
}

/// Go: shouldRetryXAIWebsocketSend. A failed first send may be retried on a fresh connection
/// unless the error is request scoped.
pub fn should_retry_send(err: &ExecError) -> bool {
    !err.is_request_scoped()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(s: &str) -> Value {
        cpa_json::parse(s.as_bytes())
    }

    fn parse(s: &str) -> Option<ExecError> {
        parse_error_frame(s.as_bytes(), &frame(s))
    }

    #[test]
    fn bad_credentials_frames_map_to_401_and_free_usage_gets_a_day_of_cooldown() {
        let err = parse(r#"{"type":"error","status":403,"error":{"code":"bad-credentials"}}"#).unwrap();
        assert_eq!(err.status, 401);
        let err = parse(r#"{"type":"error","status":429,"code":"free-usage-exhausted","error":{"message":"x"}}"#).unwrap();
        assert_eq!(err.status, 429);
        assert_eq!(err.retry_after, Some(Duration::from_secs(24 * 3600)));
    }

    #[test]
    fn bare_error_frames_derive_a_status() {
        let err = parse(r#"{"error":{"message":"Request validation error: bad"}}"#).unwrap();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, r#"{"type":"error","status":400,"error":{"message":"Request validation error: bad"}}"#);
        assert_eq!(parse(r#"{"error":{"code":"503"}}"#).unwrap().status, 503);
        assert_eq!(parse(r#"{"error":"boom"}"#).unwrap().status, 500);
        assert!(parse(r#"{"type":"response.created"}"#).is_none());
    }
}
