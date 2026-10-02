//! Session identity enrichment and derived ids (Go: sdk/cliproxy/session/identity.go, plus the
//! affinity id helpers of auth/selector.go).
//!
//! `enrich` runs at the top of every execution and places `canonical_session_id`,
//! `parent_session_id` or a derived `ctx:v1:<sha256>` id (from the first conversation turn) into
//! the request metadata. `session_ids` returns the (primary, fallback) pair affinity binds on.

use cpa_json::{J, Value};
use http::HeaderMap;
use serde_json::Map;
use sha2::{Digest, Sha256};

use cpa_translator::Format;

use super::info::{
    LCP_AFFINITY_SESSION_ID, bound_session_identity, claude_metadata_identities,
    extract_session_info, normalize_explicit_id,
};
use crate::executor::{Metadata, Options, Request, meta};

const IDENTITY_VERSION: &str = "cpa-session-root-v1";
const IDENTITY_PREFIX: &str = "ctx:v1:";
const INSTRUCTION_RUNE_LIMIT: usize = 50;

/// Recognised protocol session prefixes, used when looking up a bare session id.
pub const CANDIDATE_SESSION_PREFIXES: [&str; 18] = [
    "lcp:v1:",
    "lcp:",
    "codex:",
    "claude:",
    "header:",
    "session:",
    "affinity:",
    "slot:",
    "task:",
    "conv:",
    "thread:",
    "clientreq:",
    "geminicache:",
    "pck:",
    "user:",
    "execution:",
    "agy:",
    "derived:",
];

/// Legacy protocol prefixes unwrapped before projecting to a UUID (Go: knownSessionPrefixes).
const KNOWN_SESSION_PREFIXES: [&str; 20] = [
    "lcp:v1:", "lcp:", "ctx:v1:", "ctx:", "codex:", "claude:", "header:", "session:", "affinity:", "slot:", "task:", "conv:", "thread:",
    "clientreq:", "geminicache:", "pck:", "user:", "execution:", "agy:", "derived:",
];

fn is_canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36 && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

