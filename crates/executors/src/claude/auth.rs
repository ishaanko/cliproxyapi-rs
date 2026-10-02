//! Claude credential preparation and token refresh (Go: claude_executor_auth.go).

use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use cpa_auth::Auth;
use cpa_auth::claude::{ClaudeAuth, ensure_device_id_pool};
use cpa_auth::error::AuthFlowError;
use cpa_runtime::executor::ExecError;
use serde_json::Value;

use super::helps::cli_identity_seed::{claude_cli_auth_identity_seed, stable_claude_cli_account_uuid};
use super::helps::credential_identity::{claude_credential_account_uuid, ensure_claude_credential_device_pool_required};
use super::request::{claude_creds, is_claude_oauth_token};

pub const CLAUDE_ACCOUNT_PROFILE_CHECKED_AT_KEY: &str = "claude_account_profile_checked_at";
const CLAUDE_ACCOUNT_PROFILE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether the credential needs its device id pool or account uuid filled in before a request
/// (Go: ShouldPrepareRequestAuth). Only OAuth tokens qualify.
pub fn should_prepare_request_auth(auth: &Auth) -> bool {
    let (api_key, _) = claude_creds(auth);
    if !is_claude_oauth_token(&api_key) {
        return false;
    }
    let mut probe = auth.metadata.clone();
    let (_, pool_was_rewritten) = ensure_device_id_pool(&mut probe);
    if pool_was_rewritten {
        return true;
    }
    claude_credential_account_uuid(auth).is_empty()
}

/// Setup tokens have no profile scope, so the account profile call is skipped
/// (Go: isClaudeSetupToken).
pub fn is_claude_setup_token(auth: &Auth, api_key: &str) -> bool {
    if !is_claude_oauth_token(api_key) {
        return false;
    }
    let flag = |key: &str| auth.metadata.get(key).and_then(Value::as_bool).unwrap_or(false);
    if flag("skip_account_profile") || flag("is_setup_token") || flag("setup_token") {
        return true;
    }
    let kind = auth.attributes.get("auth_kind").map(|k| k.to_lowercase()).unwrap_or_default();
    if kind == "setup_token" || kind == "setup-token" {
        return true;
    }
    let mut scopes = auth.meta_str("scopes").to_lowercase();
    if scopes.is_empty() {
        scopes = auth.meta_str("scope").to_lowercase();
    }
    !scopes.is_empty() && !scopes.contains("user:profile") && !scopes.contains("user:office")
}

/// Go: isClaudeOAuthScope403 (substring match over the profile error text).
pub fn is_claude_oauth_scope_403(message: &str) -> bool {
    let msg = message.to_lowercase();
    [
        "status 403",
        "403 forbidden",
        "403",
        "forbidden",
        "permission_error",
        "scope requirement",
        "insufficient_scope",
        "user:profile",
        "user:office",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

fn set_metadata_string(auth: &mut Auth, key: &str, value: &str) {
    if !value.trim().is_empty() {
        auth.metadata.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn fallback_identity(auth: &mut Auth, api_key: &str, fallback_prefix: &str) {
    let mut seed = claude_cli_auth_identity_seed(auth);
    if seed.is_empty() {
        seed = format!("{fallback_prefix}{api_key}");
    }
    let uuid = stable_claude_cli_account_uuid(&seed);
    set_metadata_string(auth, "account_uuid", &uuid);
    set_metadata_string(auth, CLAUDE_ACCOUNT_PROFILE_CHECKED_AT_KEY, &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
}

/// Fills the device id pool and account uuid on an OAuth credential (Go: PrepareRequestAuth).
/// Returns the updated credential; the conductor merges and persists it. A 403 from the profile
/// endpoint, an empty uuid and setup tokens fall back to a stable synthesized account uuid.
pub async fn prepare_request_auth(auth: &Auth, global_proxy: &str) -> Result<Option<Auth>, ExecError> {
    if !should_prepare_request_auth(auth) {
        return Ok(None);
    }
    let (api_key, _) = claude_creds(auth);
    let mut auth = auth.clone();
    ensure_claude_credential_device_pool_required(&mut auth);
    if !claude_credential_account_uuid(&auth).is_empty() {
        return Ok(Some(auth));
    }

    if is_claude_setup_token(&auth, &api_key) {
        fallback_identity(&mut auth, &api_key, "claude-setup-token|");
        return Ok(Some(auth));
    }

    let proxy = if auth.proxy_url.trim().is_empty() { global_proxy.to_string() } else { auth.proxy_url.trim().to_string() };
    let profile = match ClaudeAuth::new(&proxy) {
        Ok(service) => {
            match tokio::time::timeout(CLAUDE_ACCOUNT_PROFILE_TIMEOUT, service.fetch_oauth_profile(&api_key)).await {
                Ok(result) => result,
                Err(_) => Err(AuthFlowError::Transport("context deadline exceeded".to_string())),
            }
        }
        Err(e) => Err(e),
    };
    match profile {
        Err(err) => {
            let message = err.to_string();
            if is_claude_oauth_scope_403(&message) {
                tracing::debug!(
                    "Claude OAuth account profile lookup returned 403 for auth {}: {message} (falling back to stable credential identity)",
                    auth.id
                );
                fallback_identity(&mut auth, &api_key, "claude-oauth-fallback|");
                return Ok(Some(auth));
            }
            Err(ExecError::new(0, format!("populate Claude OAuth account profile: {message}")))
        }
        Ok(profile) => {
            if profile.account.uuid.trim().is_empty() {
                tracing::debug!(
                    "Claude OAuth account profile lookup returned empty account UUID for auth {} (falling back to stable credential identity)",
                    auth.id
                );
                fallback_identity(&mut auth, &api_key, "claude-oauth-fallback|");
                return Ok(Some(auth));
            }
            set_metadata_string(&mut auth, "account_uuid", &profile.account.uuid);
            set_metadata_string(&mut auth, "email", &profile.account.email);
            set_metadata_string(&mut auth, "organization_uuid", &profile.organization.uuid);
            set_metadata_string(&mut auth, "organization_name", &profile.organization.name);
            set_metadata_string(&mut auth, CLAUDE_ACCOUNT_PROFILE_CHECKED_AT_KEY, &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
            Ok(Some(auth))
        }
    }
}

/// Refreshes the OAuth tokens (Go: ClaudeExecutor.Refresh). An auth without a refresh token is
/// returned unchanged; identity fields are never erased.
pub async fn refresh(auth: &Auth, global_proxy: &str) -> Result<Auth, ExecError> {
    tracing::debug!("claude executor: refresh called");
    cpa_auth::refresh::refresh_auth(auth, global_proxy).await.map_err(|err| {
        let mut e = ExecError::new(err.status_code().unwrap_or(0), err.to_string());
        e.retry_after = err.retry_after();
        e
    })
}
