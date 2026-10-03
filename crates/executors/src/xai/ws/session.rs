//! Websocket sessions: the process-wide store keyed by execution session id, one upstream
//! connection per session, request serialization and invalidation (Go: codex_websockets_session.go, shared by the xAI executor; xAI keeps its own global store).
//!
//! The pool key is the client-side execution session, not the credential: a session reuses its
//! connection only while `(auth id, ws url, proxy url)` stay the same.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use http::HeaderMap;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use super::conn::{CloseInfo, Read, ReadError, WsConn, spawn_conn};
use super::transport::{DialFailure, Dialed};

/// Frames buffered per active request (Go: 4096).
const READ_BUFFER: usize = 4096;

static GLOBAL_STORE: LazyLock<Mutex<HashMap<String, Arc<Session>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

static NEXT_ACTIVE_GEN: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct ConnSlot {
    conn: Option<Arc<WsConn>>,
    ws_url: String,
    auth_id: String,
    proxy_url: String,
}

struct Active {
    conn_id: u64,
    generation: u64,
    tx: mpsc::Sender<Read>,
}

/// One execution session (or an ephemeral per-request one with an empty id).
pub struct Session {
    pub id: String,
    /// Serializes requests on the session; held from request start to stream completion.
    pub req_mu: Arc<tokio::sync::Mutex<()>>,
    slot: Mutex<ConnSlot>,
    active: Mutex<Option<Active>>,
    disconnect_notified: AtomicBool,
    disconnect_tx: watch::Sender<Option<String>>,
}

impl Session {
    fn new(id: &str) -> Arc<Session> {
        let (disconnect_tx, _) = watch::channel(None);
        Arc::new(Session {
            id: id.to_string(),
            req_mu: Arc::new(tokio::sync::Mutex::new(())),
            slot: Mutex::new(ConnSlot::default()),
            active: Mutex::new(None),
            disconnect_notified: AtomicBool::new(false),
            disconnect_tx,
        })
    }

    /// A one-request session that is closed when the request ends.
    pub fn ephemeral() -> Arc<Session> {
        Session::new("")
    }

