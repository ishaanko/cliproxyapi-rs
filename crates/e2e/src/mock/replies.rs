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
        Family::Codex => responses_object(ctx, content, "completed"),
        Family::Gemini => gemini_response(ctx, content, true),
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

// ---------------------------------------------------------------- Anthropic

fn tool_args() -> Value {
    json!({"city": "Paris"})
}

fn anthropic_content(ctx: &ReqCtx, content: Content) -> (Vec<Value>, &'static str) {
    match content {
        Content::Text => (vec![json!({"type":"text","text":format!("{TEXT_A}{TEXT_B}")})], "end_turn"),
        Content::Thinking => (
            vec![
                json!({"type":"thinking","thinking":format!("{THINK_A}{THINK_B}"),"signature":"c2lnbmF0dXJlLW1vY2s="}),
                json!({"type":"text","text":format!("{TEXT_A}{TEXT_B}")}),
            ],
            "end_turn",
        ),
        Content::ToolCall => (
            vec![json!({"type":"tool_use","id":"toolu_mock01","name":ctx.tool,"input":tool_args()})],
            "tool_use",
        ),
    }
}

fn anthropic_message(ctx: &ReqCtx, content: Content) -> Value {
    let (blocks, stop) = anthropic_content(ctx, content);
    json!({
        "id":"msg_mock01","type":"message","role":"assistant","model":ctx.model,
        "content":blocks,"stop_reason":stop,"stop_sequence":null,
        "usage":{"input_tokens":USAGE_IN,"output_tokens":USAGE_OUT}
    })
}

fn anthropic_events(ctx: &ReqCtx, content: Content) -> Vec<Ev> {
    let (blocks, stop) = anthropic_content(ctx, content);
    let mut evs = vec![Ev::named(
        "message_start",
        json!({"type":"message_start","message":{
            "id":"msg_mock01","type":"message","role":"assistant","model":ctx.model,"content":[],
            "stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":USAGE_IN,"output_tokens":1}}}),
    )];
    for (i, block) in blocks.iter().enumerate() {
        let (start, deltas): (Value, Vec<Value>) = match block["type"].as_str().unwrap_or_default() {
            "thinking" => (
                json!({"type":"thinking","thinking":""}),
                vec![
                    json!({"type":"thinking_delta","thinking":THINK_A}),
                    json!({"type":"thinking_delta","thinking":THINK_B}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ],
            ),
            "tool_use" => (
                json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                vec![
                    json!({"type":"input_json_delta","partial_json":"{\"city\":"}),
                    json!({"type":"input_json_delta","partial_json":"\"Paris\"}"}),
                ],
            ),
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
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"input_tokens":USAGE_IN,"output_tokens":USAGE_OUT}}),
    ));
    evs.push(Ev::named("message_stop", json!({"type":"message_stop"})));
    evs
}

// ------------------------------------------------------------ OpenAI chat

fn openai_completion(ctx: &ReqCtx, content: Content) -> Value {
    let (message, finish) = match content {
        Content::Text => (json!({"role":"assistant","content":format!("{TEXT_A}{TEXT_B}")}), "stop"),
        Content::Thinking => (
            json!({"role":"assistant","content":format!("{TEXT_A}{TEXT_B}"),"reasoning_content":format!("{THINK_A}{THINK_B}")}),
            "stop",
        ),
        Content::ToolCall => (
            json!({"role":"assistant","content":null,"tool_calls":[{
                "id":"call_mock01","type":"function","function":{"name":ctx.tool,"arguments":tool_args().to_string()}}]}),
            "tool_calls",
        ),
    };
    let mut usage = json!({"prompt_tokens":USAGE_IN,"completion_tokens":USAGE_OUT,"total_tokens":USAGE_IN + USAGE_OUT});
    if content == Content::Thinking {
        usage["completion_tokens_details"] = json!({"reasoning_tokens":USAGE_REASONING});
    }
    json!({
        "id":"chatcmpl-mock01","object":"chat.completion","created":CREATED,"model":ctx.model,
        "choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage
    })
}

fn openai_chunk(ctx: &ReqCtx, delta: Value, finish: Value) -> Ev {
    Ev::data(json!({
        "id":"chatcmpl-mock01","object":"chat.completion.chunk","created":CREATED,"model":ctx.model,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    }))
}

fn openai_events(ctx: &ReqCtx, content: Content) -> Vec<Ev> {
    let mut evs = vec![openai_chunk(ctx, json!({"role":"assistant","content":""}), Value::Null)];
    let finish = match content {
        Content::Text | Content::Thinking => {
            if content == Content::Thinking {
                evs.push(openai_chunk(ctx, json!({"reasoning_content":THINK_A}), Value::Null));
                evs.push(openai_chunk(ctx, json!({"reasoning_content":THINK_B}), Value::Null));
            }
            evs.push(openai_chunk(ctx, json!({"content":TEXT_A}), Value::Null));
            evs.push(openai_chunk(ctx, json!({"content":TEXT_B}), Value::Null));
            "stop"
        }
        Content::ToolCall => {
            evs.push(openai_chunk(
                ctx,
                json!({"tool_calls":[{"index":0,"id":"call_mock01","type":"function","function":{"name":ctx.tool,"arguments":""}}]}),
                Value::Null,
            ));
            for part in ["{\"city\":", "\"Paris\"}"] {
                evs.push(openai_chunk(ctx, json!({"tool_calls":[{"index":0,"function":{"arguments":part}}]}), Value::Null));
            }
            "tool_calls"
        }
    };
    evs.push(openai_chunk(ctx, json!({}), json!(finish)));
    evs.push(Ev::data(json!({
        "id":"chatcmpl-mock01","object":"chat.completion.chunk","created":CREATED,"model":ctx.model,"choices":[],
        "usage":{"prompt_tokens":USAGE_IN,"completion_tokens":USAGE_OUT,"total_tokens":USAGE_IN + USAGE_OUT}
    })));
    evs.push(Ev::done());
    evs
}

