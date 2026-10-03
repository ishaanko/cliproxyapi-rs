//! 429 classification, short cooldowns and the AI-credits balance probe (Go:
//! antigravity_executor_credits.go).
//!
//! State is process-wide like Go's package-level `sync.Map`s: it is keyed by credential id and
//! shared by every executor instance. In Home mode (a Home client is installed) the credits
//! balance, short cooldowns and the refresh lock live in Home KV instead of the in-memory maps,
//! under the `cpa:antigravity:*` keys; KV failures on request paths surface as
//! `503 home kv store unavailable`.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_home::HomeError;
use cpa_home::kv::{self, hash_key_part};
use cpa_json::J;
use cpa_runtime::conductor::{
    AntigravityCreditsHint, get_antigravity_credits_hint_required, has_known_antigravity_credits_hint_async,
    set_antigravity_credits_hint_async,
};
use cpa_runtime::executor::ExecError;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
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
pub(crate) fn home_kv_unavailable_status_err(cause: Option<&HomeError>) -> ExecError {
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

/// Last probed AI-credits balance. Serialized with Go's field names (it is stored in Home KV).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CreditsBalance {
    #[serde(rename = "CreditAmount")]
    pub credit_amount: f64,
    #[serde(rename = "MinCreditAmount")]
    pub min_credit_amount: f64,
    #[serde(rename = "PaidTierID")]
    pub paid_tier_id: String,
    #[serde(rename = "Known")]
    pub known: bool,
}

/// Home KV lifetime of a stored credits balance.
const HOME_BALANCE_TTL: Duration = Duration::from_secs(30 * 60);
/// Extra Home KV lifetime of a short cooldown beyond its own duration.
const HOME_COOLDOWN_GRACE: Duration = Duration::from_secs(5);

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

pub(crate) async fn mark_credits_permanently_disabled(auth: &Auth) {
    let Some(id) = trimmed_id(auth) else { return };
    if cooling_disabled(auth, None) {
        return;
    }
    FAILURE_BY_AUTH
        .lock()
        .insert(id.clone(), FailureState { permanently_disabled: true, explicit_balance_exhausted: true });
    let balance = CreditsBalance { credit_amount: 0.0, min_credit_amount: 1.0, paid_tier_id: String::new(), known: true };
    store_credits_balance_best_effort(&id, balance).await;
    set_antigravity_credits_hint_async(
        &id,
        AntigravityCreditsHint {
            known: true,
            available: false,
            credit_amount: 0.0,
            min_credit_amount: 1.0,
            paid_tier_id: String::new(),
            updated_at: None,
        },
    )
    .await;
}

fn clear_credits_permanently_disabled(auth: &Auth) {
    clear_credits_failure_state(auth);
}

fn balance_key(auth_id: &str) -> String {
    format!("cpa:antigravity:credits-balance:{}", auth_id.trim())
}

fn refresh_lock_key(auth_id: &str) -> String {
    format!("cpa:antigravity:credits-refresh-lock:{}", auth_id.trim())
}

/// Stores the probed balance: Home KV for 30 minutes in Home mode (failures are logged), else the
/// in-memory map (Go: storeAntigravityCreditsBalanceBestEffort).
async fn store_credits_balance_best_effort(auth_id: &str, bal: CreditsBalance) {
    let auth_id = auth_id.trim();
    if auth_id.is_empty() {
        return;
    }
    let client = match kv::current_kv() {
        Ok(None) => {
            BALANCE_BY_AUTH.lock().insert(auth_id.to_string(), bal);
            return;
        }
        Ok(Some(client)) => client,
        Err(e) => {
            tracing::error!("antigravity executor: home kv best-effort credits balance set failed prefix=cpa:antigravity:*: {e}");
            return;
        }
    };
    let raw = match serde_json::to_vec(&bal) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::error!("antigravity executor: home kv best-effort credits balance set failed prefix=cpa:antigravity:*: {e}");
            return;
        }
    };
    let opts = cpa_home::KvSetOptions { ex: HOME_BALANCE_TTL, ..Default::default() };
    if let Err(e) = client.kv_set(&balance_key(auth_id), &raw, opts).await {
        tracing::error!("antigravity executor: home kv best-effort credits balance set failed prefix=cpa:antigravity:*: {e}");
    }
}

/// The conductor hint when known, else the stored balance (Home KV in Home mode), else
/// optimistic `true` (Go: antigravityAuthHasCreditsRequired).
pub(crate) async fn auth_has_credits_required(auth: &Auth) -> Result<bool, HomeError> {
    let Some(id) = trimmed_id(auth) else { return Ok(false) };
    if let Some(hint) = get_antigravity_credits_hint_required(&id).await?
        && hint.known
    {
        return Ok(hint.available);
    }
    if let Some(client) = kv::current_kv()? {
        let Some(raw) = client.kv_get(&balance_key(&id)).await? else { return Ok(true) };
        let balance: CreditsBalance = serde_json::from_slice(&raw).map_err(HomeError::other)?;
        return Ok(credits_balance_available(&id, &balance).await);
    }
    let balance = BALANCE_BY_AUTH.lock().get(&id).cloned();
    match balance {
        None => Ok(true),
        Some(b) => Ok(credits_balance_available(&id, &b).await),
    }
}

async fn credits_balance_available(auth_id: &str, bal: &CreditsBalance) -> bool {
    if !bal.known {
        return false;
    }
    let available = bal.credit_amount >= bal.min_credit_amount;
    set_antigravity_credits_hint_async(
        auth_id.trim(),
        AntigravityCreditsHint {
            known: true,
            available,
            credit_amount: bal.credit_amount,
            min_credit_amount: bal.min_credit_amount,
            paid_tier_id: bal.paid_tier_id.clone(),
            updated_at: None,
        },
    )
    .await;
    available
}

