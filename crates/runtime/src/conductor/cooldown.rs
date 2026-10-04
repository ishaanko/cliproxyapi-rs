//! Cooldown and availability state machine (Go: conductor_cooldown.go MarkResult math,
//! selector.go availability, quota_signals.go, cooldown_view.go).
//!
//! Everything here is pure over `Auth`/`ModelState` plus an explicit `now`, so the ladder and
//! never-shorten rules can be tested without executors. Time fields use `None` for Go's zero time;
//! `Option<DateTime>` ordering (`None` < `Some`) matches comparing against the zero time.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::types::{Auth, AuthError, ModelState, QuotaState, Status};
use http::HeaderMap;
use serde::Serialize;

use super::errors::{
    CODE_FORCE_COOLDOWN, CODE_UNAUTHORIZED, is_cloudflare_challenge_result,
    is_invalid_grant_result, is_model_support_result, should_skip_credential_cooldown,
};
use super::util::{add_duration, after, canonical_model_key};

pub const QUOTA_BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const QUOTA_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
pub const MIN_QUOTA_COOLDOWN_FLOOR: Duration = Duration::from_secs(10);
pub const TRANSIENT_ERROR_COOLDOWN: Duration = Duration::from_secs(60);
pub const REFRESH_PENDING_BACKOFF: Duration = Duration::from_secs(60);
pub const REFRESH_FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);
pub const INVALID_GRANT_BACKOFF_BASE: Duration = Duration::from_secs(60);
pub const INVALID_GRANT_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
pub const REFRESH_INEFFECTIVE_BACKOFF: Duration = Duration::from_secs(30);

const MAX_QUOTA_SIGNAL_HEADERS: usize = 64;
const MAX_QUOTA_SIGNAL_VALUE: usize = 512;

type Time = Option<DateTime<Utc>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    None,
    Cooldown,
    Disabled,
    Other,
}

/// Outcome of an availability check (Go: `(blocked, reason, next)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub blocked: bool,
    pub reason: BlockReason,
    pub next: Time,
}

impl Block {
    const FREE: Block = Block {
        blocked: false,
        reason: BlockReason::None,
        next: None,
    };
}

/// Go: nextQuotaCooldown. Ladder 1s, 2s, 4s ... capped at 30m (level frozen at the cap).
pub fn next_quota_cooldown(prev_level: i32, disable_cooling: bool) -> (Duration, i32) {
    let level = prev_level.max(0);
    if disable_cooling {
        return (Duration::ZERO, level);
    }
    // Saturating shift: past ~level 33 Go overflows; we clamp to the cap instead.
    let cooldown = if level >= 31 {
        QUOTA_BACKOFF_MAX
    } else {
        QUOTA_BACKOFF_BASE.saturating_mul(1u32 << level)
    };
    if cooldown >= QUOTA_BACKOFF_MAX {
        return (QUOTA_BACKOFF_MAX, level);
    }
    (cooldown, level + 1)
}

/// Go: quotaCooldownAfterFailure. A failure inside a live window reuses it, so a burst escalates
/// the ladder at most once per window.
pub fn quota_cooldown_after_failure(quota: &QuotaState, now: DateTime<Utc>) -> (Time, i32) {
    if after(quota.next_recover_at, now) {
        return (quota.next_recover_at, quota.backoff_level);
    }
    let (cooldown, level) = next_quota_cooldown(quota.backoff_level, false);
    let next = if cooldown > Duration::ZERO {
        Some(add_duration(now, cooldown))
    } else {
        None
    };
    (next, level)
}

pub fn next_cloudflare_cooldown(
    level: i32,
    disable_cooling: bool,
    now: DateTime<Utc>,
) -> (Time, i32) {
    if disable_cooling {
        return (None, level);
    }
    let (mut cooldown, next_level) = next_quota_cooldown(level, disable_cooling);
    if cooldown < Duration::from_secs(10) {
        cooldown = Duration::from_secs(10);
    }
    (Some(add_duration(now, cooldown)), next_level)
}

/// Deadline for transient failures (408/5xx and unknown statuses). `transient_seconds`: 0 keeps
/// the 60s default, negative disables transient cooldowns.
pub fn recoverable_failure_retry_after(
    now: DateTime<Utc>,
    retry_after: Option<Duration>,
    disable_cooling: bool,
    transient_seconds: i64,
) -> Time {
    if disable_cooling || transient_seconds < 0 {
        return None;
    }
    if let Some(ra) = retry_after
        && ra > Duration::ZERO
    {
        return Some(add_duration(now, ra));
    }
    if transient_seconds == 0 {
        return Some(add_duration(now, TRANSIENT_ERROR_COOLDOWN));
    }
    Some(add_duration(
        now,
        Duration::from_secs(transient_seconds as u64),
    ))
}

// ---- Availability ----

/// Go: availabilityBlock.
pub fn availability_block(
    unavailable: bool,
    quota_exceeded: bool,
    next_retry_after: Time,
    next_recover_at: Time,
    now: DateTime<Utc>,
) -> Block {
    if !unavailable && !quota_exceeded {
        return Block::FREE;
    }
    let has_recovery_time = next_retry_after.is_some() || next_recover_at.is_some();
    let mut next: Time = None;
    for candidate in [next_retry_after, next_recover_at] {
        if after(candidate, now) && (next.is_none() || candidate > next) {
            next = candidate;
        }
    }
    if next.is_some() {
        let reason = if quota_exceeded {
            BlockReason::Cooldown
        } else {
            BlockReason::Other
        };
        return Block {
            blocked: true,
            reason,
            next,
        };
    }
    if has_recovery_time {
        return Block::FREE;
    }
    Block {
        blocked: true,
        reason: BlockReason::Other,
        next: None,
    }
}

pub fn is_disabled(auth: &Auth) -> bool {
    auth.disabled || auth.status == Status::Disabled
}

/// Terminal unauthorized state with no refresh pending (Go: hasUnauthorizedAuthFailure).
pub fn has_unauthorized_auth_failure(auth: &Auth) -> bool {
    let Some(err) = &auth.last_error else {
        return false;
    };
    auth.unavailable
        && auth.status == Status::Error
        && auth.next_refresh_after.is_none()
        && auth.next_retry_after.is_none()
        && (err.http_status == 401 || err.code.eq_ignore_ascii_case(CODE_UNAUTHORIZED))
}

pub fn has_disabled_invalid_grant_failure(auth: &Auth) -> bool {
    if !is_disabled(auth) {
        return false;
    }
    match &auth.last_error {
        Some(e) => {
            is_invalid_grant_result(e)
                || e.message.to_lowercase().contains("invalid_grant")
                || e.code.to_lowercase().contains("invalid_grant")
        }
        None => false,
    }
}

