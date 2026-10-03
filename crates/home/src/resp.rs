//! RESP codec shared by the Home client, the usage-queue server and test peers.
//!
//! Client side: [`Value`], [`encode_command`] and [`read_value`] (RESP2, tolerating the RESP3
//! null/push/map shapes a Redis-compatible peer may emit). Server side (Go:
//! `internal/api/redis_queue_protocol.go`): [`read_command`] parses a request array the way Go's
//! `readRESPArray` does, and the `write_*` helpers append replies to a byte buffer.

use std::io;

use bytes::Bytes;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// One decoded RESP reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `$-1` / `*-1` / `_`.
    Nil,
    Simple(String),
    /// `-ERR ...` payload without the leading `-`.
    Error(String),
    Int(i64),
    Bulk(Bytes),
    Array(Vec<Value>),
}

impl Value {
    /// Text of a bulk or simple string.
    pub fn text(&self) -> Option<String> {
        match self {
            Value::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Simple(s) => Some(s.clone()),
            _ => None,
        }
    }
}

/// Failure to read a request or reply.
#[derive(Debug, thiserror::Error)]
pub enum RespError {
    /// The peer closed the connection at a message boundary (Go: `io.EOF`).
    #[error("EOF")]
    Eof,
    /// Malformed framing.
    #[error("protocol error")]
    Protocol,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl RespError {
    fn from_read_exact(e: io::Error, read_any: bool) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            if read_any {
                RespError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF"))
            } else {
                RespError::Eof
            }
        } else {
            RespError::Io(e)
        }
    }
}

