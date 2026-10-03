//! Sideband relay for existing calls and the shared websocket relay (Go: sideband.go).

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use cpa_executors::helps::logging::UpstreamRequestLog;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue};
use tokio::sync::{Mutex, watch};
use tokio_tungstenite::tungstenite::Message as UpMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as UpCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::call::auth_account_type;
use crate::reply::{Reply, content_type_of, live_error, realtime_error};
use crate::session::{Claim, LiveSession, SessionStore};
use crate::upstream::{call_response_headers, copy_handshake_headers, headers_for_logging, prepare_request_headers, protocol_headers, set_account_header};
use crate::util::is_call_id;
use crate::ws_client::{self, DialFailure, Dialed, UpstreamStream};
use crate::{Caller, Handler, RequestParts, live_selection_headers, proxy_url_for_auth, selection_error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebandStyle {
    Frameless,
    RealtimeCalls,
    RealtimeQuery,
}

/// `websocket.IsWebSocketUpgrade`.
pub(crate) fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let contains = |name: &str, token: &str| {
        headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    contains("connection", "upgrade") && contains("upgrade", "websocket")
}

/// Client-requested subprotocols (`websocket.Subprotocols`).
pub(crate) fn requested_subprotocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// `sidebandTarget`.
fn sideband_target(parts: &RequestParts) -> (SidebandStyle, String, bool) {
    if let Some(call_id) = parts.call_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        let style = if parts.path.contains("/realtime/calls/") { SidebandStyle::RealtimeCalls } else { SidebandStyle::Frameless };
        return (style, call_id.to_string(), is_call_id(call_id));
    }
    let call_id = parts.query_first("call_id").unwrap_or("").trim().to_string();
    let ok = is_call_id(&call_id);
    (SidebandStyle::RealtimeQuery, call_id, ok)
}

/// `buildSidebandURL`.
pub(crate) fn build_sideband_url(base: &str, style: SidebandStyle, call_id: &str) -> String {
    let root = base.trim_end_matches('/');
    match style {
        SidebandStyle::RealtimeCalls => format!("{root}/realtime/calls/{call_id}"),
        SidebandStyle::RealtimeQuery => {
            let encoded: String = url::form_urlencoded::byte_serialize(call_id.as_bytes()).collect();
            format!("{root}/realtime?intent=quicksilver&call_id={encoded}")
        }
        SidebandStyle::Frameless => format!("{root}/live/{call_id}"),
    }
}

/// gorilla's failed upgrade answer.
pub(crate) fn upgrade_failed() -> Response {
    let mut reply = Reply::new(400);
    reply.set("content-type", "text/plain; charset=utf-8");
    reply.set("sec-websocket-version", "13");
    reply.set("x-content-type-options", "nosniff");
    reply.body = bytes::Bytes::from_static(b"Bad Request\n");
    reply.into_response()
}

