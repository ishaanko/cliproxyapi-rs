//! Local Claude input-token estimate and the one-shot `message_start` patch for translated
//! streams (Go: helps/claude_input_tokens.go).

use cpa_json::{J, Value};
use cpa_translator::{Ctx, Format, Param};

use crate::claude::signing::{json_valid, value_span};
use crate::helps::apply_patch::apply_patch_translation_error;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::text::trim_space;
use crate::helps::token_count::{Tokenizer, tokenizer_for_model};

/// Tracks the one-time input token update for a translated Claude stream (Go:
/// `ClaudeInputTokenState`).
#[derive(Debug, Clone)]
pub struct ClaudeInputTokenState {
    upstream_format: Format,
    response_format: Format,
    original_request: Vec<u8>,
    handled: bool,
}

impl ClaudeInputTokenState {
    /// Request-scoped state; the estimate only runs for a Claude client served by a non-Claude
    /// upstream with a Claude response (Go: `NewClaudeInputTokenState`).
    pub fn new(source_format: Format, upstream_format: Format, response_format: Format, original_request: &[u8]) -> Self {
        let enabled = source_format == Format::Claude && upstream_format != Format::Claude && response_format == Format::Claude;
        Self { upstream_format, response_format, original_request: original_request.to_vec(), handled: !enabled }
    }

    /// Whether the patch already happened or is not applicable.
    pub fn handled(&self) -> bool {
        self.handled
    }

    /// Patches the first `message_start` found in `chunks` (at most once per state) (Go: `apply`).
    pub fn apply(&mut self, chunks: &mut [Vec<u8>]) {
        if self.handled {
            return;
        }
        for chunk in chunks.iter_mut() {
            if let Some(updated) = self.apply_chunk(chunk) {
                self.handled = true;
                if let Some(updated) = updated {
                    *chunk = updated;
                }
                break;
            }
        }
    }

    /// `None` when the chunk has no `message_start`; `Some(None)` when it has one that stays
    /// unchanged; `Some(Some(bytes))` with the patched chunk. Line endings, other events and the
    /// bytes around the payload are preserved.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |i| i + line_start);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let indent = line.iter().position(|b| !matches!(b, b' ' | b'\t')).unwrap_or(line.len());
            let trimmed_left = &line[indent..];
            if trimmed_left.starts_with(b"data:") {
                let mut payload_offset = line.len() - trimmed_left.len() + 5;
                while payload_offset < line.len() && matches!(line[payload_offset], b' ' | b'\t') {
                    payload_offset += 1;
                }
                let mut payload_end = line.len();
                while payload_end > payload_offset && matches!(line[payload_end - 1], b' ' | b'\t') {
                    payload_end -= 1;
                }
                let payload = &line[payload_offset..payload_end];
                let parsed = cpa_json::parse(payload);
                if parsed.g("type").str() == "message_start" {
                    let input_tokens = parsed.g("message.usage.input_tokens");
                    if input_tokens.exists() && input_tokens.int() != 0 {
                        return Some(None);
                    }
                    let count = match self.estimate() {
                        Ok(count) => count,
                        Err(err) => {
                            self.log_estimate_error(&err);
                            return Some(None);
                        }
                    };
                    if count == 0 {
                        return Some(None);
                    }
                    let updated_payload = set_input_tokens(payload, parsed, count);
                    let mut updated = Vec::with_capacity(chunk.len() + updated_payload.len());
                    updated.extend_from_slice(&chunk[..line_start + payload_offset]);
                    updated.extend_from_slice(&updated_payload);
                    updated.extend_from_slice(&chunk[line_start + payload_end..]);
                    return Some(Some(updated));
                }
            }

            if line_end == chunk.len() {
                break;
            }
            line_start = line_end + 1;
        }
        None
    }

    fn estimate(&self) -> Result<i64, String> {
        count_claude_input_tokens(&self.original_request)
    }

    /// Logs without the request body (it may hold user content).
    fn log_estimate_error(&self, err: &str) {
        tracing::warn!(
            upstream_format = %self.upstream_format,
            response_format = %self.response_format,
            error = %err,
            "failed to estimate Claude input tokens"
        );
    }
}

