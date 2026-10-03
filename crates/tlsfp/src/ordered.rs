//! HTTP/1.1 request header ordering and casing on the wire (Go: `internal/httpwire`
//! `NewOrderedRequestConn`).
//!
//! The native clients emit headers in a fixed order and casing that a generic HTTP client cannot
//! reproduce. [`OrderedConn`] wraps the TLS stream, buffers each request head and rewrites it:
//! listed headers come first in the listed order and casing, unlisted headers follow in their
//! original order and casing, and the request line and body bytes are untouched. An optional
//! trailing header line is appended after the last header (the Chrome profile's `Connection: close`).

use std::borrow::Cow;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAX_BUFFERED_HEADER: usize = 1 << 20;

/// Desired header names (case-insensitive match, emitted with the listed casing) for a request
/// `(method, request_target)`.
pub type HeaderOrder = fn(method: &str, target: &str) -> &'static [&'static str];

/// Stateful request rewriter; feed it everything the HTTP client writes.
pub struct HeaderRewriter {
    order: HeaderOrder,
    /// Header line (without CRLF) appended after the last header of every request head.
    trailing: Option<&'static str>,
    header: Vec<u8>,
    body_remaining: i64,
    chunked: Option<ChunkTracker>,
}

impl HeaderRewriter {
    pub fn new(order: HeaderOrder) -> Self {
        Self { order, trailing: None, header: Vec::new(), body_remaining: 0, chunked: None }
    }

    /// Appends `line` (for example `Connection: close`) after the last header of each request.
    pub fn with_trailing_header(mut self, line: &'static str) -> Self {
        self.trailing = Some(line);
        self
    }

    /// Consumes one client write and returns the bytes to send on the wire (possibly none while a
    /// request head is still incomplete).
    pub fn rewrite(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(input.len());
        // Borrowed until a request head ends mid-write; only then is the tail copied.
        let mut buf: Cow<'_, [u8]> = Cow::Borrowed(input);
        let mut pos = 0usize;
        while pos < buf.len() {
            let remaining = &buf[pos..];
            if self.body_remaining > 0 {
                let n = usize::try_from(self.body_remaining).unwrap_or(usize::MAX).min(remaining.len());
                out.extend_from_slice(&remaining[..n]);
                self.body_remaining -= n as i64;
                pos += n;
                continue;
            }
            if let Some(tracker) = self.chunked.as_mut() {
                let (n, completed) = tracker.consume(remaining)?;
                out.extend_from_slice(&remaining[..n]);
                if completed {
                    self.chunked = None;
                }
                pos += n;
                continue;
            }
            if self.header.is_empty() && !is_http_method_prefix(remaining) {
                out.extend_from_slice(remaining);
                return Ok(out);
            }
            self.header.extend_from_slice(remaining);
            let Some(end) = find(&self.header, b"\r\n\r\n") else {
                if self.header.len() > MAX_BUFFERED_HEADER {
                    return Err(io::Error::other(format!("httpwire: request header exceeds {MAX_BUFFERED_HEADER} bytes")));
                }
                return Ok(out);
            };
            let end = end + 4;
            let head = std::mem::take(&mut self.header);
            let (ordered, content_length, chunked) = order_request_header(&head[..end], self.order, self.trailing);
            out.extend_from_slice(&ordered);
            buf = Cow::Owned(head[end..].to_vec());
            pos = 0;
            if chunked {
                self.chunked = Some(ChunkTracker::default());
            } else {
                self.body_remaining = content_length;
            }
        }
        Ok(out)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// True when `p` can start a request line (`TOKEN SP ...`), so non-HTTP bytes pass through.
fn is_http_method_prefix(p: &[u8]) -> bool {
    if p.is_empty() {
        return false;
    }
    for (i, &b) in p.iter().enumerate() {
        if b == b' ' && i > 0 {
            return true;
        }
        if !is_token_char(b) {
            return false;
        }
    }
    true
}

fn header_line_named(line: &[u8], name: &str) -> bool {
    match line.iter().position(|&b| b == b':') {
        Some(colon) if colon > 0 => line[..colon].eq_ignore_ascii_case(name.as_bytes()),
        _ => false,
    }
}

fn header_value<'a>(lines: &'a [&[u8]], name: &str) -> Option<&'a [u8]> {
    let line = lines.iter().find(|l| header_line_named(l, name))?;
    let colon = line.iter().position(|&b| b == b':')?;
    Some(&line[colon + 1..])
}

