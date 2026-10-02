//! Dynamic plugin host (Go: internal/pluginhost, sdk/pluginabi, sdk/pluginapi).
//!
//! Plugins are shared libraries speaking a C ABI with JSON payloads ([`abi`], [`api`]). The
//! [`Host`] loads them from the plugins directory, negotiates capabilities, and exposes typed
//! adapters for every hook point (executors, interceptors, translators, schedulers, model
//! routing, auth providers, quota, management routes, command line flags).

pub mod abi;
pub mod api;
pub mod caps;
pub mod client;
pub mod config;
pub mod ctx;
pub mod loader;
pub mod platform;
pub mod wire;

pub use caps::{Capabilities, PluginInfo, Record};
pub use client::{PluginError, PluginResult};
pub use ctx::CallCtx;
