//! Credential refresh delegated to the Home control plane (Go: helps/home_refresh.go).
//!
//! With Home enabled, executors do not refresh locally: they ask Home for the refreshed
//! credential and map its answer (an updated auth, or a typed error) to an [`ExecError`].

use std::sync::LazyLock;

use cpa_auth::{Auth, Status};
use cpa_config::Config;
use cpa_home::HomeError;
use cpa_runtime::executor::ExecError;
use regex::Regex;
use serde::Deserialize;

use super::status::status_err;
use super::usage::reporter::access_token_sha256;

/// Home's error reply: `{"error": {"type", "message", "code", "diagnostic", "upstream"}}`.
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    diagnostic: Option<String>,
    #[serde(default)]
    upstream: Option<UpstreamResponse>,
}

/// A relayed upstream response; `body` is base64 (Go marshals `[]byte`).
#[derive(Deserialize)]
struct UpstreamResponse {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct AuthEnvelope {
    auth: Auth,
    #[serde(default)]
    auth_index: Option<String>,
}

/// Replaces local refresh when Home is enabled (Go: `RefreshAuthViaHome`). `None` means Home is
/// disabled and the executor refreshes locally; `Some` is Home's verdict: the updated auth or the
/// mapped failure.
pub async fn refresh_auth_via_home(cfg: &Config, auth: &Auth) -> Option<Result<Auth, ExecError>> {
    if !cfg.home.enabled {
        return None;
    }
    Some(refresh(auth).await)
}

async fn refresh(auth: &Auth) -> Result<Auth, ExecError> {
    let Some(client) = cpa_home::kv::current().filter(|c| c.heartbeat_ok()) else {
        return Err(status_err(503, "home control center unavailable"));
    };

    let mut auth_index = auth.index.trim().to_string();
    if auth_index.is_empty() {
        auth_index = auth.clone().ensure_index().trim().to_string();
    }
    if auth_index.is_empty() {
        return Err(status_err(502, "home refresh: auth_index is empty"));
    }

    let raw = match client.get_refresh_auth(&auth_index, &access_token_sha256(auth)).await {
        Ok(raw) => raw,
        Err(err) => {
            tracing::debug!(
                "{}",
                safe_diagnostic_for_log(&format!("Home refresh transport failed: {}", safe_error_diagnostic(&err)))
            );
            return Err(status_err(503, "home refresh temporarily unavailable"));
        }
    };

    if let Ok(ErrorEnvelope { error: Some(detail) }) = serde_json::from_slice::<ErrorEnvelope>(&raw) {
        return Err(home_error(detail));
    }

    let (mut updated, returned_index) = match parse_refresh_auth(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            // Never the serde text: it can quote payload fragments.
            tracing::debug!(
                "{}",
                safe_diagnostic_for_log(&format!("Home refresh response decode failed: {}", safe_json_diagnostic(&err)))
            );
            return Err(status_err(502, "home returned invalid auth payload"));
        }
    };
    if updated.disabled || updated.status == Status::Disabled {
        return Err(status_err(401, "credential unauthorized"));
    }
    if !returned_index.is_empty() {
        auth_index = returned_index;
    }
    updated.index = auth_index;
    updated.ensure_index();
    Ok(updated)
}

/// Maps Home's error object: an upstream response passes through with its status and body, any
/// other failure becomes a generic status for the error type.
fn home_error(detail: ErrorDetail) -> ExecError {
    let kind = detail.kind.as_deref().map(str::trim).filter(|k| !k.is_empty());
    let code = kind.or_else(|| detail.code.as_deref().map(str::trim)).unwrap_or_default();
    if let Some(upstream) = detail.upstream {
        let status = upstream.status.and_then(|s| u16::try_from(s).ok()).unwrap_or(0);
        let body = decode_base64_body(upstream.body.as_deref().unwrap_or_default());
        log_diagnostic(detail.diagnostic.as_deref(), || format!("Home refresh upstream response: status={status}"), true);
        // The relayed body doubles as the message, even when empty (no "status N" fallback).
        return ExecError::new(status, String::from_utf8_lossy(&body).into_owned()).with_body(body);
    }
    let status = status_from_home_error_code(code);
    let message = match status {
        401 => "credential unauthorized",
        404 => "credential refresh target not found",
        _ => "credential refresh temporarily unavailable",
    };
    log_diagnostic(
        detail.diagnostic.as_deref(),
        || if code.is_empty() { message.to_string() } else { format!("Home refresh failed: type={}", code.to_lowercase()) },
        false,
    );
    status_err(status, message)
}

