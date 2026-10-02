//! Kimi device-code OAuth for kimi.com and kimi.ai (internal/auth/kimi/*, sdk/auth/kimi.go).

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client_ext, read_text};
use crate::singleflight::SingleFlight;
use crate::storage::{KimiTokenStorage, TokenStorage};
use crate::types::Auth;
use crate::util::{encode_query, format_rfc3339_utc, now_rfc3339_local};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

pub const KIMI_DEFAULT_DOMAIN: &str = "kimi.com";
pub const KIMI_AI_DOMAIN: &str = "kimi.ai";
pub const KIMI_OAUTH_HOST: &str = "https://auth.kimi.com";
pub const KIMI_AI_OAUTH_HOST: &str = "https://auth.kimi.ai";
pub const KIMI_API_BASE_URL: &str = "https://api.kimi.com/coding";
pub const KIMI_AI_API_BASE_URL: &str = "https://api.kimi.ai/coding";

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);
/// `kimiRefreshLead`: 5 minutes.
pub const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);

static REFRESH_FLIGHT: LazyLock<SingleFlight<KimiTokenData>> = LazyLock::new(SingleFlight::default);

pub fn is_kimi_ai_domain(domain: &str) -> bool {
    let d = domain.trim().to_lowercase();
    d == "kimi.ai" || d == "ai" || d == "kimi-ai" || d.ends_with(".kimi.ai")
}

pub fn is_kimi_com_domain(domain: &str) -> bool {
    let d = domain.trim().to_lowercase();
    d == "kimi.com" || d == "com" || d == "kimi" || d.ends_with(".kimi.com")
}

fn url_host(raw: &str) -> String {
    url::Url::parse(raw.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .unwrap_or_default()
}

fn is_kimi_ai_host(raw: &str) -> bool {
    let h = url_host(raw);
    h == "kimi.ai" || h == "api.kimi.ai" || h == "auth.kimi.ai" || h.ends_with(".kimi.ai")
}

fn is_kimi_com_host(raw: &str) -> bool {
    let h = url_host(raw);
    h == "kimi.com" || h == "api.kimi.com" || h == "auth.kimi.com" || h.ends_with(".kimi.com")
}

/// `kimi.ai` for any ai-ish domain, else `kimi.com`.
pub fn normalize_kimi_domain(domain: &str) -> &'static str {
    if is_kimi_ai_domain(domain) {
        KIMI_AI_DOMAIN
    } else {
        KIMI_DEFAULT_DOMAIN
    }
}

pub fn resolve_kimi_oauth_host(domain: &str) -> &'static str {
    if is_kimi_ai_domain(domain) {
        KIMI_AI_OAUTH_HOST
    } else {
        KIMI_OAUTH_HOST
    }
}

pub fn resolve_kimi_api_base_url(domain: &str) -> &'static str {
    if is_kimi_ai_domain(domain) {
        KIMI_AI_API_BASE_URL
    } else {
        KIMI_API_BASE_URL
    }
}

/// Decides kimi.com vs kimi.ai for an existing auth by probing attributes, metadata, the token
/// storage, provider and file name, in that order.
pub fn resolve_kimi_domain_from_auth(auth: &Auth) -> &'static str {
    let classify_domain = |v: &str| -> Option<&'static str> {
        if is_kimi_ai_domain(v) {
            Some(KIMI_AI_DOMAIN)
        } else if is_kimi_com_domain(v) {
            Some(KIMI_DEFAULT_DOMAIN)
        } else {
            None
        }
    };
    let classify_url = |v: &str| -> Option<&'static str> {
        if is_kimi_ai_host(v) {
            Some(KIMI_AI_DOMAIN)
        } else if is_kimi_com_host(v) {
            Some(KIMI_DEFAULT_DOMAIN)
        } else {
            None
        }
    };

    if let Some(d) = auth.attributes.get("domain").filter(|d| !d.is_empty())
        && let Some(r) = classify_domain(d)
    {
        return r;
    }
    if let Some(b) = auth.attributes.get("base_url").filter(|b| !b.is_empty())
        && let Some(r) = classify_url(b)
    {
        return r;
    }
    for (key, by_url) in [("domain", false), ("base_url", true), ("type", false)] {
        let v = auth.meta_str(key);
        if v.is_empty() {
            continue;
        }
        let r = if by_url {
            classify_url(&v)
        } else {
            classify_domain(&v)
        };
        if let Some(r) = r {
            return r;
        }
    }
    if let Some(TokenStorage::Kimi(s)) = &auth.storage {
        if !s.domain.is_empty()
            && let Some(r) = classify_domain(&s.domain)
        {
            return r;
        }
        if !s.base_url.is_empty()
            && let Some(r) = classify_url(&s.base_url)
        {
            return r;
        }
        if !s.type_.is_empty()
            && let Some(r) = classify_domain(&s.type_)
        {
            return r;
        }
    }
    if !auth.provider.is_empty()
        && let Some(r) = classify_domain(&auth.provider)
    {
        return r;
    }
    let id = auth.id.to_lowercase();
    let file = auth.file_name.to_lowercase();
    if ["kimi-ai", "kimi.ai"]
        .iter()
        .any(|n| id.contains(n) || file.contains(n))
    {
        return KIMI_AI_DOMAIN;
    }
    KIMI_DEFAULT_DOMAIN
}

