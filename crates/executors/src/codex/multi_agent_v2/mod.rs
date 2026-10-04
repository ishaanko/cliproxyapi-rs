//! Codex multi-agent v2 request optimizer (Go: internal/client/codex/optimize-multi-agent-v2 and
//! the wrappers in executor/helps/codex_multi_agent_v2.go).
//!
//! Official Codex clients send collaboration tools (`spawn_agent`, `send_message`,
//! `followup_task`) inside a `collaboration` namespace and `agent_message` input items with
//! encrypted content. When `client.codex.optimize-multi-agent-v2` is on (or the model runs in
//! compatibility mode) the request is rewritten so the proxy can read and describe them:
//! - the `spawn_agent` description gets the list of models available for overrides;
//! - `message.encrypted` is stripped from the collaboration tool schemas;
//! - the namespace is renamed `collaboration` -> `collaboration-optimize` upstream and mapped
//!   back by [`restore_response`];
//! - `agent_message` items become portable `message`/`user` input.
//!
//! Go reads the client's headers and a prepared-tools marker from the request `context`; here the
//! caller passes the client's headers explicitly (the Go gin request header always wins over the
//! passed headers, so pass the downstream request headers) and the remaining context values in
//! [`RequestCtx`]. Request edits run on one parsed `Value` and the original bytes are returned
//! when nothing changed, matching sjson's byte-preserving no-op.

mod orphan;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cpa_auth::Auth;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_config::Config;
use cpa_core::registry::{
    ModelInfo, ThinkingSupport, get_codex_client_models_revision, get_codex_client_models_snapshot,
    global_registry, lookup_model_info,
};
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use cpa_json::{J, Map, Value};
use http::HeaderMap;
use parking_lot::RwLock;

pub use orphan::{is_collab_spawn_subagent, rewrite_orphan_delegation_input, rewrite_orphan_delegation_input_for_config};

const SPAWN_AGENT_DESCRIPTION_MARKER: &str = "Spawns an agent";
const SPAWN_AGENT_MODELS_HEADING: &str =
    "Available model overrides (optional; inherited parent model is preferred):";
const COLLABORATION_NAMESPACE: &str = "collaboration";
const OPTIMIZED_COLLABORATION_NAMESPACE: &str = "collaboration-optimize";
const OPTIMIZED_COLLABORATION_NAME_PREFIX: &str = "collaboration-optimize__";
const OPTIMIZED_COLLABORATION_DOT_PREFIX: &str = "collaboration-optimize.";

/// Collaboration tools whose `parameters.properties.message.encrypted` field is stripped so the
/// proxy can read message content.
const COLLABORATION_MESSAGE_TOOLS: [&str; 3] = ["spawn_agent", "send_message", "followup_task"];

/// Values Go keeps on the request `context.Context` (via the gin context).
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    /// `CodexMultiAgentV2ToolsPreparedContextKey`: the handler already ran [`prepare_tools`] on
    /// this request, so the optimizer only strips `message.encrypted` instead of refreshing
    /// `spawn_agent` descriptions.
    pub tools_prepared: bool,
}

// ---------------------------------------------------------------------------------------------
// Headers and client identity
// ---------------------------------------------------------------------------------------------

