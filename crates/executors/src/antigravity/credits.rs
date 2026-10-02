//! 429 classification, short cooldowns and the AI-credits balance probe (Go:
//! antigravity_executor_credits.go).
//!
//! State is process-wide like Go's package-level `sync.Map`s: it is keyed by credential id and
//! shared by every executor instance. Home KV mode (shared state in a Home server) is not
//! supported; the in-memory paths below are the non-Home behavior.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::conductor::{AntigravityCreditsHint, set_antigravity_credits_hint};
use cpa_runtime::executor::ExecError;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::request::{load_code_assist_base_url, resolve_user_agent};
use super::AntigravityExecutor;
use crate::helps::json_retry::parse_retry_delay;

pub(crate) const CREDITS_HINT_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
pub(crate) const CREDITS_HINT_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SHORT_QUOTA_COOLDOWN_THRESHOLD: Duration = Duration::from_secs(5 * 60);
pub(crate) const INSTANT_RETRY_THRESHOLD: Duration = Duration::from_secs(3);

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// What an upstream 429 means for scheduling (Go: antigravity429DecisionKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision429Kind {
    SoftRetry,
    InstantRetrySameAuth,
    ShortCooldownSwitchAuth,
    FullQuotaExhausted,
}

#[derive(Debug, Clone)]
pub(crate) struct Decision429 {
    pub kind: Decision429Kind,
    pub retry_after: Option<Duration>,
    pub reason: String,
}

/// Classifies a 429 body: `RESOURCE_EXHAUSTED` with `QUOTA_EXHAUSTED` is a full exhaustion, while
/// `RATE_LIMIT_EXCEEDED` depends on the suggested retry delay.
pub(crate) fn decide_429(body: &[u8]) -> Decision429 {
    let mut decision = Decision429 { kind: Decision429Kind::SoftRetry, retry_after: None, reason: String::new() };
    if body.is_empty() {
        return decision;
    }
    decision.retry_after = parse_retry_delay(body);

    let v = cpa_json::parse(body);
    let status = v.g("error.status").str();
    if !status.trim().eq_ignore_ascii_case("RESOURCE_EXHAUSTED") {
        return decision;
    }

    let details = v.g("error.details");
    if details.is_array() {
        for detail in details.array() {
            if detail.g("@type").str() != ERROR_INFO_TYPE {
                continue;
            }
            let reason = detail.g("reason").str().trim().to_string();
            decision.reason = reason.clone();
            if reason.eq_ignore_ascii_case("QUOTA_EXHAUSTED") {
                decision.kind = Decision429Kind::FullQuotaExhausted;
                return decision;
            }
            if reason.eq_ignore_ascii_case("RATE_LIMIT_EXCEEDED") {
                decision.kind = match decision.retry_after {
                    None => Decision429Kind::SoftRetry,
                    Some(d) if d < INSTANT_RETRY_THRESHOLD => Decision429Kind::InstantRetrySameAuth,
                    Some(d) if d < SHORT_QUOTA_COOLDOWN_THRESHOLD => Decision429Kind::ShortCooldownSwitchAuth,
                    Some(_) => Decision429Kind::FullQuotaExhausted,
                };
                return decision;
            }
        }
    }

    let lower = String::from_utf8_lossy(body).to_lowercase();
    if ["quota_exhausted", "quota exhausted"].iter().any(|k| lower.contains(k)) {
        decision.kind = Decision429Kind::FullQuotaExhausted;
        decision.reason = "quota_exhausted".into();
        return decision;
    }
    decision.kind = Decision429Kind::SoftRetry;
    decision
}

/// Adds `"enabledCreditTypes":["GOOGLE_ONE_AI"]` to a valid JSON payload.
pub(crate) fn inject_enabled_credit_types(payload: &[u8]) -> Option<Vec<u8>> {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return None;
    }
    let mut v = cpa_json::parse(payload);
    cpa_json::set(&mut v, "enabledCreditTypes", json!(["GOOGLE_ONE_AI"]));
    Some(cpa_json::to_vec(&v))
}

/// Whether the 429 body carries `INSUFFICIENT_G1_CREDITS_BALANCE`.
pub(crate) fn has_explicit_credits_balance_exhausted_reason(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let v = cpa_json::parse(body);
    let details = v.g("error.details");
    details.is_array()
        && details.array().iter().any(|d| {
            d.g("@type").str() == ERROR_INFO_TYPE
                && d.g("reason").str().trim().eq_ignore_ascii_case("INSUFFICIENT_G1_CREDITS_BALANCE")
        })
}

