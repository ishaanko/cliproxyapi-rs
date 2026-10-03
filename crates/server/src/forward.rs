//! Stream forwarding and non-stream keepalive (Go: stream_forwarder.go, handlers.go
//! `StartNonStreamingKeepAlive`, handlers_errors.go `WriteErrorResponse`).

use std::future::Future;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at};

use crate::error::{ErrorMessage, claude_error_body, error_body, retry_after_seconds};
use crate::exec::ExecStream;
use crate::headers::{filter_upstream_headers, replace_headers};
use crate::reply::{JSON, Reply, set_sse_headers, streaming_response};

/// Dialect-specific writers for `ForwardStream` (Go: `StreamForwardOptions`). Every `write_*`
/// appends to `out`; the forwarder flushes after each step.
pub trait StreamHooks: Send {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]);

    /// A terminal failure detected while writing the last chunk (not written again).
    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        None
    }

    /// Rewrites an upstream error before it is written.
    fn normalize_terminal_error(&mut self, err: ErrorMessage) -> ErrorMessage {
        err
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage);

    /// Validates a clean upstream close before `write_done`; an error is written as terminal.
    fn close_error(&mut self, _out: &mut Vec<u8>) -> Option<ErrorMessage> {
        None
    }

    fn write_done(&mut self, _out: &mut Vec<u8>) {}

    fn write_keepalive(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b": keep-alive\n\n");
    }
}

/// Stop coalescing queued chunks into one write once this many bytes are pending.
const BATCH_LIMIT: usize = 32 * 1024;

async fn flush(tx: &mpsc::Sender<Bytes>, buf: &mut Vec<u8>) -> bool {
    if buf.is_empty() {
        return true;
    }
    tx.send(Bytes::from(std::mem::take(buf))).await.is_ok()
}

/// `ForwardStream`: pumps chunks to the client until the stream ends, errors, the client goes
/// away, or a chunk reports a terminal failure. Returns the terminal error, if any.
pub async fn forward_stream<H: StreamHooks>(
    hooks: &mut H,
    mut rx: mpsc::Receiver<Result<Bytes, ErrorMessage>>,
    tx: mpsc::Sender<Bytes>,
    keepalive: Duration,
) -> Option<ErrorMessage> {
    let mut ticker = (!keepalive.is_zero()).then(|| interval_at(Instant::now() + keepalive, keepalive));
    let mut buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            _ = tx.closed() => return None,
            item = rx.recv() => {
                // Items that are already queued are written into the same flush (no added
                // latency, fewer wakeups and write syscalls); order and terminal handling are
                // those of one flush per item.
                let mut item = item;
                loop {
                    match item {
                        Some(Ok(chunk)) => {
                            hooks.write_chunk(&mut buf, &chunk);
                            if let Some(err) = hooks.chunk_error() {
                                if !flush(&tx, &mut buf).await {
                                    return None;
                                }
                                return Some(hooks.normalize_terminal_error(err));
                            }
                        }
                        Some(Err(err)) => {
                            let err = hooks.normalize_terminal_error(err);
                            hooks.write_terminal_error(&mut buf, &err);
                            let _ = flush(&tx, &mut buf).await;
                            return Some(err);
                        }
                        None => {
                            if let Some(err) = hooks.close_error(&mut buf) {
                                hooks.write_terminal_error(&mut buf, &err);
                                let _ = flush(&tx, &mut buf).await;
                                return Some(err);
                            }
                            hooks.write_done(&mut buf);
                            let _ = flush(&tx, &mut buf).await;
                            return None;
                        }
                    }
                    if buf.len() >= BATCH_LIMIT {
                        break;
                    }
                    item = match rx.try_recv() {
                        Ok(next) => Some(next),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => None,
                    };
                }
                if !flush(&tx, &mut buf).await {
                    return None;
                }
            },
            _ = async { ticker.as_mut().expect("guarded by the branch condition").tick().await }, if ticker.is_some() => {
                hooks.write_keepalive(&mut buf);
                if !flush(&tx, &mut buf).await {
                    return None;
                }
            }
        }
    }
}