// -------------------------------------------------------------- Responses

fn responses_usage(content: Content) -> Value {
    let reasoning = if content == Content::Thinking { USAGE_REASONING } else { 0 };
    json!({
        "input_tokens":USAGE_IN,"input_tokens_details":{"cached_tokens":0},
        "output_tokens":USAGE_OUT,"output_tokens_details":{"reasoning_tokens":reasoning},
        "total_tokens":USAGE_IN + USAGE_OUT
    })
}

fn responses_items(ctx: &ReqCtx, content: Content) -> Vec<Value> {
    let message = json!({"id":"msg_mock01","type":"message","status":"completed","role":"assistant",
        "content":[{"type":"output_text","annotations":[],"text":format!("{TEXT_A}{TEXT_B}")}]});
    match content {
        Content::Text => vec![message],
        Content::Thinking => vec![
            json!({"id":"rs_mock01","type":"reasoning","summary":[{"type":"summary_text","text":format!("{THINK_A}{THINK_B}")}]}),
            message,
        ],
        Content::ToolCall => vec![json!({"id":"fc_mock01","type":"function_call","status":"completed",
            "call_id":"call_mock01","name":ctx.tool,"arguments":tool_args().to_string()})],
    }
}

fn responses_object(ctx: &ReqCtx, content: Content, status: &str) -> Value {
    let output = if status == "completed" { responses_items(ctx, content) } else { vec![] };
    let mut obj = json!({
        "id":"resp_mock01","object":"response","created_at":CREATED,"status":status,"model":ctx.model,
        "output":output,"parallel_tool_calls":true,"store":false
    });
    if status == "completed" {
        obj["usage"] = responses_usage(content);
    }
    obj
}

fn responses_events(ctx: &ReqCtx, content: Content) -> Vec<Ev> {
    let mut seq = 0u64;
    let mut next = || {
        seq += 1;
        seq - 1
    };
    let mut evs = vec![
        Ev::named("response.created", json!({"type":"response.created","sequence_number":next(),"response":responses_object(ctx, content, "in_progress")})),
        Ev::named("response.in_progress", json!({"type":"response.in_progress","sequence_number":next(),"response":responses_object(ctx, content, "in_progress")})),
    ];
    let items = responses_items(ctx, content);
    for (i, item) in items.iter().enumerate() {
        let kind = item["type"].as_str().unwrap_or_default();
        let id = item["id"].clone();
        match kind {
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
                for d in ["{\"city\":", "\"Paris\"}"] {
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
    evs.push(Ev::named("response.completed", json!({"type":"response.completed","sequence_number":next(),"response":responses_object(ctx, content, "completed")})));
    evs
}

/// Events for the Codex upstream WebSocket (one JSON text frame per event).
pub fn codex_ws_frames(ctx: &ReqCtx, content: Content) -> Vec<Value> {
    responses_events(ctx, content).into_iter().map(|e| e.data).collect()
}

// ----------------------------------------------------------------- Gemini

fn gemini_parts(ctx: &ReqCtx, content: Content) -> Vec<Value> {
    match content {
        Content::Text => vec![json!({"text":format!("{TEXT_A}{TEXT_B}")})],
        Content::Thinking => vec![
            json!({"text":format!("{THINK_A}{THINK_B}"),"thought":true}),
            json!({"text":format!("{TEXT_A}{TEXT_B}")}),
        ],
        Content::ToolCall => vec![json!({"functionCall":{"name":ctx.tool,"args":tool_args()}})],
    }
}

fn gemini_usage(content: Content) -> Value {
    let mut usage = json!({"promptTokenCount":USAGE_IN,"candidatesTokenCount":USAGE_OUT,"totalTokenCount":USAGE_IN + USAGE_OUT});
    if content == Content::Thinking {
        usage["thoughtsTokenCount"] = json!(USAGE_REASONING);
        usage["totalTokenCount"] = json!(USAGE_IN + USAGE_OUT + USAGE_REASONING);
    }
    usage
}

fn gemini_response(ctx: &ReqCtx, content: Content, last: bool) -> Value {
    let mut candidate = json!({"content":{"role":"model","parts":gemini_parts(ctx, content)},"index":0});
    let mut resp = json!({"candidates":[candidate.clone()],"modelVersion":ctx.model,"responseId":"mockresp01"});
    if last {
        candidate["finishReason"] = json!("STOP");
        resp["candidates"] = json!([candidate]);
        resp["usageMetadata"] = gemini_usage(content);
    }
    resp
}

fn gemini_events(ctx: &ReqCtx, content: Content) -> Vec<Ev> {
    let chunk = |parts: Vec<Value>, last: bool| {
        let mut candidate = json!({"content":{"role":"model","parts":parts},"index":0});
        let mut resp = json!({"modelVersion":ctx.model,"responseId":"mockresp01"});
        if last {
            candidate["finishReason"] = json!("STOP");
            resp["usageMetadata"] = gemini_usage(content);
        }
        resp["candidates"] = json!([candidate]);
        Ev::data(resp)
    };
    match content {
        Content::Text => vec![chunk(vec![json!({"text":TEXT_A})], false), chunk(vec![json!({"text":TEXT_B})], true)],
        Content::Thinking => vec![
            chunk(vec![json!({"text":THINK_A,"thought":true})], false),
            chunk(vec![json!({"text":THINK_B,"thought":true})], false),
            chunk(vec![json!({"text":format!("{TEXT_A}{TEXT_B}")})], true),
        ],
        Content::ToolCall => vec![chunk(gemini_parts(ctx, content), true)],
    }
}