/// Upstream failure as `statusErr{code, msg: body}`; a 429 also carries the parsed retry delay.
pub(crate) fn new_status_err(status: u16, body: &[u8]) -> ExecError {
    let mut err = crate::helps::status::status_err(status, String::from_utf8_lossy(body).into_owned());
    if status == 429
        && let Some(d) = parse_retry_delay(body)
    {
        err.retry_after = Some(d);
    }
    err
}

/// Go: `homeKVUnavailableStatusErr`.
#[allow(dead_code)]
pub(crate) fn home_kv_unavailable_status_err(cause: Option<&str>) -> ExecError {
    match cause {
        None => ExecError::new(503, "home kv store unavailable"),
        Some(c) => ExecError::new(503, format!("home kv store unavailable: {c}")),
    }
}

pub(crate) fn credits_retry_enabled(cfg: &Config) -> bool {
    cfg.quota_exceeded.antigravity_credits
}

/// Go: `QuotaCooldownDisabledForAuthWithConfig` (Home mode, per-auth override, global flag).
pub(crate) fn cooling_disabled(auth: &Auth, cfg: Option<&Config>) -> bool {
    if cfg.is_some_and(|c| c.home.enabled) {
        return true;
    }
    if let Some(over) = auth.disable_cooling_override() {
        return over;
    }
    cfg.is_some_and(|c| c.disable_cooling)
}

// ---------------------------------------------------------------- failure and balance state