/// Starts an SSE response: headers (status 200), `initial` bytes first, then the forwarder task
/// takes over `rx`. `upstream_headers` are added only where the handler set none (Content-Type
/// wins).
pub fn start_sse_stream<H: StreamHooks + 'static>(
    mut headers: HeaderMap,
    upstream_headers: &HeaderMap,
    initial: Vec<u8>,
    rx: mpsc::Receiver<Result<Bytes, ErrorMessage>>,
    mut hooks: H,
    keepalive: Duration,
    set_sse: bool,
) -> Response {
    if set_sse {
        set_sse_headers(&mut headers);
    }
    crate::headers::write_upstream_headers(&mut headers, upstream_headers);
    crate::sniff::ensure_content_type(&mut headers, &initial);
    let (tx, out_rx) = mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        if !initial.is_empty() && tx.send(Bytes::from(initial)).await.is_err() {
            return;
        }
        let outcome = forward_stream(&mut hooks, rx, tx, keepalive).await;
        if let Some(err) = outcome {
            tracing::debug!(status = err.status_or_500(), "stream terminated with error: {}", err.text);
        }
    });
    streaming_response(200, headers, out_rx)
}

/// Headers + footer for an upstream stream that closed without data (status 200).
pub fn empty_stream_reply(upstream_headers: &HeaderMap, footer: &'static str, set_sse: bool) -> Reply {
    let mut headers = HeaderMap::new();
    if set_sse {
        set_sse_headers(&mut headers);
    }
    crate::headers::write_upstream_headers(&mut headers, upstream_headers);
    Reply {
        status: 200,
        headers,
        body: Bytes::from_static(footer.as_bytes()),
    }
}

/// `StartNonStreamingKeepAlive` + final write: runs `fut`; if it takes longer than `interval`,
/// the response is committed as 200 with `application/json` and a `\n` is emitted every
/// interval, followed by the final body (status and headers of the final reply can no longer
/// be changed, exactly like Go).
pub async fn with_nonstream_keepalive<F>(interval: Duration, fut: F) -> Response
where
    F: Future<Output = Reply> + Send + 'static,
{
    if interval.is_zero() {
        return fut.await.into_response();
    }
    let mut fut = Box::pin(fut);
    let mut ticker = interval_at(Instant::now() + interval, interval);
    tokio::select! {
        reply = &mut fut => return reply.into_response(),
        _ = ticker.tick() => {}
    }
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    let _ = tx.try_send(Bytes::from_static(b"\n"));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                reply = &mut fut => {
                    let _ = tx.send(reply.body).await;
                    return;
                }
                _ = ticker.tick() => {
                    if tx.send(Bytes::from_static(b"\n")).await.is_err() {
                        return;
                    }
                }
                _ = tx.closed() => return,
            }
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    streaming_response(200, headers, rx)
}

/// Retry-After and (with passthrough) upstream error headers shared by the error writers.
fn error_headers(msg: &ErrorMessage, passthrough: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(secs) = msg.retry_after.and_then(retry_after_seconds)
        && let Ok(v) = HeaderValue::from_str(&secs.to_string())
    {
        headers.append(header::RETRY_AFTER, v);
    }
    if passthrough {
        replace_headers(&mut headers, &msg.addon);
    }
    headers
}

