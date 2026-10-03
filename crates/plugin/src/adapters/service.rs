//! Service-side plugin hooks (Go: sdk/cliproxy service_plugins.go and service_executors.go): the
//! model hooks and auth-file parser of the host, and the runtime sync the server drives on start
//! and on every config change.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_auth::plugin_parser::{PluginAuthParser, PluginParseRequest, set_plugin_auth_parser};
use cpa_config::Config;
use cpa_core::registry::{ModelInfo, ModelRegistry};
use cpa_pluginapi::api::AuthParseRequest;
use cpa_runtime::conductor::SharedManager;
use cpa_runtime::service::{PluginAuthModels, ServicePlugins};
use cpa_runtime::usage::UsageTracker;

use crate::adapters::translation::TranslatorHooks;
use crate::ctx::CallCtx;
use crate::host::Host;

/// The host as a [`ServicePlugins`] and [`PluginAuthParser`].
pub struct ServiceHooks(pub Arc<Host>);

#[async_trait]
impl ServicePlugins for ServiceHooks {
    fn models_for_provider(&self, provider: &str) -> Vec<ModelInfo> {
        self.0.models_for_provider(provider)
    }

    async fn models_for_auth(&self, auth: &Auth) -> PluginAuthModels {
        let r = self.0.models_for_auth(&CallCtx::background(), auth).await;
        PluginAuthModels { provider: r.provider, models: r.models, auth: r.auth, handled: r.handled, err: r.err }
    }
}

/// Drives `fut` to completion from synchronous code (file store and synthesizer parse hooks).
fn block_on<F: Future + Send>(fut: F) -> Result<F::Output, String>
where
    F::Output: Send,
{
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => Ok(tokio::task::block_in_place(|| h.block_on(fut))),
        Ok(_) | Err(_) => std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| rt.block_on(fut))
                    .map_err(|e| e.to_string())
            })
            .join()
            .unwrap_or_else(|_| Err("plugin parse thread panicked".into()))
        }),
    }
}

impl PluginAuthParser for ServiceHooks {
    fn parse_auths(&self, req: &PluginParseRequest<'_>) -> Result<Option<Vec<Auth>>, String> {
        let host = self.0.clone();
        let req = AuthParseRequest {
            provider: req.provider.to_string(),
            path: req.path.to_string(),
            file_name: req.file_name.to_string(),
            raw_json: req.raw_json.to_vec(),
            host: Default::default(),
        };
        block_on(async move { host.parse_auths(&CallCtx::background(), req).await })?.map_err(|e| e.message)
    }
}

impl Host {
    /// Go `syncPluginRuntimeConfigForConfig`: applies `cfg` and re-registers every runtime
    /// hook that depends on the loaded plugin set.
    pub async fn sync_runtime_config(self: &Arc<Self>, cfg: &Arc<Config>, manager: &SharedManager, usage: &UsageTracker) {
        self.apply_config(&CallCtx::background(), Some(cfg.clone())).await;
        manager.set_plugin_scheduler(Some(self.clone()));
        set_plugin_auth_parser(Some(Arc::new(ServiceHooks(self.clone()))));
        self.register_frontend_auth_providers();
        self.register_usage_plugins(usage);
        cpa_translator::registry::set_plugin_hooks(Some(Arc::new(TranslatorHooks(self.clone()))));
    }

    /// Go `syncPluginModelRuntime` up to the per-auth re-registration (done by the service):
    /// plugin static models and plugin executors.
    pub async fn sync_model_runtime(self: &Arc<Self>, manager: &SharedManager, registry: &ModelRegistry) {
        self.register_models(&CallCtx::background(), registry).await;
        self.register_executors(manager, registry);
    }

    /// The service shutdown tail (Go: `Service.Shutdown`): detaches every hook and unloads plugins.
    pub async fn shutdown_runtime(self: &Arc<Self>, manager: &SharedManager, registry: &ModelRegistry) {
        let ctx = CallCtx::background();
        cpa_translator::registry::set_plugin_hooks(None);
        set_plugin_auth_parser(None);
        self.apply_config(&ctx, Some(Arc::new(Config::default()))).await;
        self.register_models(&ctx, registry).await;
        self.register_executors(manager, registry);
        self.register_frontend_auth_providers();
        self.shutdown_all(&ctx).await;
    }
}
