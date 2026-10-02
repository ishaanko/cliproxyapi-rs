//! Claude Code session and agent identity extraction (Go: helps/claude_code_session.go). The
//! gin-context header fallback of Go's `claudeCodeHeader` is dropped; callers pass the headers.

use std::sync::LazyLock;
use std::time::Instant;

use cpa_json::J;
use http::HeaderMap;
use regex::Regex;
use uuid::Uuid;

use crate::helps::id_cache::{CodexCache, ID_TTL};

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
    let model_name = model_name.trim();
    let scope = claude_code_execution_scope(payload, headers)?;
    if model_name.is_empty() {
        return None;
    }
    let identity = ["cli-proxy-api:codex:claude-code", model_name, scope.as_str()].join("\0");
    Some(CodexCache {
        id: Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes()).to_string(),
        expire: Instant::now() + ID_TTL,
    })
}

#[cfg(test)]
mod tests {
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
