//! Translator micro-benchmark: wall time, allocations and bytes per operation for the hot bench
//! scenarios (small requests, SSE streams, 2 MB requests). Run:
//! `cargo run --release -p cpa-translator --example perf [-- filter]`.
//! Reuses the bench crate's scenario generators so the inputs are the real benchmark bodies.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use cpa_translator::{Ctx, Format, Param, translate_non_stream, translate_request, translate_stream};

#[allow(dead_code)]
#[path = "../../bench/src/scenarios.rs"]
mod scenarios;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator, only adding counters.
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

/// User-space instruction counter for this thread (load-insensitive); 0 without `--features prof`.
#[cfg(feature = "prof")]
struct Insns(Option<perf_event::Counter>);
#[cfg(feature = "prof")]
impl Insns {
    fn new() -> Self {
        let c = perf_event::Builder::new(perf_event::events::Hardware::INSTRUCTIONS).exclude_kernel(true).exclude_hv(true).build().ok();
        if let Some(mut c) = c {
            let _ = c.enable();
            return Insns(Some(c));
        }
        Insns(None)
    }
    fn read(&mut self) -> u64 {
        self.0.as_mut().and_then(|c| c.read().ok()).unwrap_or(0)
    }
}
#[cfg(not(feature = "prof"))]
struct Insns;
#[cfg(not(feature = "prof"))]
impl Insns {
    fn new() -> Self {
        Insns
    }
    fn read(&mut self) -> u64 {
        0
    }
}

/// Runs `f` `iters` times, reports the best of 5 rounds (us/op) plus instructions, allocs and bytes per op.
fn bench(name: &str, filter: &str, iters: u32, mut f: impl FnMut()) {
    if !name.contains(filter) {
        return;
    }
    f();
    let mut ctr = Insns::new();
    let mut best = f64::MAX;
    let mut best_insns = u64::MAX;
    let (mut allocs, mut bytes) = (0, 0);
    for _ in 0..5 {
        let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
        let i0 = ctr.read();
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        let ns = t.elapsed().as_nanos() as f64 / f64::from(iters);
        best_insns = best_insns.min((ctr.read() - i0) / u64::from(iters));
        allocs = (ALLOCS.load(Relaxed) - a0) / u64::from(iters);
        bytes = (BYTES.load(Relaxed) - b0) / u64::from(iters);
        best = best.min(ns);
    }
    println!("{name:<34} {:>11.1} us/op {:>10.1}k insn/op {allocs:>8} allocs/op {bytes:>11} B/op", best / 1000.0, best_insns as f64 / 1000.0);
}

fn lines(events: &[&str]) -> Vec<Vec<u8>> {
    events.iter().flat_map(|e| e.split('\n').map(|l| l.as_bytes().to_vec())).collect()
}

fn claude_stream(n: usize) -> Vec<Vec<u8>> {
    let mut ev = vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bench\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5-20250929\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
    ];
    let delta = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"@@0001700000000000 token \"}}\n";
    ev.extend(std::iter::repeat_n(delta, n));
    ev.extend([
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n",
    ]);
    lines(&ev)
}

fn codex_stream(n: usize) -> Vec<Vec<u8>> {
    let resp = |status: &str, output: &str, usage: &str| {
        format!("{{\"id\":\"resp_bench\",\"object\":\"response\",\"created_at\":1700000000,\"status\":\"{status}\",\"model\":\"gpt-5.5\",\"output\":{output},\"parallel_tool_calls\":true,\"store\":false{usage}}}")
    };
    let done_item = r#"{"id":"msg_bench","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","annotations":[],"text":"Hello from the benchmark mock"}]}"#;
    let usage = ",\"usage\":{\"input_tokens\":11,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":0},\"total_tokens\":18}";
    let mut ev: Vec<String> = vec![
        format!("event: response.created\ndata: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{}}}\n", resp("in_progress", "[]", "")),
        format!("event: response.in_progress\ndata: {{\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{}}}\n", resp("in_progress", "[]", "")),
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":2,\"output_index\":0,\"item\":{\"id\":\"msg_bench\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n".into(),
        "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":3,\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"annotations\":[],\"text\":\"\"}}\n".into(),
    ];
    for k in 0..n {
        ev.push(format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"sequence_number\":{},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"delta\":\"@@0001700000000000 token \"}}\n", k + 4));
    }
    let k = n + 4;
    ev.push(format!("event: response.output_text.done\ndata: {{\"type\":\"response.output_text.done\",\"sequence_number\":{k},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"text\":\"Hello from the benchmark mock\"}}\n"));
    ev.push(format!("event: response.content_part.done\ndata: {{\"type\":\"response.content_part.done\",\"sequence_number\":{},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"annotations\":[],\"text\":\"Hello from the benchmark mock\"}}}}\n", k + 1));
    ev.push(format!("event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"sequence_number\":{},\"output_index\":0,\"item\":{done_item}}}\n", k + 2));
    ev.push(format!("event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":{},\"response\":{}}}\n", k + 3, resp("completed", &format!("[{done_item}]"), usage)));
    let refs: Vec<&str> = ev.iter().map(String::as_str).collect();
    lines(&refs)
}

