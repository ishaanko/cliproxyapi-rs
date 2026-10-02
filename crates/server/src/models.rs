//! Model list endpoints: `/v1/models` (unified OpenAI / Claude / Codex-client / Grok shapes) and
//! the Gemini `/v1beta/models` pair (Go: server_routes.go unified handler, the per-dialect
//! `*Models` handlers, claude/models, grokbuild).

use axum::http::HeaderMap;
use cpa_config::Config;
use cpa_core::registry::global_registry;
use cpa_runtime::conductor::Manager;
use serde_json::{Map, Value, json};

use crate::error::error_response_json;
use crate::reply::Reply;

/// Source of the model lists. The default reads the process-wide registry; the Codex client
/// catalog (a template-driven projection of the same data) is supplied separately so the service
/// wiring can plug the full builder in.
pub trait ModelCatalog: Send + Sync {
    /// `registry.GetAvailableModels(handlerType)` for `openai`, `claude` and `gemini`.
    fn available_models(&self, handler_type: &str) -> Vec<Map<String, Value>>;

    /// Body for `GET /v1/models?client_version=...` (compact JSON), or an error message.
    fn codex_client_models(&self, client_version: &str, cfg: &Config, manager: &Manager) -> Result<Vec<u8>, String>;

    /// Entries for the Grok Shell list.
    fn grok_models(&self) -> Vec<GrokModel>;
}

/// Input of the Grok Shell response (`grokbuild.ModelInfo`).
#[derive(Debug, Clone, Default)]
pub struct GrokModel {
    pub id: String,
    pub display_name: String,
    pub context_length: i64,
    pub reasoning_levels: Vec<String>,
}

/// Registry-backed catalog.
pub struct RegistryModelCatalog;

impl ModelCatalog for RegistryModelCatalog {
    fn available_models(&self, handler_type: &str) -> Vec<Map<String, Value>> {
        global_registry().get_available_models(handler_type)
    }

    fn codex_client_models(&self, client_version: &str, cfg: &Config, manager: &Manager) -> Result<Vec<u8>, String> {
        crate::codex_models::build_client_models_body(client_version, cfg, manager)
    }

    fn grok_models(&self) -> Vec<GrokModel> {
        global_registry()
            .get_available_model_infos()
            .into_iter()
            .map(|info| GrokModel {
                id: info.id.clone(),
                display_name: info.display_name.clone(),
                context_length: info.context_length,
                reasoning_levels: info.thinking.as_ref().map(|t| t.levels.clone()).unwrap_or_default(),
            })
            .collect()
    }
}

/// `WriteModelListResponse`: 200 with `application/json; charset=utf-8`.
fn model_list_reply(value: &Value) -> Reply {
    Reply::json_value(200, value)
}

fn model_list_reply_raw(body: Vec<u8>) -> Reply {
    Reply::json(200, body)
}

/// `isAnthropicModelsRequest`.
pub fn is_anthropic_models_request(headers: &HeaderMap) -> bool {
    if headers.get("anthropic-version").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.starts_with("claude-cli"))
}

/// `grokbuild.IsGrokShellUserAgent`.
fn is_grok_shell_user_agent(headers: &HeaderMap) -> bool {
    headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.to_lowercase().contains("grok-shell"))
}

/// `GET /v1/models` dispatch (`unifiedModelsHandler`).
pub fn unified_models(
    catalog: &dyn ModelCatalog,
    cfg: &Config,
    manager: &Manager,
    headers: &HeaderMap,
    query: &[(String, String)],
) -> Reply {
    if is_grok_shell_user_agent(headers) {
        return model_list_reply(&build_grok_response(&catalog.grok_models()));
    }
    if let Some((_, client_version)) = query.iter().find(|(k, _)| k == "client_version") {
        return match catalog.codex_client_models(client_version, cfg, manager) {
            Ok(body) => model_list_reply_raw(body),
            Err(err) => Reply::json(
                500,
                error_response_json(&format!("Failed to encode model list: {err}"), "server_error"),
            ),
        };
    }
    if is_anthropic_models_request(headers) {
        claude_models(catalog, cfg)
    } else {
        openai_models(catalog)
    }
}

