//! Model lists in Home mode (Go: the `handleHome*Models` family and `decodeHomeModels` in
//! internal/api/server_routes.go).
//!
//! With Home enabled the model catalog lives in Home, so every list endpoint asks Home for it
//! (`GET {"type":"models",...}` through [`cpa_home::Client::get_models`]), decodes the per-provider
//! sections into [`HomeModelEntry`] values and formats them for the requesting client dialect.

use std::collections::HashMap;

use chrono::DateTime;
use cpa_config::{Config, resolve_oauth_model_setting};
use cpa_core::registry::{
    DEFAULT_CLAUDE_MAX_INPUT_TOKENS, DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS, NativeCapabilities, NativeCapabilityRoute,
    ThinkingSupport, resolve_responses_web_search_capability,
};
use cpa_runtime::conductor::Manager;
use cpa_runtime::service::listing::build_claude_models_response;
use serde_json::{Map, Value, json};

use crate::codex_models::{CatalogContext, build_models, marshal_compact};
use crate::error::error_response_json;
use crate::reply::Reply;
use crate::req::ReqInfo;

type Entry = Map<String, Value>;

/// One model as Home reported it, merged across provider sections (Go: `homeModelEntry`).
#[derive(Debug, Clone, Default)]
pub struct HomeModelEntry {
    pub id: String,
    pub created: i64,
    pub owned_by: String,
    pub display_name: String,
    pub context_length: i64,
    pub max_context_length: i64,
    pub max_completion_tokens: i64,
    pub thinking: Option<ThinkingSupport>,
    /// Lower-cased section names the model appeared under, in first-seen order.
    pub providers: Vec<String>,
    /// One route per section occurrence, including sections with a blank name.
    pub native_capability_routes: Vec<NativeCapabilityRoute>,
}

// ------------------------------------------------------------------ decoding

/// `homeModelsErrorType`: `error.type` of an error envelope, trimmed; empty when absent.
fn models_error_field(raw: &[u8], field: &str) -> Option<String> {
    let top: Value = serde_json::from_slice(raw).ok()?;
    let error = top.as_object()?.get("error")?.as_object()?;
    Some(error.get(field).and_then(Value::as_str).unwrap_or("").trim().to_string())
}

/// `homeModelsAuthStatus`: the HTTP status for an error envelope (401 for credential problems,
/// 502 otherwise); `None` when the payload is model data.
pub fn models_auth_status(raw: &[u8]) -> Option<u16> {
    let error_type = models_error_field(raw, "type").unwrap_or_default();
    match error_type.as_str() {
        "" => None,
        "no_credentials" | "invalid_credential" => Some(401),
        _ => Some(502),
    }
}

/// `homeModelsErrorMessage`.
pub fn models_error_message(raw: &[u8]) -> String {
    models_error_field(raw, "message")
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "home models request failed".to_string())
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Go decodes into `map[string][]map[string]any`; mismatches are reported like its errors.
fn parse_sections(raw: &[u8]) -> Result<Vec<(String, Vec<Entry>)>, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| format!("parse home models payload: {e}"))?;
    let object = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Object(object) => object,
        other => {
            return Err(format!(
                "parse home models payload: json: cannot unmarshal {} into Go value of type map[string][]map[string]interface {{}}",
                kind_of(&other)
            ));
        }
    };
    let mut sections = Vec::with_capacity(object.len());
    for (name, items) in object {
        let items = match items {
            Value::Null => Vec::new(),
            Value::Array(items) => items,
            other => {
                return Err(format!(
                    "parse home models payload: json: cannot unmarshal {} into Go value of type []map[string]interface {{}}",
                    kind_of(&other)
                ));
            }
        };
        let mut models = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Value::Object(model) => models.push(model),
                Value::Null => models.push(Entry::new()),
                other => {
                    return Err(format!(
                        "parse home models payload: json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
                        kind_of(&other)
                    ));
                }
            }
        }
        sections.push((name, models));
    }
    Ok(sections)
}

fn trimmed_str(model: &Entry, key: &str) -> String {
    model.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// `homeModelInt64Value`: the first listed key holding a number (or a numeric string).
fn int_value(model: &Entry, keys: &[&str]) -> i64 {
    for key in keys {
        match model.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().unwrap_or_else(|| n.as_f64().map_or(0, |f| f as i64)),
            Some(Value::String(s)) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return n;
                }
            }
            _ => {}
        }
    }
    0
}

/// `homeModelNativeCapabilities`.
fn native_capabilities(model: &Entry) -> Option<NativeCapabilities> {
    let raw = model.get("native_capabilities")?.as_object()?;
    Some(NativeCapabilities { web_search: raw.get("web_search").and_then(Value::as_bool) })
}

