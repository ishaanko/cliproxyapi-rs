//! Micro-benchmark for the cpa-json engine on payloads shaped like the proxy's hot paths
//! (SSE events, a 2 MB agentic chat body). Run with
//! `cargo run --release -p cpa-json --example jbench [filter]`; prints ns and allocations per op.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use cpa_json::{J, Value};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator unchanged; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(n as u64, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn bench(name: &str, filter: &str, iters: u32, mut f: impl FnMut()) {
    if !filter.is_empty() && !name.contains(filter) {
        return;
    }
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    // User-space instruction count: unlike wall time it does not move with machine load.
    let mut counter = perf_event::Builder::new(perf_event::events::Hardware::INSTRUCTIONS).build().ok();
    if let Some(c) = counter.as_mut() {
        let _ = c.enable();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / f64::from(iters);
    let instr = counter.as_mut().and_then(|c| {
        let _ = c.disable();
        c.read().ok()
    });
    let allocs = (ALLOCS.load(Relaxed) - a0) as f64 / f64::from(iters);
    let bytes = (BYTES.load(Relaxed) - b0) as f64 / f64::from(iters);
    let instr = instr.map_or(0.0, |i| i as f64 / f64::from(iters));
    println!("{name:<28} {instr:>12.0} instr/op {ns:>10.0} ns/op {allocs:>8.1} allocs/op {bytes:>11.0} B/op");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const WORDS: &[&str] = &[
    "fn", "let", "mut", "impl", "struct", "pub", "match", "return", "self", "Vec<String>", "Option<&str>", "async", "await", "\"quoted\"",
    "\\n", "{", "}", "(", ")", "=>", "->", "::", "//", "TODO:", "error", "request", "response", "stream", "token", "model", "config",
    "naïve", "日本語", "→", "\t", "x", "y", "42", "0xff", "/usr/lib", "src/main.rs", "cargo", "test", "assert_eq!",
];

fn blob(rng: &mut Rng, n: usize) -> String {
    let mut s = String::with_capacity(n + 64);
    while s.len() < n {
        for _ in 0..(4 + rng.next() % 10) {
            s.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
            s.push(' ');
        }
        s.push('\n');
    }
    s
}

/// OpenAI chat history of roughly `target` bytes of tool output (same shape as the bench harness).
fn large_chat(target: usize) -> Vec<u8> {
    use cpa_json::json;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let tools: Vec<Value> = (0..24)
        .map(|i| {
            let mut props = cpa_json::Map::new();
            for k in 0..6 {
                props.insert(format!("arg_{k}"), json!({"type":"string","description":blob(&mut rng, 120)}));
            }
            let desc = blob(&mut rng, 600);
            json!({"type":"function","function":{"name":format!("tool_{i}"),"description":desc,
                "parameters":{"type":"object","properties":props,"required":["arg_0"]}}})
        })
        .collect();
    let mut messages = vec![json!({"role":"system","content":blob(&mut rng, 6000)})];
    let (mut size, mut i) = (0, 0);
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
    json!({"model":"claude-sonnet-4-5","messages":messages,"tools":tools,"stream":false}).to_string().into_bytes()
}

fn main() {
    let filter = std::env::args().nth(1).unwrap_or_default();
    let f = filter.as_str();

    let ev = br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"@@1759470000123456 token "}}"#;
    let start = br#"{"type":"message_start","message":{"id":"msg_bench","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":1}}}"#;
    let chat_req = br#"{"model":"claude-sonnet-4-5-20250929","messages":[{"role":"system","content":"You are a helpful coding assistant. Answer concisely and use tools when needed."},{"role":"user","content":"What is the weather in Paris?"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"{\"temp_c\":18,\"sky\":\"cloudy\"}"},{"role":"user","content":"And what should I wear?"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get the weather","parameters":{"type":"object","properties":{"city":{"type":"string","description":"City name"}},"required":["city"]}}}],"stream":true,"stream_options":{"include_usage":true}}"#;

    bench("sse/parse", f, 200_000, || {
        std::hint::black_box(cpa_json::parse(std::hint::black_box(ev)));
    });
    bench("sse/valid", f, 200_000, || {
        std::hint::black_box(cpa_json::valid(std::hint::black_box(ev)));
    });
    bench("sse/start parse", f, 200_000, || {
        std::hint::black_box(cpa_json::parse(std::hint::black_box(start)));
    });
    let root = cpa_json::parse(ev);
    bench("sse/get x4", f, 500_000, || {
        let r = std::hint::black_box(&root);
        std::hint::black_box((r.g("type").str(), r.g("delta.text").str(), r.g("index").int(), r.g("delta.type").str()));
    });
    bench("sse/translate-like", f, 100_000, || {
        let r = cpa_json::parse(std::hint::black_box(ev));
        let mut out = cpa_json::parse_str(
            r#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}"#,
        );
        cpa_json::set(&mut out, "id", "chatcmpl-1");
        cpa_json::set(&mut out, "created", 1759470000i64);
        cpa_json::set(&mut out, "model", "claude-sonnet-4-5");
        cpa_json::set(&mut out, "choices.0.delta.content", r.g("delta.text").str());
        std::hint::black_box(cpa_json::to_vec(&out));
    });
    bench("req/parse small", f, 100_000, || {
        std::hint::black_box(cpa_json::parse(std::hint::black_box(chat_req)));
    });
    bench("req/parse+to_vec small", f, 100_000, || {
        let v = cpa_json::parse(std::hint::black_box(chat_req));
        std::hint::black_box(cpa_json::to_vec(&v));
    });

    let large = large_chat(2_000_000);
    println!("large body: {} bytes", large.len());
    bench("large/parse", f, 40, || {
        std::hint::black_box(cpa_json::parse(std::hint::black_box(&large)));
    });
    bench("large/valid", f, 40, || {
        std::hint::black_box(cpa_json::valid(std::hint::black_box(&large)));
    });
    for (label, unit) in [("plain", "abcdefghijklmnopqrstuvwxy"), ("esc/25B", "abcdefghijklmnopqrstuvw\\n"), ("esc+utf8", "abcdefghijklmnopq日本語\\n")] {
        let doc = format!("\"{}\"", unit.repeat(2_000_000 / unit.len()));
        bench(&format!("str/valid {label}"), f, 20, || {
            std::hint::black_box(cpa_json::valid(std::hint::black_box(doc.as_bytes())));
        });
        bench(&format!("str/parse {label}"), f, 20, || {
            std::hint::black_box(cpa_json::parse(std::hint::black_box(doc.as_bytes())));
        });
        let v = cpa_json::parse(doc.as_bytes());
        bench(&format!("str/to_vec {label}"), f, 20, || {
            std::hint::black_box(cpa_json::to_vec(std::hint::black_box(&v)));
        });
    }
    bench("raw/from_utf8", f, 100, || {
        std::hint::black_box(std::str::from_utf8(std::hint::black_box(&large)).is_ok());
    });
    bench("raw/memchr_iter backslash", f, 100, || {
        std::hint::black_box(memchr::memchr_iter(b'\\', std::hint::black_box(&large)).count());
    });
    println!("backslashes: {}", memchr::memchr_iter(b'\\', &large).count());
    println!("non-ascii bytes: {}", large.iter().filter(|b| **b > 127).count());
    let lroot = cpa_json::parse(&large);
    bench("large/to_vec", f, 40, || {
        std::hint::black_box(cpa_json::to_vec(std::hint::black_box(&lroot)));
    });
    bench("large/clone", f, 40, || {
        std::hint::black_box(std::hint::black_box(&lroot).clone());
    });
    bench("large/raw_at stream", f, 200, || {
        std::hint::black_box(cpa_json::raw_at(std::hint::black_box(&large), "stream"));
    });
    bench("large/get tools.#", f, 400, || {
        std::hint::black_box(lroot.g("tools.#").int());
    });
    bench("large/get messages.0.role", f, 400, || {
        std::hint::black_box(lroot.g("messages.0.role").str());
    });
}
