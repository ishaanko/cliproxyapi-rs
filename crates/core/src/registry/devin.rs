//! The Devin model catalog (Go: registry/devin_models.go, devin_models_updater.go).
//!
//! The embedded devin_models.json is aggregated at load time: effort/speed variants
//! (`foo-high-fast`, `foo_max`, ...) fold into one base model whose thinking levels are the union.
//! The remote refresh is driven by the caller: fetch [`DEVIN_MODELS_URLS`] (8 MiB cap, every
//! [`super::MODELS_REFRESH_INTERVAL`]) and pass the bytes to [`load_devin_models_from_bytes`].

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use parking_lot::RwLock;
use serde::Deserialize;

use super::catalog::static_models;
use super::definitions::{static_devin_models, upsert_model_infos};
use super::model_info::{ModelInfo, ThinkingSupport, null_default};

pub const MAX_DEVIN_MODELS_SIZE: usize = 8 << 20;

/// Remote Devin catalog locations, tried in order.
pub const DEVIN_MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/devin_models.json",
    "https://models.router-for.me/devin_models.json",
];

static EMBEDDED_DEVIN_MODELS_JSON: &str = include_str!("../../assets/devin_models.json");

const DEVIN_BUILTIN_SWE_1_6_SLOW_ID: &str = "devin/swe-1-6-slow";

#[derive(Default)]
struct DevinStore {
    models: Vec<ModelInfo>,
    raw_json: Vec<u8>,
    revision: u64,
}

static DEVIN_STORE: LazyLock<RwLock<DevinStore>> = LazyLock::new(|| {
    let mut store = DevinStore::default();
    match validate_devin_models_json(EMBEDDED_DEVIN_MODELS_JSON.as_bytes()) {
        Ok(models) => {
            store.models = with_devin_builtins(models);
            store.raw_json = EMBEDDED_DEVIN_MODELS_JSON.as_bytes().to_vec();
            store.revision = 1;
        }
        Err(err) => tracing::warn!(
            "registry: failed to parse embedded devin_models.json (will rely on static fallback and remote refresh): embed: {err}"
        ),
    }
    RwLock::new(store)
});

fn devin_builtin_swe_1_6_slow() -> ModelInfo {
    ModelInfo {
        id: DEVIN_BUILTIN_SWE_1_6_SLOW_ID.into(),
        object: "model".into(),
        r#type: "devin".into(),
        owned_by: "cognition".into(),
        display_name: "SWE-1.6 Slow".into(),
        context_length: 200_000,
        max_completion_tokens: 64_000,
        input_token_limit: 200_000,
        output_token_limit: 64_000,
        supported_input_modalities: vec!["text".into(), "image".into()],
        supported_output_modalities: vec!["text".into()],
        supported_generation_methods: vec!["generateContent".into(), "countTokens".into()],
        ..Default::default()
    }
}

/// Injects the hard-coded Devin models that must not depend on catalog updates; built-ins replace
/// any matching ids already present.
pub fn with_devin_builtins(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    upsert_model_infos(models, vec![devin_builtin_swe_1_6_slow()])
}

/// The active Devin catalog: the dynamic/embedded devin_models.json, else models.json's `devin`
/// section, else the hard-coded static list.
pub fn get_devin_models() -> Vec<ModelInfo> {
    let models = DEVIN_STORE.read().models.clone();
    if !models.is_empty() {
        return with_devin_builtins(models);
    }
    let from_catalog = static_models().devin.clone();
    if !from_catalog.is_empty() {
        return with_devin_builtins(from_catalog);
    }
    with_devin_builtins(static_devin_models().clone())
}

fn strip_devin_prefix(id: &str) -> &str {
    id.strip_prefix("devin/").unwrap_or(id)
}

