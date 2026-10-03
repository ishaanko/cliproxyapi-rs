//! Conversions between host types and plugin payloads (Go: the clone/convert helpers spread over
//! `adapters*.go`, `auth_provider.go` and `auth_callbacks.go`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::Utc;
use cpa_auth::Auth;
use cpa_auth::Status;
use cpa_auth::storage::PluginTokenStorage;
use cpa_auth::types::{ATTRIBUTE_PATH, ATTRIBUTE_SOURCE, ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_FILE};
use cpa_config::Config;
use cpa_core::registry::{ModelInfo as RegistryModel, ThinkingSupport as RegistryThinking};
use cpa_pluginapi::api::{
    AuthData, Header, HostConfigSummary, ModelAlias, ModelInfo as PluginModel, ThinkingSupport as PluginThinking,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

/// Go `textproto.CanonicalHeaderKey`: each hyphen separated word gets an upper-case first letter.
pub fn canonical_header_key(key: &str) -> String {
    if key.bytes().any(|b| b == b' ' || !b.is_ascii()) {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for c in key.chars() {
        if upper {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c.to_ascii_lowercase());
        }
        upper = c == '-';
    }
    out
}

/// Server headers as Go `http.Header` (canonical keys, every value).
pub fn headers_to_go(headers: &HeaderMap) -> Header {
    let mut out: Header = BTreeMap::new();
    for (name, value) in headers {
        out.entry(canonical_header_key(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

/// Go `http.Header` back to a [`HeaderMap`]; entries that are not valid HTTP headers are skipped.
pub fn headers_from_go(headers: &Header) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (key, values) in headers {
        let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else { continue };
        for v in values {
            if let Ok(value) = HeaderValue::from_str(v) {
                out.append(name.clone(), value);
            }
        }
    }
    out
}

/// Go `Header.Add`/`Header.Set` use canonical keys.
pub fn header_get<'a>(h: &'a Header, key: &str) -> Option<&'a Vec<String>> {
    let canon = canonical_header_key(key);
    h.iter().find(|(k, _)| canonical_header_key(k) == canon).map(|(_, v)| v)
}

/// `mergeHeaders`: removes `clear`, replaces keys present in `updates`, keeps the rest.
pub fn merge_headers(current: &Header, updates: &Header, clear: &[String]) -> Header {
    let mut out: Header = current.clone();
    for key in clear {
        header_del(&mut out, key);
    }
    for (key, values) in updates {
        header_del(&mut out, key);
        let canon = canonical_header_key(key);
        for v in values {
            out.entry(canon.clone()).or_default().push(v.clone());
        }
    }
    out
}

/// `http.Header.Del`: canonical key removal (also drops differently cased duplicates).
pub fn header_del(h: &mut Header, key: &str) {
    let canon = canonical_header_key(key);
    h.retain(|k, _| canonical_header_key(k) != canon);
}

/// Query pairs as `url.Values`.
pub fn query_to_go(pairs: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in pairs {
        out.entry(k.clone()).or_default().push(v.clone());
    }
    out
}

pub fn query_from_go(q: &BTreeMap<String, Vec<String>>) -> Vec<(String, String)> {
    q.iter().flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone()))).collect()
}

/// Metadata keys only this port stores in execution metadata (request facts for usage records
/// and selector bookkeeping); plugins never saw them in the Go host, and the client API key
/// must not reach plugins.
const HOST_ONLY_METADATA_KEYS: [&str; 8] = [
    "client_ip",
    "resolved_client_ip",
    "x_forwarded_for",
    "user_agent",
    "request_id",
    "trace_id",
    "client_api_key",
    "cpa.session_affinity_ids",
];

/// Execution metadata as plugins see it: a JSON object with sorted keys (Go marshals maps
/// sorted), minus the host-only facts.
pub fn plugin_visible_metadata(src: &HashMap<String, Value>) -> Map<String, Value> {
    let mut keys: Vec<&String> = src.keys().filter(|k| !HOST_ONLY_METADATA_KEYS.contains(&k.as_str())).collect();
    keys.sort();
    let mut out = Map::new();
    for k in keys {
        out.insert(k.clone(), src[k].clone());
    }
    out
}

