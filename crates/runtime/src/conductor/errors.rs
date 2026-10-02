//! Conductor errors and failure classification (Go: errors.go, selector.go error types,
//! conductor_cooldown.go predicates, internal/clienterror).
//!
//! Upstream failures arrive as [`ExecError`]; per-attempt outcomes are stored as
//! [`AuthError`] (Go `auth.Error`). The predicates here work on a [`Failure`] view so the same
//! rules apply to both. `ExecError::message` plays the role of Go's `err.Error()`: executors put
//! the upstream error body text there, exactly like the Go `statusErr` types.

use std::sync::LazyLock;
use std::time::Duration;

use cpa_auth::types::AuthError;
use cpa_json::J;
use http::{HeaderMap, HeaderValue, header};
use regex::Regex;
use serde_json::Value;

use crate::executor::{ErrorCode, ExecError};

use super::util::parse_suffix;

pub const CODE_AUTH_NOT_FOUND: &str = "auth_not_found";
pub const CODE_AUTH_UNAVAILABLE: &str = "auth_unavailable";
pub const CODE_PROVIDER_NOT_FOUND: &str = "provider_not_found";
pub const CODE_EXECUTOR_NOT_FOUND: &str = "executor_not_found";
pub const CODE_EMPTY_STREAM: &str = "empty_stream";
pub const CODE_MODEL_COOLDOWN: &str = "model_cooldown";
pub const CODE_MODEL_NOT_FOUND: &str = "model_not_found";
pub const CODE_UNAUTHORIZED: &str = "unauthorized";
pub const CODE_REQUEST_SCOPED: &str = "request_scoped";
pub const CODE_CONNECTION_LIFECYCLE: &str = "connection_lifecycle";
pub const CODE_TRANSIENT_TRANSPORT: &str = "transient_transport";
pub const CODE_FORCE_COOLDOWN: &str = "force_cooldown";

/// Machine code string for an executor-declared [`ErrorCode`].
pub fn error_code_str(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::RequestScoped => CODE_REQUEST_SCOPED,
        ErrorCode::ConnectionLifecycle => CODE_CONNECTION_LIFECYCLE,
        ErrorCode::TransientTransport => CODE_TRANSIENT_TRANSPORT,
        ErrorCode::ForceCooldown => CODE_FORCE_COOLDOWN,
    }
}

/// `AuthError` helpers mirroring the Go `*Error` methods.
pub trait AuthErrorExt {
    /// Go `Error()`: `code: message`, or the bare message when there is no code.
    fn go_string(&self) -> String;
    fn status_code(&self) -> i32;
    fn is_request_scoped(&self) -> bool;
}

impl AuthErrorExt for AuthError {
    fn go_string(&self) -> String {
        if self.code.is_empty() {
            self.message.clone()
        } else {
            format!("{}: {}", self.code, self.message)
        }
    }

    fn status_code(&self) -> i32 {
        self.http_status
    }

    fn is_request_scoped(&self) -> bool {
        self.code == CODE_REQUEST_SCOPED
    }
}

// ---- Constructors for conductor-generated errors ----

/// A conductor `*Error{Code, Message}` as an `ExecError` (message is Go's `Error()` text).
pub fn auth_error(code: &str, message: &str, status: i32) -> ExecError {
    let mut e = ExecError::new(status.max(0) as u16, format!("{code}: {message}"));
    e.auth_code = Some(code.to_string());
    e.upstream_attempted = false;
    e
}

pub fn auth_not_found(message: &str) -> ExecError {
    auth_error(CODE_AUTH_NOT_FOUND, message, 0)
}

/// `auth_not_found` carrying the latest candidate error as its cause (Go `WithCause`).
pub fn auth_not_found_with_cause(message: &str, cause: Option<&str>) -> ExecError {
    with_cause(auth_error(CODE_AUTH_NOT_FOUND, message, 0), cause)
}

pub fn provider_not_found(message: &str) -> ExecError {
    auth_error(CODE_PROVIDER_NOT_FOUND, message, 0)
}

pub fn executor_not_found() -> ExecError {
    auth_error(CODE_EXECUTOR_NOT_FOUND, "executor not registered", 0)
}

pub fn empty_stream(message: &str) -> ExecError {
    let mut e = auth_error(CODE_EMPTY_STREAM, message, 0);
    e.retryable = true;
    e
}

/// Appends " (last upstream error: <summary>)" like Go's `errorWithCause.Error`.
pub fn with_cause(mut err: ExecError, cause: Option<&str>) -> ExecError {
    let Some(cause) = cause else { return err };
    err.cause_text = Some(cause.to_string());
    let summary = extract_upstream_error_summary(cause);
    if !summary.is_empty() && !err.message.contains(&summary) {
        err.message = format!("{} (last upstream error: {summary})", err.message);
    }
    err
}

/// Raw message of a conductor error without the `code: ` prefix and cause suffix.
pub fn auth_error_base_message(err: &ExecError) -> String {
    let mut msg = err.message.as_str();
    if let Some(code) = err.auth_code.as_deref()
        && let Some(rest) = msg.strip_prefix(code).and_then(|r| r.strip_prefix(": "))
    {
        msg = rest;
    }
    if err.cause_text.is_some()
        && let Some(idx) = msg.rfind(" (last upstream error: ")
    {
        msg = &msg[..idx];
    }
    msg.to_string()
}

/// `auth_unavailable` (Go newAuthUnavailableErrorWithCause): 503 + `Retry-After` when a recovery
/// time is known.
pub fn auth_unavailable(
    next: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    cause: Option<&str>,
) -> ExecError {
    let mut err = auth_error(CODE_AUTH_UNAVAILABLE, "no auth available", 0);
    if let Some(next) = next.filter(|n| *n > now) {
        err.status = 503;
        err.retryable = true;
        let retry_after = (next - now).to_std().unwrap_or(Duration::ZERO);
        if let Some(h) = safe_retry_after_header(retry_after) {
            err.headers = h;
        }
    }
    with_cause(err, cause)
}

