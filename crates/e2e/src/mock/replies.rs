//! Renders scripted replies in each upstream family's native wire format.

use bytes::Bytes;
use serde_json::{Value, json};

use super::script::{Content, Reply};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Anthropic,
    /// OpenAI-compatible chat completions provider.
    Compat,
    /// Codex API-key provider (Responses wire format).
    Codex,
    Gemini,
}

impl Family {
    /// URL path prefix the family's base-url uses on the mock.
    pub fn prefix(self) -> &'static str {
        match self {
            Family::Anthropic => "anthropic",
            Family::Compat => "compat",
            Family::Codex => "codex",
            Family::Gemini => "gemini",
        }
    }

    pub fn from_prefix(prefix: &str) -> Option<Self> {
        [Family::Anthropic, Family::Compat, Family::Codex, Family::Gemini].into_iter().find(|f| f.prefix() == prefix)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Generate,
    CountTokens,
    Compact,
    Models,
}

/// Facts about the inbound request that shape the reply.
#[derive(Clone, Debug)]
pub struct ReqCtx {
    pub model: String,
    pub stream: bool,
    /// Name of the first tool declared in the request (for `ToolCall` replies).
    pub tool: String,
}

pub enum RBody {
    Full(Bytes),
    /// Chunks written one by one; `abort` makes the connection fail after the last chunk.
    Chunks { chunks: Vec<Bytes>, abort: bool },
}

pub struct Rendered {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: RBody,
}

const USAGE_IN: u64 = 11;
const USAGE_OUT: u64 = 7;
const USAGE_REASONING: u64 = 3;
const CREATED: u64 = 1_700_000_000;
const TEXT_A: &str = "Hello from ";
const TEXT_B: &str = "mock";
const THINK_A: &str = "Let me ";
const THINK_B: &str = "think.";

/// One SSE event or WebSocket frame.
#[derive(Clone, Debug)]
pub struct Ev {
    pub name: Option<&'static str>,
    pub data: Value,
}

impl Ev {
    fn named(name: &'static str, data: Value) -> Self {
        Ev { name: Some(name), data }
    }

    fn data(data: Value) -> Self {
        Ev { name: None, data }
    }

    fn done() -> Self {
        Ev { name: None, data: Value::String("[DONE]".into()) }
    }
}

fn sse(events: &[Ev]) -> Vec<Bytes> {
    events
        .iter()
        .map(|e| {
            let data = match &e.data {
                Value::String(s) if s == "[DONE]" => s.clone(),
                v => v.to_string(),
            };
            let text = match e.name {
                Some(n) => format!("event: {n}\ndata: {data}\n\n"),
                None => format!("data: {data}\n\n"),
            };
            Bytes::from(text)
        })
        .collect()
}

fn json_body(v: &Value) -> Bytes {
    Bytes::from(v.to_string())
}

pub fn error_body(family: Family, status: u16) -> Value {
    match family {
        Family::Anthropic => {
            let t = match status {
                400 => "invalid_request_error",
                401 => "authentication_error",
                403 => "permission_error",
                404 => "not_found_error",
                429 => "rate_limit_error",
                529 => "overloaded_error",
                _ => "api_error",
            };
            json!({"type":"error","error":{"type":t,"message":format!("mock upstream error {status}")}})
        }
        Family::Gemini => {
            let s = match status {
                400 => "INVALID_ARGUMENT",
                401 => "UNAUTHENTICATED",
                403 => "PERMISSION_DENIED",
                404 => "NOT_FOUND",
                429 => "RESOURCE_EXHAUSTED",
                503 => "UNAVAILABLE",
                _ => "INTERNAL",
            };
            json!({"error":{"code":status,"message":format!("mock upstream error {status}"),"status":s}})
        }
        Family::Compat | Family::Codex => {
            let (t, code) = match status {
                400 => ("invalid_request_error", Value::Null),
                401 => ("authentication_error", json!("invalid_api_key")),
                403 => ("permission_error", Value::Null),
                404 => ("invalid_request_error", json!("model_not_found")),
                429 => ("rate_limit_error", json!("rate_limit_exceeded")),
                _ => ("server_error", Value::Null),
            };
            json!({"error":{"message":format!("mock upstream error {status}"),"type":t,"code":code}})
        }
    }
}

