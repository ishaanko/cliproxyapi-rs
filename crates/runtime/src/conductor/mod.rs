//! Auth manager / conductor (Go: sdk/cliproxy/auth). Selects a credential per request,
//! invokes the provider executor, classifies failures into cooldowns, and retries or fails
//! over across credentials.
//!
//! Layout:
//! - [`Manager`] (this file): state, configuration, executors, small accessors.
//! - `pick`: candidate filtering, availability tiers, strategy/affinity selection.
//! - `exec`, `stream`: the Execute / ExecuteCount / ExecuteStream loops (retry rounds, failover).
//! - `retry`: round-to-round retry decisions and cooldown waits.
//! - `results`: MarkResult, registry projections, cooldown persistence, quota reset.
//! - `lifecycle`, `refresh`: register/update/remove/load, token refresh and the auto-refresh loop.
//! - `cooldown`, `errors`, `rules`: the cooldown state machine, failure classification and
//!   request-scoped rules (pure, unit-tested without executors).
//! - `selector`, `session`: strategies and session affinity; `models`: aliases, prefixes, pools.
//!
//! Home mode (`home*.rs`): dispatch through the Home control plane instead of local selection.
//!
//! Not ported (Go-only features): plugin schedulers/interceptors, the scheduler's incremental index (selection recomputes per request),
//! per-auth `RoundTripper`s (executors resolve `Auth::proxy_url` themselves), downstream-websocket
//! transport preference and the LCP prefix matcher.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_auth::Store;
use cpa_config::Config;
use cpa_core::registry::{ModelRegistry, global_registry};
use parking_lot::{Mutex, RwLock};

use crate::executor::{DynExecutor, ExecError, Metadata, Options, Request, Response, StreamResult};
use crate::usage::UsageTracker;

pub mod clock;
pub mod cooldown;
pub mod cooldown_state;
mod credits;
pub mod errors;
pub mod events;
mod exec;
pub mod home;
mod home_concurrency;
mod home_dispatch;
mod home_exec;
mod home_model_info;
pub mod home_publisher;
mod home_selection;
mod home_session_alias;
mod lifecycle;
pub mod merge;
pub mod models;
mod pick;
mod refresh;
mod results;
mod retry;
pub mod rewriter;
mod routing;
pub mod rules;
pub mod selector;
pub mod session;
mod stream;
#[cfg(test)]
mod home_tests;
#[cfg(test)]
mod tests;
pub mod usage;
pub mod util;

pub use clock::{Clock, ManualClock, SystemClock};
pub use cooldown::{CooldownView, CoolingPolicy, ExecResult};
pub use cooldown_state::{CooldownStateRecord, CooldownStateStore, FileCooldownStateStore};
pub use credits::{
    ANTIGRAVITY_CREDITS_METADATA_KEY, AntigravityCreditsHint, antigravity_credits_hint,
    antigravity_credits_hint_async, get_antigravity_credits_hint_required,
    has_known_antigravity_credits_hint, has_known_antigravity_credits_hint_async,
    set_antigravity_credits_hint, set_antigravity_credits_hint_async,
};
pub use errors::{enrich_auth_selection_error, safe_response_headers};
pub use events::{ErrorEventSink, Hook, ResultPolicy};
pub use home::{EXCLUDED_AUTH_IDS_METADATA_KEY, HomeAuthDispatcher, HomeDispatchBundle};
pub use home_concurrency::{home_concurrency_busy_error, home_safe_response_headers};
pub use home_model_info::RESOLVED_HOME_MODEL_OPTIONS;
pub use home_selection::HomeDispatchSelection;
pub use lifecycle::UpdateOptions;
pub use models::{ResolvedModelInfo, codex_api_key_model_is_compat, resolved_model_info};
pub use refresh::ForceRefreshResult;
pub use selector::{Selector, SelectorConfig, Strategy};

/// Shared handle used by HTTP handlers and the management API.
pub type SharedManager = Arc<Manager>;

