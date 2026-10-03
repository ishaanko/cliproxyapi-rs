//! Grok Shell client support (Go: internal/client/grokbuild): the `/v1/models` envelope Grok
//! clients expect and the keepalive SSE rewrite for the codex stream.

use http::HeaderMap;
use serde::Serialize;

/// Input model information to be formatted.
#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_length: i64,
    pub reasoning_levels: Vec<String>,
}

/// Reasoning effort level in Grok Shell model entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasoningEffort {
    pub value: String,
}

/// A single model entry formatted for Grok Shell. `context_window` and `reasoning_efforts` are
/// omitted from JSON when empty (Go `omitempty`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub model: String,
    pub name: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub context_window: i64,
    pub api_backend: String,
    pub supported_in_api: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<ReasoningEffort>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// The model list response envelope formatted for Grok Shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Response {
    pub object: String,
    pub data: Vec<ModelEntry>,
}

/// True when the User-Agent indicates a Grok Shell client.
pub fn is_grok_shell_user_agent(user_agent: &str) -> bool {
    user_agent.to_lowercase().contains("grok-shell")
}

/// Builds the Grok Shell formatted model list response.
pub fn build_response(models: &[ModelInfo]) -> Response {
    let data = models
        .iter()
        .map(|m| {
            let name = if m.display_name.is_empty() { &m.id } else { &m.display_name };
            let reasoning_efforts = m
                .reasoning_levels
                .iter()
                .map(|level| level.trim())
                .filter(|level| !level.is_empty())
                .map(|level| ReasoningEffort { value: level.to_string() })
                .collect();
            ModelEntry {
                id: m.id.clone(),
                model: m.id.clone(),
                name: name.clone(),
                context_window: m.context_length.max(0),
                api_backend: "responses".into(),
                supported_in_api: true,
                reasoning_efforts,
            }
        })
        .collect();
    Response { object: "list".into(), data }
}

const KEEPALIVE_SSE_COMMENT: &[u8] = b": keepalive\n\n";

/// The standard SSE comment used for keepalive.
pub fn keepalive_sse_comment() -> Vec<u8> {
    KEEPALIVE_SSE_COMMENT.to_vec()
}

/// True when the user agent contains `grok-pager` or `grok-shell`.
pub fn is_grok_client_user_agent(user_agent: &str) -> bool {
    let ua = user_agent.to_lowercase();
    ua.contains("grok-pager") || ua.contains("grok-shell")
}

/// True when any `User-Agent` header value indicates a Grok client.
pub fn is_grok_client_headers(headers: &HeaderMap) -> bool {
    headers
        .get_all(http::header::USER_AGENT)
        .iter()
        .any(|v| is_grok_client_user_agent(&String::from_utf8_lossy(v.as_bytes())))
}

/// True when a JSON payload has `"type":"keepalive"` (gjson `type` string).
pub fn is_keepalive_payload(payload: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(|s| s == "keepalive")))
        .unwrap_or(false)
}

/// True when an SSE line is a keepalive `event:` or `data:` frame.
pub fn is_keepalive_sse_line(line: &[u8]) -> bool {
    let trimmed = line.trim_ascii();
    if let Some(name) = trimmed.strip_prefix(b"event:") {
        return name.trim_ascii() == b"keepalive";
    }
    if let Some(data) = trimmed.strip_prefix(b"data:") {
        return is_keepalive_payload(data.trim_ascii());
    }
    false
}