/// Go: isAuthBlockedForModel.
pub fn is_auth_blocked_for_model(auth: &Auth, model: &str, now: DateTime<Utc>) -> Block {
    if is_disabled(auth) {
        return Block {
            blocked: true,
            reason: BlockReason::Disabled,
            next: None,
        };
    }
    if has_unauthorized_auth_failure(auth) {
        return Block {
            blocked: true,
            reason: BlockReason::Other,
            next: None,
        };
    }
    if let Some(exp) = auth.access_token_expiration_time()
        && exp <= now
    {
        return Block {
            blocked: true,
            reason: BlockReason::Other,
            next: None,
        };
    }
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && after(auth.quota.next_recover_at, now)
    {
        return Block {
            blocked: true,
            reason: BlockReason::Cooldown,
            next: auth.quota.next_recover_at,
        };
    }
    if !model.is_empty() {
        if !auth.model_states.is_empty() {
            let model_key = canonical_model_key(model);
            let mut matched = false;
            let mut blocked = false;
            let mut blocked_reason = BlockReason::None;
            let mut next_retry: Time = None;
            for (state_model, state) in &auth.model_states {
                if canonical_model_key(state_model) != model_key {
                    continue;
                }
                matched = true;
                if state.status == Status::Disabled {
                    return Block {
                        blocked: true,
                        reason: BlockReason::Disabled,
                        next: None,
                    };
                }
                let b = availability_block(
                    state.unavailable,
                    state.quota.exceeded,
                    state.next_retry_after,
                    state.quota.next_recover_at,
                    now,
                );
                if !b.blocked {
                    continue;
                }
                if b.next.is_none() {
                    return Block {
                        blocked: true,
                        reason: b.reason,
                        next: None,
                    };
                }
                if !blocked
                    || b.next > next_retry
                    || (b.next == next_retry && b.reason == BlockReason::Cooldown)
                {
                    blocked = true;
                    blocked_reason = b.reason;
                    next_retry = b.next;
                }
            }
            if matched {
                return Block {
                    blocked,
                    reason: blocked_reason,
                    next: next_retry,
                };
            }
            return Block::FREE;
        }
        return availability_block(
            auth.unavailable,
            auth.quota.exceeded,
            auth.next_retry_after,
            auth.quota.next_recover_at,
            now,
        );
    }
    let mut quota_exceeded = auth.quota.exceeded;
    // With per-model states the aggregate quota flag only summarizes single-model cooldowns.
    if !auth.model_states.is_empty() && auth.quota.reason != "credential_quota" && !auth.unavailable
    {
        quota_exceeded = false;
    }
    availability_block(
        auth.unavailable,
        quota_exceeded,
        auth.next_retry_after,
        auth.quota.next_recover_at,
        now,
    )
}

// ---- Model state helpers ----

pub fn new_model_state() -> ModelState {
    ModelState {
        status: Status::Active,
        ..Default::default()
    }
}

/// Re-keys `model_states` by canonical model key, merging duplicates (Go: normalizeModelStates).
pub fn normalize_model_states(auth: &mut Auth) -> bool {
    if auth.model_states.is_empty() {
        return false;
    }
    let mut normalized: BTreeMap<String, ModelState> = BTreeMap::new();
    let mut changed = false;
    for (model, state) in std::mem::take(&mut auth.model_states) {
        let mut key = canonical_model_key(&model);
        if key.is_empty() {
            key = model.trim().to_string();
        }
        if key != model {
            changed = true;
        }
        match normalized.get_mut(&key) {
            Some(existing) => {
                merge_model_state(existing, &state);
                changed = true;
            }
            None => {
                normalized.insert(key, state);
            }
        }
    }
    auth.model_states = normalized;
    changed
}

/// The state for `model` (canonical key), created active when missing (Go: ensureModelState).
pub fn ensure_model_state<'a>(auth: &'a mut Auth, model: &str) -> Option<&'a mut ModelState> {
    let key = canonical_model_key(model);
    if key.is_empty() {
        return None;
    }
    normalize_model_states(auth);
    Some(auth.model_states.entry(key).or_insert_with(new_model_state))
}

pub fn existing_model_state<'a>(auth: &'a Auth, model: &str) -> Option<&'a ModelState> {
    let key = canonical_model_key(model);
    if key.is_empty() {
        return None;
    }
    auth.model_states.get(&key)
}

/// Go: mergeModelState. Merges `source` into `target`, keeping the longer cooldown.
pub fn merge_model_state(target: &mut ModelState, source: &ModelState) {
    let (preferred, fallback) = if source.updated_at > target.updated_at {
        (source.clone(), target.clone())
    } else {
        (target.clone(), source.clone())
    };
    let mut merged = ModelState {
        status: preferred.status,
        status_message: preferred.status_message.clone(),
        unavailable: target.unavailable || source.unavailable,
        next_retry_after: target.next_retry_after,
        last_error: preferred.last_error.clone(),
        quota: QuotaState {
            exceeded: target.quota.exceeded || source.quota.exceeded,
            reason: preferred.quota.reason.clone(),
            next_recover_at: target.quota.next_recover_at,
            backoff_level: target.quota.backoff_level,
            ..Default::default()
        },
        updated_at: target.updated_at,
    };
    merged.quota = merge_quota_observation(merged.quota, &fallback.quota);
    merged.quota = merge_quota_observation(merged.quota, &preferred.quota);
    if source.next_retry_after > merged.next_retry_after {
        merged.next_retry_after = source.next_retry_after;
    }
    if source.quota.next_recover_at > merged.quota.next_recover_at {
        merged.quota.next_recover_at = source.quota.next_recover_at;
    }
    if source.quota.backoff_level > merged.quota.backoff_level {
        merged.quota.backoff_level = source.quota.backoff_level;
    }
    if source.updated_at > merged.updated_at {
        merged.updated_at = source.updated_at;
    }
    if merged.status_message.is_empty() {
        merged.status_message = fallback.status_message.clone();
    }
    if merged.last_error.is_none() {
        merged.last_error = fallback.last_error.clone();
    }
    if merged.quota.reason.is_empty() {
        merged.quota.reason = fallback.quota.reason.clone();
    }
    if target.status == Status::Disabled || source.status == Status::Disabled {
        merged.status = Status::Disabled;
    } else if merged.unavailable || merged.quota.exceeded {
        merged.status = Status::Error;
    }
    *target = merged;
}

pub fn reset_model_state(state: &mut ModelState, now: DateTime<Utc>) {
    state.unavailable = false;
    state.status = Status::Active;
    state.status_message.clear();
    state.next_retry_after = None;
    state.last_error = None;
    apply_cooldown_fields(&mut state.quota, &QuotaState::default());
    state.updated_at = Some(now);
}

