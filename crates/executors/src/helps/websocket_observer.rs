//! Upstream websocket response frames delivered to a plugin observer (Go:
//! helps/websocket_observer_helpers.go `EmitWebSocketResponseEvent`).

use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::executor::{Options, WebSocketResponseEvent, WebSocketResponseObserver};
use serde_json::Value;

use super::payload::payload_requested_model;

/// The request's observer with the facts of every event it receives, captured once per request
/// (a websocket request emits one event per upstream frame).
#[derive(Clone)]
pub struct WsFrameObserver {
    observer: WebSocketResponseObserver,
    template: WebSocketResponseEvent,
}

impl WsFrameObserver {
    /// `None` when no observer is installed.
    pub fn new(opts: &Options, auth: Option<&Auth>, provider: &str, model: &str) -> Option<Self> {
        let observer = opts.websocket_response_observer.clone()?;
        let (auth_id, auth_label, auth_type) = match auth {
            Some(a) => (a.id.clone(), a.label.clone(), a.account_info().0.to_string()),
            None => Default::default(),
        };
        let meta_str = |key: &str| opts.metadata.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let template = WebSocketResponseEvent {
            request_id: meta_str("request_id"),
            trace_id: meta_str("trace_id"),
            source_format: opts.source_format.to_string(),
            model: model.to_string(),
            requested_model: payload_requested_model(opts, model),
            provider: provider.to_string(),
            auth_id,
            auth_label,
            auth_type,
            metadata: opts.metadata.clone(),
            ..Default::default()
        };
        Some(WsFrameObserver { observer, template })
    }

    /// Delivers one upstream frame; an empty payload is skipped.
    pub fn emit(&self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let mut event = self.template.clone();
        event.event_type = cpa_json::parse(payload).g("type").str();
        event.payload = bytes::Bytes::copy_from_slice(payload);
        (self.observer.0)(event);
    }
}