/// Sets `message.usage.input_tokens` like sjson: an existing number is replaced in place, a missing
/// one is appended to `usage` (creating `usage` and `message` as needed).
fn set_input_tokens(payload: &[u8], mut parsed: Value, count: i64) -> Vec<u8> {
    if let Some(span) = value_span(payload, "message.usage.input_tokens") {
        let mut out = Vec::with_capacity(payload.len() + 8);
        out.extend_from_slice(&payload[..span.start]);
        out.extend_from_slice(count.to_string().as_bytes());
        out.extend_from_slice(&payload[span.end..]);
        return out;
    }
    cpa_json::set(&mut parsed, "message.usage.input_tokens", count);
    cpa_json::to_vec(&parsed)
}

/// Translates a stream chunk and estimates Claude `message_start` input usage once (Go:
/// `TranslateStreamWithClaudeInputTokens`). `upstream_format` produced `raw`; output is in
/// `response_format`. A recorded tool-input error skips post-processing so it is not masked.
#[allow(clippy::too_many_arguments)]
pub fn translate_stream_with_claude_input_tokens(
    ctx: &Ctx,
    upstream_format: Format,
    response_format: Format,
    model: &str,
    original_request: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
    state: Option<&mut ClaudeInputTokenState>,
) -> Vec<Vec<u8>> {
    let mut chunks = cpa_translator::translate_stream(ctx, upstream_format, response_format, model, original_request, request, raw, param);
    if apply_patch_translation_error(param).is_some() {
        return chunks;
    }
    if response_format == Format::OpenAIResponse {
        for chunk in &mut chunks {
            *chunk = ensure_responses_usage_details(chunk);
        }
    }
    if let Some(state) = state {
        state.apply(&mut chunks);
    }
    chunks
}

/// The O200kBase tokenizer (the model id only selects the encoding).
fn claude_input_tokenizer() -> Result<Tokenizer, String> {
    tokenizer_for_model("gpt-4o").map_err(|e| format!("initialize O200kBase tokenizer: {e}"))
}

/// Estimates tokens for a Claude request with the O200kBase tokenizer (Go: `CountClaudeInputTokens`).
/// Counts system text, message text, thinking, tool use/result content, tool definitions and the
/// tool choice; images, audio, video, metadata and sampling fields are ignored.
pub fn count_claude_input_tokens(payload: &[u8]) -> Result<i64, String> {
    let enc = claude_input_tokenizer()?;
    let segments = collect_claude_input_token_segments(payload).map_err(|e| format!("count Claude input tokens: {e}"))?;
    if segments.is_empty() {
        return Ok(0);
    }
    Ok(enc.count(&segments.join("\n")) as i64)
}

/// The trimmed text pieces that are tokenized, in request order (Go:
/// `collectClaudeInputTokenSegments`).
pub fn collect_claude_input_token_segments(payload: &[u8]) -> Result<Vec<String>, String> {
    if trim_space(payload).is_empty() {
        return Ok(Vec::new());
    }
    if !json_valid(payload) {
        return Err("invalid Claude request JSON".to_string());
    }

    let text = String::from_utf8_lossy(payload);
    let root = text.as_ref();
    let mut segments = Vec::with_capacity(32);
    if let Some(system) = raw_at(root, "system") {
        collect_system(system, &mut segments);
    }
    if let Some(messages) = raw_at(root, "messages") {
        if kind(messages) == Kind::Array {
            for message in children(messages) {
                append_string(&mut segments, &gstring(message, "role"));
                if let Some(content) = raw_at(message, "content") {
                    collect_content(content, &mut segments);
                }
            }
        }
    }
    if let Some(tools) = raw_at(root, "tools") {
        if kind(tools) == Kind::Array {
            for tool in children(tools) {
                append_string(&mut segments, &gstring(tool, "type"));
                append_string(&mut segments, &gstring(tool, "name"));
                append_string(&mut segments, &gstring(tool, "description"));
                append_json(&mut segments, raw_at(tool, "input_schema"));
            }
        }
    }
    if let Some(tool_choice) = raw_at(root, "tool_choice") {
        if kind(tool_choice) == Kind::String {
            append_string(&mut segments, &raw_to_string(tool_choice));
        } else {
            append_string(&mut segments, &gstring(tool_choice, "type"));
            append_string(&mut segments, &gstring(tool_choice, "name"));
        }
    }
    Ok(segments)
}