pub fn is_model_state_active_cooldown(state: &ModelState, now: DateTime<Utc>) -> bool {
    if state.status == Status::Disabled {
        return true;
    }
    if after(state.next_retry_after, now) {
        return true;
    }
    if after(state.quota.next_recover_at, now) {
        return true;
    }
    state.quota.exceeded && state.quota.next_recover_at.is_none()
}

pub fn model_state_is_clean(state: &ModelState) -> bool {
    state.status == Status::Active
        && !state.unavailable
        && state.status_message.is_empty()
        && state.next_retry_after.is_none()
        && state.last_error.is_none()
        && !state.quota.exceeded
        && state.quota.reason.is_empty()
        && state.quota.next_recover_at.is_none()
        && state.quota.backoff_level == 0
}

pub fn has_model_error(auth: &Auth, now: DateTime<Utc>) -> bool {
    auth.model_states.values().any(|state| {
        state.last_error.is_some()
            || (state.status == Status::Error
                && state.unavailable
                && (state.next_retry_after.is_none() || after(state.next_retry_after, now)))
    })
}

pub fn clear_aggregated_availability(auth: &mut Auth) {
    auth.unavailable = false;
    auth.next_retry_after = None;
    apply_cooldown_fields(&mut auth.quota, &QuotaState::default());
}

/// Go: updateAggregatedAvailability. Folds per-model states into the credential-level flags.
pub fn update_aggregated_availability(auth: &mut Auth, now: DateTime<Utc>) {
    // A terminal unauthorized credential stays blocked until its tokens change.
    // Model-level results must not make it selectable again.
    if has_unauthorized_auth_failure(auth) {
        auth.unavailable = true;
        return;
    }
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && after(auth.quota.next_recover_at, now)
    {
        auth.unavailable = true;
        return;
    }
    if auth.model_states.is_empty() {
        clear_aggregated_availability(auth);
        return;
    }
    let mut all_unavailable = true;
    let mut earliest_retry: Time = None;
    let mut quota_exceeded = false;
    let mut quota_recover: Time = None;
    let mut max_backoff = 0;
    for state in auth.model_states.values_mut() {
        let mut state_unavailable = false;
        if state.status == Status::Disabled {
            state_unavailable = true;
        } else if state.unavailable {
            if state.next_retry_after.is_none() {
                state_unavailable = false;
            } else if after(state.next_retry_after, now) {
                state_unavailable = true;
                if earliest_retry.is_none() || state.next_retry_after < earliest_retry {
                    earliest_retry = state.next_retry_after;
                }
            } else {
                state.unavailable = false;
                state.next_retry_after = None;
            }
        }
        if !state_unavailable {
            all_unavailable = false;
        }
        if state.quota.exceeded {
            quota_exceeded = true;
            if quota_recover.is_none()
                || (state.quota.next_recover_at.is_some()
                    && state.quota.next_recover_at < quota_recover)
            {
                quota_recover = state.quota.next_recover_at;
            }
            if state.quota.backoff_level > max_backoff {
                max_backoff = state.quota.backoff_level;
            }
        }
    }
    auth.unavailable = all_unavailable;
    auth.next_retry_after = if all_unavailable {
        earliest_retry
    } else {
        None
    };
    if quota_exceeded {
        auth.quota.exceeded = true;
        auth.quota.reason = "quota".into();
        if auth.quota.next_recover_at > quota_recover {
            quota_recover = auth.quota.next_recover_at;
        }
        auth.quota.next_recover_at = quota_recover;
        auth.quota.backoff_level = max_backoff;
    } else if auth.quota.exceeded && after(auth.quota.next_recover_at, now) {
        // Retain active auth-level quota cooldown.
    } else {
        auth.quota.exceeded = false;
        auth.quota.reason.clear();
        auth.quota.next_recover_at = None;
        auth.quota.backoff_level = 0;
    }
}

pub fn clear_auth_state_on_success(auth: &mut Auth, now: DateTime<Utc>) {
    if has_unauthorized_auth_failure(auth) {
        auth.unavailable = true;
        return;
    }
    auth.unavailable = false;
    auth.status = Status::Active;
    auth.status_message.clear();
    auth.quota.exceeded = false;
    auth.quota.reason.clear();
    auth.quota.next_recover_at = None;
    auth.quota.backoff_level = 0;
    auth.last_error = None;
    auth.next_retry_after = None;
    auth.updated_at = Some(now);
}

/// Resets model states whose last error was an unauthorized failure; returns the resumed models.
pub fn clear_unauthorized_model_states(auth: &mut Auth, now: DateTime<Utc>) -> Vec<String> {
    let mut resumed = Vec::new();
    for (model, state) in auth.model_states.iter_mut() {
        let mut is_unauth = false;
        if let Some(e) = &state.last_error
            && (e.http_status == 401
                || e.code.eq_ignore_ascii_case(CODE_UNAUTHORIZED)
                || super::errors::Failure {
                    status: e.http_status,
                    text: &e.message,
                    request_scoped: false,
                    code: None,
                    raw_message: None,
                }
                .is_unauthorized())
        {
            is_unauth = true;
        }
        if !is_unauth && state.status_message.to_lowercase().contains("unauthorized") {
            is_unauth = true;
        }
        if !is_unauth {
            continue;
        }
        reset_model_state(state, now);
        resumed.push(model.clone());
    }
    if !resumed.is_empty() {
        update_aggregated_availability(auth, now);
    }
    resumed
}

/// Wipes cooldown/quota deadlines on the credential and every model (Go: clearCooldownStateForAuth).
pub fn clear_cooldown_state_for_auth(auth: &mut Auth, now: DateTime<Utc>) -> bool {
    if has_unauthorized_auth_failure(auth) {
        return false;
    }
    let mut changed = false;
    if auth.unavailable
        || auth.next_retry_after.is_some()
        || auth.quota.exceeded
        || auth.quota.next_recover_at.is_some()
    {
        auth.unavailable = false;
        auth.next_retry_after = None;
        apply_cooldown_fields(&mut auth.quota, &QuotaState::default());
        auth.updated_at = Some(now);
        changed = true;
    }
    for state in auth.model_states.values_mut() {
        if state.unavailable
            || state.next_retry_after.is_some()
            || state.quota.exceeded
            || state.quota.next_recover_at.is_some()
        {
            state.unavailable = false;
            state.next_retry_after = None;
            apply_cooldown_fields(&mut state.quota, &QuotaState::default());
            state.updated_at = Some(now);
            changed = true;
        }
    }
    if !auth.model_states.is_empty() {
        update_aggregated_availability(auth, now);
    }
    if changed {
        auth.generation += 1;
        auth.updated_at = Some(now);
    }
    changed
}

// ---- Quota observation ----