/// Deterministic 36-character lowercase UUID for any session id (Go: NormalizeToCanonicalUUID):
/// UUIDs pass through, known prefixes are unwrapped, everything else is projected to a UUIDv8.
pub fn normalize_to_canonical_uuid(raw_id: &str) -> String {
    let mut clean = raw_id.trim();
    if clean.is_empty() {
        return String::new();
    }
    if is_canonical_uuid(clean) {
        return clean.to_lowercase();
    }
    loop {
        let Some(prefix) = KNOWN_SESSION_PREFIXES.iter().find(|p| clean.starts_with(**p)) else {
            break;
        };
        clean = clean[prefix.len()..].trim();
    }
    if clean.is_empty() {
        return String::new();
    }
    if is_canonical_uuid(clean) {
        return clean.to_lowercase();
    }
    if let Some(idx) = clean.find(':').filter(|i| *i > 0) {
        let candidate = clean[idx + 1..].trim();
        if is_canonical_uuid(candidate) {
            return candidate.to_lowercase();
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(b"cpa:canonical-uuid:v1\x00");
    hasher.update(clean.as_bytes());
    let sum = hasher.finalize();
    let mut u = [0u8; 16];
    u.copy_from_slice(&sum[..16]);
    u[6] = (u[6] & 0x0f) | 0x80;
    u[8] = (u[8] & 0x3f) | 0x80;
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Irreversible namespace for a downstream caller credential (Go: CallerScope).
pub fn caller_scope(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    h.update(b"cli-proxy-api:caller-scope:v1\x00");
    h.update(value.as_bytes());
    hex::encode(h.finalize())
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    match metadata.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) => v.to_string().trim().to_string(),
    }
}

fn first_normalized_metadata_id(key: &str, sets: &[&Metadata]) -> String {
    for m in sets {
        if let Some(Value::String(s)) = m.get(key) {
            let n = normalize_explicit_id(s);
            if !n.is_empty() {
                return n;
            }
        }
    }
    String::new()
}

/// Derived session id stored in execution metadata.
pub fn derived_id(metadata: &Metadata) -> String {
    metadata
        .get(meta::DERIVED_SESSION_ID)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn set(m: &mut Metadata, key: &str, value: impl Into<Value>) {
    m.insert(key.to_string(), value.into());
}

fn set_both(req: &mut Request, opts: &mut Options, key: &str, value: &str) {
    set(&mut req.metadata, key, value);
    set(&mut opts.metadata, key, value);
}

fn remove_both(req: &mut Request, opts: &mut Options, key: &str) {
    req.metadata.remove(key);
    opts.metadata.remove(key);
}

/// Derives a session identity once and places it in both request and option metadata. When
/// `opts.original_request` is unset it shares the request payload (cheap `Bytes` clone).
pub fn enrich(req: &mut Request, opts: &mut Options) {
    if opts.original_request.is_empty() && !req.payload.is_empty() {
        opts.original_request = req.payload.clone();
    }
    let payload = opts.original_request.clone();
    let execution_id =
        first_normalized_metadata_id(meta::EXECUTION_SESSION_ID, &[&opts.metadata, &req.metadata]);

    let sync_execution = |req: &mut Request, opts: &mut Options| {
        if execution_id.is_empty() {
            remove_both(req, opts, meta::EXECUTION_SESSION_ID);
        } else {
            set_both(req, opts, meta::EXECUTION_SESSION_ID, &execution_id);
        }
    };

    if has_explicit_session(&opts.headers, &payload) {
        remove_both(req, opts, meta::DERIVED_SESSION_ID);
        sync_execution(req, opts);
        if let Some(info) = extract_session_info(&opts.headers, &payload, &opts.metadata)
            && !info.session_id.is_empty()
        {
            let canonical = bound_session_identity(&info.session_id);
            set_both(req, opts, meta::CANONICAL_SESSION_ID, &canonical);
            if !info.parent_session_id.is_empty() && info.parent_session_id != info.session_id {
                let parent = bound_session_identity(&info.parent_session_id);
                set_both(req, opts, meta::PARENT_SESSION_ID, &parent);
            } else {
                remove_both(req, opts, meta::PARENT_SESSION_ID);
            }
        }
        return;
    }

    // Metadata-provided canonical or LCP session (SDK callers or pre-routed stages).
    for key in [meta::CANONICAL_SESSION_ID, LCP_AFFINITY_SESSION_ID] {
        let id = first_normalized_metadata_id(key, &[&opts.metadata, &req.metadata]);
        if id.is_empty() {
            continue;
        }
        remove_both(req, opts, meta::DERIVED_SESSION_ID);
        sync_execution(req, opts);
        let canonical = bound_session_identity(&id);
        set_both(req, opts, meta::CANONICAL_SESSION_ID, &canonical);
        let parent =
            first_normalized_metadata_id(meta::PARENT_SESSION_ID, &[&opts.metadata, &req.metadata]);
        if !parent.is_empty() && parent != id {
            let bounded = bound_session_identity(&parent);
            set_both(req, opts, meta::PARENT_SESSION_ID, &bounded);
        } else {
            remove_both(req, opts, meta::PARENT_SESSION_ID);
        }
        return;
    }

    if !execution_id.is_empty() {
        let canonical = bound_session_identity(&format!("execution:{execution_id}"));
        remove_both(req, opts, meta::DERIVED_SESSION_ID);
        remove_both(req, opts, meta::PARENT_SESSION_ID);
        set_both(req, opts, meta::EXECUTION_SESSION_ID, &execution_id);
        set_both(req, opts, meta::CANONICAL_SESSION_ID, &canonical);
        return;
    }

    remove_both(req, opts, meta::EXECUTION_SESSION_ID);
    remove_both(req, opts, meta::PARENT_SESSION_ID);
    let mut derived =
        first_normalized_metadata_id(meta::DERIVED_SESSION_ID, &[&opts.metadata, &req.metadata]);
    remove_both(req, opts, meta::DERIVED_SESSION_ID);
    if derived.is_empty() {
        let mut scope = metadata_string(&opts.metadata, meta::CALLER_SCOPE);
        if scope.is_empty() {
            scope = metadata_string(&req.metadata, meta::CALLER_SCOPE);
        }
        derived = derive_id(opts.source_format, &payload, &scope);
    }
    if derived.is_empty() {
        return;
    }
    set_both(req, opts, meta::DERIVED_SESSION_ID, &derived);
}

fn header_has(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| !normalize_explicit_id(v).is_empty())
}

const EXPLICIT_HEADERS: &[&str] = &[
    "X-Claude-Code-Session-Id",
    "X-Claude-Code-Agent-Id",
    "X-Claude-Code-Parent-Agent-Id",
    "Session-Id",
    "Session_id",
    "x-codex-parent-thread-id",
    "X-Codex-Parent-Thread-Id",
    "X-Codex-Turn-Metadata",
    "X-Openai-Subagent",
    "X-Http-Session-Id",
    "X-Session-ID",
    "X-Session-Affinity",
    "X-Parent-Session-ID",
    "X-Parent-Session-Id",
    "X-Parent-Session-Affinity",
    "X-Parent-ID",
    "X-Parent-Id",
    "X-Slot-Session-Id",
    "X-Parent-Slot-Session-Id",
    "X-Task-ID",
    "X-Task-Id",
    "X-Parent-Task-ID",
    "X-Parent-Task-Id",
    "X-Conversation-Id",
    "X-Conversation-ID",
    "X-Parent-Conversation-Id",
    "X-Parent-Conversation-ID",
    "X-Thread-Id",
    "X-Thread-ID",
    "X-Parent-Thread-Id",
    "X-Parent-Thread-ID",
    "Thread-Id",
    "X-Client-Request-Id",
];

const EXPLICIT_BODY_PATHS: &[&str] = &[
    "session_id",
    "sessionId",
    "sessionID",
    "child_session_id",
    "childSessionId",
    "task_id",
    "taskId",
    "taskID",
    "action_id",
    "actionId",
    "cachedContent",
    "cached_content",
    "thread_id",
    "threadId",
    "conversation_id",
    "conversationId",
    "chat_id",
    "chatId",
    "prompt_cache_key",
    "promptCacheKey",
    "parent_session_id",
    "parentSessionId",
    "parent_thread_id",
    "parentThreadId",
    "parent_id",
    "parentId",
    "parentID",
    "parent_task_id",
    "parentTaskId",
    "parent_action_id",
    "parentActionId",
    "parent_session",
    "parentSession",
    "parent_subagent_id",
    "forkSource.sessionId",
    "previousSessionId",
    "forked_from_thread_id",
    "forked_from_id",
    "metadata.session_id",
    "metadata.sessionId",
    "metadata.task_id",
    "metadata.taskId",
    "metadata.thread_id",
    "metadata.conversation_id",
    "metadata.parent_id",
    "metadata.parent_task_id",
    "metadata.parent_agent_id",
    "extra_body.session_id",
    "extra_body.task_id",
    "extra_body.parent_id",
    "extra_body.parent_task_id",
];

/// Whether the headers or body carry any explicit client session signal.
pub fn has_explicit_session(headers: &HeaderMap, payload: &[u8]) -> bool {
    if EXPLICIT_HEADERS.iter().any(|h| header_has(headers, h)) {
        return true;
    }
    if payload.is_empty() {
        return false;
    }
    let root = cpa_json::parse(payload);
    let req = root.g("request");
    let nested = if req.exists() && !root.g("contents").exists() {
        req.into_value()
    } else {
        None
    };
    let probe = |path: &str| -> bool {
        !normalize_explicit_id(&root.g(path).str()).is_empty()
            || nested
                .as_ref()
                .is_some_and(|n| !normalize_explicit_id(&n.g(path).str()).is_empty())
    };
    if EXPLICIT_BODY_PATHS.iter().any(|p| probe(p)) {
        return true;
    }
    if !claude_metadata_identities(payload).0.is_empty() {
        return true;
    }
    let mut user_id = root.g("metadata.user_id").str().trim().to_string();
    if user_id.is_empty()
        && let Some(n) = &nested
    {
        user_id = n.g("metadata.user_id").str().trim().to_string();
    }
    if !normalize_explicit_id(&user_id).is_empty() {
        return true;
    }
    let mut conversation = root.g("conversation").into_value();
    if conversation.is_none() {
        conversation = nested
            .as_ref()
            .and_then(|n| n.g("conversation").into_value());
    }
    match conversation {
        Some(c) => {
            !normalize_explicit_id(&c.g("id").str()).is_empty()
                || c.as_str()
                    .is_some_and(|s| !normalize_explicit_id(s).is_empty())
        }
        None => false,
    }
}

// ---- Derived identity ----

#[derive(Debug, Clone)]
struct Part {
    kind: String,
    mime: String,
    value: String,
}

fn normalized_string(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default()
}

fn first_field<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| obj.get(*k))
}