pub fn map_to_hash(src: &Map<String, Value>) -> HashMap<String, Value> {
    src.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Go `strings.TrimSpace(...) == ""` over several values.
pub fn first_non_empty(values: &[&str]) -> String {
    values.iter().map(|v| v.trim()).find(|v| !v.is_empty()).unwrap_or("").to_string()
}

pub fn normalize_provider_id(provider: &str) -> String {
    provider.trim().to_lowercase()
}

pub fn status_str(s: Status) -> &'static str {
    match s {
        Status::Unknown => "unknown",
        Status::Active => "active",
        Status::Pending => "pending",
        Status::Refreshing => "refreshing",
        Status::Error => "error",
        Status::Disabled => "disabled",
    }
}

// ---- host config summary ----

pub fn host_config_summary(cfg: Option<&Config>) -> HostConfigSummary {
    let Some(cfg) = cfg else { return HostConfigSummary::default() };
    let mut aliases: BTreeMap<String, Vec<ModelAlias>> = BTreeMap::new();
    for (provider, list) in &cfg.oauth_model_alias {
        let key = normalize_provider_id(provider);
        if key.is_empty() {
            continue;
        }
        for alias in list {
            let (name, value) = (alias.name.trim(), alias.alias.trim());
            if name.is_empty() || value.is_empty() {
                continue;
            }
            aliases.entry(key.clone()).or_default().push(ModelAlias { name: name.into(), alias: value.into() });
        }
    }
    let mut excluded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (provider, models) in &cfg.oauth_excluded_models {
        let key = normalize_provider_id(provider);
        if !key.is_empty() {
            excluded.insert(key, models.clone());
        }
    }
    HostConfigSummary {
        auth_dir: cfg.auth_dir.trim().to_string(),
        proxy_url: cfg.proxy_url.trim().to_string(),
        force_model_prefix: cfg.force_model_prefix,
        oauth_model_alias: aliases,
        excluded_models: excluded,
    }
}

// ---- model info ----

pub fn plugin_model_to_registry(m: &PluginModel) -> RegistryModel {
    RegistryModel {
        id: m.id.clone(),
        object: m.object.clone(),
        created: m.created,
        owned_by: m.owned_by.clone(),
        r#type: m.kind.clone(),
        display_name: m.display_name.clone(),
        name: m.name.clone(),
        version: m.version.clone(),
        description: m.description.clone(),
        input_token_limit: m.input_token_limit as _,
        output_token_limit: m.output_token_limit as _,
        supported_generation_methods: m.supported_generation_methods.clone(),
        context_length: m.context_length as _,
        max_completion_tokens: m.max_completion_tokens as _,
        supported_parameters: m.supported_parameters.clone(),
        supported_input_modalities: m.supported_input_modalities.clone(),
        supported_output_modalities: m.supported_output_modalities.clone(),
        thinking: m.thinking.as_ref().map(|t| RegistryThinking {
            min: t.min as _,
            max: t.max as _,
            zero_allowed: t.zero_allowed,
            dynamic_allowed: t.dynamic_allowed,
            levels: t.levels.clone(),
        }),
        user_defined: m.user_defined,
        ..Default::default()
    }
}

pub fn registry_model_to_plugin(m: &RegistryModel) -> PluginModel {
    PluginModel {
        id: m.id.clone(),
        object: m.object.clone(),
        created: m.created,
        owned_by: m.owned_by.clone(),
        kind: m.r#type.clone(),
        display_name: m.display_name.clone(),
        name: m.name.clone(),
        version: m.version.clone(),
        description: m.description.clone(),
        input_token_limit: m.input_token_limit,
        output_token_limit: m.output_token_limit,
        supported_generation_methods: m.supported_generation_methods.clone(),
        context_length: m.context_length,
        max_completion_tokens: m.max_completion_tokens,
        supported_parameters: m.supported_parameters.clone(),
        supported_input_modalities: m.supported_input_modalities.clone(),
        supported_output_modalities: m.supported_output_modalities.clone(),
        thinking: m.thinking.as_ref().map(|t| PluginThinking {
            min: t.min,
            max: t.max,
            zero_allowed: t.zero_allowed,
            dynamic_allowed: t.dynamic_allowed,
            levels: t.levels.clone(),
        }),
        user_defined: m.user_defined,
    }
}

