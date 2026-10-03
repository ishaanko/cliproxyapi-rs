//! Benchmark scenarios: client request, matching direct-to-mock route, and large conversation
//! generators.

use bytes::Bytes;
use hyper::Method;
use serde_json::{Value, json};

/// Mock upstream behavior for a scenario, applied through `/__ctl` before its cells run.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// Think time before the response starts, uniform in `first_ms..=first_max_ms`.
    pub first_ms: u64,
    pub first_max_ms: u64,
    /// Pause between SSE events and the number of text deltas per stream.
    pub gap_us: u64,
    pub chunks: usize,
}

impl Shape {
    /// Zero-latency upstream with short 20-delta streams: isolates proxy overhead.
    pub const FAST: Shape = Shape { first_ms: 0, first_max_ms: 0, gap_us: 0, chunks: 20 };
    /// Paced stream used by the streaming-overhead test.
    pub const PACED: Shape = Shape { first_ms: 20, first_max_ms: 0, gap_us: 5000, chunks: 40 };
}

/// Everything the harness needs to drive one scenario.
pub struct Scenario {
    pub id: &'static str,
    pub shape: Shape,
    /// Default measured request count for `cpa-bench quick`.
    pub quick_requests: u64,
    pub description: &'static str,
    pub method: Method,
    /// Path on the server under test.
    pub path: String,
    /// Same request sent straight to the mock (baseline); `None` when the server answers alone.
    pub direct_path: Option<String>,
    pub body: Bytes,
    /// SSE scenario: eligible for the streaming-timing test.
    pub stream: bool,
    /// Substring a correct 200 response from the server must contain (guards against measuring
    /// error paths). Not checked for direct-to-mock requests.
    pub expect: &'static str,
    /// Cell kind in the results: `tput` (the five standard scenarios), `large` or `extra`.
    pub kind: &'static str,
    /// Concurrency levels for `extra` scenarios (standard ones use `--conc`).
    pub conc: &'static [usize],
    /// Measured seconds per cell for `extra` scenarios.
    pub measure_s: f64,
    /// `quick` defaults: concurrency and the minimum requests per connection.
    pub quick_conc: usize,
    pub quick_waves: u64,
    /// Drive over a websocket (`response.create` per request) instead of plain HTTP.
    pub ws: bool,
}

impl Scenario {
    /// Defaults shared by every scenario; literals override what differs.
    fn base() -> Scenario {
        Scenario {
            id: "",
            shape: Shape::FAST,
            quick_requests: 6000,
            description: "",
            method: Method::POST,
            path: String::new(),
            direct_path: None,
            body: Bytes::new(),
            stream: false,
            expect: "",
            kind: "tput",
            conc: &[],
            measure_s: 5.0,
            quick_conc: 64,
            quick_waves: 6,
            ws: false,
        }
    }
}

const CLAUDE_MODEL: &str = "claude-sonnet-4-5-20250929";
const CODEX_MODEL: &str = "gpt-5.5";
const GEMINI_MODEL: &str = "gemini-2.5-flash";
const COMPAT_MODEL: &str = "compat-gpt-4o";

fn bytes(v: &Value) -> Bytes {
    Bytes::from(v.to_string())
}

fn weather_tool_chat() -> Value {
    json!({"type":"function","function":{"name":"get_weather","description":"Get the weather",
        "parameters":{"type":"object","properties":{"city":{"type":"string","description":"City name"}},"required":["city"]}}})
}

/// A short agentic-style chat history (system prompt, a tool round trip, a question).
fn chat_messages() -> Value {
    json!([
        {"role":"system","content":"You are a helpful coding assistant. Answer concisely and use tools when needed."},
        {"role":"user","content":"What is the weather in Paris?"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},
        {"role":"tool","tool_call_id":"call_1","content":"{\"temp_c\":18,\"sky\":\"cloudy\"}"},
        {"role":"user","content":"And what should I wear?"}
    ])
}

