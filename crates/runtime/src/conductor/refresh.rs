//! Token refresh: 401 recovery during a request, forced refresh, request-time credential
//! preparation and the background auto-refresh loop (Go: conductor_refresh.go,
//! auto_refresh_loop.go).
//!
//! A per-credential async lock serializes request-triggered and background refreshes so one
//! refresh token is never used twice concurrently; a caller that finds the access token already
//! replaced reuses the result instead of refreshing again.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_auth::types::{AUTH_KIND_API_KEY, Status, parse_time_value};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

use super::cooldown::{
    REFRESH_FAILURE_BACKOFF, REFRESH_INEFFECTIVE_BACKOFF, REFRESH_PENDING_BACKOFF,
    clear_unauthorized_model_states, has_disabled_invalid_grant_failure,
    has_unauthorized_auth_failure, invalid_grant_backoff_duration, is_disabled,
};
use super::errors::{Failure, refresh_error_from_error};
use super::models::executor_key_from_auth;
use super::util::{add_chrono, chrono_to_std, to_chrono};
use super::{Manager, executor_locked};
use crate::executor::{DynExecutor, ExecError};

/// Upper bound for operator-supplied intervals (100 years); keeps time math from overflowing.
const MAX_INTERVAL: Duration = Duration::from_secs(100 * 365 * 24 * 3600);
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const REFRESH_MAX_CONCURRENCY: usize = 16;
/// Cap on the loop's sleep so the loop re-evaluates promptly after a suspend/resume.
const MAX_REFRESH_TIMER_WAIT: Duration = Duration::from_secs(30);

/// Outcome of a forced refresh for one credential (management API).
#[derive(Debug, Clone, Serialize)]
pub struct ForceRefreshResult {
    pub id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Default)]
pub(crate) struct RefreshState {
    handle: Option<JoinHandle<()>>,
    shared: Option<Arc<LoopShared>>,
    /// Queued/running jobs by credential id: `(registration_epoch, pending_until, running)`.
    jobs: HashMap<String, JobInfo>,
}

#[derive(Clone, Copy)]
struct JobInfo {
    epoch: u64,
    pending_until: DateTime<Utc>,
    running: bool,
}

struct RefreshPool {
    queue: Arc<Semaphore>,
    workers: Arc<Semaphore>,
}

/// Ends a refresh job on drop (normal completion or panic).
struct JobGuard {
    manager: Manager,
    id: String,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.manager.finish_refresh_job(&self.id, None);
    }
}

struct LoopShared {
    dirty: Mutex<HashSet<String>>,
    removed: Mutex<HashSet<String>>,
    notify: Notify,
}

fn parse_duration_string(raw: &str) -> Duration {
    let s = raw.trim();
    if s.is_empty() {
        return Duration::ZERO;
    }
    if let Ok(d) = cpa_config::GoDuration::parse(s)
        && d.0 > 0
    {
        return d.to_std();
    }
    match s.parse::<f64>() {
        Ok(secs) if secs > 0.0 => {
            Duration::try_from_secs_f64(secs).map_or(MAX_INTERVAL, |d| d.min(MAX_INTERVAL))
        }
        _ => Duration::ZERO,
    }
}

fn parse_duration_value(v: &Value) -> Duration {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f > 0.0 => {
                Duration::try_from_secs_f64(f).map_or(MAX_INTERVAL, |d| d.min(MAX_INTERVAL))
            }
            _ => Duration::ZERO,
        },
        Value::String(s) => parse_duration_string(s),
        _ => Duration::ZERO,
    }
}

const INTERVAL_KEYS: [&str; 4] = [
    "refresh_interval_seconds",
    "refreshIntervalSeconds",
    "refresh_interval",
    "refreshInterval",
];

/// Credential-specific refresh interval from metadata or attributes (number = seconds, or a Go
/// duration string).
fn auth_preferred_interval(auth: &Auth) -> Duration {
    for key in INTERVAL_KEYS {
        if let Some(v) = auth.metadata.get(key) {
            let d = parse_duration_value(v);
            if !d.is_zero() {
                return d;
            }
        }
    }
    for key in INTERVAL_KEYS {
        if let Some(v) = auth.attributes.get(key) {
            let d = parse_duration_string(v);
            if !d.is_zero() {
                return d;
            }
        }
    }
    Duration::ZERO
}