// ---- raw JSON access: gjson `String()` semantics over slices of the original payload ----

#[derive(PartialEq, Eq)]
enum Kind {
    String,
    Array,
    Object,
    Other,
}

fn kind(raw: &str) -> Kind {
    match raw.trim_start().as_bytes().first() {
        Some(b'"') => Kind::String,
        Some(b'[') => Kind::Array,
        Some(b'{') => Kind::Object,
        _ => Kind::Other,
    }
}

fn raw_at<'a>(raw: &'a str, path: &str) -> Option<&'a str> {
    cpa_json::raw_at(raw.as_bytes(), path)
}

fn children(raw: &str) -> Vec<&str> {
    cpa_json::raw_children(raw.as_bytes(), "")
}

/// gjson `Result.String()` of a raw value: strings decoded, objects and arrays as their raw text,
/// scalars as text.
fn raw_to_string(raw: &str) -> String {
    match kind(raw) {
        Kind::Array | Kind::Object => raw.to_string(),
        _ => cpa_json::Res::owned(cpa_json::parse_str(raw)).str(),
    }
}

/// `Get(path).String()` relative to `raw`; "" when missing.
fn gstring(raw: &str, path: &str) -> String {
    raw_at(raw, path).map(raw_to_string).unwrap_or_default()
}

/// Go: `appendClaudeTokenString`.
fn append_string(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

/// Go: `appendClaudeTokenJSON`: strings verbatim, other values compacted (key order kept).
fn append_json(segments: &mut Vec<String>, value: Option<&str>) {
    let Some(raw) = value else { return };
    if kind(raw) == Kind::String {
        append_string(segments, &raw_to_string(raw));
        return;
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    append_string(segments, &compact_json(raw));
}

/// `json.Compact`: drops whitespace outside strings.
fn compact_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if matches!(c, ' ' | '\t' | '\r' | '\n') {
            continue;
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }
    out
}

// ---- segment collectors ----

fn collect_system(system: &str, segments: &mut Vec<String>) {
    match kind(system) {
        Kind::String => append_string(segments, &raw_to_string(system)),
        Kind::Array => {
            for part in children(system) {
                if kind(part) == Kind::String {
                    append_string(segments, &raw_to_string(part));
                } else if gstring(part, "type") == "text" {
                    append_string(segments, &gstring(part, "text"));
                }
            }
        }
        _ => {}
    }
}

/// Go: `collectClaudeContentTokenSegments` over the raw text of an existing `content` value.
fn collect_content(content: &str, segments: &mut Vec<String>) {
    match kind(content) {
        Kind::String => append_string(segments, &raw_to_string(content)),
        Kind::Array => {
            for part in children(content) {
                collect_content(part, segments);
            }
        }
        Kind::Object => collect_content_object(content, segments),
        Kind::Other => {}
    }
}

fn collect_content_object(content: &str, segments: &mut Vec<String>) {
    let s = |path: &str| gstring(content, path);
    let nested = |segments: &mut Vec<String>, path: &str| {
        if let Some(raw) = raw_at(content, path) {
            collect_content(raw, segments);
        }
    };
    match s("type").as_str() {
        "text" => append_string(segments, &s("text")),
        "thinking" => append_string(segments, &s("thinking")),
        "document" => collect_document(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            append_string(segments, &s("id"));
            append_string(segments, &s("name"));
            append_json(segments, raw_at(content, "input"));
        }
        "tool_result"
        | "mcp_tool_result"
        | "web_search_tool_result"
        | "web_fetch_tool_result"
        | "code_execution_tool_result"
        | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            append_string(segments, &s("tool_use_id"));
            append_string(segments, &s("tool_call_id"));
            nested(segments, "content");
        }
        "web_search_result" | "search_result" => {
            if let Some(source) = raw_at(content, "source").filter(|r| kind(r) == Kind::String) {
                append_string(segments, &raw_to_string(source));
            }
            append_string(segments, &s("title"));
            append_string(segments, &s("url"));
            append_string(segments, &s("page_age"));
            nested(segments, "content");
        }
        "web_fetch_result" => {
            append_string(segments, &s("url"));
            append_string(segments, &s("retrieved_at"));
            nested(segments, "content");
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            append_string(segments, &s("stdout"));
            append_string(segments, &s("stderr"));
            append_string(segments, &s("return_code"));
            nested(segments, "content");
            nested(segments, "output");
        }
        "tool_reference" => append_string(segments, &s("tool_name")),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => append_json(segments, Some(content)),
        _ => append_string(segments, &s("text")),
    }
}