pub fn is_kimi_ai_auth(auth: &Auth) -> bool {
    is_kimi_ai_domain(resolve_kimi_domain_from_auth(auth))
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct KimiTokenData {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    /// Unix seconds; 0 when the server sent no `expires_in`.
    pub expires_at: i64,
    pub scope: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct KimiAuthBundle {
    pub token_data: KimiTokenData,
    pub device_id: String,
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
}

impl DeviceCodeResponse {
    /// URL the user should open: the complete one when present.
    pub fn verification_url(&self) -> &str {
        if self.verification_uri_complete.is_empty() {
            &self.verification_uri
        } else {
            &self.verification_uri_complete
        }
    }
}

/// Device-flow HTTP client for one kimi domain.
#[derive(Clone)]
pub struct DeviceFlowClient {
    client: reqwest::Client,
    device_id: String,
    domain: &'static str,
    oauth_host: String,
}

impl DeviceFlowClient {
    /// `device_id` empty generates a fresh UUID; `proxy_url` as per-auth/global proxy.
    pub fn new(domain: &str, device_id: &str, proxy_url: &str) -> Result<Self> {
        let client = build_client_ext(proxy_url, Some(Duration::from_secs(30)), None)?;
        Ok(Self::with_client(client, domain, device_id))
    }

    pub fn with_client(client: reqwest::Client, domain: &str, device_id: &str) -> Self {
        let domain = normalize_kimi_domain(domain);
        let device_id = device_id.trim();
        Self {
            client,
            device_id: if device_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                device_id.to_string()
            },
            domain,
            oauth_host: resolve_kimi_oauth_host(domain).to_string(),
        }
    }

    /// Points the OAuth endpoints at another host (tests).
    pub fn with_oauth_host(mut self, host: &str) -> Self {
        self.oauth_host = host.trim_end_matches('/').to_string();
        self
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    fn device_code_url(&self) -> String {
        format!("{}/api/oauth/device_authorization", self.oauth_host)
    }

    fn token_url(&self) -> String {
        format!("{}/api/oauth/token", self.oauth_host)
    }

    fn common_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("X-Msh-Platform", "CLIProxyAPI".to_string()),
            ("X-Msh-Version", crate::client_version()),
            ("X-Msh-Device-Name", hostname()),
            ("X-Msh-Device-Model", device_model()),
            ("X-Msh-Device-Id", self.device_id.clone()),
        ]
    }

    fn form_request(&self, url: &str, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(encode_query(pairs));
        for (k, v) in self.common_headers() {
            req = req.header(k, v);
        }
        req
    }

    pub async fn request_device_code(&self) -> Result<DeviceCodeResponse> {
        let resp = self
            .form_request(&self.device_code_url(), &[("client_id", CLIENT_ID)])
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "kimi: device code request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "kimi: failed to read device code response: {}",
                e.without_url()
            ))
        })?;
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "kimi: device code request failed with status {status}: {body}"
            )));
        }
        serde_json::from_str(&body).map_err(|e| {
            AuthFlowError::other(format!("kimi: failed to parse device code response: {e}"))
        })
    }

    /// Polls until authorized, denied or expired (`min(15 min, expires_in)`). `interval` is at
    /// least 5 s; `slow_down` keeps the interval, like the Go code.
    pub async fn poll_for_token(&self, device: &DeviceCodeResponse) -> Result<KimiTokenData> {
        self.poll_for_token_with_min_interval(device, DEFAULT_POLL_INTERVAL)
            .await
    }

    pub(crate) async fn poll_for_token_with_min_interval(
        &self,
        device: &DeviceCodeResponse,
        min_interval: Duration,
    ) -> Result<KimiTokenData> {
        let interval = crate::util::secs_to_duration(device.interval).max(min_interval);
        let mut deadline = crate::util::deadline_after(MAX_POLL_DURATION);
        if device.expires_in > 0 {
            deadline = deadline.min(crate::util::deadline_after(crate::util::secs_to_duration(
                device.expires_in,
            )));
        }
        loop {
            tokio::time::sleep(interval).await;
            if tokio::time::Instant::now() > deadline {
                return Err(AuthFlowError::other("kimi: device code expired"));
            }
            match self.exchange_device_code(&device.device_code).await? {
                Some(token) => return Ok(token),
                None => continue,
            }
        }
    }

    /// `Ok(None)` while authorization is pending.
    async fn exchange_device_code(&self, device_code: &str) -> Result<Option<KimiTokenData>> {
        let resp = self
            .form_request(
                &self.token_url(),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!("kimi: token request failed: {}", e.without_url()))
            })?;
        let (_, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "kimi: failed to read token response: {}",
                e.without_url()
            ))
        })?;

        #[derive(Deserialize, Default)]
        struct Oauth {
            #[serde(default)]
            error: String,
            #[serde(default)]
            error_description: String,
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            #[serde(default)]
            token_type: String,
            #[serde(default)]
            expires_in: f64,
            #[serde(default)]
            scope: String,
        }
        let oauth: Oauth = serde_json::from_str(&body).map_err(|e| {
            AuthFlowError::other(format!("kimi: failed to parse token response: {e}"))
        })?;
        match oauth.error.as_str() {
            "" => {}
            "authorization_pending" | "slow_down" => return Ok(None),
            "expired_token" => return Err(AuthFlowError::other("kimi: device code expired")),
            "access_denied" => return Err(AuthFlowError::other("kimi: access denied by user")),
            other => {
                return Err(AuthFlowError::other(format!(
                    "kimi: OAuth error: {other} - {}",
                    oauth.error_description
                )));
            }
        }
        if oauth.access_token.is_empty() {
            return Err(AuthFlowError::other("kimi: empty access token in response"));
        }
        Ok(Some(KimiTokenData {
            access_token: oauth.access_token,
            refresh_token: oauth.refresh_token,
            token_type: oauth.token_type,
            expires_at: expires_at(oauth.expires_in),
            scope: oauth.scope,
        }))
    }

    /// Refreshes tokens; single-flight per `tokenURL:refresh_token`.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<KimiTokenData> {
        let refresh_token = refresh_token.trim();
        if refresh_token.is_empty() {
            return Err(AuthFlowError::other("kimi: refresh token is required"));
        }
        let key = format!("{}:{}", self.token_url(), refresh_token);
        let this = self.clone();
        let rt = refresh_token.to_string();
        REFRESH_FLIGHT
            .run(&key, move || async move {
                this.refresh_single_flight(&rt).await
            })
            .await
    }

    async fn refresh_single_flight(&self, refresh_token: &str) -> Result<KimiTokenData> {
        let resp = self
            .form_request(
                &self.token_url(),
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ],
            )
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "kimi: refresh request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "kimi: failed to read refresh response: {}",
                e.without_url()
            ))
        })?;
        if status == 401 || status == 403 {
            return Err(AuthFlowError::Status {
                status,
                message: format!("kimi: refresh token rejected (status {status})"),
                retry_after: None,
            });
        }
        if status != 200 {
            return Err(AuthFlowError::Status {
                status,
                message: format!("kimi: refresh failed with status {status}: {body}"),
                retry_after: None,
            });
        }
        #[derive(Deserialize, Default)]
        struct R {
            #[serde(default)]
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            #[serde(default)]
            token_type: String,
            #[serde(default)]
            expires_in: f64,
            #[serde(default)]
            scope: String,
        }
        let r: R = serde_json::from_str(&body).map_err(|e| {
            AuthFlowError::other(format!("kimi: failed to parse refresh response: {e}"))
        })?;
        if r.access_token.is_empty() {
            return Err(AuthFlowError::other(
                "kimi: empty access token in refresh response",
            ));
        }
        Ok(KimiTokenData {
            access_token: r.access_token,
            refresh_token: r.refresh_token,
            token_type: r.token_type,
            expires_at: expires_at(r.expires_in),
            scope: r.scope,
        })
    }
}