// ---- auth ----

/// `storageJSONFromAuth`: the plugin storage payload, else the metadata as JSON.
pub fn storage_json_from_auth(auth: Option<&Auth>) -> Vec<u8> {
    let Some(auth) = auth else { return Vec::new() };
    if let Some(cpa_auth::TokenStorage::Plugin(p)) = &auth.storage {
        return p.raw_json_payload().unwrap_or_default();
    }
    if auth.metadata.is_empty() {
        return Vec::new();
    }
    cpa_auth::util::marshal_compact(&Value::Object(auth.metadata.clone())).map(String::into_bytes).unwrap_or_default()
}

/// `authIDForPath`: path relative to the auth dir (when inside it), slash separated.
pub fn auth_id_for_path(path: &str, auth_dir: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    let mut id = path.to_string();
    let dir = auth_dir.trim();
    if !dir.is_empty()
        && let Some(rel) = relative_path(dir, path)
        && !rel.is_empty()
        && !rel.starts_with("..")
    {
        id = rel;
    }
    let cleaned = crate::platform::clean_path(Path::new(&id));
    cleaned.to_string_lossy().replace('\\', "/")
}

/// `filepath.Rel` for the common case of `path` below `base`; `None` otherwise.
pub fn relative_path(base: &str, path: &str) -> Option<String> {
    let base = crate::platform::clean_path(Path::new(base));
    let path = crate::platform::clean_path(Path::new(path));
    path.strip_prefix(&base).ok().map(|p| p.to_string_lossy().into_owned())
}

/// `pluginAuthDataToCoreAuth`.
pub fn plugin_auth_data_to_core_auth(data: &AuthData, path: &str, file_name: &str, auth_dir: &str) -> Option<Auth> {
    let provider = normalize_provider_id(&data.provider);
    if provider.is_empty() {
        return None;
    }
    let mut metadata = data.metadata.clone();
    metadata.insert("type".into(), Value::String(provider.clone()));
    let mut attributes: BTreeMap<String, String> = data.attributes.clone();
    let path = path.trim();
    if !path.is_empty() {
        attributes.entry(ATTRIBUTE_PATH.into()).and_modify(|v| fill_if_empty(v, path)).or_insert_with(|| path.into());
        attributes.entry(ATTRIBUTE_SOURCE.into()).and_modify(|v| fill_if_empty(v, path)).or_insert_with(|| path.into());
        attributes
            .entry(ATTRIBUTE_SOURCE_BACKEND.into())
            .and_modify(|v| fill_if_empty(v, AUTH_SOURCE_FILE))
            .or_insert_with(|| AUTH_SOURCE_FILE.into());
    }
    let file_name = first_non_empty(&[&data.file_name, file_name]);
    if !file_name.is_empty() && attributes.get(ATTRIBUTE_SOURCE).is_none_or(|v| v.is_empty()) {
        attributes.insert(ATTRIBUTE_SOURCE.into(), file_name.clone());
    }
    let mut id = data.id.trim().to_string();
    if id.is_empty() {
        id = auth_id_for_path(&first_non_empty(&[path, &file_name]), auth_dir);
    }
    let status = if data.disabled { Status::Disabled } else { Status::Active };
    let now = Utc::now();
    let storage = PluginTokenStorage { provider: provider.clone(), raw_json: data.storage_json.clone(), meta: metadata.clone() };
    let mut auth = Auth::default();
    auth.provider = provider;
    auth.id = id;
    auth.file_name = file_name;
    auth.label = data.label.trim().to_string();
    auth.prefix = data.prefix.trim().to_string();
    auth.proxy_url = data.proxy_url.trim().to_string();
    auth.disabled = data.disabled;
    auth.status = status;
    auth.storage = Some(cpa_auth::TokenStorage::Plugin(storage));
    auth.metadata = metadata;
    auth.attributes = attributes;
    auth.created_at = Some(now);
    auth.updated_at = Some(now);
    auth.next_refresh_after = data.next_refresh_after;
    Some(auth)
}

