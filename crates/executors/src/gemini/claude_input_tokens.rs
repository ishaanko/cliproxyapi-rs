//! One-time Claude `message_start` input token estimate for translated streams (Go:
//! helps/claude_input_tokens.go).
//!
//! When a Claude client talks to a non-Claude upstream and wants Claude output, the upstream's
//! first usage frame arrives after `message_start`, so the translator emits `input_tokens: 0`.
//! [`ClaudeInputTokenState`] patches the first `message_start` with an o200k estimate of the
//! original Claude request.

use bytes::Bytes;
use cpa_json::{J, Res, Value};
use cpa_translator::{Ctx, Format, Param};

use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::token_count::tokenizer_for_model;
use crate::helps::apply_patch::apply_patch_translation_error;

/// Request-scoped state: estimates once, on the first `message_start` seen.
pub struct ClaudeInputTokenState {
    original_request: Bytes,
    handled: bool,
}

impl ClaudeInputTokenState {
    /// Enabled only for Claude clients served by a non-Claude upstream with Claude output.
    pub fn new(source: Format, upstream: Format, response: Format, original_request: Bytes) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self { original_request, handled: !enabled }
    }

    fn apply(&mut self, chunks: &mut [Vec<u8>]) {
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

    /// `None`: no `message_start` in the chunk. `Some(None)`: found, leave unchanged.
    /// `Some(Some(chunk))`: found and patched.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |i| i + line_start);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let trimmed_left = line.iter().position(|b| *b != b' ' && *b != b'\t').map_or(line.len(), |i| i);
            if line[trimmed_left..].starts_with(b"data:") {
                let mut payload_offset = trimmed_left + 5;
                while payload_offset < line.len() && (line[payload_offset] == b' ' || line[payload_offset] == b'\t') {
                    payload_offset += 1;
                }
                let mut payload_end = line.len();
                while payload_end > payload_offset && (line[payload_end - 1] == b' ' || line[payload_end - 1] == b'\t') {
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
                            tracing::warn!("failed to estimate Claude input tokens: {err}");
                            return Some(None);
                        }
                    };
                    if count == 0 {
                        return Some(None);
                    }
                    let mut updated = parsed;
                    cpa_json::set(&mut updated, "message.usage.input_tokens", count);
                    let updated_payload = cpa_json::to_vec(&updated);
                    let start = line_start + payload_offset;
                    let stop = line_start + payload_end;
                    let mut out = Vec::with_capacity(chunk.len() + updated_payload.len());
                    out.extend_from_slice(&chunk[..start]);
                    out.extend_from_slice(&updated_payload);
                    out.extend_from_slice(&chunk[stop..]);
                    return Some(Some(out));
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
        // `gpt-5` selects the o200k_base encoding, which Go uses for this estimate.
        let enc = tokenizer_for_model("gpt-5").map_err(|e| format!("initialize O200kBase tokenizer: {e}"))?;
        count_claude_input_tokens(&enc, &self.original_request).map_err(|e| format!("count Claude input tokens: {e}"))
    }
}

/// Translates one upstream chunk to the client format; for `openai-response` clients each frame
/// gets usage details, and the Claude input estimate is applied once (Go:
/// TranslateStreamWithClaudeInputTokens).
#[allow(clippy::too_many_arguments)]
pub fn translate_stream_with_claude_input_tokens(
    ctx: &Ctx,
    upstream: Format,
    response: Format,
    model: &str,
    original_request: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
    state: &mut ClaudeInputTokenState,
) -> Vec<Vec<u8>> {
    let mut chunks = cpa_translator::translate_stream(ctx, upstream, response, model, original_request, request, raw, param);
    if apply_patch_translation_error(param).is_some() {
        return chunks;
    }
    if response == Format::OpenAIResponse {
        for chunk in chunks.iter_mut() {
            *chunk = ensure_responses_usage_details(chunk);
        }
    }
    state.apply(&mut chunks);
    chunks
}

/// Estimates tokens for a Claude request with the o200k tokenizer (Go: countClaudeInputTokens).
pub fn count_claude_input_tokens(enc: &crate::helps::token_count::Tokenizer, payload: &[u8]) -> Result<i64, String> {
    let segments = collect_segments(payload)?;
    if segments.is_empty() {
        return Ok(0);
    }
    Ok(enc.count(&segments.join("\n")) as i64)
}

