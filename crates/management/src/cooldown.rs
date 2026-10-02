//! Cooldown projection for the credentials list and cooldown reset (Go:
//! `sdk/cliproxy/auth/cooldown_view.go`, `auth_files.go: reconcileAuthFileCooldownState`,
//! `conductor_cooldown.go: ResetQuota`). Works on `Auth` snapshots only.

use chrono::{DateTime, Utc};
use cpa_auth::types::{AuthError, ModelState, QuotaState};
use cpa_auth::{Auth, Status};
use serde::Serialize;

use crate::http::rfc3339;

type Time = Option<DateTime<Utc>>;

fn after(t: Time, now: DateTime<Utc>) -> bool {
    t.is_some_and(|t| t > now)
}

/// An unexpired local retry restriction.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CooldownView {
    pub scope: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model_key: String,
    pub reason: String,
    pub retry_at: String,
    pub remaining_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_level: Option<i32>,
    #[serde(skip_serializing_if = "is_zero_u16")]
    pub http_status: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockReason {
    Cooldown,
    Other,
}

/// `availabilityBlock`: whether an unavailable/quota-exceeded state still blocks at `now`, why,
/// and until when.
fn availability_block(
    unavailable: bool,
    quota_exceeded: bool,
    next_retry_after: Time,
    next_recover_at: Time,
    now: DateTime<Utc>,
) -> Option<(BlockReason, Time)> {
    if !unavailable && !quota_exceeded {
        return None;
    }
    let has_recovery_time = next_retry_after.is_some() || next_recover_at.is_some();
    let next = [next_retry_after, next_recover_at]
        .into_iter()
        .flatten()
        .filter(|c| *c > now)
        .max();
    if next.is_some() {
        let reason = if quota_exceeded {
            BlockReason::Cooldown
        } else {
            BlockReason::Other
        };
        return Some((reason, next));
    }
    if has_recovery_time {
        None
    } else {
        Some((BlockReason::Other, None))
    }
}

