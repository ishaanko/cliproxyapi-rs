//! The global model registry (Go: registry/model_registry.go).
//!
//! Tracks which clients (one per auth) provide which models, with reference counts, per-provider
//! capability variants, quota-exceeded windows and suspensions. A model is hidden from listings
//! once no client can serve it. All state lives behind one `RwLock`; `generation` and
//! `registration_epoch` let callers invalidate their own list caches.
//!
//! Differences from Go: model listings are sorted by id (Go iterates a map, so order was random),
//! hooks run in order on one shared worker thread instead of a goroutine each (no context/timeout), and
//! `ModelInfo` slices hold values rather than nullable pointers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use cpa_json::{Map, Value};
use parking_lot::RwLock;

use super::definitions::lookup_static_model_info;
use super::model_info::{
    DEFAULT_CLAUDE_MAX_INPUT_TOKENS, DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS, ModelInfo,
    NativeCapabilities,
};
use crate::misc::log_credential_separator;

/// How long a client stays "quota exceeded" for a model after being marked.
const MODEL_QUOTA_EXCEEDED_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Optional callbacks for observing model list changes. Implementations must be non-blocking and
/// resilient; calls run on a separate thread and panics are caught and logged.
pub trait ModelRegistryHook: Send + Sync {
    fn on_models_registered(&self, provider: &str, client_id: &str, models: Vec<ModelInfo>);
    fn on_models_unregistered(&self, provider: &str, client_id: &str);
}

type HookJob = Box<dyn FnOnce() + Send>;

/// Queues a hook notification on the single shared worker thread (started on first use), so
/// notifications never block the registry and run in the order they were issued.
fn run_hook_job(job: impl FnOnce() + Send + 'static) {
    static WORKER: LazyLock<std::sync::mpsc::Sender<HookJob>> = LazyLock::new(|| {
        let (tx, rx) = std::sync::mpsc::channel::<HookJob>();
        std::thread::Builder::new()
            .name("model-registry-hooks".into())
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .expect("spawn model registry hook worker");
        tx
    });
    let _ = WORKER.send(Box::new(job));
}

/// A model's availability record.
#[derive(Debug, Clone)]
struct ModelRegistration {
    /// Model metadata (last registered).
    info: ModelInfo,
    /// Provider-specific variants to support differing capabilities.
    info_by_provider: HashMap<String, ModelInfo>,
    /// Number of active client bindings that can provide this model.
    count: i64,
    last_updated: Instant,
    /// Clients that exceeded quota for this model, with the time it was marked.
    quota_exceeded_clients: HashMap<String, Instant>,
    /// Available bindings grouped by provider.
    providers: HashMap<String, i64>,
    /// Temporarily disabled clients keyed by client id (value is the reason).
    suspended_clients: HashMap<String, String>,
}

/// One registered route to a public model, for native capability resolution.
#[derive(Debug, Clone, Default)]
pub struct NativeCapabilityRoute {
    pub provider: String,
    pub native_capabilities: Option<NativeCapabilities>,
}

/// Desired availability and quota state for a client's model.
#[derive(Debug, Clone, Default)]
pub struct ClientModelProjection {
    pub model_id: String,
    pub suspended: bool,
    pub suspend_reason: String,
    pub quota_exceeded: bool,
}

struct AvailableModelsCacheEntry {
    models: Vec<Map<String, Value>>,
    expires_at: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// model id -> registration
    models: HashMap<String, ModelRegistration>,
    /// client id -> registered model ids (raw order, duplicates allowed and counted)
    client_models: HashMap<String, Vec<String>>,
    /// client id -> model id -> the client's own model info
    client_model_infos: HashMap<String, HashMap<String, ModelInfo>>,
    /// client id -> provider identifier
    client_providers: HashMap<String, String>,
    /// monotonic registration epoch per client id
    client_epochs: HashMap<String, u64>,
    /// latest applied projection generation per client id
    client_generations: HashMap<String, u64>,
    /// per-handler snapshots for `get_available_models`
    available_models_cache: HashMap<String, AvailableModelsCacheEntry>,
    /// changes in model registrations and availability
    generation: u64,
    hook: Option<Arc<dyn ModelRegistryHook>>,
}

/// Registry of available models. Use [`global_registry`] for the process-wide instance.
#[derive(Default)]
pub struct ModelRegistry {
    state: RwLock<State>,
    /// Monotonic count of client registration and deregistration structural changes.
    registration_epoch: AtomicU64,
}

static GLOBAL_REGISTRY: LazyLock<ModelRegistry> = LazyLock::new(ModelRegistry::default);

/// The process-wide model registry.
pub fn global_registry() -> &'static ModelRegistry {
    &GLOBAL_REGISTRY
}

/// Looks up model metadata: dynamic registry (provider-specific, then global) before static
/// definitions. `provider` is trimmed and lowercased.
pub fn lookup_model_info(model_id: &str, provider: Option<&str>) -> Option<ModelInfo> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    let provider = provider
        .map(|p| p.trim().to_lowercase())
        .unwrap_or_default();
    global_registry()
        .get_model_info(model_id, &provider)
        .or_else(|| lookup_static_model_info(model_id))
}

/// Resolves native web search across every route that can serve a public model, conservatively:
/// a known unsupported route or an explicit model-level `false` wins; missing or unknown data
/// yields `None` (unknown).
pub fn resolve_responses_web_search_capability(routes: &[NativeCapabilityRoute]) -> Option<bool> {
    if routes.is_empty() {
        return None;
    }
    let mut has_unknown = false;
    for route in routes {
        let explicit = route
            .native_capabilities
            .as_ref()
            .and_then(|c| c.web_search);
        if explicit == Some(false) {
            return Some(false);
        }
        match responses_web_search_provider_path_support(&route.provider) {
            None => {
                has_unknown = true;
                continue;
            }
            Some(false) => return Some(false),
            Some(true) => {}
        }
        if explicit.is_none() {
            has_unknown = true;
        }
    }
    if has_unknown { None } else { Some(true) }
}