fn gemini_stream(n: usize) -> Vec<Vec<u8>> {
    let chunk = |text: &str, tail: &str, usage: &str| {
        format!("data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{text}\"}}]}}{tail},\"index\":0}}],{usage}\"modelVersion\":\"gemini-2.5-flash\",\"responseId\":\"benchresp01\"}}\n")
    };
    let mut ev: Vec<String> = (0..n).map(|_| chunk("@@0001700000000000 token ", "", "")).collect();
    ev.push(chunk("", ",\"finishReason\":\"STOP\"", "\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":7,\"totalTokenCount\":18},"));
    let refs: Vec<&str> = ev.iter().map(String::as_str).collect();
    lines(&refs)
}

fn compat_stream(n: usize) -> Vec<Vec<u8>> {
    let chunk = |delta: &str, finish: &str| {
        format!("data: {{\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"mock-gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n")
    };
    let mut ev = vec![chunk("{\"role\":\"assistant\",\"content\":\"\"}", "null")];
    ev.extend((0..n).map(|_| chunk("{\"content\":\"@@0001700000000000 token \"}", "null")));
    ev.push(chunk("{}", "\"stop\""));
    ev.push("data: {\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"mock-gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n".to_string());
    ev.push("data: [DONE]\n".to_string());
    let refs: Vec<&str> = ev.iter().map(String::as_str).collect();
    lines(&refs)
}

const GEMINI_JSON: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello from the benchmark mock"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18},"modelVersion":"gemini-2.5-flash","responseId":"benchresp01"}"#;

/// Starts SIGPROF sampling when built with `--features prof` and `PROF=1`; the returned closure
/// prints the top self and inclusive symbols.
#[cfg(feature = "prof")]
fn profiler() -> Option<impl FnOnce()> {
    use std::collections::{HashMap, HashSet};
    std::env::var_os("PROF")?;
    let guard = pprof::ProfilerGuardBuilder::default().frequency(2000).blocklist(&["libc", "libgcc", "pthread", "vdso"]).build().ok()?;
    Some(move || {
        let Ok(report) = guard.report().build() else { return };
        let (mut selfs, mut incl): (HashMap<String, isize>, HashMap<String, isize>) = Default::default();
        let mut total: isize = 0;
        for (frames, n) in &report.data {
            total += n;
            let names: Vec<String> = frames
                .frames
                .iter()
                .map(|f| f.iter().map(|s| s.name()).next().unwrap_or_default())
                .collect();
            if let Some(top) = names.first() {
                *selfs.entry(top.clone()).or_default() += n;
            }
            for name in names.iter().collect::<HashSet<_>>() {
                *incl.entry(name.clone()).or_default() += n;
            }
        }
        for (title, map) in [("SELF", selfs), ("INCLUSIVE", incl)] {
            let mut v: Vec<_> = map.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            println!("--- {title} (of {total} samples)");
            for (name, n) in v.iter().take(45) {
                println!("{:5.1}% {name}", *n as f64 * 100.0 / total as f64);
            }
        }
    })
}

#[cfg(not(feature = "prof"))]
fn profiler() -> Option<impl FnOnce()> {
    None::<fn()>
}

fn main() {
    let stop_profiler = profiler();
    let filter = std::env::args().nth(1).unwrap_or_default();
    let ctx = Ctx::default();
    let mut std_scn = scenarios::standard();
    std_scn.extend(scenarios::extras());
    let get = |id: &str| std_scn.iter().find(|s| s.id == id).map(|s| s.body.to_vec()).unwrap_or_default();
    let chat_claude = get("chat-claude-stream");
    let resp_codex = get("responses-codex-stream");
    let claude_gemini = get("claude-gemini-json");

    bench("req chat->claude", &filter, 5000, || {
        black_box(translate_request(Format::OpenAI, Format::Claude, "claude-sonnet-4-5-20250929", &chat_claude, true));
    });
    bench("req responses->codex", &filter, 5000, || {
        black_box(translate_request(Format::OpenAIResponse, Format::Codex, "gpt-5.5", &resp_codex, true));
    });
    bench("req claude->gemini", &filter, 5000, || {
        black_box(translate_request(Format::Claude, Format::Gemini, "gemini-2.5-flash", &claude_gemini, false));
    });

    let agent_claude = get("agent-claude-native-stream");
    let gemini_native = get("gemini-native-stream");
    let agent_chat = get("agent-chat-claude-stream");
    let agent_resp = get("agent-responses-codex-stream");
    bench("req claude->claude agent250k", &filter, 200, || {
        black_box(translate_request(Format::Claude, Format::Claude, "claude-sonnet-4-5-20250929", &agent_claude, true));
    });
    bench("req gemini->gemini small", &filter, 3000, || {
        black_box(translate_request(Format::Gemini, Format::Gemini, "gemini-2.5-flash", &gemini_native, true));
    });
    bench("req chat->claude agent250k", &filter, 200, || {
        black_box(translate_request(Format::OpenAI, Format::Claude, "claude-sonnet-4-5-20250929", &agent_chat, true));
    });
    bench("req responses->codex agent250k", &filter, 200, || {
        black_box(translate_request(Format::OpenAIResponse, Format::Codex, "gpt-5.5", &agent_resp, true));
    });

    let translated_chat = translate_request(Format::OpenAI, Format::Claude, "claude-sonnet-4-5-20250929", &chat_claude, true);
    let cs = claude_stream(20);
    bench("stream claude->chat (20 deltas)", &filter, 2000, || {
        let mut p = Param::default();
        for l in &cs {
            black_box(translate_stream(&ctx, Format::Claude, Format::OpenAI, "claude-sonnet-4-5-20250929", &chat_claude, &translated_chat, l, &mut p));
        }
    });
    let translated_codex = translate_request(Format::OpenAIResponse, Format::Codex, "gpt-5.5", &resp_codex, true);
    let xs = codex_stream(20);
    bench("stream codex->responses (20)", &filter, 2000, || {
        let mut p = Param::default();
        for l in &xs {
            black_box(translate_stream(&ctx, Format::Codex, Format::OpenAIResponse, "gpt-5.5", &resp_codex, &translated_codex, l, &mut p));
        }
    });
    let chat_body = get("chat-compat-json");
    let gs = gemini_stream(200);
    bench("stream gemini->chat (200)", &filter, 200, || {
        let mut p = Param::default();
        for l in &gs {
            black_box(translate_stream(&ctx, Format::Gemini, Format::OpenAI, "gemini-2.5-flash", &chat_body, &chat_body, l, &mut p));
        }
    });
    bench("stream gemini->gemini (200)", &filter, 200, || {
        let mut p = Param::default();
        for l in &gs {
            black_box(translate_stream(&ctx, Format::Gemini, Format::Gemini, "gemini-2.5-flash", &gemini_native, &gemini_native, l, &mut p));
        }
    });
    let cps = compat_stream(200);
    bench("stream openai->openai (200)", &filter, 200, || {
        let mut p = Param::default();
        for l in &cps {
            black_box(translate_stream(&ctx, Format::OpenAI, Format::OpenAI, "mock", &chat_body, &chat_body, l, &mut p));
        }
    });
    bench("req openai->openai small", &filter, 3000, || {
        black_box(translate_request(Format::OpenAI, Format::OpenAI, "mock", &chat_body, true));
    });
    bench("req chat->gemini small", &filter, 3000, || {
        black_box(translate_request(Format::OpenAI, Format::Gemini, "gemini-2.5-flash", &chat_body, true));
    });
    let translated_gem = translate_request(Format::Claude, Format::Gemini, "gemini-2.5-flash", &claude_gemini, false);
    bench("resp gemini->claude nonstream", &filter, 5000, || {
        let mut p = Param::default();
        black_box(translate_non_stream(&ctx, Format::Gemini, Format::Claude, "gemini-2.5-flash", &claude_gemini, &translated_gem, GEMINI_JSON.as_bytes(), &mut p));
    });

    for s in scenarios::large(2_000_000) {
        let (client, upstream, model) = match s.id {
            "large-chat-claude" => (Format::OpenAI, Format::Claude, "claude-sonnet-4-5-20250929"),
            _ => (Format::Claude, Format::Gemini, "gemini-2.5-flash"),
        };
        let body = s.body.to_vec();
        bench(&format!("req 2MB {}", s.id), &filter, 20, || {
            black_box(translate_request(client, upstream, model, &body, false));
        });
    }
    if let Some(stop) = stop_profiler {
        stop();
    }
}
