//! Plugin model registration (Go: `adapters.go`): static models, per-auth model discovery and the
//! provider/executor ownership rules.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cpa_auth::Auth;
use cpa_core::registry::{ModelInfo as RegistryModel, ModelRegistry};
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    AuthModelRequest, ModelRegistrationRequest, ModelRegistrationResponse, ModelResponse, StaticModelRequest,
};
use cpa_translator::Format;

use crate::caps::Record;
use crate::convert::{
    auth_data_has_value, auth_data_with_defaults, host_config_summary, normalize_provider_id, plugin_auth_data_to_core_auth,
    plugin_model_to_registry, storage_json_from_auth,
};
use crate::ctx::CallCtx;
use crate::host::Host;

/// Models a plugin registered for its provider (Go: `pluginModelRegistration`).
#[derive(Debug, Clone, Default)]
pub struct ModelRegistration {
    pub plugin_id: String,
    pub provider: String,
    pub priority: i64,
    pub models: Vec<RegistryModel>,
    pub has_executor: bool,
}

/// Go `modelClientRegistration`.
pub(crate) struct ModelClientRegistration {
    pub client_id: String,
    pub provider: String,
    pub models: Vec<RegistryModel>,
}

/// Go `AuthModelResult`.
#[derive(Default)]
pub struct AuthModelResult {
    pub provider: String,
    pub models: Vec<RegistryModel>,
    pub auth: Option<Auth>,
    pub handled: bool,
    pub err: Option<String>,
}

/// `normalizeExecutorFormatName`: Go accepts any string as a format; formats the host does not
/// know cannot flow through the translator registry and are dropped.
pub(crate) fn normalize_executor_format_name(raw: &str) -> Option<Format> {
    match raw.trim().to_lowercase().as_str() {
        "" | "none" => None,
        "chat-completions" | "chat_completions" | "openai-chat-completions" | "openai_chat_completions" => Some(Format::OpenAI),
        "responses" | "openai-responses" | "openai_responses" => Some(Format::OpenAIResponse),
        "anthropic" => Some(Format::Claude),
        _ => Format::parse(raw.trim()),
    }
}

/// `normalizeExecutorFormats`: de-duplicated known formats in declaration order.
pub(crate) fn normalize_executor_formats(raw: &[String]) -> Vec<Format> {
    let mut out: Vec<Format> = Vec::new();
    for item in raw {
        if let Some(f) = normalize_executor_format_name(item)
            && !out.contains(&f)
        {
            out.push(f);
        }
    }
    out
}

impl Host {
    pub(crate) fn model_provider(&self, plugin_id: &str) -> String {
        self.state.lock().model_providers.get(plugin_id).cloned().unwrap_or_default()
    }

    pub(crate) fn model_registration(&self, plugin_id: &str) -> ModelRegistration {
        self.state.lock().model_registrations.get(plugin_id).cloned().unwrap_or_default()
    }

