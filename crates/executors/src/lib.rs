//! Provider executors (Go: internal/runtime/executor).
//!
//! [`helps`] holds the provider-independent helpers every executor uses (payload rules, proxy-aware
//! HTTP clients, usage extraction, the SSE line reader, upstream status errors). Each provider
//! lives in its own module and implements [`cpa_runtime::executor::Executor`].

use std::sync::Arc;

use cpa_config::Config;
use cpa_runtime::executor::DynExecutor;
use tokio::sync::watch;

/// Live config handle shared by executors: read `cfg.borrow().clone()` per request so hot
/// reloads apply without re-registering executors.
pub type ConfigRx = watch::Receiver<Arc<Config>>;

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod devin;
pub mod gemini;
pub mod helps;
pub mod kimi;
pub mod meta;
pub mod openai_compat;
pub mod xai;

/// Builds every provider executor for registration with the conductor.
///
/// Each provider agent adds exactly one line below (uncomment its constructor, which takes the
/// live config handle and returns a `DynExecutor`). Keep one line per provider so merges stay
/// trivial.
pub fn all_executors(cfg: ConfigRx) -> Vec<DynExecutor> {
    let _ = &cfg;
    #[allow(unused_mut)]
    let mut executors: Vec<DynExecutor> = Vec::new();
    // executors.push(claude::new(cfg.clone()));
    // executors.push(codex::new(cfg.clone()));
    // executors.push(gemini::new(cfg.clone()));
    // executors.push(antigravity::new(cfg.clone()));
    // executors.push(openai_compat::new(cfg.clone()));
    // executors.push(xai::new(cfg.clone()));
    executors.push(kimi::new(cfg.clone()));
    executors.push(devin::new(cfg.clone()));
    executors.push(meta::new(cfg.clone()));
    executors
}