fn auth_last_refresh_timestamp(auth: &Auth) -> Option<DateTime<Utc>> {
    const KEYS: [&str; 4] = [
        "last_refresh",
        "lastRefresh",
        "last_refreshed_at",
        "lastRefreshedAt",
    ];
    for key in KEYS {
        if let Some(v) = auth.metadata.get(key)
            && let Some(t) = parse_time_value(v)
        {
            return Some(t);
        }
    }
    for key in KEYS {
        if let Some(v) = auth.attributes.get(key)
            && let Some(t) = parse_time_value(&Value::String(v.trim().to_string()))
        {
            return Some(t);
        }
    }
    None
}

/// The 401 error recorded on a credential whose access and refresh tokens are both dead.
fn terminal_unauthorized_error(err: &ExecError) -> cpa_auth::types::AuthError {
    cpa_auth::types::AuthError {
        code: "unauthorized".into(),
        message: err.message.clone(),
        retryable: false,
        http_status: 401,
    }
}

fn auth_has_refresh_credential(auth: &Auth) -> bool {
    if !auth.refresh_token().is_empty() {
        return true;
    }
    // Meta exchanges its device token for a replacement API key after a 401.
    auth.provider.trim().eq_ignore_ascii_case("meta")
        && (!auth.meta_str("dca_token").is_empty() || !auth.attr("dca_token").is_empty())
}

/// Whether the credential is due for a refresh (Go: shouldRefresh). Providers without a refresh
/// lead (API keys, meta, devin, ...) never refresh in the background.
pub(crate) fn should_refresh(auth: &Auth, now: DateTime<Utc>) -> bool {
    if has_unauthorized_auth_failure(auth) || has_disabled_invalid_grant_failure(auth) {
        return false;
    }
    if auth.next_refresh_after.is_some_and(|t| now < t) {
        return false;
    }
    let last_refresh = auth
        .last_refreshed_at
        .or_else(|| auth_last_refresh_timestamp(auth));
    let expiry = auth.expiration_time();
    let interval = auth_preferred_interval(auth);
    if !interval.is_zero() {
        let interval = to_chrono(interval);
        if let Some(exp) = expiry {
            if exp <= now || exp - now <= interval {
                return true;
            }
        }
        return match last_refresh {
            None => true,
            Some(l) => now - l >= interval,
        };
    }
    let Some(lead) = cpa_auth::refresh::provider_refresh_lead(&auth.provider.to_lowercase()) else {
        return false;
    };
    let lead = to_chrono(lead);
    if lead <= chrono::Duration::zero() {
        return expiry.is_some_and(|exp| now > exp);
    }
    if let Some(exp) = expiry {
        return exp - now <= lead;
    }
    match last_refresh {
        Some(l) => now - l >= lead,
        None => true,
    }
}

/// When the loop should look at this credential next (Go: nextRefreshCheckAt).
fn next_refresh_check_at(now: DateTime<Utc>, auth: &Auth) -> Option<DateTime<Utc>> {
    if has_unauthorized_auth_failure(auth) || has_disabled_invalid_grant_failure(auth) {
        return None;
    }
    if auth.auth_kind() == AUTH_KIND_API_KEY {
        return None;
    }
    if let Some(next) = auth.next_refresh_after
        && now < next
    {
        return Some(next);
    }
    let last_refresh = auth
        .last_refreshed_at
        .or_else(|| auth_last_refresh_timestamp(auth));
    let expiry = auth.expiration_time();
    let pref = auth_preferred_interval(auth);
    if !pref.is_zero() {
        let pref = to_chrono(pref);
        let mut candidates = Vec::new();
        if let Some(exp) = expiry {
            if exp <= now || exp - now <= pref {
                return Some(now);
            }
            candidates.push(add_chrono(exp, -pref));
        }
        let Some(l) = last_refresh else {
            return Some(now);
        };
        candidates.push(add_chrono(l, pref));
        let next = candidates.into_iter().min()?;
        return Some(if next <= now { now } else { next });
    }
    let lead = cpa_auth::refresh::provider_refresh_lead(&auth.provider.to_lowercase())?;
    let lead = to_chrono(lead);
    if let Some(exp) = expiry {
        let due = add_chrono(exp, -lead);
        return Some(if due <= now { now } else { due });
    }
    if let Some(l) = last_refresh {
        let due = add_chrono(l, lead);
        return Some(if due <= now { now } else { due });
    }
    Some(now)
}

