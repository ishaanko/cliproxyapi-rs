//! Responses stream error chunks and error-text sanitization
//! (Go: handlers/openai_responses_stream_error.go and the sanitize helpers of
//! openai/openai_responses_handlers.go).

use cpa_core::util::{GoJsonStyle, go_json_sorted};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

use crate::error::{ErrorMessage, status_text};

fn marshal(v: &Value) -> String {
    go_json_sorted(v, GoJsonStyle::MARSHAL_USE_NUMBER).unwrap_or_else(|| "null".into())
}

fn parse_object(text: &str) -> Option<Map<String, Value>> {
    let t = text.trim();
    if t.is_empty() || !cpa_json::valid(t.as_bytes()) {
        return None;
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// `(code, errorType)` per status (`openAIResponsesStreamErrorClassFor`).
fn error_class(status: u16) -> (&'static str, &'static str) {
    const CLIENT: &str = "invalid_request_error";
    const SERVER: &str = "server_error";
    match status {
        401 => ("invalid_api_key", CLIENT),
        403 => ("insufficient_quota", CLIENT),
        429 => ("rate_limit_exceeded", CLIENT),
        404 => ("model_not_found", CLIENT),
        408 => ("request_timeout", SERVER),
        s if s >= 500 => ("internal_server_error", SERVER),
        s if s >= 400 => ("invalid_request_error", CLIENT),
        _ => ("unknown_error", CLIENT),
    }
}

fn nonblank(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// `Sprint` of a JSON value (numbers keep their literal, strings are plain).
fn sprint(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "<nil>".into(),
        other => other.to_string(),
    }
}

/// `openAIResponsesStreamErrorDetail`.
fn error_detail(status: u16, err_text: &str, code: &str, message: &str) -> Map<String, Value> {
    let mut code = code.to_string();
    let mut message = message.to_string();
    let payload = parse_object(err_text);
    if let Some(p) = &payload {
        if let Some(Value::Object(e)) = p.get("error") {
            return e.clone();
        }
        if let Some(Value::Object(resp)) = p.get("response")
            && let Some(Value::Object(e)) = resp.get("error")
        {
            return e.clone();
        }
        if let Some(m) = nonblank(p.get("message")) {
            message = m;
        }
        if let Some(v) = p.get("code")
            && !v.is_null()
        {
            code = nonblank(Some(v)).unwrap_or_else(|| sprint(v).trim().to_string());
        }
    }
    let (_, error_type) = error_class(status);
    let mut detail = Map::new();
    detail.insert("type".into(), Value::String(error_type.into()));
    detail.insert("code".into(), Value::String(code));
    detail.insert("message".into(), Value::String(message));
    detail.insert("param".into(), Value::Null);
    if let Some(p) = &payload {
        if let Some(t) = nonblank(p.get("type"))
            && t != "error"
        {
            detail.insert("type".into(), Value::String(t));
        }
        if let Some(param) = p.get("param") {
            detail.insert("param".into(), param.clone());
        }
    }
    detail
}

fn seq_from_payload(err_text: &str) -> Option<i64> {
    let p = parse_object(err_text)?;
    let n = p.get("sequence_number")?;
    match n {
        // json.Number.Int64(): only integral literals count.
        Value::Number(num) => num.to_string().parse::<i64>().ok(),
        _ => None,
    }
}

/// `BuildOpenAIResponsesStreamErrorChunk`.
pub fn build_error_chunk(status: u16, err_text: &str, sequence_number: i64) -> Vec<u8> {
    let status = if status == 0 { 500 } else { status };
    let sequence_number = sequence_number.max(0);
    let mut message = err_text.trim().to_string();
    if message.is_empty() {
        message = status_text(status).to_string();
    }
    let code = error_class(status).0;
    let seq = seq_from_payload(err_text).unwrap_or(sequence_number);
    let detail = error_detail(status, err_text, if code.is_empty() { "unknown_error" } else { code }, &message);
    let detail_json = marshal(&Value::Object(detail));
    format!(r#"{{"type":"error","error":{detail_json},"sequence_number":{seq}}}"#).into_bytes()
}

/// `BuildOpenAIResponsesStreamFailedChunk`: the terminal event used by Codex clients.
pub fn build_failed_chunk(status: u16, err_text: &str, sequence_number: i64) -> Vec<u8> {
    let status = if status == 0 { 500 } else { status };
    let sequence_number = sequence_number.max(0);
    let chunk = build_error_chunk(status, err_text, sequence_number);
    let parsed: Value = serde_json::from_slice(&chunk).unwrap_or(Value::Null);
    let seq = parsed
        .get("sequence_number")
        .map(|v| v.to_string())
        .unwrap_or_else(|| sequence_number.to_string());
    let detail = parsed.get("error").cloned().unwrap_or(Value::Null);
    format!(
        r#"{{"type":"response.failed","sequence_number":{seq},"response":{{"status":"failed","error":{}}}}}"#,
        marshal(&detail)
    )
    .into_bytes()
}

// ------------------------------------------------------------------ sanitization

const MESSAGE_LIMIT: usize = 2048;
const FIELD_LIMIT: usize = 256;

static SENSITIVE_VALUE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)((?:"?(?:api[_-]?key|access[_-]?token|token|authorization|secret)"?)\s*[=:]\s*"?)([^\s"&,;}]+)"#)
        .expect("static regex")
});
static BEARER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").expect("static regex"));

fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        text.to_string()
    }
}

fn redact(text: &str) -> String {
    let once = SENSITIVE_VALUE.replace_all(text, "${1}[REDACTED]");
    BEARER.replace_all(&once, "Bearer [REDACTED]").into_owned()
}

