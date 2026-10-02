//! xAI (Grok) device-code OAuth (internal/auth/xai/*, sdk/auth/xai.go and the refresh half of the
//! xAI executor). Endpoints come from OIDC discovery and must live on x.ai over https.

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::credmeta::{Metadata, set_if_nonempty};
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client_ext, read_text};
use crate::jwt::parse_claims_map;
use crate::singleflight::SingleFlight;
use crate::storage::{TokenStorage, XaiTokenStorage};
use crate::types::Auth;
use crate::util::{encode_query, format_rfc3339_utc, now_rfc3339_utc};

pub const DEFAULT_API_BASE_URL: &str = "https://api.x.ai/v1";
pub const CLI_CHAT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub const ISSUER: &str = "https://auth.x.ai";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const MAX_POLL_DURATION: Duration = Duration::from_secs(30 * 60);
/// xAI refreshes 5 minutes before expiry.
pub const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

static REFRESH_FLIGHT: LazyLock<SingleFlight<TokenData>> = LazyLock::new(SingleFlight::default);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

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
    /// Not on the wire: the token endpoint to poll, filled from discovery.
    #[serde(skip)]
    pub token_endpoint: String,
}

impl DeviceCodeResponse {
    /// URL the user should open: the complete one when present.
    pub fn verification_url(&self) -> String {
        let c = self.verification_uri_complete.trim();
        if c.is_empty() {
            self.verification_uri.trim().to_string()
        } else {
            c.to_string()
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct TokenData {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expire: String,
    pub email: String,
    pub subject: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AuthBundle {
    pub token_data: TokenData,
    pub last_refresh: String,
    pub base_url: String,
    pub redirect_uri: String,
    pub token_endpoint: String,
}

/// `https` and a host of `x.ai` or `*.x.ai`.
pub fn validate_oauth_endpoint(raw: &str, field: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AuthFlowError::other(format!(
            "xai discovery {field} is empty"
        )));
    }
    let parsed = url::Url::parse(raw)
        .map_err(|e| AuthFlowError::other(format!("xai discovery {field} is invalid: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(AuthFlowError::other(format!(
            "xai discovery {field} must use https: {raw:?}"
        )));
    }
    let host = parsed.host_str().unwrap_or("").trim().to_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(AuthFlowError::other(format!(
            "xai discovery {field} host {host:?} is not on x.ai"
        )));
    }
    Ok(raw.to_string())
}

#[derive(Clone)]
pub struct XaiAuth {
    client: reqwest::Client,
    discovery_url: String,
    /// Test hook: floor/step for the poll interval (Go `minPollInterval`).
    min_poll_interval: Option<Duration>,
    /// Test hook: skip the x.ai host validation of discovery endpoints.
    skip_endpoint_validation: bool,
}

impl XaiAuth {
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
            discovery_url: DISCOVERY_URL.to_string(),
            min_poll_interval: None,
            skip_endpoint_validation: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(mut self, discovery_url: &str, min_poll: Duration) -> Self {
        self.discovery_url = discovery_url.to_string();
        self.min_poll_interval = Some(min_poll);
        self.skip_endpoint_validation = true;
        self
    }

