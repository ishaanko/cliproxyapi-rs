//! Model list payloads for `GET /v1/models` and `GET /v1beta/models[/{name}]` (Go:
//! `unifiedModelsHandler`, `OpenAIModels`, `ClaudeModels`, `GeminiModels`, `GeminiGetHandler`,
//! internal/client/claude/models and internal/client/grokbuild).
//!
//! Pure functions over a [`ModelRegistry`]: the HTTP layer picks a format with
//! [`route_models_request`], calls the matching builder and serializes the returned JSON value.
//! Map keys are emitted in alphabetical order, as Go serializes its maps.
//!
//! Not covered: the Codex client catalog (`/v1/models?client_version=`) and Home-mode lists.

use cpa_core::registry::ModelRegistry;
use serde_json::{Map, Value, json};

/// Which payload `/v1/models` should produce (Go: the branches of `unifiedModelsHandler`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsRoute {
    /// `User-Agent` contains `grok-shell`.
    Grok,
    /// The query carries `client_version`: the Codex client catalog (not built here).
    CodexClient { client_version: String },
    /// `Anthropic-Version` header or a `claude-cli` User-Agent.
    Claude,
    OpenAi,
}

/// Go `isAnthropicModelsRequest`.
pub fn is_anthropic_models_request(anthropic_version: Option<&str>, user_agent: Option<&str>) -> bool {
    anthropic_version.is_some_and(|v| !v.is_empty()) || user_agent.is_some_and(|ua| ua.starts_with("claude-cli"))
}

/// Chooses the `/v1/models` format. `client_version` is the query value when the parameter is
/// present at all (even empty).
pub fn route_models_request(
    client_version: Option<&str>,
    anthropic_version: Option<&str>,
    user_agent: Option<&str>,
) -> ModelsRoute {
    if user_agent.is_some_and(|ua| cpa_misc::grokbuild::is_grok_shell_user_agent(ua)) {
        return ModelsRoute::Grok;
    }
    if let Some(version) = client_version {
        return ModelsRoute::CodexClient { client_version: version.to_string() };
    }
    if is_anthropic_models_request(anthropic_version, user_agent) {
        ModelsRoute::Claude
    } else {
        ModelsRoute::OpenAi
    }
}

/// Re-inserts keys alphabetically (Go map serialization order).
fn sorted(map: Map<String, Value>) -> Map<String, Value> {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().collect()
}

