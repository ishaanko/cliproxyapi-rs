//! v0 client-key and provider-key lists (Go: `config_lists.go`, `config_auth_index.go`).
//!
//! Every list has GET (entries plus their live `auth-index`), PUT (replace), PATCH (edit one
//! entry) and DELETE. All writes go through [`persist`]: the live config is copied, edited,
//! sanitized like the loader does, saved with comments preserved and reloaded.

use std::collections::{BTreeMap, HashMap};

use axum::extract::State;
use axum::http::Uri;
use bytes::Bytes;
use cpa_config::{
    ClaudeKey, ClaudeModel, CloakConfig, CodexKey, CodexModel, Config, GeminiKey, OAuthModelAlias,
    OpenAiCompatibility, OpenAiCompatibilityApiKey, OpenAiCompatibilityModel,
    RequestScopedErrorRule, VertexCompatKey, VertexCompatModel, format_sorted_headers,
    normalize_cloak_config, normalize_excluded_models, normalize_headers,
};
use cpa_runtime::service::synth::StableIdGenerator;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::http::{ApiError, ApiResult, ok_json, query_get};
use crate::state::ManagementState;
use crate::v0_util::{first_json, nil_empty, persist, sscanf_int, yaml_snapshot};

// ---- decoding helpers ----

fn invalid_body() -> ApiError {
    ApiError::bad_request("invalid body")
}

/// Go: `json.Unmarshal(data, &arr)`, else `{"items": [...]}` which must be non-empty. `null`
/// decodes to an empty list.
fn items<T: DeserializeOwned>(body: &[u8]) -> ApiResult<Vec<T>> {
    if let Ok(v) = serde_json::from_slice::<Option<Vec<T>>>(body) {
        return Ok(v.unwrap_or_default());
    }
    #[derive(Deserialize)]
    struct Wrapper<T> {
        #[serde(default = "Vec::new")]
        items: Vec<T>,
    }
    match serde_json::from_slice::<Wrapper<T>>(body) {
        Ok(w) if !w.items.is_empty() => Ok(w.items),
        _ => Err(invalid_body()),
    }
}

/// Go: `c.ShouldBindJSON(&body)` into a struct.
fn bind<T: DeserializeOwned>(body: &[u8]) -> ApiResult<T> {
    first_json(body)
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(invalid_body)
}

/// Keeps a JSON `null` distinguishable from an absent key (Go: `json.RawMessage`).
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

fn trim(s: &str) -> String {
    s.trim().to_string()
}

/// Go: `rejectInvalidCredentialWeight`.
fn reject_weight(field: &str, weight: Option<i64>) -> ApiResult<()> {
    cpa_config::validate_credential_weight(weight)
        .map_err(|e| ApiError::bad_request(format!("{field}: {e}")))
}

/// Go: `rejectInvalidFingerprintProfile`.
fn reject_fingerprint(field: &str, profile: &str) -> ApiResult<()> {
    cpa_config::validate_claude_fingerprint_profile(profile)
        .map_err(|e| ApiError::bad_request(format!("{field}: {e}")))
}

/// Go: `parseCredentialWeightPatch`. `null` clears the weight.
fn parse_weight_patch(raw: &Value) -> ApiResult<Option<i64>> {
    if raw.is_null() {
        return Ok(None);
    }
    let Some(weight) = raw.as_i64() else {
        return Err(ApiError::bad_request("weight must be an integer"));
    };
    cpa_config::validate_credential_weight(Some(weight))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Some(weight))
}

/// Go: `applyDisableCoolingPatch` / `applyDisableCodexCloakingPatch`.
fn apply_bool_patch(raw: &Option<Value>, target: &mut Option<bool>, name: &str) -> ApiResult<()> {
    match raw {
        None => Ok(()),
        Some(Value::Null) => {
            *target = None;
            Ok(())
        }
        Some(Value::Bool(b)) => {
            *target = Some(*b);
            Ok(())
        }
        Some(_) => Err(ApiError::bad_request(format!(
            "{name} must be a boolean or null"
        ))),
    }
}

fn decode<T: DeserializeOwned>(v: &Option<Value>) -> ApiResult<Option<T>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|_| invalid_body()),
    }
}

/// Union of the `value` objects the key PATCH endpoints accept; each endpoint reads its own subset.
#[derive(Deserialize, Default)]
#[serde(default)]
struct KeyPatch {
    #[serde(rename = "api-key")]
    api_key: Option<String>,
    priority: Option<i64>,
    #[serde(deserialize_with = "present")]
    weight: Option<Value>,
    prefix: Option<String>,
    #[serde(rename = "base-url")]
    base_url: Option<String>,
    #[serde(rename = "proxy-url")]
    proxy_url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    #[serde(rename = "excluded-models")]
    excluded_models: Option<Vec<String>>,
    #[serde(rename = "disable-cooling", deserialize_with = "present")]
    disable_cooling: Option<Value>,
    #[serde(rename = "disable-codex-cloaking", deserialize_with = "present")]
    disable_codex_cloaking: Option<Value>,
    #[serde(rename = "request-retry")]
    request_retry: Option<i64>,
    #[serde(rename = "request-scoped-errors")]
    request_scoped_errors: Option<Vec<RequestScopedErrorRule>>,
    models: Option<Value>,
    #[serde(rename = "fingerprint-profile")]
    fingerprint_profile: Option<String>,
    #[serde(rename = "rebuild-mid-system-message")]
    rebuild_mid_system_message: Option<bool>,
    #[serde(deserialize_with = "present")]
    cloak: Option<Value>,
    #[serde(rename = "alpha-search")]
    alpha_search: Option<bool>,
    websockets: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct KeyPatchBody {
    index: Option<i64>,
    #[serde(rename = "match")]
    match_: Option<String>,
    value: Option<KeyPatch>,
}

impl KeyPatchBody {
    /// Go: `ShouldBindJSON` plus the `body.Value == nil` check.
    fn parse(body: &[u8]) -> ApiResult<(Option<i64>, Option<String>, KeyPatch)> {
        let b: KeyPatchBody = bind(body)?;
        let value = b.value.ok_or_else(invalid_body)?;
        Ok((b.index, b.match_, value))
    }
}

/// `body.Index` if it points into a list of `len` entries.
fn index_in(index: Option<i64>, len: usize) -> Option<usize> {
    index
        .filter(|i| *i >= 0 && (*i as usize) < len)
        .map(|i| i as usize)
}

/// Go: `fmt.Sscanf(query "index", "%d")` in range.
fn query_index(uri: &Uri, len: usize) -> Option<usize> {
    let raw = query_get(uri, "index").unwrap_or_default();
    if raw.is_empty() {
        return None;
    }
    sscanf_int(&raw)
        .filter(|i| *i >= 0 && (*i as usize) < len)
        .map(|i| i as usize)
}

fn not_found() -> ApiError {
    ApiError::new(404, "item not found")
}

// ---- GET with auth-index ----

/// Credential id -> its `auth-index` for every live credential (Go: `liveAuthIndexByID`).
fn live_index_by_id(st: &ManagementState) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for mut auth in st.registry.list() {
        let id = auth.id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        let mut idx = auth.index.trim().to_string();
        if idx.is_empty() {
            idx = auth.ensure_index();
        }
        if !idx.is_empty() {
            out.insert(id, idx);
        }
    }
    out
}