/// In-band error emitted mid-stream (after the first events) by `Reply::StreamError`.
fn stream_error_event(family: Family, seq: u64) -> Ev {
    match family {
        Family::Anthropic => Ev::named("error", json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})),
        Family::Compat => Ev::data(json!({"error":{"message":"mock mid-stream error","type":"server_error","code":"server_error"}})),
        Family::Codex => Ev::named(
            "response.failed",
            json!({"type":"response.failed","sequence_number":seq,"response":{"id":"resp_mock01","object":"response","status":"failed","error":{"code":"server_error","message":"mock mid-stream error"}}}),
        ),
        Family::Gemini => Ev::data(json!({"error":{"code":500,"message":"mock mid-stream error","status":"INTERNAL"}})),
    }
}

/// Renders a scripted reply for the given family and operation.
pub fn render(family: Family, op: Op, ctx: &ReqCtx, reply: &Reply) -> Rendered {
    match reply {
        Reply::Error { status, headers, body } => {
            let body = body.clone().unwrap_or_else(|| error_body(family, *status));
            let mut headers = headers.clone();
            headers.push(("content-type".into(), "application/json".into()));
            Rendered { status: *status, headers, body: RBody::Full(json_body(&body)) }
        }
        Reply::Raw { status, content_type, body } => Rendered {
            status: *status,
            headers: vec![("content-type".into(), content_type.clone())],
            body: RBody::Full(Bytes::from(body.clone())),
        },
        Reply::Ok { content } => success(family, op, ctx, *content, None),
        Reply::Cut { content, after, abort } => success(family, op, ctx, *content, Some(Cut::Truncate { after: *after, abort: *abort })),
        Reply::StreamError { content, after } => success(family, op, ctx, *content, Some(Cut::InBandError { after: *after })),
    }
}

enum Cut {
    Truncate { after: usize, abort: bool },
    InBandError { after: usize },
}

fn success(family: Family, op: Op, ctx: &ReqCtx, content: Content, cut: Option<Cut>) -> Rendered {
    if op == Op::CountTokens {
        let body = match family {
            Family::Gemini => json!({"totalTokens": 42}),
            _ => json!({"input_tokens": 42}),
        };
        return Rendered { status: 200, headers: json_headers(), body: RBody::Full(json_body(&body)) };
    }
    if op == Op::Models {
        return Rendered { status: 200, headers: json_headers(), body: RBody::Full(json_body(&models_body(family))) };
    }
    let streaming = ctx.stream && op != Op::Compact;
    if !streaming {
        let body = json_response(family, ctx, content);
        let bytes = json_body(&body);
        return match cut {
            None => Rendered { status: 200, headers: json_headers(), body: RBody::Full(bytes) },
            Some(Cut::Truncate { after, abort }) => {
                let n = (bytes.len() / 2).min(after.max(1) * 16);
                Rendered { status: 200, headers: json_headers(), body: RBody::Chunks { chunks: vec![bytes.slice(..n)], abort } }
            }
            Some(Cut::InBandError { .. }) => Rendered {
                status: 500,
                headers: json_headers(),
                body: RBody::Full(json_body(&error_body(family, 500))),
            },
        };
    }
    let mut events = stream_events(family, ctx, content);
    let mut abort = false;
    match cut {
        None => {}
        Some(Cut::Truncate { after, abort: a }) => {
            events.truncate(after);
            abort = a;
        }
        Some(Cut::InBandError { after }) => {
            events.truncate(after);
            events.push(stream_error_event(family, after as u64));
        }
    }
    let headers = vec![("content-type".into(), "text/event-stream".into()), ("cache-control".into(), "no-cache".into())];
    Rendered { status: 200, headers, body: RBody::Chunks { chunks: sse(&events), abort } }
}

fn json_headers() -> Vec<(String, String)> {
    vec![("content-type".into(), "application/json".into())]
}