fn string_field(obj: &Map<String, Value>, keys: &[&str]) -> String {
    first_field(obj, keys)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn truncate_runes(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// Drops `cache_control` keys recursively (case-insensitive), like Go's normalizeJSONValue.
fn normalize_json_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !k.trim().eq_ignore_ascii_case("cache_control"))
                .map(|(k, c)| (k.clone(), normalize_json_value(c)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(normalize_json_value).collect()),
        other => other.clone(),
    }
}

/// Go-style compact JSON (sorted keys, HTML escaping).
fn go_json(v: &Value) -> String {
    cpa_auth::util::marshal_compact(v).unwrap_or_default()
}

fn canonical_parts(v: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    append_canonical_parts(&mut parts, v);
    parts
}

fn text_part(value: &str) -> Part {
    Part {
        kind: "text".into(),
        mime: String::new(),
        value: value.to_string(),
    }
}

fn append_canonical_parts(parts: &mut Vec<Part>, v: &Value) {
    match v {
        Value::Null => {}
        Value::String(s) => {
            if !s.is_empty() {
                parts.push(text_part(s));
            }
        }
        Value::Array(items) => items.iter().for_each(|c| append_canonical_parts(parts, c)),
        Value::Object(m) => {
            if let Some(Value::String(t)) = m.get("text") {
                if !t.is_empty() {
                    parts.push(text_part(t));
                }
                return;
            }
            if let Some(nested) = m.get("content") {
                append_canonical_parts(parts, nested);
                return;
            }
            if let Some(nested) = m.get("parts") {
                append_canonical_parts(parts, nested);
                return;
            }
            if let Some(image) = m.get("image_url") {
                append_media_part(parts, "image", image, "");
                return;
            }
            if let Some(inline) = first_field(m, &["inlineData", "inline_data"]) {
                append_media_part(parts, "inline_data", inline, "");
                return;
            }
            if let Some(file) = first_field(m, &["fileData", "file_data"]) {
                append_media_part(parts, "file", file, "");
                return;
            }
            if let Some(source) = m.get("source") {
                append_media_part(
                    parts,
                    &normalized_string(m.get("type")),
                    source,
                    &normalized_string(m.get("media_type")),
                );
                return;
            }
            let normalized = normalize_json_value(v);
            let encoded = go_json(&normalized);
            if !encoded.is_empty() {
                parts.push(Part {
                    kind: "json".into(),
                    mime: String::new(),
                    value: encoded,
                });
            }
        }
        other => {
            let encoded = go_json(other);
            if !encoded.is_empty() {
                parts.push(Part {
                    kind: "json".into(),
                    mime: String::new(),
                    value: encoded,
                });
            }
        }
    }
}