fn expires_at(expires_in: f64) -> i64 {
    if expires_in > 0.0 {
        crate::util::unix_now_plus(expires_in as i64)
    } else {
        0
    }
}

fn expired_string(expires_at: i64) -> String {
    DateTime::from_timestamp(expires_at, 0)
        .map(format_rfc3339_utc)
        .unwrap_or_default()
}

fn hostname() -> String {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `macOS arm64`, `Linux amd64`, ... using Go's GOOS/GOARCH spellings.
fn device_model() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    match std::env::consts::OS {
        "macos" => format!("macOS {arch}"),
        "windows" => format!("Windows {arch}"),
        "linux" => format!("Linux {arch}"),
        other => format!("{other} {arch}"),
    }
}

/// Login-side wrapper: device flow + storage construction for one domain.
pub struct KimiAuth {
    device: DeviceFlowClient,
    domain: &'static str,
}

impl KimiAuth {
    pub fn new(domain: &str, proxy_url: &str) -> Result<Self> {
        let domain = normalize_kimi_domain(domain);
        Ok(Self {
            device: DeviceFlowClient::new(domain, "", proxy_url)?,
            domain,
        })
    }

    pub fn from_device_client(device: DeviceFlowClient) -> Self {
        let domain = device.domain;
        Self { device, domain }
    }

