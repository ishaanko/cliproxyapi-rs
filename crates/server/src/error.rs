//! Handler-level error model (Go: interfaces.ErrorMessage, handlers.BuildErrorResponseBody*,
//! the Claude error shape, auth-selection error enrichment).

use std::time::Duration;

use axum::http::HeaderMap;
use cpa_json::J;
use cpa_runtime::executor::ExecError;
use serde_json::Value;

/// `http.StatusText`.
pub fn status_text(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

/// Handler error with its HTTP status (Go: `interfaces.ErrorMessage`). `text` is the Go
/// `Error.Error()` string: for upstream failures that is the upstream body.
#[derive(Debug, Clone, Default)]
pub struct ErrorMessage {
    /// 0 means unset (reported as 500).
    pub status: u16,
    pub text: String,
    /// Credential is unusable until re-login (`coreauth.IsTerminalAuthError`).
    pub terminal_auth: bool,
    /// `Retry-After` for cooldown/unavailable selection errors (`coreauth.SafeResponseHeaders`).
    pub retry_after: Option<Duration>,
    /// Upstream error headers, forwarded only with `passthrough-headers`.
    pub addon: HeaderMap,
    /// A trusted in-process component (a plugin) supplied the whole downstream response
    /// (Go: `DirectResponse` with `Body` and `Headers`).
    pub direct: Option<std::sync::Arc<DirectResponse>>,
}

/// Preformatted downstream response carried by an [`ErrorMessage`].
#[derive(Debug, Clone, Default)]
pub struct DirectResponse {
    pub body: bytes::Bytes,
    pub headers: HeaderMap,
}

impl ErrorMessage {
    pub fn new(status: u16, text: impl Into<String>) -> Self {
        ErrorMessage {
            status,
            text: text.into(),
            ..Default::default()
        }
    }

    /// Status with the Go fallback to 500.
    pub fn status_or_500(&self) -> u16 {
        if self.status > 0 { self.status } else { 500 }
    }

    /// Error text, falling back to the status text (as `WriteErrorResponse` does).
    pub fn text_or_status(&self) -> String {
        let status = self.status_or_500();
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            status_text(status).to_string()
        } else {
            trimmed.to_string()
        }
    }
}

/// `Error()` of an executor failure. Executors put the upstream body text in `message` (Go's
/// `statusErr{msg: body}`); `body` is only used when the message is empty.
pub fn exec_error_text(err: &ExecError) -> String {
    if !err.message.is_empty() {
        return err.message.clone();
    }
    match &err.body {
        Some(body) if !body.is_empty() => String::from_utf8_lossy(body).into_owned(),
        _ => String::new(),
    }
}

/// Conductor credential-selection failures render as `<code>: <message>` (Go `*auth.Error`).
/// `ExecError` on this base carries no machine code, so the code is read back from the text.
const SELECTION_CODES: &[&str] = &["auth_not_found", "auth_unavailable"];

/// Messages of selection failures built without the code prefix.
const SELECTION_MESSAGES: &[&str] = &[
    "no auth available",
    "no auth candidates",
    "selector returned no auth",
    "selector returned no eligible auth",
    "selected auth has no ID",
    "selector repeatedly returned an ineligible auth",
];

/// `(code, base message)` of an `auth_not_found` / `auth_unavailable` failure.
fn selection_parts(err: &ExecError) -> Option<(&'static str, &str)> {
    let message = err.message.trim();
    for code in SELECTION_CODES {
        if let Some(rest) = message.strip_prefix(code).and_then(|r| r.strip_prefix(": ")) {
            return Some((code, rest));
        }
    }
    SELECTION_MESSAGES
        .iter()
        .any(|m| message.starts_with(m))
        .then_some(("auth_not_found", message))
}

/// `isAuthSelectionUnavailable` / the codes handled by `enrichAuthSelectionError`.
pub fn is_selection_error(err: &ExecError) -> bool {
    selection_parts(err).is_some()
}