fn models_body(family: Family) -> Value {
    match family {
        Family::Gemini => json!({"models":[{"name":"models/gemini-mock","displayName":"Gemini Mock"}]}),
        Family::Anthropic => json!({"data":[{"id":"claude-mock","type":"model","display_name":"Claude Mock"}],"has_more":false}),
        _ => json!({"object":"list","data":[{"id":"mock-model","object":"model","owned_by":"mock"}]}),
    }
}

/// Non-streaming body for a successful generate call.
pub fn json_response(family: Family, ctx: &ReqCtx, content: Content) -> Value {
    match family {
        Family::Anthropic => anthropic_message(ctx, content),
        Family::Compat => openai_completion(ctx, content),
        Family::Codex => responses_object(ctx, content, final_status(content)),
        Family::Gemini => gemini_response(ctx, content),
    }
}

pub fn stream_events(family: Family, ctx: &ReqCtx, content: Content) -> Vec<Ev> {
    match family {
        Family::Anthropic => anthropic_events(ctx, content),
        Family::Compat => openai_events(ctx, content),
        Family::Codex => responses_events(ctx, content),
        Family::Gemini => gemini_events(ctx, content),
    }
}

// ------------------------------------------------------------ content model

/// One piece of assistant output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Think,
    Text,
    /// Tool call number 1 or 2 (different ids and arguments).
    Tool(u8),
}

fn items(c: Content) -> Vec<Item> {
    match c {
        Content::Text | Content::Length | Content::Cached => vec![Item::Text],
        Content::Thinking => vec![Item::Think, Item::Text],
        Content::ToolCall => vec![Item::Tool(1)],
        Content::Parallel => vec![Item::Tool(1), Item::Tool(2)],
        Content::Mixed => vec![Item::Text, Item::Tool(1)],
        // Only the Responses family renders images; the others answer plain text.
        Content::Image => vec![Item::Text],
    }
}

fn has_tool(c: Content) -> bool {
    items(c).iter().any(|i| matches!(i, Item::Tool(_)))
}

fn truncated(c: Content) -> bool {
    c == Content::Length
}

fn cached(c: Content) -> bool {
    c == Content::Cached
}

fn tool_id(prefix: &str, n: u8) -> String {
    format!("{prefix}_mock{n:02}")
}

fn tool_input(n: u8) -> Value {
    if n == 1 { json!({"city": "Paris"}) } else { json!({"city": "Rome"}) }
}

/// A JSON string cut in two, to stream as partial argument deltas.
fn halves(s: &str) -> (String, String) {
    let mid = s.len() / 2;
    (s[..mid].to_string(), s[mid..].to_string())
}

// ---------------------------------------------------------------- Anthropic

fn anthropic_usage(c: Content) -> Value {
    let mut u = json!({"input_tokens":USAGE_IN,"output_tokens":USAGE_OUT});
    if cached(c) {
        u["cache_read_input_tokens"] = json!(5);
        u["cache_creation_input_tokens"] = json!(3);
    }
    u
}

fn anthropic_stop(c: Content) -> &'static str {
    if truncated(c) {
        "max_tokens"
    } else if has_tool(c) {
        "tool_use"
    } else {
        "end_turn"
    }
}

fn anthropic_blocks(ctx: &ReqCtx, c: Content) -> Vec<Value> {
    items(c)
        .into_iter()
        .map(|i| match i {
            Item::Think => json!({"type":"thinking","thinking":format!("{THINK_A}{THINK_B}"),"signature":"c2lnbmF0dXJlLW1vY2s="}),
            Item::Text => json!({"type":"text","text":format!("{TEXT_A}{TEXT_B}")}),
            Item::Tool(n) => json!({"type":"tool_use","id":tool_id("toolu", n),"name":ctx.tool,"input":tool_input(n)}),
        })
        .collect()
}

fn anthropic_message(ctx: &ReqCtx, c: Content) -> Value {
    json!({
        "id":"msg_mock01","type":"message","role":"assistant","model":ctx.model,
        "content":anthropic_blocks(ctx, c),"stop_reason":anthropic_stop(c),"stop_sequence":null,
        "usage":anthropic_usage(c)
    })
}