/// Terminal upstream-auth failure: every candidate is blocked by a 401 (non-retryable 503).
pub fn terminal_auth_error(cause: Option<&str>) -> ExecError {
    let mut err = auth_error(CODE_AUTH_UNAVAILABLE, "no auth available", 503);
    err.terminal_auth = true;
    with_cause(err, cause)
}

fn safe_retry_after_header(retry_after: Duration) -> Option<HeaderMap> {
    if retry_after.is_zero() {
        return None;
    }
    let mut seconds = retry_after.as_secs();
    if retry_after.subsec_nanos() != 0 {
        seconds += 1;
    }
    let seconds = seconds.max(1);
    let mut h = HeaderMap::new();
    h.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    Some(h)
}

/// `model_cooldown`: every credential for the model is cooling. HTTP 429 with a JSON message and
/// `Retry-After`; `model` is the client-requested route model.
pub fn model_cooldown_error(
    model: &str,
    provider: &str,
    reset_in: Duration,
    cause: Option<&str>,
) -> ExecError {
    let model_name = if model.is_empty() {
        "requested model"
    } else {
        model
    };
    let mut message = format!("All credentials for model {model_name} are cooling down");
    if !provider.is_empty() {
        message = format!("{message} via provider {provider}");
    }
    let mut reset_seconds = reset_in.as_secs();
    if reset_in.subsec_nanos() != 0 {
        reset_seconds += 1;
    }
    let display = if !reset_in.is_zero() && reset_in < Duration::from_secs(1) {
        Duration::from_secs(1)
    } else {
        // Round half away from zero to whole seconds.
        let secs = reset_in.as_secs() + u64::from(reset_in.subsec_millis() >= 500);
        Duration::from_secs(secs)
    };
    let mut body = serde_json::Map::new();
    body.insert("code".into(), Value::String(CODE_MODEL_COOLDOWN.into()));
    body.insert("model".into(), Value::String(model.to_string()));
    body.insert(
        "reset_time".into(),
        Value::String(
            cpa_config::GoDuration(display.as_nanos().min(i64::MAX as u128) as i64).to_string(),
        ),
    );
    body.insert("reset_seconds".into(), Value::from(reset_seconds));
    if !provider.is_empty() {
        body.insert("provider".into(), Value::String(provider.to_string()));
    }
    if let Some(cause) = cause {
        let summary = extract_upstream_error_summary(cause);
        if !summary.is_empty() {
            body.insert("last_upstream_error".into(), Value::String(summary.clone()));
            message = format!("{message} (last error: {summary})");
        }
    }
    body.insert("message".into(), Value::String(message));
    let mut root = serde_json::Map::new();
    root.insert("error".into(), Value::Object(body));
    let text = cpa_auth::util::marshal_compact(&Value::Object(root)).unwrap_or_default();

    let mut err = ExecError::new(429, text);
    err.auth_code = Some(CODE_MODEL_COOLDOWN.to_string());
    err.upstream_attempted = false;
    err.cause_text = cause.map(str::to_string);
    err.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    err.headers
        .insert(header::RETRY_AFTER, HeaderValue::from(reset_seconds));
    err
}

/// Headers that are safe to relay for a conductor-generated error (retry/cooldown hints only);
/// empty for upstream errors (Go: SafeResponseHeaders).
pub fn safe_response_headers(err: &ExecError) -> HeaderMap {
    if matches!(
        err.auth_code.as_deref(),
        Some(CODE_MODEL_COOLDOWN) | Some(CODE_AUTH_UNAVAILABLE)
    ) {
        err.headers.clone()
    } else {
        HeaderMap::new()
    }
}

/// Adds routing context to `auth_not_found` / `auth_unavailable` errors for the client (Go:
/// handlers enrichAuthSelectionError). `model_cooldown` and other errors pass through unchanged.
pub fn enrich_auth_selection_error(err: ExecError, providers: &[String], model: &str) -> ExecError {
    if is_model_cooldown(&err) {
        return err;
    }
    let Some(code) = err.auth_code.clone() else {
        return err;
    };
    if code != CODE_AUTH_NOT_FOUND && code != CODE_AUTH_UNAVAILABLE {
        return err;
    }
    let provider_text = if providers.is_empty() {
        "unknown".to_string()
    } else {
        providers.join(",")
    };
    let model_text = if model.trim().is_empty() {
        "unknown"
    } else {
        model.trim()
    };
    let mut base = auth_error_base_message(&err).trim().to_string();
    if base.is_empty() {
        base = "no auth available".into();
    }
    let summary = err
        .cause_text
        .as_deref()
        .map(extract_upstream_error_summary)
        .unwrap_or_default();
    let mut detail = if !summary.is_empty() && !base.contains(&summary) {
        format!(
            "{base} (providers={provider_text}, model={model_text}; last upstream error: {summary})"
        )
    } else {
        format!("{base} (providers={provider_text}, model={model_text})")
    };
    if format!(",{provider_text},").contains(",claude,") {
        detail.push_str(
            "; check Claude auth/key session and cooldown state via /v0/management/auth-files",
        );
    }
    let mut out = err.clone();
    out.message = format!("{code}: {detail}");
    if out.status == 0 {
        out.status = 503;
    }
    out
}

/// Whether the error is one of the "no usable credential" outcomes (Go: isAuthUnavailableError).
pub fn is_auth_unavailable_error(err: &ExecError) -> bool {
    matches!(
        err.auth_code.as_deref(),
        Some(CODE_AUTH_UNAVAILABLE) | Some(CODE_MODEL_COOLDOWN)
    )
}