fn append_media_part(parts: &mut Vec<Part>, kind: &str, value: &Value, fallback_mime: &str) {
    let kind = kind.trim();
    let kind = if kind.is_empty() { "media" } else { kind };
    match value {
        Value::String(s) => {
            if !s.is_empty() {
                parts.push(Part {
                    kind: kind.into(),
                    mime: fallback_mime.into(),
                    value: s.clone(),
                });
            }
        }
        Value::Object(m) => {
            let mut mime = string_field(m, &["mimeType", "mime_type", "media_type"]);
            if mime.is_empty() {
                mime = fallback_mime.to_string();
            }
            let media_value = string_field(m, &["url", "uri", "fileUri", "file_uri", "data"]);
            if !media_value.is_empty() {
                parts.push(Part {
                    kind: kind.into(),
                    mime,
                    value: media_value,
                });
            }
        }
        other => append_canonical_parts(parts, other),
    }
}

fn content_value(v: &Value) -> Value {
    let Value::Object(m) = v else {
        return v.clone();
    };
    for key in ["content", "parts", "text"] {
        if let Some(c) = m.get(key) {
            return c.clone();
        }
    }
    v.clone()
}

fn append_instruction(instructions: &mut Vec<String>, value: &Value) {
    let text: Vec<String> = canonical_parts(value)
        .into_iter()
        .filter(|p| p.kind == "text" && !p.value.is_empty())
        .map(|p| p.value)
        .collect();
    let joined = text.join("\n");
    if joined.is_empty() {
        return;
    }
    instructions.push(truncate_runes(&joined, INSTRUCTION_RUNE_LIMIT));
}

type Root = (Vec<String>, Vec<Part>);

fn messages_root(body: &Map<String, Value>, include_top_level_system: bool) -> Root {
    let mut instructions = Vec::new();
    if include_top_level_system && let Some(system) = body.get("system") {
        append_instruction(&mut instructions, system);
    }
    if let Some(Value::Array(messages)) = body.get("messages") {
        for raw in messages {
            let Value::Object(message) = raw else {
                continue;
            };
            match normalized_string(message.get("role")).as_str() {
                "system" | "developer" => append_instruction(
                    &mut instructions,
                    message.get("content").unwrap_or(&Value::Null),
                ),
                "user" => {
                    let parts = canonical_parts(message.get("content").unwrap_or(&Value::Null));
                    if !parts.is_empty() {
                        return (instructions, parts);
                    }
                }
                _ => {}
            }
        }
    }
    (instructions, Vec::new())
}

