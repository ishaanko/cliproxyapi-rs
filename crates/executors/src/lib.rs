//! Provider executors (Go: internal/runtime/executor).
//!
//! [`helps`] holds the provider-independent helpers every executor uses (payload rules, proxy-aware
//! HTTP clients, usage extraction, the SSE line reader, upstream status errors). Each provider
//! lives in its own module and implements [`cpa_runtime::executor::Executor`].

// `ExecError` is the conductor's error type (it carries the upstream body and headers for
// passthrough) and every executor returns it by value, so the size lint would fire everywhere.
#![allow(clippy::result_large_err)]

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
    helps::home_kv::install();
    // Kimi's Anthropic-compatible path runs on an embedded Claude executor.
    let kimi_claude = claude::new_embedded(
        cfg.clone(),
        claude::Embedding { request_log_provider: "kimi", upstream_model: kimi::normalize_kimi_upstream_model },
    );
    vec![
        claude::new(cfg.clone()),
        codex::new(cfg.clone()),
        gemini::new(cfg.clone()),
        gemini::new_interactions(cfg.clone()),
        gemini::new_vertex(cfg.clone()),
        gemini::new_aistudio(cfg.clone()),
        antigravity::new(cfg.clone()),
        openai_compat::new(cfg.clone()),
        xai::new(cfg.clone()),
        kimi::new_with_claude(cfg.clone(), Some(kimi_claude)),
        devin::new(cfg.clone()),
        meta::new(cfg.clone()),
    ]
}