/// `OpenAIModels`: `{"data":[{created?, id, object, owned_by?}], "object":"list"}`, sorted by id.
pub fn openai_models_response(registry: &ModelRegistry) -> Value {
    let data: Vec<Value> = registry
        .get_available_models("openai")
        .into_iter()
        .map(|model| {
            let mut out = Map::new();
            for key in ["created", "id", "object", "owned_by"] {
                if let Some(value) = model.get(key) {
                    out.insert(key.to_string(), value.clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    json!({"data": data, "object": "list"})
}

const CLAUDE_DD_MODEL_PREFIX: &str = "claude-fable-5-dd-";

/// Go `EnsureClaudeModelIDPrefix`: non-`claude-` ids become `claude-fable-5-dd-` plus the
/// reversed id so Anthropic clients accept them.
pub fn ensure_claude_model_id_prefix(id: &str) -> String {
    if id.is_empty() || id.starts_with("claude-") {
        return id.to_string();
    }
    format!("{CLAUDE_DD_MODEL_PREFIX}{}", id.chars().rev().collect::<String>())
}

/// Go `ResolveClaudeModelIDPrefix`: reverses [`ensure_claude_model_id_prefix`] for routing,
/// preserving a trailing `(thinking)` suffix.
pub fn resolve_claude_model_id_prefix(id: &str) -> String {
    if id.is_empty() {
        return id.to_string();
    }
    let (base, suffix) = match id.rfind('(') {
        Some(open) if id.ends_with(')') => (&id[..open], Some(&id[open + 1..id.len() - 1])),
        _ => (id, None),
    };
    let Some(encoded) = base.strip_prefix(CLAUDE_DD_MODEL_PREFIX).filter(|e| !e.is_empty()) else {
        return id.to_string();
    };
    let resolved: String = encoded.chars().rev().collect();
    match suffix {
        Some(suffix) => format!("{resolved}({suffix})"),
        None => resolved,
    }
}

/// Go `claudemodels.BuildResponse`: sorts by display name then id and (unless
/// `disable_cloaking`) rewrites ids with [`ensure_claude_model_id_prefix`].
pub fn build_claude_models_response(available: Vec<Map<String, Value>>, disable_cloaking: bool) -> Value {
    let mut models: Vec<Map<String, Value>> = available
        .into_iter()
        .map(|mut model| {
            if !disable_cloaking
                && let Some(id) = model.get("id").and_then(Value::as_str)
            {
                let prefixed = ensure_claude_model_id_prefix(id);
                model.insert("id".into(), Value::String(prefixed));
            }
            model
        })
        .collect();
    let text = |m: &Map<String, Value>, key: &str| m.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    models.sort_by(|a, b| {
        text(a, "display_name")
            .cmp(&text(b, "display_name"))
            .then_with(|| text(a, "id").cmp(&text(b, "id")))
    });
    let first_id = models.first().map(|m| text(m, "id")).unwrap_or_default();
    let last_id = models.last().map(|m| text(m, "id")).unwrap_or_default();
    json!({
        "data": models,
        "first_id": first_id,
        "has_more": false,
        "last_id": last_id,
    })
}

/// `ClaudeModels`: the Anthropic-format list; `disable_cloaking` is
/// `claude-code.disable-cloaking-model-list`.
pub fn claude_models_response(registry: &ModelRegistry, disable_cloaking: bool) -> Value {
    build_claude_models_response(registry.get_available_models("claude"), disable_cloaking)
}

/// Go `grokbuild.BuildResponse` over the registry's available models.
pub fn grok_models_response(registry: &ModelRegistry) -> Value {
    let models: Vec<cpa_misc::grokbuild::ModelInfo> = registry
        .get_available_model_infos()
        .iter()
        .map(|info| cpa_misc::grokbuild::ModelInfo {
            id: info.id.clone(),
            display_name: info.display_name.clone(),
            context_length: info.context_length,
            reasoning_levels: info.thinking.iter().flat_map(|t| t.levels.iter().cloned()).collect(),
        })
        .collect();
    serde_json::to_value(cpa_misc::grokbuild::build_response(&models)).unwrap_or(Value::Null)
}

/// Go `GeminiModels`' per-model normalization: `models/` name prefix, display name and
/// description defaulting to the name, and `generateContent` as the default method list.
fn normalize_gemini_model(mut model: Map<String, Value>) -> Map<String, Value> {
    let name = model.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string);
    if let Some(name) = name {
        if !name.starts_with("models/") {
            model.insert("name".into(), Value::String(format!("models/{name}")));
        }
        for key in ["displayName", "description"] {
            if model.get(key).and_then(Value::as_str).is_none_or(str::is_empty) {
                model.insert(key.into(), Value::String(name.clone()));
            }
        }
    }
    if !model.contains_key("supportedGenerationMethods") {
        model.insert("supportedGenerationMethods".into(), json!(["generateContent"]));
    }
    sorted(model)
}

/// `GeminiModels` (`GET /v1beta/models`): `{"models":[...]}`.
pub fn gemini_models_response(registry: &ModelRegistry) -> Value {
    let models: Vec<Value> = registry
        .get_available_models("gemini")
        .into_iter()
        .map(|m| Value::Object(normalize_gemini_model(m)))
        .collect();
    json!({"models": models})
}

/// `GeminiGetHandler` (`GET /v1beta/models/{action}`): the registry entry whose name matches
/// `action` with or without the `models/` prefix; `None` means 404.
pub fn gemini_model_response(registry: &ModelRegistry, action: &str) -> Option<Value> {
    let action = action.strip_prefix('/').unwrap_or(action);
    let prefixed = format!("models/{action}");
    registry
        .get_available_models("gemini")
        .into_iter()
        .find(|m| {
            let name = m.get("name").and_then(Value::as_str).unwrap_or("");
            name == action || name == prefixed
        })
        .map(|mut m| {
            if let Some(name) = m.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && !n.starts_with("models/"))
            {
                let prefixed = format!("models/{name}");
                m.insert("name".into(), Value::String(prefixed));
            }
            Value::Object(m)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_follow_unified_models_handler() {
        assert_eq!(route_models_request(None, None, Some("Grok-Shell/1")), ModelsRoute::Grok);
        assert_eq!(
            route_models_request(Some(""), Some("2023"), None),
            ModelsRoute::CodexClient { client_version: String::new() }
        );
        assert_eq!(route_models_request(None, Some("2023-06-01"), None), ModelsRoute::Claude);
        assert_eq!(route_models_request(None, None, Some("claude-cli/1.0")), ModelsRoute::Claude);
        assert_eq!(route_models_request(None, None, Some("curl")), ModelsRoute::OpenAi);
    }

    #[test]
    fn claude_id_prefix_round_trips_like_go() {
        for (id, want) in [
            ("claude-sonnet-4-6", "claude-sonnet-4-6"),
            ("gpt-4o", "claude-fable-5-dd-o4-tpg"),
            ("Claude-Opus-4", "claude-fable-5-dd-4-supO-edualC"),
            ("", ""),
        ] {
            assert_eq!(ensure_claude_model_id_prefix(id), want);
        }
        assert_eq!(resolve_claude_model_id_prefix("claude-fable-5-dd-o4-tpg(high)"), "gpt-4o(high)");
        assert_eq!(resolve_claude_model_id_prefix("claude-fable-5-dd-"), "claude-fable-5-dd-");
    }

    #[test]
    fn claude_list_sorts_by_display_name_then_id() {
        let model = |id: &str, name: &str| {
            let mut m = Map::new();
            m.insert("id".into(), json!(id));
            m.insert("display_name".into(), json!(name));
            m
        };
        let out = build_claude_models_response(
            vec![model("claude-z", "Zebra"), model("gpt-4o", "Alpha"), model("claude-c", "Alpha"), model("claude-b", "Beta")],
            false,
        );
        let ids: Vec<&str> = out["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["claude-c", "claude-fable-5-dd-o4-tpg", "claude-b", "claude-z"]);
        assert_eq!(out["first_id"], "claude-c");
        assert_eq!(out["last_id"], "claude-z");
        assert_eq!(out["has_more"], false);
    }

    #[test]
    fn claude_list_edge_cases_from_go_tests() {
        let model = |id: &str, name: &str| {
            let mut m = Map::new();
            m.insert("id".into(), json!(id));
            m.insert("display_name".into(), json!(name));
            m
        };
        // Cloaking disabled keeps ids; the extra fields of an entry survive the rewrite.
        let out = build_claude_models_response(vec![model("gpt-4o", "GPT-4o")], true);
        assert_eq!(out["data"][0]["id"], "gpt-4o");
        assert_eq!((&out["first_id"], &out["last_id"]), (&json!("gpt-4o"), &json!("gpt-4o")));
        let mut with_tokens = model("claude-z", "Zebra");
        with_tokens.insert("max_tokens".into(), json!(64000));
        assert_eq!(build_claude_models_response(vec![with_tokens], false)["data"][0]["max_tokens"], 64000);
        // Empty input yields empty ids.
        let out = build_claude_models_response(Vec::new(), false);
        assert_eq!(out["data"], json!([]));
        assert_eq!((&out["first_id"], &out["last_id"]), (&json!(""), &json!("")));
    }

    #[test]
    fn claude_id_prefix_cases_from_go_tests() {
        for (id, want) in [
            ("my-claude-custom", "claude-fable-5-dd-motsuc-edualc-ym"),
            ("gemini-2.5-pro", "claude-fable-5-dd-orp-5.2-inimeg"),
        ] {
            assert_eq!(ensure_claude_model_id_prefix(id), want);
        }
        for (id, want) in [
            ("", ""),
            ("claude-sonnet-4-6", "claude-sonnet-4-6"),
            ("gpt-4o", "gpt-4o"),
            ("claude-fable-5-dd-o4-tpg", "gpt-4o"),
            ("claude-fable-5-dd-orp-5.2-inimeg", "gemini-2.5-pro"),
        ] {
            assert_eq!(resolve_claude_model_id_prefix(id), want, "{id}");
        }
        let round_trip = ensure_claude_model_id_prefix("custom-model-x");
        assert_eq!(resolve_claude_model_id_prefix(&round_trip), "custom-model-x");
    }
}