impl Manager {
    // ---- 401 recovery, forced refresh, preparation ----

    /// Refreshes local OAuth credentials once after a 401 so the same credential can be retried
    /// before failing over (Go: tryRefreshAfterUnauthorized).
    pub(crate) async fn try_refresh_after_unauthorized(
        &self,
        auth: &Auth,
        err: &ExecError,
        already_tried: bool,
    ) -> Option<Auth> {
        if already_tried || err.is_request_scoped() {
            return None;
        }
        if !Failure::of_exec(err).is_unauthorized() || !auth_has_refresh_credential(auth) {
            return None;
        }
        if has_unauthorized_auth_failure(auth) {
            return None;
        }
        // The refresh itself is rare and large; keep it out of the callers' future size.
        Box::pin(self.refresh_after_unauthorized(auth)).await
    }

    async fn refresh_after_unauthorized(&self, auth: &Auth) -> Option<Auth> {
        tracing::debug!(
            "unauthorized response for {} ({}), refreshing credentials before fallback",
            auth.provider,
            auth.id
        );
        match self
            .refresh_auth_for_request(&auth.id, &auth.access_token())
            .await
        {
            Ok(refreshed) => Some(refreshed),
            Err(e) => {
                tracing::debug!(
                    "credential refresh before fallback failed for {} ({}): {e}",
                    auth.provider,
                    auth.id
                );
                None
            }
        }
    }

    fn refresh_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks
            .lock()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    /// Synchronous refresh of one credential. `failed_access_token` lets concurrent callers reuse
    /// a refresh that already replaced the token that produced the 401.
    pub async fn refresh_auth_for_request(
        &self,
        id: &str,
        failed_access_token: &str,
    ) -> Result<Auth, ExecError> {
        self.refresh_auth_at_epoch(id, failed_access_token, 0, false).await
    }

    pub(crate) async fn refresh_auth_at_epoch(
        &self,
        id: &str,
        failed_access_token: &str,
        epoch: u64,
        force: bool,
    ) -> Result<Auth, ExecError> {
        let id = id.trim();
        let plain = |m: &str| {
            let mut e = ExecError::new(0, m);
            e.upstream_attempted = false;
            e
        };
        if id.is_empty() {
            return Err(plain("auth id is empty"));
        }
        let lock = self.refresh_lock(id);
        let _guard = lock.lock().await;

        let (auth, exec) = {
            let st = self.state.read();
            let auth = st.auths.get(id).cloned();
            let exec = auth
                .as_ref()
                .and_then(|a| executor_locked(&st, &executor_key_from_auth(a)));
            (auth, exec)
        };
        let (Some(auth), Some(exec)) = (auth, exec) else {
            return Err(plain("auth or executor not found"));
        };
        if epoch != 0 && auth.registration_epoch != epoch {
            return Err(plain("auth registration changed before refresh"));
        }
        if has_disabled_invalid_grant_failure(&auth) && !force {
            return Err(plain("auth is disabled with invalid grant"));
        }
        if has_unauthorized_auth_failure(&auth) && !force {
            return Err(plain("auth is unauthorized"));
        }
        if !failed_access_token.is_empty() {
            let current = auth.access_token();
            if !current.is_empty() && current != failed_access_token {
                return Ok(auth);
            }
        }

        let base = auth.clone();
        let updated = exec.refresh(&base).await;
        let now = self.now();
        match updated {
            Err(err) => {
                self.apply_refresh_failure(id, &base, &err, now, failed_access_token, force);
                Err(err)
            }
            Ok(mut updated) => {
                if updated.runtime.is_none() {
                    updated.runtime = auth.runtime.clone();
                }
                updated.last_refreshed_at = Some(now);
                updated.next_refresh_after = None;
                updated.last_error = None;
                updated.status_message.clear();
                updated.unavailable = false;
                updated.refresh_failures = 0;
                if matches!(updated.status, Status::Error | Status::Unknown) {
                    updated.status = Status::Active;
                }
                updated.updated_at = Some(now);
                clear_unauthorized_model_states(&mut updated, now);
                if should_refresh(&updated, now) {
                    updated.next_refresh_after = Some(
                        now + chrono::Duration::from_std(REFRESH_INEFFECTIVE_BACKOFF)
                            .unwrap_or_default(),
                    );
                }
                let saved = self
                    .update_refreshed_auth(&base, updated)
                    .await
                    .map_err(|e| {
                        tracing::warn!(
                            "persist refreshed auth {} ({}) failed: {e}",
                            auth.provider,
                            auth.id
                        );
                        e
                    })?;
                let Some(saved) = saved else {
                    return Err(plain(&format!("auth {id} not found")));
                };
                self.apply_projections_pub(&saved, now);
                Ok(saved)
            }
        }
    }

