//! Static model definitions per provider and lookup helpers (Go: registry/model_definitions.go).

use std::collections::HashSet;

use super::catalog::static_models;
use super::devin::get_devin_models;
use super::model_info::{ModelInfo, ThinkingSupport};
use super::model_registry::global_registry;

const CODEX_BUILTIN_IMAGE_15_MODEL_ID: &str = "gpt-image-1.5";
const CODEX_BUILTIN_IMAGE_MODEL_ID: &str = "gpt-image-2";
const CODEX_BUILTIN_IMAGE_25_FLARE_MODEL_ID: &str = "gpt-image-2.5-flare";
const CODEX_BUILTIN_IMAGE_25_SUNBURST_MODEL_ID: &str = "gpt-image-2.5-sunburst";
const CODEX_BUILTIN_IMAGE_25_MODEL_ID: &str = "gpt-image-2.5";
const XAI_BUILTIN_IMAGE_MODEL_ID: &str = "grok-imagine-image";
const XAI_BUILTIN_IMAGE_QUALITY_MODEL_ID: &str = "grok-imagine-image-quality";
const XAI_BUILTIN_IMAGE_20_MODEL_ID: &str = "grok-imagine-image-2.0";
const XAI_BUILTIN_VIDEO_MODEL_ID: &str = "grok-imagine-video";
const XAI_BUILTIN_VIDEO_15_MODEL_ID: &str = "grok-imagine-video-1.5";
const XAI_BUILTIN_VIDEO_15_PREVIEW_ID: &str = "grok-imagine-video-1.5-preview";

/// Standard Claude model definitions.
pub fn get_claude_models() -> Vec<ModelInfo> {
    static_models().claude.clone()
}

/// Standard Gemini model definitions.
pub fn get_gemini_models() -> Vec<ModelInfo> {
    static_models().gemini.clone()
}

/// Gemini model definitions for Vertex AI.
pub fn get_gemini_vertex_models() -> Vec<ModelInfo> {
    static_models().vertex.clone()
}

/// Model definitions for AI Studio.
pub fn get_ai_studio_models() -> Vec<ModelInfo> {
    static_models().aistudio.clone()
}

/// Codex free plan tier models (plus the hard-coded image models).
pub fn get_codex_free_models() -> Vec<ModelInfo> {
    with_codex_builtins(static_models().codex_free.clone())
}

/// Codex team plan tier models (plus the hard-coded image models).
pub fn get_codex_team_models() -> Vec<ModelInfo> {
    with_codex_builtins(static_models().codex_team.clone())
}

/// Codex plus plan tier models (plus the hard-coded image models).
pub fn get_codex_plus_models() -> Vec<ModelInfo> {
    with_codex_builtins(static_models().codex_plus.clone())
}

/// Codex pro plan tier models (plus the hard-coded image models).
pub fn get_codex_pro_models() -> Vec<ModelInfo> {
    with_codex_builtins(static_models().codex_pro.clone())
}

/// Standard Kimi (Moonshot AI) model definitions.
pub fn get_kimi_models() -> Vec<ModelInfo> {
    static_models().kimi.clone()
}

/// Standard Antigravity model definitions.
pub fn get_antigravity_models() -> Vec<ModelInfo> {
    static_models().antigravity.clone()
}

/// Standard xAI Grok model definitions (plus the hard-coded image/video models).
pub fn get_xai_models() -> Vec<ModelInfo> {
    with_xai_builtins(static_models().xai.clone())
}

/// Standard Meta Muse model definitions.
pub fn get_meta_models() -> Vec<ModelInfo> {
    static_models().meta.clone()
}

