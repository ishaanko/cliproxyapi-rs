//! Claude `message_start` input-token estimate for translated streams (Go:
//! helps/claude_input_tokens.go).
//!
//! When a Claude client talks to a non-Claude upstream the translated `message_start` carries no
//! input token count, so the first one is patched with an o200k estimate of the original Claude
//! request. [`translate_stream_with_claude_input_tokens`] is the stream translation entry point
//! executors call per upstream frame.

use cpa_json::{J, Kind, Res};
use cpa_translator::{Ctx, Format, Param};

use crate::helps::apply_patch::apply_patch_translation_error;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::token_count::tokenizer_for_model;

/// One-time input token update for a translated Claude stream.
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

    /// `None` when the chunk has no `message_start`; `Some(None)` when found but left as is;
    /// `Some(Some(bytes))` when the input token count was patched.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let eol = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |i| i + line_start);
            let mut line_end = eol;
            if line_end > line_start && chunk[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line = &chunk[line_start..line_end];
            let trimmed_left_len = line.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
            let trimmed_left = &line[trimmed_left_len..];
            if trimmed_left.starts_with(b"data:") {
                let mut payload_offset = trimmed_left_len + "data:".len();
                while payload_offset < line.len() && (line[payload_offset] == b' ' || line[payload_offset] == b'\t') {
                    payload_offset += 1;
                }
                let mut payload_end = line.len();
                while payload_end > payload_offset && (line[payload_end - 1] == b' ' || line[payload_end - 1] == b'\t') {
                    payload_end -= 1;
                }
                let payload = &line[payload_offset..payload_end];
                let mut parsed = cpa_json::parse(payload);
                if parsed.g("type").str() == "message_start" {
                    let input_tokens = parsed.g("message.usage.input_tokens");
                    if input_tokens.exists() && input_tokens.int() != 0 {
                        return Some(None);
                    }
                    let count = match count_claude_input_tokens(&self.original_request) {
                        Ok(count) => count,
                        Err(err) => {
                            tracing::warn!("failed to estimate Claude input tokens: {err}");
                            return Some(None);
                        }
                    };
                    if count == 0 {
                        return Some(None);
                    }
                    cpa_json::set(&mut parsed, "message.usage.input_tokens", count);
                    let updated_payload = cpa_json::to_vec(&parsed);
                    let start = line_start + payload_offset;
                    let stop = line_start + payload_end;
                    let mut updated = Vec::with_capacity(chunk.len() + updated_payload.len());
                    updated.extend_from_slice(&chunk[..start]);
                    updated.extend_from_slice(&updated_payload);
                    updated.extend_from_slice(&chunk[stop..]);
                    return Some(Some(updated));
                }
            }
            if eol == chunk.len() {
                break;
            }
            line_start = eol + 1;
        }
        None
    }
}

/// Translates one upstream frame (Go: TranslateStreamWithClaudeInputTokens): runs the registry
/// translator, normalizes Responses usage details, then patches the Claude `message_start`.
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
        for chunk in chunks.iter_mut() {
            *chunk = ensure_responses_usage_details(chunk);
        }
    }
    state.apply(&mut chunks);
    chunks
}

/// o200k estimate of a Claude request (Go: CountClaudeInputTokens).
pub fn count_claude_input_tokens(payload: &[u8]) -> Result<i64, String> {
    let enc = tokenizer_for_model("gpt-5").map_err(|e| format!("initialize O200kBase tokenizer: {e}"))?;
    if crate::helps::text::trim_space(payload).is_empty() {
        return Ok(0);
    }
    if !cpa_json::valid(payload) {
        return Err("count Claude input tokens: invalid Claude request JSON".into());
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

fn add(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

fn add_json(segments: &mut Vec<String>, value: &Res<'_>) {
    if !value.exists() {
        return;
    }
    if value.kind() == Kind::String {
        add(segments, &value.str());
        return;
    }
    add(segments, &value.raw());
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    if system.kind() == Kind::String {
        add(segments, &system.str());
        return;
    }
    if !system.is_array() {
        return;
    }
    for part in system.array() {
        if part.kind() == Kind::String {
            add(segments, &part.str());
        } else if part.g("type").str() == "text" {
            add(segments, &part.g("text").str());
        }
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    if !messages.is_array() {
        return;
    }
    for message in messages.array() {
        add(segments, &message.g("role").str());
        collect_content(&message.g("content"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    if !content.exists() {
        return;
    }
    if content.kind() == Kind::String {
        add(segments, &content.str());
        return;
    }
    if content.is_array() {
        for part in content.array() {
            collect_content(&part, segments);
        }
        return;
    }
    if !content.is_object() {
        return;
    }
    let g = |p: &str| content.g(p).str();
    match g("type").as_str() {
        "text" => add(segments, &g("text")),
        "thinking" => add(segments, &g("thinking")),
        "document" => collect_document(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            add(segments, &g("id"));
            add(segments, &g("name"));
            add_json(segments, &content.g("input"));
        }
        "tool_result" | "mcp_tool_result" | "web_search_tool_result" | "web_fetch_tool_result"
        | "code_execution_tool_result" | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            add(segments, &g("tool_use_id"));
            add(segments, &g("tool_call_id"));
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            let source = content.g("source");
            if source.kind() == Kind::String {
                add(segments, &source.str());
            }
            add(segments, &g("title"));
            add(segments, &g("url"));
            add(segments, &g("page_age"));
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            add(segments, &g("url"));
            add(segments, &g("retrieved_at"));
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            add(segments, &g("stdout"));
            add(segments, &g("stderr"));
            add(segments, &g("return_code"));
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => add(segments, &g("tool_name")),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => add_json(segments, content),
        _ => add(segments, &g("text")),
    }
}

fn collect_document(document: &Res<'_>, segments: &mut Vec<String>) {
    let source = document.g("source");
    if source.g("type").str() != "text" {
        return;
    }
    add(segments, &document.g("title").str());
    add(segments, &document.g("context").str());
    add(segments, &source.g("data").str());
    add(segments, &source.g("content").str());
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    if !tools.is_array() {
        return;
    }
    for tool in tools.array() {
        add(segments, &tool.g("type").str());
        add(segments, &tool.g("name").str());
        add(segments, &tool.g("description").str());
        add_json(segments, &tool.g("input_schema"));
    }
}

fn collect_tool_choice(tool_choice: &Res<'_>, segments: &mut Vec<String>) {
    if !tool_choice.exists() {
        return;
    }
    if tool_choice.kind() == Kind::String {
        add(segments, &tool_choice.str());
        return;
    }
    add(segments, &tool_choice.g("type").str());
    add(segments, &tool_choice.g("name").str());
}
