//! Codex client model catalog for `GET /v1/models?client_version=...`
//! (Go: internal/client/codex/models, codex_client_models.go of the OpenAI handler).
//!
//! Every available model becomes one catalog entry: a clone of the template whose slug matches
//! the model's metadata id (or of the default `gpt-5.5` template), adjusted with the registry's
//! modalities, thinking levels, display data, provider restrictions and the executor-backed
//! `apply_patch` capability. Output keys are sorted and HTML is not escaped, like Go's
//! `MarshalCompact` over `map[string]any`.

use std::collections::HashSet;

use cpa_config::Config;
use cpa_core::registry::{
    OPENAI_IMAGE_MODEL_TYPE, ThinkingSupport, get_codex_client_models_snapshot, global_registry, lookup_model_info,
};
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use cpa_runtime::conductor::Manager;
use serde_json::{Map, Value, json};

use crate::exec::request_details;

type Entry = Map<String, Value>;

const ALLOWED_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const LEGACY_ALLOWED_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
const FALLBACK_INSTRUCTIONS: &str = "You are Codex, a coding agent. You and the user share one workspace.";

/// Providers able to serve a model id (Go: `ProvidersForModelFunc`).
pub type ProvidersForModel<'a> = dyn Fn(&str) -> Vec<String> + 'a;

/// Inputs of the builder that depend on the running server (Go passes them as closures).
pub struct CatalogContext<'a> {
    /// `None` is Go's nil `providersForModel` (Home mode): templates keep their provider-neutral
    /// capabilities instead of being treated as "no provider found".
    pub providers_for_model: Option<&'a ProvidersForModel<'a>>,
    pub web_search_capability: &'a dyn Fn(&str) -> Option<bool>,
    /// `None` leaves `apply_patch_tool_type` null for every model.
    pub apply_patch_capability: Option<&'a dyn Fn(&str) -> bool>,
    pub optimize_multi_agent_v2: bool,
    pub client_version: &'a str,
}

fn str_value(map: &Entry, key: &str) -> String {
    map.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn int_value(map: &Entry, key: &str) -> i64 {
    match map.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        _ => 0,
    }
}

fn thinking_support_of(model: &Entry) -> Option<ThinkingSupport> {
    match model.get("thinking") {
        None | Some(Value::Null) => None,
        Some(raw) => serde_json::from_value(raw.clone()).ok(),
    }
}

/// Segment after the first `/`.
fn after_slash(id: &str) -> Option<&str> {
    id.find('/').map(|idx| id[idx + 1..].trim())
}

/// Registry metadata id, else the part after the provider prefix, else the id itself.
fn metadata_model_id(id: &str) -> String {
    let id = id.trim();
    if let Some(info) = lookup_model_info(id, None) {
        let meta = info.metadata_model_id.trim();
        if !meta.is_empty() {
            return meta.to_string();
        }
    }
    if let Some(base) = after_slash(id) {
        if let Some(info) = lookup_model_info(base, None)
            && !info.metadata_model_id.trim().is_empty()
        {
            return info.metadata_model_id.trim().to_string();
        }
        return base.to_string();
    }
    id.to_string()
}

fn providers_of(ctx: &CatalogContext<'_>, id: &str) -> Vec<String> {
    let Some(providers_for_model) = ctx.providers_for_model else { return Vec::new() };
    let mut providers = providers_for_model(id);
    if providers.is_empty()
        && let Some(base) = after_slash(id)
    {
        providers = providers_for_model(base);
    }
    providers
}

fn is_image_or_video_model(id: &str) -> bool {
    let target = id.trim();
    let target = after_slash(target).unwrap_or(target);
    matches!(
        target,
        "grok-imagine-image-quality"
            | "gpt-image-1.5"
            | "gpt-image-2"
            | "gpt-image-2.5-flare"
            | "gpt-image-2.5-sunburst"
            | "gpt-image-2.5"
            | "grok-imagine-image"
            | "grok-imagine-image-2.0"
            | "grok-imagine-video"
            | "grok-imagine-video-1.5"
            | "grok-imagine-video-1.5-preview"
    )
}

// ------------------------------------------------------------------ versions and reasoning