fn entry_value<T: Serialize>(entry: &T, auth_index: &str) -> Value {
    let mut v = serde_json::to_value(entry).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v
        && !auth_index.is_empty()
    {
        map.insert("auth-index".into(), auth_index.into());
    }
    v
}

/// `{ <key>: [entry + auth-index, ...] }`. The list itself is never null; nested empty lists
/// follow [`nil_empty`].
async fn list_response<T: Serialize>(
    st: &ManagementState,
    key: &str,
    entries: &[T],
    kind: &str,
    parts: impl Fn(&T) -> Option<Vec<String>>,
) -> ApiResult {
    let index_by_id = live_index_by_id(st);
    let yaml = yaml_snapshot(st).await;
    let yaml_list = yaml.as_ref().and_then(|y| y.get(key));
    let mut ids = StableIdGenerator::new();
    let out: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let auth_index = parts(entry)
                .map(|p| {
                    let refs: Vec<&str> = p.iter().map(String::as_str).collect();
                    let (id, _) = ids.next(kind, &refs);
                    index_by_id.get(&id).cloned().unwrap_or_default()
                })
                .unwrap_or_default();
            let mut v = entry_value(entry, &auth_index);
            nil_empty(&mut v, yaml_list.and_then(|l| l.get(i)));
            v
        })
        .collect();
    Ok(ok_json(&json!({ key: out })))
}

fn key_parts(key: &str, base: &str, proxy: &str, prefix: &str, headers: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let (key, base) = (key.trim(), base.trim());
    (!key.is_empty() || !base.is_empty()).then(|| {
        vec![
            key.to_string(),
            base.to_string(),
            proxy.trim().to_string(),
            prefix.trim().to_string(),
            format_sorted_headers(headers),
        ]
    })
}

// ---- api-keys ----

pub(crate) async fn get_api_keys(State(st): State<ManagementState>) -> ApiResult {
    let mut v = json!({"api-keys": st.cfg().api_keys});
    let yaml = yaml_snapshot(&st).await;
    nil_empty(&mut v, yaml.as_ref());
    Ok(ok_json(&v))
}

/// Go: `putStringList`.
fn string_list(body: &[u8]) -> ApiResult<Vec<String>> {
    if let Ok(v) = serde_json::from_slice::<Option<Vec<String>>>(body) {
        return Ok(v.unwrap_or_default());
    }
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        items: Vec<String>,
    }
    match serde_json::from_slice::<Wrapper>(body) {
        Ok(w) if !w.items.is_empty() => Ok(w.items),
        _ => Err(invalid_body()),
    }
}

pub(crate) async fn put_api_keys(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let arr = string_list(&body)?;
    persist(&st, move |c| {
        c.api_keys = arr;
        Ok(())
    })
    .await
}

/// Go: `patchStringList`.
pub(crate) async fn patch_api_keys(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        old: Option<String>,
        new: Option<String>,
        index: Option<i64>,
        value: Option<String>,
    }
    let b: Body = bind(&body)?;
    persist(&st, move |c| {
        let target = &mut c.api_keys;
        if let (Some(i), Some(v)) = (index_in(b.index, target.len()), &b.value) {
            target[i] = v.clone();
            return Ok(());
        }
        if let (Some(old), Some(new)) = (&b.old, &b.new) {
            match target.iter_mut().find(|k| *k == old) {
                Some(slot) => *slot = new.clone(),
                None => target.push(new.clone()),
            }
            return Ok(());
        }
        Err(ApiError::bad_request("missing fields"))
    })
    .await
}

/// Go: `deleteFromStringList`.
pub(crate) async fn delete_api_keys(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    persist(&st, move |c| {
        let target = &mut c.api_keys;
        if let Some(i) = query_index(&uri, target.len()) {
            target.remove(i);
            return Ok(());
        }
        let val = query_get(&uri, "value").unwrap_or_default();
        let val = val.trim();
        if !val.is_empty() {
            target.retain(|v| v.trim() != val);
            return Ok(());
        }
        Err(ApiError::bad_request("missing index or value"))
    })
    .await
}

// ---- gemini / interactions ----

fn normalize_gemini_target(entry: &mut GeminiKey, p: &KeyPatch) -> ApiResult<()> {
    if let Some(v) = &p.api_key {
        entry.api_key = trim(v);
    }
    if let Some(v) = p.priority {
        entry.priority = v;
    }
    if let Some(w) = &p.weight {
        entry.weight = parse_weight_patch(w)?;
    }
    if let Some(v) = &p.prefix {
        entry.prefix = trim(v);
    }
    if let Some(v) = &p.base_url {
        entry.base_url = trim(v);
    }
    if let Some(v) = &p.proxy_url {
        entry.proxy_url = trim(v);
    }
    if let Some(h) = &p.headers {
        entry.headers = normalize_headers(h);
    }
    if let Some(m) = &p.excluded_models {
        entry.excluded_models = normalize_excluded_models(m);
    }
    apply_bool_patch(&p.disable_cooling, &mut entry.disable_cooling, "disable-cooling")?;
    if let Some(v) = p.request_retry {
        entry.request_retry = Some(v);
    }
    if let Some(v) = &p.request_scoped_errors {
        entry.request_scoped_errors = v.clone();
    }
    Ok(())
}

