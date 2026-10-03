//! Native executor whose credential refresh is delegated to a plugin auth provider (Go:
//! `plugin_refresh_compat_executor.go`).
//!
//! Plugins often set `base_url` on their auths so host routing uses the built-in OpenAI
//! compatible executor; that executor's refresh is a no-op, so this wrapper keeps native
//! execution and sends refresh to `Host::refresh_auth`.

use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::Format;

use crate::ctx::CallCtx;
use crate::host::Host;

pub struct PluginRefreshCompatExecutor {
    inner: DynExecutor,
    host: Option<Arc<Host>>,
    provider: String,
}

impl PluginRefreshCompatExecutor {
    /// Wraps `inner` (Go: `NewPluginRefreshCompatExecutor`).
    pub fn new(inner: DynExecutor, host: Option<Arc<Host>>) -> Arc<Self> {
        let provider = inner.identifier().trim().to_lowercase();
        Arc::new(PluginRefreshCompatExecutor { inner, host, provider })
    }

    /// The native executor behind the wrapper.
    pub fn inner(&self) -> &DynExecutor {
        &self.inner
    }
}

fn has_refresh_token(auth: &Auth) -> bool {
    ["refresh_token", "refreshToken"]
        .iter()
        .any(|k| matches!(auth.metadata.get(*k), Some(serde_json::Value::String(s)) if !s.trim().is_empty()))
}

#[async_trait]
impl Executor for PluginRefreshCompatExecutor {
    fn identifier(&self) -> &str {
        if self.provider.is_empty() { self.inner.identifier() } else { &self.provider }
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.inner.execute(auth, req, opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        self.inner.execute_stream(auth, req, opts).await
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.inner.count_tokens(auth, req, opts).await
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        if let Some(host) = &self.host {
            match host.refresh_auth(&CallCtx::background(), auth).await {
                Ok(Some(refreshed)) => return Ok(refreshed),
                Ok(None) => {}
                Err(e) => return Err(ExecError::new(u16::try_from(e.status).unwrap_or(0), e.message)),
            }
        }
        if has_refresh_token(auth) {
            let mut provider = self.identifier().to_string();
            if provider.is_empty() {
                provider = auth.provider.trim().to_string();
            }
            return Err(ExecError::new(0, format!("plugin auth provider refresh is unavailable for provider {provider}")));
        }
        Ok(auth.clone())
    }

    async fn close_execution_session(&self, session_id: &str) {
        self.inner.close_execution_session(session_id).await;
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        let inner = self.inner.for_api_key().unwrap_or_else(|| self.inner.clone());
        Some(Arc::new(PluginRefreshCompatExecutor { inner, host: self.host.clone(), provider: self.provider.clone() }))
    }

    fn request_to_format(&self, req: &Request, opts: &Options) -> Option<Format> {
        self.inner.request_to_format(req, opts)
    }

    fn should_prepare_request_auth(&self, auth: &Auth) -> bool {
        self.inner.should_prepare_request_auth(auth)
    }

    async fn prepare_request_auth(&self, auth: &Auth) -> Result<Option<Auth>, ExecError> {
        self.inner.prepare_request_auth(auth).await
    }

    fn supports_apply_patch(&self, model: &str) -> bool {
        self.inner.supports_apply_patch(model)
    }
}
