//! Antigravity (Cloud Code Gemini envelope): port of antigravity_*.go and helps/antigravity_*.
//!
//! One request goes: compaction expansion, short-cooldown precheck, signature validation, token
//! refresh, translation to the Cloud Code envelope, thinking and payload rules, reasoning
//! replay, schema cleaning, then a single upstream call per credential (the conductor owns
//! retries and rotation). Streaming and non-stream results are translated back to the client
//! format; 429s feed short cooldowns and the AI-credits fallback.
//!
//! Home mode: when a Home client is installed, the credits balance, short cooldowns and the
//! hint-refresh lock live in Home KV (see `credits`).
//!
//! Not ported: the raw `HttpRequest` passthrough (no trait hook yet), request-log capture (the
//! executor has no handle to the per-request log) and the Codex multi-agent-v2 request rewrite
//! (a Codex-client-only option).

// ExecError is the runtime contract's error type; its size is not this module's to change.
#![allow(clippy::result_large_err)]

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_auth::antigravity::AntigravityEndpoints;
use cpa_config::Config;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};
use std::sync::Arc;

use crate::helps::http_request;
use crate::ConfigRx;

mod auth;
mod compaction;
mod credits;
mod execute;
mod grounding;
mod pipeline;
mod replay;
mod replay_capture;
mod request;
mod signature;
mod stream;
mod tokens;
mod transport;

#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod grounding_tests;
#[cfg(test)]
mod misc_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod tests;

/// Provider key of this executor.
pub const IDENTIFIER: &str = "antigravity";

/// Executor for the `antigravity` provider.
#[derive(Clone)]
pub struct AntigravityExecutor {
    cfg: ConfigRx,
    pub(crate) endpoints: AntigravityEndpoints,
    /// Explicit OAuth client secret; production reads `CPA_ANTIGRAVITY_CLIENT_SECRET`.
    pub(crate) client_secret: Option<String>,
}

impl AntigravityExecutor {
    pub fn new(cfg: ConfigRx) -> Self {
        Self { cfg, endpoints: AntigravityEndpoints::default(), client_secret: None }
    }

    /// Snapshot of the live config for one request.
    pub(crate) fn cfg(&self) -> Arc<Config> {
        self.cfg.borrow().clone()
    }
}

/// Home-mode credits check, exposed for the integration tests that install a process-wide Home
/// client (Go: antigravityAuthHasCreditsRequired).
#[doc(hidden)]
pub async fn auth_has_credits_required(auth: &Auth) -> Result<bool, cpa_home::HomeError> {
    credits::auth_has_credits_required(auth).await
}

/// Builds the executor for registration with the conductor.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(AntigravityExecutor::new(cfg))
}

#[async_trait]
impl Executor for AntigravityExecutor {
    fn identifier(&self) -> &str {
        IDENTIFIER
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.execute_impl(auth, req, opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        self.execute_stream_impl(auth, req, opts).await
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let cfg = self.cfg();
        if let Some(result) = crate::helps::home_refresh::refresh_auth_via_home(&cfg, auth).await {
            return result;
        }
        self.refresh_token(&cfg, auth.clone()).await
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.count_tokens_impl(auth, req, opts).await
    }

    fn should_prepare_request_auth(&self, auth: &Auth) -> bool {
        Self::needs_request_auth(auth)
    }

    async fn prepare_request_auth(&self, auth: &Auth) -> Result<Option<Auth>, ExecError> {
        self.prepare_request_auth_impl(&self.cfg(), auth).await
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: AntigravityExecutor.PrepareRequest.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let (token, _) = self.ensure_access_token(&self.cfg(), auth).await?;
        if token.trim().is_empty() {
            return Err(ExecError::new(401, "missing access token"));
        }
        http_request::set_header(req, "Authorization", &format!("Bearer {token}"));
        Ok(())
    }

    /// Go: AntigravityExecutor.HttpRequest. A header whitelist: everything is stripped except
    /// `Content-Type`, then the Antigravity user agent and the bearer token are set.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        let content_type = req.headers().get(http::header::CONTENT_TYPE).cloned();
        req.headers_mut().clear();
        if let Some(ct) = content_type.filter(|v| !v.is_empty()) {
            req.headers_mut().insert(http::header::CONTENT_TYPE, ct);
        }
        http_request::set_header(&mut req, "User-Agent", &request::resolve_user_agent(auth));
        self.prepare_request(&mut req, auth).await?;
        let client = self.client(&self.cfg(), auth, "");
        http_request::execute(&client, req).await
    }
}