fn parse_dotted_version(version: &str) -> Vec<i64> {
    let mut v = version.trim();
    if v.starts_with('v') || v.starts_with('V') {
        v = &v[1..];
    }
    if let Some(idx) = v.find(['-', '+']) {
        v = &v[..idx];
    }
    let mut nums = Vec::new();
    for part in v.split('.') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<i64>() {
            Ok(n) if n >= 0 => nums.push(n),
            _ => return Vec::new(),
        }
    }
    nums
}

fn compare_dotted_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (na, nb) = (parse_dotted_version(a), parse_dotted_version(b));
    if na.is_empty() || nb.is_empty() {
        return None;
    }
    for i in 0..na.len().max(nb.len()) {
        let (x, y) = (na.get(i).copied().unwrap_or(0), nb.get(i).copied().unwrap_or(0));
        if x != y {
            return Some(x.cmp(&y));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// `max` / `ultra` levels need Codex CLI >= 0.144.0 (unknown versions get the modern set).
fn supports_extended_reasoning_levels(client_version: &str) -> bool {
    let v = client_version.trim();
    if v.is_empty() {
        return true;
    }
    match compare_dotted_versions(v, "0.144.0") {
        Some(ord) => ord != std::cmp::Ordering::Less,
        None => true,
    }
}

fn normalize_reasoning_level(raw: &str, client_version: &str) -> String {
    let level = raw.trim().to_lowercase();
    let allowed = if supports_extended_reasoning_levels(client_version) { ALLOWED_LEVELS } else { LEGACY_ALLOWED_LEVELS };
    if allowed.contains(&level.as_str()) { level } else { String::new() }
}

fn reasoning_description(level: &str) -> String {
    match level {
        "none" => "No reasoning",
        "minimal" => "Fastest responses with minimal reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth for everyday tasks",
        "high" => "Greater reasoning depth for complex problems",
        "xhigh" => "Extra high reasoning depth for complex problems",
        "max" => "Maximum available reasoning depth for complex problems",
        other => other,
    }
    .to_string()
}

fn apply_thinking_metadata(entry: &mut Entry, thinking: Option<&ThinkingSupport>, client_version: &str) {
    let Some(thinking) = thinking else { return };
    let mut levels = Vec::new();
    let (mut default_level, mut first_level) = (String::new(), String::new());
    for raw in &thinking.levels {
        let level = normalize_reasoning_level(raw, client_version);
        if level.is_empty() {
            continue;
        }
        if first_level.is_empty() {
            first_level = level.clone();
        }
        if (default_level.is_empty() && level != "none") || level == "medium" {
            default_level = level.clone();
        }
        levels.push(json!({"effort": level, "description": reasoning_description(&level)}));
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
        entry.shift_remove("default_reasoning_level");
        return;
    }
    if default_level.is_empty() {
        default_level = first_level;
    }
    entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
    entry.insert("default_reasoning_level".into(), Value::String(default_level));
}

/// `sanitizeCodexClientReasoningMetadata`: drops levels the client cannot parse and repairs the
/// default.
fn sanitize_reasoning_metadata(entry: &mut Entry, client_version: &str) {
    let Some(Value::Array(raw_levels)) = entry.get("supported_reasoning_levels").cloned() else {
        return;
    };
    let mut levels = Vec::new();
    let mut allowed: HashSet<String> = HashSet::new();
    for raw in raw_levels {
        let Value::Object(mut level_entry) = raw else { continue };
        let level = normalize_reasoning_level(&str_value(&level_entry, "effort"), client_version);
        if level.is_empty() {
            continue;
        }
        level_entry.insert("effort".into(), Value::String(level.clone()));
        levels.push(Value::Object(level_entry));
        allowed.insert(level);
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
        entry.shift_remove("default_reasoning_level");
        return;
    }
    let mut default_level = normalize_reasoning_level(&str_value(entry, "default_reasoning_level"), client_version);
    if !allowed.contains(&default_level) {
        default_level = levels[0].get("effort").and_then(Value::as_str).unwrap_or("").trim().to_string();
    }
    entry.insert("supported_reasoning_levels".into(), Value::Array(levels));
    entry.insert("default_reasoning_level".into(), Value::String(default_level));
}

// ------------------------------------------------------------------ modalities

fn filter_input_modalities(modalities: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for raw in modalities {
        let m = raw.trim().to_lowercase();
        if (m == "text" || m == "image") && !out.iter().any(|v| v == &Value::String(m.clone())) {
            out.push(Value::String(m));
        }
    }
    out
}

fn has_image(modalities: &[Value]) -> bool {
    modalities.iter().any(|m| m.as_str() == Some("image"))
}

fn apply_input_modalities(entry: &mut Entry, modalities: &[String]) {
    if modalities.is_empty() {
        return;
    }
    let codex = filter_input_modalities(modalities);
    if codex.is_empty() {
        return;
    }
    let image = has_image(&codex);
    entry.insert("input_modalities".into(), Value::Array(codex));
    if image {
        entry.insert("supports_image_detail_original".into(), Value::Bool(true));
    } else {
        entry.shift_remove("supports_image_detail_original");
    }
}

fn intersect_strings(a: &[String], b: &[String]) -> Vec<String> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let b_set: HashSet<String> = b.iter().map(|s| s.trim().to_lowercase()).collect();
    let mut seen = HashSet::new();
    a.iter()
        .filter(|item| {
            let key = item.trim().to_lowercase();
            b_set.contains(&key) && seen.insert(key)
        })
        .cloned()
        .collect()
}

fn intersect_thinking(a: &ThinkingSupport, b: &ThinkingSupport) -> ThinkingSupport {
    let max = if b.max > 0 && (a.max == 0 || b.max < a.max) { b.max } else { a.max };
    ThinkingSupport {
        min: a.min.max(b.min),
        max,
        zero_allowed: a.zero_allowed && b.zero_allowed,
        dynamic_allowed: a.dynamic_allowed && b.dynamic_allowed,
        levels: intersect_strings(&a.levels, &b.levels),
    }
}

/// `applyCodexClientModelCapabilities`: template entries are constrained by what the providers
/// serving the model actually support.
fn apply_model_capabilities(
    entry: &mut Entry,
    id: &str,
    metadata_id: &str,
    info: Option<&cpa_core::registry::ModelInfo>,
    ctx: &CatalogContext<'_>,
) {
    if info.is_some_and(|i| i.r#type == OPENAI_IMAGE_MODEL_TYPE) {
        entry.insert("visibility".into(), json!("hide"));
        entry.shift_remove("input_modalities");
        entry.shift_remove("supports_image_detail_original");
        return;
    }
    let providers = providers_of(ctx, id);
    let is_alias = !metadata_id.is_empty() && !id.eq_ignore_ascii_case(metadata_id);
    let provider_info = |provider: &str| {
        lookup_model_info(id, Some(provider)).or_else(|| after_slash(id).and_then(|base| lookup_model_info(base, Some(provider))))
    };

    let mut modalities: Option<Vec<String>> = None;
    for p in &providers {
        let Some(p_info) = provider_info(p) else { continue };
        let is_codex = p.trim().eq_ignore_ascii_case("codex");
        if (!is_codex && is_alias) || p_info.explicit_input_modalities {
            let mods = p_info.supported_input_modalities.clone();
            modalities = Some(match modalities {
                None => mods,
                Some(prev) => intersect_strings(&prev, &mods),
            });
        }
    }
    if modalities.is_none()
        && let Some(i) = info
        && i.explicit_input_modalities
    {
        modalities = Some(i.supported_input_modalities.clone());
    }
    if let Some(mods) = modalities {
        let codex = filter_input_modalities(&mods);
        let image = has_image(&codex);
        entry.insert("input_modalities".into(), Value::Array(codex));
        if image {
            entry.insert("supports_image_detail_original".into(), Value::Bool(true));
        } else {
            entry.shift_remove("supports_image_detail_original");
        }
    }

    let mut thinking: Option<ThinkingSupport> = None;
    let mut constrained = false;
    for p in &providers {
        let Some(p_info) = provider_info(p) else { continue };
        let is_codex = p.trim().eq_ignore_ascii_case("codex");
        if (!is_codex && is_alias) || p_info.explicit_thinking {
            constrained = true;
            let t = p_info.thinking.clone().unwrap_or_default();
            thinking = Some(match thinking {
                None => t,
                Some(prev) => intersect_thinking(&prev, &t),
            });
        }
    }
    if !constrained
        && let Some(i) = info
        && i.explicit_thinking
    {
        thinking = Some(i.thinking.clone().unwrap_or_default());
        constrained = true;
    }
    if constrained && let Some(t) = thinking {
        apply_thinking_metadata(entry, Some(&t), ctx.client_version);
    }
}

// ------------------------------------------------------------------ providers

fn is_pure_codex_provider(id: &str, ctx: &CatalogContext<'_>) -> bool {
    if ctx.providers_for_model.is_none() {
        return true;
    }
    let providers = providers_of(ctx, id);
    !providers.is_empty() && providers.iter().all(|p| p.trim().eq_ignore_ascii_case("codex"))
}

fn null_required_options(entry: &mut Entry) {
    entry.insert("apply_patch_tool_type".into(), Value::Null);
    entry.insert("upgrade".into(), Value::Null);
    entry.insert("availability_nux".into(), Value::Null);
}

fn apply_search_tool_support(entry: &mut Entry, id: &str, template_model: bool, ctx: &CatalogContext<'_>) {
    if entry.get("supports_search_tool").and_then(Value::as_bool) != Some(true) {
        return;
    }
    if !template_model {
        entry.insert("supports_search_tool".into(), Value::Bool(false));
        return;
    }
    if ctx.providers_for_model.is_none() {
        return;
    }
    let providers = providers_of(ctx, id);
    if providers.is_empty() || providers.iter().any(|p| !p.trim().eq_ignore_ascii_case("codex")) {
        entry.insert("supports_search_tool".into(), Value::Bool(false));
    }
}

fn apply_provider_capabilities(entry: &mut Entry, id: &str, is_template: bool, ctx: &CatalogContext<'_>) {
    if !is_template {
        apply_search_tool_support(entry, id, false, ctx);
        return;
    }
    if !is_pure_codex_provider(id, ctx) {
        entry.insert("supports_search_tool".into(), Value::Bool(false));
        entry.insert("prefer_websockets".into(), Value::Bool(false));
        entry.insert("service_tiers".into(), Value::Array(vec![]));
        null_required_options(entry);
        return;
    }
    apply_search_tool_support(entry, id, true, ctx);
}

fn apply_cpa_web_search_capability(entry: &mut Entry, id: &str, ctx: &CatalogContext<'_>) {
    // Templates must not supply runtime capability claims or leak CPA-only fields.
    entry.shift_remove("cpa_capabilities");
    if ctx.client_version != "cpa" {
        return;
    }
    if let Some(supported) = (ctx.web_search_capability)(id.trim()) {
        entry.insert("cpa_capabilities".into(), json!({"web_search": supported}));
    }
}

fn use_compact_instructions(entry: &mut Entry) {
    entry.insert("base_instructions".into(), json!(FALLBACK_INSTRUCTIONS));
    entry.insert(
        "model_messages".into(),
        json!({
            "instructions_template": FALLBACK_INSTRUCTIONS,
            "instructions_variables": null,
            "approvals": null,
            "collaboration_modes": null,
            "auto_review": null,
            "permissions": null,
            "multi_agent": null,
        }),
    );
}

// ------------------------------------------------------------------ devin

fn starts_with_devin(s: &str) -> bool {
    let lower = s.trim().to_lowercase();
    if lower.starts_with("devin/") {
        return true;
    }
    lower.find('/').is_some_and(|idx| lower[idx + 1..].starts_with("devin/"))
}

fn is_devin_model(id: &str, model: Option<&Entry>, entry: &Entry, ctx: &CatalogContext<'_>) -> bool {
    let is_devin_info = |info: &cpa_core::registry::ModelInfo| {
        info.r#type.eq_ignore_ascii_case("devin")
            || info.owned_by.eq_ignore_ascii_case("cognition")
            || info.id.to_lowercase().starts_with("devin/")
    };
    if starts_with_devin(id) || starts_with_devin(&str_value(entry, "slug")) {
        return true;
    }
    if str_value(entry, "type").eq_ignore_ascii_case("devin") || str_value(entry, "owned_by").eq_ignore_ascii_case("cognition") {
        return true;
    }
    if let Some(model) = model
        && (str_value(model, "type").eq_ignore_ascii_case("devin") || str_value(model, "owned_by").eq_ignore_ascii_case("cognition"))
    {
        return true;
    }
    match lookup_model_info(id, None) {
        Some(info) => {
            if is_devin_info(&info) {
                return true;
            }
        }
        None => {
            if let Some(base) = after_slash(id)
                && let Some(info) = lookup_model_info(base, None)
                && is_devin_info(&info)
            {
                return true;
            }
        }
    }
    providers_of(ctx, id).iter().any(|p| p.trim().eq_ignore_ascii_case("devin"))
}

fn apply_devin_display_name(entry: &mut Entry, id: &str, model: &Entry, ctx: &CatalogContext<'_>) {
    if !is_devin_model(id, Some(model), entry, ctx) {
        return;
    }
    let mut display = str_value(entry, "display_name");
    if display.is_empty() {
        display = id.to_string();
    }
    let trimmed = display.trim().to_string();
    if trimmed.ends_with(" (Devin)") {
        return;
    }
    let lower = trimmed.to_lowercase();
    let renamed = if lower.ends_with(" (devin)") {
        format!("{} (Devin)", &trimmed[..trimmed.len() - " (devin)".len()])
    } else if lower.ends_with("(devin)") {
        format!("{} (Devin)", trimmed[..trimmed.len() - "(devin)".len()].trim())
    } else {
        format!("{trimmed} (Devin)")
    };
    entry.insert("display_name".into(), Value::String(renamed));
}

// ------------------------------------------------------------------ apply_patch

/// `applyCodexClientApplyPatchCapability`: advertise the freeform tool only for text models whose
/// every routing candidate supports it.
fn apply_apply_patch_capability(entry: &mut Entry, id: &str, capability: Option<&dyn Fn(&str) -> bool>) {
    entry.insert("apply_patch_tool_type".into(), Value::Null);
    let Some(capability) = capability else { return };
    let base_id = id.trim().to_lowercase();
    let base_id = after_slash(&base_id).unwrap_or(&base_id).trim().to_string();
    if is_image_or_video_model(&base_id) {
        return;
    }
    let (mut supports_text, mut has_modalities) = (false, false);
    if let Some(Value::Array(mods)) = entry.get("input_modalities") {
        has_modalities = !mods.is_empty();
        supports_text = mods.iter().any(|m| m.as_str() == Some("text"));
    }
    if !supports_text && (has_modalities || entry.get("visibility").and_then(Value::as_str) == Some("hide")) {
        return;
    }
    if capability(id.trim()) {
        entry.insert("apply_patch_tool_type".into(), json!("freeform"));
    }
}

// ------------------------------------------------------------------ builder

fn apply_non_template_metadata(entry: &mut Entry, id: &str, model: &Entry, ctx: &CatalogContext<'_>) {
    let info = lookup_model_info(id, None);
    let mut display_name = str_value(model, "display_name");
    let mut description = str_value(model, "description");
    let mut context_window = int_value(model, "context_length");
    let mut thinking = thinking_support_of(model);

    if let Some(info) = &info {
        if !info.display_name.is_empty() {
            display_name = info.display_name.clone();
        }
        if !info.description.is_empty() {
            description = info.description.clone();
        }
        if context_window <= 0 && info.context_length > 0 {
            context_window = info.context_length;
        }
        if info.r#type == OPENAI_IMAGE_MODEL_TYPE {
            entry.insert("visibility".into(), json!("hide"));
            entry.shift_remove("input_modalities");
            entry.shift_remove("supports_image_detail_original");
        } else {
            apply_input_modalities(entry, &info.supported_input_modalities);
        }
        if thinking.is_none() {
            thinking = info.thinking.clone();
        }
    }
    apply_thinking_metadata(entry, thinking.as_ref(), ctx.client_version);

    let max_context = int_value(model, "max_context_length");
    if max_context > 0 {
        context_window = max_context;
    }
    if display_name.is_empty() {
        display_name = id.to_string();
    }
    if description.is_empty() {
        description = id.to_string();
    }
    entry.insert("slug".into(), json!(id));
    entry.insert("display_name".into(), json!(display_name));
    entry.insert("description".into(), json!(description));
    entry.insert("prefer_websockets".into(), json!(false));
    if ctx.optimize_multi_agent_v2 {
        entry.insert("multi_agent_version".into(), json!("v2"));
    }
    entry.insert("service_tiers".into(), Value::Array(vec![]));
    null_required_options(entry);
    if context_window > 0 {
        entry.insert("context_window".into(), json!(context_window));
        entry.insert("max_context_window".into(), json!(context_window));
    }
    if let Some(plans) = model.get("available_in_plans") {
        entry.insert("available_in_plans".into(), plans.clone());
    }
    // Codex 0.156+ caps an explicit model_catalog_url body at 1 MiB, so non-template models get
    // the compact instructions instead of a cloned template prompt.
    use_compact_instructions(entry);
}