/// `homeModelThinkingSupport`: a malformed object reads as absent.
fn thinking_support(model: &Entry) -> Option<ThinkingSupport> {
    match model.get("thinking") {
        None | Some(Value::Null) => None,
        Some(raw) => serde_json::from_value(raw.clone()).ok(),
    }
}

fn append_unique_provider(providers: &mut Vec<String>, provider: &str) {
    if !provider.is_empty() && !providers.iter().any(|p| p == provider) {
        providers.push(provider.to_string());
    }
}

/// `decodeHomeModels`: merges the per-provider sections into one entry per model id, sorted by id.
/// Sections are visited in payload order (Go visits them in map order); the first section that
/// mentions an id supplies its metadata.
pub fn decode_home_models(raw: &[u8]) -> Result<Vec<HomeModelEntry>, String> {
    if raw.is_empty() {
        return Err("home models payload is empty".into());
    }
    let sections = parse_sections(raw)?;
    if sections.is_empty() {
        return Err("home models payload has no sections".into());
    }

    let mut index_by_id: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<HomeModelEntry> = Vec::with_capacity(256);
    for (section, models) in sections {
        let provider = section.trim().to_lowercase();
        for model in models {
            let mut id = trimmed_str(&model, "id");
            if id.is_empty() {
                let name = trimmed_str(&model, "name");
                id = name.strip_prefix("models/").unwrap_or(&name).to_string();
            }
            if id.is_empty() {
                continue;
            }
            let route = NativeCapabilityRoute { provider: provider.clone(), native_capabilities: native_capabilities(&model) };
            if let Some(&index) = index_by_id.get(&id) {
                append_unique_provider(&mut out[index].providers, &provider);
                out[index].native_capability_routes.push(route);
                continue;
            }

            let mut display_name = trimmed_str(&model, "display_name");
            if display_name.is_empty() {
                display_name = trimmed_str(&model, "displayName");
            }
            let mut providers = Vec::new();
            append_unique_provider(&mut providers, &provider);
            index_by_id.insert(id.clone(), out.len());
            out.push(HomeModelEntry {
                id,
                created: int_value(&model, &["created"]),
                owned_by: trimmed_str(&model, "owned_by"),
                display_name,
                context_length: int_value(&model, &["context_length", "contextLength", "inputTokenLimit", "max_input_tokens"]),
                max_context_length: int_value(&model, &["max_context_length", "maxContextLength"]),
                max_completion_tokens: int_value(
                    &model,
                    &["max_completion_tokens", "maxCompletionTokens", "outputTokenLimit", "max_tokens"],
                ),
                thinking: thinking_support(&model),
                providers,
                native_capability_routes: vec![route],
            });
        }
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    if out.is_empty() {
        return Err("home models payload contains no models".into());
    }
    Ok(out)
}

// ------------------------------------------------------------------ formatting

fn has_provider(entry: &HomeModelEntry, name: &str) -> bool {
    entry.providers.iter().any(|p| p.to_lowercase() == name)
}

/// `formatHomeCodexModel`: the generic model object fed to the Codex catalog builder.
pub fn format_codex_model(entry: &HomeModelEntry) -> Entry {
    let mut model = Entry::new();
    model.insert("id".into(), json!(entry.id));
    model.insert("object".into(), json!("model"));
    if entry.created > 0 {
        model.insert("created".into(), json!(entry.created));
    }
    if !entry.owned_by.is_empty() {
        model.insert("owned_by".into(), json!(entry.owned_by));
    }
    if has_provider(entry, "devin") {
        model.insert("type".into(), json!("devin"));
    }
    if !entry.display_name.is_empty() {
        model.insert("display_name".into(), json!(entry.display_name));
        model.insert("description".into(), json!(entry.display_name));
    }
    for (key, value) in [
        ("context_length", entry.context_length),
        ("max_context_length", entry.max_context_length),
        ("max_completion_tokens", entry.max_completion_tokens),
    ] {
        if value > 0 {
            model.insert(key.into(), json!(value));
        }
    }
    if let Some(thinking) = entry.thinking.as_ref().and_then(|t| serde_json::to_value(t).ok()) {
        model.insert("thinking".into(), thinking);
    }
    model
}

/// `formatHomeCodexModelWithSettings`: `oauth-settings` may raise `max_context_length`, trying the
/// `codex` channel first and the other providers alphabetically.
pub fn format_codex_model_with_settings(entry: &HomeModelEntry, cfg: &Config) -> Entry {
    let mut model = format_codex_model(entry);
    if cfg.oauth_settings.is_empty() {
        return model;
    }
    let mut providers = entry.providers.clone();
    providers.sort_by(|a, b| {
        let (a_codex, b_codex) = (a.eq_ignore_ascii_case("codex"), b.eq_ignore_ascii_case("codex"));
        b_codex.cmp(&a_codex).then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    for provider in providers {
        let channel = provider.trim().to_lowercase();
        let Some(settings) = cfg.oauth_settings.get(&channel) else { continue };
        if let Some(setting) = resolve_oauth_model_setting(settings, &entry.id, "", "")
            && setting.max_context_length > 0
        {
            model.insert("max_context_length".into(), json!(setting.max_context_length));
            break;
        }
    }
    model
}

/// `formatHomeClaudeModel`.
pub fn format_claude_model(entry: &HomeModelEntry) -> Entry {
    let display_name = if entry.display_name.is_empty() { &entry.id } else { &entry.display_name };
    let max_input = if entry.context_length > 0 { entry.context_length } else { DEFAULT_CLAUDE_MAX_INPUT_TOKENS };
    let max_output = if entry.max_completion_tokens > 0 { entry.max_completion_tokens } else { DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS };
    let mut model = Entry::new();
    model.insert("id".into(), json!(entry.id));
    model.insert("object".into(), json!("model"));
    model.insert("owned_by".into(), json!(entry.owned_by));
    model.insert("type".into(), json!("model"));
    model.insert("display_name".into(), json!(display_name));
    model.insert("max_input_tokens".into(), json!(max_input));
    model.insert("max_tokens".into(), json!(max_output));
    if entry.created > 0
        && let Some(time) = DateTime::from_timestamp(entry.created, 0)
    {
        model.insert("created_at".into(), json!(time.format("%Y-%m-%dT%H:%M:%SZ").to_string()));
    }
    model
}

/// `formatHomeGeminiModel`.
pub fn format_gemini_model(entry: &HomeModelEntry) -> Value {
    let name = if entry.id.starts_with("models/") { entry.id.clone() } else { format!("models/{}", entry.id) };
    let display_name = if entry.display_name.is_empty() { &entry.id } else { &entry.display_name };
    json!({
        "name": name,
        "displayName": display_name,
        "description": display_name,
        "supportedGenerationMethods": ["generateContent"],
    })
}

/// `homeGeminiModelMatches`.
fn gemini_model_matches(entry: &HomeModelEntry, action: &str) -> bool {
    let id = entry.id.trim();
    if id.is_empty() || action.is_empty() {
        return false;
    }
    let normalized_action = action.strip_prefix("models/").unwrap_or(action);
    let normalized_id = id.strip_prefix("models/").unwrap_or(id);
    action == id || action == format!("models/{id}") || normalized_action == normalized_id
}

/// `grokModelsFromHomeEntries` + `grokbuild.BuildResponse`; Home carries no reasoning levels.
fn grok_response(entries: &[HomeModelEntry]) -> Value {
    let data: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let name = if entry.display_name.is_empty() { &entry.id } else { &entry.display_name };
            let mut model = Entry::new();
            model.insert("id".into(), json!(entry.id));
            model.insert("model".into(), json!(entry.id));
            model.insert("name".into(), json!(name));
            if entry.context_length > 0 {
                model.insert("context_window".into(), json!(entry.context_length));
            }
            model.insert("api_backend".into(), json!("responses"));
            model.insert("supported_in_api".into(), json!(true));
            Value::Object(model)
        })
        .collect();
    json!({"object": "list", "data": data})
}

// ------------------------------------------------------------------ capabilities

/// `homeWebSearchCapabilityForModel`: resolves each model from the routes Home reported for it.
pub fn web_search_capability_for_model(entries: &[HomeModelEntry]) -> impl Fn(&str) -> Option<bool> + '_ {
    let routes: HashMap<&str, &[NativeCapabilityRoute]> =
        entries.iter().map(|e| (e.id.as_str(), e.native_capability_routes.as_slice())).collect();
    move |id| routes.get(id.trim()).and_then(|r| resolve_responses_web_search_capability(r))
}

