//! Codex client model catalog for `GET /v1/models?client_version=...`
//! (Go: internal/client/codex/models).
//!
//! Simplified projection: every available model that has a catalog template (matched on the
//! metadata model id) is emitted with its public id as `slug`, display name, description and
//! base instructions overrides, `apply_patch_tool_type`, and the multi-agent flag. The Go builder's
//! per-provider modality/thinking intersection and the Devin/search-tool rules are not ported.

use cpa_config::Config;
use cpa_core::registry::get_codex_client_models_json;
use cpa_core::registry::global_registry;
use cpa_runtime::conductor::Manager;
use serde_json::{Map, Value, json};

fn str_field(map: &Map<String, Value>, key: &str) -> String {
    map.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Model id used to look up the template: registry metadata id, else the part after the last
/// provider prefix.
fn metadata_model_id(id: &str) -> String {
    let id = id.trim();
    if let Some(info) = cpa_core::registry::lookup_model_info(id, None) {
        let meta = info.metadata_model_id.trim();
        if !meta.is_empty() {
            return meta.to_string();
        }
    }
    match id.find('/') {
        Some(idx) => id[idx + 1..].trim().to_string(),
        None => id.to_string(),
    }
}

/// Compact JSON catalog body, HTML characters left unescaped like Go's `MarshalCompact`.
pub fn build_client_models_body(client_version: &str, cfg: &Config, manager: &Manager) -> Result<Vec<u8>, String> {
    let raw = get_codex_client_models_json();
    let templates: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let templates: Vec<&Value> = templates
        .get("models")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();

    let available = global_registry().get_available_models("openai");
    let mut out: Vec<Value> = Vec::new();
    for model in &available {
        let id = str_field(model, "id");
        if id.is_empty() {
            continue;
        }
        let meta = metadata_model_id(&id);
        let Some(template) = templates
            .iter()
            .find(|t| t.get("slug").and_then(Value::as_str).map(str::trim) == Some(meta.as_str()))
        else {
            continue;
        };
        let mut entry = template.as_object().cloned().unwrap_or_default();
        entry.insert("slug".into(), Value::String(id.clone()));
        for (src, dst) in [("display_name", "display_name"), ("description", "description"), ("base_instructions", "base_instructions")] {
            let v = str_field(model, src);
            if !v.is_empty() {
                entry.insert(dst.into(), Value::String(v));
            }
        }
        if cfg.client.codex.optimize_multi_agent_v2 {
            entry.insert("multi_agent_version".into(), json!("v2"));
        }
        entry.insert("apply_patch_tool_type".into(), Value::Null);
        if cfg.client.codex.enable_apply_patch {
            let providers = global_registry().get_model_providers(&id);
            if manager.supports_apply_patch_for_providers(&providers, &id) {
                entry.insert("apply_patch_tool_type".into(), json!("freeform"));
            }
        }
        if client_version != "cpa" {
            entry.remove("cpa_capabilities");
        }
        out.push(Value::Object(entry));
    }
    serde_json::to_vec(&json!({ "models": out })).map_err(|e| e.to_string())
}
