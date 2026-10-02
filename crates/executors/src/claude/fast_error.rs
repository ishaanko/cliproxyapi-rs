//! Fast-mode error handling (Go: claude_executor_fast_error.go).
//!
//! A `speed:"fast"` request failure is request scoped: it must not rotate credentials or cool the
//! selected credential, unless the failure is a genuine credential-level rate limit.

use cpa_runtime::executor::{ErrorCode, ExecError};
use http::HeaderMap;

use super::helps::ratelimit::{claude_headers_indicate_unified_rate_limit_rejection, parse_claude_rate_limit_reset};
use super::request::{claude_request_uses_fast_mode, claude_requested_betas};

/// Wraps an error from a fast request as request scoped (Go: wrapClaudeFastRequestError). The
/// status is the upstream status (0 for a 2xx or transport failure); a credential-scoped cause
/// stays credential scoped.
pub fn wrap_claude_fast_request_error(fast_request: bool, status: u16, err: ExecError) -> ExecError {
    if !fast_request {
        return err;
    }
    let mut wrapped = ExecError::new(if (200..300).contains(&status) { 0 } else { status }, err.message.clone());
    wrapped.retry_after = err.retry_after;
    wrapped.credential_scoped = err.credential_scoped;
    if !wrapped.credential_scoped {
        wrapped.code = Some(ErrorCode::RequestScoped);
    }
    wrapped
}

/// Passes an upstream error response through without retry or rebuilding
/// (Go: newClaudeFastDirectResponseError). `body` is already decoded, so representation headers
/// are dropped. The body doubles as the message so JSON bodies reach the client unchanged.
pub fn new_claude_fast_direct_response_error(status: u16, headers: &HeaderMap, body: &[u8]) -> ExecError {
    let mut out_headers = headers.clone();
    out_headers.remove("content-encoding");
    out_headers.remove("content-length");

    let mut retry_after = None;
    let mut credential_scoped = false;
    if status == 429 {
        retry_after = parse_claude_rate_limit_reset(headers, chrono::Utc::now());
        credential_scoped = claude_headers_indicate_unified_rate_limit_rejection(headers);
    }
    let message = if body.is_empty() {
        format!("claude Fast upstream request failed with status {status}")
    } else {
        String::from_utf8_lossy(body).into_owned()
    };
    let mut err = ExecError::new(status, message).with_body(bytes::Bytes::copy_from_slice(body));
    err.headers = out_headers;
    err.retry_after = retry_after;
    err.credential_scoped = credential_scoped;
    if !credential_scoped {
        err.code = Some(ErrorCode::RequestScoped);
    }
    err
}

/// Whether the outgoing request is a fast-mode request (Go: claudeRequestIsFast).
pub fn claude_request_is_fast(headers: &HeaderMap, body: &[u8]) -> bool {
    let betas = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    claude_request_uses_fast_mode(body, &claude_requested_betas(&betas, &[]))
}
