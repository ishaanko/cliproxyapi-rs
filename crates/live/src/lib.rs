//! Codex live / realtime endpoints (Go: internal/client/codex/live): SDP call bootstrap with an
//! optional local WebRTC media relay, sideband and direct websocket relays, local client secrets
//! and the capability stubs.
//!
//! The crate is framework-light: [`Handler`] methods take a [`Caller`] (what the auth middleware
//! learned) and plain request parts and return a [`Reply`]; [`http`] adapts them to axum routes.
//! Not ported: Home-mode dispatch selections (the runtime has no Home mode) and the session
//! hierarchy log metadata.

#![allow(clippy::result_large_err)]

mod call;
mod capabilities;
mod client_secret;
mod go_json;
pub mod endpoints;
mod log;
mod media;
mod multipart;
mod relay;
mod reply;
mod session;
mod sideband;
mod tcp_proxy;
mod upstream;
mod util;
mod websocket;
mod ws_client;

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_config::{CodexLiveMediaRelayConfig, Config};
use cpa_core::format::Format;
use cpa_runtime::conductor::Manager;
use cpa_runtime::executor::{ExecError, Options};
use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;

pub use client_secret::{CLIENT_SECRET_PREFIX, ClientSecretAuthorization};
pub use log::UpstreamLog;
pub use media::{MediaError, MediaLimiter, MediaRelayFactory, MediaRelaySession, MediaRoute};
pub use relay::PionMediaRelay;
pub use reply::Reply;
pub use session::{LiveSession, SessionStore};

/// Default sideband and hangup API base (Go: `defaultSidebandAPIBaseURL`).
pub const DEFAULT_SIDEBAND_API_BASE_URL: &str = "wss://api.openai.com/v1";
/// Upstream call bootstrap URL (Go: `upstreamCallURL`).
pub const UPSTREAM_CALL_URL: &str = "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";

/// A local ephemeral key presented by the caller.
#[derive(Debug, Clone, Default)]
pub struct ClientSecretCaller {
    /// Session principal of the key (`sess_...`).
    pub principal: String,
    /// Canonical upstream session JSON bound to the key.
    pub session: String,
}

/// Authentication result of a live request (the gin context values the Go handlers read).
#[derive(Clone, Default)]
pub struct Caller {
    /// `userApiKey`.
    pub principal: String,
    /// `accessProvider`.
    pub provider: String,
    pub client_secret: Option<ClientSecretCaller>,
    /// Receives the auth index of the credential selected for the request (trace header).
    pub trace: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub log: Option<Arc<dyn UpstreamLog>>,
}

impl Caller {
    /// `requestOwner`: trimmed principal and provider.
    pub(crate) fn owner(&self) -> (String, String) {
        (self.principal.trim().to_string(), self.provider.trim().to_string())
    }

    pub(crate) fn record_trace(&self, auth: &mut Auth) {
        if let Some(trace) = &self.trace {
            trace(&auth.ensure_index());
        }
    }

    pub(crate) fn log(&self) -> Option<&dyn UpstreamLog> {
        self.log.as_deref()
    }
}

/// The parts of the inbound request the handlers read.
#[derive(Debug, Clone, Default)]
pub struct RequestParts {
    /// `c.Request.URL.Path`.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: ::http::HeaderMap,
    /// The `:call_id` route parameter, when the route has one.
    pub call_id: Option<String>,
}