pub fn apply_cooldown_fields(dst: &mut QuotaState, cooldown: &QuotaState) {
    dst.exceeded = cooldown.exceeded;
    dst.reason = cooldown.reason.clone();
    dst.next_recover_at = cooldown.next_recover_at;
    dst.backoff_level = cooldown.backoff_level;
}

pub fn cooldown_fields_of(q: &QuotaState) -> QuotaState {
    QuotaState {
        exceeded: q.exceeded,
        reason: q.reason.clone(),
        next_recover_at: q.next_recover_at,
        backoff_level: q.backoff_level,
        ..Default::default()
    }
}

/// Keeps the newest observation snapshot (Go: mergeQuotaObservation).
pub fn merge_quota_observation(mut target: QuotaState, source: &QuotaState) -> QuotaState {
    match source.observed_at {
        None => return target,
        Some(s) if target.observed_at.is_some_and(|t| s < t) => return target,
        Some(s) => {
            target.observed_at = Some(s);
            target.signals = source.signals.clone();
        }
    }
    target
}

pub fn provider_supports_quota_observation(provider: &str) -> bool {
    matches!(
        provider.trim().to_lowercase().as_str(),
        "claude" | "codex" | "devin"
    )
}

fn is_quota_signal_header_for_provider(provider: &str, name: &str) -> bool {
    let provider = provider.trim().to_lowercase();
    let name = name.trim().to_lowercase();
    if name == "retry-after" {
        return provider == "claude" || provider == "codex";
    }
    if name.starts_with("anthropic-ratelimit-unified-") {
        return provider == "claude";
    }
    if name.starts_with("x-ratelimit-") {
        return provider == "codex";
    }
    if !name.starts_with("x-codex-") || provider != "codex" {
        return false;
    }
    if name == "x-codex-active-limit"
        || name == "x-codex-plan-type"
        || name.starts_with("x-codex-credits-")
    {
        return true;
    }
    [
        "-allowed",
        "-limit-reached",
        "-limit-name",
        "-used-percent",
        "-window-minutes",
        "-reset-after-seconds",
        "-reset-at",
        "-over-secondary-limit-percent",
    ]
    .iter()
    .any(|m| name.contains(m))
}

fn quota_signal_retention_rank(name: &str) -> u8 {
    let lower = name.trim().to_lowercase();
    if lower == "retry-after" || lower.starts_with("anthropic-ratelimit-unified-") {
        0
    } else if lower == "x-codex-plan-type"
        || lower == "x-codex-active-limit"
        || lower.starts_with("x-codex-credits-")
    {
        1
    } else if lower == "x-codex-allowed"
        || lower == "x-codex-limit-reached"
        || lower.starts_with("x-codex-primary-")
        || lower.starts_with("x-codex-secondary-")
    {
        2
    } else if lower.starts_with("x-codex-code-review-") {
        3
    } else if lower.starts_with("x-codex-additional-") {
        5
    } else if lower.starts_with("x-codex-") {
        4
    } else {
        6
    }
}

fn valid_quota_signal_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_QUOTA_SIGNAL_VALUE
        && !value.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

/// Canonical header name (Go `http.CanonicalHeaderKey`): `x-codex-plan-type` -> `X-Codex-Plan-Type`.
fn canonical_header_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.trim().chars() {
        if upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        upper = c == '-';
    }
    out
}

fn collect_quota_signals(provider: &str, headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut names: Vec<String> = Vec::new();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for key in headers.keys() {
        let canonical = canonical_header_key(key.as_str());
        if !is_quota_signal_header_for_provider(provider, &canonical) {
            continue;
        }
        let Some(last) = headers.get_all(key).iter().next_back() else {
            continue;
        };
        let Ok(value) = last.to_str() else { continue };
        let value = value.trim();
        if !valid_quota_signal_value(value) {
            continue;
        }
        if !values.contains_key(&canonical) {
            names.push(canonical.clone());
        }
        values.insert(canonical, value.to_string());
    }
    names.sort_by(|a, b| {
        quota_signal_retention_rank(a)
            .cmp(&quota_signal_retention_rank(b))
            .then(a.cmp(b))
    });
    names.truncate(MAX_QUOTA_SIGNAL_HEADERS);
    names
        .into_iter()
        .filter_map(|n| values.get(&n).map(|v| (n.clone(), v.clone())))
        .collect()
}

/// Replaces the passive quota snapshot with the signals of the current response (Go:
/// ObserveResponseHeadersForProvider). Responses without quota signals keep the old snapshot.
pub fn observe_response_headers(
    q: &mut QuotaState,
    provider: &str,
    headers: &HeaderMap,
    observed_at: DateTime<Utc>,
) -> bool {
    if !provider_supports_quota_observation(provider) {
        if q.signals.is_empty() && q.observed_at.is_none() {
            return false;
        }
        q.signals.clear();
        q.observed_at = None;
        return true;
    }
    let next = collect_quota_signals(provider, headers);
    if next.is_empty() {
        return false;
    }
    q.signals = next;
    q.observed_at = Some(observed_at);
    true
}

// ---- MarkResult math ----

/// Cooling configuration resolved by the manager for one credential.
#[derive(Debug, Clone, Copy)]
pub struct CoolingPolicy {
    pub disable_cooling: bool,
    /// `transient-error-cooldown-seconds`: 0 = 60s default, negative = disabled.
    pub transient_seconds: i64,
}

/// An execution outcome as recorded by the manager (Go: auth.Result).
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub auth_id: String,
    pub provider: String,
    /// State model: the upstream model for pooled/aliased routes, else the route model.
    pub model: String,
    pub route_model: String,
    pub success: bool,
    pub retry_after: Option<Duration>,
    pub credential_scope: bool,
    pub error: Option<AuthError>,
    /// Execution options of the attempt (headers, original request, metadata) for session
    /// affinity bookkeeping.
    pub options: crate::executor::Options,
    /// Count-tokens results must not replace the last observed quota snapshot.
    pub skip_quota_observation: bool,
    /// Upstream response headers of this attempt, for passive quota observation.
    pub response_headers: HeaderMap,
}

impl ExecResult {
    pub fn status_code(&self) -> i32 {
        self.error.as_ref().map_or(0, |e| e.http_status)
    }
}