fn anthropic_events(ctx: &ReqCtx, c: Content) -> Vec<Ev> {
    let mut evs = vec![
        Ev::named(
            "message_start",
            json!({"type":"message_start","message":{
                "id":"msg_mock01","type":"message","role":"assistant","model":ctx.model,"content":[],
                "stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":USAGE_IN,"output_tokens":1}}}),
        ),
        Ev::named("ping", json!({"type":"ping"})),
    ];
    for (i, block) in anthropic_blocks(ctx, c).iter().enumerate() {
        let (start, deltas): (Value, Vec<Value>) = match block["type"].as_str().unwrap_or_default() {
            "thinking" => (
                json!({"type":"thinking","thinking":""}),
                vec![
                    json!({"type":"thinking_delta","thinking":THINK_A}),
                    json!({"type":"thinking_delta","thinking":THINK_B}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ],
            ),
            "tool_use" => {
                let (a, b) = halves(&block["input"].to_string());
                (
                    json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                    vec![json!({"type":"input_json_delta","partial_json":a}), json!({"type":"input_json_delta","partial_json":b})],
                )
            }
            _ => (
                json!({"type":"text","text":""}),
                vec![json!({"type":"text_delta","text":TEXT_A}), json!({"type":"text_delta","text":TEXT_B})],
            ),
        };
        evs.push(Ev::named("content_block_start", json!({"type":"content_block_start","index":i,"content_block":start})));
        for d in deltas {
            evs.push(Ev::named("content_block_delta", json!({"type":"content_block_delta","index":i,"delta":d})));
        }
        evs.push(Ev::named("content_block_stop", json!({"type":"content_block_stop","index":i})));
    }
    evs.push(Ev::named(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":anthropic_stop(c),"stop_sequence":null},"usage":anthropic_usage(c)}),
    ));
    evs.push(Ev::named("message_stop", json!({"type":"message_stop"})));
    evs
}

// ------------------------------------------------------------ OpenAI chat

fn openai_usage(c: Content) -> Value {
    let mut usage = json!({"prompt_tokens":USAGE_IN,"completion_tokens":USAGE_OUT,"total_tokens":USAGE_IN + USAGE_OUT});
    if c == Content::Thinking {
        usage["completion_tokens_details"] = json!({"reasoning_tokens":USAGE_REASONING});
    }
    if cached(c) {
        usage["prompt_tokens_details"] = json!({"cached_tokens":5});
    }
    usage
}

fn openai_finish(c: Content) -> &'static str {
    if truncated(c) {
        "length"
    } else if has_tool(c) {
        "tool_calls"
    } else {
        "stop"
    }
}

fn openai_completion(ctx: &ReqCtx, c: Content) -> Value {
    let its = items(c);
    let text = its.contains(&Item::Text).then(|| format!("{TEXT_A}{TEXT_B}"));
    let mut message = json!({"role":"assistant","content":text});
    if its.contains(&Item::Think) {
        message["reasoning_content"] = json!(format!("{THINK_A}{THINK_B}"));
    }
    let calls: Vec<Value> = its
        .iter()
        .filter_map(|i| match i {
            Item::Tool(n) => Some(json!({"id":tool_id("call", *n),"type":"function","function":{"name":ctx.tool,"arguments":tool_input(*n).to_string()}})),
            _ => None,
        })
        .collect();
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id":"chatcmpl-mock01","object":"chat.completion","created":CREATED,"model":ctx.model,
        "choices":[{"index":0,"message":message,"finish_reason":openai_finish(c)}],"usage":openai_usage(c)
    })
}

fn openai_chunk(ctx: &ReqCtx, delta: Value, finish: Value) -> Ev {
    Ev::data(json!({
        "id":"chatcmpl-mock01","object":"chat.completion.chunk","created":CREATED,"model":ctx.model,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    }))
}

