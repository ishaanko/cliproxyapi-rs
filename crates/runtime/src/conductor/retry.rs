//! Retry rounds (Go: conductor_selection.go retry helpers).
//!
//! A round visits each eligible credential once. After a round fails with a retry-worthy error
//! the manager decides whether another round can help and how long to wait for a cooldown to
//! lapse; `request-retry` bounds the rounds, credentials may override the bound individually.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_auth::types::{AuthError, Status};
use rand::Rng;

use super::cooldown::{BlockReason, MIN_QUOTA_COOLDOWN_FLOOR, availability_block, is_auth_blocked_for_model, is_disabled};
use super::errors::{is_credential_retry_round_status, is_request_retry_round_error};
use super::models::executor_key_from_auth;
use super::pick::{Eligibility, pinned_auth_id};
use super::util::{canonical_model_key, chrono_to_std};
use super::{Manager, Metadata};
use crate::executor::ExecError;

/// Cooldown waits never extend by more than this much random jitter.
const COOLDOWN_WAIT_JITTER_CAP: Duration = Duration::from_secs(2);

fn effective_request_retry_limit(auth: &Auth, default_retry: i64) -> i64 {
    auth.request_retry_override().unwrap_or(default_retry.max(0))
}

/// Whether a credential cooling because of this last error is worth waiting for: no error means
/// only quota cooldowns are, otherwise the retry-round statuses.
fn credential_retry_round_state_eligible(last_err: Option<&AuthError>, quota_exceeded: bool) -> bool {
    match last_err {
        None => quota_exceeded,
        Some(e) => is_credential_retry_round_status(e.http_status),
    }
}

/// `(eligible, next)`: can this credential serve a later round, and when does it recover.
pub(crate) fn retry_round_availability_for_auth(auth: &Auth, model: &str, now: DateTime<Utc>) -> (bool, Option<DateTime<Utc>>) {
    let b = is_auth_blocked_for_model(auth, model, now);
    if !b.blocked {
        return (true, None);
    }
    if b.next.is_none() || b.reason == BlockReason::Disabled {
        return (false, None);
    }
    let next = b.next;
    if auth.quota.exceeded && auth.quota.reason == "credential_quota" && auth.quota.next_recover_at.is_some_and(|t| t > now) {
        return (credential_retry_round_state_eligible(auth.last_error.as_ref(), true), next);
    }
    let model_key = canonical_model_key(model);
    if !model_key.is_empty() && !auth.model_states.is_empty() {
        let mut matched_blocked = false;
        for (state_model, state) in &auth.model_states {
            if canonical_model_key(state_model) != model_key {
                continue;
            }
            if state.status == Status::Disabled {
                return (false, None);
            }
            let sb = availability_block(state.unavailable, state.quota.exceeded, state.next_retry_after, state.quota.next_recover_at, now);
            if !sb.blocked {
                continue;
            }
            matched_blocked = true;
            if sb.next.is_none() || !credential_retry_round_state_eligible(state.last_error.as_ref(), state.quota.exceeded) {
                return (false, None);
            }
        }
        if matched_blocked {
            return (true, next);
        }
    }
    if !credential_retry_round_state_eligible(auth.last_error.as_ref(), auth.quota.exceeded) {
        return (false, None);
    }
    (true, next)
}

/// Random delay added to a cooldown wait so concurrent requests do not wake in lockstep. Never
/// pushes the total past `max_wait` (when set).
pub(crate) fn jittered_cooldown_wait(wait: Duration, max_wait: Duration) -> Duration {
    if wait.is_zero() {
        return wait;
    }
    let mut range = (wait / 4).min(COOLDOWN_WAIT_JITTER_CAP);
    if !max_wait.is_zero() {
        range = range.min(max_wait.saturating_sub(wait));
    }
    if range.is_zero() {
        return wait;
    }
    let nanos = u64::try_from(range.as_nanos()).unwrap_or(u64::MAX).max(1);
    wait + Duration::from_nanos(rand::rng().random_range(0..nanos))
}

impl Manager {
    /// Credentials excluded from retry round `round` because their own limit is exhausted.
    pub(crate) fn request_retry_round_exclusions(&self, round: i64, default_retry: i64) -> HashSet<String> {
        let mut excluded = HashSet::new();
        if round <= 0 {
            return excluded;
        }
        let st = self.state.read();
        for a in st.auths.values() {
            if a.id.trim().is_empty() {
                continue;
            }
            if effective_request_retry_limit(a, default_retry) < round {
                excluded.insert(a.id.clone());
            }
        }
        excluded
    }

