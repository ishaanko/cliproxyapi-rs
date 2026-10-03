//! Magic-byte response decompression (Go: `decodeResponseBody` with no `Content-Encoding`).
//!
//! reqwest already decodes bodies whose `Content-Encoding` header is present and strips the
//! header. Misbehaving upstreams that compress without the header are caught here: gzip (`1f
//! 8b`) and zstd (`28 b5 2f fd`) have reliable magic bytes; brotli and deflate have none and
//! pass through untouched.

use std::io::{self, Write};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

enum Kind {
    Gzip,
    Zstd,
}

/// Go's `Peek(4)` rule: four bytes (or EOF with at least two) decide the format.
fn sniff(head: &[u8]) -> Option<Kind> {
    if head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        Some(Kind::Gzip)
    } else if head.len() >= 4 && head[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        Some(Kind::Zstd)
    } else {
        None
    }
}

fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Gzip => "gzip",
        Kind::Zstd => "zstd",
    }
}

/// Largest plain output one `feed` may produce, and of a whole body: a tiny compressed payload
/// must not expand without bound (decompression bomb).
const MAX_FEED_OUTPUT: usize = 32 << 20;
const MAX_TOTAL_OUTPUT: usize = 256 << 20;

/// Decoder output buffer that refuses to grow past the per-feed and total limits.
#[derive(Default)]
struct Sink {
    buf: Vec<u8>,
    /// Plain bytes already handed out by `take`.
    taken: usize,
}