    /// The stored session for `id`, created on first use.
    pub fn get_or_create(id: &str) -> Option<Arc<Session>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let mut store = GLOBAL_STORE.lock();
        Some(Arc::clone(store.entry(id.to_string()).or_insert_with(|| Session::new(id))))
    }

    pub fn is_ephemeral(&self) -> bool {
        self.id.is_empty()
    }

    pub fn kind(&self) -> &'static str {
        if self.is_ephemeral() { "ephemeral" } else { "persistent" }
    }

    /// Receiver that turns `Some(error text)` once the upstream connection dropped unexpectedly.
    pub fn disconnect_receiver(&self) -> watch::Receiver<Option<String>> {
        self.disconnect_tx.subscribe()
    }

    fn notify_upstream_disconnect(&self, err: &str) {
        if !self.disconnect_notified.swap(true, Ordering::AcqRel) {
            self.disconnect_tx.send_replace(Some(err.to_string()));
        }
    }

    // ---- active request routing

    /// Starts routing the connection's frames to a fresh channel, replacing any previous one.
    pub fn activate(&self, conn: &WsConn) -> (u64, mpsc::Receiver<Read>) {
        let (tx, rx) = mpsc::channel(READ_BUFFER);
        let generation = NEXT_ACTIVE_GEN.fetch_add(1, Ordering::Relaxed);
        *self.active.lock() = Some(Active { conn_id: conn.id, generation, tx });
        (generation, rx)
    }

    /// Stops routing for the activation `generation` of `conn_id`; false when it was replaced.
    pub fn clear_active(&self, conn_id: u64, generation: u64) -> bool {
        let mut active = self.active.lock();
        match &*active {
            Some(a) if a.conn_id == conn_id && a.generation == generation => {
                *active = None;
                true
            }
            _ => false,
        }
    }

    fn active_sender(&self, conn_id: u64) -> Option<mpsc::Sender<Read>> {
        self.active.lock().as_ref().filter(|a| a.conn_id == conn_id).map(|a| a.tx.clone())
    }

    /// Hands a frame to the active request; frames with no active request are dropped.
    pub async fn deliver(&self, conn_id: u64, read: Read) {
        if let Some(tx) = self.active_sender(conn_id) {
            let _ = tx.send(read).await;
        }
    }

    /// Hands a terminal error to the active request of `conn_id` without waiting and stops routing,
    /// so a connection closed locally never strands its in-flight request. A full buffer still
    /// ends the request: dropping the sender makes the receiver drain and then see the channel close.
    pub fn fail_active(&self, conn_id: u64, error: ReadError) {
        let active = {
            let mut active = self.active.lock();
            if active.as_ref().is_some_and(|a| a.conn_id == conn_id) { active.take() } else { None }
        };
        if let Some(active) = active {
            let _ = active.tx.try_send(Read::Err(error));
        }
    }

    /// Delivers a terminal read error to the active request, then invalidates the connection.
    pub async fn deliver_terminal(self: &Arc<Self>, conn: &Arc<WsConn>, error: ReadError, reason: &str) {
        let text = error.to_string();
        self.deliver(conn.id, Read::Err(error)).await;
        let generation = self.active.lock().as_ref().filter(|a| a.conn_id == conn.id).map(|a| a.generation);
        if let Some(generation) = generation {
            self.clear_active(conn.id, generation);
        }
        self.invalidate(conn, reason, Some(&text), true);
    }

    // ---- connection slot

    /// Whether the session already dialed a different target (Go: websocketSessionTargetChanged).
    pub fn target_changed(&self, auth_id: &str, ws_url: &str, proxy_url: &str) -> bool {
        let slot = self.slot.lock();
        if slot.auth_id.trim().is_empty() && slot.ws_url.trim().is_empty() {
            return false;
        }
        !target_matches(&slot, auth_id, ws_url, proxy_url)
    }

    /// The live connection when it matches the target and has seen no close frame (Go:
    /// existingWebsocketSessionConn).
    pub fn existing_conn(&self, auth_id: &str, ws_url: &str, proxy_url: &str) -> Option<Arc<WsConn>> {
        let slot = self.slot.lock();
        let conn = slot.conn.clone()?;
        if !target_matches(&slot, auth_id, ws_url, proxy_url) || conn.disconnect_error().is_some() {
            return None;
        }
        Some(conn)
    }

    /// Reuses the session's connection when the target is unchanged, else (re)dials via `dial`.
    /// Returns the connection and the handshake response headers (`None` when reused).
    pub async fn ensure_conn<F, Fut>(
        self: &Arc<Self>,
        auth_id: &str,
        ws_url: &str,
        proxy_url: &str,
        dial: F,
    ) -> Result<(Arc<WsConn>, Option<HeaderMap>), DialFailure>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Dialed, DialFailure>>,
    {
        let stale = self.detach_mismatched(auth_id, ws_url, proxy_url);
        if let Some((conn, previous_auth, previous_url)) = stale {
            tracing::info!(
                "xai websockets: upstream disconnected session={} auth={previous_auth} url={previous_url} session_object={} reason=target_changed last_event={}",
                self.id,
                self.kind(),
                conn.last_event_type()
            );
            conn.close();
        }
        if let Some(conn) = self.slot.lock().conn.clone() {
            tracing::info!("xai websockets: upstream connected session={} auth={auth_id} url={ws_url} session_object={} reused=true", self.id, self.kind());
            return Ok((conn, None));
        }
        let mut dialed = dial().await?;
        let response_headers = std::mem::take(&mut dialed.response_headers);
        let conn = spawn_conn(dialed, Arc::clone(self));
        {
            let mut slot = self.slot.lock();
            if let Some(existing) = slot.conn.clone() {
                drop(slot);
                conn.close();
                return Ok((existing, None));
            }
            slot.conn = Some(Arc::clone(&conn));
            slot.ws_url = ws_url.to_string();
            slot.auth_id = auth_id.to_string();
            slot.proxy_url = proxy_url.to_string();
        }
        tracing::info!("xai websockets: upstream connected session={} auth={auth_id} url={ws_url} session_object={} reused=false", self.id, self.kind());
        Ok((conn, Some(response_headers)))
    }

    fn detach_mismatched(&self, auth_id: &str, ws_url: &str, proxy_url: &str) -> Option<(Arc<WsConn>, String, String)> {
        let mut slot = self.slot.lock();
        let conn = slot.conn.clone()?;
        if target_matches(&slot, auth_id, ws_url, proxy_url) {
            return None;
        }
        let previous = (conn, slot.auth_id.clone(), slot.ws_url.clone());
        slot.conn = None;
        Some(previous)
    }

    /// Drops `conn` from the slot and closes it; optionally tells the downstream handler (Go:
    /// invalidateUpstreamConnWithNotify). A stale `conn` is ignored.
    pub fn invalidate(&self, conn: &Arc<WsConn>, reason: &str, err: Option<&str>, notify: bool) {
        let (auth_id, ws_url) = {
            let mut slot = self.slot.lock();
            if !slot.conn.as_ref().is_some_and(|c| c.id == conn.id) {
                return;
            }
            slot.conn = None;
            (slot.auth_id.clone(), slot.ws_url.clone())
        };
        let last_event = conn.last_event_type();
        tracing::info!(
            "xai websockets: upstream disconnected session={} auth={auth_id} url={ws_url} session_object={} reason={reason} last_event={last_event} is_terminal={} err={}",
            self.id,
            self.kind(),
            is_terminal_event(&last_event),
            err.unwrap_or("")
        );
        if notify {
            self.notify_upstream_disconnect(err.unwrap_or(reason));
        }
        conn.close();
    }

    /// Closes the session's connection (Go: closeCodexWebsocketSession).
    pub fn close(&self, reason: &str) {
        let (conn, auth_id, ws_url) = {
            let mut slot = self.slot.lock();
            let conn = slot.conn.take();
            (conn, slot.auth_id.clone(), slot.ws_url.clone())
        };
        if let Some(conn) = conn {
            tracing::info!(
                "xai websockets: upstream disconnected session={} auth={auth_id} url={ws_url} session_object={} reason={reason} last_event={}",
                self.id,
                self.kind(),
                conn.last_event_type()
            );
            conn.close();
        }
    }

    /// The close frame the upstream sent on `conn`, mapped to a close code for write errors.
    pub fn upstream_close(&self, conn: &WsConn) -> Option<CloseInfo> {
        conn.disconnect_error()
    }

    fn auth_id(&self) -> String {
        self.slot.lock().auth_id.trim().to_string()
    }
}

