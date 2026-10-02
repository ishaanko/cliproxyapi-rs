//! OpenAI Chat Completions response -> Claude Messages response
//! (Go: openai/claude/openai_claude_response.go).

use std::collections::{BTreeMap, HashMap};

use cpa_core::util::{self, fix_json};
use cpa_json::{Res, Value, J};

use crate::common::{self, append_sse_event_bytes};
use crate::registry::{Ctx, Param};

/// Streaming conversion state (Go: ConvertOpenAIResponseToAnthropicParams).
struct State {
    message_id: String,
    model: String,
    created_at: i64,
    tool_name_map: Option<HashMap<String, String>>,
    /// Whether the original request streams (parsed from it once, on the first non-[DONE] line).
    request_streams: Option<bool>,
    /// True once a tool_use content_block_start has been emitted on the wire. Raw upstream
    /// tool_calls presence can produce stop_reason=tool_use with zero announced tool blocks.
    saw_tool_call: bool,
    content_accumulator: String,
    tool_calls_accumulator: BTreeMap<i64, ToolCallAccumulator>,
    text_content_block_started: bool,
    thinking_content_block_started: bool,
    finish_reason: String,
    content_blocks_stopped: bool,
    message_delta_sent: bool,
    message_started: bool,
    message_stop_sent: bool,
    tool_call_block_indexes: HashMap<i64, i64>,
    text_content_block_index: i64,
    thinking_content_block_index: i64,
    next_content_block_index: i64,
    /// Currently open tool call index (-1 if none).
    open_tool_call_index: i64,
    /// Text or thinking chunks that arrived while a tool call block was open.
    interleaved_content_chunks: Vec<InterleavedContentChunk>,
    usage_input_tokens: i64,
    usage_output_tokens: i64,
    usage_cached_tokens: i64,
    usage_cache_write_tokens: i64,
}