/// `canonicalModelKey`: the model name without a `(thinking)` suffix.
fn canonical_model_key(model: &str) -> String {
    let model = model.trim();
    if let Some(stripped) = model.strip_suffix(')')
        && let Some(open) = stripped.rfind('(')
    {
        let name = stripped[..open].trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    model.to_string()
}

fn normalize_identifier(value: &str) -> String {
    value.trim().to_lowercase().replace(['-', ' '], "_")
}

/// `isModelNotFoundIdentifier`: a code or type such as `model_not_found`, optionally as the last
/// segment of a URI or fragment.
fn is_model_not_found_identifier(value: &str) -> bool {
    let mut candidate = value.trim().to_lowercase();
    match candidate.rfind('#').filter(|i| i + 1 < candidate.len()) {
        Some(i) => candidate = candidate[i + 1..].to_string(),
        None => {
            if let Some(q) = candidate.find('?') {
                candidate.truncate(q);
            }
            let trimmed = candidate.trim_end_matches('/');
            candidate = match trimmed.rfind(['/', ':']) {
                Some(i) => trimmed[i + 1..].to_string(),
                None => trimmed.to_string(),
            };
        }
    }
    matches!(
        normalize_identifier(&candidate).as_str(),
        "model_not_found"
            | "model_not_found_error"
            | "unknown_model"
            | "model_does_not_exist"
            | "model_not_exist"
    )
}

fn is_missing_model_phrase(value: &str) -> bool {
    matches!(
        value.trim_matches(|c| " .!;\t\r\n".contains(c)),
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

/// `isExplicitModelNotFoundMessage` without a requested model (the cooldown view has none).
fn is_explicit_model_not_found_message(message: &str) -> bool {
    let lower = message.trim().to_lowercase();
    let lower = lower.trim_matches(|c| " .!;\t\r\n".contains(c));
    if lower.is_empty()
        || lower.contains("in request")
        || lower.contains("in body")
        || lower.contains("request body")
    {
        return false;
    }
    let normalized = lower.replace('-', "_");
    if normalized.contains("model_not_found") || normalized.contains("unknown_model") {
        return true;
    }
    let remainder_after = |prefix: &str| -> Option<String> {
        if lower != prefix
            && !lower.starts_with(&format!("{prefix} "))
            && !lower.starts_with(&format!("{prefix}:"))
        {
            return None;
        }
        let rest = lower[prefix.len()..].trim();
        Some(rest.strip_prefix(':').unwrap_or(rest).trim().to_string())
    };
    for prefix in ["no such model", "unknown model"] {
        if let Some(rest) = remainder_after(prefix) {
            return rest.is_empty();
        }
    }
    for prefix in [
        "the requested model",
        "requested model",
        "the model",
        "model",
    ] {
        if let Some(rest) = remainder_after(prefix) {
            return is_missing_model_phrase(&rest);
        }
    }
    false
}

/// `containsStructuredModelNotFound` with no requested model, so the "type not_found plus an
/// exact model reference" rule can never fire.
fn contains_structured_model_not_found(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Object(map) => map.iter().any(|(key, item)| {
            if let Value::String(text) = item {
                match key.trim().to_lowercase().as_str() {
                    "code" | "type" if is_model_not_found_identifier(text) => return true,
                    "error" | "message" | "detail" | "error_description" | "title"
                        if is_explicit_model_not_found_message(text) =>
                    {
                        return true;
                    }
                    _ => {}
                }
            }
            matches!(item, Value::Object(_) | Value::Array(_))
                && contains_structured_model_not_found(item)
        }),
        Value::Array(items) => items.iter().any(|item| {
            matches!(item, Value::String(t) if is_explicit_model_not_found_message(t))
                || contains_structured_model_not_found(item)
        }),
        _ => false,
    }
}

fn is_structured_model_not_found(message: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(message.trim())
        .is_ok_and(|v| contains_structured_model_not_found(&v))
}

/// `isExplicitModelNotFoundError`: the code, or a JSON error body in the message (also checked
/// as the `code: message` rendering of the error).
fn is_model_not_found(err: &AuthError) -> bool {
    if is_model_not_found_identifier(&err.code) || is_structured_model_not_found(&err.message) {
        return true;
    }
    let rendered = if err.code.is_empty() {
        err.message.clone()
    } else {
        format!("{}: {}", err.code, err.message)
    };
    is_structured_model_not_found(&rendered)
}

fn is_model_support_error(err: &AuthError) -> bool {
    if is_model_not_found(err) {
        return true;
    }
    if !matches!(err.http_status, 400 | 404 | 422) {
        return false;
    }
    let lower = err.message.trim().to_lowercase();
    [
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
    ]
    .iter()
    .any(|p| lower.contains(p))
}

fn is_cloudflare_challenge(err: &AuthError) -> bool {
    if err.http_status >= 500 {
        return false;
    }
    let lower = err.message.trim().to_lowercase();
    lower.contains("challenge-platform")
        || lower.contains("cf-mitigated")
        || lower.contains("cloudflare challenge")
        || (lower.contains("just a moment") && lower.contains("cloudflare"))
}

fn is_invalid_grant(err: &AuthError) -> bool {
    let has = |s: &str| s.to_lowercase().contains("invalid_grant");
    (has(&err.code) || has(&err.message)) && matches!(err.http_status, 400 | 401 | 0)
}

fn status_reason(message: &str) -> &'static str {
    match message.trim() {
        "quota" | "quota exhausted" => "quota",
        "cloudflare challenge" => "cloudflare_challenge",
        "invalid_grant" => "invalid_grant",
        "unauthorized" => "unauthorized",
        "payment_required" => "payment_required",
        "not_found" => "not_found",
        "model_not_supported" => "model_not_supported",
        "transient upstream error" => "transient_error",
        _ => "unknown",
    }
}

fn error_reason(err: Option<&AuthError>) -> &'static str {
    let Some(err) = err else { return "unknown" };
    if is_model_support_error(err) {
        return "model_not_supported";
    }
    if is_cloudflare_challenge(err) {
        return "cloudflare_challenge";
    }
    if is_invalid_grant(err) {
        return "invalid_grant";
    }
    match err.http_status {
        401 => return "unauthorized",
        402 | 403 => return "payment_required",
        404 => return "not_found",
        429 => return "quota",
        408 | 500 | 502 | 503 | 504 | 520..=526 => return "transient_error",
        _ => {}
    }
    status_reason(&err.code)
}