fn target_matches(slot: &ConnSlot, auth_id: &str, ws_url: &str, proxy_url: &str) -> bool {
    slot.auth_id.trim() == auth_id.trim() && slot.ws_url.trim() == ws_url.trim() && slot.proxy_url.trim() == proxy_url.trim()
}

fn is_terminal_event(event_type: &str) -> bool {
    matches!(event_type, "response.completed" | "response.done" | "response.incomplete" | "response.failed" | "error")
}

/// Releases a stored session and its connection (client session ended). The special id `*`
/// closes every session (Go: CloseAllExecutionSessionsID).
pub fn close_execution_session(session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    if session_id == "*" {
        let sessions: Vec<Arc<Session>> = GLOBAL_STORE.lock().drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.close("executor_shutdown");
        }
        return;
    }
    let session = GLOBAL_STORE.lock().remove(session_id);
    super::ids::delete_state(session_id);
    if let Some(session) = session {
        session.close("session_closed");
    }
}

/// Closes every session whose connection belongs to `auth_id` (credential removed).
pub fn close_sessions_for_auth_id(auth_id: &str, reason: &str) {
    let auth_id = auth_id.trim();
    if auth_id.is_empty() {
        return;
    }
    let reason = if reason.trim().is_empty() { "auth_removed" } else { reason.trim() };
    let matches: Vec<Arc<Session>> = {
        let mut store = GLOBAL_STORE.lock();
        let ids: Vec<String> = store.iter().filter(|(_, s)| s.auth_id() == auth_id).map(|(id, _)| id.clone()).collect();
        ids.iter()
            .filter_map(|id| {
                super::ids::delete_state(id);
                store.remove(id)
            })
            .collect()
    };
    for session in matches {
        session.close(reason);
    }
}

/// Receiver of the disconnect notification of an execution session (created on demand).
pub fn upstream_disconnect_receiver(session_id: &str) -> Option<watch::Receiver<Option<String>>> {
    Session::get_or_create(session_id).map(|s| s.disconnect_receiver())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::codec::{Deflate, split};

    async fn dialed_session() -> (Arc<Session>, Arc<WsConn>, tokio::io::DuplexStream) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (reader, writer) = split(Box::new(client), &[], Deflate::default());
        let session = Session::ephemeral();
        let dialed = Dialed { reader, writer, response_headers: HeaderMap::new() };
        let (conn, _) = session.ensure_conn("a", "wss://x", "", || async move { Ok(dialed) }).await.unwrap();
        (session, conn, server)
    }

    #[tokio::test]
    async fn closing_the_session_ends_the_in_flight_request() {
        let (session, conn, _server) = dialed_session().await;
        let (_generation, mut rx) = session.activate(&conn);
        session.close("test");
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("request stranded");
        assert!(matches!(read, Some(Read::Err(_))));
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("sender kept").is_none());
    }

    #[tokio::test]
    async fn invalidating_the_connection_ends_the_in_flight_request() {
        let (session, conn, _server) = dialed_session().await;
        let (_generation, mut rx) = session.activate(&conn);
        session.invalidate(&conn, "test", None, false);
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("request stranded");
        assert!(matches!(read, Some(Read::Err(_))));
    }
}