/// Logs Home's diagnostic when it sent one, else `fallback()` (Go carries it on the error for
/// the conductor's logs and passes Home's text and error types through `SafeDiagnosticForLog`;
/// `fallback_is_safe` marks the fixed upstream-status line, which Go logs as is).
fn log_diagnostic(diagnostic: Option<&str>, fallback: impl FnOnce() -> String, fallback_is_safe: bool) {
    let text = match diagnostic.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => safe_diagnostic_for_log(d),
        None if fallback_is_safe => fallback(),
        None => safe_diagnostic_for_log(&fallback()),
    };
    tracing::debug!("{text}");
}

/// Go `[]byte` JSON: standard base64; a non-base64 body reads as empty.
fn decode_base64_body(encoded: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(encoded).unwrap_or_default()
}

/// Home answers with `{"auth": {...}, "auth_index": "..."}` or a bare auth object.
fn parse_refresh_auth(raw: &[u8]) -> Result<(Auth, String), serde_json::Error> {
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(raw)?;
    if object.contains_key("auth") {
        let envelope: AuthEnvelope = serde_json::from_slice(raw)?;
        let index = envelope.auth_index.unwrap_or_default().trim().to_string();
        return Ok((envelope.auth, index));
    }
    Ok((serde_json::from_slice(raw)?, String::new()))
}

fn status_from_home_error_code(code: &str) -> u16 {
    match code.trim().to_lowercase().as_str() {
        "authentication_error" | "unauthorized" | "invalid_grant" | "refresh_token_expired" | "refresh_token_revoked"
        | "refresh_token_reused" => 401,
        "model_not_found" => 404,
        _ => 503,
    }
}

const DIAGNOSTIC_LOG_RUNE_LIMIT: usize = 300;
const DIAGNOSTIC_LOG_SCAN_RUNE_LIMIT: usize = 600;

static ACCESS_TOKEN_EXPIRED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)access token expired").expect("static regex"));
static SENSITIVE_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(["']?(?:access[\s_-]*token|refresh[\s_-]*token|id[\s_-]*token|api[\s_-]*key|client[\s_-]*secret|private[\s_-]*key|proxy[\s_-]*authorization|authorization|password|credential|token|secret)["']?\s*[:=]\s*)(?:(?:bearer|basic)\s+[^\s,;]+|"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|[^\s,;&}\]]+)"#,
    )
    .expect("static regex")
});
static AUTHORIZATION_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)\s+[^\s,;]+").expect("static regex"));
static URL_USERINFO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^/\s@]+@").expect("static regex"));
static DIAGNOSTIC_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bstatus(?:\s+code)?\s*[:=]?\s*([1-5][0-9]{2})\b").expect("static regex"));

/// Bounded single-line diagnostic for ordinary logs: keeps the "access token expired" signal and
/// redacts credential values and URL userinfo (Go: `logging.SafeDiagnosticForLog`).
pub(crate) fn safe_diagnostic_for_log(message: &str) -> String {
    let (prefix, source_truncated) = match message.char_indices().nth(DIAGNOSTIC_LOG_SCAN_RUNE_LIMIT) {
        Some((i, _)) => (&message[..i], true),
        None => (message, false),
    };
    let mut excerpt = prefix.to_string();
    if source_truncated
        && !ACCESS_TOKEN_EXPIRED.is_match(&excerpt)
        && let Some(marker) = ACCESS_TOKEN_EXPIRED.find(message)
    {
        excerpt.push_str(" ... ");
        excerpt.push_str(marker.as_str());
    }
    let excerpt = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    if excerpt.is_empty() {
        return String::new();
    }
    let excerpt = URL_USERINFO.replace_all(&excerpt, "${1}[REDACTED]@");
    let excerpt = SENSITIVE_ASSIGNMENT.replace_all(&excerpt, "${1}\"[REDACTED]\"");
    let excerpt = AUTHORIZATION_VALUE.replace_all(&excerpt, "${1} [REDACTED]");
    truncate_diagnostic_excerpt(&excerpt, source_truncated)
}

/// Caps the excerpt at 300 characters, keeping the access-token-expired marker visible.
fn truncate_diagnostic_excerpt(message: &str, source_truncated: bool) -> String {
    let runes: Vec<char> = message.chars().collect();
    if runes.len() <= DIAGNOSTIC_LOG_RUNE_LIMIT {
        return if source_truncated { format!("{message}...") } else { message.to_string() };
    }
    let mut output: String = runes[..DIAGNOSTIC_LOG_RUNE_LIMIT].iter().collect();
    if let Some(m) = ACCESS_TOKEN_EXPIRED.find(message) {
        let marker_start = message[..m.start()].chars().count();
        let marker_len = m.as_str().chars().count();
        let marker_end = marker_start + marker_len;
        if marker_end > DIAGNOSTIC_LOG_RUNE_LIMIT {
            const SEPARATOR: &str = " ... ";
            let prefix_limit = DIAGNOSTIC_LOG_RUNE_LIMIT.saturating_sub(SEPARATOR.chars().count() + marker_len);
            output = runes[..prefix_limit].iter().collect::<String>() + SEPARATOR + m.as_str();
        }
    }
    output + "..."
}