/// For Grok clients, a keepalive SSE line becomes an SSE comment and `Some(comment)` is returned;
/// any other line (or a non-Grok client) yields `None` and the caller forwards the line as is.
pub fn transform_keepalive_sse_line(line: &[u8], is_grok_client: bool) -> Option<Vec<u8>> {
    if is_grok_client && is_keepalive_sse_line(line) {
        return Some(keepalive_sse_comment());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_shell_user_agent() {
        for (ua, want) in [
            ("grok-shell/0.2.119 (macos; aarch64)", true),
            ("grok-pager/0.2.119 grok-shell/0.2.119 (macos; aarch64)", true),
            ("GROK-PAGER/1.0 GROK-SHELL/1.0", true),
            ("curl/8.7.1", false),
        ] {
            assert_eq!(is_grok_shell_user_agent(ua), want, "{ua}");
        }
    }

    #[test]
    fn build_response_maps_entries() {
        let response = build_response(&[
            ModelInfo {
                id: "grok-4".into(),
                display_name: "Grok 4".into(),
                context_length: 256000,
                reasoning_levels: vec!["high".into()],
            },
            ModelInfo { id: "plain-model".into(), ..Default::default() },
        ]);
        assert_eq!(response.object, "list");
        assert_eq!(response.data.len(), 2);
        let entry = &response.data[0];
        assert_eq!((entry.id.as_str(), entry.model.as_str(), entry.name.as_str()), ("grok-4", "grok-4", "Grok 4"));
        assert_eq!(entry.context_window, 256000);
        assert_eq!(entry.api_backend, "responses");
        assert!(entry.supported_in_api);
        assert_eq!(entry.reasoning_efforts, vec![ReasoningEffort { value: "high".into() }]);
        let plain = &response.data[1];
        assert_eq!(plain.name, "plain-model");
        // omitempty fields are absent from the wire form
        let json = serde_json::to_string(plain).unwrap();
        assert_eq!(
            json,
            r#"{"id":"plain-model","model":"plain-model","name":"plain-model","api_backend":"responses","supported_in_api":true}"#
        );
    }

    #[test]
    fn grok_client_user_agent() {
        for (ua, want) in [
            ("grok-shell/0.2.119 (macos; aarch64)", true),
            ("grok-pager/1.0.5 grok-shell/1.0.5 (linux; x86_64)", true),
            ("grok-pager/1.0.5", true),
            ("GROK-PAGER/1.0", true),
            ("GROK-SHELL/1.0", true),
            ("curl/8.7.1", false),
            ("openai-python/1.0.0", false),
            ("", false),
        ] {
            assert_eq!(is_grok_client_user_agent(ua), want, "{ua}");
        }
    }

    #[test]
    fn grok_client_headers() {
        let mut h = HeaderMap::new();
        assert!(!is_grok_client_headers(&h));
        h.insert("user-agent", "curl/8.7.1".parse().unwrap());
        assert!(!is_grok_client_headers(&h));
        h.append("User-Agent", "grok-pager/1.0.5".parse().unwrap());
        assert!(is_grok_client_headers(&h));
    }

    #[test]
    fn keepalive_payload_and_lines() {
        for (p, want) in [
            (&br#"{"type":"keepalive","sequence_number":3}"#[..], true),
            (br#"{"type":"keepalive"}"#, true),
            (br#"{"type":"response.created"}"#, false),
            (b"", false),
        ] {
            assert_eq!(is_keepalive_payload(p), want);
        }
        for (line, want) in [
            (&b"event: keepalive"[..], true),
            (b"event: keepalive\n", true),
            (b"  event: keepalive  ", true),
            (br#"data: {"type":"keepalive","sequence_number":3}"#, true),
            (b"event: response.created", false),
            (b"event: keepalive-other", false),
            (br#"data: {"type":"response.created"}"#, false),
            (b"", false),
        ] {
            assert_eq!(is_keepalive_sse_line(line), want, "{}", String::from_utf8_lossy(line));
        }
    }

    #[test]
    fn transform_keepalive_lines() {
        let comment = keepalive_sse_comment();
        assert_eq!(transform_keepalive_sse_line(b"event: keepalive", true), Some(comment.clone()));
        assert_eq!(
            transform_keepalive_sse_line(br#"data: {"type":"keepalive","sequence_number":3}"#, true),
            Some(comment)
        );
        assert_eq!(transform_keepalive_sse_line(br#"data: {"type":"response.created"}"#, true), None);
        assert_eq!(transform_keepalive_sse_line(b"event: keepalive", false), None);
    }
}
