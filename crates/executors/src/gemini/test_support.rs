//! Test helpers: a tiny HTTP/1.1 upstream that records requests and replays canned replies, and
//! constructors for executors with a fixed config.

use std::collections::HashMap;
use std::sync::Arc;

use cpa_auth::Auth;
use cpa_config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::ConfigRx;

/// One request the mock upstream saw.
pub(crate) struct Captured {
    pub target: String,
    /// Lower-cased header names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Captured {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }

    pub fn json(&self) -> cpa_json::Value {
        cpa_json::parse(&self.body)
    }
}

/// A canned reply.
pub(crate) struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

pub(crate) fn json_reply(body: &str) -> Reply {
    Reply { status: 200, content_type: "application/json", body: body.to_string() }
}

pub(crate) fn sse_reply(body: &str) -> Reply {
    Reply { status: 200, content_type: "text/event-stream", body: body.to_string() }
}

/// Starts the mock; replies are served in order (the last one repeats). Returns the base URL and
/// the stream of captured requests.
pub(crate) async fn mock_upstream(replies: Vec<Reply>) -> (String, mpsc::UnboundedReceiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut served = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let reply = &replies[served.min(replies.len() - 1)];
            served += 1;
            let mut buf = Vec::new();
            let head_end = loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.lines();
            let target = lines.next().unwrap_or("").split_whitespace().nth(1).unwrap_or("").to_string();
            let headers: HashMap<String, String> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                .collect();
            let want: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            while buf.len() < head_end + want {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = buf[head_end..].to_vec();
            let _ = tx.send(Captured { target, headers, body });
            let response = format!(
                "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                reply.status,
                reply.content_type,
                reply.body.len(),
                reply.body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (base, rx)
}

/// A config handle with defaults (the sender is leaked so the receiver stays valid).
pub(crate) fn config_rx() -> ConfigRx {
    config_rx_with(Config::default())
}

pub(crate) fn config_rx_with(cfg: Config) -> ConfigRx {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    std::mem::forget(tx);
    rx
}

/// An API-key auth for `provider` pointing at `base_url`.
pub(crate) fn key_auth(provider: &str, base_url: &str) -> Auth {
    let mut auth = Auth::new("test-auth", provider);
    auth.attributes.insert("api_key".into(), "test-key".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    auth
}
