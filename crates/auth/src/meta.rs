//! Meta (Muse) device-code OAuth (internal/auth/meta/meta.go, sdk/auth/meta.go and the refresh half
//! of the Meta executor). The device flow yields a short-lived "DCA" token that is exchanged for a
//! long-lived API key (`mint_api_key`); that key becomes the credential's `access_token`.

use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client_ext, read_text};
use crate::storage::{MetaTokenStorage, TokenStorage};
use crate::types::Auth;
use crate::util::{encode_query, format_rfc3339_utc, now_rfc3339_utc, sha256_hex_prefix};

pub const DEFAULT_API_BASE_URL: &str = "https://api.meta.ai/v1";
pub const AUTH_HOST: &str = "https://auth.meta.com";
pub const DEVICE_AUTHORIZATION_ENDPOINT: &str = "https://auth.meta.com/oidc/device/authorization/";
pub const TOKEN_ENDPOINT: &str = "https://auth.meta.com/oidc/device/token/";
pub const CLIENT_ID: &str = "1031625952748946";
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const DEFAULT_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
pub const USER_AGENT: &str = "muse-code/1.0.2";
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct DeviceCodeResponse {
    #[serde(default)]
    pub device_code: String,
    #[serde(default)]
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub interval: i64,
    #[serde(skip)]
    pub token_endpoint: String,
}