    fn eligible_for_retry<'a>(
        &self,
        auth: &'a Auth,
        provider_set: &[String],
        model: &str,
        pinned: &str,
        eligibility: &Eligibility,
    ) -> Option<&'a Auth> {
        if is_disabled(auth) {
            return None;
        }
        if !pinned.is_empty() && auth.id != pinned {
            return None;
        }
        if !eligibility.allows(auth) {
            return None;
        }
        let key = executor_key_from_auth(auth);
        if !provider_set.contains(&key) {
            return None;
        }
        if !model.is_empty() && !self.auth_supports_route_model(auth, model) {
            return None;
        }
        Some(auth)
    }

    fn provider_set(providers: &[String]) -> Vec<String> {
        providers.iter().map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect()
    }

    /// Smallest wait until some eligible credential can serve another round (Go:
    /// closestCooldownWaitWithAttempted). A credential that already failed this round with 429
    /// and has cooling enabled waits at least the 10s quota floor.
    pub(crate) fn closest_cooldown_wait(
        &self,
        providers: &[String],
        model: &str,
        attempt: i64,
        eligibility: &Eligibility,
        pinned: &str,
        default_retry: i64,
        status: i32,
        attempted: &HashSet<String>,
    ) -> Option<Duration> {
        if providers.is_empty() {
            return None;
        }
        let now = self.now();
        let provider_set = Self::provider_set(providers);
        let st = self.state.read();
        let mut min_wait: Option<Duration> = None;
        for auth in st.auths.values() {
            if self.eligible_for_retry(auth, &provider_set, model, pinned, eligibility).is_none() {
                continue;
            }
            if attempt >= effective_request_retry_limit(auth, default_retry) {
                continue;
            }
            let check_model = if model.trim().is_empty() { model.to_string() } else { self.selection_model_for_auth(auth, model) };
            let (eligible, next) = retry_round_availability_for_auth(auth, &check_model, now);
            if !eligible {
                continue;
            }
            let was_attempted = attempted.contains(&auth.id);
            let cooling_disabled = self.cooldown_disabled_for_auth(auth);
            if !was_attempted || cooling_disabled || status != 429 {
                let Some(next) = next else {
                    return Some(Duration::ZERO);
                };
                let wait = next - now;
                if wait < chrono::Duration::zero() {
                    continue;
                }
                let wait = chrono_to_std(wait);
                if min_wait.is_none_or(|m| wait < m) {
                    min_wait = Some(wait);
                }
                continue;
            }
            // Already attempted this round with a 429 and cooling on: never a zero-wait round.
            let wait = match next {
                None => MIN_QUOTA_COOLDOWN_FLOOR,
                Some(n) => chrono_to_std(n - now).max(MIN_QUOTA_COOLDOWN_FLOOR),
            };
            if min_wait.is_none_or(|m| wait < m) {
                min_wait = Some(wait);
            }
        }
        min_wait
    }

    /// Whether any eligible credential could still serve retry round `attempt`.
    pub(crate) fn retry_allowed(
        &self,
        attempt: i64,
        providers: &[String],
        model: &str,
        eligibility: &Eligibility,
        pinned: &str,
        default_retry: i64,
    ) -> bool {
        if attempt < 0 || providers.is_empty() {
            return false;
        }
        let provider_set = Self::provider_set(providers);
        if provider_set.is_empty() {
            return false;
        }
        let now = self.now();
        let st = self.state.read();
        st.auths.values().any(|auth| {
            if self.eligible_for_retry(auth, &provider_set, model, pinned, eligibility).is_none() {
                return false;
            }
            if attempt >= effective_request_retry_limit(auth, default_retry) {
                return false;
            }
            let check_model = if model.trim().is_empty() { model.to_string() } else { self.selection_model_for_auth(auth, model) };
            retry_round_availability_for_auth(auth, &check_model, now).0
        })
    }

    /// Decides whether another retry round should run and how long to wait first (Go:
    /// shouldRetryAfterErrorWithAttempted, non-Home). `max_wait` limits only positive cooldown
    /// waits; zero means never wait.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn should_retry_after_error(
        &self,
        err: &ExecError,
        attempt: i64,
        providers: &[String],
        model: &str,
        max_wait: Duration,
        default_retry: i64,
        attempted: &HashSet<String>,
        meta_map: &Metadata,
    ) -> (Duration, bool) {
        let status = err.status as i32;
        if status == 200 || super::errors::Failure::of_exec(err).is_request_invalid() {
            return (Duration::ZERO, false);
        }
        let eligibility = Eligibility::from_meta(meta_map);
        let pinned = pinned_auth_id(meta_map);
        if !is_request_retry_round_error(err) || !self.retry_allowed(attempt, providers, model, &eligibility, &pinned, default_retry) {
            return (Duration::ZERO, false);
        }
        if let Some(wait) = self.closest_cooldown_wait(providers, model, attempt, &eligibility, &pinned, default_retry, status, attempted) {
            if !wait.is_zero() && (max_wait.is_zero() || wait > max_wait) {
                return (Duration::ZERO, false);
            }
            return (wait, true);
        }
        if let Some(ra) = err.retry_after {
            if !ra.is_zero() && (max_wait.is_zero() || ra > max_wait) {
                return (Duration::ZERO, false);
            }
            return (ra, true);
        }
        (Duration::ZERO, true)
    }

    /// Sleeps `wait` plus jitter on the manager clock.
    pub(crate) async fn wait_for_cooldown(&self, wait: Duration, max_wait: Duration) {
        if wait.is_zero() {
            return;
        }
        let clock = self.clock.read().clone();
        clock.sleep(jittered_cooldown_wait(wait, max_wait)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    #[test]
    fn jitter_is_bounded_and_respects_max_wait() {
        let w = Duration::from_secs(20);
        for _ in 0..50 {
            let j = jittered_cooldown_wait(w, Duration::ZERO);
            assert!(j >= w && j < w + COOLDOWN_WAIT_JITTER_CAP);
            let j = jittered_cooldown_wait(w, Duration::from_secs(21));
            assert!(j >= w && j <= Duration::from_secs(21));
        }
        assert_eq!(jittered_cooldown_wait(w, w), w);
        assert_eq!(jittered_cooldown_wait(Duration::ZERO, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn unauthorized_cooldown_is_not_worth_waiting_for() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut a = Auth::new("a", "p");
        a.unavailable = true;
        a.next_retry_after = Some(now + chrono::Duration::seconds(30));
        a.last_error = Some(AuthError { http_status: 401, ..Default::default() });
        assert!(!retry_round_availability_for_auth(&a, "m", now).0);
        a.last_error = Some(AuthError { http_status: 503, ..Default::default() });
        assert_eq!(retry_round_availability_for_auth(&a, "m", now), (true, a.next_retry_after));
    }
}