fn responses_web_search_provider_path_support(provider: &str) -> Option<bool> {
    let provider = provider.trim().to_lowercase();
    match provider.as_str() {
        "codex" | "xai" | "claude" | "antigravity" => Some(true),
        "openai"
        | "openai-compatibility"
        | "gemini"
        | "aistudio"
        | "vertex"
        | "kimi"
        | "kimi-ai"
        | "kimi.ai"
        | "kimi.com"
        | "interactions"
        | "gemini-interactions" => Some(false),
        p if p.starts_with("openai-compatible-") => Some(false),
        _ => None,
    }
}

/// `config.override_header` of the model (trimmed non-empty keys), `None` when there is none.
pub fn model_override_headers(
    model_id: &str,
    provider: Option<&str>,
) -> Option<HashMap<String, String>> {
    let info = lookup_model_info(model_id, provider)?;
    let config = info.config?;
    let out: HashMap<String, String> = config
        .override_header
        .into_iter()
        .filter_map(|(k, v)| {
            let k = k.trim().to_string();
            (!k.is_empty()).then_some((k, v))
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Availability of a registration at `now`, plus when the earliest quota window recovers.
///
/// Quota windows and quota-reason suspensions leave a model listed (only hard suspensions hide
/// it); `expires_at` tells list caches when to refresh.
fn model_registration_availability(
    registration: &ModelRegistration,
    now: Instant,
) -> (bool, Option<Instant>) {
    let available_clients = registration.count;
    let mut expired_clients = 0i64;
    let mut expires_at: Option<Instant> = None;
    for quota_time in registration.quota_exceeded_clients.values() {
        let recovery_at = *quota_time + MODEL_QUOTA_EXCEEDED_WINDOW;
        if now < recovery_at {
            expired_clients += 1;
            if expires_at.is_none_or(|e| recovery_at < e) {
                expires_at = Some(recovery_at);
            }
        }
    }

    let mut cooldown_suspended = 0i64;
    let mut other_suspended = 0i64;
    let mut quota_and_other_suspended = 0i64;
    for (client_id, reason) in &registration.suspended_clients {
        if reason.eq_ignore_ascii_case("quota") {
            cooldown_suspended += 1;
            continue;
        }
        other_suspended += 1;
        if registration
            .quota_exceeded_clients
            .get(client_id)
            .is_some_and(|q| now < *q + MODEL_QUOTA_EXCEEDED_WINDOW)
        {
            quota_and_other_suspended += 1;
        }
    }

    // A credential-wide quota can mark the same client both quota-exceeded and suspended; count
    // that unavailable client only once.
    let effective_clients =
        (available_clients - expired_clients - other_suspended + quota_and_other_suspended).max(0);
    let available = effective_clients > 0
        || (available_clients > 0
            && (expired_clients > 0 || cooldown_suspended > 0)
            && other_suspended == 0);
    (available, expires_at)
}

impl ModelRegistry {
    /// A fresh empty registry (the process normally uses [`global_registry`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Current generation counter of model registrations/availability.
    pub fn get_generation(&self) -> u64 {
        self.state.read().generation
    }

    /// Epoch incremented whenever client registrations or deregistrations occur.
    pub fn registration_epoch(&self) -> u64 {
        self.registration_epoch.load(Ordering::SeqCst)
    }

    /// Resolves native web search capability across every registered route for the exact public
    /// model id (see [`resolve_responses_web_search_capability`]).
    pub fn get_responses_web_search_capability(&self, model_id: &str) -> Option<bool> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return None;
        }
        let state = self.state.read();
        let mut routes = Vec::new();
        for (client_id, model_ids) in &state.client_models {
            for registered_id in model_ids {
                if registered_id.trim() != model_id {
                    continue;
                }
                let native_capabilities = state
                    .client_model_infos
                    .get(client_id)
                    .and_then(|infos| infos.get(registered_id))
                    .and_then(|info| info.native_capabilities.clone());
                routes.push(NativeCapabilityRoute {
                    provider: state
                        .client_providers
                        .get(client_id)
                        .cloned()
                        .unwrap_or_default(),
                    native_capabilities,
                });
            }
        }
        resolve_responses_web_search_capability(&routes)
    }

    /// Sets (or clears) the hook observing registration changes.
    pub fn set_hook(&self, hook: Option<Arc<dyn ModelRegistryHook>>) {
        self.state.write().hook = hook;
    }

    fn trigger_models_registered(
        state: &State,
        provider: &str,
        client_id: &str,
        models: &[ModelInfo],
    ) {
        let Some(hook) = state.hook.clone() else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        let models_copy: Vec<ModelInfo> = models
            .iter()
            .filter(|m| !m.id.is_empty() && seen.insert(m.id.clone()))
            .cloned()
            .collect();
        let (provider, client_id) = (provider.to_string(), client_id.to_string());
        run_hook_job(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook.on_models_registered(&provider, &client_id, models_copy)
            }));
            if result.is_err() {
                tracing::error!("model registry hook OnModelsRegistered panic");
            }
        });
    }

    fn trigger_models_unregistered(state: &State, provider: &str, client_id: &str) {
        let Some(hook) = state.hook.clone() else {
            return;
        };
        let (provider, client_id) = (provider.to_string(), client_id.to_string());
        run_hook_job(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook.on_models_unregistered(&provider, &client_id)
            }));
            if result.is_err() {
                tracing::error!("model registry hook OnModelsUnregistered panic");
            }
        });
    }

    /// Registers a client (one per auth) and the models it provides, replacing any previous
    /// registration of the same client by diffing: counters and provider tallies are adjusted for
    /// added/removed/provider-changed models and transient suspension/quota state of surviving
    /// bindings is reset. A list without any model id unregisters the client. The client's epoch is
    /// bumped and its projection generation reset to 0.
    pub fn register_client(&self, client_id: &str, client_provider: &str, models: &[ModelInfo]) {
        let mut guard = self.state.write();
        let state = &mut *guard;

        let provider = client_provider.to_lowercase();
        let mut unique_model_ids: Vec<String> = Vec::with_capacity(models.len());
        let mut raw_model_ids: Vec<String> = Vec::with_capacity(models.len());
        let mut new_models: HashMap<String, &ModelInfo> = HashMap::with_capacity(models.len());
        let mut new_counts: HashMap<String, i64> = HashMap::with_capacity(models.len());
        for model in models {
            if model.id.is_empty() {
                continue;
            }
            raw_model_ids.push(model.id.clone());
            *new_counts.entry(model.id.clone()).or_insert(0) += 1;
            if new_models.contains_key(&model.id) {
                continue;
            }
            new_models.insert(model.id.clone(), model);
            unique_model_ids.push(model.id.clone());
        }

        if unique_model_ids.is_empty() {
            // No models supplied: unregister existing client state if present.
            self.unregister_client_internal(state, client_id);
            state.client_models.remove(client_id);
            state.client_model_infos.remove(client_id);
            state.client_providers.remove(client_id);
            Self::invalidate_available_models_cache(state);
            log_credential_separator();
            return;
        }

        // Monotonically increment the client registration epoch and reset its generation to 0.
        *state
            .client_epochs
            .entry(client_id.to_string())
            .or_insert(0) += 1;
        state.client_generations.insert(client_id.to_string(), 0);
        self.registration_epoch.fetch_add(1, Ordering::SeqCst);

        let now = Instant::now();

        let old_models = state.client_models.get(client_id).cloned();
        let old_provider = state
            .client_providers
            .get(client_id)
            .cloned()
            .unwrap_or_default();
        let provider_changed = old_provider != provider;

        let client_infos_snapshot =
            |new_models: &HashMap<String, &ModelInfo>| -> HashMap<String, ModelInfo> {
                new_models
                    .iter()
                    .map(|(id, m)| (id.clone(), (*m).clone()))
                    .collect()
            };

        let Some(old_models) = old_models else {
            // Pure addition path.
            for model_id in &raw_model_ids {
                let model = new_models[model_id];
                Self::add_model_registration(state, model_id, &provider, model, now, client_id);
            }
            state
                .client_models
                .insert(client_id.to_string(), raw_model_ids.clone());
            state
                .client_model_infos
                .insert(client_id.to_string(), client_infos_snapshot(&new_models));
            if provider.is_empty() {
                state.client_providers.remove(client_id);
            } else {
                state
                    .client_providers
                    .insert(client_id.to_string(), provider.clone());
            }
            Self::invalidate_available_models_cache(state);
            Self::trigger_models_registered(state, &provider, client_id, models);
            tracing::debug!(
                "Registered client {client_id} from provider {client_provider} with {} models",
                raw_model_ids.len()
            );
            log_credential_separator();
            return;
        };

        let mut old_counts: HashMap<String, i64> = HashMap::with_capacity(old_models.len());
        for id in &old_models {
            *old_counts.entry(id.clone()).or_insert(0) += 1;
        }

        let added: Vec<&String> = unique_model_ids
            .iter()
            .filter(|id| old_counts.get(*id).copied().unwrap_or(0) == 0)
            .collect();
        let removed: Vec<String> = old_counts
            .keys()
            .filter(|id| new_counts.get(*id).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();

        // Provider change for overlapping models: move tallies off the old provider first.
        if provider_changed && !old_provider.is_empty() {
            for (id, &new_count) in &new_counts {
                if new_count == 0 {
                    continue;
                }
                let old_count = old_counts.get(id).copied().unwrap_or(0);
                if old_count == 0 {
                    continue;
                }
                let to_remove = new_count.min(old_count);
                let Some(count) = state
                    .models
                    .get(id)
                    .and_then(|reg| reg.providers.get(&old_provider))
                    .copied()
                else {
                    continue;
                };
                if count <= to_remove {
                    if let Some(reg) = state.models.get_mut(id) {
                        reg.providers.remove(&old_provider);
                        reg.info_by_provider.remove(&old_provider);
                    }
                } else {
                    let web_search =
                        Self::has_client_supporting_web_search(state, id, &old_provider, client_id);
                    if let Some(reg) = state.models.get_mut(id) {
                        reg.providers
                            .insert(old_provider.clone(), count - to_remove);
                        if let Some(info) = reg.info_by_provider.get_mut(&old_provider) {
                            info.supports_web_search = web_search;
                        }
                    }
                }
            }
        }

        // Apply removals first to keep counters accurate.
        for id in &removed {
            for _ in 0..old_counts[id] {
                Self::remove_model_registration(state, client_id, id, &old_provider, now);
            }
        }
        for (id, &old_count) in &old_counts {
            let new_count = new_counts.get(id).copied().unwrap_or(0);
            if new_count == 0 || old_count <= new_count {
                continue;
            }
            for _ in 0..(old_count - new_count) {
                Self::remove_model_registration(state, client_id, id, &old_provider, now);
            }
        }

        // Apply additions.
        for (id, &new_count) in &new_counts {
            let old_count = old_counts.get(id).copied().unwrap_or(0);
            if new_count <= old_count {
                continue;
            }
            let model = new_models[id];
            for _ in 0..(new_count - old_count) {
                Self::add_model_registration(state, id, &provider, model, now, client_id);
            }
        }

        // Update metadata for models that remain associated with the client.
        let added_set: std::collections::HashSet<&String> = added.iter().copied().collect();
        for id in &unique_model_ids {
            let model = new_models[id];
            if !state.models.contains_key(id) {
                continue;
            }
            let has_web_search = model.supports_web_search
                || Self::has_client_supporting_web_search(state, id, "", client_id);
            let has_prov_web_search = model.supports_web_search
                || Self::has_client_supporting_web_search(state, id, &provider, client_id);
            let old_provider_web_search =
                (provider_changed && !old_provider.is_empty()).then(|| {
                    Self::has_client_supporting_web_search(state, id, &old_provider, client_id)
                });

            let Some(reg) = state.models.get_mut(id) else {
                continue;
            };
            reg.info = model.clone();
            reg.info.supports_web_search = has_web_search;
            if !provider.is_empty() {
                let mut info = model.clone();
                info.supports_web_search = has_prov_web_search;
                reg.info_by_provider.insert(provider.clone(), info);
            }
            if let Some(web_search) = old_provider_web_search
                && let Some(info) = reg.info_by_provider.get_mut(&old_provider)
            {
                info.supports_web_search = web_search;
            }
            reg.last_updated = now;
            // Re-registering an existing client/model binding starts a fresh registry snapshot for
            // that binding: cooldown and suspension are transient scheduling state and must not
            // survive this reconciliation step.
            reg.quota_exceeded_clients.remove(client_id);
            reg.suspended_clients.remove(client_id);
            if provider_changed && !provider.is_empty() {
                if added_set.contains(id) {
                    continue;
                }
                let overlap_count = new_counts[id].min(old_counts.get(id).copied().unwrap_or(0));
                if overlap_count <= 0 {
                    continue;
                }
                *reg.providers.entry(provider.clone()).or_insert(0) += overlap_count;
            }
        }

        // Update client bookkeeping.
        if !raw_model_ids.is_empty() {
            state
                .client_models
                .insert(client_id.to_string(), raw_model_ids.clone());
        }
        state
            .client_model_infos
            .insert(client_id.to_string(), client_infos_snapshot(&new_models));
        if provider.is_empty() {
            state.client_providers.remove(client_id);
        } else {
            state
                .client_providers
                .insert(client_id.to_string(), provider.clone());
        }

        Self::invalidate_available_models_cache(state);
        Self::trigger_models_registered(state, &provider, client_id, models);
        if added.is_empty() && removed.is_empty() && !provider_changed {
            // Only metadata (e.g. display name) changed; keep no-op re-registration quiet.
            return;
        }
        tracing::debug!(
            "Reconciled client {client_id} (provider {provider}) models: +{}, -{}",
            added.len(),
            removed.len()
        );
        log_credential_separator();
    }

    fn invalidate_available_models_cache(state: &mut State) {
        state.generation += 1;
        state.available_models_cache.clear();
    }

    fn add_model_registration(
        state: &mut State,
        model_id: &str,
        provider: &str,
        model: &ModelInfo,
        now: Instant,
        exclude_client_id: &str,
    ) {
        if model_id.is_empty() {
            return;
        }
        if state.models.contains_key(model_id) {
            let has_web_search = model.supports_web_search
                || Self::has_client_supporting_web_search(state, model_id, "", exclude_client_id);
            let has_prov_web_search = model.supports_web_search
                || Self::has_client_supporting_web_search(
                    state,
                    model_id,
                    provider,
                    exclude_client_id,
                );
            let Some(existing) = state.models.get_mut(model_id) else {
                return;
            };
            existing.count += 1;
            existing.last_updated = now;
            existing.info = model.clone();
            existing.info.supports_web_search = has_web_search;
            if !provider.is_empty() {
                *existing.providers.entry(provider.to_string()).or_insert(0) += 1;
                let mut info = model.clone();
                info.supports_web_search = has_prov_web_search;
                existing.info_by_provider.insert(provider.to_string(), info);
            }
            tracing::debug!(
                "Incremented count for model {model_id}, now {} clients",
                existing.count
            );
            return;
        }

        let mut registration = ModelRegistration {
            info: model.clone(),
            info_by_provider: HashMap::new(),
            count: 1,
            last_updated: now,
            quota_exceeded_clients: HashMap::new(),
            providers: HashMap::new(),
            suspended_clients: HashMap::new(),
        };
        if !provider.is_empty() {
            registration.providers.insert(provider.to_string(), 1);
            registration
                .info_by_provider
                .insert(provider.to_string(), model.clone());
        }
        state.models.insert(model_id.to_string(), registration);
        tracing::debug!("Registered new model {model_id} from provider {provider}");
    }

    fn remove_model_registration(
        state: &mut State,
        client_id: &str,
        model_id: &str,
        provider: &str,
        now: Instant,
    ) {
        let Some(registration) = state.models.get_mut(model_id) else {
            return;
        };
        registration.count -= 1;
        registration.last_updated = now;
        registration.quota_exceeded_clients.remove(client_id);
        registration.suspended_clients.remove(client_id);
        if registration.count < 0 {
            registration.count = 0;
        }
        if !provider.is_empty()
            && let Some(&count) = registration.providers.get(provider)
        {
            if count <= 1 {
                registration.providers.remove(provider);
                registration.info_by_provider.remove(provider);
            } else {
                registration
                    .providers
                    .insert(provider.to_string(), count - 1);
            }
        }
        tracing::debug!(
            "Decremented count for model {model_id}, now {} clients",
            registration.count
        );
        if registration.count <= 0 {
            state.models.remove(model_id);
            tracing::debug!("Removed model {model_id} as no clients remain");
            return;
        }
        let web_search = Self::has_client_supporting_web_search(state, model_id, "", client_id);
        let prov_web_search = (!provider.is_empty())
            .then(|| Self::has_client_supporting_web_search(state, model_id, provider, client_id));
        if let Some(registration) = state.models.get_mut(model_id) {
            registration.info.supports_web_search = web_search;
            if let (Some(ws), Some(info)) = (
                prov_web_search,
                registration.info_by_provider.get_mut(provider),
            ) {
                info.supports_web_search = ws;
            }
        }
    }

    /// Removes a client and decrements counts for its models.
    pub fn unregister_client(&self, client_id: &str) {
        let mut guard = self.state.write();
        let state = &mut *guard;
        self.unregister_client_internal(state, client_id);
        Self::invalidate_available_models_cache(state);
    }

    fn unregister_client_internal(&self, state: &mut State, client_id: &str) {
        *state
            .client_epochs
            .entry(client_id.to_string())
            .or_insert(0) += 1;
        *state
            .client_generations
            .entry(client_id.to_string())
            .or_insert(0) += 1;
        self.registration_epoch.fetch_add(1, Ordering::SeqCst);

        let provider = state.client_providers.get(client_id).cloned();
        let Some(models) = state.client_models.get(client_id).cloned() else {
            state.client_providers.remove(client_id);
            return;
        };

        let now = Instant::now();
        for model_id in &models {
            let Some(registration) = state.models.get_mut(model_id) else {
                continue;
            };
            registration.count -= 1;
            registration.last_updated = now;
            registration.quota_exceeded_clients.remove(client_id);
            registration.suspended_clients.remove(client_id);

            if let Some(provider) = &provider
                && let Some(&count) = registration.providers.get(provider)
            {
                if count <= 1 {
                    registration.providers.remove(provider);
                    registration.info_by_provider.remove(provider);
                } else {
                    registration.providers.insert(provider.clone(), count - 1);
                }
            }
            tracing::debug!(
                "Decremented count for model {model_id}, now {} clients",
                registration.count
            );

            if registration.count <= 0 {
                state.models.remove(model_id);
                tracing::debug!("Removed model {model_id} as no clients remain");
            } else {
                let web_search =
                    Self::has_client_supporting_web_search(state, model_id, "", client_id);
                let prov_web_search = provider
                    .as_deref()
                    .map(|p| Self::has_client_supporting_web_search(state, model_id, p, client_id));
                if let Some(registration) = state.models.get_mut(model_id) {
                    registration.info.supports_web_search = web_search;
                    if let (Some(p), Some(ws)) = (provider.as_deref(), prov_web_search)
                        && let Some(info) = registration.info_by_provider.get_mut(p)
                    {
                        info.supports_web_search = ws;
                    }
                }
            }
        }

        state.client_models.remove(client_id);
        state.client_model_infos.remove(client_id);
        state.client_providers.remove(client_id);
        tracing::debug!("Unregistered client {client_id}");
        log_credential_separator();
        Self::trigger_models_unregistered(state, provider.as_deref().unwrap_or(""), client_id);
    }

    /// Marks a model as quota exceeded for a client (ignored for unknown models).
    pub fn set_model_quota_exceeded(&self, client_id: &str, model_id: &str) {
        let mut state = self.state.write();
        if let Some(registration) = state.models.get_mut(model_id) {
            registration
                .quota_exceeded_clients
                .insert(client_id.to_string(), Instant::now());
            Self::invalidate_available_models_cache(&mut state);
            tracing::debug!("Marked model {model_id} as quota exceeded for client {client_id}");
        }
    }

    /// Clears the quota exceeded status for a model and client.
    pub fn clear_model_quota_exceeded(&self, client_id: &str, model_id: &str) {
        let mut state = self.state.write();
        if let Some(registration) = state.models.get_mut(model_id) {
            registration.quota_exceeded_clients.remove(client_id);
            Self::invalidate_available_models_cache(&mut state);
        }
    }

    /// Atomically applies model suspension and quota exceeded state for `client_id`, provided the
    /// epoch matches the client's current epoch and the generation is not older than the applied
    /// one. Rejected (false) for unknown/unregistered clients, stale epoch/generation, or when no
    /// projected model is both owned by the client and registered. This is how conductor cooldown
    /// state hides a model from listings when all its clients are cooling.
    pub fn apply_client_model_projections(
        &self,
        client_id: &str,
        epoch: u64,
        generation: u64,
        projections: &[ClientModelProjection],
    ) -> bool {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return false;
        }
        let mut guard = self.state.write();
        let state = &mut *guard;

        let Some(registered) = state.client_models.get(client_id).filter(|m| !m.is_empty()) else {
            return false;
        };
        if epoch != state.client_epochs.get(client_id).copied().unwrap_or(0) {
            return false;
        }
        if generation
            < state
                .client_generations
                .get(client_id)
                .copied()
                .unwrap_or(0)
        {
            return false;
        }

        let registered_set: std::collections::HashSet<String> = registered
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();

        let has_valid_model = projections.iter().any(|proj| {
            let model_id = proj.model_id.trim();
            !model_id.is_empty()
                && registered_set.contains(model_id)
                && state.models.contains_key(model_id)
        });
        if !has_valid_model {
            return false;
        }

        state
            .client_generations
            .insert(client_id.to_string(), generation);

        let now = Instant::now();
        let mut changed = false;
        for proj in projections {
            let model_id = proj.model_id.trim();
            if model_id.is_empty() || !registered_set.contains(model_id) {
                continue;
            }
            let Some(registration) = state.models.get_mut(model_id) else {
                continue;
            };

            if proj.suspended {
                if registration.suspended_clients.get(client_id) != Some(&proj.suspend_reason) {
                    registration
                        .suspended_clients
                        .insert(client_id.to_string(), proj.suspend_reason.clone());
                    registration.last_updated = now;
                    changed = true;
                }
            } else if registration.suspended_clients.remove(client_id).is_some() {
                registration.last_updated = now;
                changed = true;
            }

            if proj.quota_exceeded {
                if !registration.quota_exceeded_clients.contains_key(client_id) {
                    registration
                        .quota_exceeded_clients
                        .insert(client_id.to_string(), now);
                    registration.last_updated = now;
                    changed = true;
                }
            } else if registration
                .quota_exceeded_clients
                .remove(client_id)
                .is_some()
            {
                registration.last_updated = now;
                changed = true;
            }
        }

        if changed {
            Self::invalidate_available_models_cache(state);
        }
        true
    }

    /// Applies capability mutations to every model info of `client_id` if the client is registered
    /// and its epoch equals `expected_epoch`, then refreshes the derived web-search flags. Returns
    /// whether it was applied.
    pub fn apply_client_model_capabilities(
        &self,
        client_id: &str,
        expected_epoch: u64,
        mut mutate: impl FnMut(&str, &mut ModelInfo),
    ) -> bool {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return false;
        }
        let mut guard = self.state.write();
        let state = &mut *guard;

        if state.client_epochs.get(client_id).copied() != Some(expected_epoch) {
            return false;
        }
        let Some(client_infos) = state
            .client_model_infos
            .get_mut(client_id)
            .filter(|i| !i.is_empty())
        else {
            return false;
        };
        for (id, info) in client_infos.iter_mut() {
            mutate(id, info);
        }
        let ids: Vec<String> = client_infos.keys().cloned().collect();

        let provider = state
            .client_providers
            .get(client_id)
            .cloned()
            .unwrap_or_default();
        for id in ids {
            if !state.models.contains_key(&id) {
                continue;
            }
            let has_web_search = Self::has_client_supporting_web_search(state, &id, "", "");
            let prov_web_search = (!provider.is_empty())
                .then(|| Self::has_client_supporting_web_search(state, &id, &provider, ""));
            if let Some(reg) = state.models.get_mut(&id) {
                reg.info.supports_web_search = has_web_search;
                if let (Some(ws), Some(info)) =
                    (prov_web_search, reg.info_by_provider.get_mut(&provider))
                {
                    info.supports_web_search = ws;
                }
            }
        }
        Self::invalidate_available_models_cache(state);
        true
    }

    /// Whether any client other than `exclude_client_id` (optionally restricted to `provider`)
    /// registered `model_id` with web search support.
    fn has_client_supporting_web_search(
        state: &State,
        model_id: &str,
        provider: &str,
        exclude_client_id: &str,
    ) -> bool {
        state.client_model_infos.iter().any(|(client_id, infos)| {
            if client_id == exclude_client_id {
                return false;
            }
            if !provider.is_empty()
                && state.client_providers.get(client_id).map(String::as_str) != Some(provider)
            {
                return false;
            }
            infos
                .get(model_id)
                .is_some_and(|info| info.supports_web_search)
        })
    }

    /// Marks a client's model as unavailable until explicitly resumed (no-op when already
    /// suspended, or the model/ids are unknown).
    pub fn suspend_client_model(&self, client_id: &str, model_id: &str, reason: &str) {
        if client_id.is_empty() || model_id.is_empty() {
            return;
        }
        let mut state = self.state.write();
        let Some(registration) = state.models.get_mut(model_id) else {
            return;
        };
        if registration.suspended_clients.contains_key(client_id) {
            return;
        }
        registration
            .suspended_clients
            .insert(client_id.to_string(), reason.to_string());
        registration.last_updated = Instant::now();
        Self::invalidate_available_models_cache(&mut state);
        if reason.is_empty() {
            tracing::debug!("Suspended client {client_id} for model {model_id}");
        } else {
            tracing::debug!("Suspended client {client_id} for model {model_id}: {reason}");
        }
    }

    /// Clears a previous suspension so the client counts toward availability again.
    pub fn resume_client_model(&self, client_id: &str, model_id: &str) {
        if client_id.is_empty() || model_id.is_empty() {
            return;
        }
        let mut state = self.state.write();
        let Some(registration) = state.models.get_mut(model_id) else {
            return;
        };
        if registration.suspended_clients.remove(client_id).is_none() {
            return;
        }
        registration.last_updated = Instant::now();
        Self::invalidate_available_models_cache(&mut state);
        tracing::debug!("Resumed client {client_id} for model {model_id}");
    }

    /// Whether the client registered support for `model_id` (trimmed, case-insensitive).
    pub fn client_supports_model(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.state.read();
        state.client_models.get(client_id).is_some_and(|models| {
            models
                .iter()
                .any(|id| id.trim().eq_ignore_ascii_case(model_id))
        })
    }

    /// Whether the model is currently suspended for the client.
    pub fn is_model_suspended_for_client(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.state.read();
        state
            .models
            .get(model_id)
            .is_some_and(|r| r.suspended_clients.contains_key(client_id))
    }

    /// Whether the model is currently marked quota exceeded for the client.
    pub fn is_model_quota_exceeded_for_client(&self, client_id: &str, model_id: &str) -> bool {
        let (client_id, model_id) = (client_id.trim(), model_id.trim());
        if client_id.is_empty() || model_id.is_empty() {
            return false;
        }
        let state = self.state.read();
        state
            .models
            .get(model_id)
            .is_some_and(|r| r.quota_exceeded_clients.contains_key(client_id))
    }

    /// All models with at least one available client, converted for `handler_type`:
    ///
    /// - `openai`: `{id, object, owned_by, [created, type, display_name, version, description,
    ///   context_length, max_context_length, max_completion_tokens, supported_parameters]}`
    /// - `claude`: `{id, object, owned_by, created_at (RFC3339), type: "model", display_name,
    ///   max_input_tokens, max_tokens}`
    /// - `gemini`: `{name, [version, displayName, description, inputTokenLimit, outputTokenLimit,
    ///   supportedGenerationMethods, supportedInputModalities, supportedOutputModalities]}`
    /// - anything else: `{id, object, [owned_by, type, created]}`
    ///
    /// Sorted by `id`; each map's keys are in alphabetical order, as Go serializes its maps. Cached
    /// per handler type until a registry change or the earliest quota window recovery.
    pub fn get_available_models(&self, handler_type: &str) -> Vec<Map<String, Value>> {
        let now = Instant::now();
        {
            let state = self.state.read();
            if let Some(cache) = state.available_models_cache.get(handler_type)
                && cache.expires_at.is_none_or(|e| now < e)
            {
                return cache.models.clone();
            }
        }

        let mut state = self.state.write();
        if let Some(cache) = state.available_models_cache.get(handler_type)
            && cache.expires_at.is_none_or(|e| now < e)
        {
            return cache.models.clone();
        }
        let (models, expires_at) = Self::build_available_models(&state, handler_type, now);
        state.available_models_cache.insert(
            handler_type.to_string(),
            AvailableModelsCacheEntry {
                models: models.clone(),
                expires_at,
            },
        );
        models
    }

    fn build_available_models(
        state: &State,
        handler_type: &str,
        now: Instant,
    ) -> (Vec<Map<String, Value>>, Option<Instant>) {
        let mut entries: Vec<(&String, Map<String, Value>)> =
            Vec::with_capacity(state.models.len());
        let mut expires_at: Option<Instant> = None;
        for (id, registration) in &state.models {
            let (available, registration_expires_at) =
                model_registration_availability(registration, now);
            if let Some(reg_expires) = registration_expires_at
                && expires_at.is_none_or(|e| reg_expires < e)
            {
                expires_at = Some(reg_expires);
            }
            if !available {
                continue;
            }
            entries.push((id, convert_model_to_map(&registration.info, handler_type)));
        }
        entries.sort_by(|a, b| a.0.cmp(b.0));
        (entries.into_iter().map(|(_, m)| m).collect(), expires_at)
    }

    /// Cloned metadata for all currently available models, sorted by trimmed id.
    pub fn get_available_model_infos(&self) -> Vec<ModelInfo> {
        let now = Instant::now();
        let state = self.state.read();
        let mut result: Vec<ModelInfo> = state
            .models
            .values()
            .filter(|registration| model_registration_availability(registration, now).0)
            .map(|registration| registration.info.clone())
            .collect();
        result.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
        result
    }

    /// Models available for a provider identifier (e.g. `codex`, `gemini`, `antigravity`),
    /// computed from that provider's clients only: availability accounts for those clients'
    /// quota windows and suspensions. Order is by model id.
    pub fn get_available_models_by_provider(&self, provider: &str) -> Vec<ModelInfo> {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return Vec::new();
        }
        let state = self.state.read();

        struct ProviderModel<'a> {
            count: i64,
            info: Option<&'a ModelInfo>,
        }
        let mut provider_models: HashMap<String, ProviderModel> = HashMap::new();

        // Clients in id order: the first client holding a model supplies its info (Go: map order).
        let mut provider_clients: Vec<(&String, &String)> = state.client_providers.iter().collect();
        provider_clients.sort();
        for (client_id, client_provider) in provider_clients {
            if *client_provider != provider {
                continue;
            }
            let Some(model_ids) = state.client_models.get(client_id).filter(|m| !m.is_empty())
            else {
                continue;
            };
            let client_infos = state.client_model_infos.get(client_id);
            for model_id in model_ids {
                let model_id = model_id.trim();
                if model_id.is_empty() {
                    continue;
                }
                let entry = provider_models
                    .entry(model_id.to_string())
                    .or_insert(ProviderModel {
                        count: 0,
                        info: None,
                    });
                entry.count += 1;
                if entry.info.is_none() {
                    entry.info = client_infos
                        .and_then(|infos| infos.get(model_id))
                        .or_else(|| state.models.get(model_id).map(|reg| &reg.info));
                }
            }
        }
        if provider_models.is_empty() {
            return Vec::new();
        }

        let now = Instant::now();
        let belongs_to_provider = |client_id: &str| {
            !client_id.is_empty()
                && state
                    .client_providers
                    .get(client_id)
                    .is_some_and(|p| *p == provider)
        };
        let mut result: Vec<(String, ModelInfo)> = Vec::with_capacity(provider_models.len());
        for (model_id, entry) in &provider_models {
            if entry.count <= 0 {
                continue;
            }
            let registration = state.models.get(model_id);

            let mut expired_clients = 0i64;
            let mut cooldown_suspended = 0i64;
            let mut other_suspended = 0i64;
            let mut quota_and_other_suspended = 0i64;
            if let Some(registration) = registration {
                for (client_id, quota_time) in &registration.quota_exceeded_clients {
                    if belongs_to_provider(client_id)
                        && now.saturating_duration_since(*quota_time) < MODEL_QUOTA_EXCEEDED_WINDOW
                    {
                        expired_clients += 1;
                    }
                }
                for (client_id, reason) in &registration.suspended_clients {
                    if !belongs_to_provider(client_id) {
                        continue;
                    }
                    if reason.eq_ignore_ascii_case("quota") {
                        cooldown_suspended += 1;
                        continue;
                    }
                    other_suspended += 1;
                    if registration
                        .quota_exceeded_clients
                        .get(client_id)
                        .is_some_and(|q| now < *q + MODEL_QUOTA_EXCEEDED_WINDOW)
                    {
                        quota_and_other_suspended += 1;
                    }
                }
            }

            let available_clients = entry.count;
            let effective_clients = (available_clients - expired_clients - other_suspended
                + quota_and_other_suspended)
                .max(0);
            let available = effective_clients > 0
                || (available_clients > 0
                    && (expired_clients > 0 || cooldown_suspended > 0)
                    && other_suspended == 0);
            if !available {
                continue;
            }
            if let Some(info) = entry.info {
                result.push((model_id.clone(), info.clone()));
            } else if let Some(registration) = registration {
                result.push((model_id.clone(), registration.info.clone()));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result.into_iter().map(|(_, info)| info).collect()
    }

    /// Number of available clients for a model: registered bindings minus clients inside their
    /// quota window and suspended clients (a client both suspended and quota-limited counts once).
    pub fn get_model_count(&self, model_id: &str) -> i64 {
        let state = self.state.read();
        let Some(registration) = state.models.get(model_id) else {
            return 0;
        };
        let now = Instant::now();

        let expired_clients = registration
            .quota_exceeded_clients
            .values()
            .filter(|q| now.saturating_duration_since(**q) < MODEL_QUOTA_EXCEEDED_WINDOW)
            .count() as i64;
        let suspended_clients = registration
            .suspended_clients
            .keys()
            .filter(|client_id| {
                !registration
                    .quota_exceeded_clients
                    .get(*client_id)
                    .is_some_and(|q| now < *q + MODEL_QUOTA_EXCEEDED_WINDOW)
            })
            .count() as i64;
        (registration.count - expired_clients - suspended_clients).max(0)
    }

    /// Providers currently supplying a model, ordered by binding count (descending), then name.
    pub fn get_model_providers(&self, model_id: &str) -> Vec<String> {
        let state = self.state.read();
        let Some(registration) = state.models.get(model_id) else {
            return Vec::new();
        };
        let mut providers: Vec<(&String, i64)> = registration
            .providers
            .iter()
            .filter(|(_, c)| **c > 0)
            .map(|(n, c)| (n, *c))
            .collect();
        providers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        providers.into_iter().map(|(n, _)| n.clone()).collect()
    }

    /// Model info, preferring the provider-specific definition when `provider` (already
    /// lowercased, "" for none) has live bindings, else the global (last registered) info.
    pub fn get_model_info(&self, model_id: &str, provider: &str) -> Option<ModelInfo> {
        let state = self.state.read();
        let reg = state.models.get(model_id)?;
        if !provider.is_empty()
            && reg.providers.get(provider).is_some_and(|c| *c > 0)
            && let Some(info) = reg.info_by_provider.get(provider)
        {
            return Some(info.clone());
        }
        Some(reg.info.clone())
    }

    /// Removes expired quota tracking entries.
    pub fn cleanup_expired_quotas(&self) {
        let mut guard = self.state.write();
        let state = &mut *guard;
        let now = Instant::now();
        let mut invalidated = false;
        for (model_id, registration) in state.models.iter_mut() {
            registration.quota_exceeded_clients.retain(|client_id, quota_time| {
                let expired = now.saturating_duration_since(*quota_time) >= MODEL_QUOTA_EXCEEDED_WINDOW;
                if expired {
                    invalidated = true;
                    tracing::debug!("Cleaned up expired quota tracking for model {model_id}, client {client_id}");
                }
                !expired
            });
        }
        if invalidated {
            Self::invalidate_available_models_cache(state);
        }
    }

    /// The newest (by `created`) available model with a live client for the handler type; used to
    /// resolve the model name `auto`. Models without `created` sort last, ties by id.
    pub fn get_first_available_model(&self, handler_type: &str) -> Result<String, String> {
        let mut models = self.get_available_models(handler_type);
        if models.is_empty() {
            return Err(format!(
                "no models available for handler type: {handler_type}"
            ));
        }
        let created =
            |m: &Map<String, Value>| m.get("created").and_then(Value::as_i64).unwrap_or(i64::MIN);
        models.sort_by_key(|m| std::cmp::Reverse(created(m)));
        for model in &models {
            if let Some(model_id) = model.get("id").and_then(Value::as_str)
                && self.get_model_count(model_id) > 0
            {
                return Ok(model_id.to_string());
            }
        }
        Err(format!(
            "no available clients for any model in handler type: {handler_type}"
        ))
    }

    /// Models registered for `client_id` (client-specific info preferred, global as fallback;
    /// duplicates removed) together with the client's current registration epoch.
    pub fn get_models_and_epoch_for_client(&self, client_id: &str) -> (Vec<ModelInfo>, u64) {
        let state = self.state.read();
        let epoch = state.client_epochs.get(client_id).copied().unwrap_or(0);
        let Some(model_ids) = state.client_models.get(client_id).filter(|m| !m.is_empty()) else {
            return (Vec::new(), epoch);
        };
        let client_infos = state.client_model_infos.get(client_id);

        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::with_capacity(model_ids.len());
        for model_id in model_ids {
            if !seen.insert(model_id) {
                continue;
            }
            // Prefer the client's own info to preserve the original type/owned_by.
            if let Some(info) = client_infos.and_then(|infos| infos.get(model_id)) {
                result.push(info.clone());
            } else if let Some(reg) = state.models.get(model_id) {
                result.push(reg.info.clone());
            }
        }
        (result, epoch)
    }

    /// Current registration epoch for `client_id` (0 when never registered).
    pub fn client_registration_epoch(&self, client_id: &str) -> u64 {
        self.state
            .read()
            .client_epochs
            .get(client_id)
            .copied()
            .unwrap_or(0)
    }

    /// Models registered for a specific client (empty when the client is unknown).
    pub fn get_models_for_client(&self, client_id: &str) -> Vec<ModelInfo> {
        self.get_models_and_epoch_for_client(client_id).0
    }
}