/// `writeDirectErrorResponse`: the plugin's own body, with its headers minus hop-by-hop and
/// CPA-reserved ones; JSON content type when none was given.
pub fn direct_error_reply(status: u16, direct: &crate::error::DirectResponse) -> Reply {
    let mut headers = HeaderMap::new();
    for (name, value) in &filter_upstream_headers(&direct.headers) {
        if crate::headers::is_cpa_reserved_response_header(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    }
    Reply { status, headers, body: direct.body.clone() }
}

/// `BaseAPIHandler.WriteErrorResponse`: OpenAI-shaped error reply.
pub fn openai_error_reply(msg: &ErrorMessage, passthrough: bool) -> Reply {
    if let Some(direct) = &msg.direct {
        return direct_error_reply(msg.status_or_500(), direct);
    }
    let mut headers = error_headers(msg, passthrough);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    Reply {
        status: msg.status_or_500(),
        headers,
        body: Bytes::from(error_body(msg)),
    }
}

/// `ClaudeCodeAPIHandler.WriteErrorResponse`: Anthropic-shaped error reply.
pub fn claude_error_reply(msg: &ErrorMessage, passthrough: bool) -> Reply {
    if let Some(direct) = &msg.direct {
        return direct_error_reply(msg.status_or_500(), direct);
    }
    let mut headers = error_headers(msg, passthrough);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    Reply {
        status: msg.status_or_500(),
        headers,
        body: Bytes::from(claude_error_body(msg)),
    }
}

/// Splits the first item off a stream for the "peek before committing headers" logic.
pub enum First {
    Error(ErrorMessage),
    Closed,
    Chunk(Bytes),
}

pub async fn peek_first(stream: &mut ExecStream) -> First {
    match stream.rx.recv().await {
        Some(Err(e)) => First::Error(e),
        None => First::Closed,
        Some(Ok(chunk)) => First::Chunk(chunk),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;
    impl StreamHooks for Plain {
        fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\n\n");
        }
        fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
            out.extend_from_slice(format!("ERR {}\n\n", err.status_or_500()).as_bytes());
        }
        fn write_done(&mut self, out: &mut Vec<u8>) {
            out.extend_from_slice(b"data: [DONE]\n\n");
        }
    }

    async fn run(items: Vec<Result<Bytes, ErrorMessage>>) -> String {
        let (src_tx, src_rx) = mpsc::channel(8);
        for i in items {
            src_tx.send(i).await.unwrap();
        }
        drop(src_tx);
        let (tx, mut rx) = mpsc::channel(16);
        let mut hooks = Plain;
        forward_stream(&mut hooks, src_rx, tx, Duration::ZERO).await;
        let mut out = String::new();
        while let Some(b) = rx.recv().await {
            out.push_str(std::str::from_utf8(&b).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn clean_close_writes_done() {
        let out = run(vec![Ok(Bytes::from_static(b"{\"a\":1}"))]).await;
        assert_eq!(out, "data: {\"a\":1}\n\ndata: [DONE]\n\n");
    }

    #[tokio::test]
    async fn terminal_error_is_written_without_done() {
        let out = run(vec![Ok(Bytes::from_static(b"x")), Err(ErrorMessage::new(502, "boom"))]).await;
        assert_eq!(out, "data: x\n\nERR 502\n\n");
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_comment_is_emitted_while_idle() {
        let (src_tx, src_rx) = mpsc::channel::<Result<Bytes, ErrorMessage>>(1);
        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(async move {
            let mut hooks = Plain;
            forward_stream(&mut hooks, src_rx, tx, Duration::from_secs(5)).await;
        });
        tokio::time::sleep(Duration::from_secs(11)).await;
        drop(src_tx);
        handle.await.unwrap();
        let mut out = String::new();
        while let Some(b) = rx.recv().await {
            out.push_str(std::str::from_utf8(&b).unwrap());
        }
        assert_eq!(out, ": keep-alive\n\n: keep-alive\n\ndata: [DONE]\n\n");
    }

    #[tokio::test(start_paused = true)]
    async fn nonstream_keepalive_commits_200_and_appends_body() {
        let reply = with_nonstream_keepalive(Duration::from_secs(5), async {
            tokio::time::sleep(Duration::from_secs(12)).await;
            Reply::new(500).with_body("{\"error\":1}")
        })
        .await;
        assert_eq!(reply.status(), 200);
        let body = axum::body::to_bytes(reply.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"\n\n{\"error\":1}");
    }

    #[tokio::test]
    async fn fast_nonstream_reply_is_untouched() {
        let reply = with_nonstream_keepalive(Duration::from_secs(5), async { Reply::new(418).with_body("x") }).await;
        assert_eq!(reply.status(), 418);
    }
}