/// Failure bookkeeping for a credential without a model key (Go: applyAuthFailureState).
pub fn apply_auth_failure_state(
    auth: &mut Auth,
    result_err: Option<&AuthError>,
    retry_after: Option<Duration>,
    now: DateTime<Utc>,
    policy: CoolingPolicy,
    disable_cooling: bool,
) {
    let prev_auth_retry_after = auth.next_retry_after;
    if should_skip_credential_cooldown(result_err) {
        return;
    }
    auth.unavailable = true;
    auth.status = Status::Error;
    auth.updated_at = Some(now);
    if let Some(e) = result_err {
        auth.last_error = Some(e.clone());
        if !e.message.is_empty() {
            auth.status_message = e.message.clone();
        }
    }
    let status = result_err.map_or(0, |e| e.http_status);
    let cloudflare = result_err.is_some_and(is_cloudflare_challenge_result);
    let invalid_grant = result_err.is_some_and(is_invalid_grant_result);
    if cloudflare {
        auth.status_message = "cloudflare challenge".into();
        let (next, level) =
            next_cloudflare_cooldown(auth.quota.backoff_level, disable_cooling, now);
        apply_cooldown_fields(
            &mut auth.quota,
            &QuotaState {
                exceeded: true,
                reason: "cloudflare challenge".into(),
                next_recover_at: next,
                backoff_level: level,
                ..Default::default()
            },
        );
        auth.next_retry_after = next;
    } else if invalid_grant {
        auth.status_message = "invalid_grant".into();
        auth.next_retry_after = if disable_cooling {
            None
        } else {
            Some(add_duration(now, Duration::from_secs(30 * 60)))
        };
    } else {
        match status {
            401 => {
                auth.status_message = "unauthorized".into();
                auth.next_retry_after = if disable_cooling {
                    None
                } else {
                    Some(add_duration(now, Duration::from_secs(30 * 60)))
                };
            }
            402 | 403 => {
                auth.status_message = "payment_required".into();
                auth.next_retry_after = if disable_cooling {
                    None
                } else {
                    Some(add_duration(now, Duration::from_secs(30 * 60)))
                };
            }
            404 => {
                auth.status_message = "not_found".into();
                auth.next_retry_after = if disable_cooling {
                    None
                } else if let Some(ra) = retry_after.filter(|d| *d > Duration::ZERO) {
                    Some(add_duration(now, ra))
                } else {
                    Some(add_duration(now, Duration::from_secs(12 * 3600)))
                };
            }
            429 => {
                auth.status_message = "quota exhausted".into();
                auth.quota.exceeded = true;
                auth.quota.reason = "quota".into();
                let mut next: Time = None;
                if !disable_cooling {
                    if let Some(ra) = retry_after {
                        next = Some(add_duration(now, ra.max(MIN_QUOTA_COOLDOWN_FLOOR)));
                    } else {
                        let (n, level) = quota_cooldown_after_failure(&auth.quota, now);
                        next = n;
                        auth.quota.backoff_level = level;
                    }
                    if auth.quota.exceeded && auth.quota.next_recover_at > next {
                        next = auth.quota.next_recover_at;
                    }
                }
                auth.quota.next_recover_at = next;
                auth.next_retry_after = next;
            }
            408 | 500 | 502 | 503 | 504 | 520..=526 => {
                auth.status_message = "transient upstream error".into();
                auth.next_retry_after = recoverable_failure_retry_after(
                    now,
                    retry_after,
                    disable_cooling,
                    policy.transient_seconds,
                );
                auth.unavailable = auth.next_retry_after.is_some();
            }
            _ => {
                if auth.status_message.is_empty() {
                    auth.status_message = "request failed".into();
                }
                auth.next_retry_after = recoverable_failure_retry_after(
                    now,
                    None,
                    disable_cooling,
                    policy.transient_seconds,
                );
                auth.unavailable = auth.next_retry_after.is_some();
            }
        }
    }
    // A later failure only extends a still-live credential cooldown.
    if auth.next_retry_after.is_some()
        && prev_auth_retry_after > auth.next_retry_after
        && after(prev_auth_retry_after, now)
    {
        auth.next_retry_after = prev_auth_retry_after;
    }
    if result_err.is_some_and(|e| e.code == CODE_FORCE_COOLDOWN) && auth.next_retry_after.is_none()
    {
        auth.next_retry_after = Some(add_duration(now, TRANSIENT_ERROR_COOLDOWN));
        auth.unavailable = true;
    }
    if disable_cooling && auth.next_retry_after.is_none() && auth.quota.next_recover_at.is_none() {
        auth.unavailable = false;
        auth.quota.exceeded = false;
    }
}

/// Applies one execution result to the credential's state (Go: MarkResult body under the lock).
/// `model_key` is the canonical state model (already resolved from the route model when empty).
pub fn apply_result(
    auth: &mut Auth,
    result: &ExecResult,
    model_key: &str,
    now: DateTime<Utc>,
    policy: CoolingPolicy,
) {
    auth.record_recent_request(now, result.success);
    if result.success {
        auth.success += 1;
    } else {
        auth.failed += 1;
    }

    let was_terminal_unauthorized = has_unauthorized_auth_failure(auth);

    if result.success {
        if was_terminal_unauthorized {
            // In-flight successes must not revive a terminal unauthorized credential.
            if !model_key.is_empty()
                && let Some(state) = ensure_model_state(auth, model_key)
            {
                reset_model_state(state, now);
            }
        } else if auth.quota.reason == "credential_quota" && after(auth.quota.next_recover_at, now) {
            // Retain active credential-scoped cooldown.
        } else if !model_key.is_empty() {
            if let Some(state) = ensure_model_state(auth, model_key) {
                reset_model_state(state, now);
            }
            update_aggregated_availability(auth, now);
            if !has_model_error(auth, now) {
                auth.last_error = None;
                auth.status_message.clear();
                auth.status = Status::Active;
            }
        } else {
            clear_auth_state_on_success(auth, now);
        }
    } else if !model_key.is_empty() {
        if !should_skip_credential_cooldown(result.error.as_ref()) {
            apply_model_failure(auth, result, model_key, now, policy, was_terminal_unauthorized);
        }
    } else {
        let mut disable = policy.disable_cooling;
        if result
            .error
            .as_ref()
            .is_some_and(|e| e.code == CODE_FORCE_COOLDOWN)
        {
            disable = false;
        }
        if !was_terminal_unauthorized {
            apply_auth_failure_state(
                auth,
                result.error.as_ref(),
                result.retry_after,
                now,
                policy,
                disable,
            );
        }
    }

    if was_terminal_unauthorized {
        auth.unavailable = true;
        auth.status = Status::Error;
        auth.next_refresh_after = None;
        auth.next_retry_after = None;
    }

    auth.generation += 1;
    auth.updated_at = Some(now);

    if !result.skip_quota_observation {
        observe_response_headers(
            &mut auth.quota,
            &result.provider,
            &result.response_headers,
            now,
        );
        if !model_key.is_empty()
            && let Some(state) = auth.model_states.get_mut(&canonical_model_key(model_key))
        {
            observe_response_headers(
                &mut state.quota,
                &result.provider,
                &result.response_headers,
                now,
            );
        }
    }
}