fn request_content_length(lines: &[&[u8]]) -> i64 {
    header_value(lines, "Content-Length")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(0)
}

fn request_uses_chunked(lines: &[&[u8]]) -> bool {
    lines
        .iter()
        .filter(|l| header_line_named(l, "Transfer-Encoding"))
        .filter_map(|l| {
            let colon = l.iter().position(|&b| b == b':')?;
            std::str::from_utf8(&l[colon + 1..]).ok()
        })
        .any(|v| v.split(',').any(|e| e.trim().eq_ignore_ascii_case("chunked")))
}

/// Reorders and re-cases one request head (including the final blank line). Returns the new head,
/// the declared content length and whether the body is chunked.
fn order_request_header(header: &[u8], order: HeaderOrder, trailing: Option<&str>) -> (Vec<u8>, i64, bool) {
    let body = &header[..header.len() - 4];
    let lines = split_crlf(body);
    let header_lines = &lines[1..];
    let request_line = String::from_utf8_lossy(lines[0]);
    let mut parts = request_line.splitn(3, ' ');
    // A malformed request line keeps the original header order.
    let desired = match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(_)) => order(method, target),
        _ => &[],
    };
    let mut used = vec![false; header_lines.len()];
    let mut ordered: Vec<Vec<u8>> = vec![lines[0].to_vec()];
    for name in desired {
        for (i, line) in header_lines.iter().enumerate() {
            if used[i] || !header_line_named(line, name) {
                continue;
            }
            match line.iter().position(|&b| b == b':') {
                Some(colon) if colon > 0 && !name.is_empty() => {
                    let mut rewritten = name.as_bytes().to_vec();
                    rewritten.extend_from_slice(&line[colon..]);
                    ordered.push(rewritten);
                }
                _ => ordered.push(line.to_vec()),
            }
            used[i] = true;
        }
    }
    for (i, line) in header_lines.iter().enumerate() {
        if !used[i] {
            ordered.push(line.to_vec());
        }
    }
    if let Some(line) = trailing {
        ordered.push(line.as_bytes().to_vec());
    }
    let mut out = Vec::with_capacity(header.len() + trailing.map_or(0, |l| l.len() + 2));
    for line in ordered {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    (out, request_content_length(header_lines), request_uses_chunked(header_lines))
}

