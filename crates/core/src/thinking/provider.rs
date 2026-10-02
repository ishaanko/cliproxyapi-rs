//! Per-provider appliers (Go: internal/thinking/provider/*). Each turns a canonical
//! [`ThinkingConfig`](super::ThinkingConfig) into one provider's wire fields.

mod antigravity;
mod claude;
mod codex;
mod gemini;
mod interactions;
mod kimi;
mod openai;
mod xai;

pub use antigravity::AntigravityApplier;
pub use claude::ClaudeApplier;
pub use codex::CodexApplier;
pub use gemini::GeminiApplier;
pub use interactions::InteractionsApplier;
pub use kimi::KimiApplier;
pub use openai::OpenAIApplier;
pub use xai::XaiApplier;

use cpa_json::{J, Kind, Value};

use super::json::set;

/// Re-sets `<prefix>.includeThoughts` from the ORIGINAL body's `includeThoughts` /
/// `include_thoughts` (first boolean found), so summary visibility survives effort rewriting.
pub(crate) fn restore_include_thoughts(result: &mut Value, original: &Value, prefix: &str) {
    for key in ["includeThoughts", "include_thoughts"] {
        match original.g(&format!("{prefix}.{key}")).kind() {
            Kind::True => {
                set(result, &format!("{prefix}.includeThoughts"), true);
                return;
            }
            Kind::False => {
                set(result, &format!("{prefix}.includeThoughts"), false);
                return;
            }
            _ => {}
        }
    }
}
