//! Home control plane integration of the conductor (Go: `conductor_home.go`).
//!
//! In Home mode credentials are not held locally. Every execution attempt asks Home for a
//! credential (`RPOP`), runs the request on the local executor with it, and reports the outcome;
//! Home owns cooldown state, retry limits and per-credential concurrency. This file holds the
//! Home state of the [`Manager`], websocket session bookkeeping and the small helpers shared by
//! the dispatch (`home_dispatch.rs`) and execution (`home_exec.rs`) paths.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_home::executionregistry::Registry;
use cpa_home::{DispatchParams, HomeError};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use super::Manager;
use super::errors::{auth_error, auth_not_found};
use super::home_concurrency::{home_unavailable, valid_canonical_concurrency_model_key};
use super::home_selection::HomeDispatchSelection;
use super::home_session_alias::AliasCache;
use super::models::{
    apply_api_key_model_alias, execution_alias_pool_model, executor_key_from_auth,
    resolve_openai_compat_upstream_model_pool, rewrite_model_for_auth,
};
use crate::executor::{DynExecutor, ExecError, HomeErrKind, Metadata, Options, meta};

pub(crate) const HOME_AUTH_COUNT_METADATA_KEY: &str = "__cliproxy_home_auth_count";
pub(crate) const HOME_RETRY_ROUND_METADATA_KEY: &str = "request_retry_round";
/// Credential ids already attempted in the current request retry round.
pub const EXCLUDED_AUTH_IDS_METADATA_KEY: &str = "excluded_auth_ids";
pub(crate) const DOWNSTREAM_WEBSOCKET_METADATA_KEY: &str = "downstream_websocket";

/// The slice of the Home client the conductor dispatches through. `cpa_home::Client` implements
/// it; tests substitute doubles.
#[async_trait]
pub trait HomeAuthDispatcher: Send + Sync {
    fn heartbeat_ok(&self) -> bool;
    async fn rpop_auth(&self, params: &DispatchParams<'_>) -> Result<Vec<u8>, HomeError>;
    fn abort_ambiguous_dispatch(&self);
}

#[async_trait]
impl HomeAuthDispatcher for cpa_home::Client {
    fn heartbeat_ok(&self) -> bool {
        cpa_home::Client::heartbeat_ok(self)
    }

    async fn rpop_auth(&self, params: &DispatchParams<'_>) -> Result<Vec<u8>, HomeError> {
        cpa_home::Client::rpop_auth(self, params).await
    }

