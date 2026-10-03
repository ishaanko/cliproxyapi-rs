//! Service wiring (Go: sdk/cliproxy, internal/watcher and internal/watcher/synthesizer).
//!
//! [`ServiceBuilder`] loads the config, opens the auth store and returns a [`Service`]; starting it
//! registers executors, loads and synthesizes auths (config API keys plus auth-dir files),
//! registers each auth's models in the model registry and keeps everything in sync with the config
//! file and the auth directory:
//!
//! ```ignore
//! let service = ServiceBuilder::new("config.yaml").executors(my_executors).build()?;
//! service.start().await?;
//! let cfg = service.config();               // Arc<Config> snapshot
//! let rx = service.subscribe_config();      // watch::Receiver<Arc<Config>>
//! let manager = service.manager();          // for request execution
//! ```
//!
//! Layout:
//! - [`synth`]: config/file -> `Auth` synthesis with Go-identical ids and attributes.
//! - [`sync`]: published-auth state and minimal add/modify/delete diffing.
//! - [`models`]: per-auth model registration rules (static catalogs, config models, aliases,
//!   exclusions, prefixes).
//! - [`listing`]: `/v1/models` and `/v1beta/models` payloads over the registry.
//! - [`antigravity`]: web-search capability probe for Antigravity auths.
//!
//! - [`discovery`], [`pprof`], `extras`: the mDNS advertiser and the profiling server, applied on
//!   start and on every config reload.
//!
//! Home mode (`lifecycle/home.rs`): the config arrives over the Home subscription.
//!
//! Not ported: the aistudio websocket gateway and runtime-only auths.

pub mod antigravity;
pub mod discovery;
mod extras;
pub mod pprof;
pub mod listing;
pub mod models;
mod persist;
mod lifecycle;
pub mod plugins;
pub mod sync;
pub mod synth;
#[cfg(test)]
mod tests;

pub use listing::{
    ModelsRoute, claude_models_response, gemini_model_response, gemini_models_response, grok_models_response,
    is_anthropic_models_request, openai_models_response, resolve_claude_model_id_prefix, route_models_request,
};
pub use models::{ModelRegistration, register_models_for_auth, resolve_models_for_auth};
pub use persist::{StoreBackend, StorePersister};
pub use plugins::{PluginAuthModels, ServicePlugins};
pub use lifecycle::{
    ExecutorFactory, HomeHooks, HomePluginWork, HomePlugins, ManagerPort, Service, ServiceBuilder, ServiceError, force_home_runtime_config,
    merge_home_config,
};
pub use sync::{AuthSync, AuthUpdate, AuthUpdateAction};
pub use synth::{SynthesisContext, snapshot_core_auths, synthesize_auth_dir, synthesize_auth_file, synthesize_auth_files, synthesize_config_auths};
