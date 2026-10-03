//! Benchmark scenarios: client request, matching direct-to-mock route, and large conversation
//! generators.

use bytes::Bytes;
use hyper::Method;
use serde_json::{Value, json};

/// Everything the harness needs to drive one scenario.
pub struct Scenario {
    pub id: &'static str,
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
        },
    ]
}

/// The two ~2 MB agentic conversations (non-stream).
pub fn large(target_bytes: usize) -> Vec<Scenario> {
    vec![
        Scenario {
            id: "large-chat-claude",
            description: "2 MB OpenAI chat conversation translated to a Claude upstream",
            method: Method::POST,
            path: "/v1/chat/completions".into(),
            direct_path: Some("/anthropic/v1/messages".into()),
            body: large_chat(CLAUDE_MODEL, target_bytes),
            stream: false,
            expect: "\"choices\"",
        },
        Scenario {
            id: "large-claude-gemini",
            description: "2 MB Claude conversation translated to a Gemini upstream",
            method: Method::POST,
            path: "/v1/messages".into(),
            direct_path: Some(format!("/gemini/v1beta/models/{GEMINI_MODEL}:generateContent")),
            body: large_claude(GEMINI_MODEL, target_bytes),
            stream: false,
            expect: "Hello from the benchmark mock",
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

fn tool_names() -> Vec<String> {
    (0..24).map(|i| format!("tool_{i}")).collect()
}

fn tool_schema(rng: &mut Rng) -> Value {
    let props: serde_json::Map<String, Value> = (0..6)
        .map(|i| (format!("arg_{i}"), json!({"type":"string","description":blob(rng, 120)})))
        .collect();
    json!({"type":"object","properties":props,"required":["arg_0"]})
}

/// OpenAI chat history: system prompt, 24 tools, then tool-call round trips with big tool outputs.
fn large_chat(model: &str, target: usize) -> Bytes {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let tools: Vec<Value> = tool_names()
        .into_iter()
        .map(|n| json!({"type":"function","function":{"name":n,"description":blob(&mut rng, 600),"parameters":tool_schema(&mut rng)}}))
        .collect();
    let mut messages = vec![json!({"role":"system","content":blob(&mut rng, 6000)})];
    let mut size = 0;
    let mut i = 0;
    while size < target {
        let len = 8_000 + (rng.next() % 24_000) as usize;
        let out = blob(&mut rng, len);
        size += out.len();
        let args = json!({"arg_0": format!("src/file_{i}.rs"), "arg_1": blob(&mut rng, 80)}).to_string();
        messages.push(json!({"role":"user","content":blob(&mut rng, 300)}));
        messages.push(json!({"role":"assistant","content":blob(&mut rng, 200),"tool_calls":[
            {"id":format!("call_{i}"),"type":"function","function":{"name":format!("tool_{}", i % 24),"arguments":args}}]}));
        messages.push(json!({"role":"tool","tool_call_id":format!("call_{i}"),"content":out}));
        i += 1;
    }
    messages.push(json!({"role":"user","content":"Summarize what you found."}));
    bytes(&json!({"model":model,"messages":messages,"tools":tools,"stream":false}))
}

/// Claude messages history with tool_use / tool_result blocks.
fn large_claude(model: &str, target: usize) -> Bytes {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let tools: Vec<Value> = tool_names()
        .into_iter()
        .map(|n| json!({"name":n,"description":blob(&mut rng, 600),"input_schema":tool_schema(&mut rng)}))
        .collect();
    let system = json!([{"type":"text","text":blob(&mut rng, 6000),"cache_control":{"type":"ephemeral"}}]);
    let mut messages = vec![];
    let mut size = 0;
    let mut i = 0;
    while size < target {
        let len = 8_000 + (rng.next() % 24_000) as usize;
        let out = blob(&mut rng, len);
        size += out.len();
        messages.push(json!({"role":"user","content":blob(&mut rng, 300)}));
        messages.push(json!({"role":"assistant","content":[
            {"type":"text","text":blob(&mut rng, 200)},
            {"type":"tool_use","id":format!("toolu_{i}"),"name":format!("tool_{}", i % 24),"input":{"arg_0":format!("src/file_{i}.rs"),"arg_1":blob(&mut rng, 80)}}]}));
        messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":format!("toolu_{i}"),"content":out}]}));
        i += 1;
    }
    messages.push(json!({"role":"user","content":"Summarize what you found."}));
    bytes(&json!({"model":model,"max_tokens":4096,"system":system,"messages":messages,"tools":tools}))
}