/// `sanitizeResponsesStreamEventName`.
pub fn sanitize_event_name(name: &str) -> String {
    truncate(&redact(name.trim()), FIELD_LIMIT)
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.trim().to_lowercase().replace('-', "_");
    if k.contains("tokens") || k.contains("token_count") || k.contains("token_limit") || k.contains("token_usage") {
        return false;
    }
    matches!(
        k.as_str(),
        "authorization"
            | "secret"
            | "password"
            | "passwd"
            | "api_key"
            | "apikey"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "api_token"
            | "client_secret"
            | "client_key"
    ) || k.ends_with("_secret")
        || k.ends_with("_password")
        || k.ends_with("_api_key")
        || k.ends_with("_token")
}

fn sanitize_node(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(truncate(&redact(s), MESSAGE_LIMIT)),
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, item) in m {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), Value::String("[REDACTED]".into()));
                } else {
                    out.insert(k.clone(), sanitize_node(item));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sanitize_node).collect()),
        other => other.clone(),
    }
}

/// `responsesStreamErrorText`: the error text with secrets redacted and size bounded; JSON errors
/// are re-encoded (keys sorted) keeping only `error` and `sequence_number`.
pub fn stream_error_text(err: Option<&ErrorMessage>, status: u16) -> String {
    let mut text = status_text(status).to_string();
    if let Some(e) = err
        && !e.text.trim().is_empty()
    {
        text = e.text.trim().to_string();
    }
    let trimmed = text.trim().to_string();
    if !cpa_json::valid(trimmed.as_bytes()) {
        return truncate(&redact(&trimmed), MESSAGE_LIMIT);
    }
    let Some(root) = parse_object(&trimmed) else {
        return truncate(&redact(&trimmed), MESSAGE_LIMIT);
    };
    let error_node = match root.get("error") {
        Some(Value::Object(e)) => Some(e),
        _ => match root.get("response") {
            Some(Value::Object(resp)) => match resp.get("error") {
                Some(Value::Object(e)) => Some(e),
                _ => None,
            },
            _ => None,
        },
    };
    if let Some(node) = error_node {
        let mut out = Map::new();
        out.insert("error".into(), sanitize_node(&Value::Object(node.clone())));
        if let Some(seq) = root.get("sequence_number") {
            out.insert("sequence_number".into(), seq.clone());
        }
        return marshal(&Value::Object(out));
    }
    marshal(&sanitize_node(&Value::Object(root)))
}

/// `sanitizeResponsesStreamErrorMessage`.
pub fn sanitize_error_message(err: &ErrorMessage) -> ErrorMessage {
    let status = if (400..=599).contains(&err.status) { err.status } else { 500 };
    let mut safe = err.clone();
    safe.status = status;
    safe.text = stream_error_text(Some(err), status);
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn plain_error_chunk_shape() {
        assert_eq!(
            s(build_error_chunk(502, "boom", 3)),
            r#"{"type":"error","error":{"code":"internal_server_error","message":"boom","param":null,"type":"server_error"},"sequence_number":3}"#
        );
        assert_eq!(
            s(build_error_chunk(429, "slow", -1)),
            r#"{"type":"error","error":{"code":"rate_limit_exceeded","message":"slow","param":null,"type":"invalid_request_error"},"sequence_number":0}"#
        );
    }

    #[test]
    fn json_error_text_is_used_verbatim_and_sequence_wins() {
        let text = r#"{"error":{"type":"x","code":"c","message":"m","param":null},"sequence_number":9}"#;
        assert_eq!(
            s(build_error_chunk(400, text, 1)),
            r#"{"type":"error","error":{"code":"c","message":"m","param":null,"type":"x"},"sequence_number":9}"#
        );
        let flat = r#"{"message":"oops","code":429,"type":"rate","param":"p"}"#;
        assert_eq!(
            s(build_error_chunk(500, flat, 0)),
            r#"{"type":"error","error":{"code":"429","message":"oops","param":"p","type":"rate"},"sequence_number":0}"#
        );
    }

    #[test]
    fn failed_chunk_wraps_the_same_detail() {
        assert_eq!(
            s(build_failed_chunk(502, "boom", 4)),
            r#"{"type":"response.failed","sequence_number":4,"response":{"status":"failed","error":{"code":"internal_server_error","message":"boom","param":null,"type":"server_error"}}}"#
        );
    }

    #[test]
    fn secrets_are_redacted_and_text_is_bounded() {
        assert_eq!(redact("token=abc123 and Bearer sk-live.XYZ"), "token=[REDACTED] and Bearer [REDACTED]");
        assert_eq!(redact(r#""api_key": "k""#), r#""api_key": "[REDACTED]""#);
        let long = "x".repeat(3000);
        let out = truncate(&long, MESSAGE_LIMIT);
        assert_eq!(out.chars().count(), MESSAGE_LIMIT + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn json_error_text_redacts_sensitive_keys() {
        let err = ErrorMessage::new(500, r#"{"error":{"message":"bad","refresh_token":"zzz","tokens_used":5},"extra":1}"#);
        assert_eq!(
            stream_error_text(Some(&err), 500),
            r#"{"error":{"message":"bad","refresh_token":"[REDACTED]","tokens_used":5}}"#
        );
    }

    #[test]
    fn sanitize_clamps_status() {
        let safe = sanitize_error_message(&ErrorMessage::new(200, "x"));
        assert_eq!(safe.status, 500);
        let kept = sanitize_error_message(&ErrorMessage::new(429, "x"));
        assert_eq!(kept.status, 429);
    }
}
