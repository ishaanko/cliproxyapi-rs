//! Allocation-light conversion of canonical Claude stream events to OpenAI chunks (see
//! [`crate::common::fast`]). `convert` returns `None`, before touching any state, when the event
//! needs the general `Value` path in the parent module.

use serde::Deserialize;

use super::{map_stop_reason, unix_now, ClaudeUsageTokens, StreamState, ToolCallAccumulator};
use crate::common::fast::{push_int, push_json_str, within_depth_limit, Field, Obj, Str};

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    ty: Str<'a>,
    #[serde(default)]
    index: Field<i64>,
    #[serde(default, borrow)]
    message: Field<Obj<Message<'a>>>,
    #[serde(default, borrow)]
    content_block: Field<Obj<Block<'a>>>,
    #[serde(default, borrow)]
    delta: Field<Obj<Delta<'a>>>,
    #[serde(default)]
    usage: Field<Obj<Usage>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(default, borrow)]
    id: Field<Str<'a>>,
    #[serde(default)]
    usage: Field<Obj<Usage>>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    id: Field<Str<'a>>,
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    text: Field<Str<'a>>,
    #[serde(default, borrow)]
    thinking: Field<Str<'a>>,
    #[serde(default, borrow)]
    partial_json: Field<Str<'a>>,
    #[serde(default, borrow)]
    stop_reason: Field<Str<'a>>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Field<i64>,
    #[serde(default)]
    output_tokens: Field<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Field<i64>,
    #[serde(default)]
    cache_read_input_tokens: Field<i64>,
}

impl ClaudeUsageTokens {
    /// `merge` for a typed usage object: present fields replace earlier values.
    fn merge_fast(&mut self, usage: &Usage) {
        self.has_usage = true;
        for (field, slot) in [
            (&usage.input_tokens, &mut self.input_tokens),
            (&usage.output_tokens, &mut self.output_tokens),
            (&usage.cache_creation_input_tokens, &mut self.cache_creation_input_tokens),
            (&usage.cache_read_input_tokens, &mut self.cache_read_input_tokens),
        ] {
            if let Some(v) = field.get() {
                *slot = *v;
            }
        }
    }

    /// `,"usage":{...}` exactly as `set_on` lays it out.
    fn push_usage(&self, out: &mut Vec<u8>) {
        let (prompt, completion, total, cached, created) = self.openai_usage();
        out.extend_from_slice(b",\"usage\":{\"prompt_tokens\":");
        push_int(out, prompt);
        out.extend_from_slice(b",\"completion_tokens\":");
        push_int(out, completion);
        out.extend_from_slice(b",\"total_tokens\":");
        push_int(out, total);
        out.extend_from_slice(b",\"prompt_tokens_details\":{\"cached_tokens\":");
        push_int(out, cached);
        out.extend_from_slice(b",\"cached_creation_tokens\":");
        push_int(out, created);
        out.extend_from_slice(b",\"cache_write_tokens\":");
        push_int(out, created);
        out.extend_from_slice(b"}}");
    }
}

impl StreamState {
    /// The chunk prefix `{"id":..,"object":..,"created":..,"model":..,"choices":[{"index":0,"delta":`,
    /// rebuilt only when the id, created time or model changed.
    fn head(&mut self, model_name: &str) -> &[u8] {
        if !self.head_valid || self.head_model != model_name {
            let h = &mut self.head;
            h.clear();
            h.extend_from_slice(b"{\"id\":");
            push_json_str(h, &self.response_id);
            h.extend_from_slice(b",\"object\":\"chat.completion.chunk\",\"created\":");
            push_int(h, self.created_at.max(0));
            h.extend_from_slice(b",\"model\":");
            push_json_str(h, model_name);
            h.extend_from_slice(b",\"choices\":[{\"index\":0,\"delta\":");
            self.head_model.clear();
            self.head_model.push_str(model_name);
            self.head_valid = true;
        }
        &self.head
    }

    /// A complete chunk: head, `delta` JSON, `finish_reason`, optional usage.
    fn chunk(&mut self, model_name: &str, delta: &[u8], finish: Option<&str>, with_usage: bool) -> Vec<u8> {
        self.head(model_name);
        let mut out = Vec::with_capacity(self.head.len() + delta.len() + 192);
        out.extend_from_slice(&self.head);
        out.extend_from_slice(delta);
        out.extend_from_slice(b",\"finish_reason\":");
        match finish {
            Some(f) => push_json_str(&mut out, f),
            None => out.extend_from_slice(b"null"),
        }
        out.extend_from_slice(b"}]");
        if with_usage {
            self.usage.push_usage(&mut out);
        }
        out.push(b'}');
        out
    }
}

/// `{"<key>":"<value>"}` for a plain ASCII key.
fn delta_str(key: &str, value: &str) -> Vec<u8> {
    let mut d = Vec::with_capacity(value.len() + key.len() + 8);
    d.extend_from_slice(b"{\"");
    d.extend_from_slice(key.as_bytes());
    d.extend_from_slice(b"\":");
    push_json_str(&mut d, value);
    d.push(b'}');
    d
}

