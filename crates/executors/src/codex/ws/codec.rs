//! Minimal RFC 6455 client framing with RFC 7692 permessage-deflate on the receive side.
//!
//! Go's dialer offers `permessage-deflate` (and never compresses outbound), so servers may send
//! compressed frames; a general websocket library would reject those, hence this small codec.
//! Reading and writing are separate halves so the reader never waits on a request that is
//! mid-write; pings are surfaced to the caller, which answers them through the shared writer.

use std::io;

use bytes::{Buf, BytesMut};
use flate2::{Decompress, FlushDecompress};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

use super::transport::BoxIo;

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Deflate tail every compressed message omits (RFC 7692 section 7.2.1).
const DEFLATE_TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// Largest frame or reassembled message the reader accepts (Go's gorilla has no limit, but an
/// unbounded buffer lets a peer exhaust memory); larger ones are protocol errors.
const MAX_MESSAGE_LEN: u64 = 256 << 20;

/// What the reader hands back.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    Text(Vec<u8>),
    Binary,
    /// Peer ping; the caller answers it with [`WsWriter::send_pong`] off the reader task.
    Ping(Vec<u8>),
    /// Peer close frame; code 1005 when it carried none.
    Close { code: u16, reason: String },
}

/// Negotiated `permessage-deflate` parameters (receive side).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Deflate {
    pub enabled: bool,
    /// The server resets its compression context after every message.
    pub no_context_takeover: bool,
}

impl Deflate {
    /// Parses the handshake response's `Sec-WebSocket-Extensions` value; `Err` for an extension
    /// that was not offered.
    pub fn from_response_header(value: &str) -> Result<Deflate, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Deflate::default());
        }
        let mut out = Deflate::default();
        for extension in value.split(',') {
            let mut parts = extension.split(';').map(str::trim);
            if parts.next() != Some("permessage-deflate") {
                return Err(format!("websocket: unsupported extension {value}"));
            }
            out.enabled = true;
            out.no_context_takeover = parts.any(|p| p == "server_no_context_takeover");
        }
        Ok(out)
    }
}

pub struct WsReader {
    io: ReadHalf<BoxIo>,
    buf: BytesMut,
    deflate: Deflate,
    inflater: Option<Decompress>,
    /// In-progress fragmented message: (opcode, compressed, payload so far).
    fragments: Option<(u8, bool, Vec<u8>)>,
}

pub struct WsWriter {
    io: WriteHalf<BoxIo>,
}

/// Splits a connected, upgraded stream; `leftover` holds bytes read past the handshake response.
pub fn split(io: BoxIo, leftover: &[u8], deflate: Deflate) -> (WsReader, WsWriter) {
    let (rd, wr) = tokio::io::split(io);
    let reader = WsReader { io: rd, buf: BytesMut::from(leftover), deflate, inflater: None, fragments: None };
    (reader, WsWriter { io: wr })
}

impl WsWriter {
    async fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask: [u8; 4] = rand::random();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.io.write_all(&frame).await?;
        self.io.flush().await
    }

    /// One unfragmented, uncompressed text message.
    pub async fn send_text(&mut self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(OP_TEXT, payload).await
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(OP_PONG, payload).await
    }

    /// Ends the connection (TLS close_notify / FIN).
    pub async fn shutdown(&mut self) {
        let _ = self.io.shutdown().await;
    }
}

fn protocol_error(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("websocket: {msg}"))
}

struct Frame {
    fin: bool,
    rsv1: bool,
    opcode: u8,
    payload: Vec<u8>,
}