    pub async fn start_device_flow(&self) -> Result<DeviceCodeResponse> {
        self.device.request_device_code().await
    }

    pub async fn wait_for_authorization(
        &self,
        device: &DeviceCodeResponse,
    ) -> Result<KimiAuthBundle> {
        let token_data = self.device.poll_for_token(device).await?;
        Ok(KimiAuthBundle {
            token_data,
            device_id: self.device.device_id.clone(),
        })
    }

    pub fn create_token_storage(&self, bundle: &KimiAuthBundle) -> KimiTokenStorage {
        let ai = is_kimi_ai_domain(self.domain);
        KimiTokenStorage {
            access_token: bundle.token_data.access_token.clone(),
            refresh_token: bundle.token_data.refresh_token.clone(),
            token_type: bundle.token_data.token_type.clone(),
            scope: bundle.token_data.scope.clone(),
            device_id: bundle.device_id.trim().to_string(),
            expired: if bundle.token_data.expires_at > 0 {
                expired_string(bundle.token_data.expires_at)
            } else {
                String::new()
            },
            type_: if ai { "kimi-ai" } else { "kimi" }.to_string(),
            domain: self.domain.to_string(),
            base_url: resolve_kimi_api_base_url(self.domain).to_string(),
        }
    }
}

/// Builds the `Auth` record a successful kimi login yields (`KimiAuthenticator.Login` tail).
/// `provider_key` is `kimi`, `kimi-ai` or `kimi.ai`.
pub fn build_auth_record(
    provider_key: &str,
    domain: &str,
    bundle: &KimiAuthBundle,
    mut storage: KimiTokenStorage,
) -> Auth {
    let is_ai = is_kimi_ai_domain(domain);
    let (display, prefix, base_url) = if is_ai {
        ("Kimi.ai", "kimi-ai", KIMI_AI_API_BASE_URL)
    } else {
        ("Kimi", "kimi", KIMI_API_BASE_URL)
    };
    if is_ai {
        storage.type_ = provider_key.to_string();
    }

    let mut metadata = Metadata::new();
    metadata.insert("type".into(), provider_key.into());
    metadata.insert(
        "access_token".into(),
        bundle.token_data.access_token.clone().into(),
    );
    metadata.insert(
        "refresh_token".into(),
        bundle.token_data.refresh_token.clone().into(),
    );
    metadata.insert(
        "token_type".into(),
        bundle.token_data.token_type.clone().into(),
    );
    metadata.insert("scope".into(), bundle.token_data.scope.clone().into());
    metadata.insert("timestamp".into(), Utc::now().timestamp_millis().into());
    metadata.insert("domain".into(), domain.into());
    metadata.insert("base_url".into(), base_url.into());
    if bundle.token_data.expires_at > 0 {
        metadata.insert(
            "expired".into(),
            expired_string(bundle.token_data.expires_at).into(),
        );
    }
    if !bundle.device_id.trim().is_empty() {
        metadata.insert("device_id".into(), bundle.device_id.trim().into());
    }

    let file_name = format!("{prefix}-{}.json", Utc::now().timestamp_millis());
    let attributes: BTreeMap<String, String> = [
        ("base_url".to_string(), base_url.to_string()),
        ("domain".to_string(), domain.to_string()),
    ]
    .into();
    Auth {
        id: file_name.clone(),
        provider: provider_key.to_string(),
        file_name,
        label: format!("{display} User"),
        storage: Some(TokenStorage::Kimi(storage)),
        metadata,
        attributes,
        ..Default::default()
    }
}

