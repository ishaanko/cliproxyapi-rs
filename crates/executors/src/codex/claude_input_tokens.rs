//! Claude `message_start` input token estimate for streams translated from another dialect (Go:
//! helps/claude_input_tokens.go). Upstream Codex reports no input tokens up front, so the first
//! `message_start` of a Claude-facing stream gets a local tiktoken estimate of the original request.

use cpa_json::{J, Res};
use cpa_translator::{Ctx, Format, Param};

use crate::helps::apply_patch::apply_patch_translation_error;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::token_count::tokenizer_for_model;

/// One-time input token update of a translated Claude stream.
pub struct ClaudeInputTokenState {
    original_request: Vec<u8>,
    handled: bool,
}

impl ClaudeInputTokenState {
    /// Enabled only for Claude clients served by a non-Claude upstream with a Claude response.
    pub fn new(source: Format, upstream: Format, response: Format, original_request: &[u8]) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self { original_request: original_request.to_vec(), handled: !enabled }
    }

    /// Translates one upstream line and patches the first `message_start` usage (Go:
    /// TranslateStreamWithClaudeInputTokens).
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
        let mut chunks = cpa_translator::translate_stream(&Ctx::default(), upstream, response, model, original, request, raw, param);
        if apply_patch_translation_error(param).is_some() {
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
                if let Some(updated) = updated {
                    *chunk = updated;
                }
                break;
            }
        }
    }

    /// `Some(Some(new))` when a `message_start` was patched, `Some(None)` when one was found and
    /// left alone, `None` when the chunk has none.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..].iter().position(|b| *b == b'\n').map_or(chunk.len(), |p| p + line_start);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let trimmed_left = line.iter().position(|b| *b != b' ' && *b != b'\t').map_or(line.len(), |p| p);
            if line[trimmed_left..].starts_with(b"data:") {
                let mut payload_offset = trimmed_left + "data:".len();
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
                    let existing = parsed.g("message.usage.input_tokens");
                    if existing.exists() && existing.int() != 0 {
                        return Some(None);
                    }
                    let Some(count) = self.estimate() else { return Some(None) };
                    if count == 0 {
                        return Some(None);
                    }
                    if !cpa_json::set(&mut parsed, "message.usage.input_tokens", count) {
                        return Some(None);
                    }
                    let updated_payload = cpa_json::to_vec(&parsed);
                    let (start, stop) = (line_start + payload_offset, line_start + payload_end);
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

    fn estimate(&self) -> Option<i64> {
        let tokenizer = tokenizer_for_model("gpt-5").ok()?;
        let segments = collect_segments(&self.original_request)?;
        if segments.is_empty() {
            return Some(0);
        }
        Some(tokenizer.count(&segments.join("\n")) as i64)
    }
}