impl DeviceCodeResponse {
    pub fn verification_url(&self) -> String {
        let c = self.verification_uri_complete.trim();
        if c.is_empty() {
            self.verification_uri.trim().to_string()
        } else {
            c.to_string()
        }
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
pub struct TokenData {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_description: String,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
pub struct MintedKeyResponse {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub user_email: String,
    #[serde(default)]
    pub user_full_name: String,
    #[serde(default)]
    pub subs_tier_name: String,
    #[serde(default)]
    pub subs_tier_id: String,
    #[serde(default)]
    pub is_subs_active: bool,
    #[serde(default)]
    pub has_payment_method: bool,
    #[serde(default)]
    pub require_payment: bool,
    #[serde(default)]
    pub can_subscribe: bool,
}

#[derive(Clone, Default)]
pub struct MetaAuthBundle {
    pub token_data: TokenData,
    pub minted_key: Option<MintedKeyResponse>,
    pub email: String,
    pub name: String,
}

#[derive(Clone)]
pub struct MetaAuth {
    client: reqwest::Client,
    mint_url: String,
    device_endpoint: String,
    token_endpoint: String,
    poll_floor: Option<Duration>,
}

impl MetaAuth {
    pub fn new(proxy_url: &str) -> Result<Self> {
        Ok(Self::with_client(build_client_ext(
            proxy_url,
            Some(HTTP_TIMEOUT),
            None,
        )?))
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            mint_url: String::new(),
            device_endpoint: DEVICE_AUTHORIZATION_ENDPOINT.into(),
            token_endpoint: TOKEN_ENDPOINT.into(),
            poll_floor: None,
        }
    }

    /// `SetMintURL`.
    pub fn set_mint_url(&mut self, url: &str) {
        self.mint_url = url.trim().to_string();
    }

    /// Redirects the device and token endpoints (tests, alternate deployments).
    pub fn with_endpoints(mut self, device_endpoint: &str, token_endpoint: &str) -> Self {
        self.device_endpoint = device_endpoint.to_string();
        self.token_endpoint = token_endpoint.to_string();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_poll_floor(mut self, d: Duration) -> Self {
        self.poll_floor = Some(d);
        self
    }

    pub async fn start_device_flow(&self) -> Result<DeviceCodeResponse> {
        let mut endpoint = self.device_endpoint.trim().to_string();
        if endpoint.is_empty() {
            endpoint = DEVICE_AUTHORIZATION_ENDPOINT.to_string();
        }
        let resp = self
            .client
            .post(&endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .body(encode_query(&[("client_id", CLIENT_ID)]))
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "meta device flow: request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "meta device flow: read response: {}",
                e.without_url()
            ))
        })?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::other(format!(
                "meta device flow failed (HTTP {status}): {}",
                body.trim()
            )));
        }
        let mut dcr: DeviceCodeResponse = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("meta device flow: parse response: {e}")))?;
        if dcr.device_code.trim().is_empty() || dcr.user_code.trim().is_empty() {
            return Err(AuthFlowError::other(
                "meta device flow: response missing required device_code or user_code",
            ));
        }
        dcr.token_endpoint = self.token_endpoint.clone();
        Ok(dcr)
    }

    /// Polls until the user approves, then mints an API key (a mint failure is only logged and the
    /// bundle keeps the DCA token). Bounded by `min(15 min, expires_in)`.
    pub async fn wait_for_authorization(&self, dcr: &DeviceCodeResponse) -> Result<MetaAuthBundle> {
        if dcr.device_code.is_empty() {
            return Err(AuthFlowError::other(
                "meta auth: missing device code response",
            ));
        }
        let mut max = MAX_POLL_DURATION;
        if dcr.expires_in > 0 {
            max = max.min(Duration::from_secs(dcr.expires_in as u64));
        }
        match tokio::time::timeout(max, self.poll_loop(dcr)).await {
            Ok(r) => r,
            Err(_) => Err(AuthFlowError::other(
                "meta auth: authorization timed out or canceled: context deadline exceeded",
            )),
        }
    }

    async fn poll_loop(&self, dcr: &DeviceCodeResponse) -> Result<MetaAuthBundle> {
        let token_endpoint = if dcr.token_endpoint.is_empty() {
            TOKEN_ENDPOINT
        } else {
            dcr.token_endpoint.as_str()
        };
        let mut interval = if dcr.interval > 0 {
            Duration::from_secs(dcr.interval as u64)
        } else {
            DEFAULT_POLL_INTERVAL
        };
        if let Some(floor) = self.poll_floor {
            interval = floor;
        }
        loop {
            tokio::time::sleep(interval).await;
            let resp = self
                .client
                .post(token_endpoint)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT)
                .body(encode_query(&[
                    ("grant_type", DEVICE_CODE_GRANT_TYPE),
                    ("device_code", &dcr.device_code),
                    ("client_id", CLIENT_ID),
                ]))
                .send()
                .await;
            let (status, body) = match resp {
                Ok(r) => match read_text(r).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "meta auth: read token response error: {} (retrying)",
                            e.without_url()
                        );
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "meta auth: poll request error: {} (retrying)",
                        e.without_url()
                    );
                    continue;
                }
            };

            if status == 200 {
                let mut token: TokenData = serde_json::from_str(&body).map_err(|e| {
                    AuthFlowError::other(format!("meta auth: parse token response: {e}"))
                })?;
                if token.access_token.is_empty() {
                    return Err(AuthFlowError::other(
                        "meta auth: response missing access_token",
                    ));
                }
                if token.expires_in > 0 {
                    token.expires_at = Utc::now().timestamp() + token.expires_in;
                }
                let mut bundle = MetaAuthBundle {
                    token_data: token.clone(),
                    ..Default::default()
                };
                match self.mint_api_key(&token.access_token).await {
                    Ok(minted) => {
                        if !minted.user_email.is_empty() {
                            bundle.email = minted.user_email.clone();
                        }
                        if !minted.user_full_name.is_empty() {
                            bundle.name = minted.user_full_name.clone();
                        }
                        bundle.minted_key = Some(minted);
                    }
                    Err(e) => {
                        tracing::warn!("meta auth: could not mint api_key from dca_token: {e}")
                    }
                }
                return Ok(bundle);
            }

            let err: TokenData = serde_json::from_str(&body).unwrap_or_default();
            match err.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += Duration::from_secs(5),
                "access_denied" => {
                    return Err(AuthFlowError::other("meta auth: access was denied by user"));
                }
                "expired_token" => {
                    return Err(AuthFlowError::other("meta auth: device code has expired"));
                }
                "" => tracing::warn!("meta auth: unexpected response {status}: {body}"),
                other => {
                    return Err(AuthFlowError::other(format!(
                        "meta auth: error from authorization server: {other}: {}",
                        err.error_description
                    )));
                }
            }
        }
    }

    /// Exchanges a DCA token for an API key. The mint URL is the configured one, else
    /// `META_MINT_URL`, else the default.
    pub async fn mint_api_key(&self, dca_token: &str) -> Result<MintedKeyResponse> {
        let dca_token = dca_token.trim();
        if dca_token.is_empty() {
            return Err(AuthFlowError::other("meta auth: missing dca token"));
        }
        let mut mint_url = self.mint_url.clone();
        if mint_url.is_empty() {
            mint_url = std::env::var("META_MINT_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_MINT_URL.to_string());
        }
        let resp = self
            .client
            .post(&mint_url)
            .header("Authorization", format!("Bearer {dca_token}"))
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(serde_json::json!({ "dca_token": dca_token }).to_string())
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "meta auth: mint request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "meta auth: read mint response: {}",
                e.without_url()
            ))
        })?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::other(format!(
                "meta auth: mint key failed (HTTP {status}): {}",
                body.trim()
            )));
        }
        let minted: MintedKeyResponse = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("meta auth: parse mint response: {e}")))?;
        if minted.api_key.trim().is_empty() {
            return Err(AuthFlowError::other(
                "meta auth: mint response missing api_key",
            ));
        }
        Ok(minted)
    }

    /// Builds the credential struct. With a minted key the key is the `access_token` and carries no
    /// expiry; without one the DCA token is used and expires with `dca_expired`.
    pub fn create_token_storage(&self, bundle: &MetaAuthBundle) -> MetaTokenStorage {
        let dca_expired = if bundle.token_data.expires_at > 0 {
            chrono::DateTime::from_timestamp(bundle.token_data.expires_at, 0)
                .map(format_rfc3339_utc)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let (mut api_key, mut base_url) = (String::new(), DEFAULT_API_BASE_URL.to_string());
        let (mut email, mut name) = (bundle.email.clone(), bundle.name.clone());
        if let Some(m) = &bundle.minted_key {
            api_key = m.api_key.clone();
            if !m.base_url.trim().is_empty() {
                base_url = m.base_url.trim().to_string();
            }
            if !m.user_email.is_empty() {
                email = m.user_email.clone();
            }
            if !m.user_full_name.is_empty() {
                name = m.user_full_name.clone();
            }
        }
        let (access_token, expired) = if api_key.is_empty() {
            (bundle.token_data.access_token.clone(), dca_expired.clone())
        } else {
            (api_key.clone(), String::new())
        };
        MetaTokenStorage {
            access_token,
            dca_token: bundle.token_data.access_token.clone(),
            api_key,
            token_type: bundle.token_data.token_type.clone(),
            expires_in: bundle.token_data.expires_in,
            expired,
            dca_expired,
            dca_expires_at: bundle.token_data.expires_at,
            last_refresh: now_rfc3339_utc(),
            base_url,
            email,
            name,
        }
    }
}

