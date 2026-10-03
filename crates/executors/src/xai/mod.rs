//! xAI (Grok) Responses-style upstream over HTTP: port of xai_executor*.go.
//!
//! - [`request`]: credentials, endpoint/header selection and the request body pipeline.
//! - [`tools`], [`response`], [`replay`]: tool dialect normalization, event/input
//!   normalization, and the reasoning replay cache.
//! - [`execute`], [`stream`], [`media`], [`tokens`]: the executor entry points.

mod execute;
mod media;
mod replay;
mod request;
mod response;
mod stream;
mod tokens;
mod tools;
mod util;

use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};
use http::{HeaderMap, Method};

use crate::ConfigRx;
use crate::helps::logging::UpstreamRequestLog;
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::proxy::{effective_proxy_url, new_proxy_aware_http_client};
use crate::helps::usage::UsageReporter;
use crate::helps::status::transport_error;
use request::{IDENTIFIER, auth_metadata_string};

/// Executor for xAI Grok's Responses API.
pub struct XaiExecutor {
    cfg: ConfigRx,
    /// Reads go through `Config::for_api_key` (Go: `ForAPIKey`).
    api_key_view: bool,
}

/// The `xai` executor.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(XaiExecutor { cfg, api_key_view: false })
}

impl XaiExecutor {
    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_view { config_for_api_key(&cfg) } else { cfg }
    }

    /// POST of `body` with TTFT tracking; transport failures become status-less errors.
    async fn send(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        reporter: &UsageReporter,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ExecError> {
        self.send_method(cfg, auth, opts, reporter, Method::POST, url, headers, Some(body)).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_method(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        reporter: &UsageReporter,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, ExecError> {
        tracing::debug!(target: "cpa::upstream", provider = IDENTIFIER, "{method} {url}");
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        let mut builder = client.request(method, url).headers(headers);
        if let Some(body) = body {
            builder = builder.body(body);
        }
        reporter.start_response_ttft();
        match builder.send().await {
            Ok(resp) => {
                opts.api_log.record_api_response_metadata(cfg, resp.status().as_u16(), resp.headers());
                Ok(resp)
            }
            Err(e) => {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &err.message);
                Err(err)
            }
        }
    }

    /// Go: recordXAIRequest. Records the upstream request in the request log (always as POST,
    /// like Go, even for the video status GET).
    fn record_request(&self, cfg: &Config, auth: &Auth, opts: &Options, url: &str, headers: &HeaderMap, body: &[u8]) {
        let (auth_type, auth_value) = auth.account_info();
        opts.api_log.record_api_request(
            cfg,
            UpstreamRequestLog {
                url: url.to_string(),
                method: Method::POST.to_string(),
                headers: headers.clone(),
                body: body.to_vec(),
                provider: IDENTIFIER.to_string(),
                auth_id: auth.id.clone(),
                auth_label: auth.label.clone(),
                auth_type: auth_type.to_string(),
                auth_value,
            },
        );
    }
}

#[async_trait]
impl Executor for XaiExecutor {
    fn identifier(&self) -> &str {
        IDENTIFIER
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        if opts.alt == "responses/compact" {
            return self.execute_compact(auth, &req, &opts).await;
        }
        let endpoint = request::image_endpoint_path(&opts);
        if !endpoint.is_empty() {
            return self.execute_images(auth, &req, &opts, endpoint).await;
        }
        if request::is_video_request(&opts) {
            return self.execute_videos(auth, &req, &opts).await;
        }
        self.execute_chat(auth, &req, &opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        self.execute_stream_chat(auth, &req, &opts).await
    }

    /// Refreshes the OAuth tokens with the stored refresh token; API-key credentials pass
    /// through unchanged.
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let cfg = self.config();
        let refresh_token = auth_metadata_string(auth, "refresh_token");
        if refresh_token.is_empty() {
            return Ok(auth.clone());
        }
        let token_endpoint = auth_metadata_string(auth, "token_endpoint");
        let proxy = effective_proxy_url("", Some(auth), Some(&cfg));
        let svc = cpa_auth::xai::XaiAuth::new(&proxy).map_err(|e| ExecError::new(0, e.to_string()))?;
        let td = svc
            .refresh_tokens(&refresh_token, &token_endpoint)
            .await
            .map_err(|e| ExecError::new(0, e.to_string()))?;
        let mut refreshed = auth.clone();
        cpa_auth::xai::apply_refresh_to_auth(&mut refreshed, &td, &token_endpoint);
        Ok(refreshed)
    }

    async fn count_tokens(&self, _auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.count_tokens_local(&req, &opts).await
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(XaiExecutor { cfg: self.cfg.clone(), api_key_view: true }))
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }
}