impl WsReader {
    /// Next complete frame from the buffer, `None` when more bytes are needed.
    fn parse_frame(&mut self) -> io::Result<Option<Frame>> {
        if self.buf.len() < 2 {
            return Ok(None);
        }
        let (b0, b1) = (self.buf[0], self.buf[1]);
        if b0 & 0x30 != 0 {
            return Err(protocol_error("unexpected reserved bits"));
        }
        let masked = b1 & 0x80 != 0;
        let (len, header) = match b1 & 0x7f {
            126 => {
                if self.buf.len() < 4 {
                    return Ok(None);
                }
                (u16::from_be_bytes([self.buf[2], self.buf[3]]) as u64, 4)
            }
            127 => {
                if self.buf.len() < 10 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&self.buf[2..10]);
                (u64::from_be_bytes(raw), 10)
            }
            n => (n as u64, 2),
        };
        let mask_len = if masked { 4 } else { 0 };
        if len > MAX_MESSAGE_LEN || len > isize::MAX as u64 {
            return Err(protocol_error("read limit exceeded"));
        }
        let total = header as u64 + mask_len as u64 + len;
        if (self.buf.len() as u64) < total {
            return Ok(None);
        }
        let len = len as usize;
        let mask = masked.then(|| [self.buf[header], self.buf[header + 1], self.buf[header + 2], self.buf[header + 3]]);
        let mut payload = self.buf[header + mask_len..header + mask_len + len].to_vec();
        if let Some(mask) = mask {
            payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
        }
        self.buf.advance(header + mask_len + len);
        let opcode = b0 & 0x0f;
        let fin = b0 & 0x80 != 0;
        if opcode >= 0x8 && (!fin || len > 125) {
            return Err(protocol_error("invalid control frame"));
        }
        Ok(Some(Frame { fin, rsv1: b0 & 0x40 != 0, opcode, payload }))
    }

    fn inflate(&mut self, compressed: &[u8]) -> io::Result<Vec<u8>> {
        let inflater = self.inflater.get_or_insert_with(|| Decompress::new(false));
        if self.deflate.no_context_takeover {
            inflater.reset(false);
        }
        let mut input = compressed.to_vec();
        input.extend_from_slice(&DEFLATE_TAIL);
        let mut out = Vec::with_capacity(input.len() * 4);
        let mut consumed = 0usize;
        loop {
            if out.capacity() - out.len() < 4096 {
                out.reserve(out.len().max(4096));
            }
            let (before_in, before_out) = (inflater.total_in(), inflater.total_out());
            inflater
                .decompress_vec(&input[consumed..], &mut out, FlushDecompress::Sync)
                .map_err(|e| protocol_error(&format!("inflate: {e}")))?;
            consumed += (inflater.total_in() - before_in) as usize;
            let progressed = inflater.total_in() != before_in || inflater.total_out() != before_out;
            if out.len() as u64 > MAX_MESSAGE_LEN {
                return Err(protocol_error("read limit exceeded"));
            }
            if (consumed >= input.len() && out.len() < out.capacity()) || !progressed {
                break;
            }
        }
        Ok(out)
    }

    /// Reads until one data message, ping or close frame.
    pub async fn read_event(&mut self) -> io::Result<Incoming> {
        loop {
            while let Some(frame) = self.parse_frame()? {
                match frame.opcode {
                    OP_PING => return Ok(Incoming::Ping(frame.payload)),
                    OP_PONG => {}
                    OP_CLOSE => {
                        return Ok(match frame.payload.as_slice() {
                            [] | [_] => Incoming::Close { code: 1005, reason: String::new() },
                            [hi, lo, rest @ ..] => Incoming::Close { code: u16::from_be_bytes([*hi, *lo]), reason: String::from_utf8_lossy(rest).into_owned() },
                        });
                    }
                    OP_TEXT | OP_BINARY => {
                        if self.fragments.is_some() {
                            return Err(protocol_error("expected continuation frame"));
                        }
                        if frame.rsv1 && !self.deflate.enabled {
                            return Err(protocol_error("unexpected reserved bits"));
                        }
                        if frame.fin {
                            return self.finish(frame.opcode, frame.rsv1, frame.payload);
                        }
                        self.fragments = Some((frame.opcode, frame.rsv1, frame.payload));
                    }
                    OP_CONT => {
                        let Some((opcode, compressed, mut data)) = self.fragments.take() else {
                            return Err(protocol_error("unexpected continuation frame"));
                        };
                        if (data.len() as u64).saturating_add(frame.payload.len() as u64) > MAX_MESSAGE_LEN {
                            return Err(protocol_error("read limit exceeded"));
                        }
                        data.extend_from_slice(&frame.payload);
                        if frame.fin {
                            return self.finish(opcode, compressed, data);
                        }
                        self.fragments = Some((opcode, compressed, data));
                    }
                    _ => return Err(protocol_error("unknown opcode")),
                }
            }
            if self.io.read_buf(&mut self.buf).await? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF"));
            }
        }
    }

    fn finish(&mut self, opcode: u8, compressed: bool, data: Vec<u8>) -> io::Result<Incoming> {
        let data = if compressed { self.inflate(&data)? } else { data };
        if opcode == OP_BINARY {
            return Ok(Incoming::Binary);
        }
        if std::str::from_utf8(&data).is_err() {
            return Err(protocol_error("invalid UTF-8 in text message"));
        }
        Ok(Incoming::Text(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn server_frame(opcode: u8, fin: bool, rsv1: bool, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![(if fin { 0x80 } else { 0 }) | (if rsv1 { 0x40 } else { 0 }) | opcode];
        if payload.len() < 126 {
            f.push(payload.len() as u8);
        } else {
            f.push(126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(payload);
        f
    }

    fn deflate_message(c: &mut Compress, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + 64);
        c.compress_vec(text, &mut out, FlushCompress::Sync).unwrap();
        out.truncate(out.len() - 4); // drop the 00 00 ff ff tail
        out
    }

    async fn run(deflate: Deflate, wire: Vec<u8>) -> Vec<io::Result<Incoming>> {
        let (client, mut server) = tokio::io::duplex(1 << 16);
        let (mut reader, _writer) = split(Box::new(client), &[], deflate);
        server.write_all(&wire).await.unwrap();
        server.shutdown().await.unwrap();
        tokio::spawn(async move {
            let mut sink = Vec::new();
            let _ = server.read_to_end(&mut sink).await;
        });
        let mut out = Vec::new();
        loop {
            let ev = reader.read_event().await;
            if matches!(ev, Ok(Incoming::Ping(_))) {
                continue;
            }
            let done = ev.is_err() || matches!(ev, Ok(Incoming::Close { .. }));
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    #[tokio::test]
    async fn text_fragments_and_close_frames() {
        let mut wire = server_frame(OP_TEXT, false, false, b"{\"a\":");
        wire.extend(server_frame(OP_PING, true, false, b"hi"));
        wire.extend(server_frame(OP_CONT, true, false, b"1}"));
        let mut close = 1009u16.to_be_bytes().to_vec();
        close.extend_from_slice(b"too big");
        wire.extend(server_frame(OP_CLOSE, true, false, &close));
        let events = run(Deflate::default(), wire).await;
        assert_eq!(events[0].as_ref().unwrap(), &Incoming::Text(b"{\"a\":1}".to_vec()));
        assert_eq!(events[1].as_ref().unwrap(), &Incoming::Close { code: 1009, reason: "too big".into() });
    }

    #[tokio::test]
    async fn compressed_messages_inflate_with_and_without_context_takeover() {
        for no_takeover in [false, true] {
            let mut compressor = Compress::new(Compression::default(), false);
            let first = deflate_message(&mut compressor, br#"{"type":"response.created","pad":"aaaaaaaaaaaaaaaaaaaaaaaa"}"#);
            if no_takeover {
                compressor.reset();
            }
            let second = deflate_message(&mut compressor, br#"{"type":"response.created","pad":"aaaaaaaaaaaaaaaaaaaaaaaa"}"#);
            let mut wire = server_frame(OP_TEXT, true, true, &first);
            wire.extend(server_frame(OP_TEXT, true, true, &second));
            let events = run(Deflate { enabled: true, no_context_takeover: no_takeover }, wire).await;
            let expected = Incoming::Text(br#"{"type":"response.created","pad":"aaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_vec());
            assert_eq!(events[0].as_ref().unwrap(), &expected);
            assert_eq!(events[1].as_ref().unwrap(), &expected);
        }
    }

    #[tokio::test]
    async fn compressed_frames_without_negotiation_and_bad_utf8_are_errors() {
        let events = run(Deflate::default(), server_frame(OP_TEXT, true, true, b"x")).await;
        assert!(events[0].is_err());
        let events = run(Deflate::default(), server_frame(OP_TEXT, true, false, &[0xff, 0xfe])).await;
        assert!(events[0].is_err());
    }

    #[tokio::test]
    async fn oversized_frame_length_is_a_protocol_error_not_a_panic() {
        let mut wire = vec![0x81, 127];
        wire.extend_from_slice(&u64::MAX.to_be_bytes());
        let events = run(Deflate::default(), wire).await;
        assert!(events[0].as_ref().is_err_and(|e| e.kind() == io::ErrorKind::InvalidData));
    }

    #[test]
    fn extension_header_parsing() {
        assert_eq!(Deflate::from_response_header("").unwrap(), Deflate::default());
        let d = Deflate::from_response_header("permessage-deflate; server_no_context_takeover; client_no_context_takeover").unwrap();
        assert!(d.enabled && d.no_context_takeover);
        assert!(Deflate::from_response_header("x-webkit-deflate-frame").is_err());
    }
}
