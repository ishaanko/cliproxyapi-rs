//! Plugin hooks of the service (Go: sdk/cliproxy service_plugins.go and the plugin branches of
//! service_models.go / service_executors.go).
//!
//! The service has no dependency on the plugin host; the host implements [`ServicePlugins`] and is
//! attached with `ServiceBuilder::plugins`.

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_core::registry::ModelInfo;

/// Go `pluginhost.AuthModelResult`.
#[derive(Default)]
pub struct PluginAuthModels {
    pub provider: String,
    pub models: Vec<ModelInfo>,
    /// Replacement credential the plugin wants stored for the auth.
    pub auth: Option<Auth>,
    pub handled: bool,
    pub err: Option<String>,
}

#[async_trait]
pub trait ServicePlugins: Send + Sync {
    /// Models plugins register for `provider` (Go: `ModelsForProvider`).
    fn models_for_provider(&self, provider: &str) -> Vec<ModelInfo>;
    /// Per-auth model discovery by the plugin owning the auth's provider (Go: `ModelsForAuth`).
    async fn models_for_auth(&self, auth: &Auth) -> PluginAuthModels;
}

/// Go `appendPluginModels`: `models` followed by plugin models of `provider` whose ids are new.
pub(super) fn append_plugin_models(
    plugins: Option<&dyn ServicePlugins>,
    provider: &str,
    models: Vec<ModelInfo>,
) -> Vec<ModelInfo> {
    let Some(plugins) = plugins else { return models };
    let extra = plugins.models_for_provider(provider);
    if extra.is_empty() {
        return models;
    }
    let mut seen: std::collections::HashSet<String> =
        models.iter().map(|m| m.id.trim().to_string()).filter(|id| !id.is_empty()).collect();
    let mut out = models;
    for model in extra {
        let id = model.id.trim().to_string();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        out.push(model);
    }
    out
}