// ---------------------------------------------------------------- short cooldown

fn short_cooldown_key(auth: &Auth, model: &str) -> Option<String> {
    let id = trimmed_id(auth)?;
    let model = model.trim();
    (!model.is_empty()).then(|| format!("{id}|{model}|sc"))
}

fn short_cooldown_kv_key(auth: &Auth, model: &str) -> Option<String> {
    let id = trimmed_id(auth)?;
    let model = model.trim();
    (!model.is_empty()).then(|| format!("cpa:antigravity:short-cooldown:{id}:{}", hash_key_part(model)))
}

/// Remaining short cooldown for `(auth, model)`, if any. In Home mode the deadline (unix nanos)
/// is read from Home KV and an expired one is deleted; a KV failure is an error (Go:
/// antigravityIsInShortCooldownRequired). Local entries that expired are dropped.
pub(crate) async fn is_in_short_cooldown_required(auth: &Auth, model: &str) -> Result<Option<Duration>, HomeError> {
    if cooling_disabled(auth, None) {
        return Ok(None);
    }
    if let Some(client) = kv::current_kv()? {
        let Some(key) = short_cooldown_kv_key(auth, model) else { return Ok(None) };
        let Some(raw) = client.kv_get(&key).await? else { return Ok(None) };
        let until_nanos: i128 = String::from_utf8_lossy(&raw).trim().parse::<i64>().map_err(HomeError::other)?.into();
        let now_nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128);
        let remaining = until_nanos - now_nanos;
        if remaining <= 0 {
            client.kv_del(&[key]).await?;
            return Ok(None);
        }
        return Ok(Some(Duration::from_nanos(remaining.min(u64::MAX as i128) as u64)));
    }
    let Some(key) = short_cooldown_key(auth, model) else { return Ok(None) };
    let mut map = SHORT_COOLDOWN_BY_AUTH.lock();
    let Some(until) = map.get(&key).copied() else { return Ok(None) };
    match until.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => Ok(Some(remaining)),
        _ => {
            map.remove(&key);
            Ok(None)
        }
    }
}

/// Records a short cooldown: in Home mode the deadline goes to Home KV with the duration plus 5s
/// as expiry and any failure is an error (Go: markAntigravityShortCooldownRequired).
pub(crate) async fn mark_short_cooldown_required(auth: &Auth, model: &str, duration: Duration) -> Result<(), HomeError> {
    if cooling_disabled(auth, None) {
        return Ok(());
    }
    if let Some(client) = kv::current_kv()? {
        let Some(key) = short_cooldown_kv_key(auth, model) else { return Ok(()) };
        if duration.is_zero() {
            return Ok(());
        }
        let until = SystemTime::now() + duration;
        let nanos = until.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let opts = cpa_home::KvSetOptions { ex: duration + HOME_COOLDOWN_GRACE, ..Default::default() };
        if !client.kv_set(&key, nanos.to_string().as_bytes(), opts).await? {
            return Err(HomeError::other("home kv store unavailable"));
        }
        return Ok(());
    }
    if let Some(key) = short_cooldown_key(auth, model) {
        SHORT_COOLDOWN_BY_AUTH.lock().insert(key, Instant::now() + duration);
    }
    Ok(())
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
    /// In Home mode a 10 minute `SET NX` lock in Home KV throttles the probe across nodes instead
    /// of the local per-credential state.
    pub(crate) async fn maybe_refresh_credits_hint(&self, cfg: &Config, auth: &Auth, access_token: &str) {
        if !credits_retry_enabled(cfg) || cooling_disabled(auth, Some(cfg)) {
            return;
        }
        let Some(id) = trimmed_id(auth) else { return };
        if has_known_antigravity_credits_hint_async(&id).await {
            return;
        }
        let token = if access_token.trim().is_empty() { auth.meta_str("access_token") } else { access_token.to_string() };
        if token.trim().is_empty() {
            return;
        }
        match kv::current_kv() {
            Ok(None) => self.queue_credits_refresh(auth, &token),
            Err(e) => {
                tracing::error!("antigravity executor: home kv best-effort refresh lock failed prefix=cpa:antigravity:*: {e}");
            }
            Ok(Some(client)) => {
                match client.kv_set_nx(&refresh_lock_key(&id), b"1", CREDITS_HINT_REFRESH_INTERVAL).await {
                    Err(e) => {
                        tracing::error!(
                            "antigravity executor: home kv best-effort refresh lock failed prefix=cpa:antigravity:*: {e}"
                        );
                    }
                    Ok(false) => {}
                    Ok(true) => {
                        let this = self.clone();
                        let auth = auth.clone();
                        tokio::spawn(async move {
                            let cfg = this.cfg();
                            let probe = this.update_credits_balance(&cfg, &auth, &token, None);
                            let _ = tokio::time::timeout(CREDITS_HINT_REFRESH_TIMEOUT, probe).await;
                        });
                    }
                }
            }
        }
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

        let publish = async |balance: Option<CreditsBalance>, hint: AntigravityCreditsHint| {
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
                store_credits_balance_best_effort(&auth_id, b).await;
                set_antigravity_credits_hint_async(&auth_id, hint).await;
                if available {
                    clear_credits_permanently_disabled(auth);
                }
            } else {
                set_antigravity_credits_hint_async(&auth_id, hint).await;
            }
        };

        let v = cpa_json::parse(&bytes);
        let paid_tier_id = v.g("paidTier.id").str().trim().to_string();
        let credits = v.g("paidTier.availableCredits");
        if !credits.is_array() {
            publish(
                None,
                AntigravityCreditsHint { known: true, available: false, paid_tier_id, ..Default::default() },
            )
            .await;
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
            )
            .await;
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