/// `executionErrorMessage`: converts an executor failure to a handler error.
pub fn exec_error_message(err: &ExecError) -> ErrorMessage {
    if let Some(t) = &err.terminated {
        return ErrorMessage {
            status: normalized_termination_status(i64::from(t.status)),
            text: err.message.clone(),
            direct: Some(std::sync::Arc::new(DirectResponse { body: t.body.clone(), headers: t.headers.clone() })),
            ..Default::default()
        };
    }
    // `coreauth.SafeResponseHeaders`: only conductor cooldown / unavailable failures expose
    // Retry-After, read from the headers the conductor attached (never upstream's own).
    let retry_after = cpa_runtime::conductor::errors::safe_response_headers(err)
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    ErrorMessage {
        status: err.status,
        text: exec_error_text(err),
        terminal_auth: err.terminal_auth,
        retry_after,
        addon: err.headers.clone(),
        direct: None,
    }
}

/// `normalizedTerminationStatus`: plugin-chosen statuses outside 200..=599 become 403.
pub fn normalized_termination_status(status: i64) -> u16 {
    if (200..=599).contains(&status) { status as u16 } else { 403 }
}

/// `enrichAuthSelectionError`: adds providers/model (and a Claude hint) to credential selection
/// failures; the status defaults to 503.
pub fn enrich_auth_selection_error(err: &ExecError, providers: &[String], model: &str) -> ExecError {
    let Some((code, base)) = selection_parts(err) else {
        return err.clone();
    };
    let provider_text = if providers.is_empty() {
        "unknown".to_string()
    } else {
        providers.join(",")
    };
    let model_text = if model.trim().is_empty() { "unknown" } else { model.trim() };
    let base = if base.trim().is_empty() { "no auth available" } else { base.trim() };
    let mut detail = format!("{base} (providers={provider_text}, model={model_text})");
    if format!(",{provider_text},").contains(",claude,") {
        detail.push_str("; check Claude auth/key session and cooldown state via /v0/management/auth-files");
    }
    let mut out = err.clone();
    out.message = format!("{code}: {detail}");
    if out.status == 0 {
        out.status = 503;
    }
    out
}

fn valid_json(text: &str) -> bool {
    !text.is_empty() && cpa_json::valid(text.as_bytes())
}

/// `json.Marshal` of a string (quoted, HTML-escaped).
fn jstr(s: &str) -> String {
    cpa_core::util::go_json_string(s)
}

/// `BuildErrorResponseBody`.
pub fn build_error_response_body(status: u16, err_text: &str) -> Vec<u8> {
    build_error_response_body_with_error(status, err_text, false)
}

/// `BuildErrorResponseBodyWithError`: the OpenAI-shaped error body. Valid JSON error text is
/// returned verbatim; `terminal_auth` selects the `upstream_authentication_required` shape.
pub fn build_error_response_body_with_error(status: u16, err_text: &str, terminal_auth: bool) -> Vec<u8> {
    let status = if status == 0 { 500 } else { status };
    let err_text = if err_text.trim().is_empty() { status_text(status) } else { err_text };
    let trimmed = err_text.trim();

    if terminal_auth {
        let mut message = err_text.to_string();
        if valid_json(trimmed)
            && let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(trimmed)
        {
            if let Some(Value::String(m)) = parsed.get("message").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
                message = m.clone();
            } else if let Some(Value::Object(e)) = parsed.get("error")
                && let Some(Value::String(m)) = e.get("message")
                && !m.is_empty()
            {
                message = m.clone();
            }
        }
        return format!(
            r#"{{"error":{{"message":{},"type":"authentication_error","code":"upstream_authentication_required","retryable":false}}}}"#,
            jstr(&message)
        )
        .into_bytes();
    }

    if valid_json(trimmed) {
        return trimmed.as_bytes().to_vec();
    }

    let (err_type, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("server_error", "request_timeout"),
        s if s >= 500 => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    let code_part = if code.is_empty() {
        String::new()
    } else {
        format!(r#","code":"{code}""#)
    };
    format!(
        r#"{{"error":{{"message":{},"type":"{err_type}"{code_part}}}}}"#,
        jstr(err_text)
    )
    .into_bytes()
}

/// Handler-local error JSON `{"error":{"message":..,"type":..}}` (Go: `ErrorResponse` via `c.JSON`).
pub fn error_response_json(message: &str, error_type: &str) -> Vec<u8> {
    format!(
        r#"{{"error":{{"message":{},"type":{}}}}}"#,
        jstr(message),
        jstr(error_type)
    )
    .into_bytes()
}

/// Error body for a handler error (`WriteErrorResponse` body part).
pub fn error_body(msg: &ErrorMessage) -> Vec<u8> {
    build_error_response_body_with_error(msg.status_or_500(), &msg.text_or_status(), msg.terminal_auth)
}

// ---------------------------------------------------------------- Claude shape

/// `claudeErrorTypeFromStatus`.
pub fn claude_error_type_from_status(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 => "overloaded_error",
        s if s >= 500 => "api_error",
        _ => "invalid_request_error",
    }
}

