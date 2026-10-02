//! Claude Messages responses to OpenAI Chat Completions responses
//! (Go: claude/openai/chat-completions/claude_openai_response.go).

use crate::common::unix_now;
use std::collections::BTreeMap;

use cpa_json::{J, Res, Value};

use crate::registry::{Ctx, Param};

const DATA_TAG: &[u8] = b"data:";

/// Streaming conversion state, kept across the lines of one response.
#[derive(Default)]
struct StreamState {
    created_at: i64,
    response_id: String,
    finish_reason: String,
    usage: ClaudeUsageTokens,
    trailing_usage_sent: bool,
    /// Tool calls being accumulated, keyed by Claude content block index.
    tool_calls: BTreeMap<i64, ToolCallAccumulator>,
    next_tool_call_index: i64,
}

#[derive(Default)]
struct ClaudeUsageTokens {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    has_usage: bool,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    index: i64,
    arguments: String,
}

impl ClaudeUsageTokens {
    /// Folds a Claude `usage` object in; fields that are present replace earlier values.
    fn merge(&mut self, usage: &Res<'_>) {
        if !usage.exists() {
            return;
        }
        self.has_usage = true;
        for (field, slot) in [
            ("input_tokens", &mut self.input_tokens),
            ("output_tokens", &mut self.output_tokens),
            ("cache_creation_input_tokens", &mut self.cache_creation_input_tokens),
            ("cache_read_input_tokens", &mut self.cache_read_input_tokens),
        ] {
            let v = usage.g(field);
            if v.exists() {
                *slot = v.int();
            }
        }
    }

    /// OpenAI usage numbers: prompt (input plus cache creation and reads), completion, total,
    /// cached read tokens and cache creation tokens.
    fn openai_usage(&self) -> (i64, i64, i64, i64, i64) {
        let cached = self.cache_read_input_tokens;
        let created = self.cache_creation_input_tokens;
        let prompt = self.input_tokens + created + cached;
        (prompt, self.output_tokens, prompt + self.output_tokens, cached, created)
    }

    fn set_on(&self, out: &mut Value) {
        let (prompt, completion, total, cached, created) = self.openai_usage();
        cpa_json::set(out, "usage.prompt_tokens", prompt);
        cpa_json::set(out, "usage.completion_tokens", completion);
        cpa_json::set(out, "usage.total_tokens", total);
        cpa_json::set(out, "usage.prompt_tokens_details.cached_tokens", cached);
        cpa_json::set(out, "usage.prompt_tokens_details.cached_creation_tokens", created);
        cpa_json::set(out, "usage.prompt_tokens_details.cache_write_tokens", created);
    }
}

fn one(v: &Value) -> Vec<Vec<u8>> {
    vec![cpa_json::to_vec(v)]
}

