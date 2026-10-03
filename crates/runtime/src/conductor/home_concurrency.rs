//! Home concurrency tuples and dispatch error decoding (Go: `home_concurrency.go`).
//!
//! A dispatch response may carry `concurrency: {accounted, credential_id, model}`: Home counted
//! the request against the credential's limiter and expects a release when it ends. Anything
//! malformed is treated as an ambiguous response by the caller.

use std::time::Duration;

use cpa_home::executionregistry::{PendingDispatch, Registry, RegistryError, Scope, ScopeSpec};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;

use super::errors::auth_error;
use crate::executor::{ExecError, HomeErrKind};
use cpa_auth::Auth;

pub(crate) const MAX_TUPLE_FIELD_LENGTH: usize = 256;
pub(crate) const ASCII_WHITESPACE: &str = " \t\r\n\x0b\x0c";

pub(crate) const CODE_INVALID_HOME_CONCURRENCY: &str = "invalid_home_concurrency";

/// Go: `ErrMalformedHomeConcurrencyTuple`.
pub(crate) const MALFORMED_TUPLE: &str = "malformed Home concurrency tuple";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct ConcurrencyTuple {
    #[serde(default)]
    pub accounted: bool,
    #[serde(default)]
    pub credential_id: String,
    #[serde(default)]
    pub model: String,
}

fn trim_ws(s: &str) -> &str {
    s.trim_matches(|c| ASCII_WHITESPACE.contains(c))
}

/// Lowercased model without a recognized reasoning suffix, as Home's limiter keys it.
pub(crate) fn canonical_concurrency_model_key(model: &str) -> String {
    let trimmed = trim_ws(model).to_lowercase();
    if !trimmed.ends_with(')') {
        return trimmed;
    }
    let Some(open) = trimmed.rfind('(') else {
        return trimmed;
    };
    let suffix = &trimmed[open + 1..trimmed.len() - 1];
    if !recognized_concurrency_suffix(suffix) {
        return trimmed;
    }
    let base = trim_ws(&trimmed[..open]);
    if base.is_empty() {
        return trimmed;
    }
    base.to_string()
}

pub(crate) fn valid_canonical_concurrency_model_key(model: &str) -> (String, bool) {
    let key = canonical_concurrency_model_key(model);
    let valid = !key.is_empty() && key.len() <= MAX_TUPLE_FIELD_LENGTH;
    (key, valid)
}

pub(crate) fn recognized_concurrency_suffix(value: &str) -> bool {
    if value == "-1" {
        return true;
    }
    if matches!(
        value.to_lowercase().as_str(),
        "none" | "auto" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return true;
    }
    if value.is_empty() || value.len() > 10 {
        return false;
    }
    let mut parsed: i64 = 0;
    for b in value.bytes() {
        if !b.is_ascii_digit() {
            return false;
        }
        parsed = parsed * 10 + i64::from(b - b'0');
        if parsed > 2_147_483_647 {
            return false;
        }
    }
    true
}

fn valid_tuple_field(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && value.len() <= MAX_TUPLE_FIELD_LENGTH
}

/// Go: `validateAccountedHomeConcurrencyTuple`.
pub(crate) fn validate_accounted_tuple(tuple: &ConcurrencyTuple) -> Result<(), &'static str> {
    let (model, valid_model) = valid_canonical_concurrency_model_key(&tuple.model);
    if !tuple.accounted || !valid_tuple_field(&tuple.credential_id) || !valid_model || tuple.model != model {
        return Err(MALFORMED_TUPLE);
    }
    Ok(())
}

/// Builds an `ExecError` for a conductor-level Home failure (Go: `*Error`).
pub(crate) fn home_error(code: &str, message: &str, status: i32, retryable: bool) -> ExecError {
    let mut e = auth_error(code, message, status);
    e.retryable = retryable;
    e
}

pub(crate) fn invalid_home_concurrency_response(message: &str) -> ExecError {
    home_error(CODE_INVALID_HOME_CONCURRENCY, message, 502, false)
}

