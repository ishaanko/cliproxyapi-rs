//! Codex (ChatGPT Responses backend): HTTP + SSE, upstream websocket and `/responses/compact`.
//! Port of internal/runtime/executor/codex_*.go and helps/codex_*.
//!
//! [`CodexExecutor`] is Go's `CodexAutoExecutor`: it serves a request over the upstream websocket
//! only when the client came in over a Responses websocket ([`META_DOWNSTREAM_WEBSOCKET`]) and the
//! credential enables `websockets`; everything else, including token counting and refresh, uses
//! the HTTP executor.
//!
//! Websocket continuation: a request flagged [`META_REQUIRED_UPSTREAM_WEBSOCKET`] must reuse the
//! session's live upstream connection, otherwise it fails with the replay-required error (426) so
//! the client replays the full transcript over HTTP.

mod count;
mod creds;
mod headers;
mod exec_http;
mod input_ids;
mod logging;
pub(crate) mod multi_agent_v2;
mod quota;
mod reasoning;
mod request;
mod terminal;
mod tool_schema;
mod ws;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{DynExecutor, ErrorCode, ExecError, Executor, Metadata, Options, Request, Response, StreamResult, meta};
use serde_json::Value;

use crate::ConfigRx;
use crate::helps::oauth_scope::config_for_api_key;

pub use quota::parse_codex_quota_event_headers;
pub use ws::{close_codex_websocket_sessions_for_auth_id, upstream_disconnect_receiver};

/// Metadata flag: the client is connected over a Responses websocket.
pub const META_DOWNSTREAM_WEBSOCKET: &str = "downstream_websocket";
/// Metadata flag: the request continues a response and needs the session's live upstream socket.
pub const META_REQUIRED_UPSTREAM_WEBSOCKET: &str = "required_upstream_websocket";

/// Request that cannot continue because its upstream websocket is gone (Go:
/// UpstreamWebsocketReplayRequiredError).
pub fn upstream_websocket_replay_required() -> ExecError {
    let mut err = ExecError::new(
        426,
        r#"{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}"#,
    )
    .with_code(ErrorCode::RequestScoped);
    err.upstream_attempted = false;
    err
}

pub fn is_upstream_websocket_replay_required(err: &ExecError) -> bool {
    err.status == 426 && err.message.contains("upstream_http_replay_required")
}

fn metadata_flag(metadata: &Metadata, key: &str) -> bool {
    matches!(metadata.get(key), Some(Value::Bool(true)))
}

/// The Codex provider executor.
pub struct CodexExecutor {
    cfg: ConfigRx,
    /// Execution-local view without OAuth-only configuration (API-key credentials).
    api_key_scope: bool,
}

/// Builds the Codex executor over the live config.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(CodexExecutor { cfg, api_key_scope: false })
}

impl CodexExecutor {
    /// Current config, scoped for API-key credentials when this is the API-key view.
    pub(crate) fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_scope { config_for_api_key(&cfg) } else { cfg }
    }

    /// Internal session id behind `$CPA-SESSION-ID` in custom headers.
    pub(crate) fn session_id(&self, opts: &Options, payload: &[u8]) -> Option<String> {
        crate::helps::session::ensure_session_id(None, "", opts, payload)
    }
}

#[async_trait]
impl Executor for CodexExecutor {
    fn identifier(&self) -> &str {
        "codex"
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        if metadata_flag(&opts.metadata, META_DOWNSTREAM_WEBSOCKET) && creds::websockets_enabled(auth) {
            return self.execute_ws(auth, req, opts).await;
        }
        if metadata_flag(&opts.metadata, META_REQUIRED_UPSTREAM_WEBSOCKET) {
            return Err(upstream_websocket_replay_required());
        }
        self.execute_http(auth, req, opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        if metadata_flag(&opts.metadata, META_DOWNSTREAM_WEBSOCKET) && creds::websockets_enabled(auth) {
            return self.execute_stream_ws(auth, req, opts).await;
        }
        if metadata_flag(&opts.metadata, META_REQUIRED_UPSTREAM_WEBSOCKET) {
            return Err(upstream_websocket_replay_required());
        }
        self.execute_stream_http(auth, req, opts).await
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        self.refresh_auth(auth).await
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.count_tokens_impl(auth, req, opts).await
    }

    async fn close_execution_session(&self, session_id: &str) {
        ws::close_execution_session(session_id);
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(CodexExecutor { cfg: self.cfg.clone(), api_key_scope: true }))
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }
}

/// Execution-session id of a request (Go: executionSessionIDFromOptions).
pub(crate) fn execution_session_id(opts: &Options) -> String {
    match opts.metadata.get(meta::EXECUTION_SESSION_ID) {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}