    /// Registered models of one provider (Go: `ModelsForProvider`).
    pub fn models_for_provider(&self, provider: &str) -> Vec<RegistryModel> {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return Vec::new();
        }
        self.state.lock().provider_models.get(&provider).cloned().unwrap_or_default()
    }

    /// Registers the static models of every plugin with `registry` (Go: `RegisterModels`).
    pub async fn register_models(self: &Arc<Self>, ctx: &CallCtx, registry: &ModelRegistry) {
        let snap = self.snapshot();
        let records = self.active_records_from(&snap);
        let mut registrations: Vec<ModelClientRegistration> = Vec::new();
        let mut next_clients: HashSet<String> = HashSet::new();
        let mut next_providers: HashMap<String, String> = HashMap::new();
        let mut next_model_registrations: HashMap<String, ModelRegistration> = HashMap::new();
        for rec in &records {
            let caps = rec.caps();
            if !caps.model_provider && !caps.model_registrar {
                continue;
            }
            if !caps.scope_allows_static_models() {
                continue;
            }
            let result = if caps.model_provider {
                self.call_static_models(ctx, rec).await.map(|r| ModelRegistrationResponse { provider: r.provider, models: r.models })
            } else {
                self.call_model_registrar(ctx, rec).await
            };
            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("pluginhost: model registrar {} failed: {e}", rec.id);
                    continue;
                }
            };
            let provider = resp.provider.trim().to_lowercase();
            if provider.is_empty() || resp.models.is_empty() {
                continue;
            }
            let models: Vec<RegistryModel> = resp
                .models
                .iter()
                .map(plugin_model_to_registry)
                .filter(|m| !m.id.trim().is_empty())
                .map(|mut m| {
                    m.id = m.id.trim().to_string();
                    m
                })
                .collect();
            if models.is_empty() {
                continue;
            }
            next_model_registrations.insert(
                rec.id.clone(),
                ModelRegistration {
                    plugin_id: rec.id.clone(),
                    provider: provider.clone(),
                    priority: rec.priority,
                    models: models.clone(),
                    has_executor: rec.caps().executor,
                },
            );
            next_providers.insert(rec.id.clone(), provider.clone());
            if !rec.caps().executor {
                let client_id = format!("plugin:{}:{}", rec.id, provider);
                next_clients.insert(client_id.clone());
                registrations.push(ModelClientRegistration { client_id, provider, models });
            }
        }
        self.commit_model_clients(&snap, registry, registrations, next_clients, next_providers, next_model_registrations);
    }

    /// Go `commitModelClients`: swaps the registered client set unless the snapshot moved on.
    fn commit_model_clients(
        &self,
        snap: &Arc<crate::host::Snapshot>,
        registry: &ModelRegistry,
        registrations: Vec<ModelClientRegistration>,
        next_clients: HashSet<String>,
        next_providers: HashMap<String, String>,
        next_model_registrations: HashMap<String, ModelRegistration>,
    ) {
        let stale: Vec<String> = {
            let mut st = self.state.lock();
            if !Arc::ptr_eq(&self.snapshot(), snap) {
                return;
            }
            let stale = st.model_client_ids.iter().filter(|c| !next_clients.contains(*c)).cloned().collect();
            st.model_client_ids = next_clients;
            st.model_providers = next_providers;
            st.model_registrations = next_model_registrations;
            stale
        };
        for r in &registrations {
            registry.register_client(&r.client_id, &r.provider, &r.models);
        }
        for id in &stale {
            registry.unregister_client(id);
        }
    }

    async fn call_model_registrar(&self, ctx: &CallCtx, rec: &Record) -> Result<ModelRegistrationResponse, String> {
        if !self.usable(rec) {
            return Ok(ModelRegistrationResponse::default());
        }
        let req = ModelRegistrationRequest { plugin: rec.meta.clone() };
        self.rpc(rec, ctx, abi::METHOD_MODEL_REGISTER, &req).await.map_err(|e| e.message)
    }

    async fn call_static_models(&self, ctx: &CallCtx, rec: &Record) -> Result<ModelResponse, String> {
        if !self.usable(rec) {
            return Ok(ModelResponse::default());
        }
        let req = StaticModelRequest { plugin: rec.meta.clone(), host: host_config_summary(self.runtime_config().as_deref()) };
        self.rpc(rec, ctx, abi::METHOD_MODEL_STATIC, &req).await.map_err(|e| e.message)
    }

    async fn call_models_for_auth(self: &Arc<Self>, ctx: &CallCtx, rec: &Record, auth: &Auth) -> Result<ModelResponse, String> {
        if !self.usable(rec) {
            return Ok(ModelResponse::default());
        }
        let req = AuthModelRequest {
            plugin: rec.meta.clone(),
            auth_id: auth.id.clone(),
            auth_provider: auth.provider.clone(),
            storage_json: storage_json_from_auth(Some(auth)),
            metadata: auth.metadata.clone(),
            attributes: auth.attributes.clone(),
            host: host_config_summary(self.runtime_config().as_deref()),
        };
        self.rpc_cb(rec, ctx, abi::METHOD_MODEL_FOR_AUTH, &req).await.map_err(|e| e.message)
    }

    /// Per-auth model discovery through the plugin that owns the auth's provider (Go:
    /// `ModelsForAuth`).
    pub async fn models_for_auth(self: &Arc<Self>, ctx: &CallCtx, auth: &Auth) -> AuthModelResult {
        let provider_key = normalize_provider_id(&auth.provider);
        if provider_key.is_empty() {
            return AuthModelResult::default();
        }
        for rec in self.active_records() {
            if !rec.caps().model_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            if !rec.caps().scope_allows_oauth_models() {
                continue;
            }
            if rec.caps().auth_provider {
                match self.auth_identifier(&rec) {
                    Some(id) if normalize_provider_id(&id) == provider_key => {}
                    _ => continue,
                }
            } else {
                let mut record_provider = normalize_provider_id(&self.model_provider(&rec.id));
                if record_provider.is_empty() && rec.caps().executor {
                    if let Some(candidate) = self.executor_provider(&rec) {
                        record_provider = candidate;
                    }
                }
                if record_provider != provider_key {
                    continue;
                }
            }
            let resp = match self.call_models_for_auth(ctx, &rec, auth).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("pluginhost: models for auth {} failed: {e}", auth.id);
                    return AuthModelResult { handled: true, err: Some(e), ..Default::default() };
                }
            };
            let resp_provider = normalize_provider_id(&resp.provider);
            if !resp_provider.is_empty() && resp_provider != provider_key {
                continue;
            }
            let resp_provider = if resp_provider.is_empty() { provider_key.clone() } else { resp_provider };
            let models: Vec<RegistryModel> = resp
                .models
                .iter()
                .map(plugin_model_to_registry)
                .map(|mut m| {
                    m.id = m.id.trim().to_string();
                    m
                })
                .filter(|m| !m.id.is_empty())
                .collect();
            let path = auth.attributes.get("path").cloned().unwrap_or_default();
            let updated = auth_data_has_value(&resp.auth_update)
                .then(|| self.auth_data_to_core_auth(&auth_data_with_defaults(resp.auth_update.clone(), auth), &path, &auth.file_name))
                .flatten();
            return AuthModelResult { provider: resp_provider, models, auth: updated, handled: true, err: None };
        }
        AuthModelResult::default()
    }

    /// Go `(*Host).AuthDataToCoreAuth`.
    pub fn auth_data_to_core_auth(&self, data: &cpa_pluginapi::api::AuthData, path: &str, file_name: &str) -> Option<Auth> {
        let auth_dir = host_config_summary(self.runtime_config().as_deref()).auth_dir;
        plugin_auth_data_to_core_auth(data, path, file_name, &auth_dir)
    }
}
