//! Model registry: model metadata, the embedded catalogs and the global per-client registry
//! (Go: internal/registry).
//!
//! - [`ModelInfo`] and friends: metadata types (`model_info`).
//! - Static catalogs: models.json (`catalog`, remote refresh via [`apply_remote_models`]),
//!   per-provider getters and lookups (`definitions`), the Devin catalog (`devin`) and the Codex
//!   client catalog (`codex_client`). Networking is the caller's job: core exposes the URLs and the
//!   functions that consume fetched bytes.
//! - [`ModelRegistry`] / [`global_registry`]: which clients provide which models, with quota and
//!   suspension state (`model_registry`).

mod catalog;
mod codex_client;
mod definitions;
mod devin;
mod model_info;
mod model_registry;

pub use catalog::*;
pub use codex_client::*;
pub use definitions::*;
pub use devin::*;
pub use model_info::*;
pub use model_registry::*;