fn new_view(
    scope: &'static str,
    model: &str,
    next: DateTime<Utc>,
    now: DateTime<Utc>,
    quota: &QuotaState,
    status_message: &str,
    last_err: Option<&AuthError>,
) -> CooldownView {
    let remaining_ms = (next - now).num_milliseconds();
    let remaining_seconds =
        remaining_ms.div_euclid(1000) + i64::from(remaining_ms.rem_euclid(1000) != 0);
    let mut view = CooldownView {
        scope,
        model_key: model.to_string(),
        reason: "unknown".into(),
        retry_at: rfc3339(next),
        remaining_seconds,
        backoff_level: None,
        http_status: 0,
    };
    // A shorter quota window must not label a longer non-quota retry timer.
    if quota.exceeded && quota.next_recover_at.is_none_or(|r| r >= next) {
        match quota.reason.as_str() {
            "credential_quota" | "quota" => view.reason = quota.reason.clone(),
            "cloudflare challenge" => view.reason = "cloudflare_challenge".into(),
            _ => {}
        }
    }
    let propagated_quota = quota.exceeded && quota.reason == "credential_quota";
    if view.reason == "credential_quota" {
        return view;
    }
    if (view.reason == "quota" || view.reason == "cloudflare_challenge") && quota.backoff_level >= 0
    {
        view.backoff_level = Some(quota.backoff_level);
    }
    let error_reason = error_reason(last_err);
    if view.reason == "unknown" {
        view.reason = error_reason.into();
    }
    if view.reason == "unknown" {
        view.reason = status_reason(status_message).into();
    }
    if !propagated_quota
        && let Some(e) = last_err
        && (400..=599).contains(&e.http_status)
        && error_reason == view.reason
    {
        view.http_status = e.http_status as u16;
    }
    view
}

/// `CooldownSnapshotForAuth`: unexpired credential-wide and per-model retry timers.
pub(crate) fn cooldown_snapshot(auth: &Auth, now: DateTime<Utc>) -> Vec<CooldownView> {
    let mut views = Vec::new();
    // The explicit credential-wide gate; other auth-level fields can be model aggregates.
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && after(auth.quota.next_recover_at, now)
    {
        if let Some(next) = auth.quota.next_recover_at {
            views.push(new_view(
                "credential",
                "",
                next,
                now,
                &auth.quota,
                &auth.status_message,
                auth.last_error.as_ref(),
            ));
        }
    } else if auth.model_states.is_empty()
        && let Some((_, Some(next))) = availability_block(
            auth.unavailable,
            auth.quota.exceeded,
            auth.next_retry_after,
            auth.quota.next_recover_at,
            now,
        )
    {
        views.push(new_view(
            "credential",
            "",
            next,
            now,
            &auth.quota,
            &auth.status_message,
            auth.last_error.as_ref(),
        ));
    }

    // BTreeMap iteration is sorted by key, matching Go's sorted source keys.
    let mut by_model: std::collections::BTreeMap<String, (CooldownView, BlockReason)> =
        Default::default();
    for (key, state) in &auth.model_states {
        let model = canonical_model_key(key);
        if model.is_empty() {
            continue;
        }
        let Some((reason, Some(next))) = availability_block(
            state.unavailable,
            state.quota.exceeded,
            state.next_retry_after,
            state.quota.next_recover_at,
            now,
        ) else {
            continue;
        };
        if let Some((prev, prev_reason)) = by_model.get(&model) {
            let prev_at = DateTime::parse_from_rfc3339(&prev.retry_at)
                .map(|t| t.with_timezone(&Utc))
                .ok();
            let prefer_quota_tie = Some(next) == prev_at
                && reason == BlockReason::Cooldown
                && *prev_reason != BlockReason::Cooldown;
            if prev_at.is_some_and(|p| next <= p) && !prefer_quota_tie {
                continue;
            }
        }
        let view = new_view(
            "model",
            &model,
            next,
            now,
            &state.quota,
            &state.status_message,
            state.last_error.as_ref(),
        );
        by_model.insert(model, (view, reason));
    }
    views.extend(by_model.into_values().map(|(v, _)| v));
    views
}

fn is_persistent_auth_failure(auth: &Auth, now: DateTime<Utc>) -> bool {
    // Terminal unauthorized failure with no refresh scheduled.
    if auth.unavailable
        && auth.status == Status::Error
        && auth.next_refresh_after.is_none()
        && auth
            .last_error
            .as_ref()
            .is_some_and(|e| e.http_status == 401 || e.code.eq_ignore_ascii_case("unauthorized"))
    {
        return true;
    }
    // An OAuth credential whose access token is expired cannot serve requests.
    if auth
        .access_token_expiration_time()
        .is_some_and(|exp| exp <= now)
    {
        return true;
    }
    auth.status_message
        .trim()
        .eq_ignore_ascii_case("token expired")
}