fn apply_model_failure(
    auth: &mut Auth,
    result: &ExecResult,
    model_key: &str,
    now: DateTime<Utc>,
    policy: CoolingPolicy,
    was_terminal_unauthorized: bool,
) {
    let mut disable = policy.disable_cooling;
    if result
        .error
        .as_ref()
        .is_some_and(|e| e.code == CODE_FORCE_COOLDOWN)
    {
        disable = false;
    }
    // Take the state out so sibling states and credential fields can be edited alongside it.
    normalize_model_states(auth);
    let key = canonical_model_key(model_key);
    let mut state = auth
        .model_states
        .remove(&key)
        .unwrap_or_else(new_model_state);

    state.unavailable = true;
    state.status = Status::Error;
    state.updated_at = Some(now);
    let prev_model_retry_after = state.next_retry_after;
    if let Some(err) = &result.error {
        state.last_error = Some(err.clone());
        state.status_message = err.message.clone();
        if !was_terminal_unauthorized {
            auth.last_error = Some(err.clone());
            auth.status_message = err.message.clone();
        }
    }

    let status_code = result.status_code();
    let err_ref = result.error.as_ref();
    let retry_after = result.retry_after;
    let day_half = Duration::from_secs(12 * 3600);

    if err_ref.is_some_and(is_model_support_result) {
        state.next_retry_after = if disable {
            None
        } else if let Some(ra) = retry_after.filter(|d| *d > Duration::ZERO) {
            Some(add_duration(now, ra))
        } else {
            Some(add_duration(now, day_half))
        };
    } else if err_ref.is_some_and(is_cloudflare_challenge_result) {
        let (next, level) = next_cloudflare_cooldown(state.quota.backoff_level, disable, now);
        state.next_retry_after = next;
        state.status_message = "cloudflare challenge".into();
        if auth.last_error.is_some() && !was_terminal_unauthorized {
            auth.status_message = "cloudflare challenge".into();
        }
        apply_cooldown_fields(
            &mut state.quota,
            &QuotaState {
                exceeded: true,
                reason: "cloudflare challenge".into(),
                next_recover_at: next,
                backoff_level: level,
                ..Default::default()
            },
        );
    } else if err_ref.is_some_and(is_invalid_grant_result) {
        state.next_retry_after = if disable {
            None
        } else {
            Some(add_duration(now, Duration::from_secs(30 * 60)))
        };
    } else {
        match status_code {
            401 | 402 | 403 => {
                state.next_retry_after = if disable {
                    None
                } else {
                    Some(add_duration(now, Duration::from_secs(30 * 60)))
                };
            }
            404 => {
                state.next_retry_after = if disable {
                    None
                } else if let Some(ra) = retry_after.filter(|d| *d > Duration::ZERO) {
                    Some(add_duration(now, ra))
                } else {
                    Some(add_duration(now, day_half))
                };
            }
            429 => {
                let mut next: Time = None;
                let mut credential_next: Time = None;
                let mut backoff_level = state.quota.backoff_level;
                let auth_credential_quota =
                    auth.quota.exceeded && auth.quota.reason == "credential_quota";
                if result.credential_scope {
                    backoff_level = if auth_credential_quota {
                        auth.quota.backoff_level
                    } else {
                        0
                    };
                }
                if !disable {
                    if let Some(ra) = retry_after {
                        next = Some(add_duration(now, ra.max(MIN_QUOTA_COOLDOWN_FLOOR)));
                    } else {
                        let mut quota_for_failure = state.quota.clone();
                        if result.credential_scope {
                            if auth_credential_quota {
                                quota_for_failure = auth.quota.clone();
                            } else {
                                quota_for_failure.next_recover_at = None;
                                quota_for_failure.backoff_level = 0;
                            }
                        }
                        let (n, level) = quota_cooldown_after_failure(&quota_for_failure, now);
                        next = n;
                        backoff_level = level;
                    }
                    credential_next = next;
                    if state.quota.exceeded && state.quota.next_recover_at > next {
                        next = state.quota.next_recover_at;
                    }
                }
                state.next_retry_after = next;
                apply_cooldown_fields(
                    &mut state.quota,
                    &QuotaState {
                        exceeded: true,
                        reason: "quota".into(),
                        next_recover_at: next,
                        backoff_level,
                        ..Default::default()
                    },
                );
                if result.credential_scope && !disable {
                    for other in auth.model_states.values_mut() {
                        other.unavailable = true;
                        other.status = Status::Error;
                        let mut other_quota_next = credential_next;
                        if other.quota.exceeded && other.quota.next_recover_at > other_quota_next {
                            other_quota_next = other.quota.next_recover_at;
                        }
                        let mut other_retry_after = other_quota_next;
                        // Propagation only extends a sibling's still-live deadline.
                        if other.next_retry_after.is_some()
                            && other.next_retry_after > other_retry_after
                        {
                            other_retry_after = other.next_retry_after;
                        }
                        other.next_retry_after = other_retry_after;
                        apply_cooldown_fields(
                            &mut other.quota,
                            &QuotaState {
                                exceeded: true,
                                reason: "credential_quota".into(),
                                next_recover_at: other_quota_next,
                                backoff_level,
                                ..Default::default()
                            },
                        );
                    }
                    if !was_terminal_unauthorized {
                        auth.unavailable = true;
                        let mut auth_next = credential_next;
                        if auth_credential_quota && auth.quota.next_recover_at > auth_next {
                            auth_next = auth.quota.next_recover_at;
                        }
                        auth.quota.exceeded = true;
                        auth.quota.reason = "credential_quota".into();
                        auth.quota.next_recover_at = auth_next;
                        auth.quota.backoff_level = backoff_level;
                        auth.next_retry_after = auth_next;
                    }
                }
            }
            408 | 500 | 502 | 503 | 504 | 520..=526 => {
                state.next_retry_after = recoverable_failure_retry_after(
                    now,
                    retry_after,
                    disable,
                    policy.transient_seconds,
                );
                state.unavailable = state.next_retry_after.is_some();
            }
            _ => {
                state.next_retry_after =
                    recoverable_failure_retry_after(now, None, disable, policy.transient_seconds);
                state.unavailable = state.next_retry_after.is_some();
            }
        }
    }

    if disable && state.next_retry_after.is_none() && state.quota.next_recover_at.is_none() {
        state.unavailable = false;
        state.quota.exceeded = false;
    }
    if err_ref.is_some_and(|e| e.code == CODE_FORCE_COOLDOWN) && state.next_retry_after.is_none() {
        state.next_retry_after = Some(add_duration(now, TRANSIENT_ERROR_COOLDOWN));
        state.unavailable = true;
    }
    // A later failure only extends a still-live cooldown; never shortens it.
    if state.next_retry_after.is_some()
        && prev_model_retry_after > state.next_retry_after
        && after(prev_model_retry_after, now)
    {
        state.next_retry_after = prev_model_retry_after;
    }
    auth.model_states.insert(key, state);
    auth.status = Status::Error;
    update_aggregated_availability(auth, now);
}

