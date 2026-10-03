//! Session identity helpers for executors (Go: helps/cpa_session.go, derived_session.go and
//! claude_code_session.go). The gin-context header fallback of Go's `claudeCodeHeader` is dropped;
//! callers pass the headers.

use std::sync::LazyLock;
use std::time::Instant;

use cpa_json::J;
use cpa_runtime::conductor::session::{canonical_session_id, identity::derived_id};
use cpa_runtime::executor::{Metadata, Options, meta};
use http::HeaderMap;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::helps::id_cache::{CodexCache, ID_TTL};

/// The internal session id used to expand `$CPA-SESSION-ID` in custom headers.
///
/// Go annotates the request context; here the result is passed to
/// `cpa_core::util::apply_custom_headers_from_attrs` as its `session_id`. Resolution order:
/// an explicit annotation (`existing`, where `Some("")` is an explicit clear and wins), then the
/// session id of the client request (`client_session_id`), then the canonical id derived from the
/// request headers, payload and metadata. `None` when no id can be determined.
pub fn ensure_session_id(
    existing: Option<&str>,
    client_session_id: &str,
    opts: &Options,
    payload: &[u8],
) -> Option<String> {
    if let Some(id) = existing {
        return Some(id.to_string());
    }
    if !client_session_id.is_empty() {
        return Some(client_session_id.to_string());
    }
    let eval_payload: &[u8] = if opts.original_request.is_empty() { payload } else { &opts.original_request };
    let canonical = canonical_session_id(&opts.headers, eval_payload, &opts.metadata);
    (!canonical.is_empty()).then_some(canonical)
}

/// The first context-derived session identity in metadata order.
pub fn derived_session_id(metadata_sets: &[&Metadata]) -> String {
    metadata_sets.iter().map(|m| derived_id(m)).find(|id| !id.is_empty()).unwrap_or_default()
}

/// Maps a derived session identity to a provider-scoped stable UUID (SHA-1 name UUID in the OID
/// namespace), "" when there is none.
pub fn derived_session_uuid(provider: &str, metadata_sets: &[&Metadata]) -> String {
    stable_provider_session_uuid(provider, "derived-session", &derived_session_id(metadata_sets))
}

/// Prefers a long-lived execution session and falls back to the derived identity.
pub fn provider_session_uuid(provider: &str, metadata_sets: &[&Metadata]) -> String {
    for metadata in metadata_sets {
        let execution_id = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !execution_id.is_empty() {
            return stable_provider_session_uuid(provider, "execution-session", &execution_id);
        }
    }
    derived_session_uuid(provider, metadata_sets)
}

fn stable_provider_session_uuid(provider: &str, kind: &str, identity_value: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let identity_value = identity_value.trim();
    if provider.is_empty() || identity_value.is_empty() {
        return String::new();
    }
    let identity = ["cli-proxy-api", provider.as_str(), kind, identity_value].join("\0");
    Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
}

