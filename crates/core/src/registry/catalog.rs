//! The static model catalog: the embedded models.json, its validation and the remote refresh
//! (Go: registry/model_updater.go).
//!
//! The Go updater fetches over HTTP on a timer. Networking stays outside core: the caller fetches
//! [`MODELS_URLS`] (30 s timeout each, status 200 required, every [`MODELS_REFRESH_INTERVAL`]) and
//! feeds the bytes to [`apply_remote_models`].

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde::Deserialize;

use super::model_info::{ModelInfo, null_default};

pub const MODELS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
pub const MODELS_REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);

/// Remote catalog locations, tried in order.
pub const MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json",
    "https://models.router-for.me/models.json",
];

static EMBEDDED_MODELS_JSON: &str = include_str!("../../assets/models.json");

/// Top-level structure of models.json (the legacy `gemini-cli` array is ignored).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StaticModels {
    pub claude: Vec<ModelInfo>,
    pub gemini: Vec<ModelInfo>,
    pub vertex: Vec<ModelInfo>,
    pub aistudio: Vec<ModelInfo>,
    pub codex_free: Vec<ModelInfo>,
    pub codex_team: Vec<ModelInfo>,
    pub codex_plus: Vec<ModelInfo>,
    pub codex_pro: Vec<ModelInfo>,
    pub kimi: Vec<ModelInfo>,
    pub antigravity: Vec<ModelInfo>,
    pub xai: Vec<ModelInfo>,
    pub devin: Vec<ModelInfo>,
    pub meta: Vec<ModelInfo>,
}

/// Wire shape: sections may hold `null` entries, which validation rejects.
#[derive(Deserialize, Default)]
struct RawCatalog {
    #[serde(default, deserialize_with = "null_default")]
    claude: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    gemini: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    vertex: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    aistudio: Vec<Option<ModelInfo>>,
    #[serde(default, rename = "codex-free", deserialize_with = "null_default")]
    codex_free: Vec<Option<ModelInfo>>,
    #[serde(default, rename = "codex-team", deserialize_with = "null_default")]
    codex_team: Vec<Option<ModelInfo>>,
    #[serde(default, rename = "codex-plus", deserialize_with = "null_default")]
    codex_plus: Vec<Option<ModelInfo>>,
    #[serde(default, rename = "codex-pro", deserialize_with = "null_default")]
    codex_pro: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    kimi: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    antigravity: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    xai: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    devin: Vec<Option<ModelInfo>>,
    #[serde(default, deserialize_with = "null_default")]
    meta: Vec<Option<ModelInfo>>,
}

/// Parses and validates a models catalog: no null entry, no empty id and no duplicate id within a
/// section (devin is not validated, null entries there are dropped); an empty section only warns.
pub fn parse_models_catalog(data: &[u8]) -> Result<StaticModels, String> {
    let raw: RawCatalog = serde_json::from_slice(data).map_err(|e| e.to_string())?;

    let required: [(&str, &Vec<Option<ModelInfo>>); 12] = [
        ("claude", &raw.claude),
        ("gemini", &raw.gemini),
        ("vertex", &raw.vertex),
        ("aistudio", &raw.aistudio),
        ("codex-free", &raw.codex_free),
        ("codex-team", &raw.codex_team),
        ("codex-plus", &raw.codex_plus),
        ("codex-pro", &raw.codex_pro),
        ("kimi", &raw.kimi),
        ("antigravity", &raw.antigravity),
        ("xai", &raw.xai),
        ("meta", &raw.meta),
    ];
    for (name, models) in required {
        validate_model_section(name, models)?;
    }

    let flatten = |v: Vec<Option<ModelInfo>>| v.into_iter().flatten().collect::<Vec<_>>();
    Ok(StaticModels {
        claude: flatten(raw.claude),
        gemini: flatten(raw.gemini),
        vertex: flatten(raw.vertex),
        aistudio: flatten(raw.aistudio),
        codex_free: flatten(raw.codex_free),
        codex_team: flatten(raw.codex_team),
        codex_plus: flatten(raw.codex_plus),
        codex_pro: flatten(raw.codex_pro),
        kimi: flatten(raw.kimi),
        antigravity: flatten(raw.antigravity),
        xai: flatten(raw.xai),
        devin: flatten(raw.devin),
        meta: flatten(raw.meta),
    })
}

