//! Provider token refresh on an `Auth` record: the `Refresh` methods of the Go executors, applied
//! to a clone with the same metadata write-back rules. The conductor decides *when* to refresh
//! (see [`provider_refresh_lead`]); this module does the exchange and returns the updated auth.

use std::time::Duration;

use serde_json::Value;

use crate::antigravity::{self, AntigravityAuth};
use crate::claude::{self, ClaudeAuth};
use crate::codex::{self, CodexAuth};
use crate::error::{AuthFlowError, Result};
use crate::kimi::{self, DeviceFlowClient};
use crate::login::Provider;
use crate::meta::{self, MetaAuth};
use crate::types::Auth;
use crate::xai::{self, XaiAuth};

/// `ErrRefreshNotSupported`.
pub const NOT_SUPPORTED: &str = "cliproxy auth: refresh not supported";

/// `ProviderRefreshLead`: how long before expiry a provider's credentials are refreshed.
pub fn provider_refresh_lead(provider: &str) -> Option<Duration> {
    Provider::parse(provider).and_then(Provider::refresh_lead)
}

/// Whether [`refresh_auth`] has an implementation for the auth's provider.
pub fn supports_refresh(auth: &Auth) -> bool {
    matches!(
        Provider::parse(&auth.provider),
        Some(
            Provider::Claude
                | Provider::Codex
                | Provider::Antigravity
                | Provider::Xai
                | Provider::Kimi
                | Provider::KimiAi
                | Provider::KimiAiDot
                | Provider::Meta
        )
    )
}

/// Refreshes `auth` and returns the updated clone. An auth without refresh credentials is returned
/// unchanged (like the Go executors); providers without refresh (devin, vertex, ...) error.
///
/// `global_proxy` is used when the auth has no proxy of its own.
pub async fn refresh_auth(auth: &Auth, global_proxy: &str) -> Result<Auth> {
    let proxy = if auth.proxy_url.trim().is_empty() {
        global_proxy.to_string()
    } else {
        auth.proxy_url.trim().to_string()
    };
    let mut updated = auth.clone();
    let provider = Provider::parse(&auth.provider);
    match provider {
        Some(Provider::Claude) => {
            let rt = auth.refresh_token();
            if rt.is_empty() {
                return Ok(updated);
            }
            let td = ClaudeAuth::new(&proxy)?
                .refresh_tokens_with_retry(&rt, 3)
                .await?;
            claude::apply_refresh_to_auth(&mut updated, &td);
        }
        Some(Provider::Codex) => {
            let rt = auth.meta_str("refresh_token");
            if rt.is_empty() {
                return Ok(updated);
            }
            let td = CodexAuth::new(&proxy)?
                .refresh_tokens_with_retry(&rt, 3)
                .await?;
            codex::apply_refresh_to_auth(&mut updated, &td);
        }
        Some(Provider::Antigravity) => refresh_antigravity(&mut updated, &proxy).await?,
        Some(Provider::Xai) => {
            let rt = auth.meta_str("refresh_token");
            if rt.is_empty() {
                return Ok(updated);
            }
            let endpoint = auth.meta_str("token_endpoint");
            let td = XaiAuth::new(&proxy)?.refresh_tokens(&rt, &endpoint).await?;
            xai::apply_refresh_to_auth(&mut updated, &td, &endpoint);
        }
        Some(Provider::Kimi | Provider::KimiAi | Provider::KimiAiDot) => {
            let rt = auth.meta_str("refresh_token");
            if rt.is_empty() {
                return Ok(updated);
            }
            let domain = kimi::resolve_kimi_domain_from_auth(auth);
            let device_id = auth.meta_str("device_id");
            let client = DeviceFlowClient::new(domain, &device_id, &proxy)?;
            let td = client.refresh_token(&rt).await?;
            kimi::apply_refresh_to_auth(&mut updated, &td);
        }
        Some(Provider::Meta) => refresh_meta(&mut updated, &proxy).await?,
        Some(Provider::Devin) | None => return Err(AuthFlowError::other(NOT_SUPPORTED)),
    }
    Ok(updated)
}

async fn refresh_antigravity(auth: &mut Auth, proxy: &str) -> Result<()> {
    let rt = auth.meta_str("refresh_token");
    let svc = AntigravityAuth::new(proxy)?;
    let token = svc.refresh_tokens(&rt).await?;
    antigravity::apply_refresh_to_auth(auth, &token);
    // Project discovery is best effort here: a failure only logs, like `ensureAntigravityProjectID`.
    if auth.meta_str("project_id").is_empty() {
        match svc.fetch_project_id(&token.access_token).await {
            Ok(p) if !p.trim().is_empty() => {
                auth.metadata
                    .insert("project_id".into(), Value::String(p.trim().to_string()));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("antigravity executor: ensure project id failed: {e}"),
        }
    }
    Ok(())
}

/// Meta "refresh" mints an API key from the DCA token; an auth that already has a usable key and
/// no DCA token is left alone.
async fn refresh_meta(auth: &mut Auth, proxy: &str) -> Result<()> {
    let dca = meta::extract_dca_token(auth);
    if dca.is_empty() {
        let has_token = !auth.attr("api_key").is_empty()
            || !auth.meta_str("api_key").is_empty()
            || !auth.meta_str("access_token").is_empty();
        if has_token {
            return Ok(());
        }
        return Err(AuthFlowError::Status {
            status: 401,
            message: "meta executor: missing API key or DCA token".into(),
            retry_after: None,
        });
    }
    let minted = MetaAuth::new(proxy)?
        .mint_api_key(&dca)
        .await
        .map_err(|e| AuthFlowError::other(format!("meta executor: mint API key failed: {e}")))?;
    meta::apply_mint_to_auth(auth, &dca, &minted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_leads_by_provider_key() {
        assert_eq!(
            provider_refresh_lead("codex"),
            Some(Duration::from_secs(86_400))
        );
        assert_eq!(
            provider_refresh_lead("Claude"),
            Some(Duration::from_secs(14_400))
        );
        assert_eq!(
            provider_refresh_lead("kimi-ai"),
            Some(Duration::from_secs(300))
        );
        assert_eq!(provider_refresh_lead("xai"), Some(Duration::from_secs(300)));
        assert_eq!(provider_refresh_lead("meta"), None);
        assert_eq!(provider_refresh_lead("vertex"), None);
    }

    #[tokio::test]
    async fn auth_without_refresh_token_is_returned_unchanged_and_unsupported_errors() {
        for provider in ["claude", "codex", "xai", "kimi"] {
            let mut a = Auth::new("x.json", provider);
            a.metadata.insert("access_token".into(), "at".into());
            let out = refresh_auth(&a, "").await.unwrap();
            assert_eq!(out.metadata, a.metadata, "{provider}");
        }
        let vertex = Auth::new("v.json", "vertex");
        assert!(
            refresh_auth(&vertex, "")
                .await
                .unwrap_err()
                .to_string()
                .contains("refresh not supported")
        );
        assert!(!supports_refresh(&vertex) && supports_refresh(&Auth::new("c.json", "claude")));
        // Meta with a usable key and no DCA token is a no-op; with nothing it is a 401.
        let mut m = Auth::new("m.json", "meta");
        m.metadata.insert("access_token".into(), "key".into());
        assert!(refresh_auth(&m, "").await.is_ok());
        let err = refresh_auth(&Auth::new("m2.json", "meta"), "")
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), Some(401));
    }
}