fn priority_of(entry: &Entry) -> i64 {
    match entry.get("priority") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(100),
        _ => 100,
    }
}

/// `buildCodexClientModelsWithToolCapabilities`.
pub fn build_models(models: &[Map<String, Value>], ctx: &CatalogContext<'_>) -> Vec<Entry> {
    let (raw, _revision) = get_codex_client_models_snapshot();
    let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(&raw) else {
        return Vec::new();
    };
    let mut templates: Vec<(String, Entry)> = Vec::new();
    let mut default_template: Option<Entry> = None;
    if let Some(Value::Array(items)) = payload.get("models") {
        for item in items {
            let Value::Object(model) = item else { continue };
            let slug = str_value(model, "slug");
            if slug.is_empty() {
                continue;
            }
            if slug == "gpt-5.5" {
                default_template = Some(model.clone());
            }
            templates.retain(|(s, _)| s != &slug);
            templates.push((slug, model.clone()));
        }
    }
    let Some(default_template) = default_template else {
        return Vec::new();
    };
    let template_for = |meta: &str| templates.iter().find(|(s, _)| s == meta).map(|(_, t)| t);

    let mut result: Vec<Entry> = Vec::with_capacity(models.len());
    for model in models {
        let id = str_value(model, "id");
        if id.is_empty() {
            continue;
        }
        let metadata_id = metadata_model_id(&id);
        if let Some(template) = template_for(&metadata_id) {
            let mut entry = template.clone();
            entry.insert("slug".into(), json!(id));
            let info = lookup_model_info(&id, None);
            apply_model_capabilities(&mut entry, &id, &metadata_id, info.as_ref(), ctx);
            for (src, dst) in [("display_name", "display_name"), ("description", "description"), ("base_instructions", "base_instructions")] {
                let v = str_value(model, src);
                if !v.is_empty() {
                    entry.insert(dst.into(), Value::String(v));
                }
            }
            let max_context = int_value(model, "max_context_length");
            if max_context > 0 {
                entry.insert("context_window".into(), json!(max_context));
                entry.insert("max_context_window".into(), json!(max_context));
            }
            let max_tokens = int_value(model, "max_completion_tokens");
            if max_tokens > 0 {
                entry.insert("max_tokens".into(), json!(max_tokens));
            }
            if let Some(t) = thinking_support_of(model) {
                apply_thinking_metadata(&mut entry, Some(&t), ctx.client_version);
            }
            apply_provider_capabilities(&mut entry, &id, true, ctx);
            apply_cpa_web_search_capability(&mut entry, &id, ctx);
            sanitize_reasoning_metadata(&mut entry, ctx.client_version);
            if is_image_or_video_model(&id) {
                entry.insert("visibility".into(), json!("hide"));
            }
            if ctx.optimize_multi_agent_v2 {
                entry.insert("multi_agent_version".into(), json!("v2"));
            }
            apply_devin_display_name(&mut entry, &id, model, ctx);
            apply_apply_patch_capability(&mut entry, &id, ctx.apply_patch_capability);
            result.push(entry);
            continue;
        }

        let mut entry = default_template.clone();
        apply_non_template_metadata(&mut entry, &id, model, ctx);
        let max_tokens = int_value(model, "max_completion_tokens");
        if max_tokens > 0 {
            entry.insert("max_tokens".into(), json!(max_tokens));
        }
        apply_provider_capabilities(&mut entry, &id, false, ctx);
        apply_cpa_web_search_capability(&mut entry, &id, ctx);
        sanitize_reasoning_metadata(&mut entry, ctx.client_version);
        if is_image_or_video_model(&id) {
            entry.insert("visibility".into(), json!("hide"));
        }
        apply_devin_display_name(&mut entry, &id, model, ctx);
        apply_apply_patch_capability(&mut entry, &id, ctx.apply_patch_capability);
        result.push(entry);
    }

    apply_non_template_priorities(&mut result, &templates);
    // Stable sort by priority (templates keep their own, others follow alphabetically).
    result.sort_by_key(priority_of);
    result
}

