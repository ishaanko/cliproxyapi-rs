//! Model metadata types (Go: registry.ModelInfo and friends).
//!
//! JSON follows the Go tags: the public keys are serialized with `omitempty` semantics; the
//! internal fields are never serialized, and `native_capabilities` / `support_configuration_update`
//! are read from catalog JSON only. Go `int` is `i64` here. JSON `null` for any field behaves like
//! an absent key (Go keeps the zero value).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// Marks models that are callable through OpenAI-compatible image endpoints.
pub const OPENAI_IMAGE_MODEL_TYPE: &str = "openai-image";

pub const DEFAULT_CLAUDE_MAX_INPUT_TOKENS: i64 = 200_000;
pub const DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS: i64 = 64_000;

/// Deserializes `null` (or a missing key via `#[serde(default)]`) as the type's default.
pub(super) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Tri-state native capability metadata from the catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCapabilities {
    /// Explicit per-model native web search support; `None` means the catalog does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<bool>,
}

/// A model family's supported reasoning budget range (provider-native token units). Budget models
/// use `min`/`max`; level models use `levels`. JSON is snake_case, YAML kebab-case (accepted too).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingSupport {
    /// Minimum allowed budget (inclusive).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub min: i64,
    /// Maximum allowed budget (inclusive).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub max: i64,
    /// Whether 0 is valid (to disable thinking).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_false",
        alias = "zero-allowed"
    )]
    pub zero_allowed: bool,
    /// Whether -1 is valid (dynamic budget).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_false",
        alias = "dynamic-allowed"
    )]
    pub dynamic_allowed: bool,
    /// Discrete reasoning effort levels (`low`, `medium`, ...); when set the model uses levels
    /// instead of token budgets.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub levels: Vec<String>,
}

/// Optional runtime overrides for a model definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Forces upstream request headers when non-empty; keys are header names, values replace any
    /// existing header.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub override_header: BTreeMap<String, String>,
}

/// Information about an available model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Unique model identifier.
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    /// Canonical model used to resolve client metadata. Internal, never serialized.
    #[serde(skip)]
    pub metadata_model_id: String,
    /// Thinking configuration was explicitly configured. Internal.
    #[serde(skip)]
    pub explicit_thinking: bool,
    /// Input modalities were explicitly configured. Internal.
    #[serde(skip)]
    pub explicit_input_modalities: bool,
    /// Object type (typically `model`).
    #[serde(default, deserialize_with = "null_default")]
    pub object: String,
    /// Creation timestamp (unix seconds).
    #[serde(default, deserialize_with = "null_default")]
    pub created: i64,
    /// Owning organization.
    #[serde(default, deserialize_with = "null_default")]
    pub owned_by: String,
    /// Model type (`claude`, `gemini`, `openai`, ...).
    #[serde(default, deserialize_with = "null_default")]
    pub r#type: String,
    /// Human-readable name.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub display_name: String,
    /// Gemini-style model name (`models/...`).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub version: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,
    #[serde(
        default,
        rename = "inputTokenLimit",
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub input_token_limit: i64,
    #[serde(
        default,
        rename = "outputTokenLimit",
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub output_token_limit: i64,
    #[serde(
        default,
        rename = "supportedGenerationMethods",
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub supported_generation_methods: Vec<String>,
    /// Context window size.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub context_length: i64,
    /// Explicit per-model context window override from configuration (Codex client catalog).
    /// Internal, never serialized.
    #[serde(skip)]
    pub max_context_length: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    pub max_completion_tokens: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub supported_parameters: Vec<String>,
    /// Supported input modalities (`TEXT`, `IMAGE`, `VIDEO`, `AUDIO`).
    #[serde(
        default,
        rename = "supportedInputModalities",
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub supported_input_modalities: Vec<String>,
    #[serde(
        default,
        rename = "supportedOutputModalities",
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub supported_output_modalities: Vec<String>,
    /// Antigravity model listed by `fetchAvailableModels.webSearchModelIds` that can run native
    /// `googleSearch`.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_false"
    )]
    pub supports_web_search: bool,
    /// Internal support for `configuration_update` (catalog only, never serialized).
    #[serde(default, deserialize_with = "null_default", skip_serializing)]
    pub support_configuration_update: bool,
    /// Internal static per-model capability metadata (catalog only, never serialized); separate
    /// from Antigravity's dynamically probed capability.
    #[serde(default, skip_serializing)]
    pub native_capabilities: Option<NativeCapabilities>,
    /// Reasoning budget capabilities (used for thinking normalization).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingSupport>,
    /// Runtime overrides loaded from models.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ModelConfig>,
    /// Defined through the config file's `models[]` arrays; thinking config passes through without
    /// validation. Internal.
    #[serde(skip)]
    pub user_defined: bool,
    /// Enables compatibility handling for this configured API-key model. Internal.
    #[serde(skip)]
    pub is_compat: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_follows_go_tags() {
        let info: ModelInfo = serde_json::from_str(
            r#"{"id":"m","object":"model","created":5,"owned_by":"x","type":"claude","description":null,
                "context_length":1000,"thinking":{"min":1024,"zero_allowed":true},
                "native_capabilities":{"web_search":true},"support_configuration_update":true,
                "metadata_model_id":"ignored","unknown":1}"#,
        )
        .unwrap();
        assert_eq!(
            info.native_capabilities,
            Some(NativeCapabilities {
                web_search: Some(true)
            })
        );
        assert!(info.support_configuration_update);
        assert_eq!(info.metadata_model_id, "");
        // Internal fields are not serialized; omitempty fields are dropped.
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"id":"m","object":"model","created":5,"owned_by":"x","type":"claude","context_length":1000,"thinking":{"min":1024,"zero_allowed":true}}"#
        );
    }

    #[test]
    fn thinking_support_accepts_yaml_style_keys() {
        let t: ThinkingSupport = serde_json::from_str(
            r#"{"zero-allowed":true,"dynamic-allowed":true,"levels":["low"]}"#,
        )
        .unwrap();
        assert!(t.zero_allowed && t.dynamic_allowed);
        assert_eq!(t.levels, ["low"]);
    }
}