/// `meta-<sanitized email>-<hash16>.json`, else `meta-<hash16 of sub>.json`, else `meta-oauth.json`.
pub fn credential_file_name(email: &str, sub: &str) -> String {
    let clean = email.trim();
    if !clean.is_empty() {
        let mut sanitized: String = clean
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // The mapped string is ASCII-only per char but multi-byte inputs become one `_` each, so
        // truncating at 120 bytes is safe on a char boundary.
        sanitized.truncate(120);
        return format!("meta-{sanitized}-{}.json", sha256_hex_prefix(clean, 16));
    }
    let sub = sub.trim();
    if !sub.is_empty() {
        return format!("meta-{}.json", sha256_hex_prefix(sub, 16));
    }
    "meta-oauth.json".to_string()
}

/// Login tail of `MetaAuthenticator.Login`.
pub fn build_auth_record(storage: MetaTokenStorage, bundle: &MetaAuthBundle) -> Result<Auth> {
    if storage.access_token.trim().is_empty() {
        return Err(AuthFlowError::other(
            "meta token storage missing access token",
        ));
    }
    let file_name = credential_file_name(&storage.email, &storage.dca_token);
    let label = if storage.email.trim().is_empty() {
        "Meta".to_string()
    } else {
        storage.email.trim().to_string()
    };

    let mut m = Metadata::new();
    m.insert("type".into(), "meta".into());
    m.insert("access_token".into(), storage.access_token.clone().into());
    m.insert("token_type".into(), storage.token_type.clone().into());
    m.insert("expires_in".into(), storage.expires_in.into());
    m.insert("expired".into(), storage.expired.clone().into());
    m.insert("last_refresh".into(), storage.last_refresh.clone().into());
    m.insert("base_url".into(), storage.base_url.clone().into());
    m.insert("auth_kind".into(), "oauth".into());
    if !storage.dca_expired.is_empty() {
        m.insert("dca_expired".into(), storage.dca_expired.clone().into());
    }
    if storage.dca_expires_at > 0 {
        m.insert("dca_expires_at".into(), storage.dca_expires_at.into());
    }
    if !storage.api_key.is_empty() {
        m.insert("api_key".into(), storage.api_key.clone().into());
    }
    if !storage.dca_token.is_empty() {
        m.insert("dca_token".into(), storage.dca_token.clone().into());
    }
    if !storage.email.is_empty() {
        m.insert("email".into(), storage.email.clone().into());
    }
    if !storage.name.is_empty() {
        m.insert("name".into(), storage.name.clone().into());
    }
    if let Some(k) = &bundle.minted_key {
        m.insert("subs_tier_name".into(), k.subs_tier_name.clone().into());
        m.insert("subs_tier_id".into(), k.subs_tier_id.clone().into());
        m.insert("is_subs_active".into(), k.is_subs_active.into());
        m.insert("has_payment_method".into(), k.has_payment_method.into());
    }

    let mut auth = Auth::new(file_name, "meta");
    auth.label = label;
    auth.attributes.insert("auth_kind".into(), "oauth".into());
    auth.attributes
        .insert("base_url".into(), storage.base_url.clone());
    if !storage.api_key.is_empty() {
        auth.attributes
            .insert("api_key".into(), storage.api_key.clone());
    }
    if !storage.dca_token.is_empty() {
        auth.attributes
            .insert("dca_token".into(), storage.dca_token.clone());
    }
    if !storage.email.is_empty() {
        auth.attributes
            .insert("email".into(), storage.email.clone());
    }
    auth.storage = Some(TokenStorage::Meta(storage));
    auth.metadata = m;
    Ok(auth)
}

