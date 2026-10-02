//! Upstream-websocket session state of the Responses websocket handler: credential pinning across
//! turns, native passthrough eligibility and the replay-required signal (Go:
//! openai_responses_websocket_session.go and the matching parts of openai_responses_websocket.go).

use std::collections::BTreeMap;

use axum::extract::ws::CloseFrame;
use cpa_auth::Auth;
use cpa_core::registry::global_registry;
use cpa_json::J;
use cpa_runtime::conductor::Manager;
use serde_json::Value;

use super::requests::{WS_REQUEST_TYPE_APPEND, WS_REQUEST_TYPE_CREATE};
use super::{auth_available_for_model, available_auths_for_model, provider_set_for_model, resolved_model_name, truncate_close_reason, WS_CLOSE_REASON_MAX_BYTES};
use crate::error::ErrorMessage;

const CLOSE_SERVICE_RESTART: u16 = 1012;
const REPLAY_REQUIRED_CLOSE_REASON: &str = "upstream requires HTTP replay";

/// How the previous turn reached upstream (Go: `responsesWebsocketUpstreamMode*`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum UpstreamMode {
    #[default]
    Unknown,
    Http,
    Ws,
}

/// Credential remembered for a provider (Go: `responsesWebsocketPinnedAuthState`).
#[derive(Clone, Debug, Default)]
pub struct PinnedAuthState {
    pub auth_id: String,
    pub model_key: String,
}

/// A completed compaction response observed on this socket (Go:
/// `responsesWebsocketObservedCompactionState`; plugin and provider-route targets do not exist
/// in this port, so only the model and credential are tracked).
#[derive(Clone, Debug, Default)]
pub struct ObservedCompaction {
    pub model_name: String,
    pub auth_id: String,
}

/// `strconv.ParseBool`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// `websocketUpstreamSupportsIncrementalInput`: the credential's `websockets` flag.
pub fn supports_incremental_input(auth: &Auth) -> bool {
    if let Some(raw) = auth.attributes.get("websockets")
        && !raw.trim().is_empty()
        && let Some(parsed) = parse_bool(raw.trim())
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => parse_bool(s.trim()).unwrap_or(false),
        _ => false,
    }
}

/// `responsesWebsocketUsesUpstreamWebsocketPassthrough`: every credential able to serve the model
/// is one provider (codex or xai) with websockets enabled and a registered executor.
pub fn uses_upstream_websocket_passthrough(manager: &Manager, model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() {
        return false;
    }
    let auths = available_auths_for_model(manager, model);
    if auths.is_empty() {
        return false;
    }
    let mut provider = String::new();
    for auth in &auths {
        let auth_provider = auth.provider.trim().to_lowercase();
        if auth_provider != "codex" && auth_provider != "xai" {
            return false;
        }
        if provider.is_empty() {
            if manager.executor(&auth_provider).is_none() {
                return false;
            }
            provider = auth_provider;
        } else if auth_provider != provider {
            return false;
        }
        if !supports_incremental_input(auth) {
            return false;
        }
    }
    !provider.is_empty()
}

/// `responsesWebsocketPinnedAuthMatchesModel` (the home-runtime branch has no counterpart here).
pub fn pinned_auth_matches_model(auth: &Auth, model: &str) -> bool {
    let (providers, model_key) = provider_set_for_model(&resolved_model_name(model));
    if !providers.contains(&auth.provider.trim().to_lowercase()) {
        return false;
    }
    if !auth_available_for_model(auth, &model_key, chrono::Utc::now()) {
        return false;
    }
    global_registry().client_supports_model(&auth.id, &model_key)
}

/// Model key stored with a pinned credential.
pub fn model_key_for(model: &str) -> String {
    provider_set_for_model(&resolved_model_name(model)).1
}

/// `responsesWebsocketRequestRequiresCurrentUpstream`: continuations only make sense on the
/// upstream socket that holds the previous response.
pub fn request_requires_current_upstream(payload: &[u8]) -> bool {
    let root = cpa_json::parse(payload);
    !root.g("previous_response_id").str().trim().is_empty() || root.g("type").str().trim() == WS_REQUEST_TYPE_APPEND
}

/// `responsesWebsocketNativePassthroughAllowed`.
pub fn native_passthrough_allowed(mode: UpstreamMode, use_upstream_ws: bool, pinned: &str, upstream_auth: &str) -> bool {
    mode == UpstreamMode::Ws && use_upstream_ws && !pinned.trim().is_empty() && pinned.trim() == upstream_auth.trim()
}

