//! Claude input token estimate for translated streams (Go: helps/claude_input_tokens.go).
//!
//! When a Claude client is served by a non-Claude upstream, the translated `message_start` event
//! has no input token count. The first `message_start` of the stream gets an `o200k_base`
//! estimate of the original Claude request instead.

use cpa_json::{J, Res};
use cpa_translator::{Ctx, Format, Param};

use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::token_count::tokenizer_for_model;

/// One-time `message_start` input token update for a translated Claude stream.
pub struct ClaudeInputTokenState {
    original_request: Vec<u8>,
    handled: bool,
}

impl ClaudeInputTokenState {
    /// Only active for Claude clients on a non-Claude upstream answered in Claude format.
    pub fn new(source: Format, upstream: Format, response: Format, original_request: &[u8]) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self { original_request: original_request.to_vec(), handled: !enabled }
    }

    /// Translates one upstream stream event like `translate_stream`, normalizing Responses usage
    /// details and filling the Claude `message_start` input tokens once.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_stream(
        &mut self,
        upstream: Format,
        response: Format,
        model: &str,
        original: &[u8],
        request: &[u8],
        raw: &[u8],
        param: &mut Param,
    ) -> Vec<Vec<u8>> {
        let mut chunks =
            cpa_translator::translate_stream(&Ctx::default(), upstream, response, model, original, request, raw, param);
        if param.tool_input_error.is_some() {
            return chunks;
        }
        if response == Format::OpenAIResponse {
            for chunk in &mut chunks {
                *chunk = ensure_responses_usage_details(chunk);
            }
        }
        self.apply(&mut chunks);
        chunks
    }

    fn apply(&mut self, chunks: &mut [Vec<u8>]) {
        if self.handled {
            return;
        }
        for chunk in chunks.iter_mut() {
            if let Some(updated) = self.apply_chunk(chunk) {
                self.handled = true;
                if let Some(new_chunk) = updated {
                    *chunk = new_chunk;
                }
                break;
            }
        }
    }

    /// `Some(Some(new))` when a `message_start` line was rewritten, `Some(None)` when one was
    /// found and left alone, `None` when the chunk has no `message_start`.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |p| p + line_start);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let lead = line.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
            if line[lead..].starts_with(b"data:") {
                let mut payload_offset = lead + "data:".len();
                while payload_offset < line.len() && matches!(line[payload_offset], b' ' | b'\t') {
                    payload_offset += 1;
                }
                let mut payload_end = line.len();
                while payload_end > payload_offset && matches!(line[payload_end - 1], b' ' | b'\t') {
                    payload_end -= 1;
                }
                let payload = &line[payload_offset..payload_end];
                let root = cpa_json::parse(payload);
                if root.g("type").str() == "message_start" {
                    let input = root.g("message.usage.input_tokens");
                    if input.exists() && input.int() != 0 {
                        return Some(None);
                    }
                    let count = match self.estimate() {
                        Some(c) if c != 0 => c,
                        _ => return Some(None),
                    };
                    let mut updated = root.clone();
                    cpa_json::set(&mut updated, "message.usage.input_tokens", count);
                    let new_payload = cpa_json::to_vec(&updated);
                    // Splice the new payload into the line, keeping the surrounding bytes.
                    let (start, stop) = (line_start + payload_offset, line_start + payload_end);
                    let mut out = Vec::with_capacity(chunk.len() + new_payload.len());
                    out.extend_from_slice(&chunk[..start]);
                    out.extend_from_slice(&new_payload);
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

    fn estimate(&self) -> Option<i64> {
        let enc = match tokenizer_for_model("gpt-5") {
            Ok(enc) => enc,
            Err(err) => {
                tracing::warn!("failed to estimate Claude input tokens: initialize O200kBase tokenizer: {err}");
                return None;
            }
        };
        match count_claude_input_tokens(&enc, &self.original_request) {
            Ok(n) => Some(n),
            Err(err) => {
                tracing::warn!("failed to estimate Claude input tokens: {err}");
                None
            }
        }
    }
}

/// Estimated tokens of a Claude request (system, messages, tools, tool choice).
pub fn count_claude_input_tokens(enc: &crate::helps::token_count::Tokenizer, payload: &[u8]) -> Result<i64, String> {
    if payload.trim_ascii().is_empty() {
        return Ok(0);
    }
    if !cpa_json::valid(payload) {
        return Err("invalid Claude request JSON".into());
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
    if value.is_string() {
        add(segments, &value.str());
        return;
    }
    // Compact JSON text (the Go code compacts the raw text).
    add(segments, &value.raw());
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    if system.is_string() {
        add(segments, &system.str());
        return;
    }
    if !system.is_array() {
        return;
    }
    for part in system.array() {
        if part.is_string() {
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
    if content.is_string() {
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
    let get = |k: &str| content.g(k).str();
    match get("type").as_str() {
        "text" => add(segments, &get("text")),
        "thinking" => add(segments, &get("thinking")),
        "document" => collect_document(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            add(segments, &get("id"));
            add(segments, &get("name"));
            add_json(segments, &content.g("input"));
        }
        "tool_result"
        | "mcp_tool_result"
        | "web_search_tool_result"
        | "web_fetch_tool_result"
        | "code_execution_tool_result"
        | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            add(segments, &get("tool_use_id"));
            add(segments, &get("tool_call_id"));
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            let source = content.g("source");
            if source.is_string() {
                add(segments, &source.str());
            }
            add(segments, &get("title"));
            add(segments, &get("url"));
            add(segments, &get("page_age"));
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            add(segments, &get("url"));
            add(segments, &get("retrieved_at"));
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            add(segments, &get("stdout"));
            add(segments, &get("stderr"));
            add(segments, &get("return_code"));
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => add(segments, &get("tool_name")),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => add_json(segments, content),
        _ => add(segments, &get("text")),
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
    if tool_choice.is_string() {
        add(segments, &tool_choice.str());
        return;
    }
    add(segments, &tool_choice.g("type").str());
    add(segments, &tool_choice.g("name").str());
}
