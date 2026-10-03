//! Credential refresh delegated to the Home control plane (Go: helps/home_refresh.go).
//!
//! With Home enabled, executors do not refresh locally: they ask Home for the refreshed
//! credential and map its answer (an updated auth, or a typed error) to an [`ExecError`].

use cpa_auth::{Auth, Status};
use cpa_config::Config;
use cpa_home::HomeError;
use cpa_runtime::executor::ExecError;
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
            tracing::debug!("Home refresh transport failed: {}", safe_error_diagnostic(&err));
            return Err(status_err(503, "home refresh temporarily unavailable"));
        }
    };

    if let Ok(ErrorEnvelope { error: Some(detail) }) = serde_json::from_slice::<ErrorEnvelope>(&raw) {
        return Err(home_error(detail));
    }

    let (mut updated, returned_index) = match parse_refresh_auth(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::debug!("Home refresh response decode failed: {err}");
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
        log_diagnostic(detail.diagnostic.as_deref(), || format!("Home refresh upstream response: status={status}"));
        // The relayed body doubles as the message, even when empty (no "status N" fallback).
        return ExecError::new(status, String::from_utf8_lossy(&body).into_owned()).with_body(body);
    }
    let status = status_from_home_error_code(code);
    let message = match status {
        401 => "credential unauthorized",
        404 => "credential refresh target not found",
        _ => "credential refresh temporarily unavailable",
    };
    log_diagnostic(detail.diagnostic.as_deref(), || {
        if code.is_empty() { message.to_string() } else { format!("Home refresh failed: type={}", code.to_lowercase()) }
    });
    status_err(status, message)
}

/// Home's diagnostic when it sent one, else `fallback()`, at debug level (Go carries it on the
/// error for the conductor's logs).
fn log_diagnostic(diagnostic: Option<&str>, fallback: impl FnOnce() -> String) {
    let text = diagnostic.map(str::trim).filter(|d| !d.is_empty()).map_or_else(fallback, str::to_string);
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

/// Failure class of a Home transport error, never its free-form text (Go: `SafeErrorDiagnostic`).
fn safe_error_diagnostic(err: &HomeError) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_redis_reply() {
        "home_error_reply"
    } else {
        "transport_error"
    }
}
