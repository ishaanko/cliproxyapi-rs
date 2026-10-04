//! Gemini family (Go: gemini_executor.go, gemini_vertex_executor.go, aistudio_executor.go,
//! internal/wsrelay and helps/gemini_*, vertex_*).
//!
//! - [`GeminiExecutor`] (`gemini`, and `gemini-interactions` for the native Interactions API):
//!   `generativelanguage.googleapis.com` with API keys.
//! - [`GeminiVertexExecutor`] (`vertex`): Vertex AI with service-account bearer tokens or API keys.
//! - [`AiStudioExecutor`] (`aistudio`): requests relayed to a browser page over the websocket
//!   [`wsrelay`]; the HTTP layer feeds sockets to [`wsrelay::Manager::attach`].
//!
//! The turn-shape, Vertex payload and TTFT helpers ([`content_turns`], [`vertex_payload`],
//! [`ttft`]) are also used by the Antigravity executor.

use std::sync::Arc;

use cpa_runtime::executor::DynExecutor;

use crate::ConfigRx;

mod aistudio;
mod common;
mod executor;
mod interactions;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_signatures;
#[cfg(test)]
mod tests_terminal;
pub mod ttft;
mod vertex;
pub mod vertex_payload;
mod vertex_token;
pub mod wsrelay;

pub use aistudio::AiStudioExecutor;
pub use executor::GeminiExecutor;
pub use vertex::GeminiVertexExecutor;

/// The Gemini API-key executor (`gemini`).
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(GeminiExecutor::new(cfg))
}

/// The native Interactions executor (`gemini-interactions`).
pub fn new_interactions(cfg: ConfigRx) -> DynExecutor {
    Arc::new(GeminiExecutor::new_interactions(cfg))
}

/// The Vertex AI executor (`vertex`).
pub fn new_vertex(cfg: ConfigRx) -> DynExecutor {
    Arc::new(GeminiVertexExecutor::new(cfg))
}

/// The AI Studio executor (`aistudio`) on the process-wide websocket relay.
pub fn new_aistudio(cfg: ConfigRx) -> DynExecutor {
    Arc::new(AiStudioExecutor::with_global_relay(cfg))
}
