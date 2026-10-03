//! Codex-dialect helpers the Meta executor shares with the Codex and xAI executors in Go
//! (codex_executor_request.go, codex_executor_terminal.go, codex_executor_tokens.go,
//! xai_executor_response.go). Kept local to this
//! module so Meta does not depend on another provider's internals.

use std::collections::BTreeMap;

use cpa_json::{J, Res};
use serde_json::Value;

use crate::helps::token_count::Tokenizer;

/// Missing or null `instructions` becomes the empty string (Go: normalizeCodexInstructions).
pub fn normalize_codex_instructions(body: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    let instructions = root.g("instructions");
    if !instructions.exists() || instructions.is_null() {
        cpa_json::set(&mut root, "instructions", "");
        return cpa_json::to_vec(&root);
    }
    body.to_vec()
}

/// Output items seen in `response.output_item.done` events, to rebuild an empty completed output.
#[derive(Default)]
pub struct OutputItems {
    by_index: BTreeMap<i64, Value>,
    fallback: Vec<Value>,
}

impl OutputItems {
    /// Go: collectCodexOutputItemDone / xaiCollectOutputItemDone.
    pub fn collect(&mut self, event: &[u8]) {
        let root = cpa_json::parse(event);
        let item = root.g("item");
        if !matches!(item.v(), Some(Value::Object(_) | Value::Array(_))) {
            return;
        }
        let item = item.value();
        let output_index = root.g("output_index");
        if output_index.exists() {
            self.by_index.insert(output_index.int(), item);
        } else {
            self.fallback.push(item);
        }
    }

    /// Fills in `response.output` from the collected items when the completed event carries
    /// none, or hydrates blank item ids of a non-empty output (Go: patchCodexCompletedOutput).
    pub fn patch_completed(&self, event: &[u8]) -> Vec<u8> {
        let mut root = cpa_json::parse(event);
        let output = root.g("response.output");
        if let Some(Value::Array(items)) = output.v()
            && !items.is_empty()
        {
            let items = items.clone();
            return self.hydrate_item_ids(event, &items);
        }
        if self.by_index.is_empty() && self.fallback.is_empty() {
            return event.to_vec();
        }
        let mut items: Vec<Value> = self.by_index.values().cloned().collect();
        items.extend(self.fallback.iter().cloned());
        cpa_json::set(&mut root, "response.output", Value::Array(items));
        cpa_json::to_vec(&root)
    }

    /// Copies a missing item id from the matching `output_item.done` item by output index.
    fn hydrate_item_ids(&self, event: &[u8], items: &[Value]) -> Vec<u8> {
        let mut root = cpa_json::parse(event);
        let mut changed = false;
        for (index, item) in items.iter().enumerate() {
            let id = item.g("id");
            let has_id = id.exists()
                && !id.is_null()
                && (!id.is_string() || !id.str().trim().is_empty());
            if has_id {
                continue;
            }
            let Some(done) = self.by_index.get(&(index as i64)) else {
                continue;
            };
            let completed_id = done.g("id");
            if !completed_id.is_string() || completed_id.str().trim().is_empty() {
                continue;
            }
            cpa_json::set(&mut root, &format!("response.output.{index}.id"), completed_id.value());
            changed = true;
        }
        if changed { cpa_json::to_vec(&root) } else { event.to_vec() }
    }
}

// ---------------------------------------------------------------- token counting

fn push_trimmed(segments: &mut Vec<String>, text: &str) {
    let t = text.trim();
    if !t.is_empty() {
        segments.push(t.to_string());
    }
}

/// A schema-ish value as text: strings verbatim, other JSON as its raw text.
fn value_text(v: &Res<'_>) -> String {
    if v.is_string() { v.str() } else { v.raw() }
}

/// Approximate input tokens of a Responses body: instructions, message texts, tool calls and
/// outputs, tool declarations and the text format schema, joined and tokenized once (Go:
/// countCodexInputTokens).
pub fn count_codex_input_tokens(enc: &Tokenizer, body: &[u8]) -> i64 {
    if body.is_empty() {
        return 0;
    }
    let root = cpa_json::parse(body);
    let mut segments: Vec<String> = Vec::new();
    push_trimmed(&mut segments, &root.g("instructions").str());

    if let Some(Value::Array(items)) = root.g("input").v() {
        for item in items {
            let item = Res::of(item);
            match item.g("type").str().as_str() {
                "message" => {
                    if let Some(Value::Array(parts)) = item.g("content").v() {
                        for part in parts {
                            push_trimmed(&mut segments, &Res::of(part).g("text").str());
                        }
                    }
                }
                "function_call" => {
                    push_trimmed(&mut segments, &item.g("name").str());
                    push_trimmed(&mut segments, &item.g("arguments").str());
                }
                "function_call_output" => push_trimmed(&mut segments, &item.g("output").str()),
                _ => push_trimmed(&mut segments, &item.g("text").str()),
            }
        }
    }

    if let Some(Value::Array(tools)) = root.g("tools").v() {
        for tool in tools {
            let tool = Res::of(tool);
            push_trimmed(&mut segments, &tool.g("name").str());
            push_trimmed(&mut segments, &tool.g("description").str());
            let params = tool.g("parameters");
            if params.exists() {
                push_trimmed(&mut segments, &value_text(&params));
            }
        }
    }

    let text_format = root.g("text.format");
    if text_format.exists() {
        push_trimmed(&mut segments, &text_format.g("name").str());
        let schema = text_format.g("schema");
        if schema.exists() {
            push_trimmed(&mut segments, &value_text(&schema));
        }
    }

    let text = segments.join("\n");
    if text.is_empty() { 0 } else { enc.count(&text) as i64 }
}