/// Looks a model up in the active Devin catalog; accepts namespaced (`devin/model`) and bare ids,
/// and falls back to the base model of an effort/speed variant (`gpt-6-astra-high`).
pub fn lookup_devin_model(model_id: &str) -> Option<ModelInfo> {
    let lowered = model_id.trim().to_lowercase();
    let clean = strip_devin_prefix(&lowered);
    if clean.is_empty() {
        return None;
    }

    let mut models = DEVIN_STORE.read().models.clone();
    if models.is_empty() {
        models = get_devin_models();
    }
    let find = |list: &[ModelInfo], wanted: &str| {
        list.iter()
            .find(|m| strip_devin_prefix(&m.id).to_lowercase() == wanted)
            .cloned()
    };

    if let Some(found) = find(&models, clean) {
        return Some(found);
    }
    if let Some(found) = find(&with_devin_builtins(Vec::new()), clean) {
        return Some(found);
    }
    // A thinking variant (claude-opus-5-low-fast, gpt-6-astra-high): look up its base model.
    let (base, _) = split_devin_model_id(clean);
    if base != clean && !base.is_empty() {
        return find(&models, &base);
    }
    None
}

/// Raw JSON of the current Devin catalog payload.
pub fn get_devin_models_json() -> Vec<u8> {
    DEVIN_STORE.read().raw_json.clone()
}

/// Revision counter of the Devin catalog (bumped when the payload changes).
pub fn get_devin_models_revision() -> u64 {
    DEVIN_STORE.read().revision
}

/// Raw JSON and revision of the Devin catalog, read consistently.
pub fn get_devin_models_snapshot() -> (Vec<u8>, u64) {
    let store = DEVIN_STORE.read();
    (store.raw_json.clone(), store.revision)
}

/// Validates and installs a Devin catalog payload. `Ok(true)` when the payload differs from the
/// current one (revision bumped), `Ok(false)` when identical.
pub fn load_devin_models_from_bytes(data: &[u8], source: &str) -> Result<bool, String> {
    let models = validate_devin_models_json(data).map_err(|e| format!("{source}: {e}"))?;
    let models = with_devin_builtins(models);

    let mut store = DEVIN_STORE.write();
    if store.raw_json == data {
        return Ok(false);
    }
    store.models = models;
    store.raw_json = data.to_vec();
    store.revision += 1;
    Ok(true)
}

#[derive(Deserialize, Default)]
struct DevinFilePayload {
    #[serde(default, deserialize_with = "null_default")]
    devin: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    models: Vec<Option<ModelInfo>>,
}

/// Parses and validates a Devin catalog payload: `{"devin":[...]}`, `{"models":[...]}` or a bare
/// array of models. Ids are namespaced under `devin/`, lowercased and must be unique; variants are
/// aggregated into base models.
pub fn validate_devin_models_json(data: &[u8]) -> Result<Vec<ModelInfo>, String> {
    if data.trim_ascii().is_empty() {
        return Err("empty Devin models payload".into());
    }
    if let Ok(payload) = serde_json::from_slice::<DevinFilePayload>(data) {
        let candidates = if payload.devin.is_empty() {
            payload.models
        } else {
            payload.devin
        };
        if !candidates.is_empty() {
            return sanitize_and_validate_devin_models(candidates);
        }
    }
    if let Ok(list) = serde_json::from_slice::<Vec<Option<ModelInfo>>>(data)
        && !list.is_empty()
    {
        return sanitize_and_validate_devin_models(list);
    }
    Err("invalid Devin models JSON: expected non-empty 'devin'/'models' array or model list".into())
}

struct CompoundSuffix {
    suffix: &'static str,
    effort: &'static str,
    readd: &'static str,
}

const fn compound(
    suffix: &'static str,
    effort: &'static str,
    readd: &'static str,
) -> CompoundSuffix {
    CompoundSuffix {
        suffix,
        effort,
        readd,
    }
}