impl Sink {
    fn take(&mut self) -> Vec<u8> {
        let out = std::mem::take(&mut self.buf);
        self.taken += out.len();
        out
    }
}

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > MAX_FEED_OUTPUT || self.taken + self.buf.len() + data.len() > MAX_TOTAL_OUTPUT {
            return Err(io::Error::other("decompressed body too large"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Incremental decoder fed with compressed chunks.
enum Push {
    Gzip(Box<flate2::write::GzDecoder<Sink>>),
    Zstd(Box<zstd::stream::write::Decoder<'static, Sink>>),
}

impl Push {
    fn new(kind: &Kind) -> Result<Self, String> {
        match kind {
            Kind::Gzip => Ok(Push::Gzip(Box::new(flate2::write::GzDecoder::new(Sink::default())))),
            Kind::Zstd => zstd::stream::write::Decoder::new(Sink::default())
                .map(|d| Push::Zstd(Box::new(d)))
                .map_err(|e| format!("magic-byte zstd: failed to create reader: {e}")),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Push::Gzip(_) => "gzip",
            Push::Zstd(_) => "zstd",
        }
    }

    /// Go fails at reader creation only on a bad gzip header; later corruption (or an oversized
    /// output) surfaces as a read error.
    fn describe(&self, e: &io::Error) -> String {
        if matches!(self, Push::Gzip(_)) && e.to_string().contains("invalid gzip header") {
            format!("magic-byte gzip: failed to create reader: {e}")
        } else {
            format!("magic-byte {}: read error: {e}", self.label())
        }
    }

    /// Feeds `chunk` and returns the plain bytes it completed.
    fn feed(&mut self, chunk: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Push::Gzip(d) => {
                d.write_all(chunk)?;
                Ok(d.get_mut().take())
            }
            Push::Zstd(d) => {
                d.write_all(chunk)?;
                d.flush()?;
                Ok(d.get_mut().take())
            }
        }
    }

    fn finish(self) -> io::Result<Vec<u8>> {
        match self {
            Push::Gzip(d) => d.finish().map(|mut sink| sink.take()),
            Push::Zstd(mut d) => {
                d.flush()?;
                Ok(d.get_mut().take())
            }
        }
    }
}

/// Decompresses a whole body when it starts with gzip or zstd magic bytes.
pub fn decode_body(body: Bytes) -> Result<Bytes, String> {
    let Some(kind) = sniff(&body) else { return Ok(body) };
    let label = kind_label(&kind);
    let mut decoder = Push::new(&kind)?;
    // Fed in slices so the per-feed output limit only trips on a bomb, not on a large body.
    let mut out = Vec::new();
    for piece in body.chunks(64 << 10) {
        out.extend(decoder.feed(piece).map_err(|e| decoder.describe(&e))?);
    }
    out.extend(decoder.finish().map_err(|e| format!("magic-byte {label}: {e}"))?);
    Ok(Bytes::from(out))
}

#[derive(PartialEq)]
enum Phase {
    /// Holding back the first bytes until the format is decided.
    Peeking,
    Passing,
    Done,
}

struct State<S> {
    inner: S,
    head: Vec<u8>,
    decoder: Option<Push>,
    phase: Phase,
    /// The inner stream ended while peeking.
    ended: bool,
}

impl<S> State<S> {
    /// Picks the format from the held-back head and returns its first output.
    fn decide(&mut self) -> Result<Bytes, String> {
        self.phase = Phase::Passing;
        let head = std::mem::take(&mut self.head);
        let Some(kind) = sniff(&head) else { return Ok(Bytes::from(head)) };
        let mut decoder = Push::new(&kind)?;
        let out = decoder.feed(&head).map_err(|e| decoder.describe(&e))?;
        self.decoder = Some(decoder);
        Ok(Bytes::from(out))
    }
}

/// Wraps a body stream so gzip/zstd bodies without a `Content-Encoding` header arrive decoded.
pub fn decode_stream<S>(inner: S) -> impl Stream<Item = Result<Bytes, String>>
where
    S: Stream<Item = Result<Bytes, String>> + Unpin,
{
    let state = State { inner, head: Vec::new(), decoder: None, phase: Phase::Peeking, ended: false };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if st.phase == Phase::Done {
                return None;
            }
            if st.phase == Phase::Peeking {
                match st.inner.next().await {
                    Some(Ok(chunk)) => {
                        st.head.extend_from_slice(&chunk);
                        if st.head.len() < 4 {
                            continue;
                        }
                    }
                    Some(Err(e)) => {
                        st.phase = Phase::Done;
                        return Some((Err(e), st));
                    }
                    None => st.ended = true,
                }
                match st.decide() {
                    Ok(b) if b.is_empty() => continue,
                    Ok(b) => return Some((Ok(b), st)),
                    Err(e) => {
                        st.phase = Phase::Done;
                        return Some((Err(e), st));
                    }
                }
            }
            let next = if st.ended { None } else { st.inner.next().await };
            let out = match next {
                Some(Ok(chunk)) => match st.decoder.as_mut() {
                    Some(d) => d.feed(&chunk).map(Bytes::from).map_err(|e| d.describe(&e)),
                    None => Ok(chunk),
                },
                Some(Err(e)) => Err(e),
                None => {
                    st.phase = Phase::Done;
                    match st.decoder.take().map(Push::finish) {
                        Some(Ok(rest)) if !rest.is_empty() => return Some((Ok(Bytes::from(rest)), st)),
                        Some(Err(e)) => return Some((Err(format!("magic-byte read error: {e}")), st)),
                        _ => return None,
                    }
                }
            };
            match out {
                Ok(b) if b.is_empty() => {}
                Ok(b) => return Some((Ok(b), st)),
                Err(e) => {
                    st.phase = Phase::Done;
                    return Some((Err(e), st));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn whole_body_gzip_zstd_and_plain() {
        let plain = br#"{"type":"message"}"#;
        assert_eq!(decode_body(Bytes::from(gzip(plain))).unwrap(), plain.as_slice());
        assert_eq!(decode_body(Bytes::from(zstd::encode_all(&plain[..], 0).unwrap())).unwrap(), plain.as_slice());
        assert_eq!(decode_body(Bytes::from_static(plain)).unwrap(), plain.as_slice());
        assert!(decode_body(Bytes::from_static(&[0x1f, 0x8b, 0, 0, 0])).is_err());
    }

    #[test]
    fn decompression_bomb_and_mid_body_corruption_are_rejected_with_distinct_errors() {
        let bomb = zstd::encode_all(&vec![0u8; MAX_FEED_OUTPUT + (8 << 20)][..], 0).unwrap();
        let err = decode_body(Bytes::from(bomb)).unwrap_err();
        assert!(err.contains("too large"), "{err}");

        let mut packed = gzip(&b"x".repeat(4096));
        let mid = packed.len() / 2;
        packed[mid..].fill(0xff);
        let err = decode_body(Bytes::from(packed)).unwrap_err();
        assert!(!err.contains("failed to create reader"), "{err}");
        let err = decode_body(Bytes::from_static(&[0x1f, 0x8b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap_err();
        assert!(err.contains("failed to create reader"), "{err}");
    }

    #[tokio::test]
    async fn streamed_chunks_are_decoded_across_boundaries() {
        let plain = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n".repeat(20);
        for packed in [gzip(&plain), zstd::encode_all(&plain[..], 0).unwrap(), plain.clone()] {
            let chunks: Vec<Result<Bytes, String>> = packed.chunks(3).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
            let out: Vec<u8> = decode_stream(futures_util::stream::iter(chunks))
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
                .await
                .concat()
                .to_vec();
            assert_eq!(out, plain);
        }
    }
}
