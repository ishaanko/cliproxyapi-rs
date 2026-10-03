//! Minimal fast mock upstream. No logging, no body parsing: replies are rendered from small
//! templates, so the mock stays far from being the bottleneck.
//!
//! Routes mirror what the servers call with `base-url = http://mock/<family>`:
//! `/anthropic/v1/messages`, `/compat/chat/completions`, `/codex/responses` (always SSE) and
//! `/gemini/v1beta/models/<m>:generateContent`. Streaming is chosen by a `"stream":true` scan of
//! the request body. `/__ctl?first_ms=&gap_us=&chunks=` retunes the stream shape at runtime.
//!
//! Every text delta carries `@@<unix micros>` so a client can compute per-chunk latency.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use memchr::memmem;
use tokio::net::TcpListener;

type Body = BoxBody<Bytes, Infallible>;

/// Stream shape, adjustable through `/__ctl`.
pub struct Shape {
    /// Delay before the response starts (upstream think time).
    first_ms: AtomicU64,
    /// Pause between SSE events.
    gap_us: AtomicU64,
    /// Number of text deltas per stream.
    chunks: AtomicUsize,
}

pub async fn serve(port: u16) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await.with_context(|| format!("bind mock on {addr}"))?;
    let shape = Arc::new(Shape { first_ms: AtomicU64::new(0), gap_us: AtomicU64::new(0), chunks: AtomicUsize::new(20) });
    loop {
        let (sock, _) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let shape = shape.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(shape.clone(), req));
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(sock), svc).await;
        });
    }
}

fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// Reads the request body to the end and reports whether it contains `"stream":true`.
async fn drain(mut body: Incoming) -> bool {
    let compact = memmem::Finder::new(br#""stream":true"#);
    let spaced = memmem::Finder::new(br#""stream": true"#);
    let hit = |hay: &[u8]| compact.find(hay).is_some() || spaced.find(hay).is_some();
    let mut found = false;
    // Last bytes of the previous frame, so a match split across frames is still seen.
    let mut tail: Vec<u8> = Vec::new();
    while let Some(Ok(frame)) = body.frame().await {
        let Some(data) = frame.data_ref() else { continue };
        if found {
            continue;
        }
        tail.extend_from_slice(&data[..data.len().min(16)]);
        found = hit(data) || hit(&tail);
        tail.clear();
        tail.extend_from_slice(&data[data.len().saturating_sub(16)..]);
    }
    found
}

fn full(status: StatusCode, content_type: &'static str, body: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from_static(body.as_bytes())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

#[derive(Clone, Copy)]
enum Dialect {
    Anthropic,
    Compat,
    Codex,
}

async fn handle(shape: Arc<Shape>, req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_string();
    if path == "/__ctl" {
        for pair in req.uri().query().unwrap_or_default().split('&') {
            let Some((k, v)) = pair.split_once('=') else { continue };
            let Ok(v) = v.parse::<u64>() else { continue };
            match k {
                "first_ms" => shape.first_ms.store(v, Ordering::Relaxed),
                "gap_us" => shape.gap_us.store(v, Ordering::Relaxed),
                "chunks" => shape.chunks.store(v as usize, Ordering::Relaxed),
                _ => {}
            }
        }
        return Ok(full(StatusCode::OK, "text/plain", "ok"));
    }
    let wants_stream = drain(req.into_body()).await;
    let first_ms = shape.first_ms.load(Ordering::Relaxed);
    if first_ms > 0 {
        tokio::time::sleep(Duration::from_millis(first_ms)).await;
    }
    let (gap, n) = (Duration::from_micros(shape.gap_us.load(Ordering::Relaxed)), shape.chunks.load(Ordering::Relaxed));
    Ok(match path.as_str() {
        "/anthropic/v1/messages" if wants_stream => sse(Dialect::Anthropic, n, gap),
        "/anthropic/v1/messages" => full(StatusCode::OK, "application/json", ANTHROPIC_JSON),
        "/compat/chat/completions" if wants_stream => sse(Dialect::Compat, n, gap),
        "/compat/chat/completions" => full(StatusCode::OK, "application/json", COMPAT_JSON),
        "/codex/responses" => sse(Dialect::Codex, n, gap),
        p if p.starts_with("/gemini/v1beta/models/") && p.ends_with(":generateContent") => {
            full(StatusCode::OK, "application/json", GEMINI_JSON)
        }
        _ => full(StatusCode::NOT_FOUND, "application/json", r#"{"error":"mock: unknown route"}"#),
    })
}

fn sse(dialect: Dialect, n: usize, gap: Duration) -> Response<Body> {
    let total = match dialect {
        Dialect::Anthropic => n + 5,
        Dialect::Compat => n + 4,
        Dialect::Codex => n + 8,
    };
    // Events are rendered on demand so each delta carries the time it was sent.
    let events = stream::unfold(0usize, move |i| async move {
        if i >= total {
            return None;
        }
        if i > 0 && !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
        let frame = Frame::data(event(dialect, i, n, now_us()));
        Some((Ok::<_, Infallible>(frame), i + 1))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(StreamBody::new(events).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// The `i`th SSE event of a stream with `n` text deltas.
fn event(d: Dialect, i: usize, n: usize, ts: u64) -> Bytes {
    let s = match d {
        Dialect::Anthropic => match i {
            0 => r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_bench","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":1}}}

"#
            .to_string(),
            1 => "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string(),
            k if k < n + 2 => format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"@@{ts:016} token \"}}}}\n\n"
            ),
            k if k == n + 2 => "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_string(),
            k if k == n + 3 => "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}\n\n".to_string(),
            _ => "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        },
        Dialect::Compat => {
            let chunk = |delta: &str, finish: &str| {
                format!(
                    "data: {{\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"mock-gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
                )
            };
            match i {
                0 => chunk(r#"{"role":"assistant","content":""}"#, "null"),
                k if k <= n => chunk(&format!("{{\"content\":\"@@{ts:016} token \"}}"), "null"),
                k if k == n + 1 => chunk("{}", "\"stop\""),
                k if k == n + 2 => "data: {\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"mock-gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n".to_string(),
                _ => "data: [DONE]\n\n".to_string(),
            }
        }
        Dialect::Codex => {
            let resp = |status: &str, output: &str| {
                format!("{{\"id\":\"resp_bench\",\"object\":\"response\",\"created_at\":1700000000,\"status\":\"{status}\",\"model\":\"gpt-5.5\",\"output\":{output},\"parallel_tool_calls\":true,\"store\":false}}")
            };
            let done_item = r#"{"id":"msg_bench","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","annotations":[],"text":"Hello from the benchmark mock"}]}"#;
            match i {
                0 => format!("event: response.created\ndata: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{}}}\n\n", resp("in_progress", "[]")),
                1 => format!("event: response.in_progress\ndata: {{\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{}}}\n\n", resp("in_progress", "[]")),
                2 => "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":2,\"output_index\":0,\"item\":{\"id\":\"msg_bench\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n".to_string(),
                3 => "event: response.content_part.added\ndata: {\"type\":\"response.content_part.added\",\"sequence_number\":3,\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"annotations\":[],\"text\":\"\"}}\n\n".to_string(),
                k if k < n + 4 => format!(
                    "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"sequence_number\":{k},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"delta\":\"@@{ts:016} token \"}}\n\n"
                ),
                k if k == n + 4 => format!("event: response.output_text.done\ndata: {{\"type\":\"response.output_text.done\",\"sequence_number\":{k},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"text\":\"Hello from the benchmark mock\"}}\n\n"),
                k if k == n + 5 => format!("event: response.content_part.done\ndata: {{\"type\":\"response.content_part.done\",\"sequence_number\":{k},\"item_id\":\"msg_bench\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"annotations\":[],\"text\":\"Hello from the benchmark mock\"}}}}\n\n"),
                k if k == n + 6 => format!("event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"sequence_number\":{k},\"output_index\":0,\"item\":{done_item}}}\n\n"),
                k => format!(
                    "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":{k},\"response\":{}}}\n\n",
                    resp("completed", &format!("[{done_item}]")).replace("\"store\":false}", "\"store\":false,\"usage\":{\"input_tokens\":11,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":0},\"total_tokens\":18}}")
                ),
            }
        }
    };
    Bytes::from(s)
}

const ANTHROPIC_JSON: &str = r#"{"id":"msg_bench","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"Hello from the benchmark mock"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":7}}"#;
const COMPAT_JSON: &str = r#"{"id":"chatcmpl-bench","object":"chat.completion","created":1700000000,"model":"mock-gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"Hello from the benchmark mock"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#;
const GEMINI_JSON: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello from the benchmark mock"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18},"modelVersion":"gemini-2.5-flash","responseId":"benchresp01"}"#;
