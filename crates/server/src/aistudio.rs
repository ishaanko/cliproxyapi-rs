//! AI Studio websocket relay endpoint (Go: `wsrelay.Manager.Handler` mounted by
//! `Server.AttachWebsocketRoute`, and the `wsOnConnected` / `wsOnDisconnected` callbacks of
//! `sdk/cliproxy/service_auth.go`).
//!
//! A browser page connects to `/v1/ws`; the socket is handed to the process-wide relay the
//! `aistudio` executor sends through. Each live socket is a runtime-only credential: added on
//! connect, removed on disconnect (unless the socket was replaced by a newer one).

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::HeaderValue;
use axum::response::Response;
use cpa_auth::{Auth, Status};
use cpa_executors::gemini::wsrelay::{self, Inbound, Outbound};
use cpa_runtime::service::{AuthUpdate, AuthUpdateAction, Service};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

use crate::state::AppState;

/// Inbound message cap of the relay (Go: `maxInboundMessageLen`).
const MAX_INBOUND_MESSAGE: usize = 64 << 20;

/// Go `conditionalAuth`: the API-key check applies only while `ws-auth` is on.
pub async fn ws_auth_gate(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !st.cfg().websocket_auth {
        return next.run(req).await;
    }
    crate::middleware::api_key_auth(State(st), req, next).await
}

/// `GET /v1/ws` upgrade.
pub async fn relay_websocket(ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    // gorilla's failed upgrade: 400 `Bad Request` with `Sec-Websocket-Version: 13`.
    let Ok(ws) = ws else {
        return crate::reply::Reply::new(400)
            .content_type("text/plain; charset=utf-8")
            .with_header(axum::http::HeaderName::from_static("sec-websocket-version"), "13")
            .with_header(axum::http::HeaderName::from_static("x-content-type-options"), "nosniff")
            .with_body("Bad Request\n")
            .into_response();
    };
    let mut resp = ws
        .max_message_size(MAX_INBOUND_MESSAGE)
        .max_frame_size(MAX_INBOUND_MESSAGE)
        .on_upgrade(|socket| async move {
            let (mut sink, mut stream) = socket.split();
            let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(16);
            let (in_tx, in_rx) = mpsc::channel::<Result<Inbound, String>>(16);
            // Frames to write; ends (and closes the socket) when the session drops its sender.
            let writer = tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    let msg = match frame {
                        Outbound::Text(text) => Message::Text(text.into()),
                        Outbound::Ping => Message::Ping(b"ping".to_vec().into()),
                    };
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
                let _ = sink.close().await;
            });
            wsrelay::global().attach(out_tx, tokio_stream_from(in_rx));
            while let Some(frame) = stream.next().await {
                let inbound = match frame {
                    Ok(Message::Text(text)) => Ok(Inbound::Text(text.to_string())),
                    Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                        Ok(text) => Ok(Inbound::Text(text)),
                        Err(e) => Err(e.to_string()),
                    },
                    Ok(Message::Pong(_)) => Ok(Inbound::Pong),
                    Ok(Message::Ping(_)) => continue,
                    Ok(Message::Close(_)) => break,
                    Err(e) => Err(e.to_string()),
                };
                let failed = inbound.is_err();
                if in_tx.send(inbound).await.is_err() || failed {
                    break;
                }
            }
            drop(in_tx);
            writer.abort();
        });
    // gorilla writes `Connection: Upgrade` (axum lowercases the token).
    resp.headers_mut().insert(axum::http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
    resp
}

/// Adapts a channel receiver into the stream the relay reads frames from.
fn tokio_stream_from<T: Send + 'static>(mut rx: mpsc::Receiver<T>) -> impl futures_util::Stream<Item = T> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// The runtime-only credential of one live socket (Go: `wsOnConnected`).
fn channel_auth(channel_id: &str) -> Auth {
    let now = chrono::Utc::now();
    let mut auth = Auth::new(channel_id, "aistudio");
    auth.label = channel_id.to_string();
    auth.status = Status::Active;
    auth.created_at = Some(now);
    auth.updated_at = Some(now);
    auth.attributes.insert("runtime_only".into(), "true".into());
    auth.metadata.insert("email".into(), serde_json::Value::String(channel_id.to_string()));
    auth
}

/// Wires the relay's connect and disconnect callbacks to the service's credential pool.
pub fn install_relay_hooks(service: &Arc<Service>) {
    let on_connect = Arc::clone(service);
    let on_disconnect = Arc::clone(service);
    wsrelay::global().set_hooks(
        Some(move |channel_id: &str| {
            if channel_id.is_empty() || !channel_id.to_lowercase().starts_with("aistudio-") {
                return;
            }
            let service = Arc::clone(&on_connect);
            let channel_id = channel_id.to_string();
            tokio::spawn(async move {
                // An active, enabled credential for the channel already exists.
                if let Some(existing) = service.manager().get(&channel_id)
                    && !existing.disabled
                    && existing.status == Status::Active
                {
                    return;
                }
                tracing::info!("websocket provider connected: {channel_id}");
                let auth = channel_auth(&channel_id);
                service
                    .apply_runtime_auth_update(AuthUpdate {
                        action: AuthUpdateAction::Add,
                        id: channel_id,
                        auth: Some(auth),
                    })
                    .await;
            });
        }),
        Some(move |channel_id: &str, cause: &str| {
            if channel_id.is_empty() {
                return;
            }
            if cause.contains("replaced by new connection") {
                tracing::info!("websocket provider replaced: {channel_id}");
                return;
            }
            if cause.is_empty() {
                tracing::info!("websocket provider disconnected: {channel_id}");
            } else {
                tracing::warn!("websocket provider disconnected: {channel_id} ({cause})");
            }
            let service = Arc::clone(&on_disconnect);
            let channel_id = channel_id.to_string();
            tokio::spawn(async move {
                service
                    .apply_runtime_auth_update(AuthUpdate { action: AuthUpdateAction::Delete, id: channel_id, auth: None })
                    .await;
            });
        }),
    );
}

/// Go `SetWebsocketAuthChangeHandler`: turning `ws-auth` on terminates existing sockets so they
/// reconnect through authentication; turning it off leaves them connected.
pub fn watch_ws_auth(mut config: watch::Receiver<Arc<cpa_config::Config>>) {
    tokio::spawn(async move {
        let mut enabled = config.borrow().websocket_auth;
        while config.changed().await.is_ok() {
            let next = config.borrow().websocket_auth;
            if next == enabled {
                continue;
            }
            if !enabled && next {
                wsrelay::global().stop();
                tracing::debug!("ws-auth enabled; existing websocket sessions terminated to enforce authentication");
            } else {
                tracing::debug!("ws-auth disabled; existing websocket sessions remain connected");
            }
            enabled = next;
        }
    });
}
