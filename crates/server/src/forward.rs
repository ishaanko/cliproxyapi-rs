//! Stream forwarding and non-stream keepalive (Go: stream_forwarder.go, handlers.go
//! `StartNonStreamingKeepAlive`, handlers_errors.go `WriteErrorResponse`).

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use tokio::sync::mpsc;
use http_body::{Body, Frame};
use tokio::time::{Instant, Interval, Sleep, interval_at, sleep_until};

use crate::error::{ErrorMessage, claude_error_body, error_body, retry_after_seconds};
use crate::exec::{ExecRx, ExecStream};
use crate::headers::{filter_upstream_headers, replace_headers};
use crate::reply::{JSON, Reply, set_sse_headers, streaming_response};

/// Dialect-specific writers for `ForwardStream` (Go: `StreamForwardOptions`). Every `write_*`
/// appends to `out`; the forwarder flushes after each step.
pub trait StreamHooks: Send + Unpin {
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

/// Writes to one client are spaced at least this far apart. The first chunk after a quiet period
/// goes out at once (token streams, where events are milliseconds apart, see no change); a dense
/// burst, which would otherwise cost one socket write per event, is flushed once per gap. The tokio
/// timer wheel rounds the wait up to a millisecond tick.
const FLUSH_GAP: Duration = Duration::from_millis(1);

/// SSE response body (Go: `ForwardStream`): pulls chunks from the stream when hyper polls the
/// body, writes them through the dialect hooks and hands over everything that is already queued
/// as one frame (one socket write; see [`FLUSH_GAP`] for dense bursts). It ends after the stream ends, errors, or a
/// chunk reports a terminal failure; dropping it (client gone) drops the chunk source, which
/// releases the upstream. Runs in the connection's own task: no pump task or channel per stream.
pub struct SseBody<H: StreamHooks> {
    hooks: H,
    rx: ExecRx,
    /// Bytes the handler already wrote before committing the headers; sent first.
    initial: Vec<u8>,
    ticker: Option<Pin<Box<Interval>>>,
    buf: Vec<u8>,
    finished: bool,
    /// When the last frame was handed to hyper.
    last_flush: Option<Instant>,
    /// Wait for the end of the flush gap while a dense burst accumulates in `buf`.
    hold: Option<Pin<Box<Sleep>>>,
}

impl<H: StreamHooks> SseBody<H> {
    pub fn new(hooks: H, rx: ExecRx, initial: Vec<u8>, keepalive: Duration) -> Self {
        let ticker = (!keepalive.is_zero()).then(|| Box::pin(interval_at(Instant::now() + keepalive, keepalive)));
        SseBody { hooks, rx, initial, ticker, buf: Vec::new(), finished: false, last_flush: None, hold: None }
    }

    fn terminal(&mut self, err: &ErrorMessage) {
        self.finished = true;
        tracing::debug!(status = err.status_or_500(), "stream terminated with error: {}", err.text);
    }
}

impl<H: StreamHooks> Body for SseBody<H> {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if !this.initial.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut this.initial))))));
        }
        if this.finished {
            return Poll::Ready(None);
        }
        // A due keep-alive rides along with whatever is queued (the tick is independent of traffic).
        if let Some(t) = this.ticker.as_mut()
            && t.as_mut().poll_tick(cx).is_ready()
        {
            this.hooks.write_keepalive(&mut this.buf);
        }
        loop {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if this.buf.is_empty() {
                        // One allocation per flush instead of doubling from zero.
                        this.buf.reserve(chunk.len() + 32);
                    }
                    this.hooks.write_chunk(&mut this.buf, &chunk);
                    if let Some(err) = this.hooks.chunk_error() {
                        let err = this.hooks.normalize_terminal_error(err);
                        this.terminal(&err);
                        break;
                    }
                    if this.buf.len() >= BATCH_LIMIT {
                        break;
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    let err = this.hooks.normalize_terminal_error(err);
                    this.hooks.write_terminal_error(&mut this.buf, &err);
                    this.terminal(&err);
                    break;
                }
                Poll::Ready(None) => {
                    if let Some(err) = this.hooks.close_error(&mut this.buf) {
                        this.hooks.write_terminal_error(&mut this.buf, &err);
                        this.terminal(&err);
                    } else {
                        this.hooks.write_done(&mut this.buf);
                        this.finished = true;
                    }
                    break;
                }
                Poll::Pending => break,
            }
        }
        if !this.buf.is_empty() {
            let due = this.finished || this.buf.len() >= BATCH_LIMIT || this.last_flush.is_none_or(|t| t.elapsed() >= FLUSH_GAP);
            if !due && let Some(last) = this.last_flush {
                let deadline = last + FLUSH_GAP;
                let hold = this.hold.get_or_insert_with(|| Box::pin(sleep_until(deadline)));
                hold.as_mut().reset(deadline);
                if hold.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
            }
            this.last_flush = Some(Instant::now());
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut this.buf))))));
        }
        if this.finished { Poll::Ready(None) } else { Poll::Pending }
    }
}

/// Starts an SSE response: headers (status 200), `initial` bytes first, then the body takes
/// over `rx`. `upstream_headers` are added only where the handler set none (Content-Type wins).
pub fn start_sse_stream<H: StreamHooks + 'static>(
    mut headers: HeaderMap,
    upstream_headers: &HeaderMap,
    initial: Vec<u8>,
    rx: ExecRx,
    hooks: H,
    keepalive: Duration,
    set_sse: bool,
) -> Response {
    if set_sse {
        set_sse_headers(&mut headers);
    }
    crate::headers::write_upstream_headers(&mut headers, upstream_headers);
    crate::sniff::ensure_content_type(&mut headers, &initial);
    let mut resp = Response::new(axum::body::Body::new(SseBody::new(hooks, rx, initial, keepalive)));
    *resp.headers_mut() = headers;
    resp
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

    async fn collect(body: SseBody<Plain>) -> String {
        let bytes = axum::body::to_bytes(axum::body::Body::new(body), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn run(items: Vec<Result<Bytes, ErrorMessage>>) -> String {
        let (src_tx, src_rx) = mpsc::channel(8);
        for i in items {
            src_tx.send(i).await.unwrap();
        }
        drop(src_tx);
        collect(SseBody::new(Plain, src_rx.into(), Vec::new(), Duration::ZERO)).await
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
        let handle = tokio::spawn(collect(SseBody::new(Plain, src_rx.into(), Vec::new(), Duration::from_secs(5))));
        tokio::time::sleep(Duration::from_secs(11)).await;
        drop(src_tx);
        assert_eq!(handle.await.unwrap(), ": keep-alive\n\n: keep-alive\n\ndata: [DONE]\n\n");
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