fn fill_if_empty(slot: &mut String, value: &str) {
    if slot.is_empty() {
        *slot = value.to_string();
    }
}

/// `authDataHasValue`.
pub fn auth_data_has_value(d: &AuthData) -> bool {
    !d.provider.trim().is_empty()
        || !d.id.trim().is_empty()
        || !d.file_name.trim().is_empty()
        || !d.label.trim().is_empty()
        || !d.prefix.trim().is_empty()
        || !d.proxy_url.trim().is_empty()
        || d.disabled
        || !d.storage_json.is_empty()
        || !d.metadata.is_empty()
        || !d.attributes.is_empty()
        || d.next_refresh_after.is_some()
}

/// `authDataWithDefaults`: fills empty fields from `auth` and merges metadata/attributes.
pub fn auth_data_with_defaults(mut data: AuthData, auth: &Auth) -> AuthData {
    if data.provider.trim().is_empty() {
        data.provider = auth.provider.clone();
    }
    if data.id.trim().is_empty() {
        data.id = auth.id.clone();
    }
    if data.file_name.trim().is_empty() {
        data.file_name = auth.file_name.clone();
    }
    if data.label.trim().is_empty() {
        data.label = auth.label.clone();
    }
    if data.prefix.trim().is_empty() {
        data.prefix = auth.prefix.clone();
    }
    if data.proxy_url.trim().is_empty() {
        data.proxy_url = auth.proxy_url.clone();
    }
    if data.metadata.is_empty() {
        data.metadata = auth.metadata.clone();
    } else {
        for (k, v) in &auth.metadata {
            if !data.metadata.contains_key(k) {
                data.metadata.insert(k.clone(), v.clone());
            }
        }
    }
    if data.attributes.is_empty() {
        data.attributes = auth.attributes.clone();
    } else {
        for (k, v) in &auth.attributes {
            data.attributes.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    if data.storage_json.is_empty() {
        data.storage_json = storage_json_from_auth(Some(auth));
    }
    if data.next_refresh_after.is_none() {
        data.next_refresh_after = auth.next_refresh_after;
    }
    data
}

/// `preserveFileAuthPriority`: file-backed auths keep their path/source/priority bookkeeping.
pub fn preserve_file_auth_priority(data: &mut AuthData, auth: &Auth) {
    use cpa_auth::credmeta::ATTRIBUTE_FILE_PRIORITY;
    if auth.attributes.get(ATTRIBUTE_SOURCE_BACKEND).map(String::as_str) != Some(AUTH_SOURCE_FILE) {
        return;
    }
    for key in [ATTRIBUTE_PATH, ATTRIBUTE_SOURCE, ATTRIBUTE_SOURCE_BACKEND, ATTRIBUTE_FILE_PRIORITY] {
        if let Some(v) = auth.attributes.get(key) {
            data.attributes.insert(key.to_string(), v.clone());
        }
    }
    if auth.attributes.get(ATTRIBUTE_FILE_PRIORITY).map(String::as_str) != Some("true") {
        data.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
        return;
    }
    match auth.attributes.get("priority") {
        Some(p) => {
            data.attributes.insert("priority".into(), p.clone());
        }
        None => {
            data.attributes.remove("priority");
        }
    }
    match auth.metadata.get("priority") {
        Some(p) => {
            data.metadata.insert("priority".into(), p.clone());
        }
        None => {
            data.metadata.shift_remove("priority");
        }
    }
}

pub fn auth_attribute(auth: &Auth, key: &str) -> String {
    auth.attributes.get(key).cloned().unwrap_or_default()
}

/// `isRuntimeOnlyAuth`.
pub fn is_runtime_only_auth(auth: &Auth) -> bool {
    auth.attributes.get("runtime_only").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Go `strconv.ParseBool`.
pub fn parse_go_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// `parseBoolValue`: JSON bool or a string `strconv.ParseBool` accepts.
pub fn parse_bool_value(raw: Option<&Value>) -> Option<bool> {
    match raw? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => parse_go_bool(s.trim()),
        _ => None,
    }
}

/// `parsePriorityValue`: number or a string integer.
pub fn parse_priority_value(raw: &Value) -> Option<i64> {
    match raw {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}