/// Maps a derived session identity to Antigravity's negative decimal session id, "" when none.
pub fn derived_antigravity_session_id(metadata_sets: &[&Metadata]) -> String {
    let derived = derived_session_id(metadata_sets);
    if derived.is_empty() {
        return String::new();
    }
    let sum = Sha256::digest(format!("cli-proxy-api:antigravity:derived-session\0{derived}").as_bytes());
    let value = u64::from_be_bytes(sum[..8].try_into().unwrap_or([0; 8])) & 0x7FFF_FFFF_FFFF_FFFF;
    format!("-{value}")
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------- Claude Code session scope

pub const CLAUDE_CODE_SESSION_HEADER: &str = "X-Claude-Code-Session-Id";
pub const CLAUDE_CODE_AGENT_HEADER: &str = "X-Claude-Code-Agent-Id";
pub const CLAUDE_CODE_MAIN_AGENT_ID: &str = "main";

static SESSION_SUFFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_session_([a-f0-9-]+)$").expect("static regex"));

/// Go: `ExtractClaudeCodeSessionID`: `X-Claude-Code-Session-Id` wins over payload metadata.
pub fn extract_claude_code_session_id(payload: &[u8], headers: &HeaderMap) -> String {
    let from_header = header_value_case_insensitive(headers, CLAUDE_CODE_SESSION_HEADER);
    if !from_header.is_empty() {
        return from_header;
    }
    extract_claude_code_session_id_from_payload(payload)
}

/// Go: `ExtractClaudeCodeAgentID`: the agent id header, else the root sentinel `main`.
pub fn extract_claude_code_agent_id(headers: &HeaderMap) -> String {
    let agent_id = header_value_case_insensitive(headers, CLAUDE_CODE_AGENT_HEADER);
    if agent_id.is_empty() { CLAUDE_CODE_MAIN_AGENT_ID.to_string() } else { agent_id }
}

/// Go: `ClaudeCodeExecutionScope`: `claude:<session>:agent:<agent>`, `None` without a session.
pub fn claude_code_execution_scope(payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let session_id = extract_claude_code_session_id(payload, headers);
    if session_id.is_empty() {
        return None;
    }
    Some(format!("claude:{session_id}:agent:{}", extract_claude_code_agent_id(headers)))
}

/// Go: `HeaderValueCaseInsensitive`: the first non-empty trimmed value of the header. (Go also
/// scans non-canonical map keys; `HeaderMap` names are already case-insensitive.)
pub fn header_value_case_insensitive(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go: `HeaderValuesCaseInsensitive`: every non-empty trimmed value of the header.
pub fn header_values_case_insensitive(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Go: `extractClaudeCodeSessionIDFromPayload`: `..._session_<id>` suffix or the JSON `session_id`
/// of `metadata.user_id`.
fn extract_claude_code_session_id_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let user_id = cpa_json::parse(payload).g("metadata.user_id").str();
    if user_id.is_empty() {
        return String::new();
    }
    if let Some(caps) = SESSION_SUFFIX_RE.captures(&user_id) {
        return caps[1].to_string();
    }
    if user_id.starts_with('{') {
        return cpa_json::parse_str(&user_id).g("session_id").str().trim().to_string();
    }
    String::new()
}

/// Go: `ClaudeCodePromptCache`: a deterministic upstream `prompt_cache_key` id for one Claude Code
/// agent (v5 UUID over model and execution scope). `None` without a model or session. Go leaves
/// `Expire` zero; here it is set to the standard id TTL.
pub fn claude_code_prompt_cache(model_name: &str, payload: &[u8], headers: &HeaderMap) -> Option<CodexCache> {
    Some(CodexCache { id: claude_code_prompt_cache_id(model_name, payload, headers)?, expire: Instant::now() + ID_TTL })
}

/// The id of [`claude_code_prompt_cache`] alone.
pub fn claude_code_prompt_cache_id(model_name: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model_name = model_name.trim();
    let scope = claude_code_execution_scope(payload, headers)?;
    if model_name.is_empty() {
        return None;
    }
    let identity = ["cli-proxy-api:codex:claude-code", model_name, scope.as_str()].join("\0");
    Some(uuid_sha1_oid(identity.as_bytes()))
}

/// Name-based (SHA-1, OID namespace) UUID, Go: `uuid.NewSHA1(uuid.NameSpaceOID, data)`.
pub fn uuid_sha1_oid(data: &[u8]) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, data).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn md(pairs: &[(&str, &str)]) -> Metadata {
        pairs.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()
    }

    #[test]
    fn derived_ids_are_provider_scoped_and_stable() {
        let m = md(&[(meta::DERIVED_SESSION_ID, " ctx:v1:abc ")]);
        let a = derived_session_uuid("Claude", &[&m]);
        assert_eq!(a, derived_session_uuid("claude", &[&m]));
        assert_ne!(a, derived_session_uuid("codex", &[&m]));
        assert!(Uuid::parse_str(&a).is_ok());
        assert_eq!(derived_session_uuid("claude", &[&Metadata::new()]), "");
        assert_eq!(derived_session_uuid("", &[&m]), "");
        let ag = derived_antigravity_session_id(&[&m]);
        assert!(ag.starts_with('-') && ag[1..].parse::<i64>().is_ok());
        assert_eq!(ag, derived_antigravity_session_id(&[&Metadata::new(), &m]));
    }

    #[test]
    fn execution_session_wins_over_derived() {
        let m = md(&[(meta::EXECUTION_SESSION_ID, "exec-1"), (meta::DERIVED_SESSION_ID, "d")]);
        let exec = provider_session_uuid("codex", &[&m]);
        assert_ne!(exec, derived_session_uuid("codex", &[&m]));
        let only_derived = md(&[(meta::DERIVED_SESSION_ID, "d")]);
        assert_eq!(provider_session_uuid("codex", &[&only_derived]), derived_session_uuid("codex", &[&only_derived]));
    }

    #[test]
    fn session_resolution_order() {
        let opts = Options::new(cpa_translator::Format::OpenAI);
        assert_eq!(ensure_session_id(Some(""), "c", &opts, b"{}"), Some(String::new()));
        assert_eq!(ensure_session_id(Some("x"), "c", &opts, b"{}"), Some("x".into()));
        assert_eq!(ensure_session_id(None, "c", &opts, b"{}"), Some("c".into()));
    }
}

