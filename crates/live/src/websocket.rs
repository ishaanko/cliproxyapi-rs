//! Standard Realtime websocket relayed through Codex OAuth (Go: websocket.go).

use axum::extract::ws::WebSocketUpgrade;
use axum::response::Response;
use http::{HeaderMap, HeaderValue};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as UpMessage;

use futures_util::SinkExt;

use crate::reply::{Reply, content_type_of, realtime_error};
use crate::sideband::{RelayEnd, finish_upgrade, is_websocket_upgrade, relay_websockets, upgrade_failed};
use crate::upstream::{MAX_WS_MESSAGE_SIZE, call_response_headers, copy_handshake_headers, protocol_headers};
use crate::util::{codex_realtime_model, marshal_raw_map, model_from_json, unmarshal_raw_map};
use crate::{Caller, Handler, RequestParts, live_selection_headers, selection_error};

const DEFAULT_STANDARD_REALTIME_MODEL: &str = "gpt-realtime";

/// `directRealtimeHeaders`: protocol headers without `OpenAI-Alpha`, default `Originator`.
fn direct_realtime_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = protocol_headers(source);
    headers.remove("openai-alpha");
    if headers.get("originator").is_none_or(|v| v.is_empty()) {
        headers.insert("originator", HeaderValue::from_static("Codex Desktop"));
    }
    headers
}

/// `realtimeSessionUpdate`: the client-secret session without identity fields.
fn realtime_session_update(session: &str) -> Result<String, String> {
    let mut update = unmarshal_raw_map(session.as_bytes())?;
    for field in ["model", "id", "object", "expires_at", "client_secret"] {
        update.remove(field);
    }
    Ok(marshal_raw_map(&update))
}

impl Handler {
    /// `HandleRealtimeWebsocket`: a sideband when `call_id` is given, else a direct socket.
    pub async fn handle_realtime_websocket(&self, caller: &Caller, parts: &RequestParts, ws: Option<WebSocketUpgrade>) -> Response {
        if parts.query_first("call_id").is_some_and(|c| !c.trim().is_empty()) {
            return self.handle_sideband(caller, parts, ws).await;
        }
        self.handle_direct_websocket(caller, parts, ws).await
    }

    /// `HandleDirectWebsocket`.
    pub async fn handle_direct_websocket(&self, caller: &Caller, parts: &RequestParts, ws: Option<WebSocketUpgrade>) -> Response {
        if !is_websocket_upgrade(&parts.headers) {
            let mut reply =
                realtime_error(426, "WebSocket upgrade required", "invalid_request_error", "websocket_upgrade_required");
            reply.set("upgrade", "websocket");
            return reply.into_response();
        }
        let mut requested_model = parts.query_first("model").unwrap_or("").trim().to_string();
        if requested_model.is_empty() {
            requested_model = DEFAULT_STANDARD_REALTIME_MODEL.into();
        }
        let token_session = caller.client_secret.as_ref().map(|c| c.session.clone()).filter(|s| !s.is_empty());
        if let Some(session) = &token_session {
            let token_model = codex_realtime_model(&model_from_json(session.as_bytes()));
            if codex_realtime_model(&requested_model) != token_model {
                return realtime_error(
                    403,
                    "Realtime client secret is not valid for the requested model",
                    "invalid_request_error",
                    "realtime_client_secret_scope_mismatch",
                )
                .into_response();
            }
        }
        let mut selected = match self.select_oauth(&live_selection_headers(parts, caller), &[], None) {
            Ok(a) => a,
            Err(e) => return selection_error(&parts.path, &e).into_response(),
        };
        caller.record_trace(&mut selected);

        let encoded: String = url::form_urlencoded::Serializer::new(String::new()).append_pair("model", requested_model.trim()).finish();
        let upstream_url = format!("{}/realtime?{}", self.inner.sideband_base.read().trim_end_matches('/'), encoded);
        let mut dialed = match self
            .dial_upstream(caller, &selected, &upstream_url, direct_realtime_headers(&parts.headers), &parts.headers)
            .await
        {
            Ok(d) => d,
            Err(failure) => return direct_dial_error(caller, failure),
        };
        if let Some(log) = caller.log() {
            log.websocket_handshake(dialed.status, &call_response_headers(&dialed.response_headers));
        }
        if let Some(session) = &token_session {
            let failed = |status, message: &str, kind, code| realtime_error(status, message, kind, code).into_response();
            let update = match realtime_session_update(session) {
                Ok(u) => u,
                Err(_) => {
                    return failed(500, "Failed to apply Realtime client secret session", "server_error", "realtime_session_failed");
                }
            };
            let frame = format!(r#"{{"type":"session.update","session":{update}}}"#);
            if dialed.stream.send(UpMessage::text(frame)).await.is_err() {
                return failed(502, "Failed to apply Realtime client secret session", "api_error", "realtime_upstream_unavailable");
            }
        }
        let Some(ws) = ws else {
            return upgrade_failed();
        };
        let ws = match &dialed.subprotocol {
            Some(p) => ws.protocols([p.clone()]),
            None => ws,
        };
        let log = caller.log.clone();
        let resp = ws.max_message_size(MAX_WS_MESSAGE_SIZE).max_frame_size(MAX_WS_MESSAGE_SIZE).on_upgrade(move |socket| async move {
            // The direct socket has no owner to cancel it: the relay ends with either peer.
            let (_keep, cancel_rx) = watch::channel(false);
            let end = relay_websockets(socket, dialed.stream, cancel_rx).await;
            if let (Some(log), RelayEnd::Failed) = (&log, &end) {
                log.websocket_error("relay", "relay closed");
            }
        });
        finish_upgrade(resp)
    }
}

/// The failure branch of the direct dial.
fn direct_dial_error(caller: &Caller, failure: crate::ws_client::DialFailure) -> Response {
    let mut status = failure.status.unwrap_or(502);
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
    let (mut message, mut kind) = ("Codex Realtime WebSocket upstream unavailable", "api_error");
    if status == 404 || status == 501 {
        message = "Direct Realtime WebSocket is not supported by the Codex OAuth upstream";
        kind = "not_supported_error";
        status = 501;
    }
    let code = if kind == "not_supported_error" { "realtime_capability_not_supported" } else { "realtime_websocket_upstream_unavailable" };
    let mut reply = realtime_error(status, message, kind, code);
    for (name, value) in &headers {
        reply.headers.append(name.clone(), value.clone());
    }
    reply.into_response()
}