impl Default for State {
    fn default() -> Self {
        State {
            message_id: String::new(),
            model: String::new(),
            created_at: 0,
            tool_name_map: None,
            request_streams: None,
            saw_tool_call: false,
            content_accumulator: String::new(),
            tool_calls_accumulator: BTreeMap::new(),
            text_content_block_started: false,
            thinking_content_block_started: false,
            finish_reason: String::new(),
            content_blocks_stopped: false,
            message_delta_sent: false,
            message_started: false,
            message_stop_sent: false,
            tool_call_block_indexes: HashMap::new(),
            text_content_block_index: -1,
            thinking_content_block_index: -1,
            next_content_block_index: 0,
            open_tool_call_index: -1,
            interleaved_content_chunks: Vec::new(),
            usage_input_tokens: 0,
            usage_output_tokens: 0,
            usage_cached_tokens: 0,
            usage_cache_write_tokens: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    Text,
    Thinking,
}

struct InterleavedContentChunk {
    kind: ChunkKind,
    text: String,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    /// Whether content_block_start has already been sent for this tool index.
    start_emitted: bool,
}

type Results = Vec<Vec<u8>>;

fn tpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

/// One `event: <name>\ndata: <json>\n\n` frame.
fn frame(event: &str, payload: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    append_sse_event_bytes(&mut out, event, &cpa_json::to_vec(payload), 2);
    out
}

/// Converts one OpenAI streaming line into Claude SSE frames.
pub fn convert_openai_response_to_claude(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let state = param.state(State::default);

    let Some(rest) = raw.strip_prefix(b"data:") else {
        return vec![];
    };
    let raw = rest.trim_ascii();

    if state.tool_name_map.is_none() {
        state.tool_name_map = Some(util::tool_name_map_from_claude_request(original));
    }

    if raw == b"[DONE]" {
        return convert_openai_done_to_anthropic(state);
    }

    let streams = *state.request_streams.get_or_insert_with(|| {
        let root = cpa_json::parse(original);
        matches!(root.g("stream").v(), Some(v) if !matches!(v, Value::Bool(false)))
    });
    if !streams {
        return convert_openai_non_streaming_to_anthropic(raw);
    }
    convert_openai_streaming_chunk_to_anthropic(raw, state)
}

fn has_valid_tool_call_arguments(state: &State) -> bool {
    for acc in state.tool_calls_accumulator.values() {
        if !acc.start_emitted && acc.name.is_empty() && acc.id.is_empty() && acc.arguments.is_empty() {
            continue;
        }
        if acc.arguments.is_empty() {
            continue;
        }
        let args = acc.arguments.trim();
        if args.is_empty() {
            return false;
        }
        if args == "{}" {
            continue;
        }
        let fixed = fix_json(args);
        if !cpa_json::valid(fixed.as_bytes()) || !cpa_json::parse_str(&fixed).is_object() {
            return false;
        }
    }
    true
}

fn effective_openai_finish_reason(state: &State) -> String {
    if state.finish_reason == "length" || state.finish_reason == "content_filter" {
        return state.finish_reason.clone();
    }
    if state.saw_tool_call {
        return if has_valid_tool_call_arguments(state) { "tool_calls".into() } else { "length".into() };
    }
    state.finish_reason.clone()
}

fn terminal_openai_finish_reason(state: &State) -> String {
    let reason = effective_openai_finish_reason(state);
    if reason.is_empty() { "stop".into() } else { reason }
}

fn convert_openai_streaming_chunk_to_anthropic(raw: &[u8], state: &mut State) -> Results {
    let root = cpa_json::parse(raw);
    let mut results: Results = Vec::new();

    if state.message_id.is_empty() {
        state.message_id = root.g("id").str();
    }
    if state.model.is_empty() {
        state.model = root.g("model").str();
    }
    if state.created_at == 0 {
        state.created_at = root.g("created").int();
    }

    // message_start goes out on the very first chunk even without a role field: some providers
    // (like Copilot) send tool_calls in the first chunk.
    let delta = root.g("choices.0.delta");
    if delta.exists() {
        if !state.message_started {
            let mut start = tpl(
                r#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            );
            cpa_json::set(&mut start, "message.id", state.message_id.clone());
            cpa_json::set(&mut start, "message.model", state.model.clone());
            results.push(frame("message_start", &start));
            state.message_started = true;
        }

        for reasoning_text in collect_openai_object_reasoning_texts(&delta) {
            if reasoning_text.is_empty() {
                continue;
            }
            if state.open_tool_call_index != -1 {
                match state.interleaved_content_chunks.last_mut() {
                    Some(last) if last.kind == ChunkKind::Thinking => last.text.push_str(&reasoning_text),
                    _ => state
                        .interleaved_content_chunks
                        .push(InterleavedContentChunk { kind: ChunkKind::Thinking, text: reasoning_text }),
                }
            } else {
                stop_text_content_block(state, &mut results);
                if !state.thinking_content_block_started {
                    if state.thinking_content_block_index == -1 {
                        state.thinking_content_block_index = state.next_content_block_index;
                        state.next_content_block_index += 1;
                    }
                    let mut start = tpl(r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#);
                    cpa_json::set(&mut start, "index", state.thinking_content_block_index);
                    results.push(frame("content_block_start", &start));
                    state.thinking_content_block_started = true;
                }

                let mut d = tpl(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#);
                cpa_json::set(&mut d, "index", state.thinking_content_block_index);
                cpa_json::set(&mut d, "delta.thinking", reasoning_text);
                results.push(frame("content_block_delta", &d));
            }
        }

        let content = delta.g("content");
        if content.exists() && !content.str().is_empty() {
            let text = content.str();
            if state.open_tool_call_index != -1 {
                // A tool call block is open on the wire: buffer so blocks stay strictly sequential.
                match state.interleaved_content_chunks.last_mut() {
                    Some(last) if last.kind == ChunkKind::Text => last.text.push_str(&text),
                    _ => state
                        .interleaved_content_chunks
                        .push(InterleavedContentChunk { kind: ChunkKind::Text, text: text.clone() }),
                }
                state.content_accumulator.push_str(&text);
            } else {
                if !state.text_content_block_started {
                    stop_thinking_content_block(state, &mut results);
                    if state.text_content_block_index == -1 {
                        state.text_content_block_index = state.next_content_block_index;
                        state.next_content_block_index += 1;
                    }
                    let mut start = tpl(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#);
                    cpa_json::set(&mut start, "index", state.text_content_block_index);
                    results.push(frame("content_block_start", &start));
                    state.text_content_block_started = true;
                }

                let mut d = tpl(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#);
                cpa_json::set(&mut d, "index", state.text_content_block_index);
                cpa_json::set(&mut d, "delta.text", text.clone());
                results.push(frame("content_block_delta", &d));

                state.content_accumulator.push_str(&text);
            }
        }

        let tool_calls = delta.g("tool_calls");
        if tool_calls.is_array() {
            for (array_index, tool_call) in tool_calls.array().iter().enumerate() {
                let index_node = tool_call.g("index");
                let index = if index_node.exists() { index_node.int() } else { array_index as i64 };

                let empty_map = HashMap::new();
                let tool_name_map = state.tool_name_map.as_ref().unwrap_or(&empty_map);
                let acc = state.tool_calls_accumulator.entry(index).or_default();

                // Only accept JSON-string, non-empty ids so malformed upstream fields cannot
                // overwrite a valid id.
                let id = tool_call.g("id");
                if id.is_string() && !id.str().is_empty() {
                    acc.id = id.str();
                }

                let function = tool_call.g("function");
                if function.exists() {
                    // Only record the name until content_block_start went out; reassigning after
                    // start could drift from what was already announced.
                    if !acc.start_emitted {
                        let name = function.g("name");
                        if name.is_string() && !name.str().is_empty() {
                            acc.name = util::map_tool_name(tool_name_map, &name.str());
                        }
                    }
                    let args = function.g("arguments");
                    if args.exists() {
                        let args_text = args.str();
                        if !args_text.is_empty() {
                            acc.arguments.push_str(&args_text);
                        }
                    }
                }

                // Re-check on every chunk: some upstreams split function.name and id across
                // deltas. Claude requires sequential blocks, so only start when none is open.
                let acc = &state.tool_calls_accumulator[&index];
                if !acc.start_emitted && !acc.name.is_empty() && !acc.id.is_empty() && !state.content_blocks_stopped && state.open_tool_call_index == -1 {
                    emit_tool_use_start(state, index, &mut results);
                }
            }
        }
    }

    // finish_reason is recorded now; message_delta/message_stop wait for usage or [DONE].
    let finish_reason = root.g("choices.0.finish_reason");
    if finish_reason.exists() && !finish_reason.str().is_empty() {
        let reason = finish_reason.str();
        state.finish_reason = if reason == "length" {
            "length".into()
        } else if reason == "content_filter" {
            "content_filter".into()
        } else if state.saw_tool_call {
            if has_valid_tool_call_arguments(state) { "tool_calls".into() } else { "length".into() }
        } else if reason == "tool_calls" {
            "stop".into()
        } else {
            reason
        };

        finalize_openai_anthropic_content_blocks(state, &mut results);
    }

    let usage = root.g("usage");
    let has_usage = usage.exists() && !usage.is_null();
    if has_usage {
        (state.usage_input_tokens, state.usage_output_tokens, state.usage_cached_tokens, state.usage_cache_write_tokens) =
            extract_openai_usage(&usage);
    }

    // Emit message_delta/message_stop only when generation finished: a finish_reason, or a
    // trailing usage-only chunk (no choices) after content/tools started.
    let is_trailing_usage_chunk = has_usage
        && !root.g("choices.0").exists()
        && (!state.finish_reason.is_empty()
            || state.saw_tool_call
            || state.text_content_block_started
            || state.thinking_content_block_started
            || !state.content_accumulator.is_empty()
            || !state.interleaved_content_chunks.is_empty());

    if !state.message_delta_sent && (!state.finish_reason.is_empty() || is_trailing_usage_chunk) && has_usage {
        finalize_openai_anthropic_content_blocks(state, &mut results);
        emit_anthropic_message_delta(state, &mut results);
        emit_message_stop_if_needed(state, &mut results);
    }

    results
}

/// Handles the `[DONE]` marker: closes open blocks and sends the final events.
fn convert_openai_done_to_anthropic(state: &mut State) -> Results {
    let mut results = Vec::new();
    finalize_openai_anthropic_content_blocks(state, &mut results);
    if !state.message_delta_sent {
        emit_anthropic_message_delta(state, &mut results);
    }
    emit_message_stop_if_needed(state, &mut results);
    results
}

/// A non-stream request answered with a bare JSON body arrives as a single "data:" line; the
/// result is the plain Claude message (not SSE framed), as in Go.
fn convert_openai_non_streaming_to_anthropic(raw: &[u8]) -> Results {
    let root = cpa_json::parse(raw);

    let mut out = tpl(
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(&mut out, "id", root.g("id").str());
    cpa_json::set(&mut out, "model", root.g("model").str());

    let choices = root.g("choices");
    if choices.is_array() && !choices.array().is_empty() {
        let choice = choices.g("0");
        let mut content_blocks: Vec<Value> = Vec::new();

        for reasoning_text in collect_openai_object_reasoning_texts(&choice.g("message")) {
            if reasoning_text.is_empty() {
                continue;
            }
            let mut block = tpl(r#"{"type":"thinking","thinking":""}"#);
            cpa_json::set(&mut block, "thinking", reasoning_text);
            content_blocks.push(block);
        }

        let content = choice.g("message.content");
        if content.exists() && !content.str().is_empty() {
            let mut block = tpl(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut block, "text", content.str());
            content_blocks.push(block);
        }

        let tool_calls = choice.g("message.tool_calls");
        if tool_calls.is_array() {
            for tool_call in tool_calls.array() {
                let mut block = tpl(r#"{"type":"tool_use","id":"","name":"","input":{}}"#);
                cpa_json::set(&mut block, "id", util::sanitize_claude_tool_id(&tool_call.g("id").str()));
                cpa_json::set(&mut block, "name", tool_call.g("function.name").str());
                set_tool_input(&mut block, &tool_call.g("function.arguments").str());
                content_blocks.push(block);
            }
        }

        if !content_blocks.is_empty() {
            cpa_json::set(&mut out, "content", Value::Array(content_blocks));
        }

        let finish_reason = choice.g("finish_reason");
        if finish_reason.exists() {
            cpa_json::set(&mut out, "stop_reason", map_openai_finish_reason_to_anthropic(&finish_reason.str()));
        }
    }

    let usage = root.g("usage");
    if usage.exists() {
        set_usage(&mut out, "usage", &usage);
    }

    vec![cpa_json::to_vec(&out)]
}

/// Sets `input` of a tool_use block from OpenAI function arguments (an object, else `{}`).
fn set_tool_input(block: &mut Value, arguments: &str) {
    let args = fix_json(arguments);
    if !args.is_empty() && cpa_json::valid(args.as_bytes()) {
        let parsed = cpa_json::parse_str(&args);
        if parsed.is_object() {
            cpa_json::set(block, "input", parsed);
            return;
        }
    }
    cpa_json::set(block, "input", tpl("{}"));
}

/// Writes the extracted OpenAI usage under `prefix` (`usage` or similar) like Go's usage blocks.
fn set_usage(out: &mut Value, prefix: &str, usage: &Res<'_>) {
    let (input, output, cached, cache_write) = extract_openai_usage(usage);
    cpa_json::set(out, &format!("{prefix}.input_tokens"), input);
    cpa_json::set(out, &format!("{prefix}.output_tokens"), output);
    if cached > 0 {
        cpa_json::set(out, &format!("{prefix}.cache_read_input_tokens"), cached);
    }
    if cache_write > 0 {
        cpa_json::set(out, &format!("{prefix}.cache_creation_input_tokens"), cache_write);
    }
}

fn map_openai_finish_reason_to_anthropic(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        // "stop", "content_filter" (no Anthropic equivalent) and anything unknown.
        _ => "end_turn",
    }
}

/// Content block index for an OpenAI tool index, allocating one on first use.
fn tool_content_block_index(state: &mut State, openai_tool_index: i64) -> i64 {
    if let Some(&idx) = state.tool_call_block_indexes.get(&openai_tool_index) {
        return idx;
    }
    let idx = state.next_content_block_index;
    state.next_content_block_index += 1;
    state.tool_call_block_indexes.insert(openai_tool_index, idx);
    idx
}

/// First non-empty reasoning text list among reasoning_content, reasoning, reasoning_details.
fn collect_openai_object_reasoning_texts(obj: &Res<'_>) -> Vec<String> {
    if !obj.exists() {
        return Vec::new();
    }
    for path in ["reasoning_content", "reasoning", "reasoning_details"] {
        let texts = collect_openai_reasoning_texts(&obj.g(path));
        if !texts.is_empty() {
            return texts;
        }
    }
    Vec::new()
}

fn collect_openai_reasoning_texts(node: &Res<'_>) -> Vec<String> {
    let mut texts = Vec::new();
    if !node.exists() {
        return texts;
    }
    if node.is_array() {
        for value in node.array() {
            texts.extend(collect_openai_reasoning_texts(&value));
        }
        return texts;
    }
    if node.is_string() {
        let text = node.str();
        if !text.is_empty() {
            texts.push(text);
        }
    } else if node.is_object() {
        let text = node.g("text");
        if text.exists() && !text.str().is_empty() {
            texts.push(text.str());
        }
    }
    texts
}

fn stop_thinking_content_block(state: &mut State, results: &mut Results) {
    if !state.thinking_content_block_started {
        return;
    }
    let mut stop = tpl(r#"{"type":"content_block_stop","index":0}"#);
    cpa_json::set(&mut stop, "index", state.thinking_content_block_index);
    results.push(frame("content_block_stop", &stop));
    state.thinking_content_block_started = false;
    state.thinking_content_block_index = -1;
}

fn stop_text_content_block(state: &mut State, results: &mut Results) {
    if !state.text_content_block_started {
        return;
    }
    let mut stop = tpl(r#"{"type":"content_block_stop","index":0}"#);
    cpa_json::set(&mut stop, "index", state.text_content_block_index);
    results.push(frame("content_block_stop", &stop));
    state.text_content_block_started = false;
    state.text_content_block_index = -1;
}

fn emit_message_stop_if_needed(state: &mut State, results: &mut Results) {
    if state.message_stop_sent {
        return;
    }
    results.push(frame("message_stop", &tpl(r#"{"type":"message_stop"}"#)));
    state.message_stop_sent = true;
}

fn emit_tool_use_start(state: &mut State, openai_tool_index: i64, results: &mut Results) {
    stop_thinking_content_block(state, results);
    stop_text_content_block(state, results);

    let block_index = tool_content_block_index(state, openai_tool_index);
    let Some(acc) = state.tool_calls_accumulator.get_mut(&openai_tool_index) else { return };
    let mut start = tpl(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"","input":{}}}"#);
    cpa_json::set(&mut start, "index", block_index);
    cpa_json::set(&mut start, "content_block.id", util::sanitize_claude_tool_id(&acc.id));
    cpa_json::set(&mut start, "content_block.name", acc.name.clone());
    results.push(frame("content_block_start", &start));
    acc.start_emitted = true;
    state.saw_tool_call = true;
    state.open_tool_call_index = openai_tool_index;
}

/// Finalizes a tool_use block that never got a mid-stream start. Some OpenAI-compatible providers
/// leave function.name empty for the whole stream; such calls with an id and/or arguments get a
/// synthesized `tool_<index>` name instead of being dropped. False when there is no usable
/// tool-call signal.
fn emit_belated_tool_use_start(state: &mut State, openai_tool_index: i64, results: &mut Results) -> bool {
    let Some(acc) = state.tool_calls_accumulator.get_mut(&openai_tool_index) else { return false };
    if acc.start_emitted {
        return true;
    }
    if acc.name.is_empty() && acc.id.is_empty() && acc.arguments.is_empty() {
        return false;
    }
    if acc.name.is_empty() {
        acc.name = format!("tool_{openai_tool_index}");
    }
    emit_tool_use_start(state, openai_tool_index, results);
    true
}

fn finalize_single_tool_call(state: &mut State, openai_tool_index: i64, results: &mut Results) {
    let Some(acc) = state.tool_calls_accumulator.get(&openai_tool_index) else { return };
    if !acc.start_emitted && !emit_belated_tool_use_start(state, openai_tool_index, results) {
        return;
    }
    let block_index = tool_content_block_index(state, openai_tool_index);

    // One input_json_delta with all accumulated arguments.
    if let Some(acc) = state.tool_calls_accumulator.get(&openai_tool_index)
        && !acc.arguments.is_empty()
    {
        let mut d = tpl(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#);
        cpa_json::set(&mut d, "index", block_index);
        cpa_json::set(&mut d, "delta.partial_json", fix_json(&acc.arguments));
        results.push(frame("content_block_delta", &d));
    }

    let mut stop = tpl(r#"{"type":"content_block_stop","index":0}"#);
    cpa_json::set(&mut stop, "index", block_index);
    results.push(frame("content_block_stop", &stop));
    state.tool_call_block_indexes.remove(&openai_tool_index);
    state.open_tool_call_index = -1;
}

fn emit_buffered_interleaved_content(state: &mut State, results: &mut Results) {
    if state.interleaved_content_chunks.is_empty() {
        return;
    }
    for chunk in std::mem::take(&mut state.interleaved_content_chunks) {
        if chunk.text.is_empty() {
            continue;
        }
        let idx = state.next_content_block_index;
        state.next_content_block_index += 1;

        let (start_tpl, delta_tpl, delta_path) = match chunk.kind {
            ChunkKind::Thinking => (
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
                "delta.thinking",
            ),
            ChunkKind::Text => (
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
                "delta.text",
            ),
        };
        let mut start = tpl(start_tpl);
        cpa_json::set(&mut start, "index", idx);
        results.push(frame("content_block_start", &start));

        let mut d = tpl(delta_tpl);
        cpa_json::set(&mut d, "index", idx);
        cpa_json::set(&mut d, delta_path, chunk.text);
        results.push(frame("content_block_delta", &d));

        let mut stop = tpl(r#"{"type":"content_block_stop","index":0}"#);
        cpa_json::set(&mut stop, "index", idx);
        results.push(frame("content_block_stop", &stop));
    }
}

fn finalize_openai_anthropic_content_blocks(state: &mut State, results: &mut Results) {
    stop_thinking_content_block(state, results);
    stop_text_content_block(state, results);

    if !state.content_blocks_stopped {
        if state.open_tool_call_index != -1 {
            finalize_single_tool_call(state, state.open_tool_call_index, results);
        }

        let indexes: Vec<i64> = state.tool_calls_accumulator.keys().copied().collect();
        for index in indexes {
            if state.tool_calls_accumulator.get(&index).is_none_or(|acc| acc.start_emitted) {
                continue;
            }
            finalize_single_tool_call(state, index, results);
        }
        state.content_blocks_stopped = true;

        emit_buffered_interleaved_content(state, results);
    }
}

fn emit_anthropic_message_delta(state: &mut State, results: &mut Results) {
    if state.message_delta_sent {
        return;
    }
    let mut d = tpl(
        r#"{"type":"message_delta","delta":{"stop_reason":"","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(&mut d, "delta.stop_reason", map_openai_finish_reason_to_anthropic(&terminal_openai_finish_reason(state)));
    cpa_json::set(&mut d, "usage.input_tokens", state.usage_input_tokens);
    cpa_json::set(&mut d, "usage.output_tokens", state.usage_output_tokens);
    if state.usage_cached_tokens > 0 {
        cpa_json::set(&mut d, "usage.cache_read_input_tokens", state.usage_cached_tokens);
    }
    if state.usage_cache_write_tokens > 0 {
        cpa_json::set(&mut d, "usage.cache_creation_input_tokens", state.usage_cache_write_tokens);
    }
    results.push(frame("message_delta", &d));
    state.message_delta_sent = true;
}

/// Converts a complete (non-streaming) OpenAI response into a Claude message.
pub fn convert_openai_response_to_claude_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let tool_name_map = util::tool_name_map_from_claude_request(original);
    let mut out = tpl(
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(&mut out, "id", root.g("id").str());
    cpa_json::set(&mut out, "model", root.g("model").str());

    let mut has_tool_call = false;
    let mut stop_reason_set = false;
    let mut blocks: Vec<Value> = Vec::new();

    let tool_use_block = |tool_call: &Res<'_>| {
        let mut block = tpl(r#"{"type":"tool_use","id":"","name":"","input":{}}"#);
        cpa_json::set(&mut block, "id", util::sanitize_claude_tool_id(&tool_call.g("id").str()));
        cpa_json::set(&mut block, "name", util::map_tool_name(&tool_name_map, &tool_call.g("function.name").str()));
        set_tool_input(&mut block, &tool_call.g("function.arguments").str());
        block
    };
    let text_block = |text: &str| {
        let mut block = tpl(r#"{"type":"text","text":""}"#);
        cpa_json::set(&mut block, "text", text);
        block
    };
    let thinking_block = |text: &str| {
        let mut block = tpl(r#"{"type":"thinking","thinking":""}"#);
        cpa_json::set(&mut block, "thinking", text);
        block
    };

    let choices = root.g("choices");
    if choices.is_array() && !choices.array().is_empty() {
        let choice = choices.g("0");

        let finish_reason = choice.g("finish_reason");
        if finish_reason.exists() {
            cpa_json::set(&mut out, "stop_reason", map_openai_finish_reason_to_anthropic(&finish_reason.str()));
            stop_reason_set = true;
        }

        let message = choice.g("message");
        if message.exists() {
            let content_result = message.g("content");
            if content_result.exists() {
                if content_result.is_array() {
                    let mut text_builder = String::new();
                    let mut thinking_builder = String::new();

                    fn flush_text(text: &mut String, blocks: &mut Vec<Value>, make: &dyn Fn(&str) -> Value) {
                        if !text.is_empty() {
                            blocks.push(make(text));
                            text.clear();
                        }
                    }

                    for item in content_result.array() {
                        match item.g("type").str().as_str() {
                            "text" => {
                                flush_text(&mut thinking_builder, &mut blocks, &thinking_block);
                                text_builder.push_str(&item.g("text").str());
                            }
                            "tool_calls" => {
                                flush_text(&mut thinking_builder, &mut blocks, &thinking_block);
                                flush_text(&mut text_builder, &mut blocks, &text_block);
                                let tool_calls = item.g("tool_calls");
                                if tool_calls.is_array() {
                                    for tc in tool_calls.array() {
                                        has_tool_call = true;
                                        blocks.push(tool_use_block(&tc));
                                    }
                                }
                            }
                            "reasoning" => {
                                flush_text(&mut text_builder, &mut blocks, &text_block);
                                let thinking = item.g("text");
                                if thinking.exists() {
                                    thinking_builder.push_str(&thinking.str());
                                }
                            }
                            _ => {
                                flush_text(&mut thinking_builder, &mut blocks, &thinking_block);
                                flush_text(&mut text_builder, &mut blocks, &text_block);
                            }
                        }
                    }

                    flush_text(&mut thinking_builder, &mut blocks, &thinking_block);
                    flush_text(&mut text_builder, &mut blocks, &text_block);
                } else if content_result.is_string() {
                    let text = content_result.str();
                    if !text.is_empty() {
                        blocks.push(text_block(&text));
                    }
                }
            }

            for reasoning_text in collect_openai_object_reasoning_texts(&message) {
                if reasoning_text.is_empty() {
                    continue;
                }
                blocks.push(thinking_block(&reasoning_text));
            }

            let tool_calls = message.g("tool_calls");
            if tool_calls.is_array() {
                for tool_call in tool_calls.array() {
                    has_tool_call = true;
                    blocks.push(tool_use_block(&tool_call));
                }
            }
        }
    }

    if !blocks.is_empty() {
        cpa_json::set(&mut out, "content", Value::Array(blocks));
    }

    let resp_usage = root.g("usage");
    if resp_usage.exists() {
        set_usage(&mut out, "usage", &resp_usage);
    }

    if !stop_reason_set {
        cpa_json::set(&mut out, "stop_reason", if has_tool_call { "tool_use" } else { "end_turn" });
    }

    Some(cpa_json::to_vec(&out))
}

/// Claude count_tokens response body.
pub fn claude_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    common::claude_input_tokens_json(count)
}

/// (input, output, cached, cache_write) tokens from an OpenAI usage object. Cached and
/// cache-write tokens are deducted from the input count, as Claude reports them separately.
fn extract_openai_usage(usage: &Res<'_>) -> (i64, i64, i64, i64) {
    if !usage.exists() || usage.is_null() {
        return (0, 0, 0, 0);
    }

    let mut input_tokens = usage.g("prompt_tokens").int();
    let output_tokens = usage.g("completion_tokens").int();
    let cached_tokens = usage.g("prompt_tokens_details.cached_tokens").int();
    let mut cache_write_tokens = usage.g("prompt_tokens_details.cache_write_tokens").int();
    if cache_write_tokens <= 0 {
        cache_write_tokens = usage.g("prompt_tokens_details.cache_creation_tokens").int();
    }

    let mut deduct_tokens = 0i64;
    if cached_tokens > 0 {
        deduct_tokens += cached_tokens;
    }
    if cache_write_tokens > 0 {
        deduct_tokens = deduct_tokens.saturating_add(cache_write_tokens);
    }

    if deduct_tokens > 0 {
        input_tokens = if input_tokens >= deduct_tokens { input_tokens - deduct_tokens } else { 0 };
    }
    if input_tokens < 0 {
        input_tokens = 0;
    }

    (input_tokens, output_tokens, cached_tokens, cache_write_tokens)
}