/// Go: `collectClaudeDocumentTokenSegments` (only text-source documents count).
fn collect_document(document: &str, segments: &mut Vec<String>) {
    let Some(source) = raw_at(document, "source") else { return };
    if gstring(source, "type") != "text" {
        return;
    }
    append_string(segments, &gstring(document, "title"));
    append_string(segments, &gstring(document, "context"));
    append_string(segments, &gstring(source, "data"));
    append_string(segments, &gstring(source, "content"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(chunks: &[Vec<u8>]) -> String {
        chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect()
    }

    fn message_start_input_tokens(chunks: &[Vec<u8>]) -> i64 {
        for chunk in chunks {
            for line in String::from_utf8_lossy(chunk).split('\n') {
                let trimmed = line.trim();
                if let Some(payload) = trimmed.strip_prefix("data:") {
                    let v = cpa_json::parse_str(payload.trim());
                    if v.g("type").str() == "message_start" {
                        return v.g("message.usage.input_tokens").int();
                    }
                }
            }
        }
        0
    }

    #[test]
    fn collects_segments_in_request_order() {
        let payload = br#"{
        "model":"claude-test",
        "system":[
            {"type":"text","text":"Follow repository rules.","cache_control":{"type":"ephemeral"}},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-system-image"}}
        ],
        "messages":[
            {"role":"user","content":[
                {"type":"text","text":"Review the implementation."},
                {"type":"document","source":{"type":"text","data":"Reference document text."}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-image"}}
            ]},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"Inspect the relevant files.","signature":"ignored-signature"},
                {"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"main.go"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":[
                    {"type":"text","text":"package main"},
                    {"type":"image","source":{"type":"base64","data":"ignored-tool-image"}}
                ]}
            ]}
        ],
        "tools":[{
            "name":"read_file",
            "description":"Reads a repository file.",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"}}},
            "cache_control":{"type":"ephemeral"}
        }],
        "tool_choice":{"type":"tool","name":"read_file"},
        "metadata":{"user_id":"ignored-metadata"},
        "max_tokens":4096,
        "stream":true
    }"#;
        let got = collect_claude_input_token_segments(payload).expect("segments");
        let want = [
            "Follow repository rules.",
            "user",
            "Review the implementation.",
            "Reference document text.",
            "assistant",
            "Inspect the relevant files.",
            "toolu_1",
            "read_file",
            r#"{"path":"main.go"}"#,
            "user",
            "toolu_1",
            "package main",
            "read_file",
            "Reads a repository file.",
            r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
            "tool",
            "read_file",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn includes_known_tool_results() {
        let payload = br#"{
        "messages":[{"role":"user","content":[
            {"type":"web_search_tool_result","tool_use_id":"ws_tool_1","content":[
                {"type":"web_search_result","source":"Search source","title":"Search result title","url":"https://search.example/result","page_age":"1 day","encrypted_content":"ignored-secret"}
            ]},
            {"type":"web_fetch_tool_result","tool_use_id":"fetch_tool_1","content":{
                "type":"web_fetch_result","url":"https://docs.example/page","retrieved_at":"2026-07-22T00:00:00Z","content":{
                    "type":"document","title":"Fetched document","source":{"type":"text","data":"Fetched body"}
                }
            }},
            {"type":"bash_code_execution_tool_result","tool_use_id":"bash_tool_1","content":{
                "type":"bash_code_execution_result","stdout":"command output","stderr":"command error","return_code":1,
                "content":[{"type":"text","text":"additional output"}]
            }},
            {"type":"tool_result","tool_use_id":"toolu_1","content":[
                {"type":"tool_reference","tool_name":"proxy_mcp__nia__manage_resource"}
            ]}
        ]}]
    }"#;
        let segments = collect_claude_input_token_segments(payload).expect("segments");
        let joined = format!("\n{}\n", segments.join("\n"));
        for want in [
            "ws_tool_1",
            "Search source",
            "Search result title",
            "https://search.example/result",
            "1 day",
            "fetch_tool_1",
            "https://docs.example/page",
            "2026-07-22T00:00:00Z",
            "Fetched document",
            "Fetched body",
            "bash_tool_1",
            "command output",
            "command error",
            "1",
            "additional output",
            "toolu_1",
            "proxy_mcp__nia__manage_resource",
        ] {
            assert!(joined.contains(&format!("\n{want}\n")), "missing {want:?} in {segments:?}");
        }
        assert!(!joined.contains("ignored-secret"));
    }

    #[test]
    fn raw_values_keep_escapes_and_key_order() {
        // Tool input is tokenized from its original text: escapes stay escaped, whitespace goes.
        let payload = br#"{"messages":[{"role":"user","content":[{"type":"tool_use","id":"t","name":"n","input":{ "b": "caf\u00e9", "a": [1, 2] }}]}]}"#;
        let got = collect_claude_input_token_segments(payload).expect("segments");
        assert_eq!(got, ["user", "t", "n", r#"{"b":"caf\u00e9","a":[1,2]}"#]);
        // A content object without a type is tokenized as compact JSON.
        let payload = br#"{"messages":[{"role":"user","content":{"k": "v"}}]}"#;
        assert_eq!(collect_claude_input_token_segments(payload).expect("segments"), ["user", r#"{"k":"v"}"#]);
    }

    #[test]
    fn count_excludes_multimedia_and_control_fields() {
        let base = br#"{
        "system":"System text.",
        "messages":[{"role":"user","content":[{"type":"text","text":"User text."}]}],
        "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"}}]
    }"#;
        let with_excluded = br#"{
        "model":"claude-test",
        "system":"System text.",
        "messages":[{"role":"user","content":[
            {"type":"text","text":"User text."},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"very-large-image-data"}},
            {"type":"input_audio","source":{"type":"base64","data":"very-large-audio-data"}},
            {"type":"video","source":{"type":"url","url":"https://example.com/video.mp4"}},
            {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"very-large-pdf-data"}}
        ]}],
        "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
        "metadata":{"large_wrapper":"ignored"},
        "max_tokens":8192,
        "temperature":0.8,
        "top_p":0.9,
        "thinking":{"type":"enabled","budget_tokens":4096},
        "stream":true
    }"#;
        let base_count = count_claude_input_tokens(base).expect("base count");
        assert!(base_count > 0);
        assert_eq!(count_claude_input_tokens(with_excluded).expect("excluded count"), base_count);
        assert_eq!(count_claude_input_tokens(b"  ").expect("empty"), 0);
        assert!(count_claude_input_tokens(br#"{"messages":["x""#).expect_err("invalid json").contains("invalid Claude request JSON"));
    }

    /// Counts produced by the Go `CountClaudeInputTokens` for the same payloads.
    #[test]
    fn counts_match_go() {
        let cases: [(&str, i64); 4] = [
            (r#"{"system":"System text.","messages":[{"role":"user","content":[{"type":"text","text":"User text."}]}],"tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"}}]}"#, 19),
            (r#"{"messages":[{"role":"user","content":[{"type":"tool_use","id":"t","name":"n","input":{ "b": "caf\u00e9", "a": [1, 2] }}]}]}"#, 21),
            ("{\"messages\":[{\"role\":\"user\",\"content\":\"worker 3 iteration 4 \u{4f60}\u{597d}\"}],\"tool_choice\":\"auto\"}", 12),
            (r#"{"system":[{"type":"text","text":"Follow."},"plain string"],"messages":[{"role":"user","content":{"k": "v"}},{"role":"assistant","content":[{"type":"mystery","text":"zz"}]}],"tool_choice":{"type":"tool","name":"x"}}"#, 19),
        ];
        for (payload, want) in cases {
            assert_eq!(count_claude_input_tokens(payload.as_bytes()).expect("count"), want, "{payload}");
        }
    }

    #[test]
    fn state_preserves_crlf_and_non_target_events() {
        let original = br#"{"messages":[{"role":"user","content":"Hello."}]}"#;
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::OpenAI, Format::Claude, original);
        let chunk = "event: message_start\r\ndata:  {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}  \r\n\r\nevent: ping\r\ndata: {\"type\":\"ping\",\"value\":\"keep\"}\r\n\r\n";
        let mut chunks = vec![chunk.as_bytes().to_vec()];
        state.apply(&mut chunks);
        let tokens = message_start_input_tokens(&chunks);
        assert!(tokens > 0);
        let want = format!(
            "event: message_start\r\ndata:  {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{tokens},\"output_tokens\":0}}}}}}  \r\n\r\nevent: ping\r\ndata: {{\"type\":\"ping\",\"value\":\"keep\"}}\r\n\r\n"
        );
        assert_eq!(text_of(&chunks), want);
        assert!(state.handled());
    }

    #[test]
    fn state_patches_missing_and_preserves_non_zero() {
        let original = br#"{"messages":[{"role":"user","content":"Hello."}]}"#;
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::OpenAI, Format::Claude, original);
        let mut chunks = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":0}}}\n\n".to_vec()];
        state.apply(&mut chunks);
        let patched = text_of(&chunks);
        assert!(message_start_input_tokens(&chunks) > 0, "{patched}");
        assert!(patched.contains(r#""usage":{"output_tokens":0,"input_tokens":"#), "{patched}");

        // A second message_start is left alone once handled.
        let mut second = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec()];
        state.apply(&mut second);
        assert_eq!(message_start_input_tokens(&second), 0);

        // A non-zero upstream value is preserved even when the request is not valid JSON.
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::OpenAI, Format::Claude, b"not valid json");
        let mut chunks = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":73}}}\n\n".to_vec()];
        state.apply(&mut chunks);
        assert_eq!(message_start_input_tokens(&chunks), 73);
        assert!(state.handled());
    }

    #[test]
    fn state_skips_unsupported_flows_and_invalid_requests() {
        let original = br#"{"messages":[{"role":"user","content":"Hello."}]}"#;
        let cases = [
            (Format::OpenAI, Format::Gemini, Format::Claude),
            (Format::Claude, Format::Claude, Format::Claude),
            (Format::Claude, Format::OpenAI, Format::OpenAI),
        ];
        for (source, upstream, response) in cases {
            let mut state = ClaudeInputTokenState::new(source, upstream, response, original);
            assert!(state.handled(), "{source} {upstream} {response}");
            let mut chunks = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec()];
            state.apply(&mut chunks);
            assert_eq!(message_start_input_tokens(&chunks), 0);
        }

        // An estimate failure keeps zero and still marks the state handled.
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::OpenAI, Format::Claude, br#"{"messages":["sensitive-original-request""#);
        let mut chunks = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec()];
        state.apply(&mut chunks);
        assert_eq!(message_start_input_tokens(&chunks), 0);
        assert!(state.handled());
    }
}
