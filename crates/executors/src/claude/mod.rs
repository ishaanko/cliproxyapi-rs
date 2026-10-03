//! Claude (Anthropic Messages API, OAuth + API key): port of internal/runtime/executor/claude_*.go and helps/claude_*, cloak_*.
//!
//! Plain first-party API keys are caller-owned passthrough. OAuth tokens and the
//! `claude-code-cli` fingerprint profile get the Claude Code request fingerprint: cloaking, billing
//! header with CCH signing, tool-name aliasing, device profile headers and the beta assembly.
//! TLS fingerprinting and exact wire header order/casing are not reproduced (reqwest + rustls).

use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};

use crate::ConfigRx;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::usage::UsageReporter;

pub mod auth;
pub mod body;
pub mod cache_control;
pub mod cloaking;
pub mod diagnostics;
pub mod execute;
pub mod fast_error;
pub mod helps;
pub mod http;
pub mod policy;
pub mod request;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod beta_matrix_tests;
#[cfg(test)]
mod cloaking_tests;
#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod state_tests;
pub mod signing;
pub mod stream;
pub mod thinking_replay;
pub mod tokens;
pub mod tool_remap;
pub mod tz;

/// Messages base URL when the credential names none.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Stateless Claude executor over the Messages API (Go: ClaudeExecutor).
pub struct ClaudeExecutor {
    cfg: ConfigRx,
    /// Reads configuration without OAuth-only provider settings (Go: `ForAPIKey`).
    api_key_scope: bool,
}

impl ClaudeExecutor {
    /// Config snapshot for one request.
    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_scope { config_for_api_key(&cfg) } else { cfg }
    }

    fn reporter(&self, auth: &Auth, req: &Request, opts: &Options) -> UsageReporter {
        let base_model = parse_suffix(&req.model).model_name;
        UsageReporter::new("claude", "ClaudeExecutor", &base_model, Some(auth), Some(opts))
    }
}

/// Builds the Claude executor over the live config handle.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(ClaudeExecutor { cfg, api_key_scope: false })
}

#[async_trait]
impl Executor for ClaudeExecutor {
    fn identifier(&self) -> &str {
        "claude"
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        let reporter = self.reporter(auth, &req, &opts);
        let result = self.execute_impl(&cfg, auth, req, opts, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        let reporter = self.reporter(auth, &req, &opts);
        reporter.set_stream(true);
        let result = self.execute_stream_impl(&cfg, auth, req, opts, reporter.clone()).await;
        reporter.track_failure(&result);
        result
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let cfg = self.config();
        if let Some(result) = refresh_auth_via_home(&cfg, auth).await {
            return result;
        }
        auth::refresh(auth, &cfg.proxy_url).await
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        self.count_tokens_impl(&cfg, auth, req, opts).await
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(ClaudeExecutor { cfg: self.cfg.clone(), api_key_scope: true }))
    }

    fn should_prepare_request_auth(&self, auth: &Auth) -> bool {
        auth::should_prepare_request_auth(auth)
    }

    async fn prepare_request_auth(&self, auth: &Auth) -> Result<Option<Auth>, ExecError> {
        let cfg = self.config();
        auth::prepare_request_auth(auth, &cfg.proxy_url).await
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }
}
