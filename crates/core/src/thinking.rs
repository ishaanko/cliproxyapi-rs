//! Port of the Go package; see docs/survey.

/// Reasoning-summary visibility requested by the client (Go: thinking.SummaryConfig).
/// Placeholder until the thinking port lands; fields are defined by that port.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SummaryConfig {}

/// Go: thinking.ExtractTranslatedSummaryConfig.
pub fn extract_translated_summary_config(_body: &[u8], _source_format: &str, _target_format: &str) -> SummaryConfig {
    SummaryConfig::default()
}

/// Go: thinking.ApplySummaryConfigForModel.
pub fn apply_summary_config_for_model(body: Vec<u8>, _format: &str, _model: &str, _config: &SummaryConfig) -> Vec<u8> {
    body
}