fn responses_root(body: &Map<String, Value>) -> Root {
    let mut instructions = Vec::new();
    if let Some(v) = body.get("instructions") {
        append_instruction(&mut instructions, v);
    }
    let Some(input) = body.get("input") else {
        return (instructions, Vec::new());
    };
    if let Value::String(_) = input {
        return (instructions, canonical_parts(input));
    }
    if let Value::Array(items) = input {
        for raw in items {
            let Value::Object(item) = raw else { continue };
            match normalized_string(item.get("role")).as_str() {
                "system" | "developer" => append_instruction(
                    &mut instructions,
                    item.get("content").unwrap_or(&Value::Null),
                ),
                "user" => {
                    let parts = canonical_parts(item.get("content").unwrap_or(&Value::Null));
                    if !parts.is_empty() {
                        return (instructions, parts);
                    }
                }
                _ => {}
            }
        }
    }
    (instructions, Vec::new())
}

fn gemini_root(body: &Map<String, Value>) -> Root {
    let body = match body.get("request") {
        Some(Value::Object(req)) => req,
        _ => body,
    };
    let mut instructions = Vec::new();
    if let Some(v) = first_field(body, &["systemInstruction", "system_instruction"]) {
        append_instruction(&mut instructions, &content_value(v));
    }
    if let Some(Value::Array(contents)) = body.get("contents") {
        for raw in contents {
            let Value::Object(content) = raw else {
                continue;
            };
            if normalized_string(content.get("role")) != "user" {
                continue;
            }
            let parts = canonical_parts(&content_value(raw));
            if !parts.is_empty() {
                return (instructions, parts);
            }
        }
    }
    (instructions, Vec::new())
}

fn flatten_interaction_entries(value: &Value) -> Vec<Value> {
    fn walk(current: &Value, inherited_role: &str, out: &mut Vec<Value>) {
        match current {
            Value::Array(items) => items.iter().for_each(|c| walk(c, inherited_role, out)),
            Value::Object(m) => {
                let mut role = normalized_string(m.get("role"));
                if role.is_empty() {
                    role = inherited_role.to_string();
                }
                if let Some(Value::Array(steps)) = m.get("steps") {
                    steps.iter().for_each(|c| walk(c, &role, out));
                    return;
                }
                if !role.is_empty() && normalized_string(m.get("role")).is_empty() {
                    let mut cloned = m.clone();
                    cloned.insert("role".into(), Value::String(role));
                    out.push(Value::Object(cloned));
                } else {
                    out.push(current.clone());
                }
            }
            other => out.push(other.clone()),
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out
}

fn interactions_root(body: &Map<String, Value>) -> Root {
    let mut instructions = Vec::new();
    if let Some(v) = first_field(body, &["system_instruction", "systemInstruction"]) {
        append_instruction(&mut instructions, &content_value(v));
    }
    let Some(input) = body.get("input") else {
        return (instructions, Vec::new());
    };
    if let Value::String(_) = input {
        return (instructions, canonical_parts(input));
    }
    for entry in flatten_interaction_entries(input) {
        if let Value::String(_) = entry {
            return (instructions, canonical_parts(&entry));
        }
        let Value::Object(step) = &entry else {
            continue;
        };
        let role = normalized_string(step.get("role"));
        let step_type = normalized_string(step.get("type"));
        if role == "system"
            || role == "developer"
            || step_type == "system_instruction"
            || step_type == "developer_instruction"
        {
            append_instruction(&mut instructions, &content_value(&entry));
            continue;
        }
        if role == "user"
            || step_type == "user_input"
            || ((step_type == "message" || step_type.is_empty()) && role.is_empty())
        {
            return (instructions, canonical_parts(&content_value(&entry)));
        }
    }
    (instructions, Vec::new())
}

fn hash_root(format: Format, caller_scope: &str, resource: &str, root: &Root) -> String {
    let (instructions, user) = root;
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"version\":{},\"format\":{},\"caller_scope\":{}",
        go_json(&Value::String(IDENTITY_VERSION.into())),
        go_json(&Value::String(format.as_str().into())),
        go_json(&Value::String(caller_scope.trim().into()))
    ));
    if !instructions.is_empty() {
        let items: Vec<String> = instructions
            .iter()
            .map(|i| go_json(&Value::String(i.clone())))
            .collect();
        out.push_str(&format!(",\"instructions\":[{}]", items.join(",")));
    }
    if !user.is_empty() {
        let items: Vec<String> = user
            .iter()
            .map(|p| {
                let mime = if p.mime.is_empty() {
                    String::new()
                } else {
                    format!(",\"mime\":{}", go_json(&Value::String(p.mime.clone())))
                };
                format!(
                    "{{\"kind\":{}{mime},\"value\":{}}}",
                    go_json(&Value::String(p.kind.clone())),
                    go_json(&Value::String(p.value.clone()))
                )
            })
            .collect();
        out.push_str(&format!(",\"user\":[{}]", items.join(",")));
    }
    if !resource.is_empty() {
        out.push_str(&format!(
            ",\"resource\":{}",
            go_json(&Value::String(resource.into()))
        ));
    }
    out.push('}');
    format!(
        "{IDENTITY_PREFIX}{}",
        hex::encode(Sha256::digest(out.as_bytes()))
    )
}

