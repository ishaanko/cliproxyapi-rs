//! Request-log bodies of the Devin upstream (Go: helps `BuildDevinUpstreamLogBody`,
//! `BuildDevinUpstreamResponseLogBody` and the `DevinUpstream*Log` structs).
//!
//! The Connect-RPC body is binary, so the log carries a readable rendition instead: the decoded
//! request as `json.MarshalIndent` shapes it (struct field order, `omitempty`, HTML escaping,
//! two-space indent) next to the intermediate interactions document.

use base64::Engine;
use cpa_core::util::go_json_string;
use cpa_json::Value;

use super::wire::{Prompt, Tool, ToolCall, Usage, go_lossy};

/// Go's `json.Indent(src, "", "  ")`: whitespace outside strings is dropped and re-laid out,
/// everything else (key order, number text, escapes) is kept. `None` when `src` is not JSON.
pub fn json_indent(src: &[u8]) -> Option<Vec<u8>> {
    if !cpa_json::valid(src) {
        return None;
    }
    let mut out = Vec::with_capacity(src.len() * 2);
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', depth * 2));
    };
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        i += 1;
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'"' => {
                in_string = true;
                out.push(c);
            }
            b'{' | b'[' => {
                out.push(c);
                // `{}` and `[]` stay on one line.
                let mut j = i;
                while j < src.len() && matches!(src[j], b' ' | b'\t' | b'\r' | b'\n') {
                    j += 1;
                }
                if j < src.len() && (src[j] == b'}' || src[j] == b']') {
                    out.push(src[j]);
                    i = j + 1;
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            b',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

/// `json.RawMessage` inside `json.Marshal`: compacted, with `<`, `>`, `&` and U+2028/2029 escaped.
fn compact_escaped(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let text = String::from_utf8_lossy(raw);
    let (mut in_string, mut escaped) = (false, false);
    for ch in text.chars() {
        if in_string {
            match ch {
                _ if escaped => {
                    escaped = false;
                    out.push(ch);
                }
                '\\' => {
                    escaped = true;
                    out.push(ch);
                }
                '"' => {
                    in_string = false;
                    out.push(ch);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(ch),
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !matches!(ch, ' ' | '\t' | '\r' | '\n') {
            out.push(ch);
        }
    }
    out
}

/// A compact JSON object written in declaration order.
#[derive(Default)]
struct Obj(String);

impl Obj {
    fn raw(&mut self, key: &str, raw: &str) {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push_str(&go_json_string(key));
        self.0.push(':');
        self.0.push_str(raw);
    }

    fn string(&mut self, key: &str, value: &str) {
        self.raw(key, &go_json_string(value));
    }

    /// `omitempty` string.
    fn string_opt(&mut self, key: &str, value: &str) {
        if !value.is_empty() {
            self.string(key, value);
        }
    }

    fn int(&mut self, key: &str, value: i64) {
        self.raw(key, &value.to_string());
    }

    fn finish(mut self) -> String {
        if self.0.is_empty() {
            self.0.push('{');
        }
        self.0.push('}');
        self.0
    }
}

fn array(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(","))
}

/// Go's `DevinToolCall` has no JSON tags: the keys are the Go field names.
fn tool_call_json(call: &ToolCall) -> String {
    let mut o = Obj::default();
    o.string("ID", &call.id);
    o.string("Name", &call.name);
    o.string("Arguments", &call.arguments);
    o.finish()
}

/// `formatSignatureForLog`: sealed and printable signatures as text, anything else base64.
pub fn format_signature_for_log(sig: &[u8]) -> String {
    if sig.is_empty() {
        return String::new();
    }
    if sig.starts_with(b"sealed.v1.") {
        return go_lossy(sig);
    }
    if std::str::from_utf8(sig).is_ok() && sig.iter().all(|c| (32..=126).contains(c)) {
        return go_lossy(sig);
    }
    base64::engine::general_purpose::STANDARD.encode(sig)
}

/// `BuildDevinUpstreamLogBody`: the decoded request, preceded by the intermediate interactions
/// document when the client did not speak interactions itself.
#[allow(clippy::too_many_arguments)]
pub fn request_body(
    interactions_payload: &[u8],
    is_interactions_source: bool,
    chat_model_uid: &str,
    system_prompt: &str,
    prompts: &[Prompt],
    tools: &[Tool],
    temperature: Option<f64>,
    max_tokens: i64,
    session_id: &str,
    cascade_id: &str,
) -> Vec<u8> {
    let prompt_items: Vec<String> = prompts
        .iter()
        .map(|p| {
            let role = match p.source {
                2 => "assistant",
                4 => "tool",
                _ => "user",
            };
            let mut o = Obj::default();
            o.string_opt("id", &p.message_id);
            o.int("source", i64::from(p.source));
            o.string_opt("role", role);
            o.string_opt("content", &p.content);
            o.string_opt("thinking", &p.thinking);
            o.string_opt("signature", &format_signature_for_log(&p.signature));
            o.string_opt("signature_type", &p.signature_type);
            if !p.tool_calls.is_empty() {
                o.raw("tool_calls", &array(p.tool_calls.iter().map(tool_call_json)));
            }
            o.string_opt("tool_call_id", &p.tool_call_id);
            if !p.images.is_empty() {
                let images = p.images.iter().map(|img| {
                    let mut i = Obj::default();
                    i.string("mime_type", &img.mime_type);
                    i.int("data_len", img.base64_data.len() as i64);
                    i.finish()
                });
                o.raw("images", &array(images));
            }
            o.finish()
        })
        .collect();

    let tool_items: Vec<String> = super::wire::upstream_tools(tools)
        .iter()
        .map(|t| {
            let mut o = Obj::default();
            o.string("name", &t.name);
            o.string_opt("description", &t.description);
            if !t.parameters.is_empty() && cpa_json::valid(&t.parameters) {
                o.raw("parameters", &compact_escaped(&t.parameters));
            }
            o.finish()
        })
        .collect();

    let mut req = Obj::default();
    req.string("model", chat_model_uid);
    req.string_opt("session_id", session_id);
    req.string_opt("cascade_id", cascade_id);
    req.string_opt("system_prompt", system_prompt);
    if let Some(t) = temperature {
        let number = serde_json::Number::from_f64(t).map(Value::Number).unwrap_or(Value::Null);
        let text = cpa_core::util::go_json_sorted(&number, cpa_core::util::GoJsonStyle::MARSHAL_ANY).unwrap_or_else(|| "null".into());
        req.raw("temperature", &text);
    }
    if max_tokens != 0 {
        req.int("max_tokens", max_tokens);
    }
    if !prompt_items.is_empty() {
        req.raw("prompts", &array(prompt_items));
    }
    if !tool_items.is_empty() {
        req.raw("tools", &array(tool_items));
    }
    let compact = req.finish();
    let devin_req = json_indent(compact.as_bytes()).unwrap_or_else(|| format!("{{\"model\": {}}}", go_json_string(chat_model_uid)).into_bytes());

    let mut out = Vec::new();
    if !is_interactions_source && !interactions_payload.is_empty() {
        out.extend_from_slice(b"=== INTERMEDIATE INTERACTIONS ===\n");
        out.extend_from_slice(&json_indent(interactions_payload).unwrap_or_else(|| interactions_payload.to_vec()));
        out.extend_from_slice(b"\n\n=== DEVIN UPSTREAM REQUEST ===\n");
    }
    out.extend_from_slice(&devin_req);
    out
}

/// Decoded response frames of one upstream answer (Go: `DevinUpstreamResponseLog`).
#[derive(Debug, Clone, Default)]
pub struct ResponseLog {
    pub status: String,
    pub frames_count: i64,
    pub content: String,
    pub thinking: String,
    pub signature: Vec<u8>,
    pub signature_type: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub unknown_fields: Vec<i32>,
}

fn usage_json(u: &Usage) -> String {
    let mut o = Obj::default();
    o.int("prompt_tokens", u.prompt_tokens);
    o.int("completion_tokens", u.completion_tokens);
    o.int("cached_tokens", u.cached_tokens);
    if u.cache_write_tokens > 0 {
        o.int("cache_write_tokens", u.cache_write_tokens);
    }
    if u.status_code != 0 {
        o.raw("status_code", &u.status_code.to_string());
    }
    o.string_opt("request_id", &u.request_id);
    o.string_opt("model_name", &u.model_name);
    if !u.headers.is_empty() {
        let mut h = Obj::default();
        for (k, v) in &u.headers {
            h.string(k, v);
        }
        o.raw("headers", &h.finish());
    }
    o.finish()
}

impl ResponseLog {
    /// `json.MarshalIndent(log, "", "  ")` with the signature rendered as given.
    fn marshal_indent(&self) -> Vec<u8> {
        let mut o = Obj::default();
        o.string_opt("status", &self.status);
        o.int("frames_count", self.frames_count);
        o.string_opt("content", &self.content);
        o.string_opt("thinking", &self.thinking);
        o.string_opt("signature", &go_lossy(&self.signature));
        o.string_opt("signature_type", &self.signature_type);
        if !self.tool_calls.is_empty() {
            o.raw("tool_calls", &array(self.tool_calls.iter().map(tool_call_json)));
        }
        if let Some(u) = &self.usage {
            o.raw("usage", &usage_json(u));
        }
        if !self.unknown_fields.is_empty() {
            o.raw("unknown_fields", &array(self.unknown_fields.iter().map(i32::to_string)));
        }
        let compact = o.finish();
        json_indent(compact.as_bytes()).unwrap_or_else(|| compact.into_bytes())
    }
}

/// `BuildDevinUpstreamResponseLogBody`.
pub fn response_body(log: Option<&ResponseLog>, interactions: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(log) = log {
        let mut shown = log.clone();
        if !shown.signature.is_empty() {
            shown.signature = format_signature_for_log(&shown.signature).into_bytes();
        }
        out.extend_from_slice(b"=== DEVIN UPSTREAM RESPONSE ===\n");
        out.extend_from_slice(&shown.marshal_indent());
        out.extend_from_slice(b"\n\n");
    }
    if !interactions.is_empty() {
        out.extend_from_slice(b"=== INTERMEDIATE INTERACTIONS ===\n");
        out.extend_from_slice(&json_indent(interactions).unwrap_or_else(|| interactions.to_vec()));
    }
    out
}

/// The summary block appended after a streamed response completed.
pub fn stream_summary(log: &ResponseLog) -> Vec<u8> {
    let mut out = b"\n=== DEVIN UPSTREAM RESPONSE SUMMARY ===\n".to_vec();
    out.extend_from_slice(&log.marshal_indent());
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_matches_go_for_nested_and_empty_values() {
        let out = json_indent(br#" {"a":[1,{"b":"x y"},[],{}],"c":"\"q\""} "#).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": \"x y\"\n    },\n    [],\n    {}\n  ],\n  \"c\": \"\\\"q\\\"\"\n}"
        );
        assert!(json_indent(b"{oops").is_none());
    }

    #[test]
    fn request_body_follows_struct_order_omitempty_and_html_escaping() {
        let prompts = vec![
            Prompt { message_id: "m1".into(), source: 1, content: "a <b> & c".into(), ..Default::default() },
            Prompt {
                source: 2,
                tool_calls: vec![ToolCall { id: "c1".into(), name: "f".into(), arguments: "{}".into() }],
                signature: b"raw\x01sig".to_vec(),
                ..Default::default()
            },
        ];
        let tools = vec![Tool { name: "f".into(), description: String::new(), parameters: br#"{ "type": "object", "x": "<y>" }"#.to_vec() }];
        let body = request_body(br#"{"k":1}"#, false, "uid", "sys", &prompts, &tools, Some(1.0), 7, "s", "");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "=== INTERMEDIATE INTERACTIONS ===\n{\n  \"k\": 1\n}\n\n=== DEVIN UPSTREAM REQUEST ===\n{\n  \"model\": \"uid\",\n  \"session_id\": \"s\",\n  \"system_prompt\": \"sys\",\n  \"temperature\": 1,\n  \"max_tokens\": 7,\n  \"prompts\": [\n    {\n      \"id\": \"m1\",\n      \"source\": 1,\n      \"role\": \"user\",\n      \"content\": \"a \\u003cb\\u003e \\u0026 c\"\n    },\n    {\n      \"source\": 2,\n      \"role\": \"assistant\",\n      \"signature\": \"cmF3AXNpZw==\",\n      \"tool_calls\": [\n        {\n          \"ID\": \"c1\",\n          \"Name\": \"f\",\n          \"Arguments\": \"{}\"\n        }\n      ]\n    }\n  ],\n  \"tools\": [\n    {\n      \"name\": \"f\",\n      \"parameters\": {\n        \"type\": \"object\",\n        \"x\": \"\\u003cy\\u003e\"\n      }\n    }\n  ]\n}"
        );
    }

    #[test]
    fn response_body_lists_log_then_interactions() {
        let log = ResponseLog { status: "completed".into(), frames_count: 3, content: "hi".into(), ..Default::default() };
        let body = response_body(Some(&log), br#"{"id":"i"}"#);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "=== DEVIN UPSTREAM RESPONSE ===\n{\n  \"status\": \"completed\",\n  \"frames_count\": 3,\n  \"content\": \"hi\"\n}\n\n=== INTERMEDIATE INTERACTIONS ===\n{\n  \"id\": \"i\"\n}"
        );
    }
}