pub(crate) fn home_unavailable(message: &str, retryable: bool) -> ExecError {
    home_error("home_unavailable", message, 503, retryable)
}

/// Go: `NewHomeConcurrencyBusyError`: a trusted, Home-originated admission failure.
pub fn home_concurrency_busy_error(message: &str, retry_after: Duration) -> ExecError {
    let message = message.trim();
    let message = if message.is_empty() { "credential concurrency limit exceeded" } else { message };
    busy_error(home_error("credential_concurrency_exceeded", message, 429, true), retry_after)
}

fn busy_error(mut cause: ExecError, retry_after: Duration) -> ExecError {
    cause.home = Some(HomeErrKind::ConcurrencyBusy);
    if !retry_after.is_zero() {
        cause.retry_after = Some(retry_after);
        cause.headers = safe_retry_after_header(retry_after);
    }
    cause
}

/// `Retry-After` in whole seconds, rounded up to at least one; empty for non-positive hints.
pub(crate) fn safe_retry_after_header(retry_after: Duration) -> HeaderMap {
    let mut h = HeaderMap::new();
    if retry_after.is_zero() {
        return h;
    }
    let mut secs = retry_after.as_secs();
    if retry_after.subsec_nanos() != 0 {
        secs += 1;
    }
    let secs = secs.max(1);
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        h.insert(http::header::RETRY_AFTER, v);
    }
    h
}

/// Result of probing a dispatch response for the `concurrency` field.
pub(crate) struct Envelope {
    pub tuple: ConcurrencyTuple,
    pub present: bool,
}

/// Go: `decodeHomeDispatchConcurrencyEnvelope`. The error string is only for diagnostics; the
/// caller distinguishes by `present`.
pub(crate) fn decode_concurrency_envelope(raw: &[u8]) -> (Envelope, Result<(), String>) {
    let mut envelope = Envelope { tuple: ConcurrencyTuple::default(), present: false };
    if std::str::from_utf8(raw).is_err() {
        return (envelope, Err("Home response is not valid UTF-8".into()));
    }
    let fields: Value = match serde_json::from_slice(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => return (envelope, Err("Home response is not a JSON object".into())),
    };
    let Some(raw_tuple) = fields.get("concurrency") else {
        return (envelope, Ok(()));
    };
    envelope.present = true;
    match serde_json::from_value::<ConcurrencyTuple>(raw_tuple.clone()) {
        Ok(t) => envelope.tuple = t,
        Err(e) => return (envelope, Err(e.to_string())),
    }
    if let Err(e) = validate_accounted_tuple(&envelope.tuple) {
        return (envelope, Err(e.into()));
    }
    (envelope, Ok(()))
}

pub(crate) fn canonical_dispatch_model(response_model: &str, requested_model: &str) -> String {
    let model = response_model.trim();
    if model.is_empty() { requested_model.to_string() } else { model.to_string() }
}

#[derive(Deserialize, Default)]
struct ErrorDetail {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    retry_after_ms: i64,
    #[serde(default)]
    request_retry: Option<i64>,
}

