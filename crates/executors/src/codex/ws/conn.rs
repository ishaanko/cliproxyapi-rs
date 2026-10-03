//! One upstream websocket connection: a reader task feeds received frames to the session's active
//! request and has pings answered; writes are serialized through a mutex (Go: `readUpstreamLoop`, the
//! ping/close handlers of `configureConn` and `writeMessage`).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;

use super::codec::{Incoming, WsReader, WsWriter};
use super::session::Session;
use super::transport::Dialed;

/// A connection with no frame for this long is dropped (Go: codexResponsesWebsocketIdleTimeout).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Read error text for a binary frame (Go: the executors reject binary messages).
pub const UNEXPECTED_BINARY: &str = "codex websockets executor: unexpected binary message";

/// Close code for "message too big".
pub const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// A peer close frame (Go: `*websocket.CloseError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseInfo {
    pub code: u16,
    pub text: String,
}

impl fmt::Display for CloseInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "websocket: close {}", self.code)?;
        if !self.text.is_empty() {
            write!(f, ": {}", self.text)?;
        }
        Ok(())
    }
}

/// Why reading from the upstream failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Close(CloseInfo),
    Other(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Close(c) => c.fmt(f),
            ReadError::Other(s) => f.write_str(s),
        }
    }
}

/// A frame (or terminal error) delivered to the active request.
#[derive(Debug)]
pub enum Read {
    Text(Vec<u8>),
    Err(ReadError),
}

/// Handle to a live connection; clones of the `Arc` identify it by `id`.
pub struct WsConn {
    pub id: u64,
    writer: tokio::sync::Mutex<WsWriter>,
    shutdown: watch::Sender<bool>,
    /// The peer's close frame, once seen (Go: upstreamDisconnectError).
    close: Mutex<Option<CloseInfo>>,
    last_event: Mutex<String>,
}

impl WsConn {
    /// Sends one text frame; writes are serialized and complete before the call returns.
    pub async fn write_text(&self, payload: Vec<u8>) -> Result<(), String> {
        self.writer.lock().await.send_text(&payload).await.map_err(|e| e.to_string())
    }

    /// Closes the underlying socket (idempotent).
    pub fn close(&self) {
        let _ = self.shutdown.send(true);
    }

    pub fn disconnect_error(&self) -> Option<CloseInfo> {
        self.close.lock().clone()
    }

    pub fn last_event_type(&self) -> String {
        self.last_event.lock().clone()
    }
}

/// Starts the reader task of a dialed connection bound to `session`.
pub fn spawn_conn(dialed: Dialed, session: Arc<Session>) -> Arc<WsConn> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let conn = Arc::new(WsConn {
        id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
        writer: tokio::sync::Mutex::new(dialed.writer),
        shutdown: shutdown_tx,
        close: Mutex::new(None),
        last_event: Mutex::new(String::new()),
    });
    tokio::spawn(run(dialed.reader, Arc::clone(&conn), session, shutdown_rx));
    conn
}

async fn run(mut reader: WsReader, conn: Arc<WsConn>, session: Arc<Session>, mut shutdown: watch::Receiver<bool>) {
    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // Closed locally (invalidate/close): the active request must still get a terminal
                // read instead of waiting forever (Go: ReadMessage fails on the closed socket).
                session.fail_active(conn.id, ReadError::Other("read tcp: use of closed network connection".to_string()));
                break;
            }
            event = tokio::time::timeout(IDLE_TIMEOUT, reader.read_event()) => event,
        };
        let error = match event {
            Err(_) => ReadError::Other("read tcp: i/o timeout".to_string()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => ReadError::Close(CloseInfo { code: 1006, text: "unexpected EOF".to_string() }),
            Ok(Err(e)) => ReadError::Other(e.to_string()),
            Ok(Ok(Incoming::Text(text))) => {
                let payload = text.trim_ascii().to_vec();
                if !payload.is_empty() {
                    let event_type = event_type(&payload);
                    if !event_type.is_empty() {
                        *conn.last_event.lock() = event_type;
                    }
                }
                session.deliver(conn.id, Read::Text(payload)).await;
                continue;
            }
            Ok(Ok(Incoming::Ping(payload))) => {
                // Answered from a task so the reader never waits on a request that is mid-write.
                let conn = Arc::clone(&conn);
                tokio::spawn(async move {
                    let _ = conn.writer.lock().await.send_pong(&payload).await;
                });
                continue;
            }
            Ok(Ok(Incoming::Binary)) => {
                session.deliver_terminal(&conn, ReadError::Other(UNEXPECTED_BINARY.to_string()), "unexpected_binary").await;
                break;
            }
            Ok(Ok(Incoming::Close { code, reason })) => {
                let info = CloseInfo { code, text: reason };
                *conn.close.lock() = Some(info.clone());
                ReadError::Close(info)
            }
        };
        session.deliver_terminal(&conn, error, "upstream_disconnected").await;
        break;
    }
    conn.writer.lock().await.shutdown().await;
}

/// `type` of a JSON event, "" when absent.
fn event_type(payload: &[u8]) -> String {
    cpa_json::raw_at(payload, "type").and_then(|raw| serde_json::from_str::<String>(raw).ok()).unwrap_or_default()
}
