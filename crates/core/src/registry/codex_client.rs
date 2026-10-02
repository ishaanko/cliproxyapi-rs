//! The Codex client model catalog served for `/v1/models?client_version=...`
//! (Go: registry/codex_client_models.go, codex_client_models_updater.go).
//!
//! The payload is kept as raw JSON bytes. Remote refresh is driven by the caller: fetch
//! [`CODEX_CLIENT_MODELS_URLS`] (8 MiB cap, every [`super::MODELS_REFRESH_INTERVAL`]), run
//! [`validate_codex_client_models_json`] and pass the bytes to [`load_codex_client_models_from_bytes`].

use std::collections::HashSet;
use std::sync::LazyLock;

use cpa_json::{Map, Value};
use parking_lot::RwLock;

pub const MAX_CODEX_CLIENT_MODELS_SIZE: usize = 8 << 20;

/// Remote Codex client catalog locations, tried in order.
pub const CODEX_CLIENT_MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/codex_client_models.json",
    "https://models.router-for.me/codex_client_models.json",
];

static EMBEDDED_CODEX_CLIENT_MODELS_JSON: &str =
    include_str!("../../assets/codex_client_models.json");

#[derive(Default)]
struct CodexClientStore {
    data: Vec<u8>,
    revision: u64,
}

static CODEX_CLIENT_STORE: LazyLock<RwLock<CodexClientStore>> = LazyLock::new(|| {
    let mut store = CodexClientStore::default();
    match validate_codex_client_models_json(EMBEDDED_CODEX_CLIENT_MODELS_JSON.as_bytes()) {
        Ok(()) => {
            store.data = EMBEDDED_CODEX_CLIENT_MODELS_JSON.as_bytes().to_vec();
            store.revision = 1;
        }
        Err(err) => tracing::warn!(
            "registry: failed to parse embedded codex_client_models.json (Codex client catalog will remain unavailable until a valid remote refresh): embed: {err}"
        ),
    }
    RwLock::new(store)
});

/// The current Codex client model catalog JSON.
pub fn get_codex_client_models_json() -> Vec<u8> {
    get_codex_client_models_snapshot().0
}

/// Revision of the Codex client catalog; changes only when validated content changes.
pub fn get_codex_client_models_revision() -> u64 {
    CODEX_CLIENT_STORE.read().revision
}

/// A consistent copy of the catalog and its revision.
pub fn get_codex_client_models_snapshot() -> (Vec<u8>, u64) {
    let store = CODEX_CLIENT_STORE.read();
    (store.data.clone(), store.revision)
}

/// Validates and installs a catalog. `Ok(true)` when the content changed (revision bumped),
/// `Ok(false)` when identical.
pub fn load_codex_client_models_from_bytes(data: &[u8], source: &str) -> Result<bool, String> {
    validate_codex_client_models_json(data).map_err(|e| format!("{source}: {e}"))?;
    let mut store = CODEX_CLIENT_STORE.write();
    if store.data == data {
        return Ok(false);
    }
    store.data = data.to_vec();
    store.revision += 1;
    Ok(true)
}

/// Validates the fields required to serve a complete Codex client model catalog: unique non-empty
/// slugs, the `gpt-5.5` default template, and per-model required strings/integers/reasoning levels.
pub fn validate_codex_client_models_json(data: &[u8]) -> Result<(), String> {
    let payload: Value = serde_json::from_slice(data)
        .map_err(|e| format!("decode Codex client model catalog: {e}"))?;
    let models = match payload.get("models") {
        Some(Value::Array(models)) => models.as_slice(),
        _ => &[],
    };
    if models.is_empty() {
        return Err("Codex client model catalog has no models".into());
    }

    let mut seen: HashSet<String> = HashSet::with_capacity(models.len());
    for (i, model) in models.iter().enumerate() {
        let empty = Map::new();
        // Go decodes entries into map[string]any; a non-object entry fails the whole decode.
        let Value::Object(model) = model else {
            if model.is_null() {
                // A JSON null decodes to a nil map: every required field is then missing.
                return Err(format!(
                    "Codex client model catalog models[{i}]: {}",
                    required_string(&empty, "slug").unwrap_err()
                ));
            }
            return Err("decode Codex client model catalog: json: cannot unmarshal into Go struct field codexClientModelsPayload.models of type map[string]interface {}".to_string());
        };
        let slug = required_string(model, "slug")
            .map_err(|e| format!("Codex client model catalog models[{i}]: {e}"))?;
        if !seen.insert(slug.clone()) {
            return Err(format!(
                "Codex client model catalog contains duplicate slug {slug:?}"
            ));
        }
        validate_codex_client_model(model)
            .map_err(|e| format!("Codex client model catalog model {slug:?}: {e}"))?;
    }
    if !seen.contains("gpt-5.5") {
        return Err("Codex client model catalog is missing default template \"gpt-5.5\"".into());
    }
    Ok(())
}