fn validate_model_section(section: &str, models: &[Option<ModelInfo>]) -> Result<(), String> {
    if models.is_empty() {
        tracing::warn!(
            "models catalog: {section} section is empty, continuing without those model definitions"
        );
        return Ok(());
    }
    let mut seen = std::collections::HashSet::with_capacity(models.len());
    for (i, model) in models.iter().enumerate() {
        let Some(model) = model else {
            return Err(format!("{section}[{i}] is null"));
        };
        let model_id = model.id.trim();
        if model_id.is_empty() {
            return Err(format!("{section}[{i}] has empty id"));
        }
        if !seen.insert(model_id) {
            return Err(format!(
                "{section} contains duplicate model id {model_id:?}"
            ));
        }
    }
    Ok(())
}

static CATALOG: LazyLock<RwLock<Arc<StaticModels>>> = LazyLock::new(|| {
    // Embedded data is the startup fallback; a parse failure only warns (Go parity).
    let initial = match parse_models_catalog(EMBEDDED_MODELS_JSON.as_bytes()) {
        Ok(catalog) => catalog,
        Err(err) => {
            tracing::warn!(
                "registry: failed to parse embedded models.json (embedded catalog may be incomplete or invalid; continuing startup and will rely on remote model refresh): embed: decode models catalog: {err}"
            );
            StaticModels::default()
        }
    };
    RwLock::new(Arc::new(initial))
});

/// The current catalog snapshot (embedded data until a refresh succeeds).
pub fn static_models() -> Arc<StaticModels> {
    CATALOG.read().clone()
}

/// Replaces the catalog from JSON bytes without change detection or callbacks (Go: loadModelsFromBytes).
pub fn load_models_from_bytes(data: &[u8], source: &str) -> Result<(), String> {
    let parsed = parse_models_catalog(data)
        .map_err(|e| format!("{source}: decode/validate models catalog: {e}"))?;
    *CATALOG.write() = Arc::new(parsed);
    Ok(())
}

/// Invoked with the provider names whose model definitions changed after a refresh.
pub type ModelRefreshCallback = Arc<dyn Fn(&[String]) + Send + Sync>;

struct RefreshState {
    callback: Option<ModelRefreshCallback>,
    pending: Vec<String>,
}

static REFRESH_STATE: Mutex<RefreshState> = Mutex::new(RefreshState {
    callback: None,
    pending: Vec::new(),
});

/// Registers the callback invoked when a refresh detects changes (replaces any previous one).
/// Changes detected before a callback existed are delivered immediately.
pub fn set_model_refresh_callback(callback: Option<ModelRefreshCallback>) {
    let mut state = REFRESH_STATE.lock();
    state.callback = callback.clone();
    let pending = match callback {
        Some(_) if !state.pending.is_empty() => std::mem::take(&mut state.pending),
        _ => Vec::new(),
    };
    drop(state);
    if let (Some(cb), false) = (callback, pending.is_empty()) {
        cb(&pending);
    }
}

/// Applies freshly fetched catalog bytes (Go: tryRefreshModels after a successful fetch).
///
/// Parses and validates, keeps the previous `meta` section when the new one is empty, detects
/// changed providers, stores the new catalog regardless of changes and notifies the callback.
/// Returns the changed providers; errors leave the current catalog untouched.
pub fn apply_remote_models(data: &[u8]) -> Result<Vec<String>, String> {
    let mut parsed = parse_models_catalog(data)?;
    let old = static_models();
    if parsed.meta.is_empty() && !old.meta.is_empty() {
        parsed.meta = old.meta.clone();
    }
    let changed = detect_changed_providers(&old, &parsed);
    *CATALOG.write() = Arc::new(parsed);
    notify_model_refresh(&changed);
    Ok(changed)
}

