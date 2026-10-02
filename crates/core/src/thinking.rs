//! Unified thinking/reasoning configuration (Go: internal/thinking).
//!
//! The pipeline keeps a canonical representation: [`apply_thinking`] parses the model-name suffix,
//! reads the request's own settings into a [`ThinkingConfig`], validates and clamps it against the
//! model's capabilities ([`validate_config`]), and finally lets a per-provider [`ProviderApplier`]
//! write the provider's wire fields. Summary visibility ([`SummaryConfig`]) is handled separately
//! and restored after the effort has been applied.
//!
//! - `types`: [`ThinkingConfig`], [`ThinkingMode`], [`ProviderApplier`], [`ThinkingError`].
//! - `suffix`, `convert`, `validate`: suffix parsing, level/budget conversion, validation.
//! - `extract`, `strip`, `configuration_update`: reading and removing provider fields.
//! - `summary`: reasoning-summary visibility intent.
//! - `apply`: the entry points and the provider applier registry; `provider`: the eight appliers
//!   (claude, gemini, antigravity, openai, codex, xai, kimi, interactions).

mod apply;
mod configuration_update;
mod convert;
mod extract;
mod json;
pub mod provider;
mod strip;
mod suffix;
mod summary;
mod text;
mod types;
mod validate;

#[cfg(test)]
mod tests;

pub use apply::{
    apply_thinking, apply_thinking_with_model_info, apply_thinking_with_model_info_and_summary,
    apply_thinking_with_source_and_summary, apply_thinking_with_summary, get_provider_applier,
    is_user_defined_model, register_provider,
};
pub use convert::{
    ModelCapability, THRESHOLD_HIGH, THRESHOLD_LOW, THRESHOLD_MEDIUM, THRESHOLD_MINIMAL,
    convert_budget_to_level, convert_level_to_budget, detect_model_capability, has_level,
    map_to_claude_effort,
};
pub use extract::{extract_reasoning_effort, extract_translated_reasoning_effort};
pub use strip::strip_thinking_config;
pub use suffix::{parse_level_suffix, parse_numeric_suffix, parse_special_suffix, parse_suffix};
pub use summary::{
    SummaryConfig, SummaryMode, apply_summary_config, apply_summary_config_for_model,
    apply_translated_summary_to_claude, extract_explicit_summary_config, extract_summary_config,
    extract_translated_summary_config,
};
pub use text::get_thinking_text;
pub use types::{
    ErrorCode, ProviderApplier, SuffixResult, ThinkingConfig, ThinkingError, ThinkingMode, level,
};
pub use validate::validate_config;
