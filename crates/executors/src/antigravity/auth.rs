//! Token handling and project discovery (Go: antigravity_executor_auth.go).

use std::time::Duration;

use chrono::Utc;
use cpa_auth::antigravity::{AntigravityAuth, TokenResponse, apply_refresh_to_auth};
use cpa_auth::{Auth, AuthFlowError};
use cpa_config::Config;
use cpa_runtime::executor::ExecError;
use serde_json::Value;

use super::AntigravityExecutor;

/// A token is reused only when it stays valid for at least this long.
pub(crate) const REQUEST_TOKEN_SAFETY_WINDOW: Duration = Duration::from_secs(5 * 60);
const CREDENTIAL_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(30);

/// Trimmed metadata string (strings only).
pub(crate) fn meta_string(auth: &Auth, key: &str) -> String {
    auth.metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `metadata.project_id`, trimmed; empty when missing.
pub(crate) fn project_id_from_auth(auth: &Auth) -> String {
    meta_string(auth, "project_id")
}

/// `antigravity auth missing project_id[: cause]`, carrying the cause's status (default 400) and
/// retry hint.
pub(crate) fn missing_project_id_error(cause: Option<&ExecError>) -> ExecError {
    let mut msg = "antigravity auth missing project_id".to_string();
    let mut status = 400;
    let mut retry_after = None;
    if let Some(cause) = cause {
        msg = format!("{msg}: {}", cause.message);
        if cause.status > 0 {
            status = cause.status;
        }
        retry_after = cause.retry_after;
    }
    let mut err = ExecError::new(status, msg);
    err.retry_after = retry_after;
    err
}

/// Maps a login/refresh flow failure onto the executor's typed status error.
pub(crate) fn flow_error_to_exec(err: AuthFlowError) -> ExecError {
    match err {
        AuthFlowError::Status { status, message, retry_after } => {
            let mut e = ExecError::new(status, message);
            e.retry_after = retry_after;
            e
        }
        AuthFlowError::Refresh { status, message, .. } => ExecError::new(status, message),
        AuthFlowError::RetriesExhausted { source, .. } => flow_error_to_exec(*source),
        other => ExecError::new(0, other.to_string()),
    }
}

impl AntigravityExecutor {
    fn auth_service(&self, cfg: &Config, auth: &Auth) -> AntigravityAuth {
        let mut svc = AntigravityAuth::with_client(self.client(cfg, auth, "")).with_endpoints(self.endpoints.clone());
        if let Some(secret) = &self.client_secret {
            svc = svc.with_client_secret(secret);
        }
        svc
    }

    /// Returns a usable access token, refreshing (and returning the updated auth) when the
    /// current one is missing or expires within the safety window.
    pub(crate) async fn ensure_access_token(&self, cfg: &Config, auth: &Auth) -> Result<(String, Option<Auth>), ExecError> {
        let access_token = meta_string(auth, "access_token");
        if !access_token.is_empty()
            && let Some(expiry) = auth.expiration_time()
            && expiry > Utc::now() + chrono::Duration::from_std(REQUEST_TOKEN_SAFETY_WINDOW).unwrap_or_default()
        {
            self.maybe_refresh_credits_hint(cfg, auth, &access_token);
            return Ok((access_token, None));
        }
        let updated = self.refresh_token(cfg, auth.clone()).await?;
        Ok((meta_string(&updated, "access_token"), Some(updated)))
    }

    /// Exchanges the refresh token and writes the new token fields into `auth`; project discovery
    /// and the credits probe follow as best effort.
    pub(crate) async fn refresh_token(&self, cfg: &Config, mut auth: Auth) -> Result<Auth, ExecError> {
        let refresh_token = meta_string(&auth, "refresh_token");
        if refresh_token.is_empty() {
            return Err(ExecError::new(401, "missing refresh token"));
        }
        let svc = self.auth_service(cfg, &auth);
        let token: TokenResponse = svc.refresh_tokens(&refresh_token).await.map_err(flow_error_to_exec)?;
        apply_refresh_to_auth(&mut auth, &token);
        if let Err(err) = self.ensure_project_id(cfg, &mut auth, &token.access_token).await {
            tracing::warn!("antigravity executor: ensure project id failed: {}", err.message);
        }
        self.queue_credits_refresh(&auth, &token.access_token);
        Ok(auth)
    }

    async fn ensure_project_id(&self, cfg: &Config, auth: &mut Auth, access_token: &str) -> Result<(), ExecError> {
        if !project_id_from_auth(auth).is_empty() {
            return Ok(());
        }
        let project = self.fetch_project_id(cfg, auth, access_token).await?;
        if !project.is_empty() {
            auth.metadata.insert("project_id".into(), Value::String(project));
        }
        Ok(())
    }

    /// loadCodeAssist / onboardUser discovery, bounded to 30 s.
    pub(crate) async fn fetch_project_id(&self, cfg: &Config, auth: &Auth, access_token: &str) -> Result<String, ExecError> {
        let mut token = access_token.trim().to_string();
        if token.is_empty() {
            token = meta_string(auth, "access_token");
        }
        if token.is_empty() {
            return Ok(String::new());
        }
        let svc = self.auth_service(cfg, auth);
        match tokio::time::timeout(CREDENTIAL_ACQUISITION_TIMEOUT, svc.fetch_project_id(&token)).await {
            Ok(Ok(project)) => Ok(project.trim().to_string()),
            Ok(Err(err)) => Err(flow_error_to_exec(err)),
            Err(_) => Err(ExecError::new(0, "antigravity project discovery timed out")),
        }
    }

    /// Whether the conductor must call [`Self::prepare_request_auth`] before executing.
    pub(crate) fn needs_request_auth(auth: &Auth) -> bool {
        project_id_from_auth(auth).is_empty()
    }

    /// Refreshes the token if needed and discovers the missing project id.
    pub(crate) async fn prepare_request_auth_impl(&self, cfg: &Config, auth: &Auth) -> Result<Option<Auth>, ExecError> {
        if !Self::needs_request_auth(auth) {
            return Ok(None);
        }
        let mut updated = auth.clone();
        let (token, refreshed) = self.ensure_access_token(cfg, &updated).await?;
        if let Some(r) = refreshed {
            updated = r;
        }
        if !project_id_from_auth(&updated).is_empty() {
            return Ok(Some(updated));
        }
        let project = match self.fetch_project_id(cfg, &updated, &token).await {
            Ok(p) => p,
            Err(err) => return Err(missing_project_id_error(Some(&err))),
        };
        if project.is_empty() {
            return Err(missing_project_id_error(None));
        }
        updated.metadata.insert("project_id".into(), Value::String(project));
        Ok(Some(updated))
    }
}