/// Backoff for repeated invalid_grant refresh failures: 1m, 2m, ... capped at 30m.
pub fn invalid_grant_backoff_duration(failures: i32) -> Duration {
    if failures <= 1 {
        return INVALID_GRANT_BACKOFF_BASE;
    }
    let shift = (failures - 1).min(10) as u32;
    let backoff = INVALID_GRANT_BACKOFF_BASE.saturating_mul(1u32 << shift);
    backoff.min(INVALID_GRANT_BACKOFF_MAX)
}

// ---- Cooldown view (management) ----

/// An unexpired local retry restriction (Go: CooldownView). Contains no credential metadata.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CooldownView {
    pub scope: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model_key: String,
    pub reason: String,
    pub retry_at: DateTime<Utc>,
    pub remaining_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_level: Option<i32>,
    #[serde(skip_serializing_if = "is_zero_status")]
    pub http_status: i32,
}

fn is_zero_status(v: &i32) -> bool {
    *v == 0
}

/// Timers visible to management for one credential. An empty result does not imply usability.
pub fn cooldown_snapshot_for_auth(auth: &Auth, now: DateTime<Utc>) -> Vec<CooldownView> {
    let mut views = Vec::new();
    if auth.quota.exceeded
        && auth.quota.reason == "credential_quota"
        && after(auth.quota.next_recover_at, now)
    {
        if let Some(next) = auth.quota.next_recover_at {
            views.push(new_cooldown_view(
                "credential",
                "",
                next,
                now,
                &auth.quota,
                &auth.status_message,
                auth.last_error.as_ref(),
            ));
        }
    } else if auth.model_states.is_empty() {
        let b = availability_block(
            auth.unavailable,
            auth.quota.exceeded,
            auth.next_retry_after,
            auth.quota.next_recover_at,
            now,
        );
        if let (true, Some(next)) = (b.blocked, b.next)
            && next > now
        {
            views.push(new_cooldown_view(
                "credential",
                "",
                next,
                now,
                &auth.quota,
                &auth.status_message,
                auth.last_error.as_ref(),
            ));
        }
    }

    let mut by_model: BTreeMap<String, (CooldownView, BlockReason)> = BTreeMap::new();
    for (key, state) in &auth.model_states {
        let model = canonical_model_key(key);
        if model.is_empty() {
            continue;
        }
        let b = availability_block(
            state.unavailable,
            state.quota.exceeded,
            state.next_retry_after,
            state.quota.next_recover_at,
            now,
        );
        let Some(next) = b.next.filter(|n| b.blocked && *n > now) else {
            continue;
        };
        if let Some((prev, prev_reason)) = by_model.get(&model) {
            let prefer_quota_tie = next == prev.retry_at
                && b.reason == BlockReason::Cooldown
                && *prev_reason != BlockReason::Cooldown;
            if next <= prev.retry_at && !prefer_quota_tie {
                continue;
            }
        }
        by_model.insert(
            model.clone(),
            (
                new_cooldown_view(
                    "model",
                    &model,
                    next,
                    now,
                    &state.quota,
                    &state.status_message,
                    state.last_error.as_ref(),
                ),
                b.reason,
            ),
        );
    }
    views.extend(by_model.into_values().map(|(v, _)| v));
    views
}