const DEVIN_COMPOUND_SUFFIXES: [CompoundSuffix; 16] = [
    compound("-low-fast", "low", ""),
    compound("-medium-fast", "medium", ""),
    compound("-high-fast", "high", ""),
    compound("-xhigh-fast", "xhigh", ""),
    compound("-max-fast", "max", ""),
    compound("-none-fast", "none", ""),
    compound("-low-priority", "low", ""),
    compound("-medium-priority", "medium", ""),
    compound("-high-priority", "high", ""),
    compound("-xhigh-priority", "xhigh", ""),
    compound("-max-priority", "max", ""),
    compound("-none-priority", "none", ""),
    compound("-thinking-1m", "", "-1m"),
    compound("-thinking", "", ""),
    compound("-max-1m", "max", "-1m"),
    compound("-none-1m", "none", "-1m"),
];

const DEVIN_SIMPLE_EFFORT_SUFFIXES: [(&str, &str); 7] = [
    ("-none", "none"),
    ("-minimal", "minimal"),
    ("-low", "low"),
    ("-medium", "medium"),
    ("-high", "high"),
    ("-xhigh", "xhigh"),
    ("-max", "max"),
];

const DEVIN_DISPLAY_NAME_SUFFIXES: [&str; 26] = [
    " Low Fast",
    " Medium Fast",
    " High Fast",
    " XHigh Fast",
    " Max Fast",
    " Low Thinking Fast",
    " Medium Thinking Fast",
    " High Thinking Fast",
    " XHigh Thinking Fast",
    " Max Thinking Fast",
    " No Thinking Fast",
    " Low Thinking",
    " Medium Thinking",
    " High Thinking",
    " XHigh Thinking",
    " Max Thinking",
    " No Thinking",
    " Low",
    " Medium",
    " High",
    " XHigh",
    " Max",
    " None",
    " Minimal",
    " Thinking",
    " Fast",
];

fn devin_level_rank(level: &str) -> i32 {
    match level {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        "fast" => 7,
        "priority" => 8,
        _ => 99,
    }
}

/// Splits a lowercase devin model id (without prefix) into its base id and reasoning effort.
fn split_devin_model_id(clean_id: &str) -> (String, &'static str) {
    if clean_id == "swe-1-6-slow" {
        return (clean_id.to_string(), "");
    }
    if clean_id == "swe-1-6-fast" {
        return ("swe-1-6".to_string(), "");
    }

    let upper = clean_id.to_uppercase();
    for (suffix, effort) in [
        ("_NONE", "none"),
        ("_MINIMAL", "minimal"),
        ("_LOW", "low"),
        ("_MEDIUM", "medium"),
        ("_HIGH", "high"),
        ("_XHIGH", "xhigh"),
        ("_MAX", "max"),
        ("_THINKING", "high"),
    ] {
        if upper.ends_with(suffix) {
            // ASCII suffixes: byte arithmetic on the original string is on a char boundary.
            let cut = clean_id.len() - suffix.len();
            return (clean_id.get(..cut).unwrap_or(clean_id).to_string(), effort);
        }
    }

    for s in &DEVIN_COMPOUND_SUFFIXES {
        if let Some(base) = clean_id.strip_suffix(s.suffix) {
            return (format!("{base}{}", s.readd), s.effort);
        }
    }
    for (suffix, effort) in DEVIN_SIMPLE_EFFORT_SUFFIXES {
        if let Some(base) = clean_id.strip_suffix(suffix) {
            return (base.to_string(), effort);
        }
    }
    (clean_id.to_string(), "")
}

/// Strips effort/speed suffixes from a display name (`GPT-6 Astra High Fast` -> `GPT-6 Astra`).
fn clean_devin_display_name(name: &str) -> String {
    let mut trimmed = name.trim().to_string();
    loop {
        let lower = trimmed.to_lowercase();
        let hit = DEVIN_DISPLAY_NAME_SUFFIXES
            .iter()
            .find(|s| lower.ends_with(&s.to_lowercase()));
        let Some(s) = hit else { break };
        let cut = trimmed.len().saturating_sub(s.len());
        match trimmed.get(..cut) {
            Some(head) => trimmed = head.trim().to_string(),
            None => break,
        }
    }
    trimmed
}