pub fn is_model_cooldown(err: &ExecError) -> bool {
    err.auth_code.as_deref() == Some(CODE_MODEL_COOLDOWN)
}

// ---- Upstream error summary / sanitizer ----

macro_rules! lazy_re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| match Regex::new($pat) {
            Ok(re) => re,
            // Patterns are literals checked by tests; an unreachable fallback keeps this panic-free.
            Err(_) => Regex::new("$^").unwrap_or_else(|_| unreachable!()),
        });
    };
}

lazy_re!(
    SCHEME_AUTH,
    r#"(?i)((?:[A-Za-z0-9.+_\-]+:)?//)(?:[^:\s/@]+:[^@\s]+|[^@\s/]+)@"#
);
lazy_re!(
    QUERY_PARAM,
    r#"(?i)([?&][A-Za-z0-9_.-]*(?:key|token|secret|password|auth|sig|signature)=)[^&\s,\r\n;]+"#
);
lazy_re!(COOKIE, r#"(?i)\b(?:set-)?cookie\s*:[^\r\n]+"#);
lazy_re!(AUTH_HEADER, r#"(?i)\bauthorization\s*[:=]\s*[^\r\n]+"#);
lazy_re!(
    NATURAL_SECRET,
    r#"(?i)\b([A-Za-z0-9_.-]*(?:api[ _-]?key|access[ _-]?token|client[ _-]?secret|private[ _-]?key|secret[ _-]?key|password|secret|token|credentials?|sessionid))\s*(?:(?:is|was|provided|used)?\s*[:= ]\s*|\s+is\s+|\s+was\s+|\s+provided\s+|\s+)(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#
);
lazy_re!(
    KV,
    r#"(?i)((?:'|")?(?:[A-Za-z0-9_.-]*(?:key|token|secret|password|credential|credentials|bearer|sessionid|auth|signature|sig))(?:'|")?\s*[=:]\s*)(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#
);
lazy_re!(
    INVALID_TOKEN,
    r#"(?i)\b(invalid|bad|expired|unknown)\s+(?:api\s+key|access\s+token|refresh\s+token|token|key|secret|password|credentials?|bearer)\s*(?:[:= ]\s*)?(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,\r\n;]+)"#
);
lazy_re!(
    SK_KEY,
    r#"\b(?:sk-[A-Za-z0-9._~+/=-]{6,}|ghp_[A-Za-z0-9._~+/=-]{6,})\b"#
);
lazy_re!(BEARER, r#"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]+"#);
lazy_re!(DQ_PATH, r#""/[^"\r\n]+""#);
lazy_re!(SQ_PATH, r#"'/[^'\r\n]+'"#);
lazy_re!(BT_PATH, r#"`/[^`\r\n]+`"#);
lazy_re!(
    PATH_CONNECTOR,
    r#"(?i)\s+(to|from|into|onto|for|via|with|and)\s+/"#
);
lazy_re!(
    UNIX_PATH,
    r#"(^|[\s\(\[\{<"';,=])(/(?:[^/\s\r\n"',;?#()<>{}\[\]]+(?:\s+[^/\s\r\n"',;?#()<>{}\[\]]+)*/)*[^/:\s\r\n"',;?#()<>{}\[\]]+(?::[^/:\s\r\n"',;?#()<>{}\[\]]+)?)"#
);
lazy_re!(
    FILE_EXT_PATH,
    r#"(^|[\s"'`(\[,;=])(/[^\s:\r\n"'`,;\])>]+(?:\s+[^\s:\r\n"'`,;\])>]+)*\.(?:json|yaml|yml|key|pem|txt|log|toml|conf|env|crt|cer))"#
);
lazy_re!(WIN_PATH, r#"(?i)\b[A-Za-z]:\\[^\r\n:,;'"<>]+"#);
lazy_re!(WIN_UNC_PATH, r#"\\\\[^\r\n:,;'"<>]+\\[^\r\n:,;'"<>]+"#);

/// Extracts and sanitizes a concise error summary from upstream error text (Go:
/// ExtractUpstreamErrorSummary).
pub fn extract_upstream_error_summary(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let mut json_part = raw;
    if let Some(idx) = raw.find(": {")
        && idx < 50
    {
        json_part = raw[idx + 2..].trim();
    }
    if cpa_json::valid(json_part.as_bytes()) {
        let parsed = cpa_json::parse(json_part.as_bytes());
        let (mut code, mut message) = (String::new(), String::new());
        let err_node = parsed.g("error");
        if err_node.exists() {
            if err_node.is_object() {
                code = err_node.g("code").str().trim().to_string();
                if code.is_empty() {
                    code = err_node.g("type").str().trim().to_string();
                }
                message = err_node.g("message").str().trim().to_string();
            } else if err_node.is_string() {
                message = err_node.str().trim().to_string();
            }
        }
        if code.is_empty() && message.is_empty() {
            code = parsed.g("code").str().trim().to_string();
            if code.is_empty() {
                code = parsed.g("type").str().trim().to_string();
            }
            message = parsed.g("message").str().trim().to_string();
        }
        let summary = if !code.is_empty() && !message.is_empty() {
            if code.eq_ignore_ascii_case(&message)
                || message.to_lowercase().contains(&code.to_lowercase())
            {
                message
            } else {
                format!("{code}: {message}")
            }
        } else if !message.is_empty() {
            message
        } else {
            code
        };
        if !summary.is_empty() {
            return sanitize_upstream_error_summary(&summary);
        }
    }
    sanitize_upstream_error_summary(raw)
}

const KNOWN_ERROR_PREFIXES: [&str; 17] = [
    "permission denied",
    "no such file",
    "file not found",
    "access denied",
    "operation not permitted",
    "denied",
    "read-only",
    "is a directory",
    "not a directory",
    "cannot find",
    "no space",
    "connection refused",
    "timeout",
    "failed",
    "error",
    "not supported",
    "invalid argument",
];

fn sanitize_no_truncate(s: &str) -> String {
    let mut s = s.trim().to_string();
    if s.is_empty() {
        return s;
    }
    s = SCHEME_AUTH
        .replace_all(&s, "${1}[REDACTED_AUTH]@")
        .into_owned();
    s = QUERY_PARAM.replace_all(&s, "${1}[REDACTED]").into_owned();
    s = DQ_PATH.replace_all(&s, "\"[REDACTED_PATH]\"").into_owned();
    s = SQ_PATH.replace_all(&s, "'[REDACTED_PATH]'").into_owned();
    s = BT_PATH.replace_all(&s, "`[REDACTED_PATH]`").into_owned();
    s = WIN_PATH.replace_all(&s, "[REDACTED_PATH]").into_owned();
    s = WIN_UNC_PATH.replace_all(&s, "[REDACTED_PATH]").into_owned();

    // Connector-separated paths like "copy /tmp/a TO /tmp/b: denied".
    if let Some(m) = PATH_CONNECTOR.find(&s) {
        let first_part = s[..m.start()].to_string();
        let conn_raw = s[m.start()..m.end() - 1].to_string();
        let second_part = format!("/{}", &s[m.end()..]);
        return format!(
            "{}{}{}",
            sanitize_no_truncate(&first_part),
            conn_raw,
            sanitize_no_truncate(&second_part)
        );
    }

    // Path before the error colon: first known error phrase, else the first ": ".
    let lower = s.to_lowercase();
    let mut colon_idx: Option<usize> = None;
    for word in KNOWN_ERROR_PREFIXES {
        let target = format!(": {word}");
        if let Some(idx) = lower.find(&target)
            && colon_idx.is_none_or(|c| idx < c)
        {
            colon_idx = Some(idx);
        }
    }
    // Byte indexes of the lowercased copy only line up when lowercasing preserved lengths.
    if lower.len() != s.len() {
        colon_idx = None;
    }
    let colon_idx = colon_idx.or_else(|| s.find(": "));

    if let Some(colon_idx) = colon_idx
        && s.is_char_boundary(colon_idx)
    {
        let prefix = s[..colon_idx].to_string();
        let suffix = s[colon_idx..].to_string();
        let pb = prefix.as_bytes();
        let mut slash_idx: Option<usize> = None;
        for i in 0..pb.len() {
            if pb[i] != b'/' {
                continue;
            }
            if i > 0 && pb[i - 1] == b'/' {
                continue;
            }
            if i >= 6 {
                let before = &prefix[..i];
                if before.ends_with("http:/")
                    || before.ends_with("https:/")
                    || before.ends_with("://")
                {
                    continue;
                }
            }
            if i == 0
                || matches!(
                    pb[i - 1],
                    b' ' | b'\t' | b'(' | b'[' | b'{' | b'<' | b'"' | b'\'' | b'`' | b'='
                )
            {
                slash_idx = Some(i);
                break;
            }
        }
        if let Some(slash_idx) = slash_idx {
            let lead = &prefix[..slash_idx];
            let mut path_part = prefix[slash_idx..].to_string();
            let mut trail = String::new();
            while let Some(last) = path_part.chars().last() {
                if matches!(last, ')' | ']' | '}' | '>') {
                    trail.insert(0, last);
                    path_part.pop();
                } else {
                    break;
                }
            }
            let path_part = if path_part.contains(" /") {
                path_part
                    .split(" /")
                    .map(|_| "[REDACTED_PATH]")
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                "[REDACTED_PATH]".to_string()
            };
            s = format!("{lead}{path_part}{trail}{suffix}");
        }
    }

    for _ in 0..3 {
        let prev = s.clone();
        s = UNIX_PATH
            .replace_all(&s, "${1}[REDACTED_PATH]")
            .into_owned();
        if s == prev {
            break;
        }
    }
    s = FILE_EXT_PATH
        .replace_all(&s, "${1}[REDACTED_PATH]")
        .into_owned();
    s = COOKIE.replace_all(&s, "Cookie: [REDACTED]").into_owned();
    s = AUTH_HEADER
        .replace_all(&s, "Authorization: [REDACTED]")
        .into_owned();
    s = SK_KEY.replace_all(&s, "sk-[REDACTED]").into_owned();
    s = BEARER.replace_all(&s, "Bearer [REDACTED]").into_owned();
    s = INVALID_TOKEN
        .replace_all(&s, "${1} token [REDACTED]")
        .into_owned();
    s = NATURAL_SECRET
        .replace_all(&s, "${1}: [REDACTED]")
        .into_owned();
    s = KV.replace_all(&s, "${1}[REDACTED]").into_owned();
    s
}

/// Removes credentials, tokens and paths from upstream error text and bounds it to 256 runes.
pub fn sanitize_upstream_error_summary(s: &str) -> String {
    let s = sanitize_no_truncate(s);
    let runes: Vec<char> = s.chars().collect();
    if runes.len() > 256 {
        let head: String = runes[..253].iter().collect();
        return format!("{head}...");
    }
    s
}

// ---- Failure view and predicates ----

/// What the classifiers need to know about a failure.
#[derive(Debug, Clone, Copy)]
pub struct Failure<'a> {
    pub status: i32,
    /// Go `err.Error()`.
    pub text: &'a str,
    /// Executor declared the failure request-scoped.
    pub request_scoped: bool,
    /// Machine code when the failure is a conductor-style `*Error`.
    pub code: Option<&'a str>,
    /// Raw `Error.Message` when the failure is a conductor-style `*Error`.
    pub raw_message: Option<&'a str>,
}

impl<'a> Failure<'a> {
    pub fn of_exec(e: &'a ExecError) -> Failure<'a> {
        let text = if e.message.is_empty() {
            e.body
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
        } else {
            e.message.as_str()
        };
        Failure {
            status: e.status as i32,
            text,
            request_scoped: e.is_request_scoped(),
            code: e.auth_code.as_deref(),
            raw_message: None,
        }
    }
}

/// `*Error` stored in a result, viewed as an error value (`code: message`).
pub struct ResultFailure {
    text: String,
    status: i32,
    request_scoped: bool,
    code: String,
    message: String,
}

impl ResultFailure {
    pub fn of(e: &AuthError) -> Self {
        ResultFailure {
            text: e.go_string(),
            status: e.http_status,
            request_scoped: e.is_request_scoped(),
            code: e.code.clone(),
            message: e.message.clone(),
        }
    }

    pub fn failure(&self) -> Failure<'_> {
        Failure {
            status: self.status,
            text: &self.text,
            request_scoped: self.request_scoped,
            code: if self.code.is_empty() {
                None
            } else {
                Some(&self.code)
            },
            raw_message: Some(&self.message),
        }
    }
}

const REQUEST_FAULT_CODES: [&str; 9] = [
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

const REQUEST_FAULT_TYPES: [&str; 4] = [
    "invalid_request",
    "invalid_request_error",
    "bad_request_error",
    "invalid_prompt",
];

fn json_body(text: &str) -> Option<Value> {
    let body = text.trim();
    if body.is_empty() || !cpa_json::valid(body.as_bytes()) {
        return None;
    }
    Some(cpa_json::parse(body.as_bytes()))
}

fn any_path_lower(v: &Value, paths: &[&str], pred: impl Fn(&str) -> bool) -> bool {
    paths
        .iter()
        .any(|p| pred(v.g(p).str().trim().to_lowercase().as_str()))
}

/// Port of `clienterror.IsRequestFault`: an upstream failure caused by the request itself.
pub fn is_request_fault(status: i32, text: &str) -> bool {
    if status == 402 || status == 429 {
        return false;
    }
    let body = json_body(text);
    if status == 401
        && body.as_ref().is_some_and(|b| {
            any_path_lower(
                b,
                &[
                    "error.type",
                    "type",
                    "response.error.type",
                    "body.error.type",
                ],
                |t| t == "authentication_error",
            )
        })
    {
        return false;
    }
    if body.as_ref().is_some_and(|b| {
        any_path_lower(
            b,
            &[
                "error.code",
                "code",
                "response.error.code",
                "body.error.code",
            ],
            |c| c == "model_not_found" || c == "model_not_found_error",
        )
    }) {
        return false;
    }
    if let Some(b) = &body {
        if any_path_lower(
            b,
            &[
                "error.code",
                "code",
                "response.error.code",
                "body.error.code",
            ],
            |c| REQUEST_FAULT_CODES.contains(&c),
        ) {
            return true;
        }
        if any_path_lower(
            b,
            &[
                "error.type",
                "type",
                "response.error.type",
                "body.error.type",
            ],
            |t| REQUEST_FAULT_TYPES.contains(&t),
        ) {
            return true;
        }
    }
    if is_item_not_persisted(text) {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

/// Plain-text 404 for a response item the upstream never stored (`store` was false).
pub fn is_item_not_persisted(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
}

const MODEL_SUPPORT_PATTERNS: [&str; 10] = [
    "model_not_supported",
    "requested model is not supported",
    "requested model is unsupported",
    "requested model is unavailable",
    "model is not supported",
    "model not supported",
    "unsupported model",
    "model unavailable",
    "not available for your plan",
    "not available for your account",
];

fn is_model_support_message(message: &str) -> bool {
    let lower = message.trim().to_lowercase();
    !lower.is_empty() && MODEL_SUPPORT_PATTERNS.iter().any(|p| lower.contains(p))
}

fn is_cloudflare_message(message: &str) -> bool {
    let lower = message.trim().to_lowercase();
    lower.contains("challenge-platform")
        || lower.contains("cf-mitigated")
        || lower.contains("cloudflare challenge")
        || (lower.contains("just a moment") && lower.contains("cloudflare"))
}

fn is_invalid_grant_message(message: &str) -> bool {
    message.to_lowercase().contains("invalid_grant")
}

impl Failure<'_> {
    /// Go: isExplicitModelNotFoundError(err, "").
    pub fn is_explicit_model_not_found(&self, requested_model: &str) -> bool {
        if self.code.is_some_and(is_model_not_found_identifier) {
            return true;
        }
        if let Some(raw) = self.raw_message
            && is_structured_model_not_found(raw, requested_model)
        {
            return true;
        }
        is_structured_model_not_found(self.text, requested_model)
    }

    pub fn is_model_support(&self) -> bool {
        if self.is_explicit_model_not_found("") {
            return true;
        }
        if !matches!(self.status, 400 | 404 | 422) {
            return false;
        }
        is_model_support_message(self.raw_message.unwrap_or(self.text))
    }

    pub fn is_cloudflare_challenge(&self) -> bool {
        self.status < 500 && is_cloudflare_message(self.raw_message.unwrap_or(self.text))
    }

    pub fn is_invalid_grant(&self) -> bool {
        if !is_invalid_grant_message(self.text) && !self.code.is_some_and(is_invalid_grant_message)
        {
            return false;
        }
        matches!(self.status, 0 | 400 | 401)
    }

    /// Client fault that must neither rotate nor penalize credentials (Go: isRequestInvalidError).
    pub fn is_request_invalid(&self) -> bool {
        if self.request_scoped {
            return true;
        }
        if self.is_cloudflare_challenge() || self.is_invalid_grant() || self.is_model_support() {
            return false;
        }
        if is_request_fault(self.status, self.text) {
            return true;
        }
        if let Some(raw) = self.raw_message
            && !raw.is_empty()
            && is_request_fault(self.status, raw)
        {
            return true;
        }
        false
    }

    pub fn is_unauthorized(&self) -> bool {
        if self.status == 401 {
            return true;
        }
        let raw = self.text.to_lowercase();
        raw.contains("status 401") || raw.contains("401 unauthorized")
    }
}

fn is_model_not_found_identifier(value: &str) -> bool {
    let mut candidate = value.trim().to_lowercase();
    if let Some(fragment) = candidate.rfind('#')
        && fragment + 1 < candidate.len()
    {
        candidate = candidate[fragment + 1..].to_string();
    } else {
        if let Some(q) = candidate.find('?') {
            candidate.truncate(q);
        }
        let trimmed = candidate.trim_end_matches('/').to_string();
        candidate = trimmed;
        if let Some(sep) = candidate.rfind(['/', ':']) {
            candidate = candidate[sep + 1..].to_string();
        }
    }
    let normalized = candidate.replace(['-', ' '], "_");
    matches!(
        normalized.as_str(),
        "model_not_found"
            | "model_not_found_error"
            | "unknown_model"
            | "model_does_not_exist"
            | "model_not_exist"
    )
}

fn is_not_found_error_identifier(value: &str) -> bool {
    let normalized = value.trim().to_lowercase().replace(['-', ' '], "_");
    normalized == "not_found" || normalized == "not_found_error"
}

fn is_structured_model_not_found(message: &str, requested_model: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return false;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(payload) => contains_structured_model_not_found(&payload, requested_model),
        Err(_) => false,
    }
}

fn contains_structured_model_not_found(value: &Value, requested_model: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut not_found_type = false;
            let mut exact_model_reference = false;
            for (key, item) in map {
                if let Some(text) = item.as_str() {
                    match key.trim().to_lowercase().as_str() {
                        "code" => {
                            if is_model_not_found_identifier(text) {
                                return true;
                            }
                        }
                        "type" => {
                            if is_model_not_found_identifier(text) {
                                return true;
                            }
                            not_found_type = not_found_type || is_not_found_error_identifier(text);
                        }
                        "error" | "message" | "detail" | "error_description" | "title" => {
                            if is_explicit_model_not_found_message(text, requested_model) {
                                return true;
                            }
                            exact_model_reference = exact_model_reference
                                || is_exact_requested_model_reference(text, requested_model);
                        }
                        _ => {}
                    }
                }
                if matches!(item, Value::Object(_) | Value::Array(_))
                    && contains_structured_model_not_found(item, requested_model)
                {
                    return true;
                }
            }
            not_found_type && exact_model_reference
        }
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item.as_str()
                    && is_explicit_model_not_found_message(text, requested_model)
                {
                    return true;
                }
                if contains_structured_model_not_found(item, requested_model) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn trim_msg(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .trim_matches(|c| matches!(c, ' ' | '.' | '!' | ';' | '\t' | '\r' | '\n'))
        .to_string()
}

fn is_explicit_model_not_found_message(message: &str, requested_model: &str) -> bool {
    let lower = trim_msg(message);
    if lower.is_empty() {
        return false;
    }
    if lower.contains("in request") || lower.contains("in body") || lower.contains("request body") {
        return false;
    }
    let normalized = lower.replace('-', "_");
    if normalized.contains("model_not_found") || normalized.contains("unknown_model") {
        return true;
    }
    for prefix in ["no such model", "unknown model"] {
        if lower != prefix
            && !lower.starts_with(&format!("{prefix} "))
            && !lower.starts_with(&format!("{prefix}:"))
        {
            continue;
        }
        let remainder = lower[prefix.len()..].trim().to_string();
        let remainder = remainder
            .strip_prefix(':')
            .unwrap_or(&remainder)
            .trim()
            .to_string();
        if remainder.is_empty() {
            return true;
        }
        let (missing_suffix, matches) = trim_requested_model_reference(&remainder, requested_model);
        return matches && missing_suffix.is_empty();
    }
    for prefix in [
        "the requested model",
        "requested model",
        "the model",
        "model",
    ] {
        if lower != prefix
            && !lower.starts_with(&format!("{prefix} "))
            && !lower.starts_with(&format!("{prefix}:"))
        {
            continue;
        }
        let remainder = lower[prefix.len()..].trim().to_string();
        let remainder = remainder
            .strip_prefix(':')
            .unwrap_or(&remainder)
            .trim()
            .to_string();
        if is_missing_model_phrase(&remainder) {
            return true;
        }
        let (missing_suffix, matches) = trim_requested_model_reference(&remainder, requested_model);
        return matches && is_missing_model_phrase(&missing_suffix);
    }
    false
}

fn is_exact_requested_model_reference(message: &str, requested_model: &str) -> bool {
    let lower = trim_msg(message);
    for prefix in [
        "the requested model",
        "requested model",
        "the model",
        "model",
    ] {
        if lower != prefix
            && !lower.starts_with(&format!("{prefix} "))
            && !lower.starts_with(&format!("{prefix}:"))
        {
            continue;
        }
        let remainder = lower[prefix.len()..].trim().to_string();
        let remainder = remainder
            .strip_prefix(':')
            .unwrap_or(&remainder)
            .trim()
            .to_string();
        let (suffix, matches) = trim_requested_model_reference(&remainder, requested_model);
        return matches && suffix.is_empty();
    }
    false
}

fn trim_requested_model_reference(value: &str, requested_model: &str) -> (String, bool) {
    let model = requested_model.trim().to_lowercase();
    if model.is_empty() {
        return (String::new(), false);
    }
    for candidate in [
        model.clone(),
        format!("'{model}'"),
        format!("\"{model}\""),
        format!("`{model}`"),
    ] {
        if value == candidate {
            return (String::new(), true);
        }
        if let Some(remainder) = value.strip_prefix(&candidate)
            && (remainder.is_empty() || remainder.starts_with([' ', ':', ',']))
        {
            return (
                remainder.trim_start_matches([' ', ':', ',']).to_string(),
                true,
            );
        }
    }
    (String::new(), false)
}

fn is_missing_model_phrase(value: &str) -> bool {
    matches!(
        value.trim_matches(|c| matches!(c, ' ' | '.' | '!' | ';' | '\t' | '\r' | '\n')),
        "not found"
            | "was not found"
            | "could not be found"
            | "does not exist"
            | "doesn't exist"
            | "not exist"
            | "is unknown"
            | "does not exist or you do not have access to it"
    )
}

// ---- Transport / lifecycle classification (message based; HTTP-status failures excluded) ----

pub fn is_connection_lifecycle_message(message: &str) -> bool {
    let lower = message.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if matches!(
        lower.as_str(),
        "context canceled" | "context deadline exceeded" | "eof" | "unexpected eof"
    ) {
        return true;
    }
    lower.contains("websocket: close 1000")
        || lower.contains("websocket: close 1001")
        || lower.contains("websocket: close 1006")
        || lower.contains("unexpected eof")
}

pub fn is_transient_transport_message(message: &str) -> bool {
    let lower = message.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    [
        "tls: tls handshake",
        "tls handshake timeout",
        "wsarecv",
        "wsasend",
        "a connection attempt failed",
        "connection refused",
        "connection reset",
        "i/o timeout",
        "no such host",
        "server misbehaving",
        "network is unreachable",
        "no route to host",
        "broken pipe",
        "connection aborted",
        "use of closed network connection",
        "unexpected eof",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Transport/session lifecycle failure that must not cool credentials: websocket closes and
/// client aborts. Never applies to failures that carry an HTTP status.
pub fn is_connection_lifecycle_error(err: &ExecError) -> bool {
    if err.code == Some(ErrorCode::ConnectionLifecycle) {
        return true;
    }
    if err.status != 0 {
        return false;
    }
    is_connection_lifecycle_message(&err.message)
}

/// Pre-HTTP dial/TLS/DNS/reset failure: retried under request-retry without cooling.
pub fn is_transient_transport_error(err: &ExecError) -> bool {
    if err.code == Some(ErrorCode::TransientTransport) {
        return true;
    }
    err.status == 0 && is_transient_transport_message(&err.message)
}

pub fn is_connection_lifecycle_result(err: &AuthError) -> bool {
    if err.code == CODE_CONNECTION_LIFECYCLE {
        return true;
    }
    if err.http_status != 0 {
        return false;
    }
    is_connection_lifecycle_message(&err.message)
}

pub fn is_transient_transport_result(err: &AuthError) -> bool {
    if err.code == CODE_TRANSIENT_TRANSPORT {
        return true;
    }
    if err.http_status != 0 {
        return false;
    }
    is_transient_transport_message(&err.message)
}

pub fn is_request_scoped_not_found_result(err: &AuthError) -> bool {
    err.http_status == 404 && is_item_not_persisted(&err.message)
}

pub fn is_request_scoped_result(err: &AuthError) -> bool {
    if err.is_request_scoped() || is_request_scoped_not_found_result(err) {
        return true;
    }
    ResultFailure::of(err).failure().is_request_invalid()
}

pub fn is_invalid_grant_result(err: &AuthError) -> bool {
    if !is_invalid_grant_message(&err.code) && !is_invalid_grant_message(&err.message) {
        return false;
    }
    matches!(err.http_status, 0 | 400 | 401)
}

pub fn is_model_support_result(err: &AuthError) -> bool {
    let rf = ResultFailure::of(err);
    let f = Failure {
        raw_message: Some(&err.message),
        ..rf.failure()
    };
    if f.is_explicit_model_not_found("") {
        return true;
    }
    if !matches!(err.http_status, 400 | 404 | 422) {
        return false;
    }
    is_model_support_message(&err.message)
}

pub fn is_cloudflare_challenge_result(err: &AuthError) -> bool {
    err.http_status < 500 && is_cloudflare_message(&err.message)
}

/// Failures that must not mark auth/model cooling (Go: shouldSkipCredentialCooldown).
pub fn should_skip_credential_cooldown(err: Option<&AuthError>) -> bool {
    let Some(err) = err else { return false };
    if err.code == CODE_FORCE_COOLDOWN {
        return false;
    }
    is_request_scoped_result(err)
        || is_connection_lifecycle_result(err)
        || is_transient_transport_result(err)
}

/// Statuses that are retried in another credential round (Go: isCredentialRetryRoundStatus).
pub fn is_credential_retry_round_status(status: i32) -> bool {
    matches!(status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

pub fn is_request_retry_round_error(err: &ExecError) -> bool {
    is_credential_retry_round_status(err.status as i32) || is_transient_transport_error(err)
}

/// Go: resultErrorFromError. Classifies an executor failure into the stored `AuthError`.
pub fn result_error_from_error(err: &ExecError) -> AuthError {
    let mut code = match (&err.auth_code, err.code) {
        (Some(c), _) => c.clone(),
        (None, Some(c)) => error_code_str(c).to_string(),
        (None, None) => String::new(),
    };
    let message = if err.auth_code.is_some() {
        auth_error_base_message(err)
    } else {
        err.message.clone()
    };
    let f = Failure::of_exec(err);
    if f.is_explicit_model_not_found("") {
        if code.is_empty() || code == CODE_REQUEST_SCOPED {
            code = CODE_MODEL_NOT_FOUND.into();
        }
    } else if err.is_request_scoped() || f.is_request_invalid() {
        code = CODE_REQUEST_SCOPED.into();
    } else if is_connection_lifecycle_error(err) {
        if code.is_empty() || code == CODE_CONNECTION_LIFECYCLE {
            code = CODE_CONNECTION_LIFECYCLE.into();
        }
    } else if is_transient_transport_error(err)
        && (code.is_empty() || code == CODE_TRANSIENT_TRANSPORT)
    {
        code = CODE_TRANSIENT_TRANSPORT.into();
    }
    AuthError {
        code,
        message,
        retryable: err.retryable,
        http_status: err.status as i32,
    }
}

/// Go: refreshErrorFromError.
pub fn refresh_error_from_error(err: &ExecError) -> AuthError {
    let mut status = err.status as i32;
    if status == 0 && Failure::of_exec(err).is_unauthorized() {
        status = 401;
    }
    let mut e = AuthError {
        code: String::new(),
        message: err.message.clone(),
        retryable: false,
        http_status: status,
    };
    if status == 401 {
        e.code = CODE_UNAUTHORIZED.into();
    }
    e
}

pub fn is_count_tokens_endpoint_not_found(err: &ExecError, requested_model: &str) -> bool {
    if err.status != 404 {
        return false;
    }
    let base = parse_suffix(requested_model).model_name;
    !Failure::of_exec(err).is_explicit_model_not_found(&base)
}

pub fn is_responses_compact_request(alt: &str) -> bool {
    alt == "responses/compact"
}

pub fn is_responses_compact_request_fault(alt: &str, err: &ExecError) -> bool {
    if !is_responses_compact_request(alt) {
        return false;
    }
    let f = Failure::of_exec(err);
    if err.credential_scoped || f.is_cloudflare_challenge() || f.is_invalid_grant() {
        return false;
    }
    if is_request_fault(f.status, f.text) {
        return true;
    }
    matches!(f.status, 400 | 404 | 405 | 409 | 413 | 422 | 501)
}

pub fn is_responses_compact_availability_neutral(
    alt: &str,
    err: &ExecError,
    result_err: Option<&AuthError>,
) -> bool {
    if !is_responses_compact_request(alt) {
        return false;
    }
    if result_err.is_some_and(|r| r.code == CODE_FORCE_COOLDOWN) {
        return false;
    }
    let f = Failure::of_exec(err);
    if err.credential_scoped || f.is_cloudflare_challenge() || f.is_invalid_grant() {
        return false;
    }
    if let Some(r) = result_err
        && (is_cloudflare_challenge_result(r) || is_invalid_grant_result(r))
    {
        return false;
    }
    let mut status = f.status;
    if status == 0
        && let Some(r) = result_err
    {
        status = r.http_status;
    }
    !matches!(status, 401 | 402 | 403 | 429)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fault_rules() {
        assert!(is_request_fault(400, "bad"));
        assert!(!is_request_fault(
            429,
            r#"{"error":{"code":"invalid_value"}}"#
        ));
        assert!(is_request_fault(
            200,
            r#"{"error":{"code":"context_length_exceeded"}}"#
        ));
        assert!(!is_request_fault(
            401,
            r#"{"error":{"type":"authentication_error"}}"#
        ));
        assert!(!is_request_fault(
            400,
            r#"{"error":{"code":"model_not_found"}}"#
        ));
        assert!(is_request_fault(
            404,
            "Item with id 'x' not found. Items are not persisted when `store` is set to false."
        ));
        assert!(!is_request_fault(500, "boom"));
    }

    #[test]
    fn summary_sanitizes_and_bounds() {
        let s = extract_upstream_error_summary(
            r#"status: {"error":{"code":"x","message":"upstream rejected sk-abcdefghij"}}"#,
        );
        assert_eq!(s, "x: upstream rejected sk-[REDACTED]");
        let long = "a".repeat(400);
        assert_eq!(sanitize_upstream_error_summary(&long).chars().count(), 256);
        assert_eq!(
            sanitize_upstream_error_summary(
                "failed to open /tmp/secret/file.json: permission denied"
            ),
            "failed to open [REDACTED_PATH]: permission denied"
        );
    }

    #[test]
    fn model_cooldown_message_is_json_with_retry_after() {
        let e = model_cooldown_error("m", "claude", Duration::from_millis(1500), None);
        assert_eq!(e.status, 429);
        assert_eq!(
            e.headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("2")
        );
        let v: Value = serde_json::from_str(&e.message).unwrap();
        assert_eq!(v["error"]["code"], "model_cooldown");
        assert_eq!(v["error"]["reset_seconds"], 2);
        assert_eq!(v["error"]["reset_time"], "2s");
    }

    #[test]
    fn transport_and_lifecycle_classification() {
        let e = ExecError::new(0, "dial tcp: connection refused");
        assert!(is_transient_transport_error(&e));
        assert!(!is_connection_lifecycle_error(&e));
        let c = ExecError::new(0, "context canceled");
        assert!(is_connection_lifecycle_error(&c));
        assert!(!is_transient_transport_error(&c));
        let h = ExecError::new(502, "connection reset by peer");
        assert!(!is_transient_transport_error(&h));
    }

    #[test]
    fn result_error_codes() {
        let e = ExecError::new(400, r#"{"error":{"type":"invalid_request_error"}}"#);
        assert_eq!(result_error_from_error(&e).code, CODE_REQUEST_SCOPED);
        let e = ExecError::new(404, r#"{"error":{"code":"model_not_found"}}"#);
        assert_eq!(result_error_from_error(&e).code, CODE_MODEL_NOT_FOUND);
        let e = ExecError::new(0, "i/o timeout");
        assert_eq!(result_error_from_error(&e).code, CODE_TRANSIENT_TRANSPORT);
        let e = ExecError::new(429, "slow down");
        assert_eq!(result_error_from_error(&e).code, "");
    }
}
