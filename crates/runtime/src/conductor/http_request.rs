//! Credential-injecting HTTP helpers (Go: `Manager.PrepareHttpRequest`, `NewHttpRequest`,
//! `HttpRequest` in conductor_execution.go), used by routes that call a provider endpoint outside
//! the translated execute path (for example Codex Alpha Search).

use bytes::Bytes;
use cpa_auth::Auth;
use http::{HeaderMap, Method};

use super::Manager;
use super::models::executor_key_from_auth;
use crate::executor::{DynExecutor, ExecError};

fn coded(auth_code: &str, message: &str) -> ExecError {
    let mut e = ExecError::new(500, message);
    e.auth_code = Some(auth_code.into());
    e.upstream_attempted = false;
    e
}

impl Manager {
    /// The executor registered for the auth's provider key.
    fn http_executor(&self, auth: &Auth) -> Result<DynExecutor, ExecError> {
        let key = executor_key_from_auth(auth);
        if key.is_empty() {
            return Err(coded("provider_not_found", "auth provider is empty"));
        }
        self.executor(&key)
            .ok_or_else(|| coded("provider_not_found", &format!("executor not registered for provider: {key}")))
    }

    /// `PrepareHttpRequest`: injects provider credentials into `req`.
    pub async fn prepare_http_request(&self, auth: &Auth, req: &mut reqwest::Request) -> Result<(), ExecError> {
        self.http_executor(auth)?.prepare_request(req, auth).await
    }

    /// `NewHttpRequest`: builds a request and injects provider credentials into it.
    pub async fn new_http_request(
        &self,
        auth: &Auth,
        method: &str,
        url: &str,
        body: Option<Bytes>,
        headers: Option<&HeaderMap>,
    ) -> Result<reqwest::Request, ExecError> {
        let method = match method.trim() {
            "" => Method::GET,
            m => Method::from_bytes(m.as_bytes()).map_err(|e| coded("invalid_request", &e.to_string()))?,
        };
        let url = reqwest::Url::parse(url).map_err(|e| coded("invalid_request", &e.to_string()))?;
        let mut req = reqwest::Request::new(method, url);
        if let Some(h) = headers {
            *req.headers_mut() = h.clone();
        }
        if let Some(b) = body {
            *req.body_mut() = Some(reqwest::Body::from(b));
        }
        self.prepare_http_request(auth, &mut req).await?;
        Ok(req)
    }

    /// `HttpRequest`: injects provider credentials into `req` and executes it with the
    /// provider's HTTP client.
    pub async fn http_request(&self, auth: &Auth, req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        let exec = self.http_executor(auth)?;
        exec.http_request(auth, req).await
    }
}