/// `homeApplyPatchCapabilityForModel`: only Home's own routing evidence counts, never the local
/// registry. A model with no or unnamed providers carries a blank provider that vetoes support.
pub fn apply_patch_capability_for_model<'a>(
    entries: &[HomeModelEntry],
    manager: &'a Manager,
) -> impl Fn(&str) -> bool + 'a {
    let mut providers_by_id: HashMap<String, Vec<String>> = HashMap::with_capacity(entries.len());
    for entry in entries {
        let providers = providers_by_id.entry(entry.id.trim().to_string()).or_default();
        providers.extend(entry.providers.iter().cloned());
        if entry.providers.is_empty() {
            providers.push(String::new());
        }
        // An unnamed route is dropped from `providers` but must still veto support.
        for route in &entry.native_capability_routes {
            if route.provider.trim().is_empty() {
                providers.push(String::new());
            }
        }
    }
    move |id| {
        let id = id.trim();
        let providers = providers_by_id.get(id).map(Vec::as_slice).unwrap_or(&[]);
        manager.supports_apply_patch_for_providers(providers, id)
    }
}

// ------------------------------------------------------------------ handlers

/// `WriteModelListResponse` bookkeeping: the 200 body doubles as the request log's API response.
fn list_reply(info: &ReqInfo, reply: Reply) -> Reply {
    info.api_log.set_api_response(&reply.body);
    reply
}