/// `applyCodexClientNonTemplatePriorities`: models without a template follow the templates,
/// ordered by display name then slug, 100 apart.
fn apply_non_template_priorities(result: &mut [Entry], templates: &[(String, Entry)]) {
    if result.is_empty() {
        return;
    }
    let base = templates.iter().map(|(_, t)| priority_of(t)).max().unwrap_or(0).max(0);
    let mut pending: Vec<(usize, String, String)> = Vec::new();
    for (index, entry) in result.iter().enumerate() {
        let slug = str_value(entry, "slug");
        if templates.iter().any(|(s, _)| s == &metadata_model_id(&slug)) {
            continue;
        }
        let mut display = str_value(entry, "display_name");
        if display.is_empty() {
            display = slug.clone();
        }
        pending.push((index, display, slug));
    }
    pending.sort_by(|a, b| {
        let (l, r) = (a.1.to_lowercase(), b.1.to_lowercase());
        if l == r { a.2.cmp(&b.2) } else { l.cmp(&r) }
    });
    for (rank, (index, _, _)) in pending.into_iter().enumerate() {
        result[index].insert("priority".into(), json!(base + 100 * (rank as i64 + 1)));
    }
}

/// Compact JSON of `{"models":[...]}`: sorted keys, floats formatted like Go, HTML unescaped.
pub fn marshal_compact(models: Vec<Entry>) -> Result<Vec<u8>, String> {
    let payload = json!({ "models": models });
    go_json_sorted(&payload, GoJsonStyle { html_escape: false, float_numbers: true })
        .map(String::into_bytes)
        .ok_or_else(|| "number out of range".to_string())
}