/// DCA token of an auth: attribute, `dca:`-prefixed access token, metadata, then storage.
pub fn extract_dca_token(auth: &Auth) -> String {
    if auth.is_config_api_key() {
        return String::new();
    }
    let attr = auth.attr("dca_token");
    if !attr.is_empty() {
        return attr;
    }
    let at = auth.attr("access_token");
    if at.starts_with("dca:") {
        return at;
    }
    let m = auth.meta_str("dca_token");
    if !m.is_empty() {
        return m;
    }
    let mat = auth.meta_str("access_token");
    if mat.starts_with("dca:") {
        return mat;
    }
    if let Some(TokenStorage::Meta(s)) = &auth.storage {
        if !s.dca_token.is_empty() {
            return s.dca_token.clone();
        }
        if s.access_token.starts_with("dca:") {
            return s.access_token.clone();
        }
    }
    String::new()
}

/// Executor `Refresh` write-back after a successful `mint_api_key`.
pub fn apply_mint_to_auth(auth: &mut Auth, dca_token: &str, minted: &MintedKeyResponse) {
    let mut base_url = {
        let a = auth.attr("base_url");
        if a.is_empty() {
            let m = auth.meta_str("base_url");
            if m.is_empty() {
                DEFAULT_API_BASE_URL.to_string()
            } else {
                m
            }
        } else {
            a
        }
    };
    if !minted.base_url.trim().is_empty() {
        base_url = minted.base_url.trim().to_string();
    }
    let now = crate::util::now_rfc3339_local();
    {
        let m = &mut auth.metadata;
        m.insert("base_url".into(), base_url.clone().into());
        m.insert("api_key".into(), minted.api_key.clone().into());
        m.insert("access_token".into(), minted.api_key.clone().into());
        m.insert("dca_token".into(), dca_token.into());
        m.shift_remove("expired");
        if !minted.user_email.is_empty() {
            m.insert("email".into(), minted.user_email.clone().into());
        }
        if !minted.user_full_name.is_empty() {
            m.insert("name".into(), minted.user_full_name.clone().into());
        }
        for (key, v) in [
            ("subs_tier_name", &minted.subs_tier_name),
            ("subs_tier_id", &minted.subs_tier_id),
        ] {
            if v.is_empty() {
                m.shift_remove(key);
            } else {
                m.insert(key.into(), v.clone().into());
            }
        }
        m.insert("is_subs_active".into(), minted.is_subs_active.into());
        m.insert(
            "has_payment_method".into(),
            minted.has_payment_method.into(),
        );
        m.insert("type".into(), "meta".into());
        m.insert("last_refresh".into(), now.clone().into());
    }
    auth.attributes.insert("base_url".into(), base_url.clone());
    auth.attributes
        .insert("api_key".into(), minted.api_key.clone());
    auth.attributes
        .insert("access_token".into(), minted.api_key.clone());
    if let Some(TokenStorage::Meta(s)) = auth.storage.as_mut() {
        s.api_key = minted.api_key.clone();
        s.access_token = minted.api_key.clone();
        s.dca_token = dca_token.to_string();
        s.expired.clear();
        s.base_url = base_url;
        s.last_refresh = now;
        if !minted.user_email.is_empty() {
            s.email = minted.user_email.clone();
        }
        if !minted.user_full_name.is_empty() {
            s.name = minted.user_full_name.clone();
        }
    }
    auth.last_refreshed_at = Some(Utc::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        let h = sha256_hex_prefix("a+b@x.io", 16);
        assert_eq!(
            credential_file_name(" a+b@x.io ", ""),
            format!("meta-a_b_x.io-{h}.json")
        );
        assert_eq!(
            credential_file_name("", "sub"),
            format!("meta-{}.json", sha256_hex_prefix("sub", 16))
        );
        assert_eq!(credential_file_name("", ""), "meta-oauth.json");
        let long = "a".repeat(200);
        assert!(credential_file_name(&long, "").len() < 150);
    }

    #[test]
    fn storage_prefers_minted_key() {
        let svc = MetaAuth::with_client(reqwest::Client::new());
        let mut bundle = MetaAuthBundle {
            token_data: TokenData {
                access_token: "dca:tok".into(),
                token_type: "Bearer".into(),
                expires_in: 3600,
                expires_at: 1_900_000_000,
                ..Default::default()
            },
            minted_key: None,
            email: "e@x".into(),
            name: String::new(),
        };
        let s = svc.create_token_storage(&bundle);
        assert_eq!(
            (s.access_token.as_str(), s.api_key.as_str()),
            ("dca:tok", "")
        );
        assert_eq!(s.expired, "2030-03-17T17:46:40Z");
        assert_eq!(s.base_url, DEFAULT_API_BASE_URL);

        bundle.minted_key = Some(MintedKeyResponse {
            api_key: "key-1".into(),
            base_url: "https://x/v1".into(),
            user_email: "m@x".into(),
            user_full_name: "M".into(),
            subs_tier_name: "pro".into(),
            ..Default::default()
        });
        let s = svc.create_token_storage(&bundle);
        assert_eq!(
            (
                s.access_token.as_str(),
                s.dca_token.as_str(),
                s.expired.as_str()
            ),
            ("key-1", "dca:tok", "")
        );
        assert_eq!(
            (s.email.as_str(), s.name.as_str(), s.base_url.as_str()),
            ("m@x", "M", "https://x/v1")
        );
        let auth = build_auth_record(s, &bundle).unwrap();
        assert_eq!(auth.attr("api_key"), "key-1");
        assert_eq!(auth.attr("dca_token"), "dca:tok");
        assert_eq!(auth.metadata["subs_tier_name"], "pro");
        assert_eq!(auth.label, "m@x");
    }

    #[test]
    fn dca_token_discovery_order_and_mint_write_back() {
        let mut a = Auth::new("meta-x.json", "meta");
        a.metadata.insert("access_token".into(), "dca:abc".into());
        assert_eq!(extract_dca_token(&a), "dca:abc");
        a.metadata.insert("dca_token".into(), "dca:real".into());
        assert_eq!(extract_dca_token(&a), "dca:real");
        a.attributes
            .insert("source".into(), "config:meta[abc]".into());
        a.attributes.insert("api_key".into(), "k".into());
        assert_eq!(extract_dca_token(&a), "");
        a.attributes.remove("source");
        a.attributes.remove("api_key");

        a.metadata
            .insert("expired".into(), "2020-01-01T00:00:00Z".into());
        a.metadata.insert("subs_tier_name".into(), "old".into());
        let minted = MintedKeyResponse {
            api_key: "k".into(),
            is_subs_active: true,
            ..Default::default()
        };
        apply_mint_to_auth(&mut a, "dca:real", &minted);
        assert_eq!(a.metadata["access_token"], "k");
        assert!(!a.metadata.contains_key("expired") && !a.metadata.contains_key("subs_tier_name"));
        assert_eq!(a.attr("base_url"), DEFAULT_API_BASE_URL);
        assert!(a.last_refreshed_at.is_some());
    }
}