impl RequestParts {
    pub(crate) fn query_first(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct MediaState {
    relay: Option<Arc<dyn MediaRelayFactory>>,
    err: Option<String>,
    config: Option<CodexLiveMediaRelayConfig>,
    /// A relay installed explicitly (tests); config reloads leave it alone.
    pinned: bool,
}

pub(crate) struct Inner {
    pub(crate) manager: Arc<Manager>,
    config: watch::Receiver<Arc<Config>>,
    pub(crate) sessions: SessionStore,
    pub(crate) secrets: client_secret::ClientSecretStore,
    pub(crate) call_url: RwLock<String>,
    pub(crate) sideband_base: RwLock<String>,
    media: Mutex<MediaState>,
    limiter: Arc<MediaLimiter>,
}

/// Codex live handler shared by every route.
#[derive(Clone)]
pub struct Handler {
    pub(crate) inner: Arc<Inner>,
}

impl Handler {
    pub fn new(manager: Arc<Manager>, config: watch::Receiver<Arc<Config>>) -> Self {
        let handler = Handler {
            inner: Arc::new(Inner {
                manager,
                config,
                sessions: SessionStore::default(),
                secrets: client_secret::ClientSecretStore::default(),
                call_url: RwLock::new(UPSTREAM_CALL_URL.to_string()),
                sideband_base: RwLock::new(DEFAULT_SIDEBAND_API_BASE_URL.to_string()),
                media: Mutex::new(MediaState::default()),
                limiter: Arc::new(MediaLimiter::default()),
            }),
        };
        if let Err(e) = handler.update_config() {
            tracing::error!("failed to configure Codex Live media relay: {e}");
        }
        handler
    }

    /// `UpdateConfig`: applies the current media relay settings to new sessions.
    pub fn update_config(&self) -> Result<(), String> {
        let cfg = self.inner.config.borrow().clone();
        let relay_config = cfg.codex.live_media_relay.clone();
        let mut state = self.inner.media.lock();
        if state.pinned {
            return Ok(());
        }
        let previously_configured = state.config.is_some();
        if state.config.as_ref() == Some(&relay_config) {
            return state.err.clone().map_or(Ok(()), Err);
        }
        let (relay, err) = if relay_config.enabled {
            match PionMediaRelay::new(&relay_config, self.inner.limiter.clone()) {
                Ok(r) => (Some(Arc::new(r) as Arc<dyn MediaRelayFactory>), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };
        state.relay = relay;
        state.err = err.clone();
        state.config = Some(relay_config.clone());
        drop(state);
        if err.is_none() && (previously_configured || relay_config.enabled) {
            let public_ip = if relay_config.public_ip.trim().is_empty() { "auto" } else { relay_config.public_ip.trim() };
            tracing::info!(
                enabled = relay_config.enabled,
                max_sessions = relay_config.effective_max_sessions(),
                disable_private_remote_ips = relay_config.disable_private_remote_ips,
                public_ip,
                udp_port_min = relay_config.udp_port_min,
                udp_port_max = relay_config.udp_port_max,
                ice_server_count = relay_config.ice_servers.len(),
                "{}",
                if previously_configured {
                    "codex live media relay configuration reloaded; changes apply to new sessions"
                } else {
                    "codex live media relay configured"
                }
            );
        }
        err.map_or(Ok(()), Err)
    }

    /// `currentRuntime`: config snapshot plus the media relay (reconciled with the live config).
    pub(crate) fn current_runtime(&self) -> (Arc<Config>, Option<Arc<dyn MediaRelayFactory>>, Option<String>) {
        let _ = self.update_config();
        let cfg = self.inner.config.borrow().clone();
        let state = self.inner.media.lock();
        (cfg, state.relay.clone(), state.err.clone())
    }

    pub(crate) fn cfg(&self) -> Arc<Config> {
        self.inner.config.borrow().clone()
    }

    /// `Close`: ends every call session and forgets client secrets.
    pub fn close(&self) {
        self.inner.sessions.close_all("server_stopped");
        self.inner.secrets.close();
    }

    /// Call sessions (tests and diagnostics).
    pub fn sessions(&self) -> &SessionStore {
        &self.inner.sessions
    }

    /// Overrides the upstream URLs (tests point them at local servers).
    #[doc(hidden)]
    pub fn set_upstream_urls(&self, call_url: &str, sideband_base: &str) {
        *self.inner.call_url.write() = call_url.to_string();
        *self.inner.sideband_base.write() = sideband_base.to_string();
    }

    /// Installs a media relay factory that config reloads do not replace (tests).
    #[doc(hidden)]
    pub fn set_media_relay(&self, relay: Option<Arc<dyn MediaRelayFactory>>) {
        let mut state = self.inner.media.lock();
        state.relay = relay;
        state.err = None;
        state.pinned = true;
    }

    /// `realtimeHTTPBaseURL`.
    pub(crate) fn realtime_http_base_url(&self) -> String {
        util::websocket_http_url(&self.inner.sideband_base.read()).trim_end_matches('/').to_string()
    }

    /// `selectOAuth` for the non-Home path.
    pub(crate) fn select_oauth(&self, headers: &::http::HeaderMap, body: &[u8], pinned: Option<(&str, &str)>) -> Result<Auth, ExecError> {
        let mut opts = Options::new(Format::Codex);
        opts.headers = headers.clone();
        opts.original_request = bytes::Bytes::copy_from_slice(body);
        if let Some((auth_id, session_id)) = pinned {
            opts.metadata.insert(cpa_runtime::executor::meta::PINNED_AUTH_ID.into(), serde_json::json!(auth_id));
            opts.metadata.insert(cpa_runtime::executor::meta::EXECUTION_SESSION_ID.into(), serde_json::json!(session_id));
        }
        self.inner.manager.select_auth_by_kind("codex", "", "oauth", &opts)
    }
}

/// `liveSelectionHeaders`: request headers, without credentials when a client secret is used.
pub(crate) fn live_selection_headers(parts: &RequestParts, caller: &Caller) -> ::http::HeaderMap {
    let mut headers = parts.headers.clone();
    if caller.client_secret.is_some() {
        headers.remove(::http::header::AUTHORIZATION);
        headers.remove(::http::header::PROXY_AUTHORIZATION);
    }
    headers
}

/// `writeSelectionError`: status of the selection failure plus its safe `Retry-After`.
pub(crate) fn selection_error(path: &str, err: &ExecError) -> Reply {
    let status = if err.status > 0 { err.status } else { 503 };
    let mut reply = reply::live_error(path, status, &err.message);
    for value in cpa_runtime::conductor::safe_response_headers(err).get_all(::http::header::RETRY_AFTER) {
        reply.headers.append(::http::header::RETRY_AFTER, value.clone());
    }
    reply
}

/// `proxyURLForAuth`: the credential's proxy, else the global one.
pub(crate) fn proxy_url_for_auth(cfg: &Config, auth: &Auth) -> String {
    if !auth.proxy_url.trim().is_empty() {
        return auth.proxy_url.trim().to_string();
    }
    cfg.proxy_url.trim().to_string()
}