/// Stable identity from leading instructions and the first complete user input (Go: DeriveID).
pub fn derive_id(format: Format, payload: &[u8], caller_scope: &str) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(payload) else {
        return String::new();
    };
    let mut resource = String::new();
    let gemini_like = matches!(format, Format::Gemini | Format::Antigravity);
    if gemini_like {
        let req_body = match body.get("request") {
            Some(Value::Object(req)) => req,
            _ => &body,
        };
        resource = string_field(req_body, &["cachedContent", "cached_content"]);
    }
    let root = match format {
        Format::Gemini | Format::Antigravity => gemini_root(&body),
        Format::Interactions => interactions_root(&body),
        Format::OpenAIResponse | Format::Codex => responses_root(&body),
        Format::Claude => messages_root(&body, true),
        _ => messages_root(&body, false),
    };
    if root.1.is_empty() {
        return String::new();
    }
    hash_root(format, caller_scope, &resource, &root)
}

// ---- Affinity ids (Go: selector.go extract*SessionIDs) ----

fn extract_conversation_alias(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let root = cpa_json::parse(payload);
    let mut conversation = root.g("conversation").into_value();
    if conversation.is_none() {
        let req = root.g("request");
        if req.exists() && !root.g("contents").exists() {
            conversation = req.g("conversation").into_value();
        }
    }
    let Some(conv) = conversation else {
        return String::new();
    };
    let sid = normalize_explicit_id(&conv.g("id").str());
    if !sid.is_empty() {
        return format!("conv:{sid}");
    }
    if let Some(s) = conv.as_str() {
        let sid = normalize_explicit_id(s);
        if !sid.is_empty() {
            return format!("conv:{sid}");
        }
    }
    String::new()
}

/// Client- or execution-provided identities only, as `(primary, fallback)`. Records fork/parent
/// hints in `metadata`.
pub fn explicit_session_ids(
    headers: &HeaderMap,
    payload: &[u8],
    metadata: &mut Metadata,
) -> (String, String) {
    let Some(info) = extract_session_info(headers, payload, metadata) else {
        return Default::default();
    };
    if info.client_type == "lcp" {
        return Default::default();
    }
    if info.is_fork {
        metadata.insert(meta::IS_FORK.into(), Value::Bool(true));
    }
    if !info.parent_session_id.is_empty() {
        metadata.insert(
            meta::PARENT_SESSION_ID.into(),
            Value::String(info.parent_session_id.clone()),
        );
    }
    let mut fallback = info.parent_session_id.clone();
    if fallback.is_empty() && info.session_id.starts_with("pck:") && !payload.is_empty() {
        fallback = extract_conversation_alias(payload);
    }
    (info.session_id, fallback)
}

/// `(primary, fallback)` affinity ids: explicit ids, else derived metadata id, else a hash of
/// the first system/user/assistant messages.
pub fn session_ids(
    headers: &HeaderMap,
    payload: &[u8],
    metadata: &mut Metadata,
) -> (String, String) {
    let (primary, fallback) = explicit_session_ids(headers, payload, metadata);
    if !primary.is_empty() {
        return (primary, fallback);
    }
    let derived = normalize_explicit_id(&derived_id(metadata));
    if !derived.is_empty() {
        return (format!("derived:{derived}"), String::new());
    }
    if payload.is_empty() {
        return Default::default();
    }
    extract_message_hash_ids(payload)
}

/// The single authoritative session identity of a request (Go: CanonicalSessionID).
pub fn canonical_session_id(headers: &HeaderMap, payload: &[u8], metadata: &Metadata) -> String {
    let mut scratch = metadata.clone();
    let (explicit, _) = explicit_session_ids(headers, payload, &mut scratch);
    if !explicit.is_empty() {
        return bound_session_identity(&explicit);
    }
    for key in [meta::CANONICAL_SESSION_ID, LCP_AFFINITY_SESSION_ID] {
        if let Some(Value::String(s)) = metadata.get(key)
            && !s.trim().is_empty()
        {
            return bound_session_identity(s.trim());
        }
    }
    bound_session_identity(&session_ids(headers, payload, &mut scratch).0)
}