fn non_blank(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// `claudeErrorDetailFromText`: (type, message) with upstream Anthropic errors passed through.
pub fn claude_error_detail_from_text(status: u16, err_text: &str) -> (String, String) {
    let mut message = err_text.trim().to_string();
    if message.is_empty() {
        message = status_text(status).to_string();
    }
    let mut err_type = claude_error_type_from_status(status).to_string();
    if valid_json(&message)
        && let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message)
    {
        if let Some(Value::Object(e)) = payload.get("error") {
            if let Some(t) = non_blank(e.get("type")) {
                err_type = t;
            }
            if let Some(m) = non_blank(e.get("message")) {
                message = m;
            } else if let Some(c) = non_blank(e.get("code")) {
                message = c;
            }
        } else {
            if let Some(t) = non_blank(payload.get("type"))
                && t != "error"
            {
                err_type = t;
            }
            if let Some(m) = non_blank(payload.get("message")) {
                message = m;
            }
        }
    }
    (err_type, message)
}

/// `toClaudeError` serialized: `{"type":"error","error":{"type":..,"message":..}}`.
pub fn claude_error_body(msg: &ErrorMessage) -> Vec<u8> {
    let status = msg.status_or_500();
    let trimmed = msg.text.trim();
    let text = if trimmed.is_empty() { status_text(status).to_string() } else { trimmed.to_string() };
    let (t, m) = claude_error_detail_from_text(status, &text);
    format!(
        r#"{{"type":"error","error":{{"type":{},"message":{}}}}}"#,
        jstr(&t),
        jstr(&m)
    )
    .into_bytes()
}

/// `Retry-After` header value for a duration (whole seconds, rounded up, at least 1).
pub fn retry_after_seconds(d: Duration) -> Option<u64> {
    if d.is_zero() {
        return None;
    }
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    Some(secs.max(1))
}

/// First `error.code` style lookups shared by classification helpers.
fn body_str(body: &Value, paths: &[&str]) -> Vec<String> {
    paths
        .iter()
        .map(|p| body.g(p).str().trim().to_lowercase())
        .collect()
}

const REQUEST_FAULT_CODES: &[&str] = &[
    "cyber_policy",
    "context_length_exceeded",
    "message_too_big",
    "string_above_max_length",
    "invalid_prompt",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "previous_response_not_found",
];

const REQUEST_FAULT_TYPES: &[&str] = &["invalid_request", "invalid_request_error", "bad_request_error", "invalid_prompt"];