/// Go: `decodeHomeDispatchError`. `None` when the payload is not an error envelope.
pub(crate) fn decode_dispatch_error(raw: &[u8]) -> Option<ExecError> {
    let fields: Value = match serde_json::from_slice(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => return None,
    };
    let raw_error = fields.get("error")?;
    let malformed = || home_error("invalid_auth", "home returned malformed error payload", 502, false);
    let detail = match serde_json::from_value::<Option<ErrorDetail>>(raw_error.clone()) {
        Ok(Some(d)) => d,
        _ => return Some(malformed()),
    };
    let mut code = detail.kind.trim().to_string();
    if code.is_empty() {
        code = detail.code.trim().to_string();
    }
    if code.is_empty() {
        return Some(malformed());
    }
    let message = detail.message.trim();
    let message = if message.is_empty() { "home returned error" } else { message };
    let mut result = home_error(&code, message, 502, detail.retryable);
    match code.to_lowercase().as_str() {
        "model_not_found" => result.status = 404,
        "model_cooldown" => {
            result.status = 429;
            let mut request_retry = None;
            if let Some(rr) = detail.request_retry
                && rr >= 0
            {
                request_retry = Some(rr);
            }
            result.home = Some(HomeErrKind::DispatchRetryAfter { request_retry });
            if detail.retry_after_ms > 0 {
                result.retry_after = Some(Duration::from_millis(detail.retry_after_ms as u64));
            }
            return Some(result);
        }
        "authentication_error" | "unauthorized" | "no_credentials" | "invalid_credential" => result.status = 401,
        "credential_concurrency_exceeded" | "credential_model_concurrency_exceeded" => {
            result.status = 429;
            let retry = Duration::from_millis(detail.retry_after_ms.max(0) as u64);
            return Some(busy_error(result, retry));
        }
        "user_credits_insufficient" => result.status = 402,
        "user_period_limit_exceeded" => result.status = 429,
        "auth_not_found" | "auth_unavailable" | "refresh_temporarily_unavailable" | "home_unavailable"
        | "concurrency_protocol_required" | "concurrency_tracker_unavailable" | "concurrency_node_unavailable" => {
            result.status = 503;
        }
        _ => {}
    }
    Some(result)
}

/// Go: `verifyAccountedHomeConcurrencyIdentity`.
pub(crate) fn verify_accounted_identity(tuple: &ConcurrencyTuple, auth: &Auth, auth_index: &str) -> Result<(), ExecError> {
    if !tuple.accounted {
        return Ok(());
    }
    if auth.id != tuple.credential_id || auth_index != tuple.credential_id {
        return Err(invalid_home_concurrency_response("Home concurrency identity does not match dispatched auth"));
    }
    Ok(())
}

/// Go: `installHomeConcurrencyScope`.
pub(crate) fn install_concurrency_scope(
    registry: &Registry,
    pending: &PendingDispatch,
    tuple: &ConcurrencyTuple,
    mut base: ScopeSpec,
) -> Result<Scope, ExecError> {
    if !tuple.accounted {
        base.accounted = false;
        return registry.install(pending, base).map_err(install_error);
    }
    if let Err(e) = validate_accounted_tuple(tuple) {
        return Err(invalid_home_concurrency_response(e));
    }
    base.credential_id = tuple.credential_id.clone();
    base.model = tuple.model.clone();
    base.accounted = true;
    registry.install(pending, base).map_err(install_error)
}

/// Go: `homeConcurrencyInstallError`.
pub(crate) fn install_error(err: RegistryError) -> ExecError {
    home_unavailable(&format!("home execution registry unavailable: {err}"), true)
}