/// Id passed to `close_execution_session` to release every active session of an executor.
pub const CLOSE_ALL_EXECUTION_SESSIONS_ID: &str = "__all_execution_sessions__";

/// Credential policy accepted by [`Manager::select_auth_with_credential_policy`].
pub const CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1: &str = "codex_alpha_search_v1";

#[derive(Debug, Clone, Copy)]
pub(crate) struct RetrySettings {
    pub request_retry: i64,
    pub max_retry_credentials: i64,
    pub max_retry_interval: Duration,
}

pub(crate) struct State {
    pub auths: HashMap<String, Auth>,
    pub auth_epochs: HashMap<String, u64>,
    pub executors: HashMap<String, DynExecutor>,
}

/// The auth manager: a cheap-to-clone handle (streams keep one alive to record their outcome).
/// All methods take `&self`; share it as [`SharedManager`].
#[derive(Clone)]
pub struct Manager {
    core: Arc<Core>,
}

impl std::ops::Deref for Manager {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// Manager internals (reached through `Manager`'s `Deref`).
pub struct Core {
    pub(crate) state: RwLock<State>,
    pub(crate) config: RwLock<Arc<Config>>,
    pub(crate) selector: RwLock<Arc<Selector>>,
    pub(crate) oauth_alias: RwLock<Arc<models::OAuthAliasTable>>,
    pub(crate) retry: RwLock<RetrySettings>,
    pub(crate) clock: RwLock<Arc<dyn Clock>>,
    pub(crate) registry: &'static ModelRegistry,
    pub(crate) usage: RwLock<Option<Arc<UsageTracker>>>,
    pub(crate) hook: RwLock<Option<Arc<dyn Hook>>>,
    pub(crate) result_policy: RwLock<Option<Arc<dyn ResultPolicy>>>,
    pub(crate) error_sink: RwLock<Option<ErrorEventSink>>,
    pub(crate) store: RwLock<Option<Arc<dyn Store>>>,
    pub(crate) cooldown_store: RwLock<Option<Arc<dyn CooldownStateStore>>>,
    pub(crate) cooldown_disabled: std::sync::atomic::AtomicBool,
    pub(crate) cooldown_save_lock: Mutex<()>,
    pub(crate) pool_offsets: Mutex<HashMap<String, usize>>,
    pub(crate) refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) persist_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<(u64, u64)>>>>,
    pub(crate) refresh_state: Mutex<refresh::RefreshState>,
    pub(crate) selector_config: Mutex<SelectorConfig>,
    pub(crate) home: home::HomeState,
}

impl Default for Manager {
    fn default() -> Self {
        Manager::new()
    }
}

impl Manager {
    /// A manager on the system clock and the process-wide model registry.
    pub fn new() -> Self {
        Self::with_parts(Arc::new(SystemClock), global_registry())
    }

    /// A manager with an explicit clock and model registry (tests, embedding).
    pub fn with_parts(clock: Arc<dyn Clock>, registry: &'static ModelRegistry) -> Self {
        let selector_config = SelectorConfig::default();
        let core = Core {
            state: RwLock::new(State {
                auths: HashMap::new(),
                auth_epochs: HashMap::new(),
                executors: HashMap::new(),
            }),
            config: RwLock::new(Arc::new(Config::default())),
            selector: RwLock::new(Arc::new(Selector::new(selector_config, clock.clone()))),
            oauth_alias: RwLock::new(Arc::new(models::OAuthAliasTable::default())),
            retry: RwLock::new(RetrySettings {
                request_retry: 0,
                max_retry_credentials: 0,
                max_retry_interval: Duration::ZERO,
            }),
            clock: RwLock::new(clock),
            registry,
            usage: RwLock::new(None),
            hook: RwLock::new(None),
            result_policy: RwLock::new(None),
            error_sink: RwLock::new(None),
            store: RwLock::new(None),
            cooldown_store: RwLock::new(None),
            cooldown_disabled: std::sync::atomic::AtomicBool::new(false),
            cooldown_save_lock: Mutex::new(()),
            pool_offsets: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            persist_locks: Mutex::new(HashMap::new()),
            refresh_state: Mutex::new(refresh::RefreshState::default()),
            selector_config: Mutex::new(selector_config),
            home: home::HomeState::default(),
        };
        Manager {
            core: Arc::new(core),
        }
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.clock.read().now()
    }