fn openai_events(ctx: &ReqCtx, c: Content) -> Vec<Ev> {
    let mut evs = vec![openai_chunk(ctx, json!({"role":"assistant","content":""}), Value::Null)];
    let mut tool_index = 0;
    for item in items(c) {
        match item {
            Item::Think => {
                for d in [THINK_A, THINK_B] {
                    evs.push(openai_chunk(ctx, json!({"reasoning_content":d}), Value::Null));
                }
            }
            Item::Text => {
                for d in [TEXT_A, TEXT_B] {
                    evs.push(openai_chunk(ctx, json!({"content":d}), Value::Null));
                }
            }
            Item::Tool(n) => {
                evs.push(openai_chunk(
                    ctx,
                    json!({"tool_calls":[{"index":tool_index,"id":tool_id("call", n),"type":"function","function":{"name":ctx.tool,"arguments":""}}]}),
                    Value::Null,
                ));
                let (a, b) = halves(&tool_input(n).to_string());
                for part in [a, b] {
                    evs.push(openai_chunk(ctx, json!({"tool_calls":[{"index":tool_index,"function":{"arguments":part}}]}), Value::Null));
                }
                tool_index += 1;
            }
        }
    }
    evs.push(openai_chunk(ctx, json!({}), json!(openai_finish(c))));
    evs.push(Ev::data(json!({
        "id":"chatcmpl-mock01","object":"chat.completion.chunk","created":CREATED,"model":ctx.model,"choices":[],
        "usage":openai_usage(c)
    })));
    evs.push(Ev::done());
    evs
}

// -------------------------------------------------------------- Responses

fn responses_usage(c: Content) -> Value {
    let reasoning = if c == Content::Thinking { USAGE_REASONING } else { 0 };
    let cached_tokens = if cached(c) { 5 } else { 0 };
    json!({
        "input_tokens":USAGE_IN,"input_tokens_details":{"cached_tokens":cached_tokens},
        "output_tokens":USAGE_OUT,"output_tokens_details":{"reasoning_tokens":reasoning},
        "total_tokens":USAGE_IN + USAGE_OUT
    })
}

/// `image_generation_call` output item of a Responses image answer.
fn image_call_item() -> Value {
    json!({"id":"ig_mock01","type":"image_generation_call","status":"completed","result":"AAEC","revised_prompt":"mock revised",
        "output_format":"png","size":"1024x1024","background":"opaque","quality":"high"})
}

fn responses_items(ctx: &ReqCtx, c: Content) -> Vec<Value> {
    if c == Content::Image {
        return vec![image_call_item()];
    }
    items(c)
        .into_iter()
        .map(|i| match i {
            Item::Think => json!({"id":"rs_mock01","type":"reasoning","summary":[{"type":"summary_text","text":format!("{THINK_A}{THINK_B}")}]}),
            Item::Text => json!({"id":"msg_mock01","type":"message","status":"completed","role":"assistant",
                "content":[{"type":"output_text","annotations":[],"text":format!("{TEXT_A}{TEXT_B}")}]}),
            Item::Tool(n) => json!({"id":tool_id("fc", n),"type":"function_call","status":"completed",
                "call_id":tool_id("call", n),"name":ctx.tool,"arguments":tool_input(n).to_string()}),
        })
        .collect()
}

fn responses_object(ctx: &ReqCtx, c: Content, status: &str) -> Value {
    let done = status != "in_progress";
    let output = if done { responses_items(ctx, c) } else { vec![] };
    let mut obj = json!({
        "id":"resp_mock01","object":"response","created_at":CREATED,"status":status,"model":ctx.model,
        "output":output,"parallel_tool_calls":true,"store":false
    });
    if done {
        obj["usage"] = responses_usage(c);
        if c == Content::Image {
            obj["tool_usage"] = json!({"image_gen":{"input_tokens":5,"output_tokens":7,"total_tokens":12,"input_tokens_details":{"cached_tokens":2}}});
        }
    }
    if status == "incomplete" {
        obj["incomplete_details"] = json!({"reason":"max_output_tokens"});
    }
    obj
}

fn final_status(c: Content) -> &'static str {
    if truncated(c) { "incomplete" } else { "completed" }
}