    fn apply_refresh_failure(
        &self,
        id: &str,
        base: &Auth,
        err: &ExecError,
        now: DateTime<Utc>,
        failed_access_token: &str,
        force: bool,
    ) {
        let f = Failure::of_exec(err);
        let unauthorized = f.is_unauthorized();
        let invalid_grant = f.is_invalid_grant();
        let mut reschedule = false;
        let mut should_unschedule = false;
        {
            let mut st = self.state.write();
            let Some(current) = st.auths.get_mut(id) else {
                return;
            };
            if current.registration_epoch != base.registration_epoch {
                return;
            }
            let was_terminal_unauthorized = has_unauthorized_auth_failure(current);
            if was_terminal_unauthorized && !force {
                return;
            }
            if was_terminal_unauthorized {
                // A forced refresh of a terminal unauthorized credential keeps it blocked.
                current.generation += 1;
                current.updated_at = Some(now);
                current.unavailable = true;
                current.status = Status::Error;
                current.next_refresh_after = None;
                current.next_retry_after = None;
                if unauthorized || invalid_grant {
                    current.last_error = Some(terminal_unauthorized_error(err));
                    current.status_message = "unauthorized (refresh token invalid)".into();
                }
                drop(st);
                self.queue_refresh_unschedule(id);
                return;
            }
            current.generation += 1;
            current.updated_at = Some(now);
            current.last_error = Some(refresh_error_from_error(err));
            let disabled = is_disabled(current);
            let has_valid_token = current.has_valid_access_token(now);
            // The failed token is set only when upstream rejected this exact access token; its
            // expiry no longer proves it is usable.
            let access_token_rejected =
                !failed_access_token.is_empty() && current.access_token() == failed_access_token;
            let failure_backoff =
                chrono::Duration::from_std(REFRESH_FAILURE_BACKOFF).unwrap_or_default();
            if disabled && invalid_grant {
                current.unavailable = true;
                current.status = Status::Disabled;
                current.next_refresh_after = None;
                current.refresh_failures = 0;
                current.status_message = "disabled (invalid grant)".into();
                should_unschedule = true;
            } else if disabled {
                current.unavailable = true;
                current.status = Status::Disabled;
                current.next_refresh_after = Some(now + failure_backoff);
                if current.status_message.is_empty() {
                    current.status_message = "disabled".into();
                }
                reschedule = true;
            } else if access_token_rejected && invalid_grant {
                // Neither token can recover without a new login: stop selecting the credential
                // until its tokens change.
                current.unavailable = true;
                current.status = Status::Error;
                current.next_refresh_after = None;
                current.next_retry_after = None;
                current.refresh_failures = 0;
                current.last_error = Some(terminal_unauthorized_error(err));
                current.status_message = "unauthorized (refresh token invalid)".into();
                should_unschedule = true;
            } else if !has_valid_token {
                current.unavailable = true;
                current.status = Status::Error;
                if unauthorized {
                    current.next_refresh_after = None;
                    current.next_retry_after = None;
                    current.refresh_failures = 0;
                    current.status_message = "unauthorized".into();
                } else if invalid_grant {
                    current.refresh_failures += 1;
                    current.next_refresh_after = Some(
                        now + chrono::Duration::from_std(invalid_grant_backoff_duration(
                            current.refresh_failures,
                        ))
                        .unwrap_or_default(),
                    );
                    current.status_message = "invalid grant (retrying)".into();
                    reschedule = true;
                } else {
                    current.refresh_failures = 0;
                    current.next_refresh_after = Some(now + failure_backoff);
                    current.status_message = "token expired".into();
                    reschedule = true;
                }
            } else {
                // The access token is still valid: keep status, retry later, never past expiry.
                let mut next_retry = now + failure_backoff;
                if invalid_grant {
                    current.refresh_failures += 1;
                    next_retry = now
                        + chrono::Duration::from_std(invalid_grant_backoff_duration(
                            current.refresh_failures,
                        ))
                        .unwrap_or_default();
                } else {
                    current.refresh_failures = 0;
                }
                if let Some(exp) = current.access_token_expiration_time()
                    && next_retry > exp
                {
                    next_retry = exp;
                }
                current.next_refresh_after = Some(next_retry);
                reschedule = true;
                if !current.unavailable {
                    tracing::warn!(
                        "credential refresh failed for {} ({}): {}; retaining active credential as access token is unexpired",
                        current.provider,
                        current.id,
                        super::errors::sanitize_upstream_error_summary(&err.message)
                    );
                }
            }
        }
        if reschedule {
            self.queue_refresh_reschedule(id);
        } else if should_unschedule {
            self.queue_refresh_unschedule(id);
        }
    }