#[derive(Debug, Clone, Copy, Default)]
struct FailureState {
    #[allow(dead_code)]
    permanently_disabled: bool,
    #[allow(dead_code)]
    explicit_balance_exhausted: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CreditsBalance {
    pub credit_amount: f64,
    pub min_credit_amount: f64,
    pub paid_tier_id: String,
    pub known: bool,
}

static FAILURE_BY_AUTH: LazyLock<Mutex<HashMap<String, FailureState>>> = LazyLock::new(Default::default);
static SHORT_COOLDOWN_BY_AUTH: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
static BALANCE_BY_AUTH: LazyLock<Mutex<HashMap<String, CreditsBalance>>> = LazyLock::new(Default::default);
static HINT_REFRESH_BY_ID: LazyLock<Mutex<HashMap<String, HintRefreshState>>> = LazyLock::new(Default::default);

fn trimmed_id(auth: &Auth) -> Option<String> {
    let id = auth.id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

pub(crate) fn clear_credits_failure_state(auth: &Auth) {
    if let Some(id) = trimmed_id(auth) {
        FAILURE_BY_AUTH.lock().remove(&id);
    }
}

pub(crate) fn mark_credits_permanently_disabled(auth: &Auth) {
    let Some(id) = trimmed_id(auth) else { return };
    if cooling_disabled(auth, None) {
        return;
    }
    FAILURE_BY_AUTH
        .lock()
        .insert(id.clone(), FailureState { permanently_disabled: true, explicit_balance_exhausted: true });
    BALANCE_BY_AUTH.lock().insert(
        id.clone(),
        CreditsBalance { credit_amount: 0.0, min_credit_amount: 1.0, paid_tier_id: String::new(), known: true },
    );
    set_antigravity_credits_hint(
        &id,
        AntigravityCreditsHint {
            known: true,
            available: false,
            credit_amount: 0.0,
            min_credit_amount: 1.0,
            paid_tier_id: String::new(),
            updated_at: None,
        },
    );
}

fn clear_credits_permanently_disabled(auth: &Auth) {
    clear_credits_failure_state(auth);
}

/// Whether the credential has AI credits: the conductor hint when known, else the stored
/// balance, else optimistic `true` (Go: antigravityAuthHasCredits).
#[allow(dead_code)]
pub(crate) fn auth_has_credits(auth: &Auth) -> bool {
    let Some(id) = trimmed_id(auth) else { return false };
    if let Some(hint) = cpa_runtime::conductor::antigravity_credits_hint(&id)
        && hint.known
    {
        return hint.available;
    }
    let balance = BALANCE_BY_AUTH.lock().get(&id).cloned();
    match balance {
        None => true,
        Some(b) => credits_balance_available(&id, &b),
    }
}

fn credits_balance_available(auth_id: &str, bal: &CreditsBalance) -> bool {
    if !bal.known {
        return false;
    }
    let available = bal.credit_amount >= bal.min_credit_amount;
    set_antigravity_credits_hint(
        auth_id.trim(),
        AntigravityCreditsHint {
            known: true,
            available,
            credit_amount: bal.credit_amount,
            min_credit_amount: bal.min_credit_amount,
            paid_tier_id: bal.paid_tier_id.clone(),
            updated_at: None,
        },
    );
    available
}

// ---------------------------------------------------------------- short cooldown

fn short_cooldown_key(auth: &Auth, model: &str) -> Option<String> {
    let id = trimmed_id(auth)?;
    let model = model.trim();
    (!model.is_empty()).then(|| format!("{id}|{model}|sc"))
}

/// Remaining short cooldown for `(auth, model)`, if any; expired entries are dropped.
pub(crate) fn is_in_short_cooldown(auth: &Auth, model: &str, now: Instant) -> Option<Duration> {
    if cooling_disabled(auth, None) {
        return None;
    }
    let key = short_cooldown_key(auth, model)?;
    let mut map = SHORT_COOLDOWN_BY_AUTH.lock();
    let until = *map.get(&key)?;
    match until.checked_duration_since(now) {
        Some(remaining) if !remaining.is_zero() => Some(remaining),
        _ => {
            map.remove(&key);
            None
        }
    }
}

pub(crate) fn mark_short_cooldown(auth: &Auth, model: &str, now: Instant, duration: Duration) {
    if cooling_disabled(auth, None) {
        return;
    }
    if let Some(key) = short_cooldown_key(auth, model) {
        SHORT_COOLDOWN_BY_AUTH.lock().insert(key, now + duration);
    }
}

/// The credits fallback ignores short cooldowns (Go: antigravityShouldBypassShortCooldown).
pub(crate) fn should_bypass_short_cooldown(credits_requested: bool, cfg: &Config) -> bool {
    credits_requested && credits_retry_enabled(cfg)
}

// ---------------------------------------------------------------- balance probe

struct HintRefreshState {
    registration_epoch: u64,
    last_attempt: Option<Instant>,
    task: Option<RefreshTask>,
}

struct RefreshTask {
    id: u64,
    handle: tokio::task::AbortHandle,
}

static TASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl AntigravityExecutor {
    /// Opportunistic hint refresh while a warm token is reused: only when the feature is on, the
    /// hint is still unknown and nothing is in flight (Go: maybeRefreshAntigravityCreditsHint).
    pub(crate) fn maybe_refresh_credits_hint(&self, cfg: &Config, auth: &Auth, access_token: &str) {
        if !credits_retry_enabled(cfg) || cooling_disabled(auth, Some(cfg)) {
            return;
        }
        let Some(id) = trimmed_id(auth) else { return };
        if cpa_runtime::conductor::has_known_antigravity_credits_hint(&id) {
            return;
        }
        let token = if access_token.trim().is_empty() { auth.meta_str("access_token") } else { access_token.to_string() };
        if token.trim().is_empty() {
            return;
        }
        self.queue_credits_refresh(auth, &token);
    }

    /// Starts one background balance probe per credential: deduplicated while one runs, throttled
    /// to one per [`CREDITS_HINT_REFRESH_INTERVAL`], and dropped when the credential registration
    /// was replaced by a newer epoch (Go: queueAntigravityCreditsRefresh).
    pub(crate) fn queue_credits_refresh(&self, auth: &Auth, access_token: &str) {
        let Some(id) = trimmed_id(auth) else { return };
        if access_token.trim().is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let now = Instant::now();
        let mut map = HINT_REFRESH_BY_ID.lock();
        let state = map.entry(id.clone()).or_insert_with(|| HintRefreshState {
            registration_epoch: 0,
            last_attempt: None,
            task: None,
        });
        if auth.registration_epoch < state.registration_epoch {
            return;
        }
        if auth.registration_epoch != state.registration_epoch {
            // A newer registration replaces any older probe and restarts the throttle.
            state.registration_epoch = auth.registration_epoch;
            state.last_attempt = None;
            if let Some(old) = state.task.take() {
                old.handle.abort();
            }
        } else if let Some(old) = &state.task {
            if !old.handle.is_finished() {
                return;
            }
            state.task = None;
        }
        if state.last_attempt.is_some_and(|t| now.duration_since(t) < CREDITS_HINT_REFRESH_INTERVAL) {
            return;
        }
        let task_id = TASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.last_attempt = Some(now);
        let this = self.clone();
        let auth_copy = auth.clone();
        let token = access_token.to_string();
        let handle = runtime.spawn(async move {
            let cfg = this.cfg();
            let probe = this.update_credits_balance(&cfg, &auth_copy, &token, Some(task_id));
            let _ = tokio::time::timeout(CREDITS_HINT_REFRESH_TIMEOUT, probe).await;
            let mut map = HINT_REFRESH_BY_ID.lock();
            if let Some(state) = map.get_mut(auth_copy.id.trim())
                && state.task.as_ref().is_some_and(|t| t.id == task_id)
            {
                state.task = None;
            }
        });
        state.task = Some(RefreshTask { id: task_id, handle: handle.abort_handle() });
    }

    /// Probes `loadCodeAssist` for the AI-credits balance and publishes the hint (Go:
    /// updateAntigravityCreditsBalanceForTask). `task` guards against a superseded probe
    /// overwriting a newer one.
    pub(crate) async fn update_credits_balance(&self, cfg: &Config, auth: &Auth, access_token: &str, task: Option<u64>) {
        let Some(auth_id) = trimmed_id(auth) else { return };
        let token = if access_token.trim().is_empty() { auth.meta_str("access_token") } else { access_token.trim().to_string() };
        if token.is_empty() {
            return;
        }
        let body = json!({"metadata": {"ideType": "ANTIGRAVITY"}});
        let url = format!("{}/v1internal:loadCodeAssist", load_code_assist_base_url(auth).trim_end_matches('/'));
        let client = self.client(cfg, auth, "");
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .header("User-Agent", resolve_user_agent(auth))
            .body(serde_json::to_vec(&body).unwrap_or_default())
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("antigravity executor: loadCodeAssist request error: {}", err.without_url());
                return;
            }
        };
        let status = resp.status().as_u16();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(err) => {
                tracing::debug!("antigravity executor: loadCodeAssist returned status {status}, err={}", err.without_url());
                return;
            }
        };
        if !(200..300).contains(&status) {
            tracing::debug!("antigravity executor: loadCodeAssist returned status {status}");
            return;
        }