/// `normalizeResponsesWebsocketPassthroughRequest`: the frame goes upstream unchanged except for
/// a filled-in model and `stream: true`.
pub fn normalize_passthrough_request(raw: &[u8], model: &str) -> Result<Vec<u8>, ErrorMessage> {
    if !cpa_json::valid(raw) {
        return Err(ErrorMessage::new(400, "invalid websocket request JSON"));
    }
    let mut v = cpa_json::parse(raw);
    let request_type = v.g("type").str().trim().to_string();
    if request_type != WS_REQUEST_TYPE_CREATE && request_type != WS_REQUEST_TYPE_APPEND {
        return Err(ErrorMessage::new(400, format!("unsupported websocket request type: {request_type}")));
    }
    if v.g("model").str().trim().is_empty() {
        let model = model.trim();
        if model.is_empty() {
            return Err(ErrorMessage::new(400, "missing model in response.create request"));
        }
        cpa_json::set(&mut v, "model", model);
    }
    cpa_json::set(&mut v, "stream", true);
    Ok(cpa_json::to_vec(&v))
}

/// `shouldReplayResponsesWebsocketPinnedAuthFailure`.
pub fn should_replay_pinned_auth_failure(err: &ErrorMessage) -> bool {
    matches!(err.status_or_500(), 401 | 429)
}

/// `shouldReleaseResponsesWebsocketPinnedAuth`: failures after which the credential is not
/// worth keeping affinity to.
pub fn should_release_pinned_auth(err: &ErrorMessage) -> bool {
    if matches!(err.status_or_500(), 401 | 402 | 403 | 429 | 408 | 502 | 503 | 504) {
        return true;
    }
    let msg = err.text.to_lowercase();
    [
        "stream closed before response.completed",
        "previous_response_not_found",
        "ws_failed",
        "upstream stream closed before first payload",
        "empty_stream",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// `websocketClosePayloadForUpstreamError` for the replay signal (426 `upstream_http_replay_required`).
pub fn replay_required_close_frame(err: &ErrorMessage) -> Option<CloseFrame> {
    (err.status == 426 && err.text.contains("upstream_http_replay_required")).then(|| CloseFrame {
        code: CLOSE_SERVICE_RESTART,
        reason: truncate_close_reason(REPLAY_REQUIRED_CLOSE_REASON, WS_CLOSE_REASON_MAX_BYTES).into(),
    })
}

/// The replay-required error the handler raises itself.
pub fn replay_required_error() -> ErrorMessage {
    ErrorMessage::new(
        426,
        r#"{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}"#,
    )
}

/// Pinned-credential bookkeeping of one socket (the closures in Go's `ResponsesWebsocket`).
#[derive(Default)]
pub struct PinnedAuths {
    pub current: String,
    pub by_provider: BTreeMap<String, PinnedAuthState>,
}

impl PinnedAuths {
    /// `rememberPinnedAuth`.
    pub fn remember(&mut self, manager: &Manager, auth_id: &str, model: &str) {
        let auth_id = auth_id.trim();
        let Some(auth) = (!auth_id.is_empty()).then(|| manager.get(auth_id)).flatten() else {
            return;
        };
        self.current = auth_id.to_string();
        let provider = auth.provider.trim().to_lowercase();
        if !provider.is_empty() {
            self.by_provider.insert(provider, PinnedAuthState { auth_id: auth_id.to_string(), model_key: model_key_for(model) });
        }
    }

    /// `forgetPinnedAuth`.
    pub fn forget(&mut self) {
        let current = std::mem::take(&mut self.current);
        self.by_provider.retain(|_, state| state.auth_id != current);
    }

    /// Re-validates or re-derives the pinned credential for the next turn's model (the
    /// `pinnedAuthID` refresh at the top of the Go read loop).
    pub fn refresh(&mut self, manager: &Manager, request_model: &str) {
        if !self.current.is_empty() {
            let auth = manager.get(&self.current);
            let provider = auth.as_ref().map(|a| a.provider.trim().to_lowercase()).unwrap_or_default();
            let valid = auth.as_ref().is_some_and(|a| {
                self.by_provider.get(&provider).is_some_and(|s| s.auth_id == self.current) && pinned_auth_matches_model(a, request_model)
            });
            if !valid {
                self.current.clear();
            }
        }
        if self.current.is_empty() {
            let (providers, _) = provider_set_for_model(&resolved_model_name(request_model));
            if providers.len() == 1 {
                for provider in providers {
                    let state = self.by_provider.get(&provider).cloned();
                    match state.and_then(|s| manager.get(&s.auth_id).map(|a| (s, a))) {
                        Some((state, auth)) if pinned_auth_matches_model(&auth, request_model) => self.current = state.auth_id,
                        _ => {
                            self.by_provider.remove(&provider);
                        }
                    }
                }
            }
        }
    }
}