#[cfg(test)]
mod claude_code_tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).expect("name"),
                HeaderValue::from_str(v).expect("value"),
            );
        }
        h
    }

    #[test]
    fn session_id_from_payload_json_and_suffix() {
        let json_payload = br#"{"metadata":{"user_id":"{\"device_id\":\"d\",\"session_id\":\"cache-session-1\"}"}}"#;
        assert_eq!(extract_claude_code_session_id(json_payload, &HeaderMap::new()), "cache-session-1");
        let legacy = br#"{"metadata":{"user_id":"user_abc_account__session_0a1b-2c"}}"#;
        assert_eq!(extract_claude_code_session_id(legacy, &HeaderMap::new()), "0a1b-2c");
    }

    #[test]
    fn session_header_beats_payload() {
        // The payload in the Go test is not even valid JSON; the header must still win.
        let payload = br#"{"metadata":{"user_id":"{"session_id":"payload-session"}"}}"#;
        let h = headers(&[(CLAUDE_CODE_SESSION_HEADER, "header-session")]);
        assert_eq!(extract_claude_code_session_id(payload, &h), "header-session");
    }

    #[test]
    fn execution_scope_isolates_agents() {
        let root = headers(&[("x-claude-code-session-id", "session-agents")]);
        let child_a = headers(&[("x-claude-code-session-id", "session-agents"), (CLAUDE_CODE_AGENT_HEADER, "agent-a")]);
        assert_eq!(claude_code_execution_scope(b"", &root).as_deref(), Some("claude:session-agents:agent:main"));
        assert_eq!(claude_code_execution_scope(b"", &child_a).as_deref(), Some("claude:session-agents:agent:agent-a"));
        assert_eq!(claude_code_execution_scope(b"", &HeaderMap::new()), None);
    }

    #[test]
    fn prompt_cache_is_deterministic_and_scoped() {
        let root = headers(&[(CLAUDE_CODE_SESSION_HEADER, "session-cache-agents")]);
        let child = headers(&[(CLAUDE_CODE_SESSION_HEADER, "session-cache-agents"), (CLAUDE_CODE_AGENT_HEADER, "agent-a")]);
        let a = claude_code_prompt_cache("gpt-5.4", b"", &root).expect("cache");
        assert_eq!(claude_code_prompt_cache("gpt-5.4", b"", &root).expect("cache").id, a.id);
        assert_ne!(claude_code_prompt_cache("gpt-5.4", b"", &child).expect("cache").id, a.id);
        assert_ne!(claude_code_prompt_cache("gpt-5.5", b"", &root).expect("cache").id, a.id);
        assert!(claude_code_prompt_cache(" ", b"", &root).is_none());
        // From the payload session id (Go TestClaudeCodePromptCacheStableAcrossRequests).
        let payload = br#"{"metadata":{"user_id":"{\"session_id\":\"cache-session-2\"}"}}"#;
        let none = HeaderMap::new();
        let first = claude_code_prompt_cache("grok-composer-2.5-fast", payload, &none).expect("cache");
        assert_eq!(claude_code_prompt_cache("grok-composer-2.5-fast", payload, &none).expect("cache").id, first.id);
    }
}
