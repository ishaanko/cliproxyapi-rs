//! Line reader for upstream SSE / NDJSON bodies with `bufio.Scanner` semantics.
//!
//! Go executors read streams with `bufio.NewScanner(body)` + `scanner.Buffer(nil, max)`: lines are
//! split on `\n`, one trailing `\r` is dropped, empty lines are yielded, a final unterminated
//! line is yielded, and a line that cannot fit in `max` bytes (terminator included) ends the
//! scan with `bufio.ErrTooLong`. [`LineSplitter`] is the synchronous core (feed bytes, pull
//! lines); [`LineReader`] drives it from an async byte stream such as a reqwest response body.

use std::pin::Pin;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Stream, StreamExt};

/// Scanner buffer limit used by Claude, Codex, Gemini family, OpenAI-compat and xAI (50 MiB).
pub const STREAM_SCANNER_BUFFER: usize = 52_428_800;
/// Scanner buffer limit used by the Kimi executor (1 MiB).
pub const KIMI_SCANNER_BUFFER: usize = 1_048_576;
/// `bufio.Scanner` default when `Buffer` is never called (`bufio.MaxScanTokenSize`).
pub const DEFAULT_MAX_TOKEN_SIZE: usize = 64 * 1024;

/// Why a scan stopped early.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanError {
    /// Go: `bufio.ErrTooLong`.
    #[error("bufio.Scanner: token too long")]
    TooLong,
    /// The underlying body failed mid-stream.
    #[error("{0}")]
    Read(String),
}

/// What [`LineSplitter::step`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// One line without its terminator (`\n` and a single preceding `\r` removed).
    Line(Bytes),
    /// No complete line is buffered; feed more bytes (or signal EOF).
    NeedMore,
    /// EOF reached with nothing left over.
    Done,
}

/// Incremental `ScanLines` with a maximum token size.
#[derive(Debug)]
pub struct LineSplitter {
    buf: BytesMut,
    max_token_size: usize,
    /// Bytes at the front of `buf` already known to hold no `\n`.
    scanned: usize,
}

impl LineSplitter {
    pub fn new(max_token_size: usize) -> Self {
        Self { buf: BytesMut::new(), max_token_size: max_token_size.max(1), scanned: 0 }
    }

    /// Appends received bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Pulls the next line. `at_eof` says no more bytes will arrive.
    ///
    /// A line (with its `\n`) longer than the token limit, or an unterminated tail that already
    /// fills the limit, is [`ScanError::TooLong`], exactly when `bufio.Scanner` gives up.
    pub fn step(&mut self, at_eof: bool) -> Result<Step, ScanError> {
        let window = self.buf.len().min(self.max_token_size);
        if let Some(offset) = self.buf[self.scanned.min(window)..window].iter().position(|b| *b == b'\n') {
            let end = self.scanned.min(window) + offset;
            let line = self.buf.split_to(end).freeze();
            self.buf.advance(1); // the '\n'
            self.scanned = 0;
            return Ok(Step::Line(drop_cr(line)));
        }
        self.scanned = window;
        if self.buf.len() >= self.max_token_size {
            return Err(ScanError::TooLong);
        }
        if !at_eof {
            return Ok(Step::NeedMore);
        }
        if self.buf.is_empty() {
            return Ok(Step::Done);
        }
        self.scanned = 0;
        Ok(Step::Line(drop_cr(self.buf.split().freeze())))
    }
}

fn drop_cr(line: Bytes) -> Bytes {
    if line.last() == Some(&b'\r') { line.slice(..line.len() - 1) } else { line }
}

/// Boxed upstream body bytes; read errors are already rendered to text.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// Async line reader over an upstream body.
///
/// ```ignore
/// let mut lines = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
/// while let Some(line) = lines.next_line().await {
///     let line = line?; // ScanError ends the stream like scanner.Err()
/// }
/// ```
pub struct LineReader {
    body: BodyStream,
    splitter: LineSplitter,
    eof: bool,
    failed: bool,
}

impl LineReader {
    pub fn new(body: BodyStream, max_token_size: usize) -> Self {
        Self { body, splitter: LineSplitter::new(max_token_size), eof: false, failed: false }
    }

    /// Reads a reqwest response body (already decompressed by the client when applicable).
    pub fn from_response(resp: reqwest::Response, max_token_size: usize) -> Self {
        Self::new(Box::pin(resp.bytes_stream().map(|r| r.map_err(|e| super::status::transport_message(&e)))), max_token_size)
    }

    /// [`from_response`](Self::from_response) that marks `reporter`'s TTFT on the first body
    /// byte (`packet_only`: first-packet fallback only; see `UsageReporter::observe_body_stream`).
    pub fn from_response_tracked(resp: reqwest::Response, max_token_size: usize, reporter: &super::usage::UsageReporter, packet_only: bool) -> Self {
        Self::new(
            Box::pin(reporter.observe_body_stream(resp.bytes_stream(), packet_only).map(|r| r.map_err(|e| super::status::transport_message(&e)))),
            max_token_size,
        )
    }

    /// Reads any byte stream whose errors render as text.
    pub fn from_stream<S, E>(stream: S, max_token_size: usize) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: std::fmt::Display,
    {
        Self::new(Box::pin(stream.map(|r| r.map_err(|e| e.to_string()))), max_token_size)
    }

