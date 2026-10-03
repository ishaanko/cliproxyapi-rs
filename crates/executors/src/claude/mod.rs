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

use crate::helps::http_request;
use crate::ConfigRx;
use crate::helps::logging::UpstreamRequestLog;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::usage::UsageReporter;

pub mod auth;
pub mod body;
pub mod cache_control;
pub mod cloaking;
pub mod decode;
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
    /// Set when another provider embeds this executor (Kimi's Anthropic-compatible path).
    embedding: Option<Embedding>,
}

/// How an embedding provider customizes the Claude executor (Go: the `requestLogProvider` and
/// `upstreamModelNormalizer` fields of `ClaudeExecutor`). An embedded executor is only reached
/// through its owner, whose token counting always asks the upstream (Go: `countTokensUpstream`).
#[derive(Clone, Copy)]
pub struct Embedding {
    /// Provider label of the upstream request log.
    pub request_log_provider: &'static str,
    /// Maps the client model to the model sent upstream; the client model is restored in responses.
    pub upstream_model: fn(&str) -> String,
}

impl ClaudeExecutor {
    /// Config snapshot for one request.
    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_scope { config_for_api_key(&cfg) } else { cfg }
    }

    /// Provider label of the upstream request log (Go: upstreamRequestLogProvider).
    fn upstream_request_log_provider(&self) -> &str {
        match &self.embedding {
            Some(e) if !e.request_log_provider.trim().is_empty() => e.request_log_provider,
            _ => "claude",
        }
    }

    /// Go: upstreamModel.
    fn upstream_model(&self, base_model: &str) -> String {
        match &self.embedding {
            Some(e) => (e.upstream_model)(base_model),
            None => base_model.to_string(),
        }
    }

    /// Records the outbound request on the inbound request's api log (Go:
    /// helps.RecordAPIRequest with claudeAuthLogIdentity). Skips the body copy when the call is
    /// not part of a logged request.
    fn record_upstream_request(&self, cfg: &Config, auth: &Auth, opts: &Options, url: &str, headers: &::http::HeaderMap, body: &[u8]) {
        if opts.api_log.get().is_none() {
            return;
        }
        opts.api_log.record_api_request(
            cfg,
            UpstreamRequestLog::from_auth(self.upstream_request_log_provider(), Some(auth), "POST", url, headers, body),
        );
    }

    fn reporter(&self, auth: &Auth, req: &Request, opts: &Options) -> UsageReporter {
        let base_model = parse_suffix(&req.model).model_name;
        UsageReporter::new("claude", "ClaudeExecutor", &base_model, Some(auth), Some(opts))
    }
}

/// The Claude executor Kimi embeds (Go: `ClaudeExecutor{requestLogProvider: "kimi",
/// upstreamModelNormalizer: normalizeKimiUpstreamModel}`).
pub fn new_embedded(cfg: ConfigRx, embedding: Embedding) -> DynExecutor {
    Arc::new(ClaudeExecutor { cfg, api_key_scope: false, embedding: Some(embedding) })
}

/// Builds the Claude executor over the live config handle.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(ClaudeExecutor { cfg, api_key_scope: false, embedding: None })
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
        Some(Arc::new(ClaudeExecutor { cfg: self.cfg.clone(), api_key_scope: true, embedding: self.embedding }))
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

    /// Go: ClaudeExecutor.PrepareRequest. The `x-api-key` header is used only for API-key
    /// credentials on the first-party Anthropic origin; everything else is a bearer token.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let (api_key, _) = request::claude_creds(auth);
        let use_api_key = auth.auth_kind() == cpa_auth::types::AUTH_KIND_API_KEY
            || auth.attributes.get("api_key").is_some_and(|k| !k.trim().is_empty());
        let anthropic_base = helps::upstream::is_anthropic_upstream_url(Some(req.url()));
        if api_key.trim().is_empty() {
            http_request::del_header(req, "Authorization");
            http_request::del_header(req, "x-api-key");
        } else if anthropic_base && use_api_key {
            http_request::del_header(req, "Authorization");
            http_request::set_header(req, "x-api-key", &api_key);
        } else {
            http_request::del_header(req, "x-api-key");
            http_request::set_header(req, "Authorization", &format!("Bearer {api_key}"));
        }
        http_request::apply_attr_headers(req, auth);
        Ok(())
    }

    /// Go: ClaudeExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        self.prepare_request(&mut req, auth).await?;
        let client = http::claude_http_client("", &self.config(), auth);
        client.execute(req).await.map_err(|e| e.exec_error())
    }
}