        let publish = |balance: Option<CreditsBalance>, hint: AntigravityCreditsHint| {
            if let Some(task_id) = task {
                let map = HINT_REFRESH_BY_ID.lock();
                let current = map.get(&auth_id).is_some_and(|s| {
                    s.task.as_ref().is_some_and(|t| t.id == task_id) && s.registration_epoch == auth.registration_epoch
                });
                if !current {
                    return;
                }
            }
            let available = hint.available;
            if let Some(b) = balance {
                BALANCE_BY_AUTH.lock().insert(auth_id.clone(), b);
                set_antigravity_credits_hint(&auth_id, hint);
                if available {
                    clear_credits_permanently_disabled(auth);
                }
            } else {
                set_antigravity_credits_hint(&auth_id, hint);
            }
        };

        let v = cpa_json::parse(&bytes);
        let paid_tier_id = v.g("paidTier.id").str().trim().to_string();
        let credits = v.g("paidTier.availableCredits");
        if !credits.is_array() {
            publish(
                None,
                AntigravityCreditsHint { known: true, available: false, paid_tier_id, ..Default::default() },
            );
            return;
        }
        for credit in credits.array() {
            if !credit.g("creditType").str().eq_ignore_ascii_case("GOOGLE_ONE_AI") {
                continue;
            }
            let Ok(credit_amount) = credit.g("creditAmount").str().trim().parse::<f64>() else { continue };
            let Ok(min_amount) = credit.g("minimumCreditAmountForUsage").str().trim().parse::<f64>() else {
                continue;
            };
            let bal = CreditsBalance {
                credit_amount,
                min_credit_amount: min_amount,
                paid_tier_id: paid_tier_id.clone(),
                known: true,
            };
            publish(
                Some(bal),
                AntigravityCreditsHint {
                    known: true,
                    available: credit_amount >= min_amount,
                    credit_amount,
                    min_credit_amount: min_amount,
                    paid_tier_id: paid_tier_id.clone(),
                    updated_at: None,
                },
            );
            return;
        }
    }
}

/// Reads a float from auth metadata (numbers or numeric strings), like Go's `parseMetaFloat`.
#[allow(dead_code)]
pub(crate) fn parse_meta_float(metadata: &HashMap<String, Value>, key: &str) -> Option<f64> {
    match metadata.get(key)? {
        Value::Number(n) => n.to_string().parse().ok(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