fn new_cooldown_view(
    scope: &str,
    model: &str,
    next: DateTime<Utc>,
    now: DateTime<Utc>,
    quota: &QuotaState,
    status_message: &str,
    last_err: Option<&AuthError>,
) -> CooldownView {
    // Ceil to whole seconds.
    let nanos = (next - now).num_nanoseconds().unwrap_or(i64::MAX);
    let seconds = nanos / 1_000_000_000 + i64::from(nanos % 1_000_000_000 != 0);
    let mut view = CooldownView {
        scope: scope.into(),
        model_key: model.into(),
        reason: "unknown".into(),
        retry_at: next,
        remaining_seconds: seconds,
        backoff_level: None,
        http_status: 0,
    };
    if quota.exceeded && (quota.next_recover_at.is_none() || quota.next_recover_at >= Some(next)) {
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
    let error_reason = cooldown_error_reason(last_err);
    if view.reason == "unknown" {
        view.reason = error_reason.clone();
    }
    if view.reason == "unknown" {
        view.reason = cooldown_status_reason(status_message);
    }
    if !propagated_quota
        && let Some(e) = last_err
        && (400..=599).contains(&e.http_status)
        && error_reason == view.reason
    {
        view.http_status = e.http_status;
    }
    view
}

fn cooldown_error_reason(err: Option<&AuthError>) -> String {
    let Some(e) = err else {
        return "unknown".into();
    };
    if is_model_support_result(e) {
        return "model_not_supported".into();
    }
    if is_cloudflare_challenge_result(e) {
        return "cloudflare_challenge".into();
    }
    if is_invalid_grant_result(e) {
        return "invalid_grant".into();
    }
    match e.http_status {
        401 => return "unauthorized".into(),
        402 | 403 => return "payment_required".into(),
        404 => return "not_found".into(),
        429 => return "quota".into(),
        408 | 500 | 502 | 503 | 504 | 520..=526 => return "transient_error".into(),
        _ => {}
    }
    cooldown_status_reason(&e.code)
}

/// Only exact known markers become public reason codes.
fn cooldown_status_reason(message: &str) -> String {
    match message.trim() {
        "quota" | "quota exhausted" => "quota".into(),
        "cloudflare challenge" => "cloudflare_challenge".into(),
        m @ ("invalid_grant" | "unauthorized" | "payment_required" | "not_found") => m.into(),
        "model_not_supported" => "model_not_supported".into(),
        "transient upstream error" => "transient_error".into(),
        _ => "unknown".into(),
    }
}

/// Reason string persisted with cooldown records (Go: cooldownReason).
pub fn cooldown_reason(
    status_message: &str,
    quota: &QuotaState,
    last_err: Option<&AuthError>,
) -> String {
    let reason = quota.reason.trim();
    if !reason.is_empty() {
        return reason.to_string();
    }
    let sm = status_message.trim();
    if !sm.is_empty() {
        return sm.to_string();
    }
    if let Some(e) = last_err {
        if !e.code.trim().is_empty() {
            return e.code.trim().to_string();
        }
        if !e.message.trim().is_empty() {
            return e.message.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn ladder_doubles_then_caps_at_30m() {
        let mut level = 0;
        let mut seen = Vec::new();
        for _ in 0..13 {
            let (d, l) = next_quota_cooldown(level, false);
            seen.push(d.as_secs());
            level = l;
        }
        assert_eq!(&seen[..4], &[1, 2, 4, 8]);
        assert_eq!(seen[10], 1024);
        assert_eq!(seen[11], 1800);
        assert_eq!(seen[12], 1800);
        assert_eq!(level, 11, "level freezes at the cap");
        assert_eq!(next_quota_cooldown(0, true), (Duration::ZERO, 0));
        assert_eq!(next_quota_cooldown(200, false).0, QUOTA_BACKOFF_MAX);
    }

    #[test]
    fn burst_failures_escalate_once_per_window() {
        let now = t(0);
        let q = QuotaState::default();
        let (n1, l1) = quota_cooldown_after_failure(&q, now);
        assert_eq!((n1, l1), (Some(t(1)), 1));
        let q2 = QuotaState {
            next_recover_at: n1,
            backoff_level: l1,
            ..Default::default()
        };
        let (n2, l2) = quota_cooldown_after_failure(&q2, now);
        assert_eq!((n2, l2), (n1, l1));
    }

    #[test]
    fn availability_block_cases() {
        let now = t(100);
        assert!(!availability_block(false, false, None, None, now).blocked);
        let b = availability_block(true, true, Some(t(200)), Some(t(150)), now);
        assert_eq!(
            (b.blocked, b.reason, b.next),
            (true, BlockReason::Cooldown, Some(t(200)))
        );
        // Expired cooldown is available again.
        assert!(!availability_block(true, true, Some(t(50)), None, now).blocked);
        // Flagged unavailable with no deadline is blocked indefinitely.
        let b = availability_block(true, false, None, None, now);
        assert_eq!((b.blocked, b.next), (true, None));
    }

    fn result_with(status: i32, retry_after: Option<Duration>) -> ExecResult {
        ExecResult {
            auth_id: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            route_model: "m".into(),
            success: false,
            retry_after,
            credential_scope: false,
            error: Some(AuthError {
                message: "boom".into(),
                http_status: status,
                ..Default::default()
            }),
            options: crate::executor::Options::new(cpa_translator::Format::OpenAI),
            skip_quota_observation: true,
            response_headers: HeaderMap::new(),
        }
    }

    const POLICY: CoolingPolicy = CoolingPolicy {
        disable_cooling: false,
        transient_seconds: 0,
    };

    #[test]
    fn retry_after_floor_and_never_shorten() {
        let mut auth = Auth::new("a", "p");
        apply_result(
            &mut auth,
            &result_with(429, Some(Duration::from_secs(2))),
            "m",
            t(0),
            POLICY,
        );
        assert_eq!(
            auth.model_states["m"].next_retry_after,
            Some(t(10)),
            "retry-after is floored at 10s"
        );
        assert!(auth.unavailable);
        // A longer hint extends the live cooldown.
        apply_result(
            &mut auth,
            &result_with(429, Some(Duration::from_secs(60))),
            "m",
            t(1),
            POLICY,
        );
        assert_eq!(auth.model_states["m"].next_retry_after, Some(t(61)));
        // A shorter later failure must not shorten it.
        apply_result(
            &mut auth,
            &result_with(429, Some(Duration::from_secs(1))),
            "m",
            t(2),
            POLICY,
        );
        assert_eq!(auth.model_states["m"].next_retry_after, Some(t(61)));
    }

    #[test]
    fn credential_scope_429_cools_siblings() {
        let mut auth = Auth::new("a", "p");
        auth.model_states.insert("other".into(), new_model_state());
        let mut r = result_with(429, None);
        r.credential_scope = true;
        apply_result(&mut auth, &r, "m", t(0), POLICY);
        assert_eq!(auth.quota.reason, "credential_quota");
        assert_eq!(auth.model_states["other"].quota.reason, "credential_quota");
        assert_eq!(auth.model_states["m"].quota.reason, "quota");
        assert!(is_auth_blocked_for_model(&auth, "anything", t(0)).blocked);
    }

    #[test]
    fn status_table_and_disable_cooling() {
        let mut auth = Auth::new("a", "p");
        apply_result(&mut auth, &result_with(401, None), "m", t(0), POLICY);
        assert_eq!(auth.model_states["m"].next_retry_after, Some(t(1800)));
        let mut auth = Auth::new("a", "p");
        apply_result(&mut auth, &result_with(404, None), "m", t(0), POLICY);
        assert_eq!(auth.model_states["m"].next_retry_after, Some(t(12 * 3600)));
        let mut auth = Auth::new("a", "p");
        apply_result(&mut auth, &result_with(503, None), "m", t(0), POLICY);
        assert_eq!(auth.model_states["m"].next_retry_after, Some(t(60)));
        let mut auth = Auth::new("a", "p");
        apply_result(
            &mut auth,
            &result_with(500, None),
            "m",
            t(0),
            CoolingPolicy {
                disable_cooling: true,
                transient_seconds: 0,
            },
        );
        assert!(!auth.unavailable && auth.model_states["m"].next_retry_after.is_none());
        let mut auth = Auth::new("a", "p");
        apply_result(
            &mut auth,
            &result_with(500, None),
            "m",
            t(0),
            CoolingPolicy {
                disable_cooling: false,
                transient_seconds: -1,
            },
        );
        assert!(!auth.unavailable);
    }

    #[test]
    fn success_clears_model_state() {
        let mut auth = Auth::new("a", "p");
        apply_result(&mut auth, &result_with(500, None), "m", t(0), POLICY);
        assert!(is_auth_blocked_for_model(&auth, "m", t(1)).blocked);
        let mut ok = result_with(0, None);
        ok.success = true;
        ok.error = None;
        apply_result(&mut auth, &ok, "m", t(2), POLICY);
        assert!(!is_auth_blocked_for_model(&auth, "m", t(3)).blocked);
        assert_eq!(auth.status, Status::Active);
        assert!(auth.last_error.is_none());
    }

    #[test]
    fn request_scoped_failures_do_not_cool() {
        let mut auth = Auth::new("a", "p");
        let mut r = result_with(400, None);
        r.error.as_mut().unwrap().code = "request_scoped".into();
        apply_result(&mut auth, &r, "m", t(0), POLICY);
        assert!(auth.model_states.is_empty() && !auth.unavailable);
        assert_eq!(auth.failed, 1);
    }

    #[test]
    fn cooldown_view_reports_reason() {
        let mut auth = Auth::new("a", "p");
        apply_result(
            &mut auth,
            &result_with(429, Some(Duration::from_secs(30))),
            "m",
            t(0),
            POLICY,
        );
        let views = cooldown_snapshot_for_auth(&auth, t(5));
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].reason, "quota");
        assert_eq!(views[0].remaining_seconds, 25);
        assert_eq!(views[0].http_status, 429);
    }
}
