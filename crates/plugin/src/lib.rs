//! Dynamic plugin host (Go: internal/pluginhost; the wire types live in `cpa-pluginapi`).
//!
//! Plugins are shared libraries speaking a C ABI with JSON payloads ([`abi`], [`api`]). The
//! [`Host`] loads them from the plugins directory, negotiates capabilities, and exposes typed
//! adapters for every hook point: executors, interceptors, translators, schedulers, model
//! routing, auth providers, quota, management routes and command-line flags.

pub use cpa_pluginapi::{abi, api, wire};

pub mod adapters;
pub mod authcb;
pub mod bridge;
pub mod callbacks;
pub mod caps;
pub mod cli;
pub mod client;
pub mod config;
pub mod convert;
pub mod ctx;
pub mod error;
pub mod host;
pub mod httpclient;
pub mod loader;
pub mod management;
pub mod platform;
pub mod rpc;
pub mod sniff;
pub mod usage_helpers;

pub use adapters::access::{AccessAdapter, AccessFailure, AccessResult};
pub use adapters::executors::ExecutorAdapter;
pub use adapters::models::AuthModelResult;
pub use adapters::quota::RegisteredQuotaProviderInfo;
pub use adapters::refresh_compat::PluginRefreshCompatExecutor;
pub use adapters::translation::TranslatorHooks;
pub use callbacks::{
    ModelExecError, ModelExecutionRequest, ModelExecutionResponse, ModelExecutionStream, ModelExecutor, ModelStreamError,
};
pub use caps::{Capabilities, PluginInfo, Record};
pub use client::{PluginError, PluginResult};
pub use ctx::CallCtx;
pub use host::{Host, RegisteredPluginInfo, RegisteredPluginMenu, Snapshot};
pub use management::PluginHttpResponse;

/// Value of the `X-CPA-SUPPORT-PLUGIN` header: `1` when this build can load plugins (Go:
/// `SupportPluginHeaderValue`).
pub fn support_plugin_header_value() -> &'static str {
    if cfg!(unix) { "1" } else { "0" }
}