fn collect_segments(payload: &[u8]) -> Result<Vec<String>, String> {
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    if !cpa_json::valid(payload) {
        return Err("invalid Claude request JSON".into());
    }
    let root = cpa_json::parse(payload);
    let mut segments = Vec::with_capacity(32);
    collect_system(&root.g("system"), &mut segments);
    collect_messages(&root.g("messages"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    collect_tool_choice(&root.g("tool_choice"), &mut segments);
    Ok(segments)
}

fn add(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

/// Strings verbatim, other values as compact JSON.
fn add_json(segments: &mut Vec<String>, value: &Res<'_>) {
    if !value.exists() {
        return;
    }
    if let Some(s) = value.as_str() {
        add(segments, s);
        return;
    }
    add(segments, &value.raw());
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    if let Some(s) = system.as_str() {
        add(segments, s);
        return;
    }
    let Some(Value::Array(parts)) = system.v() else { return };
    for part in parts {
        if let Value::String(s) = part {
            add(segments, s);
        } else if part.g("type").str() == "text" {
            add(segments, &part.g("text").str());
        }
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    let Some(Value::Array(items)) = messages.v() else { return };
    for message in items {
        add(segments, &message.g("role").str());
        collect_content(&message.g("content"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    let Some(value) = content.v() else { return };
    match value {
        Value::String(s) => add(segments, s),
        Value::Array(items) => {
            for part in items {
                collect_content(&Res::of(part), segments);
            }
        }
        Value::Object(_) => collect_content_object(value, segments),
        _ => {}
    }
}

fn collect_content_object(content: &Value, segments: &mut Vec<String>) {
    let s = |path: &str| content.g(path).str();
    match s("type").as_str() {
        "text" => add(segments, &s("text")),
        "thinking" => add(segments, &s("thinking")),
        "document" => {
            let source = content.g("source");
            if source.g("type").str() != "text" {
                return;
            }
            add(segments, &s("title"));
            add(segments, &s("context"));
            add(segments, &source.g("data").str());
            add(segments, &source.g("content").str());
        }
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            add(segments, &s("id"));
            add(segments, &s("name"));
            add_json(segments, &content.g("input"));
        }
        "tool_result" | "mcp_tool_result" | "web_search_tool_result" | "web_fetch_tool_result"
        | "code_execution_tool_result" | "bash_code_execution_tool_result" | "text_editor_code_execution_tool_result" => {
            add(segments, &s("tool_use_id"));
            add(segments, &s("tool_call_id"));
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            if let Some(source) = content.g("source").as_str() {
                add(segments, source);
            }
            add(segments, &s("title"));
            add(segments, &s("url"));
            add(segments, &s("page_age"));
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            add(segments, &s("url"));
            add(segments, &s("retrieved_at"));
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            add(segments, &s("stdout"));
            add(segments, &s("stderr"));
            add(segments, &s("return_code"));
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => add(segments, &s("tool_name")),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => add_json(segments, &Res::of(content)),
        _ => add(segments, &s("text")),
    }
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    let Some(Value::Array(items)) = tools.v() else { return };
    for tool in items {
        add(segments, &tool.g("type").str());
        add(segments, &tool.g("name").str());
        add(segments, &tool.g("description").str());
        add_json(segments, &tool.g("input_schema"));
    }
}

fn collect_tool_choice(choice: &Res<'_>, segments: &mut Vec<String>) {
    if !choice.exists() {
        return;
    }
    if let Some(s) = choice.as_str() {
        add(segments, s);
        return;
    }
    add(segments, &choice.g("type").str());
    add(segments, &choice.g("name").str());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_only_the_first_empty_message_start() {
        let request = Bytes::from_static(br#"{"system":"be brief","messages":[{"role":"user","content":"hello world"}]}"#);
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::Gemini, Format::Claude, request);
        let start = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}

"#;
        let mut chunks = vec![start.to_vec()];
        state.apply(&mut chunks);
        let text = String::from_utf8(chunks[0].clone()).unwrap();
        assert!(!text.contains("\"input_tokens\":0"), "{text}");
        assert!(text.starts_with("event: message_start\ndata: {"));
        // Already handled: the next message_start stays untouched.
        let mut again = vec![start.to_vec()];
        state.apply(&mut again);
        assert_eq!(again[0], start.to_vec());
    }

    #[test]
    fn disabled_for_non_claude_pairs_and_non_zero_usage_wins() {
        let request = Bytes::from_static(br#"{"messages":[{"role":"user","content":"x"}]}"#);
        let mut off = ClaudeInputTokenState::new(Format::OpenAI, Format::Gemini, Format::OpenAI, request.clone());
        let mut chunks = vec![br#"data: {"type":"message_start","message":{"usage":{"input_tokens":0}}}"#.to_vec()];
        let before = chunks.clone();
        off.apply(&mut chunks);
        assert_eq!(chunks, before);
        let mut on = ClaudeInputTokenState::new(Format::Claude, Format::Gemini, Format::Claude, request);
        let mut set = vec![br#"data: {"type":"message_start","message":{"usage":{"input_tokens":7}}}"#.to_vec()];
        let before = set.clone();
        on.apply(&mut set);
        assert_eq!(set, before);
    }
}
