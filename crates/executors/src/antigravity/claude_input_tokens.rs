//! One-time `message_start` input token estimate for translated Claude streams (Go:
//! helps/claude_input_tokens.go).
//!
//! Gemini-family upstreams report prompt tokens only at the end, so when a Claude client reads a
//! translated stream the executor fills `message.usage.input_tokens` of the first
//! `message_start` frame from a local `o200k_base` estimate of the original Claude request.

use cpa_json::{J, Res};
use cpa_translator::Format;

use crate::helps::token_count::tokenizer_for_model;

/// Request-scoped state: the estimate is applied to at most one `message_start` frame.
pub struct ClaudeInputTokenState {
    original_request: Vec<u8>,
    handled: bool,
}

impl ClaudeInputTokenState {
    pub fn new(source: Format, upstream: Format, response: Format, original_request: &[u8]) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self { original_request: original_request.to_vec(), handled: !enabled }
    }

    /// Applies the estimate to the first eligible chunk of `chunks`.
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

    /// `None`: no message_start here. `Some(None)`: found, left unchanged. `Some(Some(x))`: rewritten.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0usize;
        while line_start < chunk.len() {
            let mut line_end = chunk[line_start..].iter().position(|&b| b == b'\n').map_or(chunk.len(), |i| i + line_start);
            let next = line_end;
            if line_end > line_start && chunk[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line = &chunk[line_start..line_end];
            let trimmed_left = line.iter().position(|&b| b != b' ' && b != b'\t').map_or(line.len(), |i| i);
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
                let mut value = cpa_json::parse(payload);
                if value.g("type").str() == "message_start" {
                    let input_tokens = value.g("message.usage.input_tokens");
                    if input_tokens.exists() && input_tokens.int() != 0 {
                        return Some(None);
                    }
                    drop(input_tokens);
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
                    cpa_json::set(&mut value, "message.usage.input_tokens", count);
                    let updated_payload = cpa_json::to_vec(&value);
                    let mut out = Vec::with_capacity(chunk.len() + updated_payload.len());
                    out.extend_from_slice(&chunk[..line_start + payload_offset]);
                    out.extend_from_slice(&updated_payload);
                    out.extend_from_slice(&chunk[line_start + payload_end..]);
                    return Some(Some(out));
                }
            }
            if next == chunk.len() {
                break;
            }
            line_start = next + 1;
        }
        None
    }

    fn estimate(&self) -> Result<i64, String> {
        count_claude_input_tokens(&self.original_request)
    }
}

/// `o200k_base` token estimate of a Claude request (system, messages, tools, tool choice).
pub(crate) fn count_claude_input_tokens(payload: &[u8]) -> Result<i64, String> {
    let enc = tokenizer_for_model("gpt-5").map_err(|e| format!("initialize O200kBase tokenizer: {e}"))?;
    let segments = collect_segments(payload).map_err(|e| format!("count Claude input tokens: {e}"))?;
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
    let mut segments = Vec::new();
    collect_system(&root.g("system"), &mut segments);
    collect_messages(&root.g("messages"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    collect_tool_choice(&root.g("tool_choice"), &mut segments);
    Ok(segments)
}

fn push_str(segments: &mut Vec<String>, value: &str) {
    let t = value.trim();
    if !t.is_empty() {
        segments.push(t.to_string());
    }
}

fn push_json(segments: &mut Vec<String>, value: &Res<'_>) {
    if !value.exists() {
        return;
    }
    if value.is_string() {
        push_str(segments, &value.str());
        return;
    }
    push_str(segments, &value.raw());
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    if system.is_string() {
        push_str(segments, &system.str());
        return;
    }
    if !system.is_array() {
        return;
    }
    for part in system.array() {
        if part.is_string() {
            push_str(segments, &part.str());
        } else if part.g("type").str() == "text" {
            push_str(segments, &part.g("text").str());
        }
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    if !messages.is_array() {
        return;
    }
    for message in messages.array() {
        push_str(segments, &message.g("role").str());
        collect_content(&message.g("content"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    if !content.exists() {
        return;
    }
    if content.is_string() {
        push_str(segments, &content.str());
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
    match content.g("type").str().as_str() {
        "text" => push_str(segments, &content.g("text").str()),
        "thinking" => push_str(segments, &content.g("thinking").str()),
        "document" => collect_document(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            push_str(segments, &content.g("id").str());
            push_str(segments, &content.g("name").str());
            push_json(segments, &content.g("input"));
        }
        "tool_result"
        | "mcp_tool_result"
        | "web_search_tool_result"
        | "web_fetch_tool_result"
        | "code_execution_tool_result"
        | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            push_str(segments, &content.g("tool_use_id").str());
            push_str(segments, &content.g("tool_call_id").str());
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            let source = content.g("source");
            if source.is_string() {
                push_str(segments, &source.str());
            }
            push_str(segments, &content.g("title").str());
            push_str(segments, &content.g("url").str());
            push_str(segments, &content.g("page_age").str());
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            push_str(segments, &content.g("url").str());
            push_str(segments, &content.g("retrieved_at").str());
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            push_str(segments, &content.g("stdout").str());
            push_str(segments, &content.g("stderr").str());
            push_str(segments, &content.g("return_code").str());
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => push_str(segments, &content.g("tool_name").str()),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => push_json(segments, content),
        _ => push_str(segments, &content.g("text").str()),
    }
}

fn collect_document(document: &Res<'_>, segments: &mut Vec<String>) {
    let source = document.g("source");
    if source.g("type").str() != "text" {
        return;
    }
    push_str(segments, &document.g("title").str());
    push_str(segments, &document.g("context").str());
    push_str(segments, &source.g("data").str());
    push_str(segments, &source.g("content").str());
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    if !tools.is_array() {
        return;
    }
    for tool in tools.array() {
        push_str(segments, &tool.g("type").str());
        push_str(segments, &tool.g("name").str());
        push_str(segments, &tool.g("description").str());
        push_json(segments, &tool.g("input_schema"));
    }
}

fn collect_tool_choice(tool_choice: &Res<'_>, segments: &mut Vec<String>) {
    if !tool_choice.exists() {
        return;
    }
    if tool_choice.is_string() {
        push_str(segments, &tool_choice.str());
        return;
    }
    push_str(segments, &tool_choice.g("type").str());
    push_str(segments, &tool_choice.g("name").str());
}