fn devin_static(
    id: &str,
    owned_by: &str,
    display_name: &str,
    context_length: i64,
    max_completion_tokens: i64,
    levels: &[&str],
) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        r#type: "devin".into(),
        owned_by: owned_by.into(),
        display_name: display_name.into(),
        context_length,
        max_completion_tokens,
        thinking: Some(ThinkingSupport {
            levels: levels.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Hard-coded Devin fallback list, used when neither devin_models.json nor models.json has Devin
/// entries. Starts with the SWE-1.6 Slow built-in.
pub(super) fn static_devin_models() -> Vec<ModelInfo> {
    let mut models = super::devin::with_devin_builtins(Vec::new());
    models.extend([
        devin_static(
            "devin/swe-2",
            "cognition",
            "SWE-2",
            262_000,
            128_000,
            &["medium", "high", "max"],
        ),
        devin_static(
            "devin/claude-fable-5-1",
            "anthropic",
            "Claude Fable 5.1",
            1_000_000,
            64_000,
            &["low", "medium", "high", "xhigh", "max"],
        ),
        devin_static(
            "devin/gpt-6-astra",
            "openai",
            "GPT-6 Astra",
            1_000_000,
            64_000,
            &["low", "medium", "high", "xhigh", "max"],
        ),
        devin_static(
            "devin/glm-5-2",
            "zhipu",
            "GLM-5.2",
            200_000,
            64_000,
            &["none", "high"],
        ),
        devin_static(
            "devin/glm-5-3",
            "zhipu",
            "GLM-5.3",
            1_048_576,
            128_000,
            &["low", "high", "max"],
        ),
        devin_static(
            "devin/glm-5-3-flash",
            "zhipu",
            "GLM-5.3 Flash",
            1_000_000,
            128_000,
            &["low", "high", "max"],
        ),
        devin_static(
            "devin/gpt-5-6-sol",
            "openai",
            "GPT-5.6 Sol",
            1_000_000,
            128_000,
            &["none", "low", "medium", "high", "xhigh", "max"],
        ),
        devin_static(
            "devin/gemini-3-8-flash",
            "google",
            "Gemini 3.8 Flash",
            1_048_576,
            65_536,
            &["low", "medium", "high"],
        ),
        devin_static(
            "devin/grok-4-6",
            "xai",
            "Grok 4.6",
            500_000,
            131_072,
            &["low", "medium", "high", "xhigh"],
        ),
        devin_static(
            "devin/deepseek-v4-flash",
            "deepseek",
            "DeepSeek V4 Flash",
            1_048_576,
            64_000,
            &["high", "max"],
        ),
        devin_static(
            "devin/deepseek-v4-1-flash",
            "deepseek",
            "DeepSeek V4.1 Flash",
            1_048_576,
            64_000,
            &["high", "max"],
        ),
    ]);
    models
}

/// The Antigravity model that should run a native web search for `model_id`: the registered
/// Antigravity model (matched ignoring case and a trailing `(...)` suffix) when it supports web
/// search, else "".
pub fn antigravity_web_search_model_for(model_id: &str) -> String {
    let model_id = normalize_antigravity_capability_model_id(model_id);
    if model_id.is_empty() {
        return String::new();
    }
    for model in global_registry().get_available_models_by_provider("antigravity") {
        let current = normalize_antigravity_capability_model_id(&model.id);
        if current.is_empty() {
            continue;
        }
        if current == model_id {
            return if model.supports_web_search {
                current
            } else {
                String::new()
            };
        }
    }
    String::new()
}

fn normalize_antigravity_capability_model_id(model_id: &str) -> String {
    let mut id = model_id.trim().to_lowercase();
    if let Some(open) = id.rfind('(')
        && id.ends_with(')')
    {
        id = id[..open].trim().to_string();
    }
    id
}

/// Injects hard-coded Codex-only image models that must not depend on remote catalog updates;
/// built-ins replace any matching ids.
pub fn with_codex_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    upsert_model_infos(
        models,
        vec![
            codex_builtin_image(CODEX_BUILTIN_IMAGE_15_MODEL_ID, "GPT Image 1.5"),
            codex_builtin_image(CODEX_BUILTIN_IMAGE_MODEL_ID, "GPT Image 2"),
            codex_builtin_image(CODEX_BUILTIN_IMAGE_25_FLARE_MODEL_ID, "GPT Image 2.5 Flare"),
            codex_builtin_image(
                CODEX_BUILTIN_IMAGE_25_SUNBURST_MODEL_ID,
                "GPT Image 2.5 Sunburst",
            ),
            codex_builtin_image(CODEX_BUILTIN_IMAGE_25_MODEL_ID, "GPT Image 2.5"),
        ],
    )
}

/// Injects hard-coded xAI image/video models; built-ins replace any matching ids.
pub fn with_xai_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    const IMAGE_CREATED: i64 = 1_735_689_600; // 2025-01-01
    upsert_model_infos(
        models,
        vec![
            xai_builtin(
                XAI_BUILTIN_IMAGE_MODEL_ID,
                IMAGE_CREATED,
                "Grok Imagine Image",
                "xAI Grok image generation model.",
            ),
            xai_builtin(
                XAI_BUILTIN_IMAGE_QUALITY_MODEL_ID,
                IMAGE_CREATED,
                "Grok Imagine Image Quality",
                "xAI Grok higher-fidelity image generation model.",
            ),
            xai_builtin(
                XAI_BUILTIN_IMAGE_20_MODEL_ID,
                1_786_060_800, // 2026-08-07
                "Grok Imagine Image 2.0",
                "xAI Grok image generation model.",
            ),
            xai_builtin(
                XAI_BUILTIN_VIDEO_MODEL_ID,
                IMAGE_CREATED,
                "Grok Imagine Video",
                "xAI Grok video generation model.",
            ),
            xai_builtin(
                XAI_BUILTIN_VIDEO_15_MODEL_ID,
                IMAGE_CREATED,
                "Grok Imagine Video 1.5",
                "xAI Grok video generation model.",
            ),
            xai_builtin(
                XAI_BUILTIN_VIDEO_15_PREVIEW_ID,
                IMAGE_CREATED,
                "Grok Imagine Video 1.5 Preview",
                "Compatibility alias for the xAI Grok video generation model.",
            ),
        ],
    )
}

fn codex_builtin_image(id: &str, display_name: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model".into(),
        created: 1_704_067_200, // 2024-01-01
        owned_by: "openai".into(),
        r#type: "openai".into(),
        display_name: display_name.into(),
        version: id.into(),
        ..Default::default()
    }
}