fn is_model_state_blocked(state: &ModelState, now: DateTime<Utc>) -> bool {
    if state.status == Status::Disabled {
        return true;
    }
    if !state.unavailable && !state.quota.exceeded {
        return false;
    }
    let has_recovery_time = state.next_retry_after.is_some()
        || (state.quota.next_recover_at.is_some() && state.quota.exceeded);
    if after(state.next_retry_after, now) {
        return true;
    }
    if state.quota.exceeded && after(state.quota.next_recover_at, now) {
        return true;
    }
    !has_recovery_time
}

/// Outcome of [`reconcile_cooldown_state`].
pub(crate) struct Reconciled {
    pub unavailable: bool,
    pub status: Status,
    pub status_message: String,
    pub next_retry_after: Time,
}

/// `reconcileAuthFileCooldownState`: what the list shows for status, availability and next retry,
/// correcting stale states whose cooldown already expired.
pub(crate) fn reconcile_cooldown_state(auth: &Auth, now: DateTime<Utc>) -> Reconciled {
    let mut unavailable = auth.unavailable;
    let mut status = auth.status;
    let mut status_message = auth.status_message.clone();
    let mut next_retry = auth.next_retry_after;
    let done = |unavailable, status, status_message, next_retry| Reconciled {
        unavailable,
        status,
        status_message,
        next_retry_after: next_retry,
    };
    let drop_past = |t: Time| t.filter(|t| *t > now);

    if auth.disabled || auth.status == Status::Disabled {
        return done(unavailable, Status::Disabled, status_message, next_retry);
    }
    // Never reconcile an active authentication or token failure to active.
    if is_persistent_auth_failure(auth, now) {
        return done(true, Status::Error, status_message, drop_past(next_retry));
    }

    let mut has_active_cred_cooldown = false;
    if auth.unavailable || auth.quota.exceeded {
        if after(auth.next_retry_after, now) {
            has_active_cred_cooldown = true;
        }
        if auth.quota.exceeded
            && auth.quota.reason == "credential_quota"
            && after(auth.quota.next_recover_at, now)
        {
            has_active_cred_cooldown = true;
            if let Some(recover) = auth.quota.next_recover_at
                && next_retry.is_none_or(|n| recover > n)
            {
                next_retry = Some(recover);
            }
        }
    }

    let mut has_schedulable_models = false;
    let mut all_schedulable_blocked = true;
    let mut has_active_model_cooldown = false;
    let mut had_any_model_cooldown = false;
    for state in auth.model_states.values() {
        if state.status == Status::Disabled {
            continue;
        }
        has_schedulable_models = true;
        if state.next_retry_after.is_some()
            || (state.quota.exceeded && state.quota.next_recover_at.is_some())
        {
            had_any_model_cooldown = true;
        }
        if after(state.next_retry_after, now)
            || (state.quota.exceeded && after(state.quota.next_recover_at, now))
        {
            has_active_model_cooldown = true;
        }
        if !is_model_state_blocked(state, now) {
            all_schedulable_blocked = false;
        }
    }
    let had_cooldown = auth.next_retry_after.is_some()
        || (auth.quota.exceeded && auth.quota.next_recover_at.is_some())
        || had_any_model_cooldown;

    if has_active_cred_cooldown
        || (has_schedulable_models && all_schedulable_blocked && auth.unavailable)
    {
        return done(true, Status::Error, status_message, drop_past(next_retry));
    }
    if !auth.unavailable && !has_active_cred_cooldown {
        if status == Status::Error && has_schedulable_models && !all_schedulable_blocked {
            status = Status::Active;
            status_message.clear();
        }
        return done(false, status, status_message, None);
    }
    if had_cooldown && !has_active_cred_cooldown && !has_active_model_cooldown {
        return done(false, Status::Active, String::new(), None);
    }
    if had_cooldown && has_schedulable_models && !all_schedulable_blocked {
        if status == Status::Error && !has_active_cred_cooldown {
            status = Status::Active;
            status_message.clear();
        }
        return done(false, status, status_message, None);
    }
    unavailable = auth.unavailable;
    done(unavailable, status, status_message, drop_past(next_retry))
}