/// `OpenAIModels` (without the client_version branch): only id/object/created/owned_by survive.
pub fn openai_models(catalog: &dyn ModelCatalog) -> Reply {
    let filtered: Vec<Value> = catalog
        .available_models("openai")
        .iter()
        .map(|model| {
            let mut out = Map::new();
            out.insert("id".into(), model.get("id").cloned().unwrap_or(Value::Null));
            out.insert("object".into(), model.get("object").cloned().unwrap_or(Value::Null));
            for key in ["created", "owned_by"] {
                if let Some(v) = model.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    model_list_reply(&json!({"object": "list", "data": filtered}))
}

const CLAUDE_DD_MODEL_PREFIX: &str = "claude-fable-5-dd-";

/// `EnsureClaudeModelIDPrefix`: non-`claude-` IDs are cloaked as prefix + reversed ID.
pub fn ensure_claude_model_id_prefix(id: &str) -> String {
    if id.is_empty() || id.starts_with("claude-") {
        return id.to_string();
    }
    format!("{CLAUDE_DD_MODEL_PREFIX}{}", id.chars().rev().collect::<String>())
}

/// `ResolveClaudeModelIDPrefix`: reverses the cloaking for request routing, keeping a
/// `(thinking)` suffix.
pub fn resolve_claude_model_id_prefix(id: &str) -> String {
    if id.is_empty() {
        return id.to_string();
    }
    let (base, suffix) = match id.rfind('(') {
        Some(open) if id.ends_with(')') => (&id[..open], Some(&id[open + 1..id.len() - 1])),
        _ => (id, None),
    };
    let Some(encoded) = base.strip_prefix(CLAUDE_DD_MODEL_PREFIX) else {
        return id.to_string();
    };
    if encoded.is_empty() {
        return id.to_string();
    }
    let resolved: String = encoded.chars().rev().collect();
    match suffix {
        Some(s) => format!("{resolved}({s})"),
        None => resolved,
    }
}

/// `claudemodels.BuildResponse`.
pub fn build_claude_response(available: &[Map<String, Value>], disable_cloaking: bool) -> Value {
    let mut models: Vec<Map<String, Value>> = available.to_vec();
    for model in &mut models {
        if !disable_cloaking
            && let Some(Value::String(id)) = model.get("id")
        {
            let cloaked = ensure_claude_model_id_prefix(id);
            model.insert("id".into(), Value::String(cloaked));
        }
    }
    let text = |m: &Map<String, Value>, key: &str| m.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    models.sort_by(|a, b| {
        text(a, "display_name")
            .cmp(&text(b, "display_name"))
            .then_with(|| text(a, "id").cmp(&text(b, "id")))
    });
    let first = models.first().map(|m| text(m, "id")).unwrap_or_default();
    let last = models.last().map(|m| text(m, "id")).unwrap_or_default();
    json!({
        "data": models.into_iter().map(Value::Object).collect::<Vec<_>>(),
        "has_more": false,
        "first_id": first,
        "last_id": last,
    })
}

/// `ClaudeModels`.
pub fn claude_models(catalog: &dyn ModelCatalog, cfg: &Config) -> Reply {
    let disable_cloaking = cfg.claude_code.disable_cloaking_model_list;
    model_list_reply(&build_claude_response(&catalog.available_models("claude"), disable_cloaking))
}

/// `GeminiModels`: names get the `models/` prefix, display name and description default to the
/// name, `supportedGenerationMethods` defaults to `["generateContent"]`.
pub fn gemini_models(catalog: &dyn ModelCatalog) -> Reply {
    let normalized: Vec<Value> = catalog
        .available_models("gemini")
        .into_iter()
        .map(|mut model| {
            if let Some(Value::String(name)) = model.get("name").cloned()
                && !name.is_empty()
            {
                if !name.starts_with("models/") {
                    model.insert("name".into(), Value::String(format!("models/{name}")));
                }
                for key in ["displayName", "description"] {
                    let empty = model.get(key).and_then(Value::as_str).is_none_or(str::is_empty);
                    if empty {
                        model.insert(key.into(), Value::String(name.clone()));
                    }
                }
            }
            if !model.contains_key("supportedGenerationMethods") {
                model.insert("supportedGenerationMethods".into(), json!(["generateContent"]));
            }
            Value::Object(model)
        })
        .collect();
    model_list_reply(&json!({"models": normalized}))
}

/// `GeminiGetHandler`: one model by `name` (with or without `models/`), else a 404 JSON error.
pub fn gemini_get_model(catalog: &dyn ModelCatalog, action: &str) -> Reply {
    let action = action.strip_prefix('/').unwrap_or(action);
    let prefixed = format!("models/{action}");
    for mut model in catalog.available_models("gemini") {
        let name = model.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name == action || name == prefixed {
            if !name.is_empty() && !name.starts_with("models/") {
                model.insert("name".into(), Value::String(format!("models/{name}")));
            }
            return Reply::json_value(200, &Value::Object(model));
        }
    }
    Reply::json(404, error_response_json("Not Found", "not_found"))
}

/// `grokbuild.BuildResponse`.
fn build_grok_response(models: &[GrokModel]) -> Value {
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            let name = if m.display_name.is_empty() { &m.id } else { &m.display_name };
            let efforts: Vec<Value> = m
                .reasoning_levels
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(|l| json!({"value": l}))
                .collect();
            let mut entry = Map::new();
            entry.insert("id".into(), json!(m.id));
            entry.insert("model".into(), json!(m.id));
            entry.insert("name".into(), json!(name));
            if m.context_length > 0 {
                entry.insert("context_window".into(), json!(m.context_length));
            }
            entry.insert("api_backend".into(), json!("responses"));
            entry.insert("supported_in_api".into(), json!(true));
            if !efforts.is_empty() {
                entry.insert("reasoning_efforts".into(), Value::Array(efforts));
            }
            Value::Object(entry)
        })
        .collect();
    json!({"object": "list", "data": data})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn claude_cloaking_round_trips() {
        let cloaked = ensure_claude_model_id_prefix("gpt-5");
        assert_eq!(cloaked, "claude-fable-5-dd-5-tpg");
        assert_eq!(resolve_claude_model_id_prefix(&cloaked), "gpt-5");
        assert_eq!(resolve_claude_model_id_prefix(&format!("{cloaked}(high)")), "gpt-5(high)");
        assert_eq!(ensure_claude_model_id_prefix("claude-opus"), "claude-opus");
        assert_eq!(resolve_claude_model_id_prefix("claude-opus"), "claude-opus");
    }

    #[test]
    fn claude_response_sorted_and_cloaked() {
        let models = vec![
            map(json!({"id": "zeta", "display_name": "B"})),
            map(json!({"id": "claude-a", "display_name": "A"})),
        ];
        let out = build_claude_response(&models, false);
        assert_eq!(out["first_id"], "claude-a");
        assert_eq!(out["last_id"], "claude-fable-5-dd-atez");
        assert_eq!(out["has_more"], false);
        let plain = build_claude_response(&models, true);
        assert_eq!(plain["last_id"], "zeta");
    }
}
