//! Upstream websocket response frames delivered to an observer (Go:
//! helps/websocket_observer_helpers.go).
//!
//! Go keeps the observer in `executor.Options.WebSocketResponseObserver`; `cpa_runtime`'s
//! `Options` has no such field yet, so the observer is passed explicitly by whoever owns it.

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::executor::{Metadata, Options};
use serde_json::Value;

use super::payload::payload_requested_model;

/// One upstream websocket response frame with the request facts around it.
#[derive(Debug, Clone)]
pub struct WebSocketResponseEvent {
    pub request_id: String,
    pub trace_id: String,
    pub source_format: String,
    pub model: String,
    pub requested_model: String,
    pub provider: String,
    pub auth_id: String,
    pub auth_label: String,
    pub auth_type: String,
    pub event_type: String,
    pub payload: Vec<u8>,
    pub metadata: Metadata,
}

/// Callback receiving upstream websocket response frames.
pub type WebSocketResponseObserver = Arc<dyn Fn(WebSocketResponseEvent) + Send + Sync>;

/// Delivers `payload` to `observer`; a missing observer or empty payload is a no-op.
pub fn emit_web_socket_response_event(
    observer: Option<&WebSocketResponseObserver>,
    opts: &Options,
    auth: Option<&Auth>,
    provider: &str,
    model: &str,
    payload: &[u8],
) {
    let Some(observer) = observer else {
        return;
    };
    if payload.is_empty() {
        return;
    }
    let (auth_id, auth_label, auth_type) = match auth {
        Some(a) => (a.id.clone(), a.label.clone(), a.account_info().0.to_string()),
        None => Default::default(),
    };
    let meta_str = |key: &str| opts.metadata.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    observer(WebSocketResponseEvent {
        request_id: meta_str("request_id"),
        trace_id: meta_str("trace_id"),
        source_format: opts.source_format.to_string(),
        model: model.to_string(),
        requested_model: payload_requested_model(opts, model),
        provider: provider.to_string(),
        auth_id,
        auth_label,
        auth_type,
        event_type: cpa_json::parse(payload).g("type").str(),
        payload: payload.to_vec(),
        metadata: opts.metadata.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_translator::Format;
    use parking_lot::Mutex;

    #[test]
    fn event_carries_request_facts() {
        let seen: Arc<Mutex<Vec<WebSocketResponseEvent>>> = Arc::default();
        let sink = seen.clone();
        let observer: WebSocketResponseObserver = Arc::new(move |e| sink.lock().push(e));
        let mut opts = Options::new(Format::OpenAIResponse);
        opts.metadata.insert("request_id".into(), Value::String("r1".into()));
        opts.metadata.insert("requested_model".into(), Value::String("alias".into()));
        let mut auth = Auth::new("a1", "codex");
        auth.metadata.insert("email".into(), Value::String("x@y.z".into()));
        emit_web_socket_response_event(Some(&observer), &opts, Some(&auth), "codex", "gpt-5", br#"{"type":"response.created"}"#);
        emit_web_socket_response_event(Some(&observer), &opts, None, "codex", "gpt-5", b"");
        emit_web_socket_response_event(None, &opts, None, "codex", "gpt-5", b"{}");
        let events = seen.lock();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!((e.request_id.as_str(), e.requested_model.as_str(), e.event_type.as_str()), ("r1", "alias", "response.created"));
        assert_eq!((e.source_format.as_str(), e.auth_id.as_str()), ("openai-response", "a1"));
    }
}