/// Shared by gemini-api-key and interactions-api-key (same entry type and rules).
macro_rules! gemini_like {
    ($field:ident, $sanitize:ident, $label:literal, $kind:literal, $get:ident, $put:ident, $patch:ident, $delete:ident) => {
        pub(crate) async fn $get(State(st): State<ManagementState>) -> ApiResult {
            let entries = st.cfg().$field.clone();
            list_response(&st, $label, &entries, $kind, |e: &GeminiKey| {
                key_parts(&e.api_key, &e.base_url, &e.proxy_url, &e.prefix, &e.headers)
            })
            .await
        }

        pub(crate) async fn $put(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
            let arr: Vec<GeminiKey> = items(&body)?;
            for (i, k) in arr.iter().enumerate() {
                reject_weight(&format!(concat!($label, "[{}].weight"), i), k.weight)?;
            }
            persist(&st, move |c| {
                c.$field = arr;
                c.$sanitize();
                Ok(())
            })
            .await
        }

        pub(crate) async fn $patch(
            State(st): State<ManagementState>,
            uri: Uri,
            body: Bytes,
        ) -> ApiResult {
            let (index, match_, patch) = KeyPatchBody::parse(&body)?;
            persist(&st, move |c| {
                let list = &mut c.$field;
                let mut target = index_in(index, list.len());
                if target.is_none()
                    && let Some(m) = &match_
                {
                    let m = m.trim();
                    if !m.is_empty() {
                        let base = query_get(&uri, "base-url").map(|b| trim(&b));
                        let matches: Vec<usize> = list
                            .iter()
                            .enumerate()
                            .filter(|(_, e)| {
                                e.api_key.trim() == m
                                    && base.as_deref().is_none_or(|b| e.base_url.trim() == b)
                            })
                            .map(|(i, _)| i)
                            .collect();
                        if matches.len() > 1 {
                            return Err(ApiError::bad_request(
                                "multiple items match; index is required",
                            ));
                        }
                        target = matches.first().copied();
                    }
                }
                let Some(target) = target else {
                    return Err(not_found());
                };
                let mut entry = list[target].clone();
                normalize_gemini_target(&mut entry, &patch)?;
                if entry.api_key.is_empty() && entry.base_url.is_empty() {
                    list.remove(target);
                } else {
                    list[target] = entry;
                }
                c.$sanitize();
                Ok(())
            })
            .await
        }

        pub(crate) async fn $delete(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
            persist(&st, move |c| {
                let list = &mut c.$field;
                let val = crate::v0_util::q_trim(&uri, "api-key");
                if !val.is_empty() {
                    let base = query_get(&uri, "base-url").map(|b| trim(&b));
                    let matches: Vec<usize> = list
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| {
                            e.api_key.trim() == val
                                && base.as_deref().is_none_or(|b| e.base_url.trim() == b)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if matches.is_empty() {
                        return Err(not_found());
                    }
                    if matches.len() > 1 {
                        return Err(ApiError::bad_request(if base.is_some() {
                            "multiple items match; index is required"
                        } else {
                            "multiple items match api-key; base-url is required"
                        }));
                    }
                    list.remove(matches[0]);
                    c.$sanitize();
                    return Ok(());
                }
                if let Some(i) = query_index(&uri, list.len()) {
                    list.remove(i);
                    c.$sanitize();
                    return Ok(());
                }
                Err(ApiError::bad_request("missing api-key or index"))
            })
            .await
        }
    };
}

gemini_like!(
    gemini_key,
    sanitize_gemini_keys,
    "gemini-api-key",
    "gemini:apikey",
    get_gemini_keys,
    put_gemini_keys,
    patch_gemini_key,
    delete_gemini_key
);
gemini_like!(
    interactions_key,
    sanitize_interactions_keys,
    "interactions-api-key",
    "gemini-interactions:apikey",
    get_interactions_keys,
    put_interactions_keys,
    patch_interactions_key,
    delete_interactions_key
);

// ---- claude ----

fn normalize_claude_key(entry: &mut ClaudeKey) {
    entry.api_key = trim(&entry.api_key);
    entry.fingerprint_profile = match cpa_config::normalize_claude_fingerprint_profile(&entry.fingerprint_profile) {
        (normalized, true) => normalized.to_string(),
        _ => trim(&entry.fingerprint_profile),
    };
    entry.base_url = trim(&entry.base_url);
    entry.proxy_url = trim(&entry.proxy_url);
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    if let Some(cloak) = &mut entry.cloak {
        normalize_cloak_config(cloak);
    }
    if entry.models.is_empty() {
        return;
    }
    entry.models = std::mem::take(&mut entry.models)
        .into_iter()
        .filter_map(|mut m| {
            m.name = trim(&m.name);
            m.alias = trim(&m.alias);
            (!(m.name.is_empty() && m.alias.is_empty())).then_some(m)
        })
        .collect();
}

/// Go: `findExistingClaudeKey` (exact identity tuple, exactly one match).
fn find_existing_claude<'a>(existing: &'a [ClaudeKey], item: &ClaudeKey) -> Option<&'a ClaudeKey> {
    let tuple = |k: &ClaudeKey| (trim(&k.api_key), trim(&k.base_url), trim(&k.prefix), trim(&k.proxy_url));
    let want = tuple(item);
    let mut matches = existing.iter().filter(|k| tuple(k) == want);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

pub(crate) async fn get_claude_keys(State(st): State<ManagementState>) -> ApiResult {
    let entries = st.cfg().claude_key.clone();
    list_response(&st, "claude-api-key", &entries, "claude:apikey", |e: &ClaudeKey| {
        key_parts(&e.api_key, &e.base_url, &e.proxy_url, &e.prefix, &e.headers)
    })
    .await
}