    /// Next line, `None` at a clean end of body. After an error the reader is finished.
    pub async fn next_line(&mut self) -> Option<Result<Bytes, ScanError>> {
        if self.failed {
            return None;
        }
        loop {
            match self.splitter.step(self.eof) {
                Ok(Step::Line(line)) => return Some(Ok(line)),
                Ok(Step::Done) => return None,
                Ok(Step::NeedMore) => match self.body.next().await {
                    Some(Ok(chunk)) => self.splitter.push(&chunk),
                    Some(Err(err)) => {
                        self.failed = true;
                        return Some(Err(ScanError::Read(err)));
                    }
                    None => self.eof = true,
                },
                Err(err) => {
                    self.failed = true;
                    return Some(Err(err));
                }
            }
        }
    }

    /// [`Self::next_line`], but a closed client channel ends the wait at once with a
    /// `context canceled` read error instead of lingering until the next upstream frame. Go's
    /// request context cancels the body read the same way.
    ///
    /// Lines that are already buffered are returned without building the `closed()` wait (this
    /// runs once per upstream line); the channel state is still checked first, so a closed
    /// client wins over a buffered line exactly like a biased `select!`.
    pub async fn next_line_or_closed<T>(&mut self, client: &tokio::sync::mpsc::Sender<T>) -> Option<Result<Bytes, ScanError>> {
        if self.failed && !client.is_closed() {
            return None;
        }
        loop {
            if client.is_closed() {
                self.failed = true;
                return Some(Err(ScanError::Read("context canceled".to_string())));
            }
            match self.splitter.step(self.eof) {
                Ok(Step::Line(line)) => return Some(Ok(line)),
                Ok(Step::Done) => return None,
                Ok(Step::NeedMore) => {
                    tokio::select! {
                        biased;
                        _ = client.closed() => {}
                        chunk = self.body.next() => match chunk {
                            Some(Ok(chunk)) => self.splitter.push(&chunk),
                            Some(Err(err)) => {
                                self.failed = true;
                                return Some(Err(ScanError::Read(err)));
                            }
                            None => self.eof = true,
                        },
                    }
                }
                Err(err) => {
                    self.failed = true;
                    return Some(Err(err));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]], max: usize) -> (Vec<String>, Option<ScanError>) {
        let mut s = LineSplitter::new(max);
        let mut lines = Vec::new();
        for chunk in chunks {
            s.push(chunk);
            loop {
                match s.step(false) {
                    Ok(Step::Line(l)) => lines.push(String::from_utf8_lossy(&l).into_owned()),
                    Ok(_) => break,
                    Err(e) => return (lines, Some(e)),
                }
            }
        }
        loop {
            match s.step(true) {
                Ok(Step::Line(l)) => lines.push(String::from_utf8_lossy(&l).into_owned()),
                Ok(_) => return (lines, None),
                Err(e) => return (lines, Some(e)),
            }
        }
    }

    #[test]
    fn splits_like_scan_lines() {
        let (lines, err) = collect(&[b"data: a\r\n\nda", b"ta: b\n", b"tail"], 1024);
        assert_eq!(lines, ["data: a", "", "data: b", "tail"]);
        assert_eq!(err, None);
        // Only one trailing CR is dropped; an empty body has no tokens; a lone "\n" is one empty line.
        assert_eq!(collect(&[b"x\r\r\n"], 64).0, ["x\r"]);
        assert!(collect(&[b""], 64).0.is_empty());
        assert_eq!(collect(&[b"\n"], 64).0, [""]);
    }

    #[test]
    fn token_limit_matches_bufio() {
        // A terminated line needs len + 1 <= max.
        assert_eq!(collect(&[b"abcd\n"], 5), (vec!["abcd".to_string()], None));
        assert_eq!(collect(&[b"abcde\n"], 5).1, Some(ScanError::TooLong));
        // An unterminated final line must be shorter than max (the buffer fills before EOF is seen).
        assert_eq!(collect(&[b"abcd"], 5), (vec!["abcd".to_string()], None));
        assert_eq!(collect(&[b"abcde"], 5).1, Some(ScanError::TooLong));
        // Lines before the offending one are delivered first, even across chunks.
        let (lines, err) = collect(&[b"ok\nabc", b"defghij\n"], 5);
        assert_eq!(lines, ["ok"]);
        assert_eq!(err, Some(ScanError::TooLong));
    }

    #[tokio::test]
    async fn reader_yields_lines_then_error() {
        let chunks: Vec<Result<Bytes, String>> = vec![
            Ok(Bytes::from_static(b"event: x\ndata: 1\n")),
            Ok(Bytes::from_static(b"data: 2")),
            Err("connection reset".into()),
        ];
        let mut r = LineReader::from_stream(futures_util::stream::iter(chunks), 1024);
        assert_eq!(r.next_line().await.unwrap().unwrap(), "event: x");
        assert_eq!(r.next_line().await.unwrap().unwrap(), "data: 1");
        assert_eq!(r.next_line().await.unwrap().unwrap_err(), ScanError::Read("connection reset".into()));
        assert!(r.next_line().await.is_none());
    }
}