/// Converts model info to the list entry shape for a handler type (see
/// [`ModelRegistry::get_available_models`]); keys are inserted alphabetically.
fn convert_model_to_map(model: &ModelInfo, handler_type: &str) -> Map<String, Value> {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    let strings = |v: &[String]| Value::Array(v.iter().cloned().map(Value::String).collect());

    match handler_type {
        "openai" => {
            fields.push(("id", model.id.clone().into()));
            fields.push(("object", "model".into()));
            fields.push(("owned_by", model.owned_by.clone().into()));
            if model.created > 0 {
                fields.push(("created", model.created.into()));
            }
            if !model.r#type.is_empty() {
                fields.push(("type", model.r#type.clone().into()));
            }
            if !model.display_name.is_empty() {
                fields.push(("display_name", model.display_name.clone().into()));
            }
            if !model.version.is_empty() {
                fields.push(("version", model.version.clone().into()));
            }
            if !model.description.is_empty() {
                fields.push(("description", model.description.clone().into()));
            }
            if model.context_length > 0 {
                fields.push(("context_length", model.context_length.into()));
            }
            if model.max_context_length > 0 {
                fields.push(("max_context_length", model.max_context_length.into()));
            }
            if model.max_completion_tokens > 0 {
                fields.push(("max_completion_tokens", model.max_completion_tokens.into()));
            }
            if !model.supported_parameters.is_empty() {
                fields.push(("supported_parameters", strings(&model.supported_parameters)));
            }
        }
        "claude" => {
            fields.push(("id", model.id.clone().into()));
            fields.push(("object", "model".into()));
            fields.push(("owned_by", model.owned_by.clone().into()));
            if model.created > 0
                && let Some(ts) = chrono::DateTime::from_timestamp(model.created, 0)
            {
                fields.push((
                    "created_at",
                    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string().into(),
                ));
            }
            fields.push(("type", "model".into()));
            let display_name = if model.display_name.is_empty() {
                &model.id
            } else {
                &model.display_name
            };
            fields.push(("display_name", display_name.clone().into()));
            let max_input = if model.context_length > 0 {
                model.context_length
            } else {
                DEFAULT_CLAUDE_MAX_INPUT_TOKENS
            };
            let max_output = if model.max_completion_tokens > 0 {
                model.max_completion_tokens
            } else {
                DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS
            };
            fields.push(("max_input_tokens", max_input.into()));
            fields.push(("max_tokens", max_output.into()));
        }
        "gemini" => {
            let name = if model.name.is_empty() {
                &model.id
            } else {
                &model.name
            };
            fields.push(("name", name.clone().into()));
            if !model.version.is_empty() {
                fields.push(("version", model.version.clone().into()));
            }
            if !model.display_name.is_empty() {
                fields.push(("displayName", model.display_name.clone().into()));
            }
            if !model.description.is_empty() {
                fields.push(("description", model.description.clone().into()));
            }
            if model.input_token_limit > 0 {
                fields.push(("inputTokenLimit", model.input_token_limit.into()));
            }
            if model.output_token_limit > 0 {
                fields.push(("outputTokenLimit", model.output_token_limit.into()));
            }
            if !model.supported_generation_methods.is_empty() {
                fields.push((
                    "supportedGenerationMethods",
                    strings(&model.supported_generation_methods),
                ));
            }
            if !model.supported_input_modalities.is_empty() {
                fields.push((
                    "supportedInputModalities",
                    strings(&model.supported_input_modalities),
                ));
            }
            if !model.supported_output_modalities.is_empty() {
                fields.push((
                    "supportedOutputModalities",
                    strings(&model.supported_output_modalities),
                ));
            }
        }
        _ => {
            fields.push(("id", model.id.clone().into()));
            fields.push(("object", "model".into()));
            if !model.owned_by.is_empty() {
                fields.push(("owned_by", model.owned_by.clone().into()));
            }
            if !model.r#type.is_empty() {
                fields.push(("type", model.r#type.clone().into()));
            }
            if model.created != 0 {
                fields.push(("created", model.created.into()));
            }
        }
    }
    fields.sort_by(|a, b| a.0.cmp(b.0));
    fields
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

#[cfg(test)]
mod tests;