/// Allowlisted failure signals of a Home transport error, never its free-form text (Go:
/// `logging.SafeErrorDiagnostic`).
fn safe_error_diagnostic(err: &HomeError) -> String {
    let mut parts: Vec<String> = Vec::new();
    fn add(parts: &mut Vec<String>, part: &str) {
        if !parts.iter().any(|p| p == part) {
            parts.push(part.to_string());
        }
    }
    if err.is_timeout() {
        add(&mut parts, "timeout");
    }
    let original = err.to_string();
    if original.trim().eq_ignore_ascii_case("EOF") {
        add(&mut parts, "EOF");
    }
    let raw = original.to_lowercase();
    if raw.contains("socks") && (raw.contains("authentication failed") || raw.contains("authentication required")) {
        add(&mut parts, "proxy_authentication_failed");
    }
    const SIGNALS: &[(&str, &str)] = &[
        ("socks", "proxy=socks"),
        ("proxyconnect", "proxy_connect_failed"),
        ("proxy connect", "proxy_connect_failed"),
        ("dial ", "dial_failed"),
        ("dial failed", "dial_failed"),
        ("connection refused", "connection_refused"),
        ("connection reset", "connection_reset"),
        ("connection aborted", "connection_aborted"),
        ("stream reset", "stream_reset"),
        ("network is unreachable", "network_unreachable"),
        ("no route to host", "network_unreachable"),
        ("no such host", "dns_not_found"),
        ("server misbehaving", "dns_failure"),
        ("tls handshake timeout", "tls_handshake_timeout"),
        ("i/o timeout", "timeout"),
        ("deadline exceeded", "timeout"),
        ("unexpected eof", "unexpected_EOF"),
        ("certificate", "tls_certificate_error"),
        ("invalid character", "invalid_response_json"),
        ("cannot unmarshal", "invalid_response_json"),
        ("invalid_grant", "oauth_error=invalid_grant"),
        ("refresh_token_expired", "oauth_error=refresh_token_expired"),
        ("refresh_token_revoked", "oauth_error=refresh_token_revoked"),
        ("refresh_token_reused", "oauth_error=refresh_token_reused"),
    ];
    for (needle, label) in SIGNALS {
        if raw.contains(needle) {
            add(&mut parts, label);
        }
    }
    if let Some(caps) = DIAGNOSTIC_STATUS.captures(&original) {
        add(&mut parts, &format!("status={}", &caps[1]));
    }
    if parts.is_empty() {
        add(&mut parts, "error_type=HomeError");
    }
    parts.join(" ")
}

/// The decode-failure signal of Go's `SafeErrorDiagnostic` for a JSON error: serde's text can quote
/// payload fragments, so only the fixed label is reported.
fn safe_json_diagnostic(_err: &serde_json::Error) -> &'static str {
    "invalid_response_json"
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go: TestSafeDiagnosticForLogPreservesAccessTokenExpiredAndRedactsCredentials.
    #[test]
    fn diagnostic_keeps_expiry_signal_and_redacts_credentials() {
        let diagnostic = "access token expired\naccess_token=access-secret refresh token: refresh-secret Authorization=Bearer bearer-secret \
            Post \"https://user:password@oauth.example/token?access_token=query-secret\" via socks5://proxy-user:proxy-password@127.0.0.1:1080";
        let got = safe_diagnostic_for_log(diagnostic);
        assert!(got.contains("access token expired"), "{got}");
        for secret in ["access-secret", "refresh-secret", "bearer-secret", "query-secret", "user:password", "proxy-user", "proxy-password"] {
            assert!(!got.contains(secret), "leaked {secret}: {got}");
        }
        assert!(!got.contains('\n') && got.contains("[REDACTED]"), "{got}");
        assert_eq!(safe_diagnostic_for_log("access token expired"), "access token expired");
    }

    // Go: TestSafeDiagnosticForLogBounds*.
    #[test]
    fn diagnostic_is_bounded_and_retains_a_trailing_expiry_signal() {
        let long = "upstream context ".repeat(1000) + "access token expired\nforged log line";
        let got = safe_diagnostic_for_log(&long);
        assert!(got.chars().count() <= DIAGNOSTIC_LOG_RUNE_LIMIT + 3);
        assert!(got.contains("access token expired") && got.ends_with("..."), "{got}");
        assert_eq!(safe_diagnostic_for_log(&"x".repeat(900)).chars().count(), DIAGNOSTIC_LOG_RUNE_LIMIT + 3);
    }

    #[test]
    fn error_diagnostic_never_carries_free_form_text() {
        let err = HomeError::Io("connection refused by 10.0.0.1 token=secret".into());
        assert_eq!(safe_error_diagnostic(&err), "connection_refused");
        assert_eq!(safe_error_diagnostic(&HomeError::Timeout), "timeout");
    }
}