/// Converts one Claude streaming line (`data: {...}`) into OpenAI chat completion chunks.
pub fn convert_claude_response_to_openai(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let state = param.state(StreamState::default);

    if !raw.starts_with(DATA_TAG) {
        return vec![];
    }
    let raw = raw[DATA_TAG.len()..].trim_ascii();

    let root = cpa_json::parse(raw);
    let event_type = root.g("type").str();

    // Base OpenAI streaming chunk.
    let mut template = cpa_json::parse_str(
        r#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#,
    );
    if !model_name.is_empty() {
        cpa_json::set(&mut template, "model", model_name);
    }
    if !state.response_id.is_empty() {
        cpa_json::set(&mut template, "id", state.response_id.as_str());
    }
    if state.created_at > 0 {
        cpa_json::set(&mut template, "created", state.created_at);
    }

    match event_type.as_str() {
        "message_start" => {
            let message = root.g("message");
            if message.exists() {
                state.response_id = message.g("id").str();
                state.created_at = unix_now();

                cpa_json::set(&mut template, "id", state.response_id.as_str());
                cpa_json::set(&mut template, "model", model_name);
                cpa_json::set(&mut template, "created", state.created_at);
                cpa_json::set(&mut template, "choices.0.delta.role", "assistant");

                state.next_tool_call_index = 0;
                state.usage.merge(&message.g("usage"));
            }
            one(&template)
        }

        "content_block_start" => {
            let content_block = root.g("content_block");
            if content_block.exists() && content_block.g("type").str() == "tool_use" {
                // Tool calls are emitted whole at content_block_stop.
                let index = root.g("index").int();
                let tool_call_index = state.next_tool_call_index;
                state.next_tool_call_index += 1;
                state.tool_calls.insert(
                    index,
                    ToolCallAccumulator {
                        id: content_block.g("id").str(),
                        name: content_block.g("name").str(),
                        index: tool_call_index,
                        arguments: String::new(),
                    },
                );
            }
            vec![]
        }

        "content_block_delta" => {
            let mut has_content = false;
            let delta = root.g("delta");
            if delta.exists() {
                match delta.g("type").str().as_str() {
                    "text_delta" => {
                        let text = delta.g("text");
                        if text.exists() {
                            cpa_json::set(&mut template, "choices.0.delta.content", text.str());
                            has_content = true;
                        }
                    }
                    "thinking_delta" => {
                        let thinking = delta.g("thinking");
                        if thinking.exists() {
                            cpa_json::set(&mut template, "choices.0.delta.reasoning_content", thinking.str());
                            has_content = true;
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta.g("partial_json");
                        if partial.exists()
                            && let Some(acc) = state.tool_calls.get_mut(&root.g("index").int())
                        {
                            acc.arguments.push_str(&partial.str());
                        }
                        return vec![];
                    }
                    _ => {}
                }
            }
            if has_content { one(&template) } else { vec![] }
        }

        "content_block_stop" => {
            let index = root.g("index").int();
            if let Some(acc) = state.tool_calls.remove(&index) {
                let arguments = if acc.arguments.is_empty() { "{}".to_string() } else { acc.arguments };
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.0.index", acc.index);
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.0.id", acc.id);
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.0.type", "function");
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.0.function.name", acc.name);
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.0.function.arguments", arguments);
                return one(&template);
            }
            vec![]
        }

        "message_delta" => {
            let delta = root.g("delta");
            if delta.exists() {
                let stop_reason = delta.g("stop_reason");
                if stop_reason.exists() {
                    state.finish_reason = map_stop_reason(&stop_reason.str()).to_string();
                    cpa_json::set(&mut template, "choices.0.finish_reason", state.finish_reason.as_str());
                }
            }
            let usage = root.g("usage");
            if usage.exists() {
                state.usage.merge(&usage);
                state.usage.set_on(&mut template);
            }
            one(&template)
        }

        "message_stop" => {
            // Standard OpenAI trailing usage chunk with an empty choices array.
            if state.usage.has_usage && !state.trailing_usage_sent {
                state.trailing_usage_sent = true;
                let mut usage_template = cpa_json::parse_str(
                    r#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[]}"#,
                );
                if !state.response_id.is_empty() {
                    cpa_json::set(&mut usage_template, "id", state.response_id.as_str());
                }
                if !model_name.is_empty() {
                    cpa_json::set(&mut usage_template, "model", model_name);
                }
                if state.created_at > 0 {
                    cpa_json::set(&mut usage_template, "created", state.created_at);
                }
                state.usage.set_on(&mut usage_template);
                return one(&usage_template);
            }
            vec![]
        }

        "error" => {
            let error_data = root.g("error");
            if error_data.exists() {
                let mut error_json = cpa_json::parse_str(r#"{"error":{"message":"","type":""}}"#);
                cpa_json::set(&mut error_json, "error.message", error_data.g("message").str());
                cpa_json::set(&mut error_json, "error.type", error_data.g("type").str());
                return one(&error_json);
            }
            vec![]
        }

        // ping and unknown events produce nothing.
        _ => vec![],
    }
}

/// Maps Anthropic stop reasons to OpenAI finish reasons.
fn map_stop_reason(anthropic_reason: &str) -> &'static str {
    match anthropic_reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "refusal" | "sensitive" => "content_filter",
        // end_turn, stop_sequence and anything unknown
        _ => "stop",
    }
}