fn fnv64a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn compute_session_hash(system_prompt: &str, user: &str, assistant: &str) -> String {
    let mut buf = String::new();
    if !system_prompt.is_empty() {
        buf.push_str(&format!("sys:{system_prompt}\n"));
    }
    if !user.is_empty() {
        buf.push_str(&format!("usr:{user}\n"));
    }
    if !assistant.is_empty() {
        buf.push_str(&format!("ast:{assistant}\n"));
    }
    format!("msg:{:016x}", fnv64a(buf.as_bytes()))
}

/// Byte truncation like Go's `s[:maxLen]`; backs up to a char boundary instead of splitting.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn extract_message_content(content: &cpa_json::Res<'_>) -> String {
    if content.is_string() {
        return content.str();
    }
    if content.is_array() {
        let mut texts = Vec::new();
        content.for_each(|_, part| {
            if part.g("type").str() == "text" {
                let t = part.g("text").str();
                if !t.is_empty() {
                    texts.push(t);
                }
            }
            true
        });
        if !texts.is_empty() {
            return texts.join(" ");
        }
    }
    String::new()
}

fn extract_responses_api_content(content: &cpa_json::Res<'_>) -> String {
    if !content.is_array() {
        return String::new();
    }
    let mut texts = Vec::new();
    content.for_each(|_, part| {
        let t = part.g("type").str();
        if matches!(t.as_str(), "input_text" | "output_text" | "text") {
            let text = part.g("text").str();
            if !text.is_empty() {
                texts.push(text);
            }
        }
        true
    });
    texts.join(" ")
}

/// Hash fallback ids from the first system/user/assistant message contents (100-byte
/// truncations). Returns `(hash with assistant, hash without assistant)`, or just the short hash.
pub fn extract_message_hash_ids(payload: &[u8]) -> (String, String) {
    let root = cpa_json::parse(payload);
    let (mut system_prompt, mut first_user, mut first_assistant) =
        (String::new(), String::new(), String::new());

    let messages = root.g("messages");
    if messages.is_array() {
        messages.for_each(|_, msg| {
            let role = msg.g("role").str();
            let content = extract_message_content(&msg.g("content"));
            if content.is_empty() {
                return true;
            }
            match role.as_str() {
                "system" if system_prompt.is_empty() => {
                    system_prompt = truncate_bytes(&content, 100)
                }
                "user" if first_user.is_empty() => first_user = truncate_bytes(&content, 100),
                "assistant" if first_assistant.is_empty() => {
                    first_assistant = truncate_bytes(&content, 100)
                }
                _ => {}
            }
            !(!system_prompt.is_empty() && !first_user.is_empty() && !first_assistant.is_empty())
        });
    }

    // Claude API: top-level system field (array or string).
    if system_prompt.is_empty() {
        let top = root.g("system");
        if top.exists() {
            if top.is_array() {
                top.for_each(|_, part| {
                    let text = part.g("text").str();
                    if !text.is_empty() && system_prompt.is_empty() {
                        system_prompt = truncate_bytes(&text, 100);
                        return false;
                    }
                    true
                });
            } else if top.is_string() {
                system_prompt = truncate_bytes(&top.str(), 100);
            }
        }
    }

    // Gemini format.
    if system_prompt.is_empty() && first_user.is_empty() {
        let parts = root.g("systemInstruction.parts");
        if parts.is_array() {
            parts.for_each(|_, part| {
                let text = part.g("text").str();
                if !text.is_empty() && system_prompt.is_empty() {
                    system_prompt = truncate_bytes(&text, 100);
                    return false;
                }
                true
            });
        }
        let contents = root.g("contents");
        if contents.is_array() {
            contents.for_each(|_, msg| {
                let role = msg.g("role").str();
                msg.g("parts").for_each(|_, part| {
                    let text = part.g("text").str();
                    if text.is_empty() {
                        return true;
                    }
                    match role.as_str() {
                        "user" if first_user.is_empty() => first_user = truncate_bytes(&text, 100),
                        "model" if first_assistant.is_empty() => {
                            first_assistant = truncate_bytes(&text, 100)
                        }
                        _ => {}
                    }
                    false
                });
                !(!first_user.is_empty() && !first_assistant.is_empty())
            });
        }
    }

    // OpenAI Responses API format.
    if system_prompt.is_empty() && first_user.is_empty() {
        let instr = root.g("instructions").str();
        if !instr.is_empty() {
            system_prompt = truncate_bytes(&instr, 100);
        }
        let input = root.g("input");
        if input.is_array() {
            input.for_each(|_, item| {
                let item_type = item.g("type").str();
                if item_type == "reasoning" {
                    return true;
                }
                if !item_type.is_empty() && item_type != "message" {
                    return true;
                }
                let role = item.g("role").str();
                if item_type.is_empty() && role.is_empty() {
                    return true;
                }
                let content = item.g("content");
                let text = if content.is_string() {
                    content.str()
                } else {
                    extract_responses_api_content(&content)
                };
                if text.is_empty() {
                    return true;
                }
                match role.as_str() {
                    "developer" | "system" if system_prompt.is_empty() => {
                        system_prompt = truncate_bytes(&text, 100)
                    }
                    "user" if first_user.is_empty() => first_user = truncate_bytes(&text, 100),
                    "assistant" if first_assistant.is_empty() => {
                        first_assistant = truncate_bytes(&text, 100)
                    }
                    _ => {}
                }
                !(!first_user.is_empty() && !first_assistant.is_empty())
            });
        }
    }

    if first_user.is_empty() {
        return Default::default();
    }
    let short_hash = compute_session_hash(&system_prompt, &first_user, "");
    if first_assistant.is_empty() {
        return (short_hash, String::new());
    }
    (
        compute_session_hash(&system_prompt, &first_user, &first_assistant),
        short_hash,
    )
}

