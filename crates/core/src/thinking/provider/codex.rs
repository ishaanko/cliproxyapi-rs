//! Codex (OpenAI Responses) thinking applier (Go: internal/thinking/provider/codex).
//!
//! Like OpenAI but writes the nested `reasoning.effort`.

use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError};
use super::openai::apply_effort;
use crate::registry::ModelInfo;

/// Applies thinking to Codex/Responses request bodies as `reasoning.effort`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexApplier;

impl ProviderApplier for CodexApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        apply_effort(body, config, model_info, "reasoning.effort")
    }
}