    pub(crate) fn apply_projections_pub(&self, snapshot: &Auth, now: DateTime<Utc>) {
        let (models, epoch) = self.registry.get_models_and_epoch_for_client(&snapshot.id);
        let projections: Vec<_> = models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| self.client_model_projection_for_auth(snapshot, &m.id, now))
            .collect();
        if !projections.is_empty() {
            self.registry.apply_client_model_projections(
                &snapshot.id,
                epoch,
                snapshot.generation,
                &projections,
            );
        }
    }

    /// Immediate synchronous refresh for the management API (Go: ForceRefreshAuth).
    pub async fn force_refresh_auth(&self, id: &str) -> Result<Auth, ExecError> {
        self.refresh_auth_at_epoch(id, "", 0, true).await
    }

    /// Refreshes every credential that has refresh credentials, with bounded concurrency (Go:
    /// ForceRefreshAll).
    pub async fn force_refresh_all(&self) -> Vec<ForceRefreshResult> {
        let ids: Vec<String> = {
            let st = self.state.read();
            let mut ids: Vec<String> = st
                .auths
                .values()
                .filter(|a| !a.disabled && auth_has_refresh_credential(a))
                .map(|a| a.id.clone())
                .collect();
            ids.sort();
            ids
        };
        let workers = self.refresh_workers().min(ids.len().max(1));
        let sem = Arc::new(Semaphore::new(workers));
        let mut handles = Vec::with_capacity(ids.len());
        for id in ids {
            let this = self.clone();
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let res = this.force_refresh_auth(&id).await;
                ForceRefreshResult {
                    id,
                    success: res.is_ok(),
                    error: res.err().map(|e| e.message).unwrap_or_default(),
                }
            }));
        }
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            if let Ok(r) = h.await {
                out.push(r);
            }
        }
        out
    }

    fn refresh_workers(&self) -> usize {
        let n = self.cfg().auth_auto_refresh_workers;
        if n > 0 {
            n as usize
        } else {
            REFRESH_MAX_CONCURRENCY
        }
    }

    /// Runs the executor's request-time credential preparation under a per-credential lock and
    /// merges/persists the result (Go: prepareRequestAuth / PrepareRequestAuth).
    pub(crate) async fn prepare_request_auth(
        &self,
        executor: &DynExecutor,
        auth: &Auth,
    ) -> Result<Auth, ExecError> {
        if !executor.should_prepare_request_auth(auth) {
            return Ok(auth.clone());
        }
        Box::pin(self.prepare_request_auth_slow(executor, auth)).await
    }

    async fn prepare_request_auth_slow(
        &self,
        executor: &DynExecutor,
        auth: &Auth,
    ) -> Result<Auth, ExecError> {
        let id = auth.id.trim().to_string();
        if id.is_empty() {
            return Ok(executor
                .prepare_request_auth(auth)
                .await?
                .unwrap_or_else(|| auth.clone()));
        }
        let is_meta = auth.provider.trim().eq_ignore_ascii_case("meta");
        // Meta also mints on 401 recovery: share the refresh lock.
        let lock = self.refresh_lock(&if is_meta {
            id.clone()
        } else {
            format!("prepare:{id}")
        });
        let _guard = lock.lock().await;
        let current = self.state.read().auths.get(&id).cloned();
        if current.is_none() && is_meta {
            let mut e = ExecError::new(0, "prepare meta auth: credential no longer registered");
            e.upstream_attempted = false;
            return Err(e);
        }
        let target = current.unwrap_or_else(|| auth.clone());
        if !executor.should_prepare_request_auth(&target) {
            return Ok(target);
        }
        let base = target.clone();
        let Some(updated) = executor.prepare_request_auth(&base).await? else {
            return Ok(target);
        };
        match self.update_prepared_auth(&base, updated).await? {
            Some(saved) => Ok(saved),
            None if is_meta => {
                let mut e = ExecError::new(0, "prepare meta auth: credential removed during mint");
                e.upstream_attempted = false;
                Err(e)
            }
            None => Ok(target),
        }
    }

    // ---- Auto-refresh loop ----

    /// Starts the background loop that refreshes credentials ahead of expiry. Replaces a running
    /// loop. `interval` is the re-check delay for credentials whose refresh could not start
    /// (missing executor, full queue); 0 uses 5s.
    pub fn start_auto_refresh(&self, interval: Duration) {
        let interval = if interval.is_zero() {
            REFRESH_CHECK_INTERVAL
        } else {
            interval
        };
        let shared = Arc::new(LoopShared {
            dirty: Mutex::new(HashSet::new()),
            removed: Mutex::new(HashSet::new()),
            notify: Notify::new(),
        });
        let this = self.clone();
        let sh = shared.clone();
        // Replace the old loop and install the new one under a single lock so a concurrent
        // start/stop can never leave a handle without its shared state (or the reverse).
        let mut rs = self.refresh_state.lock();
        if let Some(old) = rs.handle.take() {
            old.abort();
        }
        rs.handle = Some(tokio::spawn(async move {
            this.auto_refresh_loop(sh, interval).await
        }));
        rs.shared = Some(shared);
    }

    /// Stops the loop (running refreshes finish).
    pub fn stop_auto_refresh(&self) {
        let mut rs = self.refresh_state.lock();
        if let Some(h) = rs.handle.take() {
            h.abort();
        }
        rs.shared = None;
    }

    pub(crate) fn queue_refresh_reschedule(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        if let Some(sh) = self.refresh_state.lock().shared.clone() {
            sh.dirty.lock().insert(id.to_string());
            sh.notify.notify_one();
        }
    }

    pub(crate) fn queue_refresh_unschedule(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        if let Some(sh) = self.refresh_state.lock().shared.clone() {
            sh.removed.lock().insert(id.to_string());
            sh.notify.notify_one();
        }
    }

    async fn auto_refresh_loop(&self, shared: Arc<LoopShared>, interval: Duration) {
        let workers = self.refresh_workers();
        let pool = RefreshPool {
            // Queued + running jobs; further due credentials are re-checked after `interval`.
            queue: Arc::new(Semaphore::new((workers * 4).max(64))),
            workers: Arc::new(Semaphore::new(workers)),
        };
        let mut queue: BinaryHeap<Reverse<(DateTime<Utc>, String)>> = BinaryHeap::new();
        let mut scheduled: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
        let now = self.now();
        for (id, next) in self.refresh_schedule_snapshot(now) {
            queue.push(Reverse((next, id.clone())));
            scheduled.insert(id, next);
        }
        loop {
            let now = self.now();
            // Drop stale heap entries (superseded or removed).
            while let Some(Reverse((t, id))) = queue.peek() {
                if scheduled.get(id) == Some(t) {
                    break;
                }
                queue.pop();
            }
            let wait = match queue.peek() {
                Some(Reverse((t, _))) => chrono_to_std(*t - now).min(MAX_REFRESH_TIMER_WAIT),
                None => Duration::from_secs(3600),
            };
            tokio::select! {
                _ = shared.notify.notified() => {}
                _ = tokio::time::sleep(wait) => {
                    let now = self.now();
                    let mut due = Vec::new();
                    while let Some(Reverse((t, id))) = queue.peek().cloned() {
                        if t > now {
                            break;
                        }
                        queue.pop();
                        if scheduled.get(&id) == Some(&t) {
                            scheduled.remove(&id);
                            due.push(id);
                        }
                    }
                    for id in due {
                        if let Some(next) = self.handle_due_auth(now, &id, interval, &pool).await {
                            scheduled.insert(id.clone(), next);
                            queue.push(Reverse((next, id)));
                        }
                    }
                }
            }
            let now = self.now();
            let dirty: Vec<String> = shared.dirty.lock().drain().collect();
            let removed: Vec<String> = shared.removed.lock().drain().collect();
            for id in removed {
                scheduled.remove(&id);
            }
            for id in dirty {
                let auth = self.state.read().auths.get(&id).cloned();
                match auth.and_then(|a| next_refresh_check_at(now, &a)) {
                    Some(next) => {
                        scheduled.insert(id.clone(), next);
                        queue.push(Reverse((next, id)));
                    }
                    None => {
                        scheduled.remove(&id);
                    }
                }
            }
        }
    }

    fn refresh_schedule_snapshot(&self, now: DateTime<Utc>) -> Vec<(String, DateTime<Utc>)> {
        self.state
            .read()
            .auths
            .values()
            .filter_map(|a| next_refresh_check_at(now, a).map(|n| (a.id.clone(), n)))
            .collect()
    }

    /// Handles one due credential: re-queue, or enqueue a refresh job. Returns the next time to
    /// look at it.
    async fn handle_due_auth(
        &self,
        now: DateTime<Utc>,
        id: &str,
        interval: Duration,
        pool: &RefreshPool,
    ) -> Option<DateTime<Utc>> {
        let interval_chrono = to_chrono(interval);
        let (auth, exec) = {
            let st = self.state.read();
            let auth = st.auths.get(id).cloned()?;
            let exec = executor_locked(&st, &executor_key_from_auth(&auth));
            (auth, exec)
        };
        let next = next_refresh_check_at(now, &auth)?;
        if !should_refresh(&auth, now) {
            return Some(next);
        }
        if exec.is_none() {
            return Some(now + interval_chrono);
        }
        let Some(epoch) = self.mark_refresh_pending(id, auth.registration_epoch, now) else {
            let auth = self.state.read().auths.get(id).cloned()?;
            let next = next_refresh_check_at(now, &auth)?;
            return Some(if next > now {
                next
            } else {
                now + interval_chrono
            });
        };
        let Ok(queue_permit) = pool.queue.clone().try_acquire_owned() else {
            // Queue full: do not hold the dispatcher.
            self.finish_refresh_job(id, Some(now + interval_chrono));
            return Some(now + interval_chrono);
        };
        let this = self.clone();
        let id_owned = id.to_string();
        let workers = pool.workers.clone();
        tokio::spawn(async move {
            let _queue_permit = queue_permit;
            // Clears the job entry even if the refresh panics.
            let _job = JobGuard {
                manager: this.clone(),
                id: id_owned.clone(),
            };
            let Ok(_worker) = workers.acquire_owned().await else {
                return;
            };
            if this.begin_refresh_job(&id_owned, epoch) {
                let _ = this.refresh_auth_at_epoch(&id_owned, "", epoch, false).await;
            }
        });
        // The job's completion reschedules via `queue_refresh_reschedule`.
        None
    }

    /// Reserves a refresh job; sets a pending backoff so the credential is not picked twice.
    fn mark_refresh_pending(&self, id: &str, epoch: u64, now: DateTime<Utc>) -> Option<u64> {
        let mut st = self.state.write();
        let auth = st.auths.get_mut(id)?;
        if auth.registration_epoch != epoch
            || has_unauthorized_auth_failure(auth)
            || has_disabled_invalid_grant_failure(auth)
        {
            return None;
        }
        let mut rs = self.refresh_state.lock();
        if rs.jobs.contains_key(id) || auth.next_refresh_after.is_some_and(|t| now < t) {
            return None;
        }
        let pending_until =
            now + chrono::Duration::from_std(REFRESH_PENDING_BACKOFF).unwrap_or_default();
        rs.jobs.insert(
            id.to_string(),
            JobInfo {
                epoch,
                pending_until,
                running: false,
            },
        );
        auth.next_refresh_after = Some(pending_until);
        auth.generation += 1;
        auth.updated_at = Some(now);
        drop(rs);
        drop(st);
        self.queue_refresh_reschedule(id);
        Some(epoch)
    }

    fn begin_refresh_job(&self, id: &str, epoch: u64) -> bool {
        let st = self.state.read();
        let mut rs = self.refresh_state.lock();
        let Some(job) = rs.jobs.get_mut(id) else {
            return false;
        };
        if st
            .auths
            .get(id)
            .is_none_or(|a| a.registration_epoch != epoch)
        {
            return false;
        }
        job.running = true;
        true
    }

    /// Ends the job; clears only its own pending marker so newer refresh outcomes are kept.
    fn finish_refresh_job(&self, id: &str, retry_at: Option<DateTime<Utc>>) {
        let job = self.refresh_state.lock().jobs.remove(id);
        let Some(job) = job else { return };
        {
            let mut st = self.state.write();
            if let Some(auth) = st.auths.get_mut(id)
                && auth.registration_epoch == job.epoch
                && auth.next_refresh_after == Some(job.pending_until)
            {
                auth.next_refresh_after = retry_at;
                auth.generation += 1;
                auth.updated_at = Some(self.now());
            }
        }
        self.queue_refresh_reschedule(id);
    }

    /// One synchronous pass of the refresh policy: refreshes (and awaits) every credential that is
    /// due now. The background loop uses the same policy; this is for tests and tooling.
    pub async fn refresh_due_auths(&self) -> usize {
        let now = self.now();
        let due: Vec<String> = {
            let st = self.state.read();
            st.auths
                .values()
                .filter(|a| a.auth_kind() != AUTH_KIND_API_KEY && should_refresh(a, now))
                .filter(|a| executor_locked(&st, &executor_key_from_auth(a)).is_some())
                .map(|a| a.id.clone())
                .collect()
        };
        let mut n = 0;
        for id in due {
            let _ = self.refresh_auth_for_request(&id, "").await;
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn claude(expiry: Option<i64>, last_refresh: Option<i64>) -> Auth {
        let mut a = Auth::new("c.json", "claude");
        a.metadata.insert("access_token".into(), json!("opaque"));
        a.metadata.insert("refresh_token".into(), json!("rt"));
        if let Some(e) = expiry {
            a.metadata
                .insert("expired".into(), json!(t(e).to_rfc3339()));
        }
        a.last_refreshed_at = last_refresh.map(t);
        a
    }

    #[test]
    fn refresh_due_by_provider_lead() {
        // Claude lead is 4h.
        assert!(should_refresh(&claude(Some(3 * 3600), None), t(0)));
        assert!(!should_refresh(&claude(Some(5 * 3600), None), t(0)));
        // No expiry: last refresh age decides.
        assert!(!should_refresh(&claude(None, Some(-3600)), t(0)));
        assert!(should_refresh(&claude(None, Some(-5 * 3600)), t(0)));
        // Pending backoff blocks.
        let mut a = claude(Some(60), None);
        a.next_refresh_after = Some(t(30));
        assert!(!should_refresh(&a, t(0)));
        // API-key-ish providers without a lead never refresh.
        let mut v = Auth::new("v", "vertex");
        v.metadata.insert("access_token".into(), json!("x"));
        assert!(!should_refresh(&v, t(0)));
    }

    #[test]
    fn preferred_interval_overrides_provider_lead() {
        let mut a = claude(Some(10 * 3600), Some(-120));
        a.metadata
            .insert("refresh_interval_seconds".into(), json!(60));
        assert!(should_refresh(&a, t(0)));
        a.metadata
            .insert("refresh_interval_seconds".into(), json!(3600));
        assert!(!should_refresh(&a, t(0)));
    }

    #[test]
    fn next_check_matches_due_time() {
        let a = claude(Some(5 * 3600), None);
        assert_eq!(next_refresh_check_at(t(0), &a), Some(t(3600)));
        let mut key = Auth::new("k", "claude");
        key.attributes.insert("api_key".into(), "sk".into());
        assert_eq!(next_refresh_check_at(t(0), &key), None);
    }
}
