//! Narrowly scoped HTTP/1.1 wire helpers (Go: internal/httpwire).
//!
//! [`OrderedRequestWriter`] wraps a connection's write half and rewrites the header order and
//! header-name casing of every HTTP/1.1 request that passes through, leaving request lines, other
//! header lines and bodies (content-length and chunked) intact. Bytes that do not start an HTTP
//! request line (for example a SOCKS5 handshake) pass through untouched.

use std::borrow::Cow;
use std::io::{self, Write};

const MAX_BUFFERED_REQUEST_HEADER: usize = 1 << 20;

/// Returns the desired header-name order for one request, given its method and request target.
/// Names compare case-insensitively; unlisted headers keep their relative order after the listed
/// ones. The listed spelling replaces the original casing of a matched header name.
pub type RequestHeaderOrder = Box<dyn Fn(&str, &str) -> Vec<String> + Send + Sync>;

/// Header-reordering writer; see the module docs.
pub struct OrderedRequestWriter<W: Write> {
    inner: W,
    order: RequestHeaderOrder,
    header: Vec<u8>,
    body_remaining: i64,
    chunked: Option<ChunkedRequestTracker>,
}

impl<W: Write> OrderedRequestWriter<W> {
    pub fn new(inner: W, order: RequestHeaderOrder) -> Self {
        OrderedRequestWriter { inner, order, header: Vec::new(), body_remaining: 0, chunked: None }
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Go's `Write`: returns how many caller bytes were accepted and the terminal error, if any.
    /// A header write failure reports the full input as accepted, so callers do not replay an
    /// ambiguous partial header.
    pub fn write_counted(&mut self, p: &[u8]) -> (usize, Option<io::Error>) {
        let original_length = p.len();
        let mut consumed = 0usize;
        let mut data: Cow<[u8]> = Cow::Borrowed(p);
        let mut pos = 0usize;
        while pos < data.len() {
            let remaining = &data[pos..];
            if self.body_remaining > 0 {
                let body_bytes = remaining.len().min(usize::try_from(self.body_remaining).unwrap_or(usize::MAX));
                let (written, err) = write_all(&mut self.inner, &remaining[..body_bytes]);
                consumed += written;
                self.body_remaining -= written as i64;
                if let Some(e) = err {
                    return (consumed, Some(e));
                }
                pos += body_bytes;
                continue;
            }
            if let Some(tracker) = &self.chunked {
                let mut preview = tracker.clone();
                let chunk_bytes = match preview.consume(remaining) {
                    Ok((n, _)) => n,
                    Err(e) => return (consumed, Some(e)),
                };
                let (written, err_write) = write_all(&mut self.inner, &remaining[..chunk_bytes]);
                consumed += written;
                let Some(tracker) = self.chunked.as_mut() else {
                    return (consumed, err_write);
                };
                match tracker.consume(&remaining[..written]) {
                    Ok((_, true)) => self.chunked = None,
                    Ok(_) => {}
                    Err(e) => return (consumed, Some(e)),
                }
                if let Some(e) = err_write {
                    return (consumed, Some(e));
                }
                pos += chunk_bytes;
                continue;
            }

            if self.header.is_empty() && !is_http_method_prefix(remaining) {
                let (written, err) = write_all(&mut self.inner, remaining);
                consumed += written;
                if let Some(e) = err {
                    return (consumed, Some(e));
                }
                return (original_length, None);
            }

            let previous_header_length = self.header.len();
            self.header.extend_from_slice(remaining);
            let Some(header_end) = find(&self.header, b"\r\n\r\n") else {
                if self.header.len() > MAX_BUFFERED_REQUEST_HEADER {
                    return (
                        consumed,
                        Some(io::Error::other(format!(
                            "httpwire: request header exceeds {MAX_BUFFERED_REQUEST_HEADER} bytes"
                        ))),
                    );
                }
                return (original_length, None);
            };
            let header_end = header_end + 4;
            let remaining_len = remaining.len();
            let buffered = std::mem::take(&mut self.header);
            let (header, body) = buffered.split_at(header_end);
            let current_header_bytes = remaining_len.min(header_end.saturating_sub(previous_header_length));

            let (ordered, content_length, chunked) = order_request_header(header, &*self.order);
            if let (_, Some(e)) = write_all(&mut self.inner, &ordered) {
                return (original_length, Some(e));
            }
            consumed += current_header_bytes;
            if chunked {
                self.chunked = Some(ChunkedRequestTracker::new());
            } else {
                self.body_remaining = content_length;
            }
            data = Cow::Owned(body.to_vec());
            pos = 0;
        }
        (original_length, None)
    }
}

impl<W: Write> Write for OrderedRequestWriter<W> {
    /// A failure after some bytes were accepted reports those bytes; the error resurfaces when the
    /// caller retries the rest.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.write_counted(buf) {
            (n, None) => Ok(n),
            (0, Some(e)) => Err(e),
            (n, Some(_)) => Ok(n),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Rewrites the header block (request line + headers + blank line). Returns the new bytes, the
/// declared content length and whether the body is chunked.
fn order_request_header(
    header: &[u8],
    order: &(dyn Fn(&str, &str) -> Vec<String> + Send + Sync),
) -> (Vec<u8>, i64, bool) {
    let block = &header[..header.len() - 4];
    let lines: Vec<&[u8]> = split_crlf(block);
    let header_lines = &lines[1..];
    let unchanged = || (header.to_vec(), request_content_length(header_lines), request_uses_chunked_encoding(header_lines));

    let request_line = String::from_utf8_lossy(lines[0]);
    let mut parts = request_line.splitn(3, ' ');
    let (Some(method), Some(target), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return unchanged();
    };
    let desired = order(method, target);
    if desired.is_empty() {
        return unchanged();
    }

    let mut used = vec![false; header_lines.len()];
    let mut ordered: Vec<Cow<[u8]>> = Vec::with_capacity(lines.len());
    ordered.push(Cow::Borrowed(lines[0]));
    for name in &desired {
        for (index, line) in header_lines.iter().enumerate() {
            if used[index] || !header_line_named(line, name) {
                continue;
            }
            match line.iter().position(|&b| b == b':') {
                Some(colon) if colon > 0 && !name.is_empty() => {
                    let mut rewritten = Vec::with_capacity(name.len() + line.len() - colon);
                    rewritten.extend_from_slice(name.as_bytes());
                    rewritten.extend_from_slice(&line[colon..]);
                    ordered.push(Cow::Owned(rewritten));
                }
                _ => ordered.push(Cow::Borrowed(line)),
            }
            used[index] = true;
        }
    }
    for (index, line) in header_lines.iter().enumerate() {
        if !used[index] {
            ordered.push(Cow::Borrowed(line));
        }
    }

    let mut out = Vec::with_capacity(header.len());
    for line in ordered {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    (out, request_content_length(header_lines), request_uses_chunked_encoding(header_lines))
}

/// `bytes.Split(b, "\r\n")`.
fn split_crlf(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(i) = find(&b[start..], b"\r\n") {
        out.push(&b[start..start + i]);
        start += i + 2;
    }
    out.push(&b[start..]);
    out
}

fn header_line_named(line: &[u8], name: &str) -> bool {
    match line.iter().position(|&b| b == b':') {
        Some(colon) if colon > 0 => line[..colon].eq_ignore_ascii_case(name.as_bytes()),
        _ => false,
    }
}

fn is_http_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// True when `p` looks like the start of a request line: a method token followed by a space.
fn is_http_method_prefix(p: &[u8]) -> bool {
    if p.is_empty() {
        return false;
    }
    for (i, &b) in p.iter().enumerate() {
        if b == b' ' && i > 0 {
            return true;
        }
        if !is_http_token_char(b) {
            return false;
        }
    }
    true
}

fn header_value<'a>(line: &'a [u8]) -> &'a [u8] {
    match line.iter().position(|&b| b == b':') {
        Some(colon) => &line[colon + 1..],
        None => &[],
    }
}

fn request_content_length(lines: &[&[u8]]) -> i64 {
    for line in lines {
        if !header_line_named(line, "Content-Length") {
            continue;
        }
        let value = String::from_utf8_lossy(header_value(line));
        return match value.trim().parse::<i64>() {
            Ok(n) if n > 0 => n,
            _ => 0,
        };
    }
    0
}

fn request_uses_chunked_encoding(lines: &[&[u8]]) -> bool {
    for line in lines {
        if !header_line_named(line, "Transfer-Encoding") {
            continue;
        }
        let value = String::from_utf8_lossy(header_value(line));
        if value.split(',').any(|e| e.trim().eq_ignore_ascii_case("chunked")) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    ReadingSize,
    ReadingData,
    ReadingDataCrlf,
    ReadingTrailers,
}

/// Follows a chunked request body to find where it ends, without buffering it.
#[derive(Debug, Clone)]
struct ChunkedRequestTracker {
    state: ChunkState,
    line: Vec<u8>,
    data_remaining: i64,
    crlf_position: usize,
    trailers: Vec<u8>,
}

impl ChunkedRequestTracker {
    fn new() -> Self {
        ChunkedRequestTracker {
            state: ChunkState::ReadingSize,
            line: Vec::new(),
            data_remaining: 0,
            crlf_position: 0,
            trailers: Vec::new(),
        }
    }

    /// Feeds `data`; returns `(bytes consumed, body completed)`. Stops right after the final
    /// trailer terminator.
    fn consume(&mut self, data: &[u8]) -> io::Result<(usize, bool)> {
        let mut consumed = 0usize;
        while consumed < data.len() {
            match self.state {
                ChunkState::ReadingSize => {
                    self.line.push(data[consumed]);
                    consumed += 1;
                    if self.line.len() > MAX_BUFFERED_REQUEST_HEADER {
                        return Err(invalid(format!(
                            "httpwire: chunk size line exceeds {MAX_BUFFERED_REQUEST_HEADER} bytes"
                        )));
                    }
                    if !self.line.ends_with(b"\r\n") {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&self.line[..self.line.len() - 2]).into_owned();
                    let mut size_text = text.trim();
                    if let Some(ext) = size_text.find(';') {
                        size_text = size_text[..ext].trim();
                    }
                    let size = match i64::from_str_radix(size_text, 16) {
                        Ok(n) if n >= 0 => n,
                        _ => return Err(invalid(format!("httpwire: invalid chunk size {size_text:?}"))),
                    };
                    self.line.clear();
                    if size == 0 {
                        self.state = ChunkState::ReadingTrailers;
                        continue;
                    }
                    self.data_remaining = size;
                    self.state = ChunkState::ReadingData;
                }
                ChunkState::ReadingData => {
                    let chunk_bytes = ((data.len() - consumed) as i64).min(self.data_remaining);
                    consumed += chunk_bytes as usize;
                    self.data_remaining -= chunk_bytes;
                    if self.data_remaining == 0 {
                        self.crlf_position = 0;
                        self.state = ChunkState::ReadingDataCrlf;
                    }
                }
                ChunkState::ReadingDataCrlf => {
                    let want = b"\r\n";
                    if data[consumed] != want[self.crlf_position] {
                        return Err(invalid("httpwire: chunk data is missing CRLF terminator".into()));
                    }
                    consumed += 1;
                    self.crlf_position += 1;
                    if self.crlf_position == want.len() {
                        self.state = ChunkState::ReadingSize;
                    }
                }
                ChunkState::ReadingTrailers => {
                    self.trailers.push(data[consumed]);
                    consumed += 1;
                    if self.trailers.len() > MAX_BUFFERED_REQUEST_HEADER {
                        return Err(invalid(format!(
                            "httpwire: chunk trailers exceed {MAX_BUFFERED_REQUEST_HEADER} bytes"
                        )));
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

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// `writeAll`: writes everything, returning the byte count and the first error (a zero-length
/// write is a short-write error).
fn write_all<W: Write>(writer: &mut W, mut data: &[u8]) -> (usize, Option<io::Error>) {
    let mut total = 0;
    while !data.is_empty() {
        match writer.write(data) {
            Ok(0) => return (total, Some(io::Error::new(io::ErrorKind::WriteZero, "short write"))),
            Ok(n) => {
                total += n;
                data = &data[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return (total, Some(e)),
        }
    }
    (total, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(names: &[&str]) -> RequestHeaderOrder {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        Box::new(move |_, _| names.clone())
    }

    fn writer(names: &[&str]) -> OrderedRequestWriter<Vec<u8>> {
        OrderedRequestWriter::new(Vec::new(), order(names))
    }

    #[test]
    fn reorders_keep_alive_requests_without_changing_bodies() {
        let order: RequestHeaderOrder = Box::new(|method, target| {
            if method == "POST" && target == "/v1/messages?beta=true" {
                ["Accept", "Authorization", "Content-Type", "User-Agent", "Connection", "Host", "Accept-Encoding", "Content-Length"]
                    .map(String::from)
                    .to_vec()
            } else {
                ["Accept", "Host", "Connection"].map(String::from).to_vec()
            }
        });
        let mut conn = OrderedRequestWriter::new(Vec::new(), order);
        let first = "POST /v1/messages?beta=true HTTP/1.1\r\nHost: api.anthropic.com\r\nUser-Agent: claude-cli/2.1.220 (external, cli)\r\nContent-Length: 7\r\nAccept: application/json\r\nX-Unknown: keep\r\nAuthorization: Bearer placeholder\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nAccept-Encoding: gzip, deflate, br, zstd\r\n\r\n{\"a\":1}";
        let second = "GET /api/oauth/profile HTTP/1.1\r\nConnection: close\r\nHost: api.anthropic.com\r\nAccept: application/json\r\n\r\n";
        let want = "POST /v1/messages?beta=true HTTP/1.1\r\nAccept: application/json\r\nAuthorization: Bearer placeholder\r\nContent-Type: application/json\r\nUser-Agent: claude-cli/2.1.220 (external, cli)\r\nConnection: keep-alive\r\nHost: api.anthropic.com\r\nAccept-Encoding: gzip, deflate, br, zstd\r\nContent-Length: 7\r\nX-Unknown: keep\r\n\r\n{\"a\":1}GET /api/oauth/profile HTTP/1.1\r\nAccept: application/json\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n";
        let parts: [String; 4] = [
            first[..29].to_string(),
            first[29..first.len() - 3].to_string(),
            format!("{}{}", &first[first.len() - 3..], &second[..17]),
            second[17..].to_string(),
        ];
        for part in &parts {
            assert_eq!(conn.write(part.as_bytes()).unwrap(), part.len());
        }
        assert_eq!(String::from_utf8(conn.into_inner()).unwrap(), want);
    }

    #[test]
    fn preserves_chunked_body_and_reorders_next_request() {
        let mut conn = writer(&["Host", "Transfer-Encoding"]);
        let first = "POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\nHost: example.com\r\n\r\n4\r\ntest\r\n0\r\nX-Trailer: done\r\n\r\n";
        let second = "GET /next HTTP/1.1\r\nTransfer-Encoding: identity\r\nHost: example.com\r\n\r\n";
        let input = format!("{first}{second}");
        let want = "POST /upload HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ntest\r\n0\r\nX-Trailer: done\r\n\r\nGET /next HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: identity\r\n\r\n";
        for byte in input.as_bytes().chunks(1) {
            assert_eq!(conn.write(byte).unwrap(), 1);
        }
        assert_eq!(String::from_utf8(conn.into_inner()).unwrap(), want);
    }

    /// Writer that accepts `fail_limit` bytes of a write and then reports `fail`.
    #[derive(Default)]
    struct PartialErrorWriter {
        buf: Vec<u8>,
        fail_limit: usize,
        fail: bool,
    }

    impl Write for PartialErrorWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if !self.fail {
                self.buf.extend_from_slice(data);
                return Ok(data.len());
            }
            // Accept up to `fail_limit` bytes once, then fail like Go's (n, err) return.
            let written = self.fail_limit.min(data.len());
            self.fail_limit = 0;
            self.buf.extend_from_slice(&data[..written]);
            if written == 0 { Err(io::Error::other("injected partial write")) } else { Ok(written) }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reports_partial_body_write() {
        let mut conn = OrderedRequestWriter::new(PartialErrorWriter::default(), order(&["Host", "Content-Length"]));
        let header = b"POST /upload HTTP/1.1\r\nContent-Length: 5\r\nHost: example.com\r\n\r\n";
        assert_eq!(conn.write_counted(header).0, header.len());

        conn.get_mut().fail_limit = 2;
        conn.get_mut().fail = true;
        // Go's write reports the partial count with the injected error.
        let (written, err) = conn.write_counted(b"hello");
        assert!(err.is_some_and(|e| e.to_string().contains("injected partial write")));
        assert_eq!(written, 2);
        assert_eq!(conn.body_remaining, 3);

        conn.get_mut().fail = false;
        let (written, err) = conn.write_counted(b"llo");
        assert!(err.is_none());
        assert_eq!(written, 3);
        let second = b"GET /next HTTP/1.1\r\nContent-Length: 0\r\nHost: example.com\r\n\r\n";
        assert_eq!(conn.write_counted(second).0, second.len());
        let want = "POST /upload HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhelloGET /next HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(String::from_utf8(conn.into_inner().buf).unwrap(), want);
    }

    #[test]
    fn tracks_only_written_chunk_bytes_after_partial_error() {
        let mut conn = OrderedRequestWriter::new(PartialErrorWriter::default(), order(&["Host", "Transfer-Encoding"]));
        let header = b"POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\nHost: example.com\r\n\r\n";
        assert_eq!(conn.write_counted(header).0, header.len());

        let chunked_body = b"4\r\ntest\r\n0\r\nX-Trailer: done\r\n\r\n";
        conn.get_mut().fail_limit = 6;
        conn.get_mut().fail = true;
        let (written, err) = conn.write_counted(chunked_body);
        assert!(err.is_some());
        assert_eq!(written, 6);

        conn.get_mut().fail = false;
        let (retried, err) = conn.write_counted(&chunked_body[written..]);
        assert!(err.is_none());
        assert_eq!(retried, chunked_body.len() - written);
        let second = b"GET /next HTTP/1.1\r\nTransfer-Encoding: identity\r\nHost: example.com\r\n\r\n";
        assert_eq!(conn.write_counted(second).0, second.len());
        let want = format!(
            "POST /upload HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n{}GET /next HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: identity\r\n\r\n",
            String::from_utf8_lossy(chunked_body)
        );
        assert_eq!(String::from_utf8(conn.into_inner().buf).unwrap(), want);
    }

    #[test]
    fn rewrites_exact_header_casing() {
        let mut conn = writer(&["x-custom-header", "Sec-CH-UA-Platform", "Host"]);
        let input = b"GET /test HTTP/1.1\r\nHost: example.com\r\nX-Custom-Header: value1\r\nSEC-CH-UA-PLATFORM: macos\r\n\r\n";
        conn.write_all(input).unwrap();
        assert_eq!(
            String::from_utf8(conn.into_inner()).unwrap(),
            "GET /test HTTP/1.1\r\nx-custom-header: value1\r\nSec-CH-UA-Platform: macos\r\nHost: example.com\r\n\r\n"
        );
    }

    #[test]
    fn bypasses_non_http_handshake_bytes() {
        let mut conn = writer(&["User-Agent", "Host"]);
        let socks = [0x05u8, 0x01, 0x00];
        assert_eq!(conn.write(&socks).unwrap(), 3);
        assert_eq!(conn.get_ref().as_slice(), &socks);
        let request = b"GET / HTTP/1.1\r\nHost: target.com\r\nUser-Agent: curl/8.0\r\n\r\n";
        assert_eq!(conn.write(request).unwrap(), request.len());
        let mut want = socks.to_vec();
        want.extend_from_slice(b"GET / HTTP/1.1\r\nUser-Agent: curl/8.0\r\nHost: target.com\r\n\r\n");
        assert_eq!(conn.into_inner(), want);
    }

    #[test]
    fn supports_custom_and_lowercase_http_methods() {
        let mut conn = writer(&["Man", "Host"]);
        conn.write_all(b"M-SEARCH * HTTP/1.1\r\nHost: 239.255.255.250:1900\r\nMan: \"ssdp:discover\"\r\n\r\n").unwrap();
        assert_eq!(
            String::from_utf8(conn.into_inner()).unwrap(),
            "M-SEARCH * HTTP/1.1\r\nMan: \"ssdp:discover\"\r\nHost: 239.255.255.250:1900\r\n\r\n"
        );
    }

    #[test]
    fn supports_long_and_fragmented_http_methods() {
        let mut conn = writer(&["x-b", "x-a"]);
        let long = "M".repeat(70);
        let input = format!("{long} /resource HTTP/1.1\r\nHost: example.com\r\nx-a: 1\r\nx-b: 2\r\n\r\n");
        for part in input.as_bytes().chunks(10) {
            conn.write_all(part).unwrap();
        }
        assert_eq!(
            String::from_utf8(conn.into_inner()).unwrap(),
            format!("{long} /resource HTTP/1.1\r\nx-b: 2\r\nx-a: 1\r\nHost: example.com\r\n\r\n")
        );
    }
}