pub(crate) async fn put_claude_keys(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let mut arr: Vec<ClaudeKey> = items(&body)?;
    persist(&st, move |c| {
        for (i, entry) in arr.iter_mut().enumerate() {
            let old_mode = find_existing_claude(&c.claude_key, entry)
                .and_then(|o| o.cloak.as_ref())
                .map(|cl| cl.mode.clone())
                .filter(|m| !m.is_empty());
            match &mut entry.cloak {
                None => {
                    if let Some(mode) = old_mode {
                        entry.cloak = Some(CloakConfig {
                            mode,
                            ..CloakConfig::default()
                        });
                    }
                }
                Some(cloak) if cloak.mode.trim().is_empty() => {
                    if let Some(mode) = old_mode {
                        cloak.mode = mode;
                    }
                }
                Some(_) => {}
            }
            normalize_claude_key(entry);
            reject_weight(&format!("claude-api-key[{i}].weight"), entry.weight)?;
            reject_fingerprint(
                &format!("claude-api-key[{i}].fingerprint-profile"),
                &entry.fingerprint_profile,
            )?;
        }
        c.claude_key = arr;
        c.sanitize_claude_keys();
        Ok(())
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CloakPatch {
    mode: Option<String>,
    #[serde(rename = "strict-mode")]
    strict_mode: Option<bool>,
    #[serde(rename = "sensitive-words")]
    sensitive_words: Option<Vec<String>>,
    #[serde(rename = "cache-user-id")]
    cache_user_id: Option<bool>,
}

pub(crate) async fn patch_claude_key(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let (index, match_, p) = KeyPatchBody::parse(&body)?;
    let models: Option<Vec<ClaudeModel>> = decode(&p.models)?;
    persist(&st, move |c| {
        let list = &mut c.claude_key;
        let mut target = index_in(index, list.len());
        if target.is_none()
            && let Some(m) = &match_
        {
            let m = m.trim();
            target = list.iter().position(|e| e.api_key == m);
        }
        let Some(target) = target else {
            return Err(not_found());
        };
        let old = list[target].clone();
        let mut entry = old.clone();
        let identity_changed = p.api_key.as_deref().is_some_and(|v| v.trim() != old.api_key)
            || p.base_url.as_deref().is_some_and(|v| v.trim() != old.base_url)
            || p.prefix.as_deref().is_some_and(|v| v.trim() != old.prefix)
            || p.proxy_url.as_deref().is_some_and(|v| v.trim() != old.proxy_url);
        if let Some(v) = &p.api_key {
            entry.api_key = trim(v);
        }
        if let Some(v) = p.priority {
            entry.priority = v;
        }
        if let Some(v) = &p.fingerprint_profile {
            reject_fingerprint("fingerprint-profile", v)?;
            entry.fingerprint_profile = cpa_config::normalize_claude_fingerprint_profile(v).0.to_string();
        }
        if let Some(w) = &p.weight {
            entry.weight = parse_weight_patch(w)?;
        }
        if let Some(v) = &p.prefix {
            entry.prefix = trim(v);
        }
        if let Some(v) = &p.base_url {
            entry.base_url = trim(v);
        }
        if let Some(v) = &p.proxy_url {
            entry.proxy_url = trim(v);
        }
        if let Some(m) = &models {
            entry.models = m.clone();
        }
        if let Some(h) = &p.headers {
            entry.headers = normalize_headers(h);
        }
        if let Some(m) = &p.excluded_models {
            entry.excluded_models = normalize_excluded_models(m);
        }
        if let Some(v) = p.rebuild_mid_system_message {
            entry.rebuild_mid_system_message = v;
        }
        apply_bool_patch(&p.disable_cooling, &mut entry.disable_cooling, "disable-cooling")?;
        if let Some(v) = p.request_retry {
            entry.request_retry = Some(v);
        }
        if let Some(v) = &p.request_scoped_errors {
            entry.request_scoped_errors = v.clone();
        }
        match &p.cloak {
            Some(Value::Null) => entry.cloak = None,
            Some(raw) => {
                let cp: CloakPatch = serde_json::from_value(raw.clone())
                    .map_err(|_| ApiError::bad_request("invalid cloak config"))?;
                let mut cloak = match (&entry.cloak, identity_changed) {
                    (Some(existing), false) => existing.clone(),
                    _ => CloakConfig::default(),
                };
                if let Some(mode) = &cp.mode {
                    let mode = trim(mode);
                    if mode.is_empty() && !identity_changed && old.cloak.is_some() {
                        cloak.mode = old.cloak.as_ref().map(|o| o.mode.clone()).unwrap_or_default();
                    } else {
                        cloak.mode = mode;
                    }
                } else if identity_changed {
                    cloak.mode.clear();
                }
                if let Some(v) = cp.strict_mode {
                    cloak.strict_mode = v;
                }
                if let Some(v) = cp.sensitive_words {
                    cloak.sensitive_words = v;
                }
                if let Some(v) = cp.cache_user_id {
                    cloak.cache_user_id = Some(v);
                }
                normalize_cloak_config(&mut cloak);
                entry.cloak = Some(cloak);
            }
            None => {
                if identity_changed && let Some(cloak) = &mut entry.cloak {
                    cloak.mode.clear();
                    normalize_cloak_config(cloak);
                }
            }
        }
        normalize_claude_key(&mut entry);
        list[target] = entry;
        c.sanitize_claude_keys();
        Ok(())
    })
    .await
}

/// DELETE of the api-key keyed lists (claude, codex, xai, meta, vertex).
macro_rules! delete_by_api_key {
    ($uri:expr, $c:expr, $field:ident, $sanitize:ident) => {{
        let list = &mut $c.$field;
        let val = crate::v0_util::q_trim($uri, "api-key");
        if !val.is_empty() {
            if let Some(base) = query_get($uri, "base-url") {
                let base = trim(&base);
                list.retain(|e| !(e.api_key.trim() == val && e.base_url.trim() == base));
                $c.$sanitize();
                return Ok(());
            }
            let matches: Vec<usize> = list
                .iter()
                .enumerate()
                .filter(|(_, e)| e.api_key.trim() == val)
                .map(|(i, _)| i)
                .collect();
            if matches.len() > 1 {
                return Err(ApiError::bad_request(
                    "multiple items match api-key; base-url is required",
                ));
            }
            if let Some(i) = matches.first() {
                list.remove(*i);
            }
            $c.$sanitize();
            return Ok(());
        }
        if let Some(i) = query_index($uri, list.len()) {
            list.remove(i);
            $c.$sanitize();
            return Ok(());
        }
        Err(ApiError::bad_request("missing api-key or index"))
    }};
}

pub(crate) async fn delete_claude_key(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    persist(&st, move |c| {
        delete_by_api_key!(&uri, c, claude_key, sanitize_claude_keys)
    })
    .await
}

// ---- codex / xai / meta ----

fn normalize_codex_models(models: &mut Vec<CodexModel>) {
    if models.is_empty() {
        return;
    }
    *models = std::mem::take(models)
        .into_iter()
        .filter_map(|mut m| {
            m.name = trim(&m.name);
            m.alias = trim(&m.alias);
            (!(m.name.is_empty() && m.alias.is_empty())).then_some(m)
        })
        .collect();
}

/// Go: `normalizeCodexKey` (also used for xai and meta entries).
fn normalize_codex_key(entry: &mut CodexKey) {
    entry.api_key = trim(&entry.api_key);
    entry.prefix = trim(&entry.prefix);
    entry.base_url = trim(&entry.base_url);
    entry.proxy_url = trim(&entry.proxy_url);
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    normalize_codex_models(&mut entry.models);
}

const META_DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";

fn codex_parts(e: &CodexKey) -> Option<Vec<String>> {
    key_parts(&e.api_key, &e.base_url, &e.proxy_url, &e.prefix, &e.headers)
}

/// PUT body of codex/xai/meta: normalized entries; `drop_empty_base` removes entries without a
/// base URL (meta defaults it instead). Weights are validated against the original position.
fn codex_put_entries(
    body: &[u8],
    label: &str,
    drop_empty_base: bool,
) -> ApiResult<Vec<CodexKey>> {
    let arr: Vec<CodexKey> = items(body)?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, mut entry) in arr.into_iter().enumerate() {
        normalize_codex_key(&mut entry);
        if entry.base_url.is_empty() {
            if drop_empty_base {
                continue;
            }
            entry.base_url = META_DEFAULT_BASE_URL.into();
        }
        reject_weight(&format!("{label}[{i}].weight"), entry.weight)?;
        out.push(entry);
    }
    Ok(out)
}

/// Which codex-shaped provider a handler edits.
#[derive(Clone, Copy, PartialEq)]
enum CodexFlavor {
    Codex,
    Xai,
    Meta,
}

fn codex_patch_apply(
    entry: &mut CodexKey,
    p: &KeyPatch,
    models: &Option<Vec<CodexModel>>,
    flavor: CodexFlavor,
) -> ApiResult<bool> {
    if let Some(v) = &p.api_key {
        entry.api_key = trim(v);
    }
    if let Some(v) = p.priority {
        entry.priority = v;
    }
    if let Some(w) = &p.weight {
        entry.weight = parse_weight_patch(w)?;
    }
    if let Some(v) = &p.prefix {
        entry.prefix = trim(v);
    }
    if let Some(v) = &p.base_url {
        let trimmed = trim(v);
        if trimmed.is_empty() {
            if flavor != CodexFlavor::Meta {
                return Ok(true);
            }
            entry.base_url = META_DEFAULT_BASE_URL.into();
        } else {
            entry.base_url = trimmed;
        }
    }
    if flavor == CodexFlavor::Xai
        && let Some(v) = p.websockets
    {
        entry.websockets = v;
    }
    if let Some(v) = &p.proxy_url {
        entry.proxy_url = trim(v);
    }
    if flavor == CodexFlavor::Codex
        && let Some(v) = p.alpha_search
    {
        entry.alpha_search = v;
    }
    if let Some(m) = models {
        entry.models = m.clone();
    }
    if let Some(h) = &p.headers {
        entry.headers = normalize_headers(h);
    }
    if let Some(m) = &p.excluded_models {
        entry.excluded_models = normalize_excluded_models(m);
    }
    apply_bool_patch(&p.disable_cooling, &mut entry.disable_cooling, "disable-cooling")?;
    if flavor == CodexFlavor::Codex {
        apply_bool_patch(
            &p.disable_codex_cloaking,
            &mut entry.disable_codex_cloaking,
            "disable-codex-cloaking",
        )?;
    }
    if let Some(v) = p.request_retry {
        entry.request_retry = Some(v);
    }
    if let Some(v) = &p.request_scoped_errors {
        entry.request_scoped_errors = v.clone();
    }
    normalize_codex_key(entry);
    Ok(false)
}

macro_rules! codex_like {
    ($field:ident, $sanitize:ident, $label:literal, $kind:literal, $flavor:expr, $get:ident, $put:ident, $patch:ident, $delete:ident) => {
        pub(crate) async fn $get(State(st): State<ManagementState>) -> ApiResult {
            let entries = st.cfg().$field.clone();
            list_response(&st, $label, &entries, $kind, codex_parts).await
        }

        pub(crate) async fn $put(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
            let arr = codex_put_entries(&body, $label, $flavor != CodexFlavor::Meta)?;
            persist(&st, move |c| {
                c.$field = arr;
                c.$sanitize();
                Ok(())
            })
            .await
        }

        pub(crate) async fn $patch(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
            let (index, match_, p) = KeyPatchBody::parse(&body)?;
            let models: Option<Vec<CodexModel>> = decode(&p.models)?;
            persist(&st, move |c| {
                let list = &mut c.$field;
                let mut target = index_in(index, list.len());
                if target.is_none()
                    && let Some(m) = &match_
                {
                    let m = m.trim();
                    target = list.iter().position(|e| e.api_key == m);
                }
                let Some(target) = target else {
                    return Err(not_found());
                };
                let mut entry = list[target].clone();
                if codex_patch_apply(&mut entry, &p, &models, $flavor)? {
                    list.remove(target);
                } else {
                    list[target] = entry;
                }
                c.$sanitize();
                Ok(())
            })
            .await
        }

        pub(crate) async fn $delete(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
            persist(&st, move |c| delete_by_api_key!(&uri, c, $field, $sanitize)).await
        }
    };
}

codex_like!(
    codex_key,
    sanitize_codex_keys,
    "codex-api-key",
    "codex:apikey",
    CodexFlavor::Codex,
    get_codex_keys,
    put_codex_keys,
    patch_codex_key,
    delete_codex_key
);
codex_like!(
    xai_key,
    sanitize_xai_keys,
    "xai-api-key",
    "xai:apikey",
    CodexFlavor::Xai,
    get_xai_keys,
    put_xai_keys,
    patch_xai_key,
    delete_xai_key
);
codex_like!(
    meta_key,
    sanitize_meta_keys,
    "meta-api-key",
    "meta:apikey",
    CodexFlavor::Meta,
    get_meta_keys,
    put_meta_keys,
    patch_meta_key,
    delete_meta_key
);

// ---- vertex ----

fn normalize_vertex_key(entry: &mut VertexCompatKey) {
    entry.api_key = trim(&entry.api_key);
    entry.prefix = trim(&entry.prefix);
    entry.base_url = trim(&entry.base_url);
    entry.proxy_url = trim(&entry.proxy_url);
    entry.headers = normalize_headers(&entry.headers);
    entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
    if entry.models.is_empty() {
        return;
    }
    entry.models = std::mem::take(&mut entry.models)
        .into_iter()
        .filter_map(|mut m| {
            m.name = trim(&m.name);
            m.alias = trim(&m.alias);
            (!(m.name.is_empty() || m.alias.is_empty())).then_some(m)
        })
        .collect();
}

pub(crate) async fn get_vertex_keys(State(st): State<ManagementState>) -> ApiResult {
    let entries = st.cfg().vertex_compat_api_key.clone();
    list_response(&st, "vertex-api-key", &entries, "vertex:apikey", |e: &VertexCompatKey| {
        Some(vec![e.api_key.clone(), e.base_url.clone(), e.proxy_url.clone()])
    })
    .await
}

pub(crate) async fn put_vertex_keys(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let mut arr: Vec<VertexCompatKey> = items(&body)?;
    for (i, entry) in arr.iter_mut().enumerate() {
        normalize_vertex_key(entry);
        if entry.api_key.is_empty() {
            return Err(ApiError::bad_request(format!(
                "vertex-api-key[{i}].api-key is required"
            )));
        }
        reject_weight(&format!("vertex-api-key[{i}].weight"), entry.weight)?;
    }
    persist(&st, move |c| {
        c.vertex_compat_api_key = arr;
        c.sanitize_vertex_compat_keys();
        Ok(())
    })
    .await
}

pub(crate) async fn patch_vertex_key(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let (index, match_, p) = KeyPatchBody::parse(&body)?;
    let models: Option<Vec<VertexCompatModel>> = decode(&p.models)?;
    persist(&st, move |c| {
        let list = &mut c.vertex_compat_api_key;
        let mut target = index_in(index, list.len());
        if target.is_none()
            && let Some(m) = &match_
        {
            let m = m.trim();
            if !m.is_empty() {
                target = list.iter().position(|e| e.api_key == m);
            }
        }
        let Some(target) = target else {
            return Err(not_found());
        };
        let mut entry = list[target].clone();
        if let Some(v) = &p.api_key {
            let trimmed = trim(v);
            if trimmed.is_empty() {
                list.remove(target);
                c.sanitize_vertex_compat_keys();
                return Ok(());
            }
            entry.api_key = trimmed;
        }
        if let Some(v) = p.priority {
            entry.priority = v;
        }
        if let Some(w) = &p.weight {
            entry.weight = parse_weight_patch(w)?;
        }
        if let Some(v) = &p.prefix {
            entry.prefix = trim(v);
        }
        if let Some(v) = &p.base_url {
            entry.base_url = trim(v);
        }
        if let Some(v) = &p.proxy_url {
            entry.proxy_url = trim(v);
        }
        if let Some(h) = &p.headers {
            entry.headers = normalize_headers(h);
        }
        if let Some(m) = &models {
            entry.models = m.clone();
        }
        if let Some(m) = &p.excluded_models {
            entry.excluded_models = normalize_excluded_models(m);
        }
        apply_bool_patch(&p.disable_cooling, &mut entry.disable_cooling, "disable-cooling")?;
        if let Some(v) = p.request_retry {
            entry.request_retry = Some(v);
        }
        normalize_vertex_key(&mut entry);
        list[target] = entry;
        c.sanitize_vertex_compat_keys();
        Ok(())
    })
    .await
}

pub(crate) async fn delete_vertex_key(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    persist(&st, move |c| {
        delete_by_api_key!(&uri, c, vertex_compat_api_key, sanitize_vertex_compat_keys)
    })
    .await
}

// ---- openai-compatibility ----

fn normalize_openai_entry(entry: &mut OpenAiCompatibility) {
    entry.base_url = trim(&entry.base_url);
    entry.headers = normalize_headers(&entry.headers);
    for k in &mut entry.api_key_entries {
        k.api_key = trim(&k.api_key);
    }
}

/// `GET /openai-compatibility`: Go's `openAICompatibilityWithAuthIndex` response shape.
pub(crate) async fn get_openai_compat(State(st): State<ManagementState>) -> ApiResult {
    let index_by_id = live_index_by_id(&st);
    let yaml = yaml_snapshot(&st).await;
    let yaml_list = yaml.as_ref().and_then(|y| y.get("openai-compatibility"));
    let mut ids = StableIdGenerator::new();
    let mut out = Vec::new();
    for (i, entry) in st.cfg().openai_compatibility.iter().enumerate() {
        let mut entry = entry.clone();
        normalize_openai_entry(&mut entry);
        let mut name = entry.name.trim().to_lowercase();
        if name.is_empty() {
            name = "openai-compatibility".into();
        }
        let kind = format!("openai-compatibility:{name}");
        let lookup = |ids: &mut StableIdGenerator, parts: &[&str]| {
            let (id, _) = ids.next(&kind, parts);
            index_by_id.get(&id).cloned().unwrap_or_default()
        };
        let mut m = Map::new();
        m.insert("name".into(), entry.name.clone().into());
        if entry.priority != 0 {
            m.insert("priority".into(), entry.priority.into());
        }
        m.insert("disabled".into(), entry.disabled.into());
        if !entry.prefix.is_empty() {
            m.insert("prefix".into(), entry.prefix.clone().into());
        }
        m.insert("base-url".into(), entry.base_url.clone().into());
        let mut auth_index = String::new();
        if entry.api_key_entries.is_empty() {
            auth_index = lookup(&mut ids, &[entry.base_url.as_str()]);
        } else {
            let mut keys = Vec::new();
            for k in &entry.api_key_entries {
                let idx = lookup(&mut ids, &[k.api_key.as_str(), entry.base_url.as_str(), k.proxy_url.as_str()]);
                keys.push(entry_value(k, &idx));
            }
            m.insert("api-key-entries".into(), Value::Array(keys));
        }
        if !entry.models.is_empty() {
            m.insert("models".into(), serde_json::to_value(&entry.models).unwrap_or(Value::Null));
        }
        if !entry.headers.is_empty() {
            m.insert("headers".into(), serde_json::to_value(&entry.headers).unwrap_or(Value::Null));
        }
        if entry.support_prompt_cache_key {
            m.insert("support-prompt-cache-key".into(), true.into());
        }
        if let Some(v) = entry.disable_cooling {
            m.insert("disable-cooling".into(), v.into());
        }
        if let Some(v) = entry.request_retry {
            m.insert("request-retry".into(), v.into());
        }
        if !entry.request_scoped_errors.is_empty() {
            m.insert(
                "request-scoped-errors".into(),
                serde_json::to_value(&entry.request_scoped_errors).unwrap_or(Value::Null),
            );
        }
        if !auth_index.is_empty() {
            m.insert("auth-index".into(), auth_index.into());
        }
        let mut v = Value::Object(m);
        nil_empty(&mut v, yaml_list.and_then(|l| l.get(i)));
        out.push(v);
    }
    Ok(ok_json(&json!({"openai-compatibility": out})))
}

pub(crate) async fn put_openai_compat(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let arr: Vec<OpenAiCompatibility> = items(&body)?;
    let mut filtered = Vec::with_capacity(arr.len());
    for (i, mut entry) in arr.into_iter().enumerate() {
        normalize_openai_entry(&mut entry);
        if entry.base_url.trim().is_empty() {
            continue;
        }
        for (k, key) in entry.api_key_entries.iter().enumerate() {
            reject_weight(
                &format!("openai-compatibility[{i}].api-key-entries[{k}].weight"),
                key.weight,
            )?;
        }
        filtered.push(entry);
    }
    persist(&st, move |c| {
        c.openai_compatibility = filtered;
        c.sanitize_openai_compatibility();
        Ok(())
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpenAiPatch {
    name: Option<String>,
    priority: Option<i64>,
    prefix: Option<String>,
    disabled: Option<bool>,
    #[serde(rename = "disable-cooling", deserialize_with = "present")]
    disable_cooling: Option<Value>,
    #[serde(rename = "base-url")]
    base_url: Option<String>,
    #[serde(rename = "api-key-entries")]
    api_key_entries: Option<Vec<OpenAiCompatibilityApiKey>>,
    models: Option<Vec<OpenAiCompatibilityModel>>,
    headers: Option<BTreeMap<String, String>>,
    #[serde(rename = "support-prompt-cache-key")]
    support_prompt_cache_key: Option<bool>,
    #[serde(rename = "request-retry")]
    request_retry: Option<i64>,
    #[serde(rename = "request-scoped-errors")]
    request_scoped_errors: Option<Vec<RequestScopedErrorRule>>,
}

pub(crate) async fn patch_openai_compat(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        name: Option<String>,
        index: Option<i64>,
        value: Option<OpenAiPatch>,
    }
    let b: Body = bind(&body)?;
    let p = b.value.ok_or_else(invalid_body)?;
    persist(&st, move |c| {
        let list = &mut c.openai_compatibility;
        let mut target = index_in(b.index, list.len());
        if target.is_none()
            && let Some(name) = &b.name
        {
            let m = name.trim();
            target = list.iter().position(|e| e.name == m);
        }
        let Some(target) = target else {
            return Err(not_found());
        };
        let mut entry = list[target].clone();
        if let Some(v) = &p.name {
            entry.name = trim(v);
        }
        if let Some(v) = p.priority {
            entry.priority = v;
        }
        if let Some(v) = &p.prefix {
            entry.prefix = trim(v);
        }
        if let Some(v) = p.disabled {
            entry.disabled = v;
        }
        apply_bool_patch(&p.disable_cooling, &mut entry.disable_cooling, "disable-cooling")?;
        if let Some(v) = p.request_retry {
            entry.request_retry = Some(v);
        }
        if let Some(v) = &p.base_url {
            let trimmed = trim(v);
            if trimmed.is_empty() {
                list.remove(target);
                c.sanitize_openai_compatibility();
                return Ok(());
            }
            entry.base_url = trimmed;
        }
        if let Some(keys) = &p.api_key_entries {
            for (k, key) in keys.iter().enumerate() {
                reject_weight(&format!("api-key-entries[{k}].weight"), key.weight)?;
            }
            entry.api_key_entries = keys.clone();
        }
        if let Some(m) = &p.models {
            entry.models = m.clone();
        }
        if let Some(h) = &p.headers {
            entry.headers = normalize_headers(h);
        }
        if let Some(v) = p.support_prompt_cache_key {
            entry.support_prompt_cache_key = v;
        }
        if let Some(v) = &p.request_scoped_errors {
            entry.request_scoped_errors = v.clone();
        }
        normalize_openai_entry(&mut entry);
        list[target] = entry;
        c.sanitize_openai_compatibility();
        Ok(())
    })
    .await
}

pub(crate) async fn delete_openai_compat(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    persist(&st, move |c| {
        let name = query_get(&uri, "name").unwrap_or_default();
        if !name.is_empty() {
            c.openai_compatibility.retain(|e| e.name != name);
            c.sanitize_openai_compatibility();
            return Ok(());
        }
        if let Some(i) = query_index(&uri, c.openai_compatibility.len()) {
            c.openai_compatibility.remove(i);
            c.sanitize_openai_compatibility();
            return Ok(());
        }
        Err(ApiError::bad_request("missing name or index"))
    })
    .await
}

// ---- oauth maps ----

/// Go: `json.Unmarshal(data, &map)`, else the `{"items": map}` wrapper; both failing is
/// `invalid body`. `null` decodes to an empty map.
fn map_body<T: DeserializeOwned>(body: &[u8]) -> ApiResult<BTreeMap<String, T>> {
    if let Ok(m) = serde_json::from_slice::<Option<BTreeMap<String, T>>>(body) {
        return Ok(m.unwrap_or_default());
    }
    #[derive(Deserialize)]
    struct Wrapper<T> {
        #[serde(default = "BTreeMap::new")]
        items: BTreeMap<String, T>,
    }
    serde_json::from_slice::<Wrapper<T>>(body)
        .map(|w| w.items)
        .map_err(|_| invalid_body())
}

/// Go: `sanitizedOAuthModelAlias`.
fn sanitized_model_alias(
    entries: BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    let copied: BTreeMap<_, _> = entries.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if copied.is_empty() {
        return BTreeMap::new();
    }
    let mut cfg = Config {
        oauth_model_alias: copied,
        ..Config::default()
    };
    cfg.sanitize_oauth_model_alias();
    cfg.oauth_model_alias
}

/// Go: `sanitizedOAuthRequestScopedErrors`.
fn sanitized_scoped_errors(
    entries: BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> BTreeMap<String, Vec<RequestScopedErrorRule>> {
    let copied: BTreeMap<_, _> = entries.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if copied.is_empty() {
        return BTreeMap::new();
    }
    let mut cfg = Config {
        oauth_request_scoped_errors: copied,
        ..Config::default()
    };
    cfg.sanitize_oauth_request_scoped_errors();
    cfg.oauth_request_scoped_errors
}

/// A map response: `null` when empty (Go returns a nil map then).
fn map_response<T: Serialize>(key: &str, map: &BTreeMap<String, T>) -> ApiResult {
    let v = if map.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(map).unwrap_or(Value::Null)
    };
    Ok(ok_json(&json!({ key: v })))
}

pub(crate) async fn get_oauth_excluded_models(State(st): State<ManagementState>) -> ApiResult {
    let m = cpa_config::normalize_oauth_excluded_models(&st.cfg().oauth_excluded_models);
    map_response("oauth-excluded-models", &m)
}

pub(crate) async fn put_oauth_excluded_models(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let entries: BTreeMap<String, Vec<String>> = map_body(&body)?;
    persist(&st, move |c| {
        c.oauth_excluded_models = cpa_config::normalize_oauth_excluded_models(&entries);
        Ok(())
    })
    .await
}

pub(crate) async fn patch_oauth_excluded_models(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        provider: Option<String>,
        models: Option<Vec<String>>,
    }
    let b: Body = bind(&body)?;
    let provider = b.provider.ok_or_else(invalid_body)?.trim().to_lowercase();
    if provider.is_empty() {
        return Err(ApiError::bad_request("invalid provider"));
    }
    let normalized = normalize_excluded_models(&b.models.unwrap_or_default());
    persist(&st, move |c| {
        if normalized.is_empty() {
            if c.oauth_excluded_models.remove(&provider).is_none() {
                return Err(ApiError::new(404, "provider not found"));
            }
            return Ok(());
        }
        c.oauth_excluded_models.insert(provider, normalized);
        Ok(())
    })
    .await
}

pub(crate) async fn delete_oauth_excluded_models(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    let provider = crate::v0_util::q_trim(&uri, "provider").to_lowercase();
    if provider.is_empty() {
        return Err(ApiError::bad_request("missing provider"));
    }
    persist(&st, move |c| {
        if c.oauth_excluded_models.remove(&provider).is_none() {
            return Err(ApiError::new(404, "provider not found"));
        }
        Ok(())
    })
    .await
}

pub(crate) async fn get_oauth_model_alias(State(st): State<ManagementState>) -> ApiResult {
    let m = sanitized_model_alias(st.cfg().oauth_model_alias.clone());
    map_response("oauth-model-alias", &m)
}

pub(crate) async fn put_oauth_model_alias(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let entries: BTreeMap<String, Vec<OAuthModelAlias>> = map_body(&body)?;
    persist(&st, move |c| {
        c.oauth_model_alias = sanitized_model_alias(entries);
        Ok(())
    })
    .await
}

/// Channel from the `channel` field, else `provider` (Go compares pointers, so an explicit empty
/// `channel` does not fall back).
fn channel_of(channel: Option<String>, provider: Option<String>) -> String {
    channel
        .or(provider)
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

pub(crate) async fn patch_oauth_model_alias(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        provider: Option<String>,
        channel: Option<String>,
        aliases: Option<Vec<OAuthModelAlias>>,
    }
    let b: Body = bind(&body)?;
    let channel = channel_of(b.channel, b.provider);
    if channel.is_empty() {
        return Err(ApiError::bad_request("invalid channel"));
    }
    let mut one = BTreeMap::new();
    one.insert(channel.clone(), b.aliases.unwrap_or_default());
    let normalized = sanitized_model_alias(one).remove(&channel).unwrap_or_default();
    persist(&st, move |c| {
        if normalized.is_empty() {
            if c.oauth_model_alias.remove(&channel).is_none() {
                return Err(ApiError::new(404, "channel not found"));
            }
            return Ok(());
        }
        c.oauth_model_alias.insert(channel, normalized);
        Ok(())
    })
    .await
}

/// `channel`, else `provider` query value (trimmed, lowercased).
fn channel_query(uri: &Uri) -> String {
    let channel = crate::v0_util::q_trim(uri, "channel").to_lowercase();
    if channel.is_empty() {
        crate::v0_util::q_trim(uri, "provider").to_lowercase()
    } else {
        channel
    }
}

pub(crate) async fn delete_oauth_model_alias(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    let channel = channel_query(&uri);
    if channel.is_empty() {
        return Err(ApiError::bad_request("missing channel"));
    }
    persist(&st, move |c| {
        if c.oauth_model_alias.remove(&channel).is_none() {
            return Err(ApiError::new(404, "channel not found"));
        }
        Ok(())
    })
    .await
}

pub(crate) async fn get_oauth_request_scoped_errors(State(st): State<ManagementState>) -> ApiResult {
    let m = sanitized_scoped_errors(st.cfg().oauth_request_scoped_errors.clone());
    map_response("oauth-request-scoped-errors", &m)
}

pub(crate) async fn put_oauth_request_scoped_errors(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let entries: BTreeMap<String, Vec<RequestScopedErrorRule>> = map_body(&body)?;
    persist(&st, move |c| {
        c.oauth_request_scoped_errors = sanitized_scoped_errors(entries);
        Ok(())
    })
    .await
}

pub(crate) async fn patch_oauth_request_scoped_errors(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        provider: Option<String>,
        channel: Option<String>,
        rules: Option<Vec<RequestScopedErrorRule>>,
    }
    let b: Body = bind(&body)?;
    let channel = channel_of(b.channel, b.provider);
    if channel.is_empty() {
        return Err(ApiError::bad_request("invalid channel"));
    }
    let mut one = BTreeMap::new();
    one.insert(channel.clone(), b.rules.unwrap_or_default());
    let normalized = sanitized_scoped_errors(one).remove(&channel).unwrap_or_default();
    persist(&st, move |c| {
        if normalized.is_empty() {
            if c.oauth_request_scoped_errors.remove(&channel).is_none() {
                return Err(ApiError::new(404, "channel not found"));
            }
            return Ok(());
        }
        c.oauth_request_scoped_errors.insert(channel, normalized);
        Ok(())
    })
    .await
}

pub(crate) async fn delete_oauth_request_scoped_errors(State(st): State<ManagementState>, uri: Uri) -> ApiResult {
    let channel = channel_query(&uri);
    if channel.is_empty() {
        return Err(ApiError::bad_request("missing channel"));
    }
    persist(&st, move |c| {
        if c.oauth_request_scoped_errors.remove(&channel).is_none() {
            return Err(ApiError::new(404, "channel not found"));
        }
        Ok(())
    })
    .await
}