    /// OIDC discovery of the device authorization and token endpoints.
    pub async fn discover(&self) -> Result<Discovery> {
        let resp = self
            .client
            .get(&self.discovery_url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "xai discovery: request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!("xai discovery: read response: {}", e.without_url()))
        })?;
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "xai discovery failed with status {status}: {}",
                body.trim()
            )));
        }
        #[derive(Deserialize, Default)]
        struct Payload {
            #[serde(default)]
            device_authorization_endpoint: String,
            #[serde(default)]
            token_endpoint: String,
        }
        let p: Payload = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("xai discovery: parse response: {e}")))?;
        if self.skip_endpoint_validation {
            return Ok(Discovery {
                device_authorization_endpoint: p.device_authorization_endpoint,
                token_endpoint: p.token_endpoint,
            });
        }
        Ok(Discovery {
            device_authorization_endpoint: validate_oauth_endpoint(
                &p.device_authorization_endpoint,
                "device_authorization_endpoint",
            )?,
            token_endpoint: validate_oauth_endpoint(&p.token_endpoint, "token_endpoint")?,
        })
    }

    pub async fn start_device_flow(&self) -> Result<DeviceCodeResponse> {
        let d = self.discover().await?;
        self.request_device_code(&d.device_authorization_endpoint, &d.token_endpoint)
            .await
    }

    pub async fn request_device_code(
        &self,
        device_authorization_endpoint: &str,
        token_endpoint: &str,
    ) -> Result<DeviceCodeResponse> {
        let endpoint = device_authorization_endpoint.trim();
        if endpoint.is_empty() {
            return Err(AuthFlowError::other(
                "xai device code: device authorization endpoint is required",
            ));
        }
        let resp = self
            .client
            .post(endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(encode_query(&[("client_id", CLIENT_ID), ("scope", SCOPE)]))
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "xai device code request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "xai device code: read response: {}",
                e.without_url()
            ))
        })?;
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "xai device code request failed with status {status}: {}",
                body.trim()
            )));
        }
        let mut dc: DeviceCodeResponse = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("xai device code: parse response: {e}")))?;
        if dc.device_code.trim().is_empty() {
            return Err(AuthFlowError::other(
                "xai device code: response missing device_code",
            ));
        }
        if dc.user_code.trim().is_empty() {
            return Err(AuthFlowError::other(
                "xai device code: response missing user_code",
            ));
        }
        if dc.verification_uri.trim().is_empty() && dc.verification_uri_complete.trim().is_empty() {
            return Err(AuthFlowError::other(
                "xai device code: response missing verification URI",
            ));
        }
        dc.token_endpoint = token_endpoint.trim().to_string();
        Ok(dc)
    }

    pub async fn wait_for_authorization(&self, device: &DeviceCodeResponse) -> Result<AuthBundle> {
        let token_data = self.poll_for_token(device).await?;
        Ok(AuthBundle {
            token_data,
            last_refresh: now_rfc3339_utc(),
            base_url: DEFAULT_API_BASE_URL.to_string(),
            redirect_uri: String::new(),
            token_endpoint: device.token_endpoint.trim().to_string(),
        })
    }

    /// Polls the token endpoint: first attempt immediately, then every `interval` (min 5 s,
    /// `slow_down` adds 5 s), until success, denial, or `min(30 min, expires_in)`.
    pub async fn poll_for_token(&self, device: &DeviceCodeResponse) -> Result<TokenData> {
        let mut token_endpoint = device.token_endpoint.trim().to_string();
        if token_endpoint.is_empty() {
            token_endpoint = self.discover().await?.token_endpoint;
        }
        let min_interval = self.min_poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL);
        let mut interval = Duration::from_secs(device.interval.max(0) as u64);
        if interval < min_interval {
            interval = min_interval;
        }
        let mut deadline = tokio::time::Instant::now() + MAX_POLL_DURATION;
        if device.expires_in > 0 {
            deadline = deadline
                .min(tokio::time::Instant::now() + Duration::from_secs(device.expires_in as u64));
        }

        let mut first = true;
        loop {
            if !first {
                tokio::time::sleep(interval).await;
                if tokio::time::Instant::now() > deadline {
                    return Err(AuthFlowError::other("xai device code expired"));
                }
            }
            first = false;
            match self
                .exchange_device_code(&token_endpoint, &device.device_code)
                .await?
            {
                PollOutcome::Token(t) => return Ok(t),
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => interval += min_interval,
            }
        }
    }

    async fn exchange_device_code(
        &self,
        token_endpoint: &str,
        device_code: &str,
    ) -> Result<PollOutcome> {
        let resp = self
            .client
            .post(token_endpoint.trim())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(encode_query(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("device_code", device_code.trim()),
                ("client_id", CLIENT_ID),
            ]))
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "xai device token request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "xai device token: read response: {}",
                e.without_url()
            ))
        })?;

        #[derive(Deserialize, Default)]
        struct Payload {
            #[serde(default)]
            error: String,
            #[serde(default)]
            error_description: String,
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            #[serde(default)]
            id_token: String,
            #[serde(default)]
            token_type: String,
            #[serde(default)]
            expires_in: i64,
        }
        let p: Payload = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("xai device token: parse response: {e}")))?;
        match p.error.as_str() {
            "" => {}
            "authorization_pending" => return Ok(PollOutcome::Pending),
            "slow_down" => return Ok(PollOutcome::SlowDown),
            "expired_token" => return Err(AuthFlowError::other("xai device code expired")),
            "access_denied" => return Err(AuthFlowError::other("xai device authorization denied")),
            other => {
                let desc = p.error_description.trim();
                return Err(AuthFlowError::other(if desc.is_empty() {
                    format!("xai device token error: {other}")
                } else {
                    format!("xai device token error: {other}: {desc}")
                }));
            }
        }
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "xai device token request failed with status {status}: {}",
                body.trim()
            )));
        }
        if p.access_token.trim().is_empty() {
            return Err(AuthFlowError::other(
                "xai device token response missing access_token",
            ));
        }
        let (email, subject) = parse_jwt_identity(&p.id_token);
        Ok(PollOutcome::Token(build_token_data(
            &p.access_token,
            &p.refresh_token,
            &p.id_token,
            &p.token_type,
            p.expires_in,
            email,
            subject,
        )))
    }

    /// Refreshes tokens, discovering the token endpoint when none is stored. Single-flight per
    /// refresh token.
    pub async fn refresh_tokens(
        &self,
        refresh_token: &str,
        token_endpoint: &str,
    ) -> Result<TokenData> {
        let refresh_token = refresh_token.trim();
        if refresh_token.is_empty() {
            return Err(AuthFlowError::other(
                "xai token refresh: refresh token is required",
            ));
        }
        let mut endpoint = token_endpoint.trim().to_string();
        if endpoint.is_empty() {
            endpoint = self.discover().await?.token_endpoint;
        }
        let this = self.clone();
        let rt = refresh_token.to_string();
        REFRESH_FLIGHT
            .run(refresh_token, move || async move {
                this.post_token_form(
                    &endpoint,
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", CLIENT_ID),
                        ("refresh_token", &rt),
                    ],
                )
                .await
            })
            .await
    }

    async fn post_token_form(
        &self,
        token_endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<TokenData> {
        let resp = self
            .client
            .post(token_endpoint.trim())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(encode_query(form))
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!("xai token request failed: {}", e.without_url()))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "xai token response: read body: {}",
                e.without_url()
            ))
        })?;
        if status != 200 {
            return Err(AuthFlowError::Status {
                status,
                message: format!(
                    "xai token request failed with status {status}: {}",
                    body.trim()
                ),
            });
        }
        #[derive(Deserialize, Default)]
        struct Payload {
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            #[serde(default)]
            id_token: String,
            #[serde(default)]
            token_type: String,
            #[serde(default)]
            expires_in: i64,
        }
        let p: Payload = serde_json::from_str(&body)
            .map_err(|e| AuthFlowError::other(format!("xai token response: parse body: {e}")))?;
        if p.access_token.trim().is_empty() {
            return Err(AuthFlowError::other(
                "xai token response missing access_token",
            ));
        }
        let (email, subject) = parse_jwt_identity(&p.id_token);
        Ok(build_token_data(
            &p.access_token,
            &p.refresh_token,
            &p.id_token,
            &p.token_type,
            p.expires_in,
            email,
            subject,
        ))
    }

    pub fn create_token_storage(&self, bundle: &AuthBundle) -> XaiTokenStorage {
        let base = bundle.base_url.trim();
        XaiTokenStorage {
            type_: "xai".into(),
            access_token: bundle.token_data.access_token.clone(),
            refresh_token: bundle.token_data.refresh_token.clone(),
            id_token: bundle.token_data.id_token.clone(),
            token_type: bundle.token_data.token_type.clone(),
            expires_in: bundle.token_data.expires_in,
            expire: bundle.token_data.expire.clone(),
            last_refresh: bundle.last_refresh.clone(),
            email: bundle.token_data.email.trim().to_string(),
            subject: bundle.token_data.subject.clone(),
            base_url: if base.is_empty() {
                DEFAULT_API_BASE_URL.to_string()
            } else {
                base.to_string()
            },
            redirect_uri: bundle.redirect_uri.clone(),
            token_endpoint: bundle.token_endpoint.clone(),
            auth_kind: "oauth".into(),
        }
    }
}