/// Executor `Refresh` write-back for kimi auths.
pub fn apply_refresh_to_auth(auth: &mut Auth, td: &KimiTokenData) {
    let domain = resolve_kimi_domain_from_auth(auth);
    auth.metadata
        .insert("access_token".into(), td.access_token.clone().into());
    if !td.refresh_token.is_empty() {
        auth.metadata
            .insert("refresh_token".into(), td.refresh_token.clone().into());
    }
    if td.expires_at > 0 {
        auth.metadata
            .insert("expired".into(), expired_string(td.expires_at).into());
    }
    if auth
        .metadata
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        let t = if is_kimi_ai_domain(domain) {
            "kimi-ai"
        } else {
            "kimi"
        };
        auth.metadata.insert("type".into(), t.into());
    }
    if !auth.metadata.contains_key("domain") {
        auth.metadata.insert("domain".into(), domain.into());
    }
    if !auth.metadata.contains_key("base_url") {
        auth.metadata.insert(
            "base_url".into(),
            resolve_kimi_base_url_for_auth(auth).into(),
        );
    }
    let base_url = resolve_kimi_base_url_for_auth(auth);
    if let Some(TokenStorage::Kimi(s)) = auth.storage.as_mut() {
        s.access_token = td.access_token.clone();
        if !td.refresh_token.is_empty() {
            s.refresh_token = td.refresh_token.clone();
        }
        if td.expires_at > 0 {
            s.expired = expired_string(td.expires_at);
        }
        if s.domain.is_empty() {
            s.domain = domain.to_string();
        }
        if s.base_url.is_empty() {
            s.base_url = base_url;
        }
    }
    auth.metadata
        .insert("last_refresh".into(), now_rfc3339_local().into());
}

/// `helps.ResolveKimiBaseURL`: explicit attribute or metadata `base_url`, else the domain default.
fn resolve_kimi_base_url_for_auth(auth: &Auth) -> String {
    let attr = auth.attr("base_url").trim_end_matches('/').to_string();
    if !attr.is_empty() {
        return attr;
    }
    let meta = auth.meta_str("base_url").trim_end_matches('/').to_string();
    if !meta.is_empty() {
        return meta;
    }
    resolve_kimi_api_base_url(resolve_kimi_domain_from_auth(auth)).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_classification() {
        assert!(is_kimi_ai_domain(" KIMI.AI "));
        assert!(is_kimi_ai_domain("api.kimi.ai"));
        assert!(!is_kimi_ai_domain("kimi.com"));
        assert!(is_kimi_com_domain("com"));
        assert_eq!(normalize_kimi_domain("whatever"), "kimi.com");
        assert_eq!(resolve_kimi_oauth_host("ai"), "https://auth.kimi.ai");
        assert_eq!(
            resolve_kimi_api_base_url("kimi.com"),
            "https://api.kimi.com/coding"
        );
    }

    #[test]
    fn domain_from_auth_probe_order() {
        let mut a = Auth {
            provider: "kimi".into(),
            ..Default::default()
        };
        assert_eq!(resolve_kimi_domain_from_auth(&a), "kimi.com");
        a.metadata
            .insert("base_url".into(), "https://api.kimi.ai/coding".into());
        assert_eq!(resolve_kimi_domain_from_auth(&a), "kimi.ai");
        // Attribute beats metadata.
        a.attributes.insert("domain".into(), "kimi.com".into());
        assert_eq!(resolve_kimi_domain_from_auth(&a), "kimi.com");
        let b = Auth {
            id: "kimi-ai-123.json".into(),
            ..Default::default()
        };
        assert_eq!(resolve_kimi_domain_from_auth(&b), "kimi.ai");
    }

    #[test]
    fn auth_record_matches_go_login_output() {
        let bundle = KimiAuthBundle {
            token_data: KimiTokenData {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                token_type: "Bearer".into(),
                expires_at: 1_900_000_000,
                scope: "s".into(),
            },
            device_id: " dev ".into(),
        };
        let auth_svc = KimiAuth::from_device_client(DeviceFlowClient::with_client(
            reqwest::Client::new(),
            "kimi.ai",
            "d",
        ));
        let storage = auth_svc.create_token_storage(&bundle);
        assert_eq!(storage.expired, "2030-03-17T17:46:40Z");
        let auth = build_auth_record("kimi-ai", "kimi.ai", &bundle, storage);
        assert!(auth.id.starts_with("kimi-ai-") && auth.id.ends_with(".json"));
        assert_eq!(auth.label, "Kimi.ai User");
        assert_eq!(auth.metadata["device_id"], "dev");
        assert_eq!(auth.metadata["base_url"], "https://api.kimi.ai/coding");
        assert_eq!(auth.attr("domain"), "kimi.ai");
    }
}
