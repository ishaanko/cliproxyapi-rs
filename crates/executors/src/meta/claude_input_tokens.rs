//! Input token estimate for translated Claude streams (Go: helps/claude_input_tokens.go).
//!
//! When a Claude client is served by a non-Claude upstream, the upstream's `message_start` has no
//! input usage. The state fills `message.usage.input_tokens` once, estimated from the original
//! Claude request with the O200k tokenizer.

use cpa_json::{J, Res};
use cpa_translator::{Ctx, Format, Param};
use serde_json::Value;

use crate::helps::apply_patch::apply_patch_translation_error;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::token_count::tokenizer_for_model;

/// One-time `message_start` input token update for a stream.
pub struct ClaudeInputTokenState {
    original_request: Vec<u8>,
    handled: bool,
}

impl ClaudeInputTokenState {
    /// Enabled only for Claude clients on a non-Claude upstream answering in Claude format.
    pub fn new(source: Format, upstream: Format, response: Format, original_request: &[u8]) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self { original_request: original_request.to_vec(), handled: !enabled }
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

    /// `None`: no `message_start` in the chunk. `Some(None)`: found, left unchanged.
    /// `Some(Some(bytes))`: found and updated.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |p| p + line_start);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let trimmed_left = line.trim_ascii_start_tabs();
            if let Some(after) = trimmed_left.strip_prefix(b"data:") {
                let payload_offset = line.len() - after.len() + after.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
                let mut payload_end = line.len();
                while payload_end > payload_offset && (line[payload_end - 1] == b' ' || line[payload_end - 1] == b'\t') {
                    payload_end -= 1;
                }
                let payload = &line[payload_offset..payload_end];
                let root = cpa_json::parse(payload);
                if root.g("type").str() == "message_start" {
                    let input_tokens = root.g("message.usage.input_tokens");
                    if input_tokens.exists() && input_tokens.int() != 0 {
                        return Some(None);
                    }
                    let count = match self.estimate() {
                        Ok(c) => c,
                        Err(err) => {
                            tracing::warn!("failed to estimate Claude input tokens: {err}");
                            return Some(None);
                        }
                    };
                    if count == 0 {
                        return Some(None);
                    }
                    let mut updated_root = root.clone();
                    cpa_json::set(&mut updated_root, "message.usage.input_tokens", count);
                    let updated_payload = cpa_json::to_vec(&updated_root);
                    let start = line_start + payload_offset;
                    let stop = line_start + payload_end;
                    let mut updated = Vec::with_capacity(chunk.len() + updated_payload.len());
                    updated.extend_from_slice(&chunk[..start]);
                    updated.extend_from_slice(&updated_payload);
                    updated.extend_from_slice(&chunk[stop..]);
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
        let enc = tokenizer_for_model("gpt-5").map_err(|e| format!("initialize O200kBase tokenizer: {e}"))?;
        count_claude_input_tokens(&enc, &self.original_request).map_err(|e| format!("count Claude input tokens: {e}"))
    }
}

trait TrimTabs {
    fn trim_ascii_start_tabs(&self) -> &[u8];
}

impl TrimTabs for [u8] {
    /// `bytes.TrimLeft(line, " \t")`.
    fn trim_ascii_start_tabs(&self) -> &[u8] {
        let n = self.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        &self[n..]
    }
}

/// Translates one upstream stream line and applies the Claude input token estimate and Responses
/// usage details (Go: TranslateStreamWithClaudeInputTokens).
#[allow(clippy::too_many_arguments)]
pub fn translate_stream_with_claude_input_tokens(
    upstream: Format,
    response: Format,
    model: &str,
    original: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
    state: &mut ClaudeInputTokenState,
) -> Vec<Vec<u8>> {
    let mut chunks = cpa_translator::translate_stream(&Ctx::default(), upstream, response, model, original, request, raw, param);
    if apply_patch_translation_error(param).is_some() {
        return chunks;
    }
    if response == Format::OpenAIResponse {
        for chunk in &mut chunks {
            *chunk = ensure_responses_usage_details(chunk);
        }
    }
    state.apply(&mut chunks);
    chunks
}

// ---------------------------------------------------------------- token counting

fn count_claude_input_tokens(enc: &crate::helps::token_count::Tokenizer, payload: &[u8]) -> Result<i64, String> {
    let trimmed = crate::helps::text::trim_space(payload);
    if trimmed.is_empty() {
        return Ok(0);
    }
    if !cpa_json::valid(payload) {
        return Err("invalid Claude request JSON".to_string());
    }
    let root = cpa_json::parse(payload);
    let mut segments: Vec<String> = Vec::with_capacity(32);
    collect_system(&root.g("system"), &mut segments);
    collect_messages(&root.g("messages"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    collect_tool_choice(&root.g("tool_choice"), &mut segments);
    if segments.is_empty() {
        return Ok(0);
    }
    Ok(enc.count(&segments.join("\n")) as i64)
}

fn push(segments: &mut Vec<String>, value: &str) {
    let t = value.trim();
    if !t.is_empty() {
        segments.push(t.to_string());
    }
}

/// A JSON value as one segment: strings verbatim, anything else compact.
fn push_json(segments: &mut Vec<String>, value: &Res<'_>) {
    let Some(v) = value.v() else { return };
    match v {
        Value::String(s) => push(segments, s),
        other => push(segments, &other.to_string()),
    }
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    match system.v() {
        Some(Value::String(s)) => push(segments, s),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part {
                    Value::String(s) => push(segments, s),
                    other if Res::of(other).g("type").str() == "text" => push(segments, &Res::of(other).g("text").str()),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    let Some(Value::Array(items)) = messages.v() else { return };
    for message in items {
        let message = Res::of(message);
        push(segments, &message.g("role").str());
        collect_content(&message.g("content"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    let Some(v) = content.v() else { return };
    match v {
        Value::String(s) => push(segments, s),
        Value::Array(parts) => {
            for part in parts {
                collect_content(&Res::of(part), segments);
            }
        }
        Value::Object(_) => collect_content_object(content, segments),
        _ => {}
    }
}

fn collect_content_object(content: &Res<'_>, segments: &mut Vec<String>) {
    match content.g("type").str().as_str() {
        "text" => push(segments, &content.g("text").str()),
        "thinking" => push(segments, &content.g("thinking").str()),
        "document" => {
            let source = content.g("source");
            if source.g("type").str() != "text" {
                return;
            }
            push(segments, &content.g("title").str());
            push(segments, &content.g("context").str());
            push(segments, &source.g("data").str());
            push(segments, &source.g("content").str());
        }
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            push(segments, &content.g("id").str());
            push(segments, &content.g("name").str());
            push_json(segments, &content.g("input"));
        }
        "tool_result" | "mcp_tool_result" | "web_search_tool_result" | "web_fetch_tool_result"
        | "code_execution_tool_result" | "bash_code_execution_tool_result" | "text_editor_code_execution_tool_result" => {
            push(segments, &content.g("tool_use_id").str());
            push(segments, &content.g("tool_call_id").str());
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            let source = content.g("source");
            if source.is_string() {
                push(segments, &source.str());
            }
            push(segments, &content.g("title").str());
            push(segments, &content.g("url").str());
            push(segments, &content.g("page_age").str());
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            push(segments, &content.g("url").str());
            push(segments, &content.g("retrieved_at").str());
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            push(segments, &content.g("stdout").str());
            push(segments, &content.g("stderr").str());
            push(segments, &content.g("return_code").str());
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => push(segments, &content.g("tool_name").str()),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => push_json(segments, content),
        _ => push(segments, &content.g("text").str()),
    }
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    let Some(Value::Array(items)) = tools.v() else { return };
    for tool in items {
        let tool = Res::of(tool);
        push(segments, &tool.g("type").str());
        push(segments, &tool.g("name").str());
        push(segments, &tool.g("description").str());
        push_json(segments, &tool.g("input_schema"));
    }
}

fn collect_tool_choice(tool_choice: &Res<'_>, segments: &mut Vec<String>) {
    if !tool_choice.exists() {
        return;
    }
    if tool_choice.is_string() {
        push(segments, &tool_choice.str());
        return;
    }
    push(segments, &tool_choice.g("type").str());
    push(segments, &tool_choice.g("name").str());
}
