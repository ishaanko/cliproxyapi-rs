//! Meta upstream error classification (Go: meta_executor.go `wrapMetaUpstreamError`,
//! `parseMetaRetryAfter`, `isMetaSubscriptionQuota`, `metaStreamEventError`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpa_json::J;
use cpa_runtime::executor::ExecError;
use serde_json::Value;

use crate::helps::status::status_err;

/// Cooldown for 404 responses that carry no reset time.
pub const NOT_FOUND_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Time until `error.resets_at` (unix seconds) for 429/404 bodies; `None` when absent or past.
pub fn parse_meta_retry_after(status: u16, body: &[u8], now: SystemTime) -> Option<Duration> {
    if !matches!(status, 429 | 404) || body.is_empty() {
        return None;
    }
    let resets_at = cpa_json::parse(body).g("error.resets_at").int();
    if resets_at <= 0 {
        return None;
    }
    let reset = UNIX_EPOCH + Duration::from_secs(resets_at as u64);
    reset.duration_since(now).ok().filter(|d| !d.is_zero())
}

/// A 429 that reports an exhausted subscription quota (credential-wide, not request-specific).
pub fn is_meta_subscription_quota(status: u16, body: &[u8]) -> bool {
    if status != 429 || body.is_empty() {
        return false;
    }
    let root = cpa_json::parse(body);
    let msg = root.g("error.message").str().to_lowercase();
    let code = root.g("error.code").str().to_lowercase();
    if msg.contains("subscription quota") || msg.contains("quota exhausted") {
        return true;
    }
    (code == "rate_limit_exceeded" || code.contains("quota")) && root.g("error.resets_at").exists()
}

/// Upstream failure as `statusErr{code, msg: body}` with Meta's retry hints: 429 resets,
/// credential scope for subscription quota, and a fixed cooldown for 404.
pub fn wrap_meta_upstream_error(status: u16, body: &[u8]) -> ExecError {
    let mut err = status_err(status, String::from_utf8_lossy(body).into_owned());
    if status == 429 {
        err.retry_after = parse_meta_retry_after(status, body, SystemTime::now());
        if is_meta_subscription_quota(status, body) {
            err.credential_scoped = true;
        }
    }
    if status == 404 {
        err.retry_after = Some(parse_meta_retry_after(status, body, SystemTime::now()).unwrap_or(NOT_FOUND_COOLDOWN));
    }
    err
}

/// The error behind an `error` / `response.failed` stream event: status from `error.code` when
/// 400..=599, else 502.
pub fn meta_stream_event_error(event: &[u8]) -> Option<ExecError> {
    let root = cpa_json::parse(event);
    let kind = root.g("type").str();
    if kind != "error" && kind != "response.failed" {
        return None;
    }
    let code = root.g("error.code").int();
    let status = if (400..=599).contains(&code) { code as u16 } else { 502 };
    Some(wrap_meta_upstream_error(status, event))
}

/// A complete `response.completed` / `response.incomplete` event for a non-stream reply: the
/// body itself when it is one, or a plain response object wrapped as `response.completed`.
pub fn meta_as_completed_event(data: &[u8]) -> Option<Vec<u8>> {
    let trimmed = crate::helps::text::trim_space(data);
    if !cpa_json::valid(trimmed) {
        return None;
    }
    let root = cpa_json::parse(trimmed);
    if matches!(root.g("type").str().as_str(), "response.completed" | "response.incomplete") {
        return Some(trimmed.to_vec());
    }
    if root.g("object").str() == "response" || root.g("output").exists() {
        let mut wrapped = cpa_json::parse_str(r#"{"type":"response.completed"}"#);
        cpa_json::set(&mut wrapped, "response", root as Value);
        return Some(cpa_json::to_vec(&wrapped));
    }
    None
}
