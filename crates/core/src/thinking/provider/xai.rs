//! xAI Grok thinking applier (Go: internal/thinking/provider/xai). The Responses-compatible
//! `reasoning.effort` format is identical to Codex, so this delegates.

use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError};
use super::CodexApplier;
use crate::registry::ModelInfo;

/// Applies thinking to xAI request bodies as `reasoning.effort`.
#[derive(Debug, Default, Clone, Copy)]
pub struct XaiApplier(CodexApplier);

impl ProviderApplier for XaiApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        self.0.apply(body, config, model_info)
    }
}