    pub(crate) fn cfg(&self) -> Arc<Config> {
        self.config.read().clone()
    }

    // ---- Configuration ----

    /// Applies a config snapshot: retry settings, OAuth model aliases, selector (rebuilt only when
    /// strategy/affinity settings changed), cooldown policy for already-registered credentials
    /// (Go: SetConfig + SetOAuthModelAlias + SetRetryConfig + selector rebuild).
    pub fn set_config(&self, cfg: Arc<Config>) {
        self.set_retry_config(
            cfg.request_retry,
            Duration::from_secs(cfg.max_retry_interval.max(0) as u64),
            cfg.max_retry_credentials,
        );
        self.set_oauth_model_alias(&cfg.oauth_model_alias);
        let previous = self.cfg();
        if home_session_alias::home_alias_ttl_changed(&previous, &cfg) {
            self.home.aliases.lock().clear();
        }
        *self.config.write() = cfg.clone();
        if !cfg.home.enabled {
            self.clear_home_runtime_auths();
        }
        self.apply_selector_config(&cfg);
        if self.clear_disabled_cooldown_states() {
            self.persist_cooldown_states_detached();
        }
    }

    /// Credential retry rounds, per-round credential limit and cooldown wait ceiling.
    pub fn set_retry_config(
        &self,
        retry: i64,
        max_retry_interval: Duration,
        max_retry_credentials: i64,
    ) {
        *self.retry.write() = RetrySettings {
            request_retry: retry.max(0),
            max_retry_credentials: max_retry_credentials.max(0),
            max_retry_interval,
        };
    }

    pub(crate) fn retry_settings(&self) -> RetrySettings {
        *self.retry.read()
    }

    /// Replaces the global OAuth model alias table (channel -> aliases).
    pub fn set_oauth_model_alias(
        &self,
        aliases: &std::collections::BTreeMap<String, Vec<cpa_config::OAuthModelAlias>>,
    ) {
        *self.oauth_alias.write() = Arc::new(models::compile_oauth_model_alias_table(aliases));
    }

    /// Go: RefreshAPIKeyModelAlias. Alias and capability tables are derived from the config
    /// snapshot on demand here, so there is nothing to rebuild; kept for call-site parity.
    pub fn refresh_api_key_model_alias(&self) {}

    fn apply_selector_config(&self, cfg: &Config) {
        // Valid TTLs are clamped up to 1s; empty/invalid/non-positive means the 1h default.
        let ttl = cpa_config::GoDuration::parse(cfg.routing.session_affinity_ttl.trim())
            .ok()
            .filter(|d| d.0 > 0)
            .map_or(Duration::from_secs(3600), |d| {
                d.to_std().max(Duration::from_secs(1))
            });
        let session_affinity = cfg.routing.session_affinity;
        let next = SelectorConfig {
            strategy: Strategy::parse(&cfg.routing.strategy),
            session_affinity,
            affinity_ttl: ttl,
            // The subagent switch only matters (and only compares) with affinity on.
            subagent_affinity: !session_affinity
                || cfg.routing.session_affinity_subagents.unwrap_or(true),
        };
        let mut current = self.selector_config.lock();
        if *current == next {
            return;
        }
        *current = next;
        *self.selector.write() = Arc::new(Selector::new(next, self.clock.read().clone()));
    }