    fn abort_ambiguous_dispatch(&self) {
        cpa_home::Client::abort_ambiguous_dispatch(self);
    }
}

/// The immutable client and registry pair of one Home lifetime.
pub struct HomeDispatchBundle {
    pub(crate) client: Arc<dyn HomeAuthDispatcher>,
    pub(crate) registry: Registry,
    pub generation: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionSelectionKey {
    credential_id: String,
    route_model: String,
}

type SessionSelections = HashMap<String, HashMap<SessionSelectionKey, HomeDispatchSelection>>;

/// Home mode state of the manager.
#[derive(Default)]
pub(crate) struct HomeState {
    pub bundle: RwLock<Option<Arc<HomeDispatchBundle>>>,
    /// Retained selections of websocket sessions: session id -> (credential, route) -> selection.
    selections: Mutex<SessionSelections>,
    /// Session-scoped runtime auths for websocket continuity: session id -> auth id -> auth.
    runtime_auths: Mutex<HashMap<String, HashMap<String, Auth>>>,
    runtime_auth_owners: Mutex<HashMap<String, HashMap<String, HomeDispatchSelection>>>,
    session_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub aliases: Mutex<AliasCache>,
    pub publisher_config: RwLock<Option<super::home_publisher::PublisherConfig>>,
    prepare_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub(crate) fn trimmed_meta(md: &Metadata, key: &str) -> String {
    md.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub(crate) fn home_execution_session_id(md: &Metadata) -> String {
    trimmed_meta(md, meta::EXECUTION_SESSION_ID)
}

pub(crate) fn downstream_websocket(md: &Metadata) -> bool {
    md.get(DOWNSTREAM_WEBSOCKET_METADATA_KEY).and_then(Value::as_bool).unwrap_or(false)
}

/// Go: `homeRetryRoundFromMetadata`: positive whole numbers only, else 0.
pub(crate) fn home_retry_round_from_metadata(md: &Metadata) -> i64 {
    match md.get(HOME_RETRY_ROUND_METADATA_KEY) {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                return i.max(0);
            }
            match n.as_f64() {
                Some(f) if f > 0.0 && f.fract() == 0.0 => f as i64,
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Go: `homeAuthCountFromMetadata`.
pub(crate) fn home_auth_count_from_metadata(md: &Metadata) -> i64 {
    match md.get(HOME_AUTH_COUNT_METADATA_KEY).and_then(Value::as_f64) {
        Some(v) if v > 0.0 => v as i64,
        _ => 1,
    }
}

/// Go: `homeExcludedAuthIDsFromMetadata`: trimmed, deduplicated and sorted.
pub(crate) fn home_excluded_auth_ids_from_metadata(md: &Metadata) -> Vec<String> {
    let Some(raw) = md.get(EXCLUDED_AUTH_IDS_METADATA_KEY) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim();
        if !value.is_empty() && seen.insert(value.to_string()) {
            ids.push(value.to_string());
        }
    };
    match raw {
        Value::Array(items) => items.iter().filter_map(Value::as_str).for_each(&mut push),
        Value::Object(map) => map.iter().filter(|(_, v)| v.as_bool().unwrap_or(false)).for_each(|(k, _)| push(k)),
        _ => {}
    }
    ids.sort();
    ids
}

pub(crate) fn with_home_auth_count(mut opts: Options, count: i64) -> Options {
    opts.metadata.insert(HOME_AUTH_COUNT_METADATA_KEY.into(), Value::from(count.max(1)));
    opts
}

pub(crate) fn with_home_retry_round(mut opts: Options, retry_round: i64) -> Options {
    if retry_round > 0 {
        opts.metadata.insert(HOME_RETRY_ROUND_METADATA_KEY.into(), Value::from(retry_round));
    } else {
        opts.metadata.remove(HOME_RETRY_ROUND_METADATA_KEY);
    }
    opts
}

pub(crate) fn with_home_excluded_auth_ids(mut opts: Options, tried: &HashSet<String>) -> Options {
    let mut excluded: HashSet<String> = home_excluded_auth_ids_from_metadata(&opts.metadata).into_iter().collect();
    excluded.extend(tried.iter().map(|id| id.trim().to_string()).filter(|id| !id.is_empty()));
    if excluded.is_empty() {
        opts.metadata.remove(EXCLUDED_AUTH_IDS_METADATA_KEY);
    } else {
        let mut ids: Vec<String> = excluded.into_iter().collect();
        ids.sort();
        opts.metadata.insert(
            EXCLUDED_AUTH_IDS_METADATA_KEY.into(),
            Value::Array(ids.into_iter().map(Value::String).collect()),
        );
    }
    opts
}

/// Go: `requestedModelFromMetadata`.
pub(crate) fn requested_model_from_metadata(md: &Metadata, fallback: &str) -> String {
    let requested = trimmed_meta(md, meta::REQUESTED_MODEL);
    if !requested.is_empty() {
        return requested;
    }
    let fallback = fallback.trim();
    if fallback.is_empty() { "unknown".into() } else { fallback.into() }
}

pub(crate) fn home_request_retry_exceeded_error() -> ExecError {
    auth_error("request_retry_exceeded", "home returned a previously tried auth", 503)
}

pub(crate) fn is_home_request_retry_exceeded(err: &ExecError) -> bool {
    err.auth_code.as_deref().is_some_and(|c| c.trim().eq_ignore_ascii_case("request_retry_exceeded"))
}

/// Go: `shouldReturnLastErrorOnPickFailure`.
pub(crate) fn should_return_last_error_on_pick_failure(home_mode: bool, last_err: Option<&ExecError>, pick_err: &ExecError) -> bool {
    if last_err.is_none() {
        return false;
    }
    if !home_mode {
        return true;
    }
    if is_home_request_retry_exceeded(pick_err) {
        return true;
    }
    matches!(
        pick_err.auth_code.as_deref().map(|c| c.trim().to_lowercase()).as_deref(),
        Some("auth_not_found" | "auth_unavailable")
    )
}

/// Go: `isHomeNextRoundImmediatelyAvailable`.
pub(crate) fn is_home_next_round_immediately_available(err: &ExecError) -> bool {
    err.auth_code.as_deref().is_some_and(|c| c.trim().eq_ignore_ascii_case("auth_unavailable"))
}

pub(crate) fn home_cooldown_request_retry(err: &ExecError) -> Option<i64> {
    match &err.home {
        Some(HomeErrKind::DispatchRetryAfter { request_retry }) => *request_retry,
        _ => None,
    }
}

pub(crate) fn is_home_dispatch_cooldown(err: &ExecError) -> bool {
    matches!(err.home, Some(HomeErrKind::DispatchRetryAfter { .. }))
}

pub(crate) fn is_home_retry_round_exhausted(err: &ExecError) -> bool {
    matches!(err.home, Some(HomeErrKind::RetryRoundExhausted { .. }))
}

/// Go: `observeHomeCooldownRetryLimit`.
pub(crate) fn observe_home_cooldown_retry_limit(err: &ExecError, retry_limit: &mut i64, accept_remote: bool) {
    if !accept_remote {
        return;
    }
    if let Some(remote) = home_cooldown_request_retry(err) {
        *retry_limit = remote;
    }
}

/// Go: `markHomeRetryRoundExhausted`: the error wrapped with the round's retry timing.
pub(crate) fn mark_home_retry_round_exhausted(mut err: ExecError, timing: RoundTimingResult, retry_now: bool) -> ExecError {
    let cause_retry_after = match &err.home {
        // Re-marking keeps the original cause's own hint.
        Some(HomeErrKind::RetryRoundExhausted { cause_retry_after, .. }) => *cause_retry_after,
        _ => err.retry_after,
    };
    let (retry_after, invalid) = match timing {
        RoundTimingResult::None => (None, false),
        RoundTimingResult::After(d) => (Some(d), false),
        RoundTimingResult::Invalid => (None, true),
    };
    err.home = Some(HomeErrKind::RetryRoundExhausted { retry_now, retry_after_invalid: invalid, cause_retry_after });
    err.retry_after = retry_after;
    err
}

/// Strips the retry-round marker, restoring the cause's own retry hint.
pub(crate) fn unwrap_home_retry_round(mut err: ExecError) -> ExecError {
    if let Some(HomeErrKind::RetryRoundExhausted { cause_retry_after, .. }) = &err.home {
        err.retry_after = *cause_retry_after;
        err.home = None;
    }
    err
}

/// Go: `preferredExecutionAttemptError` for round-exhausted fallbacks: the upstream failure with
/// the fallback's authoritative round timing.
pub(crate) fn preferred_home_error(fallback: ExecError, upstream: Option<&ExecError>) -> ExecError {
    let Some(upstream) = upstream else { return fallback };
    let Some(HomeErrKind::RetryRoundExhausted { retry_now, retry_after_invalid, .. }) = fallback.home.clone() else {
        return upstream.clone();
    };
    let mut cause = unwrap_home_retry_round(upstream.clone());
    let cause_retry_after = cause.retry_after;
    cause.home = Some(HomeErrKind::RetryRoundExhausted { retry_now, retry_after_invalid, cause_retry_after });
    cause.retry_after = fallback.retry_after;
    cause.upstream_attempted = true;
    cause
}

/// Retry timing observed across the failures of one credential round (Go: `homeRetryRoundTiming`).
#[derive(Default)]
pub(crate) struct RoundTiming {
    retry_after: Option<Duration>,
    immediate: bool,
    invalid: bool,
}

pub(crate) enum RoundTimingResult {
    None,
    After(Duration),
    Invalid,
}

impl RoundTiming {
    pub fn observe(&mut self, err: &ExecError) {
        if self.immediate || self.invalid {
            return;
        }
        let Some(retry_after) = err.retry_after else { return };
        if retry_after.is_zero() {
            self.retry_after = Some(Duration::ZERO);
            self.immediate = true;
            return;
        }
        if self.retry_after.is_none_or(|d| d.is_zero()) || retry_after < self.retry_after.unwrap_or_default() {
            self.retry_after = Some(retry_after);
        }
    }

    pub fn result(&self) -> RoundTimingResult {
        if self.immediate {
            return RoundTimingResult::None;
        }
        if self.invalid {
            return RoundTimingResult::Invalid;
        }
        match self.retry_after {
            Some(d) if !d.is_zero() => RoundTimingResult::After(d),
            _ => RoundTimingResult::None,
        }
    }
}

/// Go: `pendingHomeRetryRoundDelay`: the wait before replaying a round whose dispatch answered
/// `model_cooldown` with a usable hint.
pub(crate) fn pending_home_retry_round_delay(
    err: &ExecError,
    max_wait: Duration,
    retry_limit: &mut i64,
    accept_remote_retry_limit: bool,
) -> Option<Duration> {
    if is_home_retry_round_exhausted(err) || !is_home_dispatch_cooldown(err) {
        return None;
    }
    observe_home_cooldown_retry_limit(err, retry_limit, accept_remote_retry_limit);
    let retry_after = err.retry_after?;
    if retry_after.is_zero() || max_wait.is_zero() || retry_after > max_wait {
        return None;
    }
    Some(retry_after)
}

impl Manager {
    /// Whether the Home control plane integration is enabled in the runtime config.
    pub fn home_enabled(&self) -> bool {
        self.cfg().home.enabled
    }

    /// Publishes the selectable Home lifetime as one atomic bundle.
    pub fn publish_home_dispatch(
        &self,
        client: Arc<dyn HomeAuthDispatcher>,
        registry: Registry,
        generation: u64,
    ) -> Arc<HomeDispatchBundle> {
        let bundle = Arc::new(HomeDispatchBundle { client, registry, generation });
        *self.home.bundle.write() = Some(bundle.clone());
        bundle
    }

    /// Removes `bundle` only while it still belongs to the active lifetime.
    pub fn clear_home_dispatch_bundle(&self, bundle: &Arc<HomeDispatchBundle>) -> bool {
        let mut cur = self.home.bundle.write();
        if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, bundle)) {
            *cur = None;
            return true;
        }
        false
    }

    pub fn home_dispatch_bundle(&self) -> Option<Arc<HomeDispatchBundle>> {
        self.home.bundle.read().clone()
    }

    pub fn home_execution_registry(&self) -> Option<Registry> {
        self.home_dispatch_bundle().map(|b| b.registry.clone())
    }

    // ---- websocket session bookkeeping ----

    /// Serializes the executions of one downstream websocket session.
    pub(crate) async fn lock_home_websocket_session(
        &self,
        opts: &Options,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if !downstream_websocket(&opts.metadata) {
            return None;
        }
        let session_id = home_execution_session_id(&opts.metadata);
        if session_id.is_empty() {
            return None;
        }
        let lock = self
            .home
            .session_locks
            .lock()
            .entry(session_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        Some(lock.lock_owned().await)
    }

    /// Reuses the selection retained for this websocket session when it still matches the route.
    pub(crate) async fn retained_home_session_selection(
        &self,
        opts: &Options,
        model: &str,
        excluded: &HashSet<String>,
    ) -> Result<Option<HomeDispatchSelection>, ExecError> {
        if !downstream_websocket(&opts.metadata) {
            return Ok(None);
        }
        let session_id = home_execution_session_id(&opts.metadata);
        let credential_id = super::pick::pinned_auth_id(&opts.metadata);
        if session_id.is_empty() {
            return Ok(None);
        }
        let (route_model, valid_route) = valid_canonical_concurrency_model_key(model);
        let fallback_attempt = home_auth_count_from_metadata(&opts.metadata) > 1
            || home_retry_round_from_metadata(&opts.metadata) > 0;
        let mut retained: Option<HomeDispatchSelection> = None;
        let mut ended = Vec::new();
        {
            let mut all = self.home.selections.lock();
            if let Some(selections) = all.get_mut(&session_id) {
                let keys: Vec<SessionSelectionKey> = selections.keys().cloned().collect();
                for key in keys {
                    let Some(selection) = selections.get(&key).cloned() else { continue };
                    let matches_credential = credential_id.is_empty() || key.credential_id == credential_id;
                    let matches_route = valid_route && key.route_model == route_model;
                    let is_excluded = excluded.contains(key.credential_id.trim());
                    if !fallback_attempt
                        && !is_excluded
                        && matches_credential
                        && selection.active()
                        && matches_route
                        && retained.is_none()
                    {
                        retained = Some(selection);
                        continue;
                    }
                    selections.remove(&key);
                    ended.push(selection);
                }
                if selections.is_empty() {
                    all.remove(&session_id);
                }
            }
        }
        for selection in ended {
            self.end_home_selection_before_redispatch(&selection, "target_changed").await?;
        }
        Ok(retained)
    }

    /// Go: `predictedHomeConcurrencyModel`: the limiter model key an auth would be accounted
    /// under for `route_model`, when it resolves to exactly one upstream model.
    pub(crate) fn predicted_home_concurrency_model(&self, auth: &Auth, route_model: &str) -> Option<String> {
        let cfg = self.cfg();
        let requested = rewrite_model_for_auth(route_model, auth);
        let alias = self.alias_result_for_requested(&cfg, auth, &requested);
        let mut upstream = execution_alias_pool_model(auth, &requested, &alias);
        let pool = resolve_openai_compat_upstream_model_pool(&cfg, auth, &upstream);
        if !pool.is_empty() {
            if pool.len() != 1 {
                return None;
            }
            upstream = pool[0].clone();
        } else {
            upstream = apply_api_key_model_alias(&cfg, auth, &upstream);
        }
        let (key, valid) = valid_canonical_concurrency_model_key(&upstream);
        valid.then_some(key)
    }

    /// Ends the session's selections that no longer match `credential_id` + `model`.
    pub(crate) async fn end_mismatched_home_session_selections(
        &self,
        session_id: &str,
        credential_id: &str,
        model: &str,
        wait_for_ack: bool,
    ) -> Result<(), ExecError> {
        if session_id.is_empty() {
            return Ok(());
        }
        let (route_model, valid_route) = valid_canonical_concurrency_model_key(model);
        let mut ended = Vec::new();
        {
            let mut all = self.home.selections.lock();
            if let Some(selections) = all.get_mut(session_id) {
                let keys: Vec<SessionSelectionKey> = selections.keys().cloned().collect();
                for key in keys {
                    let matches_route = valid_route && key.route_model == route_model;
                    if key.credential_id == credential_id && matches_route {
                        continue;
                    }
                    if let Some(selection) = selections.remove(&key) {
                        ended.push(selection);
                    }
                }
                if selections.is_empty() {
                    all.remove(session_id);
                }
            }
        }
        for selection in ended {
            if !wait_for_ack {
                selection.end("target_changed");
                continue;
            }
            self.end_home_selection_before_redispatch(&selection, "target_changed").await?;
        }
        Ok(())
    }

    /// Ends a selection and waits (bounded by `cpa-cancel-bound`) for Home to acknowledge the
    /// release before another credential is requested.
    pub(crate) async fn end_home_selection_before_redispatch(
        &self,
        selection: &HomeDispatchSelection,
        reason: &str,
    ) -> Result<(), ExecError> {
        let Some(ticket) = selection.end_with_release(reason) else { return Ok(()) };
        let bound = self.cfg().credential_concurrency.clone().with_defaults().cpa_cancel_bound.to_std();
        ticket.wait(bound).await.map_err(|e| {
            home_unavailable(&format!("Home did not acknowledge credential release: {e}"), true)
        })
    }

    /// Keeps the selection of a websocket execution for the session's next request.
    pub(crate) fn retain_home_websocket_selection(
        &self,
        opts: &Options,
        model: &str,
        selection: &HomeDispatchSelection,
    ) -> bool {
        if !selection.retained() || !downstream_websocket(&opts.metadata) {
            return false;
        }
        let Some(selection_auth) = selection.clone_auth() else { return false };
        let session_id = home_execution_session_id(&opts.metadata);
        let credential_id = selection_auth.id.trim().to_string();
        let (route_model, valid_route) = valid_canonical_concurrency_model_key(model);
        if selection.accounted_model().is_empty() {
            selection.set_accounted_model(&self.predicted_home_concurrency_model(&selection_auth, model).unwrap_or_default());
        }
        if session_id.is_empty() || credential_id.is_empty() || !valid_route || selection.accounted_model().is_empty() {
            return false;
        }
        // Mismatched siblings end without waiting (Go: `_ = endMismatched...(..., false)`).
        let mut ended = Vec::new();
        let previous;
        {
            let mut all = self.home.selections.lock();
            if let Some(selections) = all.get_mut(&session_id) {
                let keys: Vec<SessionSelectionKey> = selections.keys().cloned().collect();
                for key in keys {
                    if key.credential_id == credential_id && key.route_model == route_model {
                        continue;
                    }
                    if let Some(s) = selections.remove(&key) {
                        ended.push(s);
                    }
                }
            }
            let key = SessionSelectionKey { credential_id: credential_id.clone(), route_model };
            previous = all.entry(session_id.clone()).or_default().insert(key, selection.clone());
        }
        for s in ended {
            s.end("target_changed");
        }
        self.remember_home_runtime_auth(&session_id, &selection_auth);
        if let Some(previous) = previous
            && !previous.same(selection)
        {
            previous.end("target_replaced");
        }
        true
    }

    pub(crate) fn clear_home_session_locks(&self) {
        self.home.session_locks.lock().clear();
    }

    fn take_home_session_selections(&self, session_id: &str) -> Vec<HomeDispatchSelection> {
        self.home
            .selections
            .lock()
            .remove(session_id)
            .map(|m| m.into_values().collect())
            .unwrap_or_default()
    }

    fn take_all_home_session_selections(&self) -> Vec<HomeDispatchSelection> {
        std::mem::take(&mut *self.home.selections.lock())
            .into_values()
            .flat_map(HashMap::into_values)
            .collect()
    }

    /// Drops all session auths and ends every retained selection (Home turned off).
    pub(crate) fn clear_home_runtime_auths(&self) {
        self.home.runtime_auths.lock().clear();
        self.home.runtime_auth_owners.lock().clear();
        let selections = self.take_all_home_session_selections();
        self.home.aliases.lock().clear();
        for s in selections {
            s.end("home_disabled");
        }
    }

    /// Releases what Home dispatch holds for an execution session (Go: the Home half of
    /// `CloseExecutionSession`); `CLOSE_ALL_EXECUTION_SESSIONS_ID` releases everything.
    pub(crate) fn close_home_execution_session(&self, session_id: &str) {
        let selections = if session_id == super::CLOSE_ALL_EXECUTION_SESSIONS_ID {
            self.home.runtime_auths.lock().clear();
            self.home.runtime_auth_owners.lock().clear();
            self.clear_home_session_locks();
            self.take_all_home_session_selections()
        } else {
            self.home.runtime_auths.lock().remove(session_id);
            self.home.runtime_auth_owners.lock().remove(session_id);
            self.home.session_locks.lock().remove(session_id);
            self.take_home_session_selections(session_id)
        };
        for s in selections {
            s.end("session_closed");
        }
    }

    /// Forgets `auth_id` in every session's runtime auths (the auth was removed).
    pub(crate) fn home_forget_auth(&self, auth_id: &str) {
        let mut auths = self.home.runtime_auths.lock();
        auths.retain(|_, session| {
            session.remove(auth_id);
            !session.is_empty()
        });
    }

    /// Go: `GetExecutionSessionAuthByID`: a Home runtime auth scoped to an execution session.
    pub fn get_execution_session_auth_by_id(&self, session_id: &str, auth_id: &str) -> Option<Auth> {
        let (session_id, auth_id) = (session_id.trim(), auth_id.trim());
        if session_id.is_empty() || auth_id.is_empty() {
            return None;
        }
        self.home.runtime_auths.lock().get(session_id)?.get(auth_id).cloned()
    }

    /// Binds the selection's auth to the websocket session so later requests can find it.
    pub(crate) fn bind_home_selection_runtime_auth(
        &self,
        opts: &Options,
        selection: &HomeDispatchSelection,
    ) -> Result<(), ExecError> {
        if !downstream_websocket(&opts.metadata) {
            return Ok(());
        }
        let Some(selection_auth) = selection.clone_auth() else { return Ok(()) };
        if !auth_websockets_enabled(&selection_auth) {
            return Ok(());
        }
        let session_id = home_execution_session_id(&opts.metadata);
        let auth_id = selection_auth.id.trim().to_string();
        if session_id.is_empty() || auth_id.is_empty() || !selection.mark_runtime_auth_bound() {
            return Ok(());
        }
        self.remember_home_selection_runtime_auth(&session_id, selection);
        let manager = self.clone();
        let (sid, aid, owner) = (session_id.clone(), auth_id.clone(), selection.clone());
        let bound = selection.bind(Box::new(move || {
            manager.forget_home_runtime_auth(&sid, &aid, Some(&owner));
            Ok(())
        }));
        if let Err(e) = bound {
            selection.unmark_runtime_auth_bound();
            self.forget_home_runtime_auth(&session_id, &auth_id, Some(selection));
            return Err(super::home_concurrency::install_error(e));
        }
        Ok(())
    }

    fn remember_home_selection_runtime_auth(&self, session_id: &str, selection: &HomeDispatchSelection) {
        let Some(selection_auth) = selection.clone_auth() else { return };
        let (session_id, auth_id) = (session_id.trim(), selection_auth.id.trim().to_string());
        if session_id.is_empty() || auth_id.is_empty() {
            return;
        }
        self.home.runtime_auths.lock().entry(session_id.to_string()).or_default().insert(auth_id.clone(), selection_auth);
        self.home
            .runtime_auth_owners
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .insert(auth_id, selection.clone());
    }

    /// Updates the stored session auths after Home refreshed the selection's credentials.
    pub fn replace_home_selection_auth(&self, selection: &HomeDispatchSelection, auth: &Auth) {
        selection.replace_auth(auth);
        let Some(updated) = selection.clone_auth() else { return };
        let owners = self.home.runtime_auth_owners.lock();
        let mut auths = self.home.runtime_auths.lock();
        for (session_id, session_owners) in owners.iter() {
            for (auth_id, owner) in session_owners {
                if owner.same(selection)
                    && let Some(slot) = auths.get_mut(session_id)
                {
                    slot.insert(auth_id.clone(), updated.clone());
                }
            }
        }
    }

    fn forget_home_runtime_auth(&self, session_id: &str, auth_id: &str, owner: Option<&HomeDispatchSelection>) {
        let (session_id, auth_id) = (session_id.trim(), auth_id.trim());
        if session_id.is_empty() || auth_id.is_empty() {
            return;
        }
        let mut owners = self.home.runtime_auth_owners.lock();
        if let Some(owner) = owner
            && !owners.get(session_id).and_then(|m| m.get(auth_id)).is_some_and(|o| o.same(owner))
        {
            return;
        }
        let mut auths = self.home.runtime_auths.lock();
        if let Some(session) = auths.get_mut(session_id) {
            session.remove(auth_id);
            if session.is_empty() {
                auths.remove(session_id);
            }
        }
        if let Some(session) = owners.get_mut(session_id) {
            session.remove(auth_id);
            if session.is_empty() {
                owners.remove(session_id);
            }
        }
    }

    pub(crate) fn remember_home_runtime_auth(&self, session_id: &str, auth: &Auth) {
        let (session_id, auth_id) = (session_id.trim(), auth.id.trim());
        if session_id.is_empty() || auth_id.is_empty() || !auth_websockets_enabled(auth) {
            return;
        }
        self.home
            .runtime_auths
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .insert(auth_id.to_string(), auth.clone());
    }

    /// A session-scoped runtime auth with its executor and provider key.
    pub fn home_runtime_auth_by_id(&self, session_id: &str, auth_id: &str) -> Option<(Auth, DynExecutor, String)> {
        let auth = self.get_execution_session_auth_by_id(session_id, auth_id)?;
        if !auth_websockets_enabled(&auth) {
            return None;
        }
        let logical_provider = auth.provider.trim().to_lowercase();
        let executor_key = executor_key_from_auth(&auth);
        if logical_provider.is_empty() || executor_key.is_empty() {
            return None;
        }
        let executor = self.executor(&executor_key).or_else(|| {
            (!auth.attr("base_url").is_empty()).then(|| self.executor("openai-compatibility")).flatten()
        })?;
        Some((auth, executor, logical_provider))
    }

    /// Serializes request-auth preparation per credential id.
    pub(crate) fn home_prepare_lock(&self, auth_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.home
            .prepare_locks
            .lock()
            .entry(auth_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Prepares a dispatch auth without reading or updating local auth state.
    pub(crate) async fn prepare_home_request_auth(
        &self,
        executor: &DynExecutor,
        selection: &HomeDispatchSelection,
    ) -> Result<Auth, ExecError> {
        let Some(auth) = selection.clone_auth() else {
            return Err(auth_not_found("no auth available"));
        };
        if !executor.should_prepare_request_auth(&auth) {
            return Ok(auth);
        }
        let _guard = if auth.id.trim().is_empty() {
            None
        } else {
            Some(self.home_prepare_lock(auth.id.trim()).lock_owned().await)
        };
        if !executor.should_prepare_request_auth(&auth) {
            return Ok(auth);
        }
        match executor.prepare_request_auth(&auth).await {
            Ok(Some(updated)) => Ok(updated),
            Ok(None) => Ok(auth),
            Err(e) => {
                tracing::warn!(
                    "Home credential operation failed: operation=request_auth_preparation provider={} err={}",
                    selection.provider(),
                    e.message
                );
                Err(e)
            }
        }
    }

    /// Go: `homeRetryAllowed`: whether another retry round fits the (remote or local) limit.
    pub(crate) fn home_retry_allowed(&self, attempt: i64, retry_limit: i64) -> bool {
        if !self.home_enabled() || attempt < 0 {
            return false;
        }
        let limit = if retry_limit < 0 { self.retry_settings().request_retry.max(0) } else { retry_limit };
        attempt < limit
    }

    /// Go: `observeHomeRetryLimit`.
    pub(crate) fn observe_home_retry_limit(&self, auth: &Auth, selection: Option<&HomeDispatchSelection>, retry_limit: &mut i64) {
        if let Some(limit) = selection.and_then(HomeDispatchSelection::request_retry) {
            *retry_limit = limit;
            return;
        }
        let mut limit = self.retry_settings().request_retry;
        if let Some(over) = auth.request_retry_override() {
            limit = over;
        }
        let limit = limit.max(0);
        if *retry_limit < 0 || limit > *retry_limit {
            *retry_limit = limit;
        }
    }

    /// Reports a Home-dispatched attempt without touching local auth state (Go:
    /// `reportHomeResult`; usage is recorded here because executors do not publish it).
    pub(crate) fn report_home_result(
        &self,
        result: super::cooldown::ExecResult,
        auth: Option<&Auth>,
        facts: Option<super::usage::UsageFacts>,
    ) {
        if result.auth_id.is_empty() {
            return;
        }
        let now = self.now();
        let hook = self.hook.read().clone();
        if let Some(hook) = hook {
            hook.on_result(&result);
        }
        if let Some(aff) = self.selector().affinity() {
            aff.on_result(&result);
        }
        self.record_usage(&result, auth, facts.as_ref(), now);
    }
}

/// Go: `authWebsocketsEnabled`.
pub(crate) fn auth_websockets_enabled(auth: &Auth) -> bool {
    let parse = |s: &str| match s.trim().to_lowercase().as_str() {
        "1" | "t" | "true" => Some(true),
        "0" | "f" | "false" => Some(false),
        _ => None,
    };
    if let Some(raw) = auth.attributes.get("websockets")
        && !raw.trim().is_empty()
        && let Some(parsed) = parse(raw)
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => parse(s).unwrap_or(false),
        _ => false,
    }
}