fn validate_codex_client_model(model: &Map<String, Value>) -> Result<(), String> {
    for field in [
        "display_name",
        "description",
        "base_instructions",
        "minimal_client_version",
        "visibility",
        "default_reasoning_level",
    ] {
        required_string(model, field)?;
    }

    let context_window = required_integer(model, "context_window", true)?;
    let max_context_window = required_integer(model, "max_context_window", true)?;
    if context_window > max_context_window {
        return Err(format!(
            "context_window {context_window} exceeds max_context_window {max_context_window}"
        ));
    }
    required_integer(model, "priority", false)?;

    let levels = match model.get("supported_reasoning_levels") {
        Some(Value::Array(levels)) if !levels.is_empty() => levels,
        _ => return Err("field \"supported_reasoning_levels\" must be a non-empty array".into()),
    };
    let mut seen_levels: HashSet<String> = HashSet::with_capacity(levels.len());
    for (i, raw_level) in levels.iter().enumerate() {
        let Value::Object(level) = raw_level else {
            return Err(format!(
                "field \"supported_reasoning_levels\" entry {i} must be an object"
            ));
        };
        let effort = required_string(level, "effort")
            .map_err(|e| format!("field \"supported_reasoning_levels\" entry {i}: {e}"))?;
        if !seen_levels.insert(effort.clone()) {
            return Err(format!(
                "field \"supported_reasoning_levels\" contains duplicate effort {effort:?}"
            ));
        }
    }
    let default_level = required_string(model, "default_reasoning_level").unwrap_or_default();
    if !seen_levels.contains(&default_level) {
        return Err(format!(
            "default_reasoning_level {default_level:?} is not listed in supported_reasoning_levels"
        ));
    }
    Ok(())
}

fn required_string(model: &Map<String, Value>, field: &str) -> Result<String, String> {
    match model.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(format!("field {field:?} must be a non-empty string")),
    }
}

fn required_integer(
    model: &Map<String, Value>,
    field: &str,
    positive: bool,
) -> Result<i64, String> {
    let value = match model.get(field) {
        Some(Value::Number(n)) => n.to_string().parse::<f64>().ok(),
        _ => None,
    };
    let Some(value) = value.filter(|v| v.is_finite() && v.trunc() == *v && *v <= i64::MAX as f64)
    else {
        return Err(format!("field {field:?} must be an integer"));
    };
    if positive && value <= 0.0 {
        return Err(format!("field {field:?} must be positive"));
    }
    if !positive && value < 0.0 {
        return Err(format!("field {field:?} must not be negative"));
    }
    Ok(value as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_is_valid_and_loaded() {
        let (data, revision) = get_codex_client_models_snapshot();
        assert!(!data.is_empty() && revision >= 1);
        assert!(validate_codex_client_models_json(&data).is_ok());
        assert_eq!(
            load_codex_client_models_from_bytes(&data, "same"),
            Ok(false)
        );
    }

    #[test]
    fn validation_reports_the_failing_field() {
        let err =
            validate_codex_client_models_json(br#"{"models":[{"slug":"gpt-5.5"}]}"#).unwrap_err();
        assert_eq!(
            err,
            "Codex client model catalog model \"gpt-5.5\": field \"display_name\" must be a non-empty string"
        );
        assert_eq!(
            validate_codex_client_models_json(b"{}").unwrap_err(),
            "Codex client model catalog has no models"
        );
    }
}