/// Provider names whose model definitions differ. Gemini changes affect both Gemini protocols,
/// Codex tiers are grouped under one `codex` provider and Kimi under its four aliases.
pub fn detect_changed_providers(old: &StaticModels, new: &StaticModels) -> Vec<String> {
    let sections: [(&str, &[ModelInfo], &[ModelInfo]); 17] = [
        ("claude", &old.claude, &new.claude),
        ("gemini", &old.gemini, &new.gemini),
        ("gemini-interactions", &old.gemini, &new.gemini),
        ("vertex", &old.vertex, &new.vertex),
        ("aistudio", &old.aistudio, &new.aistudio),
        ("codex", &old.codex_free, &new.codex_free),
        ("codex", &old.codex_team, &new.codex_team),
        ("codex", &old.codex_plus, &new.codex_plus),
        ("codex", &old.codex_pro, &new.codex_pro),
        ("kimi", &old.kimi, &new.kimi),
        ("kimi-ai", &old.kimi, &new.kimi),
        ("kimi.ai", &old.kimi, &new.kimi),
        ("kimi.com", &old.kimi, &new.kimi),
        ("antigravity", &old.antigravity, &new.antigravity),
        ("xai", &old.xai, &new.xai),
        ("devin", &old.devin, &new.devin),
        ("meta", &old.meta, &new.meta),
    ];
    let mut changed: Vec<String> = Vec::new();
    for (provider, old_list, new_list) in sections {
        if changed.iter().any(|c| c == provider) {
            continue;
        }
        if model_section_changed(old_list, new_list) {
            changed.push(provider.to_string());
        }
    }
    changed
}

/// Whether two model lists differ in any catalog field, including the internal metadata that is
/// omitted from normal JSON (structural comparison; header overrides are ordered maps).
fn model_section_changed(a: &[ModelInfo], b: &[ModelInfo]) -> bool {
    a != b
}

fn notify_model_refresh(changed: &[String]) {
    if changed.is_empty() {
        return;
    }
    let mut state = REFRESH_STATE.lock();
    let Some(cb) = state.callback.clone() else {
        state.pending = merge_provider_names(&state.pending, changed);
        return;
    };
    drop(state);
    cb(changed);
}

/// Lowercased, trimmed, deduplicated union of two provider lists (existing first).
fn merge_provider_names(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(existing.len() + incoming.len());
    for provider in existing.iter().chain(incoming) {
        let name = provider.trim().to_lowercase();
        if !name.is_empty() && !merged.contains(&name) {
            merged.push(name);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_with_expected_sections() {
        let c = static_models();
        assert!(
            !c.claude.is_empty()
                && !c.gemini.is_empty()
                && !c.codex_pro.is_empty()
                && !c.antigravity.is_empty()
        );
    }

    #[test]
    fn validation_rejects_bad_sections() {
        let bad_null = br#"{"claude":[null]}"#;
        assert_eq!(
            parse_models_catalog(bad_null).unwrap_err(),
            "claude[0] is null"
        );
        let empty_id = br#"{"gemini":[{"id":" "}]}"#;
        assert_eq!(
            parse_models_catalog(empty_id).unwrap_err(),
            "gemini[0] has empty id"
        );
        let dup = br#"{"kimi":[{"id":"a"},{"id":"a"}]}"#;
        assert_eq!(
            parse_models_catalog(dup).unwrap_err(),
            "kimi contains duplicate model id \"a\""
        );
    }

    #[test]
    fn change_detection_groups_providers() {
        let old = StaticModels::default();
        let mut new = StaticModels::default();
        new.gemini.push(ModelInfo {
            id: "g".into(),
            ..Default::default()
        });
        new.codex_team.push(ModelInfo {
            id: "c".into(),
            ..Default::default()
        });
        assert_eq!(
            detect_changed_providers(&old, &new),
            ["gemini", "gemini-interactions", "codex"]
        );
        let mut caps = new.clone();
        caps.codex_team[0].support_configuration_update = true;
        assert_eq!(detect_changed_providers(&new, &caps), ["codex"]);
    }

    #[test]
    fn provider_name_merge() {
        let merged = merge_provider_names(
            &["Gemini".into(), " ".into()],
            &["gemini".into(), "codex".into()],
        );
        assert_eq!(merged, ["gemini", "codex"]);
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;
    use crate::registry::ModelConfig;

    fn with_headers(pairs: &[(&str, &str)]) -> StaticModels {
        let mut catalog = StaticModels::default();
        catalog.claude.push(ModelInfo {
            id: "m".into(),
            config: Some(ModelConfig {
                override_header: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }),
            ..Default::default()
        });
        catalog
    }

    #[test]
    fn header_override_order_is_not_a_change() {
        let a = with_headers(&[("user-agent", "x"), ("x-b", "1"), ("x-a", "2")]);
        let b = with_headers(&[("x-a", "2"), ("x-b", "1"), ("user-agent", "x")]);
        assert!(detect_changed_providers(&a, &b).is_empty());
        let c = with_headers(&[("x-a", "3"), ("x-b", "1"), ("user-agent", "x")]);
        assert_eq!(detect_changed_providers(&a, &c), ["claude"]);
    }
}