/// Stamps `Connection: Upgrade` (gorilla's casing) on a successful upgrade response.
pub(crate) fn finish_upgrade(mut resp: Response) -> Response {
    resp.headers_mut().insert(http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
    resp
}

/// Normal-closure family the relay does not report as an error.
fn close_details(end: &RelayEnd) -> (u16, String) {
    match end {
        RelayEnd::Close(code, text) if matches!(*code, 1005 | 1006 | 1015) => {
            let _ = text;
            (1000, String::new())
        }
        RelayEnd::Close(code, text) => (*code, text.clone()),
        RelayEnd::Eof => (1000, String::new()),
        RelayEnd::Failed => (1011, "relay closed".into()),
    }
}

#[derive(Debug)]
pub(crate) enum RelayEnd {
    /// A peer's close frame (or an abnormal one mapped to 1006).
    Close(u16, String),
    Eof,
    Failed,
}

fn up_close_code(code: CloseCode) -> u16 {
    u16::from(code)
}

/// `relayWebsockets`: copies messages both ways until either side ends, then closes both with
/// the first side's close details. `cancel` aborts the relay without close frames.
pub(crate) async fn relay_websockets(downstream: WebSocket, upstream: UpstreamStream, mut cancel: watch::Receiver<bool>) -> RelayEnd {
    let (down_tx, mut down_rx) = downstream.split();
    let (up_tx, mut up_rx) = upstream.split();
    let down_tx = Arc::new(Mutex::new(down_tx));
    let up_tx = Arc::new(Mutex::new(up_tx));

    let to_upstream = {
        let up_tx = up_tx.clone();
        async move {
            loop {
                match down_rx.next().await {
                    None => return RelayEnd::Close(1006, String::new()),
                    Some(Err(_)) => return RelayEnd::Close(1006, String::new()),
                    Some(Ok(Message::Text(text))) => {
                        if up_tx.lock().await.send(UpMessage::text(text.as_str().to_string())).await.is_err() {
                            return RelayEnd::Failed;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if up_tx.lock().await.send(UpMessage::binary(data)).await.is_err() {
                            return RelayEnd::Failed;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return match frame {
                            Some(f) => RelayEnd::Close(f.code, f.reason.as_str().to_string()),
                            None => RelayEnd::Close(1005, String::new()),
                        };
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    };
    let to_downstream = {
        let down_tx = down_tx.clone();
        async move {
            loop {
                match up_rx.next().await {
                    None => return RelayEnd::Close(1006, String::new()),
                    Some(Err(_)) => return RelayEnd::Close(1006, String::new()),
                    Some(Ok(UpMessage::Text(text))) => {
                        if down_tx.lock().await.send(Message::Text(text.as_str().into())).await.is_err() {
                            return RelayEnd::Failed;
                        }
                    }
                    Some(Ok(UpMessage::Binary(data))) => {
                        if down_tx.lock().await.send(Message::Binary(data)).await.is_err() {
                            return RelayEnd::Failed;
                        }
                    }
                    Some(Ok(UpMessage::Close(frame))) => {
                        return match frame {
                            Some(f) => RelayEnd::Close(up_close_code(f.code), f.reason.as_str().to_string()),
                            None => RelayEnd::Close(1005, String::new()),
                        };
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    };
    let end = tokio::select! {
        end = to_upstream => end,
        end = to_downstream => end,
        _ = async { while !*cancel.borrow() { if cancel.changed().await.is_err() { std::future::pending::<()>().await; } } } => {
            return RelayEnd::Eof;
        }
    };
    let (code, reason) = close_details(&end);
    let _ = down_tx.lock().await.send(Message::Close(Some(CloseFrame { code, reason: reason.clone().into() }))).await;
    let _ = up_tx
        .lock()
        .await
        .send(UpMessage::Close(Some(UpCloseFrame { code: CloseCode::from(code), reason: reason.into() })))
        .await;
    let _ = down_tx.lock().await.close().await;
    let _ = up_tx.lock().await.close().await;
    end
}

/// Releases or consumes a claimed session when dropped (`consumeSession` in Go).
pub(crate) struct ClaimGuard {
    store: SessionStore,
    session: LiveSession,
    pub(crate) consume: bool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.consume {
            self.store.complete(&self.session, "session_closed");
        } else {
            self.store.release(&self.session);
        }
    }
}

impl Handler {
    /// Dials an upstream websocket for the credential and logs the attempt.
    pub(crate) async fn dial_upstream(
        &self,
        caller: &Caller,
        selected: &cpa_auth::Auth,
        upstream_url: &str,
        mut headers: HeaderMap,
        client_headers: &HeaderMap,
    ) -> Result<Dialed, DialFailure> {
        set_account_header(&mut headers, selected);
        prepare_request_headers(selected, &mut headers);
        let cfg = self.cfg();
        if let Some(log) = caller.log() {
            let (auth_type, auth_value) = auth_account_type(selected);
            log.websocket_request(UpstreamRequestLog {
                url: upstream_url.to_string(),
                method: "WEBSOCKET".into(),
                headers: headers_for_logging(&headers),
                body: Vec::new(),
                provider: "codex".into(),
                auth_id: selected.id.clone(),
                auth_label: selected.label.clone(),
                auth_type,
                auth_value,
            });
        }
        ws_client::dial(upstream_url, &headers, &requested_subprotocols(client_headers), &proxy_url_for_auth(&cfg, selected)).await
    }

    /// `HandleSideband`: relays an existing call's sideband websocket.
    pub async fn handle_sideband(&self, caller: &Caller, parts: &RequestParts, ws: Option<WebSocketUpgrade>) -> Response {
        let path = parts.path.as_str();
        if !is_websocket_upgrade(&parts.headers) {
            let mut reply = live_error(path, 426, "WebSocket upgrade required");
            reply.set("upgrade", "websocket");
            return reply.into_response();
        }
        let (style, call_id, ok) = sideband_target(parts);
        if !ok {
            return live_error(path, 400, "Invalid Codex live call ID").into_response();
        }
        let (session, claim) = self.inner.sessions.claim(&call_id);
        match claim {
            Claim::Busy => return live_error(path, 409, "Codex live session already joining").into_response(),
            Claim::Acquired => {}
            Claim::Missing => return live_error(path, 404, "Codex live session not found").into_response(),
        }
        if let Some(secret) = &caller.client_secret {
            if session.client_secret_principal.is_empty() || secret.principal != session.client_secret_principal {
                self.inner.sessions.release(&session);
                return realtime_error(
                    403,
                    "Realtime client secret is not valid for this call",
                    "invalid_request_error",
                    "realtime_client_secret_scope_mismatch",
                )
                .into_response();
            }
        } else {
            let (principal, provider) = caller.owner();
            if !session.owner_principal.is_empty() && (principal != session.owner_principal || provider != session.owner_provider) {
                self.inner.sessions.release(&session);
                return realtime_error(
                    403,
                    "Realtime call belongs to another API principal",
                    "invalid_request_error",
                    "realtime_call_scope_mismatch",
                )
                .into_response();
            }
        }
        let guard = ClaimGuard { store: self.inner.sessions.clone(), session: session.clone(), consume: false };

        let mut selected = match self.select_oauth(&live_selection_headers(parts, caller), &[], Some((&session.auth_id, &call_id))) {
            Ok(a) => a,
            Err(e) => return selection_error(path, &e).into_response(),
        };
        caller.record_trace(&mut selected);

        let upstream_url = build_sideband_url(&self.inner.sideband_base.read(), style, &call_id);
        let dialed = match self.dial_upstream(caller, &selected, &upstream_url, protocol_headers(&parts.headers), &parts.headers).await {
            Ok(d) => d,
            Err(failure) => return sideband_dial_error(path, caller, failure),
        };
        if let Some(log) = caller.log() {
            log.websocket_handshake(dialed.status, &call_response_headers(&dialed.response_headers));
        }
        let Some(ws) = ws else {
            return upgrade_failed();
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        // Hangup (or expiry) of the call ends the relay without a close handshake.
        if let Some(resources) = &session.resources {
            resources.add(vec![Box::new(move || {
                let _ = cancel_tx.send(true);
            })]);
        }
        let subprotocol = dialed.subprotocol.clone();
        let log = caller.log.clone();
        let ws = match &subprotocol {
            Some(p) => ws.protocols([p.clone()]),
            None => ws,
        };
        let guard_slot = Arc::new(parking_lot::Mutex::new(Some(guard)));
        let failed_slot = guard_slot.clone();
        let resp = ws
            .max_message_size(usize::MAX)
            .max_frame_size(usize::MAX)
            .on_failed_upgrade(move |_| {
                failed_slot.lock().take();
            })
            .on_upgrade(move |socket| async move {
                let mut claim = guard_slot.lock().take();
                if let Some(c) = claim.as_mut() {
                    c.consume = true;
                }
                let end = relay_websockets(socket, dialed.stream, cancel_rx).await;
                if let (Some(log), RelayEnd::Failed) = (&log, &end) {
                    log.websocket_error("relay", "relay closed");
                }
                drop(claim);
            });
        finish_upgrade(resp)
    }
}

/// `handleSidebandDialError`.
fn sideband_dial_error(path: &str, caller: &Caller, failure: DialFailure) -> Response {
    let status = failure.status.unwrap_or(502);
    let mut headers = HeaderMap::new();
    if failure.status.is_some() {
        copy_handshake_headers(&mut headers, &failure.headers);
        if let Some(log) = caller.log() {
            log.websocket_handshake(status, &call_response_headers(&failure.headers));
            log.websocket_response(&failure.body);
        }
    }
    if let Some(log) = caller.log() {
        log.websocket_error("dial", &failure.error);
    }
    if failure.status == Some(401) {
        let mut reply = Reply::new(401);
        reply.headers = headers;
        if let Some(ct) = content_type_of(&failure.headers) {
            reply.set("content-type", &ct);
        }
        reply.body = bytes::Bytes::from(failure.body);
        return reply.into_response();
    }
    let mut reply = live_error(path, status, "Codex live sideband upstream unavailable");
    for (name, value) in &headers {
        reply.headers.append(name.clone(), value.clone());
    }
    reply.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sideband_url_shapes() {
        let base = crate::DEFAULT_SIDEBAND_API_BASE_URL;
        assert_eq!(build_sideband_url(base, SidebandStyle::Frameless, "rtc_1"), "wss://api.openai.com/v1/live/rtc_1");
        assert_eq!(build_sideband_url(base, SidebandStyle::RealtimeCalls, "rtc_1"), "wss://api.openai.com/v1/realtime/calls/rtc_1");
        assert_eq!(
            build_sideband_url(base, SidebandStyle::RealtimeQuery, "rtc_2"),
            "wss://api.openai.com/v1/realtime?intent=quicksilver&call_id=rtc_2"
        );
    }

    #[test]
    fn upgrade_detection() {
        let mut headers = HeaderMap::new();
        assert!(!is_websocket_upgrade(&headers));
        headers.insert("connection", HeaderValue::from_static("keep-alive, Upgrade"));
        headers.insert("upgrade", HeaderValue::from_static("WebSocket"));
        assert!(is_websocket_upgrade(&headers));
    }
}