enum PollOutcome {
    Token(TokenData),
    Pending,
    SlowDown,
}

fn build_token_data(
    access: &str,
    refresh: &str,
    id_token: &str,
    token_type: &str,
    expires_in: i64,
    email: String,
    subject: String,
) -> TokenData {
    let expire = if expires_in > 0 {
        format_rfc3339_utc(chrono::Utc::now() + chrono::Duration::seconds(expires_in))
    } else {
        String::new()
    };
    TokenData {
        access_token: access.trim().to_string(),
        refresh_token: refresh.trim().to_string(),
        id_token: id_token.trim().to_string(),
        token_type: token_type.trim().to_string(),
        expires_in,
        expire,
        email,
        subject,
    }
}

/// `(email, sub)` from the id_token payload; empty when absent or unparseable.
pub fn parse_jwt_identity(token: &str) -> (String, String) {
    let Some(claims) = parse_claims_map(token) else {
        return (String::new(), String::new());
    };
    let get = |k: &str| {
        claims
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    (get("email"), get("sub"))
}

/// Letters, digits and `@._-` kept, everything else becomes `-`, then trimmed of `-`.
fn sanitize_file_segment(value: &str) -> String {
    let mapped: String = value
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    mapped.trim_matches('-').to_string()
}

/// `xai-<email>.json`, else `xai-<sub>.json`, else `xai-<unix millis>.json`.
pub fn credential_file_name(email: &str, subject: &str) -> String {
    let e = sanitize_file_segment(email);
    if !e.is_empty() {
        return format!("xai-{e}.json");
    }
    let s = sanitize_file_segment(subject);
    if !s.is_empty() {
        return format!("xai-{s}.json");
    }
    format!("xai-{}.json", chrono::Utc::now().timestamp_millis())
}

/// Login tail of `XAIAuthenticator.Login`: storage + metadata + attributes.
pub fn build_auth_record(storage: XaiTokenStorage) -> Result<Auth> {
    if storage.access_token.trim().is_empty() {
        return Err(AuthFlowError::other(
            "xai token storage missing access token",
        ));
    }
    let file_name = credential_file_name(&storage.email, &storage.subject);
    let label = if storage.email.trim().is_empty() {
        "xAI".to_string()
    } else {
        storage.email.trim().to_string()
    };

    let mut metadata = Metadata::new();
    metadata.insert("type".into(), "xai".into());
    metadata.insert("access_token".into(), storage.access_token.clone().into());
    metadata.insert("refresh_token".into(), storage.refresh_token.clone().into());
    metadata.insert("id_token".into(), storage.id_token.clone().into());
    metadata.insert("token_type".into(), storage.token_type.clone().into());
    metadata.insert("expires_in".into(), storage.expires_in.into());
    metadata.insert("expired".into(), storage.expire.clone().into());
    metadata.insert("last_refresh".into(), storage.last_refresh.clone().into());
    metadata.insert("base_url".into(), storage.base_url.clone().into());
    metadata.insert(
        "token_endpoint".into(),
        storage.token_endpoint.clone().into(),
    );
    metadata.insert("auth_kind".into(), "oauth".into());
    if !storage.email.is_empty() {
        metadata.insert("email".into(), storage.email.clone().into());
    }
    if !storage.subject.is_empty() {
        metadata.insert("sub".into(), storage.subject.clone().into());
    }

    let mut auth = Auth::new(file_name, "xai");
    auth.label = label;
    auth.attributes.insert("auth_kind".into(), "oauth".into());
    auth.attributes
        .insert("base_url".into(), storage.base_url.clone());
    auth.storage = Some(TokenStorage::Xai(storage));
    auth.metadata = metadata;
    Ok(auth)
}

/// Executor `Refresh` write-back.
pub fn apply_refresh_to_auth(auth: &mut Auth, td: &TokenData, token_endpoint: &str) {
    let m = &mut auth.metadata;
    m.insert("type".into(), "xai".into());
    m.insert("auth_kind".into(), "oauth".into());
    m.insert("access_token".into(), td.access_token.clone().into());
    set_if_nonempty(m, "refresh_token", &td.refresh_token);
    set_if_nonempty(m, "id_token", &td.id_token);
    set_if_nonempty(m, "token_type", &td.token_type);
    if td.expires_in > 0 {
        m.insert("expires_in".into(), td.expires_in.into());
    }
    set_if_nonempty(m, "expired", &td.expire);
    set_if_nonempty(m, "email", &td.email);
    set_if_nonempty(m, "sub", &td.subject);
    set_if_nonempty(m, "token_endpoint", token_endpoint);
    if crate::util::trimmed_str(m.get("base_url")).is_empty() {
        m.insert("base_url".into(), DEFAULT_API_BASE_URL.into());
    }
    m.insert("last_refresh".into(), now_rfc3339_utc().into());
    auth.attributes.insert("auth_kind".into(), "oauth".into());
    if auth.attr("base_url").is_empty() {
        auth.attributes
            .insert("base_url".into(), DEFAULT_API_BASE_URL.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_jwt;
    use serde_json::json;

    #[test]
    fn endpoint_validation_rejects_non_xai_hosts() {
        assert!(validate_oauth_endpoint("https://auth.x.ai/oauth2/token", "t").is_ok());
        assert!(validate_oauth_endpoint("https://x.ai/token", "t").is_ok());
        assert!(validate_oauth_endpoint("http://auth.x.ai/token", "t").is_err());
        assert!(validate_oauth_endpoint("https://evilx.ai/token", "t").is_err());
        assert!(validate_oauth_endpoint("https://auth.x.ai.evil.com/token", "t").is_err());
        assert!(validate_oauth_endpoint("", "t").is_err());
    }

    #[test]
    fn identity_and_file_names() {
        let t = make_jwt(&json!({"email": " U@x.ai ", "sub": "sub-1"}));
        assert_eq!(
            parse_jwt_identity(&t),
            ("U@x.ai".to_string(), "sub-1".to_string())
        );
        assert_eq!(
            parse_jwt_identity("garbage"),
            (String::new(), String::new())
        );
        assert_eq!(credential_file_name("a b@x.ai", ""), "xai-a-b@x.ai.json");
        assert_eq!(credential_file_name("", "sub/1"), "xai-sub-1.json");
        assert!(credential_file_name("", "").starts_with("xai-"));
    }

    #[test]
    fn auth_record_matches_go_shape() {
        let svc = XaiAuth::with_client(reqwest::Client::new());
        let bundle = AuthBundle {
            token_data: build_token_data(
                "at",
                "rt",
                "",
                "Bearer",
                3600,
                "u@x.ai".into(),
                "sub".into(),
            ),
            last_refresh: "2026-10-01T00:00:00Z".into(),
            base_url: "".into(),
            redirect_uri: "".into(),
            token_endpoint: "https://auth.x.ai/token".into(),
        };
        let storage = svc.create_token_storage(&bundle);
        assert_eq!(storage.base_url, DEFAULT_API_BASE_URL);
        let auth = build_auth_record(storage).unwrap();
        assert_eq!(auth.id, "xai-u@x.ai.json");
        assert_eq!(auth.label, "u@x.ai");
        assert_eq!(auth.attr("auth_kind"), "oauth");
        assert_eq!(auth.metadata["sub"], "sub");
        assert!(auth.expiration_time().is_some());
        assert!(build_auth_record(XaiTokenStorage::default()).is_err());
    }
}