/// The five throughput/latency scenarios.
pub fn standard() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "chat-compat-json",
            description: "OpenAI chat, non-stream, passthrough to OpenAI-compatible upstream",
            method: Method::POST,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/compat/chat/completions".into()),
            body: bytes(&json!({"model":COMPAT_MODEL,"messages":chat_messages(),"tools":[weather_tool_chat()],"stream":false})),
            stream: false,
            expect: "Hello from the benchmark mock",
            ..Scenario::base()
        },
        Scenario {
            id: "chat-claude-stream",
            description: "OpenAI chat, SSE, translated to a Claude upstream",
            method: Method::POST,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: bytes(&json!({"model":CLAUDE_MODEL,"messages":chat_messages(),"tools":[weather_tool_chat()],"stream":true,"stream_options":{"include_usage":true}})),
            stream: true,
            expect: "@@",
            ..Scenario::base()
        },
        Scenario {
            id: "responses-codex-stream",
            description: "OpenAI Responses, SSE, to a Codex upstream",
            method: Method::POST,
            path: "/v1/responses".into(),
            direct_path: Some("/codex/responses".into()),
            body: bytes(&json!({"model":CODEX_MODEL,"input":"What is the weather in Paris?","instructions":"Be concise.","stream":true})),
            stream: true,
            expect: "@@",
            ..Scenario::base()
        },
        Scenario {
            id: "claude-gemini-json",
            description: "Claude messages, non-stream, translated to a Gemini upstream",
            method: Method::POST,
            path: "/v1/messages".into(),
            direct_path: Some(format!("/gemini/v1beta/models/{GEMINI_MODEL}:generateContent")),
            body: bytes(&json!({"model":GEMINI_MODEL,"max_tokens":1024,"system":"You are a helpful coding assistant.",
                "messages":[{"role":"user","content":"What is the weather in Paris?"}],
                "tools":[{"name":"get_weather","description":"Get the weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]})),
            stream: false,
            expect: "Hello from the benchmark mock",
            ..Scenario::base()
        },
        Scenario {
            id: "models",
            description: "GET /v1/models (answered by the server, no upstream)",
            method: Method::GET,
            path: "/v1/models".into(),
            direct_path: None,
            body: Bytes::new(),
            stream: false,
            expect: "\"data\"",
            ..Scenario::base()
        },
    ]
}

/// The two ~2 MB agentic conversations (non-stream).
pub fn large(target_bytes: usize) -> Vec<Scenario> {
    vec![
        Scenario {
            quick_requests: 40,
            quick_conc: 8,
            id: "large-chat-claude",
            description: "2 MB OpenAI chat conversation translated to a Claude upstream",
            method: Method::POST,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: large_chat(CLAUDE_MODEL, target_bytes, Conv::LARGE),
            stream: false,
            expect: "\"choices\"",
            kind: "large",
            ..Scenario::base()
        },
        Scenario {
            quick_requests: 40,
            quick_conc: 8,
            id: "large-claude-gemini",
            description: "2 MB Claude conversation translated to a Gemini upstream",
            method: Method::POST,
            path: "/v1/messages".into(),
            direct_path: Some(format!("/gemini/v1beta/models/{GEMINI_MODEL}:generateContent")),
            body: large_claude(GEMINI_MODEL, target_bytes, Conv::LARGE),
            stream: false,
            expect: "Hello from the benchmark mock",
            kind: "large",
            ..Scenario::base()
        },
    ]
}

/// Every scenario `quick` can run.
pub fn all(large_bytes: usize) -> Vec<Scenario> {
    let mut v = standard();
    v.extend(large(large_bytes));
    v.extend(extras());
    v
}

const GEMINI_STREAM_PATH: &str = "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse";
const AGENT_BYTES: usize = 250_000;

fn gemini_request() -> Bytes {
    bytes(&json!({"systemInstruction":{"parts":[{"text":"You are a helpful coding assistant."}]},
        "contents":[{"role":"user","parts":[{"text":"What is the weather in Paris?"}]}],
        "tools":[{"functionDeclarations":[{"name":"get_weather","description":"Get the weather",
            "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]}]}))
}

/// What users actually do beyond the five basics: long and native SSE streams, Gemini SSE, the
/// Responses websocket, Claude Code sized tool-heavy requests and slow upstreams with many
/// requests in flight.
pub fn extras() -> Vec<Scenario> {
    let long = |chunks: usize| Shape { chunks, ..Shape::FAST };
    // LLM-like upstream: 200 ms to 2 s before the first byte, then 40 deltas 20 ms apart.
    let slow = Shape { first_ms: 200, first_max_ms: 2000, gap_us: 20_000, chunks: 40 };
    let slow_json = Shape { first_ms: 200, first_max_ms: 2000, ..Shape::FAST };
    let chat = |model: &str, stream: bool| bytes(&json!({"model":model,"messages":chat_messages(),"tools":[weather_tool_chat()],"stream":stream}));
    let claude_native = |stream: bool| bytes(&json!({"model":CLAUDE_MODEL,"max_tokens":1024,"stream":stream,"system":"You are a helpful coding assistant.",
        "messages":[{"role":"user","content":"What is the weather in Paris?"}],
        "tools":[{"name":"get_weather","description":"Get the weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]}));
    vec![
        Scenario {
            id: "chat-claude-stream-long",
            description: "OpenAI chat, SSE with 1000 small deltas, translated to a Claude upstream",
            shape: long(1000),
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: bytes(&json!({"model":CLAUDE_MODEL,"messages":chat_messages(),"tools":[weather_tool_chat()],"stream":true,"stream_options":{"include_usage":true}})),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[1, 16, 64],
            quick_requests: 300,
            quick_conc: 16,
            ..Scenario::base()
        },
        Scenario {
            id: "chat-compat-stream-long",
            description: "OpenAI chat, SSE with 2000 small deltas, passthrough to an OpenAI-compatible upstream",
            shape: long(2000),
            path: "/v1/chat/completions".into(),
            direct_path: Some("/compat/chat/completions".into()),
            body: chat(COMPAT_MODEL, true),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[1, 16, 64],
            quick_requests: 300,
            ..Scenario::base()
        },
        Scenario {
            id: "claude-native-stream",
            description: "Claude messages, SSE with 100 deltas, native Claude upstream (passthrough-ish)",
            shape: long(100),
            path: "/v1/messages".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: claude_native(true),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[16, 64, 256],
            quick_requests: 3000,
            ..Scenario::base()
        },
        Scenario {
            id: "gemini-native-stream",
            description: "Gemini streamGenerateContent (alt=sse), 200 deltas, native Gemini upstream",
            shape: long(200),
            path: GEMINI_STREAM_PATH.into(),
            direct_path: Some(format!("/gemini{GEMINI_STREAM_PATH}")),
            body: gemini_request(),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[16, 64, 256],
            quick_requests: 2000,
            ..Scenario::base()
        },
        Scenario {
            id: "chat-gemini-stream",
            description: "OpenAI chat, SSE with 200 deltas, translated to a Gemini upstream",
            shape: long(200),
            path: "/v1/chat/completions".into(),
            direct_path: Some(format!("/gemini{GEMINI_STREAM_PATH}")),
            body: chat(GEMINI_MODEL, true),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[16, 64],
            quick_requests: 2000,
            ..Scenario::base()
        },
        Scenario {
            id: "responses-ws-codex",
            description: "OpenAI Responses over websocket (response.create per request, 100 deltas), Codex upstream",
            shape: long(100),
            path: "/v1/responses".into(),
            direct_path: None,
            body: bytes(&json!({"type":"response.create","model":CODEX_MODEL,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"What is the weather in Paris?"}]}],"instructions":"Be concise."})),
            expect: "response.completed",
            kind: "extra",
            ws: true,
            conc: &[16, 64],
            quick_requests: 2000,
            ..Scenario::base()
        },
        Scenario {
            id: "agent-chat-claude-stream",
            description: "250 KB tool-heavy OpenAI chat request (40 tools), SSE, translated to a Claude upstream",
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: large_chat(CLAUDE_MODEL, AGENT_BYTES, Conv::AGENT),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[1, 16, 64],
            quick_requests: 300,
            quick_conc: 16,
            ..Scenario::base()
        },
        Scenario {
            id: "agent-claude-native-stream",
            description: "250 KB tool-heavy Claude messages request (40 tools), SSE, native Claude upstream",
            path: "/v1/messages".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: large_claude(CLAUDE_MODEL, AGENT_BYTES, Conv::AGENT),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[1, 16, 64],
            quick_requests: 300,
            quick_conc: 16,
            ..Scenario::base()
        },
        Scenario {
            id: "agent-responses-codex-stream",
            description: "250 KB tool-heavy OpenAI Responses request (40 tools), SSE, Codex upstream",
            path: "/v1/responses".into(),
            direct_path: Some("/codex/responses".into()),
            body: large_responses(CODEX_MODEL, AGENT_BYTES, Conv::AGENT),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[1, 16, 64],
            quick_requests: 300,
            quick_conc: 16,
            ..Scenario::base()
        },
        Scenario {
            id: "slow-chat-compat-json",
            description: "OpenAI chat non-stream to a slow upstream (200 ms to 2 s), many requests in flight",
            shape: slow_json,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/compat/chat/completions".into()),
            body: chat(COMPAT_MODEL, false),
            expect: "Hello from the benchmark mock",
            kind: "extra",
            conc: &[256, 1024],
            measure_s: 10.0,
            quick_conc: 1024,
            quick_waves: 2,
            quick_requests: 0,
            ..Scenario::base()
        },
        Scenario {
            id: "slow-chat-claude-stream",
            description: "OpenAI chat SSE to a slow Claude upstream (200 ms to 2 s, 40 deltas 20 ms apart), many in flight",
            shape: slow,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: bytes(&json!({"model":CLAUDE_MODEL,"messages":chat_messages(),"tools":[weather_tool_chat()],"stream":true,"stream_options":{"include_usage":true}})),
            stream: true,
            expect: "@@",
            kind: "extra",
            conc: &[256, 1024],
            measure_s: 10.0,
            quick_conc: 1024,
            quick_waves: 2,
            quick_requests: 0,
            ..Scenario::base()
        },
    ]
}

/// Deterministic xorshift so every run (and both servers) sees the same bytes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "fn", "let", "mut", "impl", "struct", "pub", "match", "return", "self", "Vec<String>", "Option<&str>", "async", "await", "\"quoted\"",
    "\\n", "{", "}", "(", ")", "=>", "->", "::", "//", "TODO:", "error", "request", "response", "stream", "token", "model", "config",
    "naïve", "日本語", "→", "\t", "x", "y", "42", "0xff", "/usr/lib", "src/main.rs", "cargo", "test", "assert_eq!",
];

/// Source-code-like text with quotes, tabs, newlines and some non-ASCII, `n` bytes long or a little more.
fn blob(rng: &mut Rng, n: usize) -> String {
    let mut s = String::with_capacity(n + 64);
    while s.len() < n {
        for _ in 0..(4 + rng.next() % 10) {
            s.push_str(rng.pick(WORDS));
            s.push(' ');
        }
        s.push('\n');
    }
    s
}

/// Size and stream mode of a generated conversation.
#[derive(Clone, Copy)]
struct Conv {
    tools: usize,
    /// Tool results are `result_min + rand % result_span` bytes.
    result_min: usize,
    result_span: u64,
    stream: bool,
}

impl Conv {
    /// The 2 MB non-stream conversation of the large-request test (24 tools, 8 to 32 KB results).
    const LARGE: Conv = Conv { tools: 24, result_min: 8_000, result_span: 24_000, stream: false };
    /// A Claude Code style agent request: 40 tools, about 250 KB, many small tool results, streamed.
    const AGENT: Conv = Conv { tools: 40, result_min: 1_000, result_span: 5_000, stream: true };
}

fn tool_names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("tool_{i}")).collect()
}

fn tool_schema(rng: &mut Rng) -> Value {
    let props: serde_json::Map<String, Value> = (0..6)
        .map(|i| (format!("arg_{i}"), json!({"type":"string","description":blob(rng, 120)})))
        .collect();
    json!({"type":"object","properties":props,"required":["arg_0"]})
}

/// OpenAI chat history: system prompt, tools, then tool-call round trips with big tool outputs.
fn large_chat(model: &str, target: usize, c: Conv) -> Bytes {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let tools: Vec<Value> = tool_names(c.tools)
        .into_iter()
        .map(|n| json!({"type":"function","function":{"name":n,"description":blob(&mut rng, 600),"parameters":tool_schema(&mut rng)}}))
        .collect();
    let mut messages = vec![json!({"role":"system","content":blob(&mut rng, 6000)})];
    let mut size = 0;
    let mut i = 0;
    while size < target {
        let len = c.result_min + (rng.next() % c.result_span) as usize;
        let out = blob(&mut rng, len);
        size += out.len();
        let args = json!({"arg_0": format!("src/file_{i}.rs"), "arg_1": blob(&mut rng, 80)}).to_string();
        messages.push(json!({"role":"user","content":blob(&mut rng, 300)}));
        messages.push(json!({"role":"assistant","content":blob(&mut rng, 200),"tool_calls":[
            {"id":format!("call_{i}"),"type":"function","function":{"name":format!("tool_{}", i % c.tools),"arguments":args}}]}));
        messages.push(json!({"role":"tool","tool_call_id":format!("call_{i}"),"content":out}));
        i += 1;
    }
    messages.push(json!({"role":"user","content":"Summarize what you found."}));
    bytes(&json!({"model":model,"messages":messages,"tools":tools,"stream":c.stream}))
}

/// Claude messages history with tool_use / tool_result blocks.
fn large_claude(model: &str, target: usize, c: Conv) -> Bytes {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let tools: Vec<Value> = tool_names(c.tools)
        .into_iter()
        .map(|n| json!({"name":n,"description":blob(&mut rng, 600),"input_schema":tool_schema(&mut rng)}))
        .collect();
    let system = json!([{"type":"text","text":blob(&mut rng, 6000),"cache_control":{"type":"ephemeral"}}]);
    let mut messages = vec![];
    let mut size = 0;
    let mut i = 0;
    while size < target {
        let len = c.result_min + (rng.next() % c.result_span) as usize;
        let out = blob(&mut rng, len);
        size += out.len();
        messages.push(json!({"role":"user","content":blob(&mut rng, 300)}));
        messages.push(json!({"role":"assistant","content":[
            {"type":"text","text":blob(&mut rng, 200)},
            {"type":"tool_use","id":format!("toolu_{i}"),"name":format!("tool_{}", i % c.tools),"input":{"arg_0":format!("src/file_{i}.rs"),"arg_1":blob(&mut rng, 80)}}]}));
        messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":format!("toolu_{i}"),"content":out}]}));
        i += 1;
    }
    messages.push(json!({"role":"user","content":"Summarize what you found."}));
    let mut body = json!({"model":model,"max_tokens":4096,"system":system,"messages":messages,"tools":tools});
    if c.stream {
        body["stream"] = Value::Bool(true);
    }
    bytes(&body)
}

/// OpenAI Responses history: function_call / function_call_output items.
fn large_responses(model: &str, target: usize, c: Conv) -> Bytes {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let tools: Vec<Value> = tool_names(c.tools)
        .into_iter()
        .map(|n| json!({"type":"function","name":n,"description":blob(&mut rng, 600),"parameters":tool_schema(&mut rng)}))
        .collect();
    let mut input = vec![];
    let mut size = 0;
    let mut i = 0;
    while size < target {
        let len = c.result_min + (rng.next() % c.result_span) as usize;
        let out = blob(&mut rng, len);
        size += out.len();
        let args = json!({"arg_0": format!("src/file_{i}.rs"), "arg_1": blob(&mut rng, 80)}).to_string();
        input.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":blob(&mut rng, 300)}]}));
        input.push(json!({"type":"function_call","call_id":format!("call_{i}"),"name":format!("tool_{}", i % c.tools),"arguments":args}));
        input.push(json!({"type":"function_call_output","call_id":format!("call_{i}"),"output":out}));
        i += 1;
    }
    input.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize what you found."}]}));
    bytes(&json!({"model":model,"instructions":blob(&mut rng, 6000),"input":input,"tools":tools,"stream":c.stream}))
}