/// Body for `GET /v1/models?client_version=...` for the running registry and config.
pub fn build_client_models_body(client_version: &str, cfg: &Config, manager: &Manager) -> Result<Vec<u8>, String> {
    let registry = global_registry();
    let available = registry.get_available_models("openai");
    let providers_for_model = |id: &str| registry.get_model_providers(id);
    let web_search = |id: &str| registry.get_responses_web_search_capability(id);
    let apply_patch = |model: &str| match request_details(model, false) {
        Ok((providers, _)) => manager.supports_apply_patch_for_providers(&providers, model),
        Err(_) => false,
    };
    let ctx = CatalogContext {
        providers_for_model: Some(&providers_for_model),
        web_search_capability: &web_search,
        apply_patch_capability: cfg.client.codex.enable_apply_patch.then_some(&apply_patch as &dyn Fn(&str) -> bool),
        optimize_multi_agent_v2: cfg.client.codex.optimize_multi_agent_v2,
        client_version,
    };
    marshal_compact(build_models(&available, &ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        providers: &'a dyn Fn(&str) -> Vec<String>,
        search: &'a dyn Fn(&str) -> Option<bool>,
        apply_patch: Option<&'a dyn Fn(&str) -> bool>,
        version: &'a str,
    ) -> CatalogContext<'a> {
        CatalogContext {
            providers_for_model: Some(providers),
            web_search_capability: search,
            apply_patch_capability: apply_patch,
            optimize_multi_agent_v2: false,
            client_version: version,
        }
    }

    fn model(id: &str) -> Map<String, Value> {
        json!({"id": id, "object": "model"}).as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn version_gate_for_extended_reasoning_levels() {
        assert!(supports_extended_reasoning_levels(""));
        assert!(supports_extended_reasoning_levels("0.144.0"));
        assert!(supports_extended_reasoning_levels("v1.0.0-beta+x"));
        assert!(!supports_extended_reasoning_levels("0.143.9"));
        assert!(supports_extended_reasoning_levels("cpa"));
        assert_eq!(normalize_reasoning_level(" XHigh ", "0.100.0"), "xhigh");
        assert_eq!(normalize_reasoning_level("ultra", "0.100.0"), "");
        assert_eq!(normalize_reasoning_level("ultra", "0.200.0"), "ultra");
    }

    #[test]
    fn thinking_levels_pick_medium_as_default() {
        let mut entry = Entry::new();
        let thinking = ThinkingSupport { levels: vec!["none".into(), "low".into(), "medium".into(), "bogus".into()], ..Default::default() };
        apply_thinking_metadata(&mut entry, Some(&thinking), "");
        assert_eq!(entry["default_reasoning_level"], "medium");
        assert_eq!(entry["supported_reasoning_levels"].as_array().map(Vec::len), Some(3));
        let first_non_none = ThinkingSupport { levels: vec!["none".into(), "high".into()], ..Default::default() };
        apply_thinking_metadata(&mut entry, Some(&first_non_none), "");
        assert_eq!(entry["default_reasoning_level"], "high");
    }

    #[test]
    fn apply_patch_needs_a_text_capable_model_and_executor_support() {
        let yes = |_: &str| true;
        let mut text = Entry::new();
        apply_apply_patch_capability(&mut text, "gpt-5", Some(&yes));
        assert_eq!(text["apply_patch_tool_type"], "freeform");
        let mut image_only = json!({"input_modalities": ["image"]}).as_object().cloned().unwrap_or_default();
        apply_apply_patch_capability(&mut image_only, "m", Some(&yes));
        assert_eq!(image_only["apply_patch_tool_type"], Value::Null);
        let mut hidden = json!({"visibility": "hide"}).as_object().cloned().unwrap_or_default();
        apply_apply_patch_capability(&mut hidden, "m", Some(&yes));
        assert_eq!(hidden["apply_patch_tool_type"], Value::Null);
        let mut video = Entry::new();
        apply_apply_patch_capability(&mut video, "x/grok-imagine-video", Some(&yes));
        assert_eq!(video["apply_patch_tool_type"], Value::Null);
        let mut none = Entry::new();
        apply_apply_patch_capability(&mut none, "gpt-5", None);
        assert_eq!(none["apply_patch_tool_type"], Value::Null);
    }

    #[test]
    fn unknown_models_get_the_default_template_with_compact_instructions() {
        let providers = |_: &str| vec!["claude".to_string()];
        let search = |_: &str| None;
        let models = vec![model("zz-custom-model")];
        let out = build_models(&models, &ctx(&providers, &search, None, ""));
        assert_eq!(out.len(), 1);
        let e = &out[0];
        assert_eq!(e["slug"], "zz-custom-model");
        assert_eq!(e["display_name"], "zz-custom-model");
        assert_eq!(e["base_instructions"], FALLBACK_INSTRUCTIONS);
        assert_eq!(e["prefer_websockets"], false);
        assert_eq!(e["service_tiers"], json!([]));
        assert_eq!(e["supports_search_tool"], false);
        assert_eq!(e["apply_patch_tool_type"], Value::Null);
        // non-template models are ranked after every template
        assert!(priority_of(e) >= 100);
    }

    #[test]
    fn template_models_keep_their_template_and_lose_search_for_non_codex_providers() {
        let search = |_: &str| None;
        let codex = |_: &str| vec!["codex".to_string()];
        let other = |_: &str| vec!["claude".to_string()];
        let m = vec![model("gpt-5.5")];
        let pure = build_models(&m, &ctx(&codex, &search, None, ""));
        assert_eq!(pure[0]["slug"], "gpt-5.5");
        let routed = build_models(&m, &ctx(&other, &search, None, ""));
        assert_eq!(routed[0]["supports_search_tool"], false);
        assert_eq!(routed[0]["prefer_websockets"], false);
        assert_eq!(routed[0]["upgrade"], Value::Null);
    }

    #[test]
    fn cpa_client_version_exposes_web_search_capability() {
        let providers = |_: &str| vec!["codex".to_string()];
        let search = |_: &str| Some(true);
        let out = build_models(&[model("gpt-5.5")], &ctx(&providers, &search, None, "cpa"));
        assert_eq!(out[0]["cpa_capabilities"], json!({"web_search": true}));
        let out = build_models(&[model("gpt-5.5")], &ctx(&providers, &search, None, "1.0.0"));
        assert!(out[0].get("cpa_capabilities").is_none());
    }

    #[test]
    fn marshal_is_compact_sorted_and_unescaped() {
        let mut e = Entry::new();
        e.insert("b".into(), json!("<x>"));
        e.insert("a".into(), json!(1));
        let out = String::from_utf8(marshal_compact(vec![e]).unwrap()).unwrap();
        assert_eq!(out, r#"{"models":[{"a":1,"b":"<x>"}]}"#);
    }

    #[test]
    fn devin_models_get_the_suffix_once() {
        let providers = |_: &str| vec!["devin".to_string()];
        let search = |_: &str| None;
        let out = build_models(&[model("devin/swe-1.5")], &ctx(&providers, &search, None, ""));
        assert!(out[0]["display_name"].as_str().unwrap_or("").ends_with(" (Devin)"));
        let twice = out[0]["display_name"].as_str().unwrap_or("").to_string();
        assert_eq!(twice.matches("(Devin)").count(), 1);
    }
}