/// Text segments of a Claude request that count as input (Go: collectClaudeInputTokenSegments);
/// `None` for invalid JSON.
fn collect_segments(payload: &[u8]) -> Option<Vec<String>> {
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Some(Vec::new());
    }
    if !cpa_json::valid(payload) {
        return None;
    }
    let root = cpa_json::parse(payload);
    let mut segments = Vec::with_capacity(32);
    collect_system(&root.g("system"), &mut segments);
    collect_messages(&root.g("messages"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    collect_tool_choice(&root.g("tool_choice"), &mut segments);
    Some(segments)
}

fn push_string(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

fn push_json(segments: &mut Vec<String>, value: &Res<'_>) {
    if !value.exists() {
        return;
    }
    if value.is_string() {
        push_string(segments, &value.str());
        return;
    }
    let raw = value.raw();
    push_string(segments, &raw);
}

fn collect_system(system: &Res<'_>, segments: &mut Vec<String>) {
    if system.is_string() {
        push_string(segments, &system.str());
        return;
    }
    if !system.is_array() {
        return;
    }
    for part in system.array() {
        if part.is_string() {
            push_string(segments, &part.str());
        } else if part.g("type").str() == "text" {
            push_string(segments, &part.g("text").str());
        }
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    if !messages.is_array() {
        return;
    }
    for message in messages.array() {
        push_string(segments, &message.g("role").str());
        collect_content(&message.g("content"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    if !content.exists() {
        return;
    }
    if content.is_string() {
        push_string(segments, &content.str());
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
    let text = |segments: &mut Vec<String>, path: &str| push_string(segments, &content.g(path).str());
    match content.g("type").str().as_str() {
        "text" => text(segments, "text"),
        "thinking" => text(segments, "thinking"),
        "document" => collect_document(content, segments),
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            text(segments, "id");
            text(segments, "name");
            push_json(segments, &content.g("input"));
        }
        "tool_result" | "mcp_tool_result" | "web_search_tool_result" | "web_fetch_tool_result" | "code_execution_tool_result"
        | "bash_code_execution_tool_result" | "text_editor_code_execution_tool_result" => {
            text(segments, "tool_use_id");
            text(segments, "tool_call_id");
            collect_content(&content.g("content"), segments);
        }
        "web_search_result" | "search_result" => {
            let source = content.g("source");
            if source.is_string() {
                push_string(segments, &source.str());
            }
            text(segments, "title");
            text(segments, "url");
            text(segments, "page_age");
            collect_content(&content.g("content"), segments);
        }
        "web_fetch_result" => {
            text(segments, "url");
            text(segments, "retrieved_at");
            collect_content(&content.g("content"), segments);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            text(segments, "stdout");
            text(segments, "stderr");
            text(segments, "return_code");
            collect_content(&content.g("content"), segments);
            collect_content(&content.g("output"), segments);
        }
        "tool_reference" => text(segments, "tool_name"),
        "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "" => push_json(segments, content),
        _ => text(segments, "text"),
    }
}

fn collect_document(document: &Res<'_>, segments: &mut Vec<String>) {
    let source = document.g("source");
    if source.g("type").str() != "text" {
        return;
    }
    push_string(segments, &document.g("title").str());
    push_string(segments, &document.g("context").str());
    push_string(segments, &source.g("data").str());
    push_string(segments, &source.g("content").str());
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    if !tools.is_array() {
        return;
    }
    for tool in tools.array() {
        push_string(segments, &tool.g("type").str());
        push_string(segments, &tool.g("name").str());
        push_string(segments, &tool.g("description").str());
        push_json(segments, &tool.g("input_schema"));
    }
}

fn collect_tool_choice(choice: &Res<'_>, segments: &mut Vec<String>) {
    if !choice.exists() {
        return;
    }
    if choice.is_string() {
        push_string(segments, &choice.str());
        return;
    }
    push_string(segments, &choice.g("type").str());
    push_string(segments, &choice.g("name").str());
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &[u8] = br#"{"system":"be brief","messages":[{"role":"user","content":"hello world"}]}"#;

    #[test]
    fn first_message_start_gets_an_estimate_once() {
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::Codex, Format::Claude, REQUEST);
        let mut chunks = vec![b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec()];
        state.apply(&mut chunks);
        let patched = String::from_utf8(chunks[0].clone()).unwrap();
        assert!(patched.contains("\"input_tokens\":") && !patched.contains("\"input_tokens\":0"), "{patched}");
        assert!(patched.starts_with("event: message_start\ndata: ") && patched.ends_with("\n\n"));
        let again = chunks.clone();
        state.apply(&mut chunks);
        assert_eq!(chunks, again);
    }

    #[test]
    fn disabled_for_non_claude_pairs() {
        let state = ClaudeInputTokenState::new(Format::OpenAI, Format::Codex, Format::OpenAI, REQUEST);
        assert!(state.handled);
    }

    #[test]
    fn nonzero_upstream_value_is_kept() {
        let mut state = ClaudeInputTokenState::new(Format::Claude, Format::Codex, Format::Claude, REQUEST);
        let original = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n".to_vec();
        let mut chunks = vec![original.clone()];
        state.apply(&mut chunks);
        assert_eq!(chunks[0], original);
        assert!(state.handled);
    }
}