/// First non-blank trimmed value of the header, looked up case-insensitively.
pub(crate) fn header_value_case_insensitive(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Whether a `User-Agent` names an official Codex client.
pub fn is_codex_client_user_agent(user_agent: &str) -> bool {
    let ua = user_agent.trim();
    ua.starts_with("Codex Desktop/")
        || ua.starts_with("codex-tui/")
        || ua == "codex_cli_rs"
        || ua.starts_with("codex_cli_rs/")
        || ua.starts_with("codex_exec/")
}

fn client_user_agent(headers: &HeaderMap) -> String {
    header_value_case_insensitive(headers, "User-Agent")
}

/// Whether `enabled` multi-agent v2 optimization applies to this client (an official Codex client).
pub fn multi_agent_v2_client_enabled(headers: &HeaderMap, enabled: bool) -> bool {
    enabled && is_codex_client_user_agent(&client_user_agent(headers))
}

fn multi_agent_v2_enabled(headers: &HeaderMap, cfg: Option<&Config>) -> bool {
    cfg.is_some_and(|c| {
        multi_agent_v2_client_enabled(headers, c.client.codex.optimize_multi_agent_v2)
    })
}

// ---------------------------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------------------------

/// Optimizes an eligible spawn_agent request (see the module docs) and reports whether the
/// collaboration namespace was renamed for upstream use. Ineligible requests come back untouched.
#[cfg_attr(not(test), allow(dead_code))] // handler-facing API, exercised by the tests
pub fn rewrite_spawn_agent_description(
    ctx: &RequestCtx,
    headers: &HeaderMap,
    payload: &[u8],
    cfg: Option<&Config>,
) -> Vec<u8> {
    optimize_request(ctx, headers, payload, cfg).0
}

/// Converts official Codex multi-agent input into standard Responses messages when multi-agent v2
/// optimization is enabled for this client, or always when `is_compat`. In compat mode it also
/// removes `author`, `recipient` and `internal_chat_message_metadata_passthrough` from every
/// input item.
pub fn rewrite_input(
    headers: &HeaderMap,
    payload: &[u8],
    cfg: Option<&Config>,
    is_compat: bool,
) -> Vec<u8> {
    if !is_compat && !multi_agent_v2_enabled(headers, cfg) {
        return payload.to_vec();
    }
    let mut root = cpa_json::parse(payload);
    let changed = rewrite_agent_message_input(&mut root, true, is_compat);
    finish(&root, changed, payload)
}

/// Prepares collaboration tool definitions at the Responses API boundary without renaming the
/// namespace. The bool is Go's `prepared` flag: true when the client is an enabled Codex client.
pub fn prepare_tools(
    _ctx: &RequestCtx,
    headers: &HeaderMap,
    payload: &[u8],
    enabled: bool,
    home_enabled: bool,
) -> (Vec<u8>, bool) {
    if !multi_agent_v2_client_enabled(headers, enabled) {
        return (payload.to_vec(), false);
    }
    let mut root = cpa_json::parse(payload);
    let changed = prepare_tools_value(&mut root, headers, home_enabled);
    (finish(&root, changed, payload), true)
}

/// Rewrites an eligible spawn_agent request and reports whether the collaboration namespace was
/// renamed for upstream use (the flag to hand to [`restore_response`]).
#[cfg_attr(not(test), allow(dead_code))] // handler-facing API, exercised by the tests
pub fn optimize_request(
    ctx: &RequestCtx,
    headers: &HeaderMap,
    payload: &[u8],
    cfg: Option<&Config>,
) -> (Vec<u8>, bool) {
    if !multi_agent_v2_enabled(headers, cfg) {
        return (payload.to_vec(), false);
    }
    let mut root = cpa_json::parse(payload);
    let (changed, optimized) = optimize_value(&mut root, ctx, headers, cfg);
    (finish(&root, changed, payload), optimized)
}

/// The standard Codex executor request stage: orphan delegation rewrite, multi-agent v2
/// optimization, and (when `is_compat`) agent_message to message/user conversion. API-key
/// credentials see the config without OAuth-only overrides.
pub fn optimize_request_for_auth_with(
    ctx: &RequestCtx,
    headers: &HeaderMap,
    payload: &[u8],
    cfg: Option<&Config>,
    auth: Option<&Auth>,
    is_compat: bool,
) -> (Vec<u8>, bool) {
    let scoped = match (cfg, auth) {
        (Some(c), Some(a)) if a.auth_kind() == AUTH_KIND_API_KEY => Some(c.for_api_key()),
        _ => None,
    };
    let cfg = scoped.as_deref().or(cfg);

    let orphans = cfg.is_some_and(|c| c.codex.orphan_delegation_compatibility)
        && !payload.is_empty()
        && orphan::is_collab_spawn_subagent(headers);
    let optimize = multi_agent_v2_enabled(headers, cfg);
    if !orphans && !optimize && !is_compat {
        return (payload.to_vec(), false);
    }

    let mut root = cpa_json::parse(payload);
    let mut changed = false;
    if orphans {
        changed |= orphan::rewrite_orphan_value(&mut root, payload);
    }
    let mut optimized = false;
    if optimize {
        let (c, o) = optimize_value(&mut root, ctx, headers, cfg);
        changed |= c;
        optimized = o;
    }
    if is_compat {
        changed |= rewrite_agent_message_input(&mut root, true, true);
    }
    (finish(&root, changed, payload), optimized)
}

/// Whether the request defines the reserved optimized namespace, which must stay untouched.
pub fn has_namespace_conflict(payload: &[u8]) -> bool {
    has_optimized_collaboration_conflict(&cpa_json::parse(payload))
}

fn finish(root: &Value, changed: bool, payload: &[u8]) -> Vec<u8> {
    if changed {
        cpa_json::to_vec(root)
    } else {
        payload.to_vec()
    }
}

/// Body of [`optimize_request`] on a parsed request; returns (changed, namespace optimized).
fn optimize_value(
    root: &mut Value,
    ctx: &RequestCtx,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> (bool, bool) {
    let home_enabled = cfg.is_some_and(|c| c.home.enabled);
    let mut changed = rewrite_agent_message_content(root);
    if ctx.tools_prepared {
        let paths = collaboration_message_tool_paths(root);
        changed |= remove_collaboration_message_encryption(root, &paths);
    } else {
        changed |= prepare_tools_value(root, headers, home_enabled);
    }
    let tool_paths = spawn_agent_tool_paths(root);
    if tool_paths.is_empty() || has_optimized_collaboration_conflict(root) {
        return (changed, false);
    }
    let (renamed, optimized) = optimize_collaboration_namespace(root, &tool_paths);
    (changed | renamed, optimized)
}

/// Body of [`prepare_tools`] after the client check; returns whether `root` changed.
fn prepare_tools_value(root: &mut Value, headers: &HeaderMap, home_enabled: bool) -> bool {
    let tool_paths = spawn_agent_tool_paths(root);
    let message_tool_paths = collaboration_message_tool_paths(root);
    if tool_paths.is_empty() && message_tool_paths.is_empty() {
        return false;
    }
    if has_optimized_collaboration_conflict(root) {
        return remove_collaboration_message_encryption(root, &message_tool_paths);
    }

    let spawn_models = (!tool_paths.is_empty())
        .then(|| spawn_agent_models_and_markdown_for_request(headers, home_enabled));
    let (models, markdown) = match &spawn_models {
        Some(m) => (m.models.as_slice(), m.markdown.as_str()),
        None => (&[][..], ""),
    };
    rewrite_collaboration_tools(root, &message_tool_paths, &tool_paths, models, markdown)
}

// ---------------------------------------------------------------------------------------------
// Spawn-agent model catalog
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct SpawnAgentModel {
    id: String,
    description: String,
    reasoning_efforts: Vec<String>,
    default_reasoning_effort: String,
    service_tiers: Vec<String>,
    priority: i64,
    display_name: String,
}

/// Model list plus its rendered markdown for one registry state.
struct SpawnModels {
    models: Vec<SpawnAgentModel>,
    markdown: String,
}

/// Codex client catalog entries keyed by slug, plus the `gpt-5.5` default template.
struct CatalogTemplates {
    by_id: HashMap<String, Map<String, Value>>,
    default: Option<Map<String, Value>>,
}

static TEMPLATES_CACHE: RwLock<Option<(u64, Arc<CatalogTemplates>)>> = RwLock::new(None);
static MODELS_CACHE: RwLock<Option<(u64, u64, Arc<SpawnModels>)>> = RwLock::new(None);

fn parse_catalog_templates(raw: &[u8]) -> CatalogTemplates {
    let catalog = cpa_json::parse(raw);
    let mut by_id = HashMap::new();
    let mut default = None;
    if let Some(Value::Array(models)) = catalog.get("models") {
        for model in models {
            let Value::Object(model) = model else {
                continue;
            };
            let id = map_string(model, "slug");
            if id.is_empty() {
                continue;
            }
            if id == "gpt-5.5" {
                default = Some(model.clone());
            }
            by_id.insert(id, model.clone());
        }
    }
    CatalogTemplates { by_id, default }
}

/// The current catalog templates, re-parsed only when the catalog revision changes.
fn load_catalog_templates() -> Arc<CatalogTemplates> {
    let current = get_codex_client_models_revision();
    if let Some((revision, templates)) = TEMPLATES_CACHE.read().as_ref()
        && *revision == current
    {
        return Arc::clone(templates);
    }
    let (raw, revision) = get_codex_client_models_snapshot();
    let mut guard = TEMPLATES_CACHE.write();
    if let Some((cached_revision, templates)) = guard.as_ref()
        && *cached_revision == revision
    {
        return Arc::clone(templates);
    }
    let templates = Arc::new(parse_catalog_templates(&raw));
    *guard = Some((revision, Arc::clone(&templates)));
    templates
}

fn registry_lookup(model_id: &str) -> Option<ModelInfo> {
    lookup_model_info(model_id, None)
}

/// Models offered for spawn_agent overrides and their markdown. Without home, the result is
/// cached per (catalog revision, registry generation).
fn spawn_agent_models_and_markdown_for_request(
    headers: &HeaderMap,
    home_enabled: bool,
) -> Arc<SpawnModels> {
    let empty = || {
        Arc::new(SpawnModels {
            models: Vec::new(),
            markdown: String::new(),
        })
    };
    if home_enabled {
        let templates = load_catalog_templates();
        let Some(default) = &templates.default else {
            return empty();
        };
        let available = home_available_models(headers);
        let models = spawn_agent_models_from_templates(
            &available,
            &templates.by_id,
            default,
            &registry_lookup,
        );
        let markdown = format_spawn_agent_models(&models);
        return Arc::new(SpawnModels { models, markdown });
    }

    let current_revision = get_codex_client_models_revision();
    let registry = global_registry();
    let current_generation = registry.get_generation();
    if let Some((revision, generation, cached)) = MODELS_CACHE.read().as_ref()
        && *revision == current_revision
        && *generation == current_generation
    {
        return Arc::clone(cached);
    }

    let templates = load_catalog_templates();
    let Some(default) = &templates.default else {
        return empty();
    };
    let available = registry.get_available_models("openai");
    let models =
        spawn_agent_models_from_templates(&available, &templates.by_id, default, &registry_lookup);
    let markdown = format_spawn_agent_models(&models);
    let result = Arc::new(SpawnModels { models, markdown });

    let mut guard = MODELS_CACHE.write();
    if get_codex_client_models_revision() == current_revision
        && registry.get_generation() == current_generation
    {
        *guard = Some((current_revision, current_generation, Arc::clone(&result)));
    }
    result
}

/// Models the Home control plane reports as available to this client (Go: codexHomeAvailableModels).
/// Without a Home client, or when the query fails, there are none. The query runs through the
/// blocking Home bridge, so outside a multi-thread runtime it also yields none.
fn home_available_models(headers: &HeaderMap) -> Vec<Map<String, Value>> {
    let Some(client) = cpa_home::kv::current() else {
        return Vec::new();
    };
    crate::helps::home_kv::call(query_home_models(&client, headers)).unwrap_or_default()
}

/// One models query against Home: `client_version` is sent empty like Go.
async fn query_home_models(
    client: &cpa_home::Client,
    headers: &HeaderMap,
) -> Result<Vec<Map<String, Value>>, cpa_home::HomeError> {
    let query = [("client_version".to_string(), String::new())];
    let raw = client.get_models(headers, &query).await?;
    Ok(decode_home_available_models(&raw))
}

/// Decodes the home control plane's models response (`{section: [{id|name, display_name}]}`)
/// into id-sorted, de-duplicated entries. Anything else (for example an error envelope) yields
/// an empty list. Sections are visited in key order so de-duplication is deterministic.
pub fn decode_home_available_models(raw: &[u8]) -> Vec<Map<String, Value>> {
    let Value::Object(sections) = cpa_json::parse(raw) else {
        return Vec::new();
    };
    if sections.is_empty() {
        return Vec::new();
    }
    // Go decodes into map[string][]map[string]any, which fails on any other shape.
    for section in sections.values() {
        match section {
            Value::Null => {}
            Value::Array(models) if models.iter().all(|m| m.is_object() || m.is_null()) => {}
            _ => return Vec::new(),
        }
    }

    let mut keys: Vec<&String> = sections.keys().collect();
    keys.sort();
    let mut seen: HashSet<String> = HashSet::new();
    let mut models: Vec<Map<String, Value>> = Vec::new();
    for key in keys {
        let Value::Array(section_models) = &sections[key] else {
            continue;
        };
        for model in section_models {
            let Value::Object(model) = model else {
                continue;
            };
            let mut model_id = map_string(model, "id");
            if model_id.is_empty() {
                let name = map_string(model, "name");
                model_id = name.strip_prefix("models/").unwrap_or(&name).to_string();
            }
            if model_id.is_empty() || !seen.insert(model_id.clone()) {
                continue;
            }
            let mut display_name = map_string(model, "display_name");
            if display_name.is_empty() {
                display_name = map_string(model, "displayName");
            }
            let mut entry = Map::new();
            entry.insert("id".into(), Value::String(model_id));
            if !display_name.is_empty() {
                entry.insert("display_name".into(), Value::String(display_name.clone()));
                entry.insert("description".into(), Value::String(display_name));
            }
            models.push(entry);
        }
    }
    models.sort_by_key(|m| map_string(m, "id"));
    models
}

/// Spawn-agent models for `available` models using a raw catalog JSON; empty when the catalog
/// has no models or no `gpt-5.5` default template. Test helper (runtime uses cached templates).
#[cfg(test)]
fn spawn_agent_models_from_sources(
    available: &[Map<String, Value>],
    catalog_json: &[u8],
    lookup: &dyn Fn(&str) -> Option<ModelInfo>,
) -> Vec<SpawnAgentModel> {
    let templates = parse_catalog_templates(catalog_json);
    match &templates.default {
        Some(default) => {
            spawn_agent_models_from_templates(available, &templates.by_id, default, lookup)
        }
        None => Vec::new(),
    }
}

/// Catalog-backed models first (by priority, id), then registry-only models profiled from the
/// default template (by lowercase display name, id).
fn spawn_agent_models_from_templates(
    available: &[Map<String, Value>],
    templates: &HashMap<String, Map<String, Value>>,
    default_template: &Map<String, Value>,
    lookup: &dyn Fn(&str) -> Option<ModelInfo>,
) -> Vec<SpawnAgentModel> {
    let mut seen: HashSet<String> = HashSet::with_capacity(available.len());
    let mut template_models: Vec<SpawnAgentModel> = Vec::new();
    let mut synthesized_models: Vec<SpawnAgentModel> = Vec::new();
    for available_model in available {
        let model_id = map_string(available_model, "id");
        if model_id.is_empty() || !seen.insert(model_id.clone()) {
            continue;
        }
        if let Some(template) = templates.get(&model_id) {
            template_models.push(spawn_agent_model_from_metadata(&model_id, template));
            continue;
        }

        let mut profile = spawn_agent_model_from_metadata(&model_id, default_template);
        profile.id = model_id.clone();
        profile.description = map_string(available_model, "description");
        profile.display_name = map_string(available_model, "display_name");
        if profile.display_name.is_empty() {
            profile.display_name = model_id.clone();
        }
        if let Some(info) = lookup(&model_id) {
            if !info.description.trim().is_empty() {
                profile.description = info.description.trim().to_string();
            }
            apply_spawn_agent_thinking(&mut profile, info.thinking.as_ref());
        }
        if profile.description.is_empty() {
            profile.description = model_id.clone();
        }
        profile.service_tiers = Vec::new();
        synthesized_models.push(profile);
    }

    template_models.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    synthesized_models.sort_by(|a, b| {
        let (left, right) = (a.display_name.to_lowercase(), b.display_name.to_lowercase());
        match left.cmp(&right) {
            Ordering::Equal => a.id.cmp(&b.id),
            other => other,
        }
    });
    template_models.extend(synthesized_models);
    template_models
}

fn spawn_agent_model_from_metadata(
    model_id: &str,
    metadata: &Map<String, Value>,
) -> SpawnAgentModel {
    let (reasoning_efforts, default_reasoning_effort) = reasoning_metadata(metadata);
    SpawnAgentModel {
        id: model_id.to_string(),
        description: map_string(metadata, "description"),
        display_name: map_string(metadata, "display_name"),
        priority: map_int(metadata, "priority"),
        reasoning_efforts,
        default_reasoning_effort,
        service_tiers: service_tier_ids(metadata),
    }
}

/// Overrides the profile's reasoning efforts from a registry model's thinking levels.
fn apply_spawn_agent_thinking(profile: &mut SpawnAgentModel, thinking: Option<&ThinkingSupport>) {
    let Some(thinking) = thinking else { return };
    if thinking.levels.is_empty() {
        return;
    }
    let mut efforts: Vec<String> = Vec::with_capacity(thinking.levels.len());
    let mut default_effort = String::new();
    let mut first_effort = String::new();
    for raw in &thinking.levels {
        let effort = normalize_reasoning_effort(raw);
        if effort.is_empty() {
            continue;
        }
        if first_effort.is_empty() {
            first_effort = effort.clone();
        }
        if (default_effort.is_empty() && effort != "none") || effort == "medium" {
            default_effort = effort.clone();
        }
        efforts.push(effort);
    }
    if efforts.is_empty() {
        return;
    }
    if default_effort.is_empty() {
        default_effort = first_effort;
    }
    profile.reasoning_efforts = efforts;
    profile.default_reasoning_effort = default_effort;
}

fn reasoning_metadata(metadata: &Map<String, Value>) -> (Vec<String>, String) {
    let mut efforts: Vec<String> = Vec::new();
    if let Some(Value::Array(levels)) = metadata.get("supported_reasoning_levels") {
        for level in levels {
            let effort = match level {
                Value::Object(level) => normalize_reasoning_effort(&map_string(level, "effort")),
                _ => String::new(),
            };
            if !effort.is_empty() {
                efforts.push(effort);
            }
        }
    }
    if efforts.is_empty() {
        return (Vec::new(), String::new());
    }
    let mut default_effort =
        normalize_reasoning_effort(&map_string(metadata, "default_reasoning_level"));
    if !efforts.contains(&default_effort) {
        default_effort = efforts[0].clone();
    }
    (efforts, default_effort)
}

fn normalize_reasoning_effort(effort: &str) -> String {
    let effort = effort.trim().to_lowercase();
    match effort.as_str() {
        "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => effort,
        _ => String::new(),
    }
}

fn service_tier_ids(metadata: &Map<String, Value>) -> Vec<String> {
    let mut tiers: Vec<String> = Vec::new();
    if let Some(Value::Array(raw_tiers)) = metadata.get("service_tiers") {
        for tier in raw_tiers {
            let id = match tier {
                Value::Object(tier) => map_string(tier, "id"),
                _ => String::new(),
            };
            if !id.is_empty() && !tiers.contains(&id) {
                tiers.push(id);
            }
        }
    }
    tiers
}

/// String field of a JSON object, trimmed; "" for missing or non-string values.
fn map_string(values: &Map<String, Value>, key: &str) -> String {
    match values.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// Integer field of a JSON object (floats truncated); 0 for missing or non-numeric values.
fn map_int(values: &Map<String, Value>, key: &str) -> i64 {
    match values.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Tool description / schema rewriting
// ---------------------------------------------------------------------------------------------

/// Rewrites spawn_agent descriptions with `models` and strips `message.encrypted` from the
/// spawn_agent tools. Test helper mirroring Go's `rewriteCodexSpawnAgentDescription`.
#[cfg(test)]
fn rewrite_spawn_agent_description_with_models(
    payload: &[u8],
    models: &[SpawnAgentModel],
) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let paths = spawn_agent_tool_paths(&root);
    let changed = rewrite_collaboration_tools(&mut root, &paths, &paths, models, "");
    finish(&root, changed, payload)
}

/// Applies the model list to each spawn_agent description and deletes `message.encrypted` from
/// each message tool. `model_list` defaults to the rendering of `models`.
fn rewrite_collaboration_tools(
    root: &mut Value,
    message_tool_paths: &[String],
    spawn_agent_tool_paths: &[String],
    models: &[SpawnAgentModel],
    model_list: &str,
) -> bool {
    if message_tool_paths.is_empty() && spawn_agent_tool_paths.is_empty() {
        return false;
    }
    let formatted;
    let model_list = if model_list.is_empty() && !models.is_empty() {
        formatted = format_spawn_agent_models(models);
        formatted.as_str()
    } else {
        model_list
    };

    let mut changed = false;
    for tool_path in spawn_agent_tool_paths {
        let description_path = format!("{tool_path}.description");
        let Some(Value::String(description)) = root.g(&description_path).v().cloned() else {
            continue;
        };
        if model_list.is_empty() {
            continue;
        }
        let rewritten = replace_spawn_agent_models(&description, model_list);
        if rewritten != description {
            cpa_json::set(root, &description_path, rewritten);
            changed = true;
        }
    }
    changed | remove_collaboration_message_encryption(root, message_tool_paths)
}

/// Whether the request defines the reserved optimized namespace in `tools` or in an
/// `additional_tools` input item.
fn has_optimized_collaboration_conflict(root: &Value) -> bool {
    if tools_have_optimized_collaboration_conflict(root.get("tools")) {
        return true;
    }
    let Some(Value::Array(input)) = root.get("input") else {
        return false;
    };
    input.iter().any(|item| {
        item.g("type").str().trim() == "additional_tools"
            && tools_have_optimized_collaboration_conflict(item.get("tools"))
    })
}

fn tools_have_optimized_collaboration_conflict(tools: Option<&Value>) -> bool {
    let Some(Value::Array(tools)) = tools else {
        return false;
    };
    tools.iter().any(|tool| {
        let name = tool.g("name").str();
        let name = name.trim();
        name == OPTIMIZED_COLLABORATION_NAMESPACE
            || name.starts_with(OPTIMIZED_COLLABORATION_NAME_PREFIX)
            || name.starts_with(OPTIMIZED_COLLABORATION_DOT_PREFIX)
            || (tool.g("type").str().trim() == "namespace"
                && tools_have_optimized_collaboration_conflict(tool.get("tools")))
    })
}

/// Renames the `collaboration` namespace owning each spawn_agent tool; returns (changed, renamed).
fn optimize_collaboration_namespace(root: &mut Value, tool_paths: &[String]) -> (bool, bool) {
    let mut optimized = false;
    for tool_path in tool_paths {
        let Some(separator) = tool_path.rfind(".tools.") else {
            continue;
        };
        let namespace_path = &tool_path[..separator];
        let namespace = root.g(namespace_path);
        let is_collaboration = namespace.g("type").str().trim() == "namespace"
            && namespace.g("name").str().trim() == COLLABORATION_NAMESPACE;
        if !is_collaboration {
            continue;
        }
        cpa_json::set(
            root,
            &format!("{namespace_path}.name"),
            OPTIMIZED_COLLABORATION_NAMESPACE,
        );
        optimized = true;
    }
    (optimized, optimized)
}

// ---------------------------------------------------------------------------------------------
// Response restore
// ---------------------------------------------------------------------------------------------

/// Restores optimized collaboration namespace values in an upstream response before it is
/// translated and returned to the client. Like Go's decode/encode round trip, a changed payload
/// is re-encoded with sorted object keys and HTML-escaped strings; an unchanged one is returned
/// as is.
pub fn restore_response(payload: &[u8], optimized: bool) -> Vec<u8> {
    if !optimized || payload.is_empty() || !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    let mut value = cpa_json::parse(payload);
    if !restore_collaboration_value(&mut value) {
        return payload.to_vec();
    }
    match go_json_sorted(&value, GoJsonStyle::MARSHAL_USE_NUMBER) {
        Some(restored) => restored.into_bytes(),
        None => payload.to_vec(),
    }
}

fn restore_collaboration_value(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Array(items) => {
            for item in items {
                changed |= restore_collaboration_value(item);
            }
        }
        Value::Object(map) => {
            let item_type = map_string(map, "type");
            let is_tool_call = item_type == "function_call" || item_type == "custom_tool_call";
            if is_tool_call
                && let Some(Value::String(namespace)) = map.get("namespace")
                && namespace == OPTIMIZED_COLLABORATION_NAMESPACE
            {
                map.insert(
                    "namespace".into(),
                    Value::String(COLLABORATION_NAMESPACE.into()),
                );
                changed = true;
            }
            if let Some(Value::String(name)) = map.get("name").cloned() {
                if name == OPTIMIZED_COLLABORATION_NAMESPACE && item_type == "namespace" {
                    map.insert("name".into(), Value::String(COLLABORATION_NAMESPACE.into()));
                    changed = true;
                } else if is_tool_call && name.starts_with(OPTIMIZED_COLLABORATION_DOT_PREFIX) {
                    let tool_name = &name[OPTIMIZED_COLLABORATION_DOT_PREFIX.len()..];
                    if !tool_name.is_empty() {
                        map.insert(
                            "namespace".into(),
                            Value::String(COLLABORATION_NAMESPACE.into()),
                        );
                        map.insert("name".into(), Value::String(tool_name.to_string()));
                        changed = true;
                    }
                } else if is_tool_call && name.starts_with(OPTIMIZED_COLLABORATION_NAME_PREFIX) {
                    let rest = &name[OPTIMIZED_COLLABORATION_NAME_PREFIX.len()..];
                    map.insert(
                        "name".into(),
                        Value::String(format!("{COLLABORATION_NAMESPACE}__{rest}")),
                    );
                    changed = true;
                }
            }
            let is_output_item =
                item_type == "function_call_output" || item_type == "custom_tool_call_output";
            for (key, child) in map.iter_mut() {
                // Arguments, inputs and tool outputs are opaque model/tool payloads.
                if key == "arguments" || key == "input" || (key == "output" && is_output_item) {
                    continue;
                }
                changed |= restore_collaboration_value(child);
            }
        }
        _ => {}
    }
    changed
}

// ---------------------------------------------------------------------------------------------
// Input rewriting
// ---------------------------------------------------------------------------------------------

/// Converts `agent_message` items to `message`/`user` (when `optimize_enabled`) and, in compat
/// mode, drops the non-standard metadata fields from every item. Requires `input` to be an array.
fn rewrite_agent_message_input(
    root: &mut Value,
    optimize_enabled: bool,
    compat_mode: bool,
) -> bool {
    if !matches!(root.get("input"), Some(Value::Array(_))) {
        return false;
    }
    let mut changed = false;
    if optimize_enabled {
        changed |= rewrite_agent_message_content(root);
    }
    let Some(Value::Array(items)) = root.get_mut("input") else {
        return changed;
    };
    for item in items.iter_mut() {
        if optimize_enabled && item.g("type").str().trim() == "agent_message" {
            cpa_json::set(item, "role", "user");
            cpa_json::set(item, "type", "message");
            changed = true;
        }
        if compat_mode {
            for field in [
                "author",
                "recipient",
                "internal_chat_message_metadata_passthrough",
            ] {
                if item.g(field).exists() {
                    cpa_json::delete(item, field);
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Turns `encrypted_content` parts of `agent_message` items into plain `input_text` parts.
fn rewrite_agent_message_content(root: &mut Value) -> bool {
    let Some(Value::Array(items)) = root.get_mut("input") else {
        return false;
    };
    let mut changed = false;
    for item in items.iter_mut() {
        if item.g("type").str().trim() != "agent_message" {
            continue;
        }
        let Some(Value::Array(parts)) = item.get_mut("content") else {
            continue;
        };
        for part in parts.iter_mut() {
            if part.g("type").str().trim() != "encrypted_content" {
                continue;
            }
            let Some(Value::String(encrypted)) = part.get("encrypted_content") else {
                continue;
            };
            let text = encrypted.clone();
            cpa_json::set(part, "type", "input_text");
            cpa_json::set(part, "text", text);
            cpa_json::delete(part, "encrypted_content");
            changed = true;
        }
    }
    changed
}

// ---------------------------------------------------------------------------------------------
// Tool discovery
// ---------------------------------------------------------------------------------------------

fn spawn_agent_tool_paths(root: &Value) -> Vec<String> {
    tool_paths_by_names(root, &["spawn_agent"])
}

/// Paths of function tools named spawn_agent, send_message or followup_task inside top-level
/// `tools` and `input[].additional_tools` arrays, including nested namespace tools.
fn collaboration_message_tool_paths(root: &Value) -> Vec<String> {
    tool_paths_by_names(root, &COLLABORATION_MESSAGE_TOOLS)
}

fn tool_paths_by_names(root: &Value, names: &[&str]) -> Vec<String> {
    let mut paths = Vec::new();
    collect_tool_paths_by_names(root.get("tools"), "tools", &mut paths, names);
    if let Some(Value::Array(input)) = root.get("input") {
        for (index, item) in input.iter().enumerate() {
            if item.g("type").str().trim() != "additional_tools" {
                continue;
            }
            collect_tool_paths_by_names(
                item.get("tools"),
                &format!("input.{index}.tools"),
                &mut paths,
                names,
            );
        }
    }
    paths
}

fn collect_tool_paths_by_names(
    tools: Option<&Value>,
    path: &str,
    paths: &mut Vec<String>,
    names: &[&str],
) {
    let Some(Value::Array(tools)) = tools else {
        return;
    };
    for (index, tool) in tools.iter().enumerate() {
        let tool_path = format!("{path}.{index}");
        let tool_type = tool.g("type").str();
        let tool_type = tool_type.trim();
        if tool_type == "function" && names.contains(&tool.g("name").str().trim()) {
            paths.push(tool_path.clone());
        }
        if tool_type == "namespace" {
            collect_tool_paths_by_names(
                tool.get("tools"),
                &format!("{tool_path}.tools"),
                paths,
                names,
            );
        }
    }
}

/// Deletes `parameters.properties.message.encrypted` from each tool so the proxy can read the
/// plaintext message.
fn remove_collaboration_message_encryption(root: &mut Value, tool_paths: &[String]) -> bool {
    let mut changed = false;
    for tool_path in tool_paths {
        let encrypted_path = format!("{tool_path}.parameters.properties.message.encrypted");
        if root.g(&encrypted_path).exists() {
            cpa_json::delete(root, &encrypted_path);
            changed = true;
        }
    }
    changed
}

// ---------------------------------------------------------------------------------------------
// Model list markdown
// ---------------------------------------------------------------------------------------------

fn format_spawn_agent_models(models: &[SpawnAgentModel]) -> String {
    let mut out = String::new();
    for model in models {
        let model_id = model.id.split_whitespace().collect::<Vec<_>>().join(" ");
        if model_id.is_empty() {
            continue;
        }
        out.push_str("- ");
        out.push_str(&markdown_code(&model_id));
        out.push_str(": ");
        let mut has_details = false;
        let description = model
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !description.is_empty() {
            write_sentence(&mut out, &description);
            has_details = true;
        }
        if !model.reasoning_efforts.is_empty() {
            if has_details {
                out.push(' ');
            }
            out.push_str("Reasoning efforts: ");
            for (index, effort) in model.reasoning_efforts.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(effort);
                if *effort == model.default_reasoning_effort {
                    out.push_str(" (default)");
                }
            }
            out.push('.');
            has_details = true;
        }
        if !model.service_tiers.is_empty() {
            if has_details {
                out.push(' ');
            }
            out.push_str("Service tiers: ");
            out.push_str(&model.service_tiers.join(", "));
            out.push('.');
        }
        out.push('\n');
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn markdown_code(value: &str) -> String {
    if value.contains('`') {
        format!("`` {value} ``")
    } else {
        format!("`{value}`")
    }
}

fn write_sentence(out: &mut String, value: &str) {
    out.push_str(value);
    if !value.ends_with(['.', '!', '?']) {
        out.push('.');
    }
}

/// Replaces any existing model-override section of a spawn_agent description with `model_list`,
/// placed before the line holding the "Spawns an agent" marker (or appended when absent).
fn replace_spawn_agent_models(description: &str, model_list: &str) -> String {
    if model_list.is_empty() {
        return description.to_string();
    }
    let (cleaned, heading_indent) = remove_spawn_agent_model_sections(description);
    let section = format!("{heading_indent}{SPAWN_AGENT_MODELS_HEADING}\n{model_list}\n");
    if let Some(marker_index) = cleaned.find(SPAWN_AGENT_DESCRIPTION_MARKER) {
        let line_start = cleaned[..marker_index].rfind('\n').map_or(0, |i| i + 1);
        return format!(
            "{}{}{}",
            &cleaned[..line_start],
            section,
            &cleaned[line_start..]
        );
    }
    let separator = if !cleaned.is_empty() && !cleaned.ends_with('\n') {
        "\n\n"
    } else {
        ""
    };
    let section = section.strip_suffix('\n').unwrap_or(&section);
    format!("{cleaned}{separator}{section}")
}

/// Drops every model-override heading with its following `- ` bullet lines; returns the cleaned
/// text and the indentation of the first indented heading.
fn remove_spawn_agent_model_sections(description: &str) -> (String, String) {
    if !description.contains(SPAWN_AGENT_MODELS_HEADING) {
        return (description.to_string(), String::new());
    }
    let lines: Vec<&str> = description.split_inclusive('\n').collect();
    let mut cleaned = String::with_capacity(description.len());
    let mut heading_indent = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() != SPAWN_AGENT_MODELS_HEADING {
            cleaned.push_str(line);
            index += 1;
            continue;
        }
        if heading_indent.is_empty()
            && let Some(heading_index) = line.find(SPAWN_AGENT_MODELS_HEADING)
            && heading_index > 0
        {
            heading_indent = line[..heading_index].to_string();
        }
        index += 1;
        while index < lines.len() && lines[index].trim().starts_with("- ") {
            index += 1;
        }
    }
    (cleaned, heading_indent)
}

#[cfg(test)]
mod tests;
