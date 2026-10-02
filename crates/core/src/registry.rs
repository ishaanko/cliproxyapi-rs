//! Port of the Go package; see docs/survey.

/// Model metadata (Go: registry.ModelInfo). Placeholder until the registry port lands.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
}