pub(super) fn convert(state: &mut StreamState, model_name: &str, raw: &[u8]) -> Option<Vec<Vec<u8>>> {
    if !within_depth_limit(raw) {
        return None;
    }
    let ev: Obj<Event<'_>> = serde_json::from_slice(raw).ok()?;
    let index = ev.index.get().copied().unwrap_or(0);
    match &*ev.ty {
        "message_start" => {
            let Some(message) = ev.message.get() else {
                return Some(vec![state.chunk(model_name, b"{}", None, false)]);
            };
            state.response_id = message.id.get().map(|s| s.to_string()).unwrap_or_default();
            state.created_at = unix_now();
            state.head_valid = false;
            state.next_tool_call_index = 0;
            if let Some(usage) = message.usage.get() {
                state.usage.merge_fast(usage);
            }
            Some(vec![state.chunk(model_name, br#"{"role":"assistant"}"#, None, false)])
        }
        "content_block_start" => {
            if let Some(block) = ev.content_block.get()
                && block.ty.get().is_some_and(|t| &**t == "tool_use")
            {
                let tool_call_index = state.next_tool_call_index;
                state.next_tool_call_index += 1;
                state.tool_calls.insert(
                    index,
                    ToolCallAccumulator {
                        id: block.id.get().map(|s| s.to_string()).unwrap_or_default(),
                        name: block.name.get().map(|s| s.to_string()).unwrap_or_default(),
                        index: tool_call_index,
                        arguments: String::new(),
                    },
                );
            }
            Some(vec![])
        }
        "content_block_delta" => {
            let Some(delta) = ev.delta.get() else { return Some(vec![]) };
            match delta.ty.get().map(|t| &**t) {
                Some("text_delta") => Some(match delta.text.get() {
                    Some(text) => vec![state.chunk(model_name, &delta_str("content", text), None, false)],
                    None => vec![],
                }),
                Some("thinking_delta") => Some(match delta.thinking.get() {
                    Some(t) => vec![state.chunk(model_name, &delta_str("reasoning_content", t), None, false)],
                    None => vec![],
                }),
                Some("input_json_delta") => {
                    if let Some(partial) = delta.partial_json.get()
                        && let Some(acc) = state.tool_calls.get_mut(&index)
                    {
                        acc.arguments.push_str(partial);
                    }
                    Some(vec![])
                }
                _ => Some(vec![]),
            }
        }
        "content_block_stop" => {
            let Some(acc) = state.tool_calls.remove(&index) else { return Some(vec![]) };
            let arguments = if acc.arguments.is_empty() { "{}" } else { acc.arguments.as_str() };
            let mut d = Vec::with_capacity(arguments.len() + acc.id.len() + acc.name.len() + 96);
            d.extend_from_slice(b"{\"tool_calls\":[{\"index\":");
            push_int(&mut d, acc.index);
            d.extend_from_slice(b",\"id\":");
            push_json_str(&mut d, &acc.id);
            d.extend_from_slice(b",\"type\":\"function\",\"function\":{\"name\":");
            push_json_str(&mut d, &acc.name);
            d.extend_from_slice(b",\"arguments\":");
            push_json_str(&mut d, arguments);
            d.extend_from_slice(b"}}]}");
            Some(vec![state.chunk(model_name, &d, None, false)])
        }
        "message_delta" => {
            let mut finish = None;
            if let Some(delta) = ev.delta.get()
                && let Some(stop_reason) = delta.stop_reason.get()
            {
                let mapped = map_stop_reason(stop_reason);
                state.finish_reason = mapped.to_string();
                finish = Some(mapped);
            }
            let with_usage = match ev.usage.get() {
                Some(usage) => {
                    state.usage.merge_fast(usage);
                    true
                }
                None => false,
            };
            Some(vec![state.chunk(model_name, b"{}", finish, with_usage)])
        }
        "message_stop" => {
            if !state.usage.has_usage || state.trailing_usage_sent {
                return Some(vec![]);
            }
            state.trailing_usage_sent = true;
            let mut out = Vec::with_capacity(256);
            out.extend_from_slice(b"{\"id\":");
            push_json_str(&mut out, &state.response_id);
            out.extend_from_slice(b",\"object\":\"chat.completion.chunk\",\"created\":");
            push_int(&mut out, state.created_at.max(0));
            out.extend_from_slice(b",\"model\":");
            push_json_str(&mut out, model_name);
            out.extend_from_slice(b",\"choices\":[]");
            state.usage.push_usage(&mut out);
            out.push(b'}');
            Some(vec![out])
        }
        // The general path owns errors; other event types (ping, ...) produce nothing.
        "error" => None,
        _ => Some(vec![]),
    }
}