fn responses_events(ctx: &ReqCtx, c: Content) -> Vec<Ev> {
    let mut seq = 0u64;
    let mut next = || {
        seq += 1;
        seq - 1
    };
    let mut evs = vec![
        Ev::named("response.created", json!({"type":"response.created","sequence_number":next(),"response":responses_object(ctx, c, "in_progress")})),
        Ev::named("response.in_progress", json!({"type":"response.in_progress","sequence_number":next(),"response":responses_object(ctx, c, "in_progress")})),
    ];
    for (i, item) in responses_items(ctx, c).iter().enumerate() {
        let kind = item["type"].as_str().unwrap_or_default();
        let id = item["id"].clone();
        match kind {
            "image_generation_call" => {
                evs.push(Ev::named("response.output_item.added", json!({"type":"response.output_item.added","sequence_number":next(),"output_index":i,"item":{"id":id,"type":"image_generation_call","status":"in_progress"}})));
                evs.push(Ev::named("response.image_generation_call.in_progress", json!({"type":"response.image_generation_call.in_progress","sequence_number":next(),"item_id":id,"output_index":i})));
                evs.push(Ev::named("response.image_generation_call.generating", json!({"type":"response.image_generation_call.generating","sequence_number":next(),"item_id":id,"output_index":i})));
                evs.push(Ev::named("response.image_generation_call.partial_image", json!({"type":"response.image_generation_call.partial_image","sequence_number":next(),"item_id":id,"output_index":i,"partial_image_index":0,"partial_image_b64":"AAE=","output_format":"png","size":"1024x1024","quality":"high","background":"opaque"})));
            }
            "reasoning" => {
                evs.push(Ev::named("response.output_item.added", json!({"type":"response.output_item.added","sequence_number":next(),"output_index":i,"item":{"id":id,"type":"reasoning","summary":[]}})));
                evs.push(Ev::named("response.reasoning_summary_part.added", json!({"type":"response.reasoning_summary_part.added","sequence_number":next(),"item_id":id,"output_index":i,"summary_index":0,"part":{"type":"summary_text","text":""}})));
                for d in [THINK_A, THINK_B] {
                    evs.push(Ev::named("response.reasoning_summary_text.delta", json!({"type":"response.reasoning_summary_text.delta","sequence_number":next(),"item_id":id,"output_index":i,"summary_index":0,"delta":d})));
                }
                evs.push(Ev::named("response.reasoning_summary_text.done", json!({"type":"response.reasoning_summary_text.done","sequence_number":next(),"item_id":id,"output_index":i,"summary_index":0,"text":format!("{THINK_A}{THINK_B}")})));
                evs.push(Ev::named("response.reasoning_summary_part.done", json!({"type":"response.reasoning_summary_part.done","sequence_number":next(),"item_id":id,"output_index":i,"summary_index":0,"part":{"type":"summary_text","text":format!("{THINK_A}{THINK_B}")}})));
            }
            "function_call" => {
                evs.push(Ev::named("response.output_item.added", json!({"type":"response.output_item.added","sequence_number":next(),"output_index":i,"item":{"id":id,"type":"function_call","status":"in_progress","call_id":item["call_id"],"name":item["name"],"arguments":""}})));
                let (a, b) = halves(item["arguments"].as_str().unwrap_or_default());
                for d in [a, b] {
                    evs.push(Ev::named("response.function_call_arguments.delta", json!({"type":"response.function_call_arguments.delta","sequence_number":next(),"item_id":id,"output_index":i,"delta":d})));
                }
                evs.push(Ev::named("response.function_call_arguments.done", json!({"type":"response.function_call_arguments.done","sequence_number":next(),"item_id":id,"output_index":i,"arguments":item["arguments"]})));
            }
            _ => {
                evs.push(Ev::named("response.output_item.added", json!({"type":"response.output_item.added","sequence_number":next(),"output_index":i,"item":{"id":id,"type":"message","status":"in_progress","role":"assistant","content":[]}})));
                evs.push(Ev::named("response.content_part.added", json!({"type":"response.content_part.added","sequence_number":next(),"item_id":id,"output_index":i,"content_index":0,"part":{"type":"output_text","annotations":[],"text":""}})));
                for d in [TEXT_A, TEXT_B] {
                    evs.push(Ev::named("response.output_text.delta", json!({"type":"response.output_text.delta","sequence_number":next(),"item_id":id,"output_index":i,"content_index":0,"delta":d})));
                }
                evs.push(Ev::named("response.output_text.done", json!({"type":"response.output_text.done","sequence_number":next(),"item_id":id,"output_index":i,"content_index":0,"text":format!("{TEXT_A}{TEXT_B}")})));
                evs.push(Ev::named("response.content_part.done", json!({"type":"response.content_part.done","sequence_number":next(),"item_id":id,"output_index":i,"content_index":0,"part":{"type":"output_text","annotations":[],"text":format!("{TEXT_A}{TEXT_B}")}})));
            }
        }
        evs.push(Ev::named("response.output_item.done", json!({"type":"response.output_item.done","sequence_number":next(),"output_index":i,"item":item})));
    }
    let terminal = if truncated(c) { "response.incomplete" } else { "response.completed" };
    evs.push(Ev::named(
        terminal,
        json!({"type":terminal,"sequence_number":next(),"response":responses_object(ctx, c, final_status(c))}),
    ));
    evs
}