fn xai_builtin(id: &str, created: i64, display_name: &str, description: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model".into(),
        created,
        owned_by: "xai".into(),
        r#type: "xai".into(),
        display_name: display_name.into(),
        name: id.into(),
        description: description.into(),
        ..Default::default()
    }
}

/// Appends `extras` to `models`, dropping `models` entries with a matching (case-insensitive,
/// trimmed) id; duplicate/blank extras are skipped. Entries with blank ids in `models` are dropped
/// too (when any extra applies).
pub(super) fn upsert_model_infos(models: Vec<ModelInfo>, extras: Vec<ModelInfo>) -> Vec<ModelInfo> {
    if extras.is_empty() {
        return models;
    }
    let mut extra_ids: HashSet<String> = HashSet::with_capacity(extras.len());
    let mut extra_list: Vec<ModelInfo> = Vec::with_capacity(extras.len());
    for extra in extras {
        let id = extra.id.trim();
        if id.is_empty() {
            continue;
        }
        if extra_ids.insert(id.to_lowercase()) {
            extra_list.push(extra);
        }
    }
    if extra_list.is_empty() {
        return models;
    }

    let mut filtered: Vec<ModelInfo> = models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim();
            !id.is_empty() && !extra_ids.contains(&id.to_lowercase())
        })
        .collect();
    filtered.extend(extra_list);
    filtered
}

/// Static model definitions for a provider/channel, `None` when the channel is unknown.
///
/// Channels: claude, gemini, gemini-interactions, vertex, aistudio, codex (pro tier), kimi
/// (kimi-ai, kimi.ai, kimi.com), antigravity, xai (x-ai, grok), devin, meta (muse).
pub fn get_static_model_definitions_by_channel(channel: &str) -> Option<Vec<ModelInfo>> {
    match channel.trim().to_lowercase().as_str() {
        "claude" => Some(get_claude_models()),
        "gemini" | "gemini-interactions" => Some(get_gemini_models()),
        "vertex" => Some(get_gemini_vertex_models()),
        "aistudio" => Some(get_ai_studio_models()),
        "codex" => Some(get_codex_pro_models()),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => Some(get_kimi_models()),
        "antigravity" => Some(get_antigravity_models()),
        "xai" | "x-ai" | "grok" => Some(get_xai_models()),
        "devin" => Some(get_devin_models()),
        "meta" | "muse" => Some(get_meta_models()),
        _ => None,
    }
}

/// Searches one provider-specific static section by exact id, without falling back across
/// providers.
pub fn lookup_static_model_info_by_channel(model_id: &str, channel: &str) -> Option<ModelInfo> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    get_static_model_definitions_by_channel(channel)?
        .into_iter()
        .find(|m| m.id == model_id)
}

/// Searches all static definitions (claude, gemini, vertex, aistudio, codex-pro, kimi,
/// antigravity, xai, devin, the hard-coded Devin list, meta) for an exact model id. Built-in
/// image models are not included, matching Go.
pub fn lookup_static_model_info(model_id: &str) -> Option<ModelInfo> {
    if model_id.is_empty() {
        return None;
    }
    let data = static_models();
    let hard_coded_devin = static_devin_models();
    [
        &data.claude,
        &data.gemini,
        &data.vertex,
        &data.aistudio,
        &data.codex_pro,
        &data.kimi,
        &data.antigravity,
        &data.xai,
        &data.devin,
        &hard_coded_devin,
        &data.meta,
    ]
    .into_iter()
    .find_map(|models| models.iter().find(|m| m.id == model_id))
    .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_replace_matching_ids_case_insensitively() {
        let models = vec![
            ModelInfo {
                id: "GPT-Image-2".into(),
                display_name: "stale".into(),
                ..Default::default()
            },
            ModelInfo {
                id: "keep".into(),
                ..Default::default()
            },
        ];
        let out = with_codex_builtins(models);
        assert_eq!(out[0].id, "keep");
        assert_eq!(
            out.iter()
                .filter(|m| m.id.eq_ignore_ascii_case("gpt-image-2"))
                .count(),
            1
        );
        assert!(
            out.iter()
                .any(|m| m.id == "gpt-image-2" && m.display_name == "GPT Image 2")
        );
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn channel_lookup() {
        assert!(get_static_model_definitions_by_channel("nope").is_none());
        let kimi = get_static_model_definitions_by_channel(" Kimi.com ").unwrap();
        let first = kimi.first().expect("kimi models").id.clone();
        assert!(lookup_static_model_info_by_channel(&first, "kimi").is_some());
        assert!(lookup_static_model_info_by_channel(&first, "claude").is_none());
        assert!(lookup_static_model_info(&first).is_some());
    }

    #[test]
    fn capability_model_id_normalization() {
        assert_eq!(
            normalize_antigravity_capability_model_id(" Gemini-3-Pro (High) "),
            "gemini-3-pro"
        );
        assert_eq!(normalize_antigravity_capability_model_id("a(b"), "a(b");
    }
}