fn clear_quota_cooldown(quota: &mut QuotaState) {
    quota.exceeded = false;
    quota.reason.clear();
    quota.next_recover_at = None;
    quota.backoff_level = 0;
}

fn has_model_error(auth: &Auth, now: DateTime<Utc>) -> bool {
    auth.model_states.values().any(|s| {
        s.last_error.is_some()
            || (s.status == Status::Error
                && s.unavailable
                && s.next_retry_after.is_none_or(|t| t > now))
    })
}

/// `ResetQuota` on a snapshot: clears credential and per-model cooldown state. Returns the model
/// keys that were reset.
pub(crate) fn reset_cooldowns(auth: &mut Auth, now: DateTime<Utc>) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for (key, state) in auth.model_states.iter_mut() {
        if key.trim().is_empty() {
            continue;
        }
        models.push(key.clone());
        state.unavailable = false;
        state.status = Status::Active;
        state.status_message.clear();
        state.next_retry_after = None;
        state.last_error = None;
        clear_quota_cooldown(&mut state.quota);
        state.updated_at = Some(now);
    }
    if auth.unavailable
        || auth.next_retry_after.is_some()
        || auth.quota.exceeded
        || auth.quota.next_recover_at.is_some()
    {
        auth.unavailable = false;
        auth.next_retry_after = None;
        clear_quota_cooldown(&mut auth.quota);
    }
    if !auth.disabled && auth.status != Status::Disabled && !has_model_error(auth, now) {
        auth.last_error = None;
        auth.status_message.clear();
        auth.status = Status::Active;
    }
    auth.generation += 1;
    auth.updated_at = Some(now);
    models.sort();
    models.dedup();
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn model_cooldown_reports_remaining_seconds_rounded_up_and_reset_clears_it() {
        let now = Utc::now();
        let mut a = Auth::new("a.json", "claude");
        a.model_states.insert(
            "claude-sonnet(8192)".into(),
            ModelState {
                unavailable: true,
                next_retry_after: Some(now + Duration::milliseconds(1500)),
                last_error: Some(AuthError {
                    http_status: 429,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        a.unavailable = true;
        let views = cooldown_snapshot(&a, now);
        assert_eq!(views.len(), 1);
        assert_eq!(
            (
                views[0].scope,
                views[0].model_key.as_str(),
                views[0].remaining_seconds
            ),
            ("model", "claude-sonnet", 2)
        );
        assert_eq!(
            (views[0].reason.as_str(), views[0].http_status),
            ("quota", 429)
        );

        let r = reconcile_cooldown_state(&a, now);
        assert!(r.unavailable && r.status == Status::Error);

        let models = reset_cooldowns(&mut a, now);
        assert_eq!(models, vec!["claude-sonnet(8192)".to_string()]);
        assert!(
            cooldown_snapshot(&a, now).is_empty() && !a.unavailable && a.status == Status::Active
        );
    }

    #[test]
    fn model_not_found_follows_the_structured_go_rules() {
        let err = |code: &str, message: &str| AuthError {
            code: code.into(),
            message: message.into(),
            ..Default::default()
        };
        assert!(is_model_not_found(&err("model_not_found", "")));
        assert!(is_model_not_found(&err(
            "https://x/errors#Model-Not-Found",
            ""
        )));
        assert!(is_model_not_found(&err(
            "",
            r#"{"error":{"code":"model_not_found"}}"#
        )));
        assert!(is_model_not_found(&err(
            "",
            r#"{"error":{"message":"The model does not exist."}}"#
        )));
        assert!(is_model_not_found(&err(
            "x",
            r#"{"message":"Unknown model"}"#
        )));
        // Plain text and request-body complaints are not model-not-found.
        assert!(!is_model_not_found(&err("", "model_not_found")));
        assert!(!is_model_not_found(&err(
            "",
            r#"{"error":"model not found in request body"}"#
        )));
        assert!(!is_model_not_found(&err(
            "",
            r#"{"error":"the model gpt-5 does not exist"}"#
        )));
    }

    #[test]
    fn expired_cooldown_reconciles_to_active() {
        let now = Utc::now();
        let mut a = Auth::new("a.json", "codex");
        a.unavailable = true;
        a.status = Status::Error;
        a.next_retry_after = Some(now - Duration::seconds(5));
        let r = reconcile_cooldown_state(&a, now);
        assert!(!r.unavailable && r.status == Status::Active && r.next_retry_after.is_none());
    }
}