/// Events for the Codex upstream WebSocket (one JSON text frame per event).
pub fn codex_ws_frames(ctx: &ReqCtx, content: Content) -> Vec<Value> {
    responses_events(ctx, content).into_iter().map(|e| e.data).collect()
}

// ----------------------------------------------------------------- Gemini

fn gemini_usage(c: Content) -> Value {
    let mut usage = json!({"promptTokenCount":USAGE_IN,"candidatesTokenCount":USAGE_OUT,"totalTokenCount":USAGE_IN + USAGE_OUT});
    if c == Content::Thinking {
        usage["thoughtsTokenCount"] = json!(USAGE_REASONING);
        usage["totalTokenCount"] = json!(USAGE_IN + USAGE_OUT + USAGE_REASONING);
    }
    if cached(c) {
        usage["cachedContentTokenCount"] = json!(5);
    }
    usage
}

fn gemini_finish(c: Content) -> &'static str {
    if truncated(c) { "MAX_TOKENS" } else { "STOP" }
}

fn gemini_parts(ctx: &ReqCtx, c: Content) -> Vec<Value> {
    items(c)
        .into_iter()
        .map(|i| match i {
            Item::Think => json!({"text":format!("{THINK_A}{THINK_B}"),"thought":true}),
            Item::Text => json!({"text":format!("{TEXT_A}{TEXT_B}")}),
            Item::Tool(n) => json!({"functionCall":{"name":ctx.tool,"args":tool_input(n)}}),
        })
        .collect()
}

fn gemini_response(ctx: &ReqCtx, c: Content) -> Value {
    json!({
        "candidates":[{"content":{"role":"model","parts":gemini_parts(ctx, c)},"finishReason":gemini_finish(c),"index":0}],
        "usageMetadata":gemini_usage(c),"modelVersion":ctx.model,"responseId":"mockresp01"
    })
}

fn gemini_events(ctx: &ReqCtx, c: Content) -> Vec<Ev> {
    // One part list per chunk; text and thoughts stream in two pieces.
    let mut chunks: Vec<Vec<Value>> = vec![];
    for item in items(c) {
        match item {
            Item::Think => chunks.extend([THINK_A, THINK_B].map(|d| vec![json!({"text":d,"thought":true})])),
            Item::Text => chunks.extend([TEXT_A, TEXT_B].map(|d| vec![json!({"text":d})])),
            Item::Tool(n) => chunks.push(vec![json!({"functionCall":{"name":ctx.tool,"args":tool_input(n)}})]),
        }
    }
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, parts)| {
            let mut candidate = json!({"content":{"role":"model","parts":parts},"index":0});
            let mut resp = json!({"modelVersion":ctx.model,"responseId":"mockresp01"});
            if i == last {
                candidate["finishReason"] = json!(gemini_finish(c));
                resp["usageMetadata"] = gemini_usage(c);
            }
            resp["candidates"] = json!([candidate]);
            Ev::data(resp)
        })
        .collect()
}