/// Folds model variants into one entry per base id: max limits, unioned modalities/methods and a
/// level-based thinking config from every effort seen. Order of first appearance is kept.
fn aggregate_devin_models(models: Vec<Option<ModelInfo>>) -> Vec<ModelInfo> {
    struct Entry {
        model: ModelInfo,
        levels: BTreeSet<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut aggregated: HashMap<String, Entry> = HashMap::new();

    for m in models.into_iter().flatten() {
        let clean_id = strip_devin_prefix(m.id.trim()).to_lowercase();
        let (mut base_id, effort) = split_devin_model_id(&clean_id);
        if base_id.is_empty() {
            base_id = clean_id.clone();
        }
        let namespaced_base = format!("devin/{base_id}");
        let is_base = base_id == clean_id;

        let entry = aggregated
            .entry(namespaced_base.clone())
            .or_insert_with(|| {
                order.push(namespaced_base.clone());
                let mut clone = m.clone();
                clone.id = namespaced_base.clone();
                clone.display_name = clean_devin_display_name(&m.display_name);
                if clone.display_name.is_empty() {
                    clone.display_name = m.display_name.clone();
                }
                Entry {
                    model: clone,
                    levels: BTreeSet::new(),
                }
            });

        if is_base {
            if !m.display_name.is_empty() {
                entry.model.display_name = clean_devin_display_name(&m.display_name);
            }
            if !m.owned_by.is_empty() {
                entry.model.owned_by = m.owned_by.clone();
            }
        }
        entry.model.context_length = entry.model.context_length.max(m.context_length);
        entry.model.max_completion_tokens = entry
            .model
            .max_completion_tokens
            .max(m.max_completion_tokens);
        entry.model.input_token_limit = entry.model.input_token_limit.max(m.input_token_limit);
        entry.model.output_token_limit = entry.model.output_token_limit.max(m.output_token_limit);
        for (dst, src) in [
            (
                &mut entry.model.supported_input_modalities,
                &m.supported_input_modalities,
            ),
            (
                &mut entry.model.supported_output_modalities,
                &m.supported_output_modalities,
            ),
            (
                &mut entry.model.supported_generation_methods,
                &m.supported_generation_methods,
            ),
        ] {
            for item in src {
                if !dst.contains(item) {
                    dst.push(item.clone());
                }
            }
        }

        if let Some(thinking) = &m.thinking {
            for level in &thinking.levels {
                if !level.is_empty() && level != "priority" {
                    entry.levels.insert(level.clone());
                }
            }
        }
        if !effort.is_empty() && effort != "priority" {
            entry.levels.insert(effort.to_string());
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for id in order {
        let Some(Entry { mut model, levels }) = aggregated.remove(&id) else {
            continue;
        };
        if !levels.is_empty() {
            let mut lvls: Vec<String> = levels.into_iter().collect();
            lvls.sort_by(|a, b| {
                devin_level_rank(a)
                    .cmp(&devin_level_rank(b))
                    .then_with(|| a.cmp(b))
            });
            model.thinking = Some(ThinkingSupport {
                levels: lvls,
                ..Default::default()
            });
        }
        if model.r#type.is_empty() {
            model.r#type = "devin".into();
        }
        if model.object.is_empty() {
            model.object = "model".into();
        }
        if model.supported_input_modalities.is_empty() {
            model.supported_input_modalities = vec!["text".into()];
        }
        if model.supported_output_modalities.is_empty() {
            model.supported_output_modalities = vec!["text".into()];
        }
        if model.input_token_limit == 0 && model.context_length > 0 {
            model.input_token_limit = model.context_length;
        }
        if model.output_token_limit == 0 && model.max_completion_tokens > 0 {
            model.output_token_limit = model.max_completion_tokens;
        }
        if model.supported_generation_methods.is_empty() {
            model.supported_generation_methods =
                vec!["generateContent".into(), "countTokens".into()];
        }
        out.push(model);
    }
    out
}

fn sanitize_and_validate_devin_models(
    mut models: Vec<Option<ModelInfo>>,
) -> Result<Vec<ModelInfo>, String> {
    let mut seen = std::collections::HashSet::with_capacity(models.len());
    for (i, slot) in models.iter_mut().enumerate() {
        let Some(m) = slot else {
            return Err(format!("model at index {i} is null"));
        };
        let mut id = m.id.trim().to_string();
        if id.is_empty() {
            return Err(format!("model at index {i} has empty id"));
        }
        // Namespace under devin/ unless already prefixed.
        if !id.to_lowercase().starts_with("devin/") {
            id = format!("devin/{id}");
        }
        let id = id.to_lowercase();
        m.id = id.clone();
        if !seen.insert(id.clone()) {
            return Err(format!("duplicate model id: {id:?}"));
        }
    }
    Ok(aggregate_devin_models(models))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_ids_split_into_base_and_effort() {
        assert_eq!(
            split_devin_model_id("claude-opus-5-low-fast"),
            ("claude-opus-5".to_string(), "low")
        );
        assert_eq!(
            split_devin_model_id("foo-thinking-1m"),
            ("foo-1m".to_string(), "")
        );
        assert_eq!(split_devin_model_id("foo_MAX"), ("foo".to_string(), "max"));
        assert_eq!(
            split_devin_model_id("gpt-6-astra-high"),
            ("gpt-6-astra".to_string(), "high")
        );
        assert_eq!(
            split_devin_model_id("swe-1-6-fast"),
            ("swe-1-6".to_string(), "")
        );
        assert_eq!(split_devin_model_id("plain"), ("plain".to_string(), ""));
    }

    #[test]
    fn display_names_lose_variant_suffixes() {
        assert_eq!(
            clean_devin_display_name("GPT-6 Astra High Fast"),
            "GPT-6 Astra"
        );
        assert_eq!(clean_devin_display_name("Claude Low Thinking"), "Claude");
    }

    #[test]
    fn embedded_catalog_aggregates_and_looks_up_variants() {
        let models = get_devin_models();
        assert!(models.iter().any(|m| m.id == DEVIN_BUILTIN_SWE_1_6_SLOW_ID));
        assert!(models.iter().all(|m| m.id.starts_with("devin/")));
        let base = models
            .iter()
            .find(|m| m.thinking.is_some())
            .expect("a level model");
        let variant = format!("{}-high", base.id.trim_start_matches("devin/"));
        assert!(lookup_devin_model(&variant).is_some());
        assert!(get_devin_models_revision() >= 1);
    }

    #[test]
    fn aggregation_unions_levels_in_rank_order() {
        let models = validate_devin_models_json(
            br#"{"devin":[{"id":"x-high","display_name":"X High","context_length":10},
                          {"id":"x","display_name":"X","owned_by":"o","context_length":20,"thinking":{"levels":["low","priority"]}},
                          {"id":"x-max-fast","context_length":5}]}"#,
        )
        .unwrap();
        assert_eq!(models.len(), 1);
        let x = &models[0];
        assert_eq!(
            (
                x.id.as_str(),
                x.display_name.as_str(),
                x.owned_by.as_str(),
                x.context_length
            ),
            ("devin/x", "X", "o", 20)
        );
        assert_eq!(x.thinking.as_ref().unwrap().levels, ["low", "high", "max"]);
        assert_eq!(x.input_token_limit, 20);
        assert!(validate_devin_models_json(b"{}").is_err());
        assert!(validate_devin_models_json(br#"[{"id":"a"},{"id":"DEVIN/a"}]"#).is_err());
    }
}
