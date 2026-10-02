//! Claude message grouping and tool result alignment (Go: common/claude_messages.go).

use cpa_json::{J, Res, Value};

/// Groups consecutive Claude messages by role. Non-user/assistant and empty-content messages are
/// dropped, string content becomes one text part, and on flush every `tool_use` part of an
/// assistant turn moves after the other parts.
#[derive(Debug, Default)]
pub struct ClaudeMessageAccumulator {
    messages: Vec<Vec<u8>>,
    role: String,
    content: Vec<Value>,
    tool_use_parts: Vec<Value>,
}

impl ClaudeMessageAccumulator {
    /// An accumulator sized for the expected message count.
    pub fn new(capacity: usize) -> Self {
        Self {
            messages: Vec::with_capacity(capacity),
            ..Default::default()
        }
    }

    /// Adds one Claude-shaped message (JSON bytes) to the current role turn.
    pub fn append(&mut self, message: &[u8]) {
        if message.is_empty() {
            return;
        }
        let root = cpa_json::parse(message);
        let role = root.g("role").str();
        if role != "user" && role != "assistant" {
            return;
        }
        let parts = claude_message_content_parts(&root.g("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        for part in parts {
            if role == "assistant" && part.g("type").str() == "tool_use" {
                self.tool_use_parts.push(part);
                continue;
            }
            self.content.push(part);
        }
        self.role = role;
    }

    /// Closes the current role turn while keeping accumulated messages.
    pub fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_use_parts);
        if !parts.is_empty() {
            let mut message = cpa_json::parse_str(r#"{"role":"","content":[]}"#);
            cpa_json::set(&mut message, "role", self.role.as_str());
            cpa_json::set(&mut message, "content", Value::Array(parts));
            self.messages.push(cpa_json::to_vec(&message));
        }
        self.role.clear();
    }

    /// Flushes the final turn and returns all accumulated messages.
    pub fn messages(&mut self) -> Vec<Vec<u8>> {
        self.flush();
        self.messages.clone()
    }
}

/// Orders `tool_result` blocks by the preceding `tool_use` IDs, keeping non-result blocks at their
/// indexes. If a complete one-to-one match is unavailable, `content` is returned unchanged.
pub fn align_claude_tool_results<'a>(content: Res<'a>, tool_use_ids: &[String]) -> Res<'a> {
    if !content.is_array() || tool_use_ids.is_empty() {
        return content;
    }

    let parts = content.array();
    let tool_result_indices: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, part)| part.g("type").str() == "tool_result")
        .map(|(i, _)| i)
        .collect();
    if tool_result_indices.len() != tool_use_ids.len() {
        return content;
    }

    let mut used = vec![false; tool_result_indices.len()];
    let mut reordered: Vec<&Res<'_>> = Vec::with_capacity(tool_use_ids.len());
    for tool_use_id in tool_use_ids {
        let matched = tool_result_indices.iter().enumerate().position(|(result_index, &part_index)| {
            !used[result_index]
                && !tool_use_id.is_empty()
                && parts[part_index].g("tool_use_id").str() == *tool_use_id
        });
        let Some(matched) = matched else {
            return content;
        };
        used[matched] = true;
        reordered.push(&parts[tool_result_indices[matched]]);
    }

    let mut ordered: Vec<Value> = parts.iter().map(Res::value).collect();
    for (&slot_index, result) in tool_result_indices.iter().zip(&reordered) {
        ordered[slot_index] = result.value();
    }
    Res::owned(Value::Array(ordered))
}

/// The content parts of a message: a non-empty string becomes one text part, an array keeps its
/// object elements, anything else yields none.
fn claude_message_content_parts(content: &Res<'_>) -> Vec<Value> {
    if !content.exists() || content.is_null() {
        return Vec::new();
    }
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return Vec::new();
        }
        let mut part = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
        cpa_json::set(&mut part, "text", text);
        return vec![part];
    }
    if !content.is_array() {
        return Vec::new();
    }
    content
        .array()
        .iter()
        .filter(|part| part.is_object())
        .map(Res::value)
        .collect()
}