fn error_reply(status: u16, message: &str, error_type: &str) -> Reply {
    Reply::json(status, error_response_json(message, error_type))
}

/// `loadHomeModelEntries`: asks Home for the catalog; failures become the JSON error reply.
pub async fn load_entries(info: &ReqInfo) -> Result<Vec<HomeModelEntry>, Reply> {
    let Some(client) = cpa_home::kv::current() else {
        return Err(error_reply(503, "home control center unavailable", "server_error"));
    };
    let raw = client
        .get_models(&info.headers, &info.query)
        .await
        .map_err(|e| error_reply(502, &e.to_string(), "server_error"))?;
    if let Some(status) = models_auth_status(&raw) {
        return Err(error_reply(status, &models_error_message(&raw), "authentication_error"));
    }
    decode_home_models(&raw).map_err(|e| error_reply(502, &e, "server_error"))
}

/// `handleHomeModels`: the plain `/v1/models` list in OpenAI or Anthropic format.
pub async fn models_reply(cfg: &Config, info: &ReqInfo, claude: bool) -> Reply {
    let entries = match load_entries(info).await {
        Ok(entries) => entries,
        Err(reply) => return reply,
    };
    if claude {
        let models: Vec<Entry> = entries.iter().map(format_claude_model).collect();
        let payload = build_claude_models_response(models, cfg.claude_code.disable_cloaking_model_list);
        return list_reply(info, Reply::json_value(200, &payload));
    }
    let data: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut model = Entry::new();
            model.insert("id".into(), json!(entry.id));
            model.insert("object".into(), json!("model"));
            if entry.created > 0 {
                model.insert("created".into(), json!(entry.created));
            }
            if !entry.owned_by.is_empty() {
                model.insert("owned_by".into(), json!(entry.owned_by));
            }
            Value::Object(model)
        })
        .collect();
    list_reply(info, Reply::json_value(200, &json!({"object": "list", "data": data})))
}

/// `handleHomeCodexClientModels`: the Codex catalog built from Home's model ids. Template
/// metadata still comes from the embedded codex client catalog.
pub async fn codex_client_models_reply(cfg: &Config, manager: &Manager, info: &ReqInfo, client_version: &str) -> Reply {
    let entries = match load_entries(info).await {
        Ok(entries) => entries,
        Err(reply) => return reply,
    };
    let models: Vec<Entry> = entries.iter().map(|e| format_codex_model_with_settings(e, cfg)).collect();
    let routed_search = web_search_capability_for_model(&entries);
    let no_search = |_: &str| None;
    let apply_patch = apply_patch_capability_for_model(&entries, manager);
    let ctx = CatalogContext {
        providers_for_model: None,
        web_search_capability: if client_version == "cpa" { &routed_search } else { &no_search },
        apply_patch_capability: cfg.client.codex.enable_apply_patch.then_some(&apply_patch as &dyn Fn(&str) -> bool),
        optimize_multi_agent_v2: cfg.client.codex.optimize_multi_agent_v2,
        client_version,
    };
    match marshal_compact(build_models(&models, &ctx)) {
        Ok(body) => list_reply(info, Reply::json(200, body)),
        Err(e) => Reply::json(500, format!(r#"{{"error":{}}}"#, Value::String(e))),
    }
}

/// `handleGrokModels` in Home mode.
pub async fn grok_models_reply(info: &ReqInfo) -> Reply {
    match load_entries(info).await {
        Ok(entries) => list_reply(info, Reply::json(200, crate::models::ordered_json(&grok_response(&entries)))),
        Err(reply) => reply,
    }
}

/// `handleHomeGeminiModels`.
pub async fn gemini_models_reply(info: &ReqInfo) -> Reply {
    match load_entries(info).await {
        Ok(entries) => {
            let models: Vec<Value> = entries.iter().map(format_gemini_model).collect();
            list_reply(info, Reply::json_value(200, &json!({"models": models})))
        }
        Err(reply) => reply,
    }
}

/// `handleHomeGeminiModel`: `action` is the raw route parameter (with its leading slash).
pub async fn gemini_model_reply(info: &ReqInfo, action: &str) -> Reply {
    let entries = match load_entries(info).await {
        Ok(entries) => entries,
        Err(reply) => return reply,
    };
    let action = action.strip_prefix('/').unwrap_or(action).trim();
    match entries.iter().find(|entry| gemini_model_matches(entry, action)) {
        Some(entry) => Reply::json_value(200, &format_gemini_model(entry)),
        None => error_reply(404, "Not Found", "not_found"),
    }
}

#[cfg(test)]
mod tests;