/// `clienterror.IsRequestFault`: the failure is caused by the request, so credentials must not
/// rotate or be penalized.
pub fn is_request_fault(status: u16, text: &str) -> bool {
    if status == 402 || status == 429 {
        return false;
    }
    let body = {
        let t = text.trim();
        if !t.is_empty() && cpa_json::valid(t.as_bytes()) {
            Some(cpa_json::parse(t.as_bytes()))
        } else {
            None
        }
    };
    if let Some(body) = &body {
        let types = body_str(body, &["error.type", "type", "response.error.type", "body.error.type"]);
        if status == 401 && types.iter().any(|t| t == "authentication_error") {
            return false;
        }
        let codes = body_str(body, &["error.code", "code", "response.error.code", "body.error.code"]);
        if codes.iter().any(|c| c == "model_not_found" || c == "model_not_found_error") {
            return false;
        }
        if codes.iter().any(|c| REQUEST_FAULT_CODES.contains(&c.as_str())) {
            return true;
        }
        if types.iter().any(|t| REQUEST_FAULT_TYPES.contains(&t.as_str())) {
            return true;
        }
    }
    if is_item_not_persisted(text) {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

/// `clienterror.IsItemNotPersisted`.
pub fn is_item_not_persisted(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(status: u16, text: &str) -> String {
        String::from_utf8(build_error_response_body(status, text)).unwrap()
    }

    #[test]
    fn openai_error_shapes() {
        assert_eq!(
            body(500, "plain text failure"),
            r#"{"error":{"message":"plain text failure","type":"server_error","code":"internal_server_error"}}"#
        );
        assert_eq!(
            body(401, "nope"),
            r#"{"error":{"message":"nope","type":"authentication_error","code":"invalid_api_key"}}"#
        );
        assert_eq!(body(400, "bad"), r#"{"error":{"message":"bad","type":"invalid_request_error"}}"#);
        assert_eq!(body(404, ""), r#"{"error":{"message":"Not Found","type":"invalid_request_error","code":"model_not_found"}}"#);
        // upstream JSON is preserved verbatim
        assert_eq!(body(400, r#" {"error": {"message": "x"}} "#), r#"{"error": {"message": "x"}}"#);
        // HTML characters are escaped like Go's encoder
        let escaped = format!(r#"{{"error":{{"message":"a{}u003cb","type":"invalid_request_error"}}}}"#, '\\');
        assert_eq!(body(400, "a<b"), escaped);
    }

    #[test]
    fn terminal_auth_shape() {
        let b = build_error_response_body_with_error(401, r#"{"error":{"message":"expired"}}"#, true);
        assert_eq!(
            String::from_utf8(b).unwrap(),
            r#"{"error":{"message":"expired","type":"authentication_error","code":"upstream_authentication_required","retryable":false}}"#
        );
    }

    #[test]
    fn claude_error_shapes() {
        let m = ErrorMessage::new(429, "slow down");
        assert_eq!(
            String::from_utf8(claude_error_body(&m)).unwrap(),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        );
        let upstream = ErrorMessage::new(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#,
        );
        assert_eq!(
            String::from_utf8(claude_error_body(&upstream)).unwrap(),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#
        );
        assert_eq!(claude_error_type_from_status(529), "overloaded_error");
        assert_eq!(claude_error_type_from_status(502), "api_error");
    }

    #[test]
    fn selection_error_enrichment() {
        let e = ExecError::new(0, "auth_not_found: no auth available");
        let out = enrich_auth_selection_error(&e, &["claude".into(), "codex".into()], "m");
        assert_eq!(out.status, 503);
        assert_eq!(
            out.message,
            "auth_not_found: no auth available (providers=claude,codex, model=m); check Claude auth/key session and cooldown state via /v0/management/auth-files"
        );
        let plain = enrich_auth_selection_error(&ExecError::new(503, "auth_unavailable: no auth available"), &["x".into()], "");
        assert_eq!(plain.message, "auth_unavailable: no auth available (providers=x, model=unknown)");
        let other = ExecError::new(500, "boom");
        assert_eq!(enrich_auth_selection_error(&other, &[], "m").message, "boom");
    }

    #[test]
    fn request_fault_classification() {
        assert!(is_request_fault(400, "x"));
        assert!(!is_request_fault(429, r#"{"error":{"code":"invalid_value"}}"#));
        assert!(!is_request_fault(404, r#"{"error":{"code":"model_not_found"}}"#));
        assert!(is_request_fault(500, r#"{"error":{"code":"context_length_exceeded"}}"#));
        assert!(!is_request_fault(401, r#"{"error":{"type":"authentication_error"}}"#));
        assert!(!is_request_fault(502, "bad gateway"));
    }
}