/// Retry hint of a failed attempt, as the HTTP layer relays it (empty unless a concrete
/// Home-originated retry/cooldown error). Go: `SafeResponseHeaders` Home cases.
pub fn home_safe_response_headers(err: &ExecError) -> Option<HeaderMap> {
    match &err.home {
        Some(HomeErrKind::ConcurrencyBusy | HomeErrKind::DispatchRetryAfter { .. }) => {
            Some(err.retry_after.map(safe_retry_after_header).unwrap_or_default())
        }
        Some(HomeErrKind::RetryRoundExhausted { .. }) => {
            Some(err.retry_after.map(safe_retry_after_header).unwrap_or_default())
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_keys_drop_recognized_suffixes_only() {
        assert_eq!(canonical_concurrency_model_key(" GPT-5(high) "), "gpt-5");
        assert_eq!(canonical_concurrency_model_key("gpt-5(8192)"), "gpt-5");
        assert_eq!(canonical_concurrency_model_key("gpt-5(-1)"), "gpt-5");
        assert_eq!(canonical_concurrency_model_key("gpt-5(weird)"), "gpt-5(weird)");
        assert_eq!(canonical_concurrency_model_key("(high)"), "(high)");
        assert!(!recognized_concurrency_suffix("99999999999"));
        assert!(!recognized_concurrency_suffix(""));
        assert!(valid_canonical_concurrency_model_key("").1 == false);
    }

    #[test]
    fn tuples_must_be_accounted_canonical_and_trimmed() {
        let ok = ConcurrencyTuple { accounted: true, credential_id: "a".into(), model: "gpt-5".into() };
        assert!(validate_accounted_tuple(&ok).is_ok());
        for bad in [
            ConcurrencyTuple { accounted: false, ..ok.clone() },
            ConcurrencyTuple { credential_id: " a".into(), ..ok.clone() },
            ConcurrencyTuple { credential_id: String::new(), ..ok.clone() },
            ConcurrencyTuple { model: "GPT-5".into(), ..ok.clone() },
            ConcurrencyTuple { model: "gpt-5(high)".into(), ..ok.clone() },
            ConcurrencyTuple { model: "x".repeat(257), ..ok.clone() },
        ] {
            assert!(validate_accounted_tuple(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn envelope_probe_distinguishes_absent_malformed_and_valid() {
        let (e, r) = decode_concurrency_envelope(br#"{"auth":{"id":"a"}}"#);
        assert!(!e.present && r.is_ok());
        let (e, r) = decode_concurrency_envelope(br#"{"concurrency":{"accounted":true,"credential_id":"a","model":"m"}}"#);
        assert!(e.present && r.is_ok() && e.tuple.credential_id == "a");
        let (e, r) = decode_concurrency_envelope(br#"{"concurrency":{"accounted":false}}"#);
        assert!(e.present && r.is_err());
        let (e, r) = decode_concurrency_envelope(b"[1]");
        assert!(!e.present && r.is_err());
        let (e, r) = decode_concurrency_envelope(&[0xff, 0xfe]);
        assert!(!e.present && r.is_err());
    }

    #[test]
    fn dispatch_errors_map_codes_to_status_and_home_classes() {
        let e = |json: &str| decode_dispatch_error(json.as_bytes()).unwrap();
        assert_eq!(e(r#"{"error":{"type":"model_not_found","message":"nope"}}"#).status, 404);
        let cd = e(r#"{"error":{"type":"model_cooldown","message":"cooling","retry_after_ms":1500,"request_retry":2}}"#);
        assert_eq!((cd.status, cd.retry_after), (429, Some(Duration::from_millis(1500))));
        assert_eq!(cd.home, Some(HomeErrKind::DispatchRetryAfter { request_retry: Some(2) }));
        let busy = e(r#"{"error":{"code":"credential_concurrency_exceeded","message":"busy","retry_after_ms":250}}"#);
        assert_eq!((busy.status, busy.home.clone()), (429, Some(HomeErrKind::ConcurrencyBusy)));
        assert_eq!(busy.headers.get("retry-after").unwrap(), "1");
        assert_eq!(e(r#"{"error":{"type":"unauthorized"}}"#).status, 401);
        assert_eq!(e(r#"{"error":{"type":"user_credits_insufficient"}}"#).status, 402);
        assert_eq!(e(r#"{"error":{"type":"auth_unavailable"}}"#).status, 503);
        assert_eq!(e(r#"{"error":{"type":"something_else","message":"m"}}"#).status, 502);
        assert_eq!(e(r#"{"error":{"message":"m"}}"#).message, "invalid_auth: home returned malformed error payload");
        assert_eq!(e(r#"{"error":null}"#).auth_code.as_deref(), Some("invalid_auth"));
        assert!(decode_dispatch_error(br#"{"auth":{}}"#).is_none());
        assert!(decode_dispatch_error(b"nope").is_none());
    }

    #[test]
    fn retry_after_header_rounds_up_to_whole_seconds() {
        assert_eq!(safe_retry_after_header(Duration::from_millis(1)).get("retry-after").unwrap(), "1");
        assert_eq!(safe_retry_after_header(Duration::from_millis(2001)).get("retry-after").unwrap(), "3");
        assert!(safe_retry_after_header(Duration::ZERO).is_empty());
    }
}