/// Converts a complete Claude SSE body (every `data:` line) into one OpenAI chat completion.
pub fn convert_claude_response_to_openai_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    _original: &[u8],
    _request: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let chunks: Vec<&[u8]> = raw
        .split(|&b| b == b'\n')
        .filter(|line| line.starts_with(DATA_TAG))
        .map(|line| line[DATA_TAG.len()..].trim_ascii())
        .collect();

    let mut out = cpa_json::parse_str(
        r#"{"id":"","object":"chat.completion","created":0,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#,
    );

    let mut message_id = String::new();
    let mut model = String::new();
    let mut created_at = 0i64;
    let mut stop_reason = String::new();
    let mut content = String::new();
    let mut reasoning: Option<String> = None;
    let mut usage_tokens = ClaudeUsageTokens::default();
    let mut tool_calls: BTreeMap<i64, ToolCallAccumulator> = BTreeMap::new();

    for chunk in chunks {
        let root = cpa_json::parse(chunk);
        match root.g("type").str().as_str() {
            "message_start" => {
                let message = root.g("message");
                if message.exists() {
                    message_id = message.g("id").str();
                    model = message.g("model").str();
                    created_at = unix_now();
                    usage_tokens.merge(&message.g("usage"));
                }
            }
            "content_block_start" => {
                let content_block = root.g("content_block");
                if content_block.exists() && content_block.g("type").str() == "tool_use" {
                    tool_calls.insert(
                        root.g("index").int(),
                        ToolCallAccumulator {
                            id: content_block.g("id").str(),
                            name: content_block.g("name").str(),
                            ..Default::default()
                        },
                    );
                }
            }
            "content_block_delta" => {
                let delta = root.g("delta");
                if delta.exists() {
                    match delta.g("type").str().as_str() {
                        "text_delta" => {
                            let text = delta.g("text");
                            if text.exists() {
                                content.push_str(&text.str());
                            }
                        }
                        "thinking_delta" => {
                            let thinking = delta.g("thinking");
                            if thinking.exists() {
                                reasoning.get_or_insert_with(String::new).push_str(&thinking.str());
                            }
                        }
                        "input_json_delta" => {
                            let partial = delta.g("partial_json");
                            if partial.exists()
                                && let Some(acc) = tool_calls.get_mut(&root.g("index").int())
                            {
                                acc.arguments.push_str(&partial.str());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(acc) = tool_calls.get_mut(&root.g("index").int())
                    && acc.arguments.is_empty()
                {
                    acc.arguments.push_str("{}");
                }
            }
            "message_delta" => {
                let sr = root.g("delta.stop_reason");
                if sr.exists() {
                    stop_reason = sr.str();
                }
                let usage = root.g("usage");
                if usage.exists() {
                    usage_tokens.merge(&usage);
                }
            }
            _ => {}
        }
    }

    if usage_tokens.has_usage {
        usage_tokens.set_on(&mut out);
    }

    cpa_json::set(&mut out, "id", message_id);
    cpa_json::set(&mut out, "created", created_at);
    cpa_json::set(&mut out, "model", model);
    cpa_json::set(&mut out, "choices.0.message.content", content);
    if let Some(reasoning) = reasoning {
        cpa_json::set(&mut out, "choices.0.message.reasoning_content", reasoning);
    }

    // Tool calls in content block order.
    if !tool_calls.is_empty() {
        for (n, acc) in tool_calls.values().enumerate() {
            cpa_json::set(&mut out, &format!("choices.0.message.tool_calls.{n}.id"), acc.id.as_str());
            cpa_json::set(&mut out, &format!("choices.0.message.tool_calls.{n}.type"), "function");
            cpa_json::set(&mut out, &format!("choices.0.message.tool_calls.{n}.function.name"), acc.name.as_str());
            cpa_json::set(
                &mut out,
                &format!("choices.0.message.tool_calls.{n}.function.arguments"),
                acc.arguments.as_str(),
            );
        }
        cpa_json::set(&mut out, "choices.0.finish_reason", "tool_calls");
    } else {
        let finish_reason = map_stop_reason(&stop_reason);
        if finish_reason != "stop" {
            cpa_json::set(&mut out, "choices.0.finish_reason", finish_reason);
        }
    }

    Some(cpa_json::to_vec(&out))
}