fn split_crlf(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\r' && b[i + 1] == b'\n' {
            out.push(&b[start..i]);
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    out.push(&b[start..]);
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ChunkState {
    #[default]
    Size,
    Data,
    DataCrlf,
    Trailers,
}

/// Follows a chunked request body so the end of the request is known without parsing payloads.
#[derive(Default)]
struct ChunkTracker {
    state: ChunkState,
    line: Vec<u8>,
    data_remaining: i64,
    crlf_position: usize,
    trailers: Vec<u8>,
}

impl ChunkTracker {
    /// Returns bytes consumed and whether the terminating trailer section ended.
    fn consume(&mut self, data: &[u8]) -> io::Result<(usize, bool)> {
        let mut consumed = 0;
        while consumed < data.len() {
            match self.state {
                ChunkState::Size => {
                    self.line.push(data[consumed]);
                    consumed += 1;
                    if self.line.len() > MAX_BUFFERED_HEADER {
                        return Err(io::Error::other(format!("httpwire: chunk size line exceeds {MAX_BUFFERED_HEADER} bytes")));
                    }
                    if !self.line.ends_with(b"\r\n") {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&self.line[..self.line.len() - 2]).trim().to_string();
                    let size_text = text.split(';').next().unwrap_or_default().trim();
                    let size = i64::from_str_radix(size_text, 16)
                        .ok()
                        .filter(|s| *s >= 0)
                        .ok_or_else(|| io::Error::other(format!("httpwire: invalid chunk size {size_text:?}")))?;
                    self.line.clear();
                    if size == 0 {
                        self.state = ChunkState::Trailers;
                        continue;
                    }
                    self.data_remaining = size;
                    self.state = ChunkState::Data;
                }
                ChunkState::Data => {
                    let n = i64::try_from(data.len() - consumed).unwrap_or(i64::MAX).min(self.data_remaining);
                    consumed += usize::try_from(n).unwrap_or(0);
                    self.data_remaining -= n;
                    if self.data_remaining == 0 {
                        self.crlf_position = 0;
                        self.state = ChunkState::DataCrlf;
                    }
                }
                ChunkState::DataCrlf => {
                    if data[consumed] != b"\r\n"[self.crlf_position] {
                        return Err(io::Error::other("httpwire: chunk data is missing CRLF terminator"));
                    }
                    consumed += 1;
                    self.crlf_position += 1;
                    if self.crlf_position == 2 {
                        self.state = ChunkState::Size;
                    }
                }
                ChunkState::Trailers => {
                    self.trailers.push(data[consumed]);
                    consumed += 1;
                    if self.trailers.len() > MAX_BUFFERED_HEADER {
                        return Err(io::Error::other(format!("httpwire: chunk trailers exceed {MAX_BUFFERED_HEADER} bytes")));
                    }
                    if self.trailers == b"\r\n" || self.trailers.ends_with(b"\r\n\r\n") {
                        return Ok((consumed, true));
                    }
                }
            }
        }
        Ok((consumed, false))
    }
}

/// Stream wrapper applying [`HeaderRewriter`] to everything written. Writes are accepted in full
/// and drained to the inner stream on the next write or flush.
pub struct OrderedConn<T> {
    inner: T,
    rewriter: HeaderRewriter,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl<T> OrderedConn<T> {
    pub fn new(inner: T, order: HeaderOrder) -> Self {
        Self::with_rewriter(inner, HeaderRewriter::new(order))
    }

    pub fn with_rewriter(inner: T, rewriter: HeaderRewriter) -> Self {
        Self { inner, rewriter, pending: Vec::new(), pending_pos: 0 }
    }
}

impl<T: AsyncWrite + Unpin> OrderedConn<T> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.pending_pos < self.pending.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending_pos += n;
        }
        self.pending.clear();
        self.pending_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for OrderedConn<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for OrderedConn<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.drain(cx))?;
        let out = self.rewriter.rewrite(buf)?;
        self.pending = out;
        self.pending_pos = 0;
        // Opportunistic drain; whatever stays pending goes out on the next write or flush.
        let _ = self.drain(cx);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.drain(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(_: &str, _: &str) -> &'static [&'static str] {
        &["Accept", "X-Stainless-OS", "anthropic-beta", "Host", "Content-Length"]
    }

    #[test]
    fn reorders_recases_and_passes_body_through() {
        let mut rw = HeaderRewriter::new(order);
        let req = b"POST /v1/messages HTTP/1.1\r\nHost: h\r\nUser-Agent: ua\r\nContent-Length: 5\r\nAnthropic-Beta: b\r\nX-Stainless-Os: Linux\r\nAccept: */*\r\n\r\nhello";
        // Split mid-head to exercise buffering.
        let mut out = rw.rewrite(&req[..40]).unwrap();
        out.extend(rw.rewrite(&req[40..]).unwrap());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "POST /v1/messages HTTP/1.1\r\nAccept: */*\r\nX-Stainless-OS: Linux\r\nanthropic-beta: b\r\nHost: h\r\nContent-Length: 5\r\nUser-Agent: ua\r\n\r\nhello"
        );
    }

    #[test]
    fn appends_trailing_header_after_the_last_header() {
        let mut rw = HeaderRewriter::new(|_, _| &[]).with_trailing_header("Connection: close");
        let req = b"GET / HTTP/1.1\r\nHost: h\r\nConnection: Keep-Alive\r\nAccept-Encoding: gzip\r\n\r\n";
        assert_eq!(
            rw.rewrite(req).unwrap(),
            b"GET / HTTP/1.1\r\nHost: h\r\nConnection: Keep-Alive\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn tracks_chunked_bodies_across_requests() {
        let mut rw = HeaderRewriter::new(order);
        let one = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nAccept: a\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        let out = rw.rewrite(one).unwrap();
        assert!(out.starts_with(b"POST / HTTP/1.1\r\nAccept: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc"));
        // The next request is recognized as a fresh head, not chunk data.
        let two = rw.rewrite(b"GET / HTTP/1.1\r\nAccept: b\r\n\r\n").unwrap();
        assert_eq!(two, b"GET / HTTP/1.1\r\nAccept: b\r\n\r\n");
    }
}