/// Whether the id names a subagent session: a Claude/Codex agent id or a hierarchy child.
pub fn is_subagent_session(primary: &str, fallback: &str) -> bool {
    if primary.contains(":agent:") {
        return true;
    }
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    is_hierarchy_parent(primary, fallback)
}

/// Go: isHierarchyParent. Same protocol prefix (or both bare ids) means a parent/child pair.
fn is_hierarchy_parent(primary: &str, fallback: &str) -> bool {
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    if primary.contains(":agent:") {
        return true;
    }
    match (primary.find(':'), fallback.find(':')) {
        (Some(i1), Some(i2)) if i1 > 0 && i2 > 0 && primary[..i1] == fallback[..i2] => true,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn req(payload: &str) -> (Request, Options) {
        let mut opts = Options::new(Format::OpenAI);
        opts.original_request = Bytes::from(payload.to_string());
        (
            Request {
                model: "m".into(),
                payload: Bytes::from(payload.to_string()),
                format: Format::OpenAI,
                metadata: Metadata::new(),
            },
            opts,
        )
    }

    #[test]
    fn derived_id_is_stable_and_ignores_later_turns() {
        let a = derive_id(Format::OpenAI, br#"{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"hello"}]}"#, "s");
        let b = derive_id(
            Format::OpenAI,
            br#"{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"more"}]}"#,
            "s",
        );
        assert!(a.starts_with("ctx:v1:") && a.len() == 7 + 64);
        assert_eq!(a, b);
        assert_ne!(
            a,
            derive_id(
                Format::OpenAI,
                br#"{"messages":[{"role":"user","content":"other"}]}"#,
                "s"
            )
        );
        assert_ne!(a, derive_id(Format::OpenAI, br#"{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"hello"}]}"#, "other-scope"));
        assert_eq!(derive_id(Format::OpenAI, b"not json", ""), "");
    }

    #[test]
    fn enrich_prefers_explicit_then_derived() {
        let (mut r, mut o) = req(r#"{"messages":[{"role":"user","content":"hello"}]}"#);
        enrich(&mut r, &mut o);
        assert!(o.metadata.get(meta::DERIVED_SESSION_ID).is_some());
        assert!(o.metadata.get(meta::CANONICAL_SESSION_ID).is_none());

        let (mut r, mut o) =
            req(r#"{"messages":[{"role":"user","content":"hello"}],"prompt_cache_key":"abc"}"#);
        enrich(&mut r, &mut o);
        assert_eq!(o.metadata[meta::CANONICAL_SESSION_ID], "pck:abc");
        assert!(o.metadata.get(meta::DERIVED_SESSION_ID).is_none());
    }

    #[test]
    fn affinity_ids_prefer_explicit_then_derived_then_hash() {
        let mut md = Metadata::new();
        let (p, _) = session_ids(&HeaderMap::new(), br#"{"prompt_cache_key":"k"}"#, &mut md);
        assert_eq!(p, "pck:k");
        let mut md = Metadata::new();
        md.insert(
            meta::DERIVED_SESSION_ID.into(),
            Value::String("ctx:v1:abc".into()),
        );
        let (p, _) = session_ids(&HeaderMap::new(), br#"{"messages":[]}"#, &mut md);
        assert_eq!(p, "derived:ctx:v1:abc");
        let mut md = Metadata::new();
        let body =
            br#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}]}"#;
        let (p, f) = session_ids(&HeaderMap::new(), body, &mut md);
        assert!(p.starts_with("msg:") && f.starts_with("msg:") && p != f);
    }
}