    /// Swaps the clock (also used by the selector's session cache on the next rebuild).
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        *self.clock.write() = clock.clone();
        let config = *self.selector_config.lock();
        *self.selector.write() = Arc::new(Selector::new(config, clock));
    }

    /// Global switch for cooldown scheduling (Go: SetQuotaCooldownDisabled).
    pub fn set_quota_cooldown_disabled(&self, disabled: bool) {
        self.cooldown_disabled
            .store(disabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Usage accounting sink; one record per finished upstream attempt.
    pub fn set_usage_tracker(&self, tracker: Option<Arc<UsageTracker>>) {
        *self.usage.write() = tracker;
    }

    pub fn set_hook(&self, hook: Option<Arc<dyn Hook>>) {
        *self.hook.write() = hook;
    }

    pub fn set_result_policy(&self, policy: Option<Arc<dyn ResultPolicy>>) {
        *self.result_policy.write() = policy;
    }

    /// Receives the JSON event of each failed attempt (request-log feed).
    pub fn set_error_event_sink(&self, sink: Option<ErrorEventSink>) {
        *self.error_sink.write() = sink;
    }

    /// Token store credentials are persisted to on register/update/refresh. Without one the
    /// manager keeps credentials in memory only.
    pub fn set_store(&self, store: Option<Arc<dyn Store>>) {
        *self.store.write() = store;
    }

    pub fn selector(&self) -> Arc<Selector> {
        self.selector.read().clone()
    }

    // ---- Executors ----

    /// Register (or replace) the executor for `executor.identifier()`.
    pub fn register_executor(&self, executor: DynExecutor) {
        let provider = executor.identifier().trim().to_string();
        if provider.is_empty() {
            return;
        }
        let (replaced, to_reschedule) = {
            let mut st = self.state.write();
            let replaced = st.executors.insert(provider.clone(), executor.clone());
            let ids: Vec<String> = st
                .auths
                .values()
                .filter(|a| models::executor_key_from_auth(a).eq_ignore_ascii_case(&provider))
                .map(|a| a.id.clone())
                .collect();
            (replaced, ids)
        };
        for id in to_reschedule {
            self.queue_refresh_reschedule(&id);
        }
        if let Some(old) = replaced
            && !Arc::ptr_eq(&old, &executor)
        {
            close_all_sessions(old);
        }
    }

    /// Removes the executor registered under `provider`.
    pub fn unregister_executor(&self, provider: &str) {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return;
        }
        self.state.write().executors.remove(&provider);
    }

    /// The executor for a provider key (kimi domain spellings fold to `kimi`).
    pub fn executor(&self, provider: &str) -> Option<DynExecutor> {
        executor_locked(&self.state.read(), provider)
    }

    /// Asks every executor to release the execution session (e.g. pooled websockets).
    pub async fn close_execution_session(&self, session_id: &str) {
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return;
        }
        self.close_home_execution_session(&session_id);
        let executors: Vec<DynExecutor> = self.state.read().executors.values().cloned().collect();
        for e in executors {
            e.close_execution_session(&session_id).await;
        }
    }

    /// Hook for the service after a credential was removed: releases the Codex / xAI websocket
    /// sessions that may still be bound to it (`remove` already asks the executor to release all
    /// sessions; this covers executors registered for those providers explicitly).
    pub async fn auth_removed(&self, _auth_id: &str, provider: &str) {
        if matches!(provider.trim().to_lowercase().as_str(), "codex" | "xai")
            && let Some(exec) = self.executor(provider)
        {
            exec.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS_ID)
                .await;
        }
    }

    /// Go: SupportsApplyPatchForProviders; every routing candidate must support the tool.
    pub fn supports_apply_patch_for_providers(&self, providers: &[String], model: &str) -> bool {
        if providers.is_empty() {
            return false;
        }
        providers.iter().all(|p| {
            self.executor(p)
                .is_some_and(|e| e.supports_apply_patch(model))
        })
    }

    // ---- Execution entry points ----

    /// Non-streaming execution across the candidate `providers` (Go: Manager.Execute).
    pub async fn execute(
        &self,
        providers: &[String],
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        self.execute_unary(exec::Kind::Execute, providers, req, opts)
            .await
    }

    /// Streaming execution; failover is only possible before the first chunk. When every
    /// credential fails during bootstrap the error is returned as a one-chunk stream carrying
    /// the upstream headers (Go behavior), otherwise as `Err`.
    pub async fn execute_stream(
        &self,
        providers: &[String],
        req: Request,
        opts: Options,
    ) -> Result<StreamResult, ExecError> {
        self.execute_stream_rounds(providers, req, opts).await
    }

    /// Token counting (Go: Manager.ExecuteCount).
    pub async fn execute_count(
        &self,
        providers: &[String],
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        self.execute_unary(exec::Kind::Count, providers, req, opts)
            .await
    }

    // ---- Snapshots ----

    /// Snapshot of all credentials with live status/cooldown state.
    pub fn list(&self) -> Vec<Auth> {
        self.state.read().auths.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Auth> {
        if id.is_empty() {
            return None;
        }
        self.state.read().auths.get(id).cloned()
    }

    /// Providers with at least one non-disabled credential (sorted).
    pub fn available_providers(&self) -> Vec<String> {
        let st = self.state.read();
        let mut out: Vec<String> = st
            .auths
            .values()
            .filter(|a| !cooldown::is_disabled(a))
            .map(|a| models::canonical_scheduling_provider(&a.provider))
            .filter(|p| !p.is_empty())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        out.sort();
        out
    }

    pub fn has_provider_auth(&self, provider: &str) -> bool {
        let key = models::canonical_scheduling_provider(provider);
        if key.is_empty() {
            return false;
        }
        self.state.read().auths.values().any(|a| {
            !cooldown::is_disabled(a) && models::canonical_scheduling_provider(&a.provider) == key
        })
    }

    /// Side-effect free lookup of the session-affinity binding (Go: LookupSessionAffinity).
    /// Returns `(auth, status)`; status is `bound`, `unbound`, `ambiguous` or `unsupported`.
    pub fn lookup_session_affinity(
        &self,
        provider: &str,
        model: &str,
        session_id: &str,
    ) -> (Option<Auth>, &'static str) {
        let selector = self.selector();
        let Some(affinity) = selector.affinity() else {
            return (None, "unsupported");
        };
        let providers: HashMap<String, String> = self
            .state
            .read()
            .auths
            .iter()
            .map(|(id, a)| (id.clone(), a.provider.clone()))
            .collect();
        let filter =
            |id: &str| provider == "mixed" || providers.get(id).is_some_and(|p| p == provider);
        let (auth_id, status) = affinity.lookup(provider, model, session_id, Some(&filter));
        if status != "bound" || auth_id.is_empty() {
            return (None, status);
        }
        match self.get(&auth_id) {
            Some(a) if provider == "mixed" || a.provider == provider => (Some(a), "bound"),
            _ => (None, "unbound"),
        }
    }

    /// Upstream model for a credential after prefix/alias resolution (Go: ResolveExecutionModel).
    pub fn resolve_execution_model(&self, auth: &Auth, route_model: &str) -> String {
        let route_model = route_model.trim();
        let (candidates, _, _) = self.execution_model_candidates_with_alias(auth, route_model);
        candidates
            .first()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| route_model.to_string())
    }
}

pub(crate) fn executor_locked(st: &State, provider: &str) -> Option<DynExecutor> {
    let provider = provider.trim();
    if provider.is_empty() {
        return None;
    }
    if let Some(e) = st.executors.get(provider) {
        return Some(e.clone());
    }
    let lower = provider.to_lowercase();
    if lower != provider
        && let Some(e) = st.executors.get(&lower)
    {
        return Some(e.clone());
    }
    if matches!(lower.as_str(), "kimi-ai" | "kimi.ai" | "kimi.com") {
        return st.executors.get("kimi").cloned();
    }
    None
}

fn close_all_sessions(executor: DynExecutor) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            executor
                .close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS_ID)
                .await;
        });
    }
}

/// Request metadata value as trimmed string (strings only).
pub(crate) fn meta_trimmed(meta: &Metadata, key: &str) -> String {
    meta.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