/// Encodes `args` as a RESP request array of bulk strings.
pub fn encode_command<A: AsRef<[u8]>>(args: &[A]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + args.iter().map(|a| a.as_ref().len() + 16).sum::<usize>());
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for a in args {
        let a = a.as_ref();
        out.push(b'$');
        out.extend_from_slice(a.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Longest header line accepted (Redis caps inline requests at 64 KiB).
const MAX_LINE_LEN: usize = 64 * 1024;
/// Largest bulk payload accepted (Redis `proto-max-bulk-len` default).
const MAX_BULK_LEN: usize = 512 * 1024 * 1024;
/// Largest element count accepted for one array/map.
const MAX_ARRAY_LEN: usize = 1024 * 1024;

/// Reads a line without its `\r\n` (or bare `\n`), at most [`MAX_LINE_LEN`] bytes. A line cut
/// short by EOF is [`RespError::Eof`]; an over-long line is [`RespError::Protocol`].
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<String, RespError> {
    let mut buf = Vec::new();
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Err(RespError::Eof);
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&avail[..=i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = avail.len();
                buf.extend_from_slice(avail);
                r.consume(n);
            }
        }
        if buf.len() > MAX_LINE_LEN {
            return Err(RespError::Protocol);
        }
    }
    if buf.len() > MAX_LINE_LEN {
        return Err(RespError::Protocol);
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parses a bulk length header; `None` for the null bulk (negative), `Protocol` over the cap.
fn bulk_len(n: i64) -> Result<Option<usize>, RespError> {
    if n < 0 {
        return Ok(None);
    }
    usize::try_from(n).ok().filter(|&n| n <= MAX_BULK_LEN).map(Some).ok_or(RespError::Protocol)
}

/// Parses an element count header; `Protocol` over [`MAX_ARRAY_LEN`].
fn array_len(n: i64) -> Result<usize, RespError> {
    usize::try_from(n).ok().filter(|&n| n <= MAX_ARRAY_LEN).ok_or(RespError::Protocol)
}

async fn read_exact_body<R: AsyncBufRead + Unpin>(r: &mut R, len: usize) -> Result<Vec<u8>, RespError> {
    // `len + 2` bytes: payload plus CRLF. Read in bounded steps so a bogus length cannot
    // reserve memory ahead of the data.
    let total = len.checked_add(2).ok_or(RespError::Protocol)?;
    let mut buf = Vec::with_capacity(total.min(64 * 1024));
    let mut remaining = total;
    let mut chunk = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let n = r.read(&mut chunk[..want]).await?;
        if n == 0 {
            return Err(RespError::from_read_exact(
                io::Error::from(io::ErrorKind::UnexpectedEof),
                !buf.is_empty(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        remaining -= n;
    }
    Ok(buf)
}

/// Reads one reply (client side).
pub async fn read_value<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Value, RespError> {
    Box::pin(read_value_inner(r)).await
}

async fn read_value_inner<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Value, RespError> {
    let prefix = r.read_u8().await.map_err(|e| RespError::from_read_exact(e, false))?;
    match prefix {
        b'+' => Ok(Value::Simple(read_line(r).await?)),
        b'-' => Ok(Value::Error(read_line(r).await?)),
        b':' => read_line(r)
            .await?
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| RespError::Protocol),
        b'$' | b'=' => {
            let n: i64 = read_line(r).await?.trim().parse().map_err(|_| RespError::Protocol)?;
            let Some(n) = bulk_len(n)? else {
                return Ok(Value::Nil);
            };
            let mut body = read_exact_body(r, n).await?;
            if body.len() < 2 || body[body.len() - 2..] != *b"\r\n" {
                return Err(RespError::Protocol);
            }
            body.truncate(body.len() - 2);
            Ok(Value::Bulk(Bytes::from(body)))
        }
        b'*' | b'>' | b'~' => {
            let n: i64 = read_line(r).await?.trim().parse().map_err(|_| RespError::Protocol)?;
            if n < 0 {
                return Ok(Value::Nil);
            }
            let n = array_len(n)?;
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                items.push(Box::pin(read_value_inner(r)).await?);
            }
            Ok(Value::Array(items))
        }
        b'%' => {
            let n: i64 = read_line(r).await?.trim().parse().map_err(|_| RespError::Protocol)?;
            let mut items = Vec::new();
            for _ in 0..array_len(n.max(0))? * 2 {
                items.push(Box::pin(read_value_inner(r)).await?);
            }
            Ok(Value::Array(items))
        }
        b'_' => {
            read_line(r).await?;
            Ok(Value::Nil)
        }
        b'#' => Ok(Value::Int(i64::from(read_line(r).await? == "t"))),
        b',' | b'(' => Ok(Value::Simple(read_line(r).await?)),
        b'!' => {
            let n: i64 = read_line(r).await?.trim().parse().map_err(|_| RespError::Protocol)?;
            let mut body = read_exact_body(r, bulk_len(n.max(0))?.unwrap_or(0)).await?;
            body.truncate(body.len().saturating_sub(2));
            Ok(Value::Error(String::from_utf8_lossy(&body).into_owned()))
        }
        _ => Err(RespError::Protocol),
    }
}

// ---------------------------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------------------------

/// True for the first bytes of a RESP message (Go: `isRedisRESPPrefix`); HTTP requests never
/// start with one of these.
pub fn is_resp_prefix(b: u8) -> bool {
    matches!(b, b'*' | b'$' | b'+' | b'-' | b':')
}

/// Reads one request array (Go: `readRESPArray`). Elements may be bulk strings, simple strings
/// or integers; a null bulk string reads as an empty string.
pub async fn read_command<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Vec<Bytes>, RespError> {
    let prefix = r.read_u8().await.map_err(|e| RespError::from_read_exact(e, false))?;
    if prefix != b'*' {
        return Err(RespError::Protocol);
    }
    let count: i64 = read_line(r).await?.parse().map_err(|_| RespError::Protocol)?;
    if count < 0 {
        return Err(RespError::Protocol);
    }
    let count = array_len(count)?;
    let mut args = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let p = r.read_u8().await.map_err(|e| RespError::from_read_exact(e, false))?;
        match p {
            b'$' => {
                let len: i64 = read_line(r).await?.parse().map_err(|_| RespError::Protocol)?;
                let Some(len) = bulk_len(len)? else {
                    args.push(Bytes::new());
                    continue;
                };
                let mut body = read_exact_body(r, len).await?;
                if body[body.len() - 2..] != *b"\r\n" {
                    return Err(RespError::Protocol);
                }
                body.truncate(body.len() - 2);
                args.push(Bytes::from(body));
            }
            b'+' | b':' => args.push(Bytes::from(read_line(r).await?.into_bytes())),
            _ => return Err(RespError::Protocol),
        }
    }
    Ok(args)
}

pub fn write_simple(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(format!("+{value}\r\n").as_bytes());
}

pub fn write_error(out: &mut Vec<u8>, message: &str) {
    out.extend_from_slice(format!("-{message}\r\n").as_bytes());
}

pub fn write_nil(out: &mut Vec<u8>) {
    out.extend_from_slice(b"$-1\r\n");
}

pub fn write_integer(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(format!(":{value}\r\n").as_bytes());
}

pub fn write_array_header(out: &mut Vec<u8>, count: usize) {
    out.extend_from_slice(format!("*{count}\r\n").as_bytes());
}

pub fn write_bulk(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
}

pub fn write_bulk_array<T: AsRef<[u8]>>(out: &mut Vec<u8>, items: &[T]) {
    write_array_header(out, items.len());
    for item in items {
        write_bulk(out, item.as_ref());
    }
}

/// `["subscribe", channel, count]`.
pub fn write_pubsub_subscribe(out: &mut Vec<u8>, channel: &str, count: i64) {
    write_array_header(out, 3);
    write_bulk(out, b"subscribe");
    write_bulk(out, channel.as_bytes());
    write_integer(out, count);
}

/// `["unsubscribe", channel, count]`.
pub fn write_pubsub_unsubscribe(out: &mut Vec<u8>, channel: &str, count: i64) {
    write_array_header(out, 3);
    write_bulk(out, b"unsubscribe");
    write_bulk(out, channel.as_bytes());
    write_integer(out, count);
}

/// `["message", channel, payload]`.
pub fn write_pubsub_message(out: &mut Vec<u8>, channel: &str, payload: &[u8]) {
    write_array_header(out, 3);
    write_bulk(out, b"message");
    write_bulk(out, channel.as_bytes());
    write_bulk(out, payload);
}

/// `["pong", payload]`.
pub fn write_pubsub_pong(out: &mut Vec<u8>, payload: &[u8]) {
    write_array_header(out, 2);
    write_bulk(out, b"pong");
    write_bulk(out, payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn cmd(input: &[u8]) -> Result<Vec<Bytes>, RespError> {
        read_command(&mut BufReader::new(input)).await
    }

    #[tokio::test]
    async fn read_command_accepts_bulk_simple_and_integer_elements() {
        let got = cmd(b"*4\r\n$4\r\nAUTH\r\n+pw\r\n:7\r\n$-1\r\n").await.unwrap();
        let got: Vec<&[u8]> = got.iter().map(|b| &b[..]).collect();
        assert_eq!(got, vec![&b"AUTH"[..], b"pw", b"7", b""]);
    }

    #[tokio::test]
    async fn read_command_error_classes_match_go() {
        assert!(matches!(cmd(b"").await, Err(RespError::Eof)));
        assert!(matches!(cmd(b"PING\r\n").await, Err(RespError::Protocol)));
        assert!(matches!(cmd(b"*x\r\n").await, Err(RespError::Protocol)));
        assert!(matches!(cmd(b"*-1\r\n").await, Err(RespError::Protocol)));
        assert!(matches!(cmd(b"*1\r\n$3\r\nabXX\r\n").await, Err(RespError::Protocol)));
        assert!(matches!(cmd(b"*1\r\n!3\r\n").await, Err(RespError::Protocol)));
        // Cut short inside a bulk body: not a clean EOF.
        match cmd(b"*1\r\n$5\r\nab").await {
            Err(RespError::Io(e)) => assert_eq!(e.to_string(), "unexpected EOF"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(cmd(b"*1\r\n").await, Err(RespError::Eof)));
    }

    #[tokio::test]
    async fn read_value_decodes_resp2_replies() {
        let mut r = BufReader::new(&b"+OK\r\n-ERR boom\r\n:42\r\n$3\r\nabc\r\n$-1\r\n*2\r\n$1\r\na\r\n:1\r\n"[..]);
        assert_eq!(read_value(&mut r).await.unwrap(), Value::Simple("OK".into()));
        assert_eq!(read_value(&mut r).await.unwrap(), Value::Error("ERR boom".into()));
        assert_eq!(read_value(&mut r).await.unwrap(), Value::Int(42));
        assert_eq!(read_value(&mut r).await.unwrap(), Value::Bulk(Bytes::from_static(b"abc")));
        assert_eq!(read_value(&mut r).await.unwrap(), Value::Nil);
        assert_eq!(
            read_value(&mut r).await.unwrap(),
            Value::Array(vec![Value::Bulk(Bytes::from_static(b"a")), Value::Int(1)])
        );
    }

    #[test]
    fn writers_produce_resp_frames() {
        let mut out = vec![];
        write_pubsub_message(&mut out, "usage", b"{}");
        write_pubsub_subscribe(&mut out, "usage", 1);
        write_pubsub_pong(&mut out, b"");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "*3\r\n$7\r\nmessage\r\n$5\r\nusage\r\n$2\r\n{}\r\n*3\r\n$9\r\nsubscribe\r\n$5\r\nusage\r\n:1\r\n*2\r\n$4\r\npong\r\n$0\r\n\r\n"
        );
        assert_eq!(encode_command(&["GET", "k"]), b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n");
    }
}
