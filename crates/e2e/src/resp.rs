//! Minimal RESP client for scenarios that talk the Redis protocol to the server port.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long one reply may take before the session is recorded as timed out.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// One RESP connection.
pub struct RespConn {
    reader: BufReader<TcpStream>,
}

/// Outcome of reading one reply.
pub enum Reply {
    Value(Value),
    Closed,
    Timeout,
}

impl RespConn {
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("connect timed out"))??;
        Ok(RespConn { reader: BufReader::new(stream) })
    }

    pub async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        // A peer that already closed makes the write fail; the following read reports it.
        let _ = self.reader.get_mut().write_all(bytes).await;
        Ok(())
    }

    pub async fn send_command(&mut self, args: &[String]) -> Result<()> {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n{}\r\n", a.len(), a).as_bytes());
        }
        self.send(&out).await
    }

    /// Reads one reply, rendered as JSON.
    pub async fn read(&mut self) -> Reply {
        match tokio::time::timeout(REPLY_TIMEOUT, read_value(&mut self.reader)).await {
            Err(_) => Reply::Timeout,
            Ok(Ok(Some(v))) => Reply::Value(v),
            Ok(Ok(None)) | Ok(Err(_)) => Reply::Closed,
        }
    }
}

async fn read_line(r: &mut BufReader<TcpStream>) -> Result<Option<String>> {
    let mut buf = String::new();
    if r.read_line(&mut buf).await? == 0 {
        return Ok(None);
    }
    Ok(Some(buf.trim_end_matches(['\r', '\n']).to_string()))
}

/// Bulk text as JSON when it parses (queue payloads), else a string.
fn bulk_value(bytes: &[u8]) -> Value {
    serde_json::from_slice::<Value>(bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

fn read_value<'a>(r: &'a mut BufReader<TcpStream>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Value>>> + Send + 'a>> {
    Box::pin(async move {
        let Some(line) = read_line(r).await? else { return Ok(None) };
        let (kind, rest) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
        Ok(Some(match kind {
            "+" => json!({"simple": rest}),
            "-" => json!({"error": rest}),
            ":" => json!({"int": rest.parse::<i64>().unwrap_or_default()}),
            "$" => {
                let n: i64 = rest.parse()?;
                if n < 0 {
                    json!({"nil": true})
                } else {
                    let mut body = vec![0u8; n as usize + 2];
                    r.read_exact(&mut body).await?;
                    body.truncate(n as usize);
                    bulk_value(&body)
                }
            }
            "*" => {
                let n: i64 = rest.parse()?;
                if n < 0 {
                    json!({"nil": true})
                } else {
                    let mut items = vec![];
                    for _ in 0..n {
                        match read_value(r).await? {
                            Some(v) => items.push(v),
                            None => return Ok(None),
                        }
                    }
                    Value::Array(items)
                }
            }
            other => json!({"unexpected_prefix": other}),
        }))
    })
}
