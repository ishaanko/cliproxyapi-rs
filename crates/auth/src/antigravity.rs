//! Antigravity (Google Cloud Code) OAuth (internal/auth/antigravity/*, sdk/auth/antigravity.go and
//! the refresh half of internal/runtime/executor/antigravity_executor_auth.go).
//!
//! Google authorization-code flow (no PKCE) with a local callback on port 51121, userinfo email
//! lookup, project discovery through `loadCodeAssist` / `onboardUser`, and refresh. Credentials
//! are metadata-only auth files (`type: antigravity`), no provider token struct.
//!
//! The Google OAuth client secret is not embedded in this repo. Set `CPA_ANTIGRAVITY_CLIENT_SECRET`
//! or call [`AntigravityAuth::with_client_secret`].

use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client, read_text};
use crate::singleflight::SingleFlight;
use crate::types::Auth;
use crate::util::{encode_query, expiry_local};

pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// Env var holding the Google OAuth client secret. Upstream constant:
/// router-for-me/CLIProxyAPI `internal/auth/antigravity/constants.go` (`ClientSecret`).
pub const CLIENT_SECRET_ENV: &str = "CPA_ANTIGRAVITY_CLIENT_SECRET";
pub const CALLBACK_PORT: u16 = 51121;

pub const SCOPES: [&str; 5] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const USER_INFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub const API_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const DAILY_API_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const API_VERSION: &str = "v1internal";

/// `AntigravityAuthenticator.RefreshLead()`: refresh 30 minutes before expiry.
pub const REFRESH_LEAD: Duration = Duration::from_secs(30 * 60);
const CREDENTIAL_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(30);

const FALLBACK_VERSION: &str = "2.9.1";
const HUB_PLATFORM: &str = "darwin/arm64";
const NODE_API_CLIENT_UA: &str = "google-api-nodejs-client/10.3.0";
const GOOG_API_CLIENT_UA: &str = "gl-node/22.21.1";

static VERSION: RwLock<String> = RwLock::new(String::new());
static REFRESH_FLIGHT: LazyLock<SingleFlight<TokenResponse>> = LazyLock::new(SingleFlight::default);

/// Antigravity client version used in User-Agent strings (Go caches a hub manifest value; this
/// crate keeps the fallback unless the app sets a fresher one).
pub fn antigravity_version() -> String {
    match VERSION.read() {
        Ok(v) if !v.is_empty() => v.clone(),
        _ => FALLBACK_VERSION.to_string(),
    }
}

pub fn set_antigravity_version(version: &str) {
    if let Ok(mut v) = VERSION.write() {
        *v = version.trim().to_string();
    }
}

/// `antigravity/hub/<version> darwin/arm64`.
pub fn hub_user_agent() -> String {
    format!("antigravity/hub/{} {HUB_PLATFORM}", antigravity_version())
}

/// Short runtime UA used by loadCodeAssist / userinfo (`AntigravityRequestUserAgent("")`).
pub fn request_user_agent() -> String {
    hub_user_agent()
}

/// Long control-plane UA used by onboardUser.
pub fn onboard_user_agent() -> String {
    format!("{} {NODE_API_CLIENT_UA}", hub_user_agent())
}

/// Version segment of an Antigravity UA (`antigravity/hub/<v> ...` or legacy `antigravity/<v> ...`).
pub fn version_from_user_agent(user_agent: &str) -> String {
    let ua = user_agent.trim();
    let lower = ua.to_lowercase();
    let rest = if lower.starts_with("antigravity/hub/") {
        &ua["antigravity/hub/".len()..]
    } else if lower.starts_with("antigravity/") {
        &ua["antigravity/".len()..]
    } else {
        return antigravity_version();
    };
    let v = rest.split([' ', '\t']).next().unwrap_or("").trim();
    if v.is_empty() {
        antigravity_version()
    } else {
        v.to_string()
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
pub struct TokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub token_type: String,
}

/// Endpoints, overridable for tests.
#[derive(Debug, Clone)]
pub struct AntigravityEndpoints {
    pub token: String,
    pub user_info: String,
    pub api: String,
    pub daily_api: String,
}

impl Default for AntigravityEndpoints {
    fn default() -> Self {
        Self {
            token: TOKEN_ENDPOINT.into(),
            user_info: USER_INFO_ENDPOINT.into(),
            api: API_ENDPOINT.into(),
            daily_api: DAILY_API_ENDPOINT.into(),
        }
    }
}

#[derive(Clone)]
pub struct AntigravityAuth {
    client: reqwest::Client,
    client_secret: Option<String>,
    endpoints: AntigravityEndpoints,
}

impl AntigravityAuth {
    pub fn new(proxy_url: &str) -> Result<Self> {
        Ok(Self::with_client(build_client(proxy_url, None)?))
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            client_secret: None,
            endpoints: AntigravityEndpoints::default(),
        }
    }

    /// Supplies the OAuth client secret explicitly instead of reading the environment.
    pub fn with_client_secret(mut self, secret: &str) -> Self {
        self.client_secret = Some(secret.to_string());
        self
    }

    pub fn with_endpoints(mut self, endpoints: AntigravityEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Fails fast when no client secret is configured (checked before starting a login).
    pub fn ensure_client_secret(&self) -> Result<()> {
        self.client_secret().map(|_| ())
    }

    fn client_secret(&self) -> Result<String> {
        if let Some(s) = self.client_secret.as_ref().filter(|s| !s.trim().is_empty()) {
            return Ok(s.clone());
        }
        match std::env::var(CLIENT_SECRET_ENV) {
            Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            _ => Err(AuthFlowError::Config(format!(
                "antigravity OAuth client secret is not configured: set {CLIENT_SECRET_ENV}"
            ))),
        }
    }

    /// Authorization URL (offline access, forced consent). Empty `redirect_uri` uses the default
    /// local callback.
    pub fn build_auth_url(&self, state: &str, redirect_uri: &str) -> String {
        let default_redirect = default_redirect_uri();
        let redirect = if redirect_uri.trim().is_empty() {
            default_redirect.as_str()
        } else {
            redirect_uri
        };
        let scope = SCOPES.join(" ");
        let query = encode_query(&[
            ("access_type", "offline"),
            ("client_id", CLIENT_ID),
            ("prompt", "consent"),
            ("redirect_uri", redirect),
            ("response_type", "code"),
            ("scope", &scope),
            ("state", state),
        ]);
        format!("{AUTH_ENDPOINT}?{query}")
    }

    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let secret = self.client_secret()?;
        let body = encode_query(&[
            ("code", code),
            ("client_id", CLIENT_ID),
            ("client_secret", &secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ]);
        let resp = self
            .client
            .post(&self.endpoints.token)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "antigravity token exchange: execute request: {}",
                    e.without_url()
                ))
            })?;
        let (status, text) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "antigravity token exchange: read response: {}",
                e.without_url()
            ))
        })?;
        if !(200..300).contains(&status) {
            return Err(status_error(
                "antigravity token exchange: request failed",
                status,
                &text,
            ));
        }
        serde_json::from_str(&text).map_err(|e| {
            AuthFlowError::other(format!("antigravity token exchange: decode response: {e}"))
        })
    }

    /// Email of the signed-in Google account.
    pub async fn fetch_user_info(&self, access_token: &str) -> Result<String> {
        let access_token = access_token.trim();
        if access_token.is_empty() {
            return Err(AuthFlowError::other(
                "antigravity userinfo: missing access token",
            ));
        }
        let resp = self
            .client
            .get(&self.endpoints.user_info)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", request_user_agent())
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "antigravity userinfo: execute request: {}",
                    e.without_url()
                ))
            })?;
        let (status, text) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "antigravity userinfo: read response: {}",
                e.without_url()
            ))
        })?;
        if !(200..300).contains(&status) {
            return Err(status_error(
                "antigravity userinfo: request failed",
                status,
                &text,
            ));
        }
        let info: Value = serde_json::from_str(&text).map_err(|e| {
            AuthFlowError::other(format!("antigravity userinfo: decode response: {e}"))
        })?;
        let email = info
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if email.is_empty() {
            return Err(AuthFlowError::other(
                "antigravity userinfo: response missing email",
            ));
        }
        Ok(email)
    }

    /// GCP project id for the account: `loadCodeAssist`, falling back to `onboardUser` polling.
    pub async fn fetch_project_id(&self, access_token: &str) -> Result<String> {
        let body = json!({ "metadata": { "ideType": "ANTIGRAVITY" } }).to_string();
        let url = format!("{}/{}:loadCodeAssist", self.endpoints.api, API_VERSION);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .header("User-Agent", request_user_agent())
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!("execute request: {}", e.without_url()))
            })?;
        let (status, text) = read_text(resp)
            .await
            .map_err(|e| AuthFlowError::Transport(format!("read response: {}", e.without_url())))?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::Status {
                status,
                message: format!("request failed with status {status}: {}", text.trim()),
            });
        }
        let load: Value = serde_json::from_str(&text)
            .map_err(|e| AuthFlowError::other(format!("decode response: {e}")))?;

        let project_id = extract_cloudaicompanion_project(&load);
        if !project_id.is_empty() {
            return Ok(project_id);
        }
        let onboarded = self
            .onboard_user(access_token, &default_tier_id(&load))
            .await?;
        if onboarded.is_empty() {
            return Err(AuthFlowError::other(
                "project id not found in loadCodeAssist or onboardUser response",
            ));
        }
        Ok(onboarded)
    }

    /// `onboardUser` against the daily endpoint, polling up to 5 times at 2 s intervals.
    pub async fn onboard_user(&self, access_token: &str, tier_id: &str) -> Result<String> {
        self.onboard_user_with_poll(access_token, tier_id, Duration::from_secs(2))
            .await
    }

    pub(crate) async fn onboard_user_with_poll(
        &self,
        access_token: &str,
        tier_id: &str,
        poll: Duration,
    ) -> Result<String> {
        tracing::info!("Antigravity: onboarding user with tier: {tier_id}");
        let user_agent = onboard_user_agent();
        let body = json!({
            "tier_id": tier_id,
            "metadata": {
                "ide_type": "ANTIGRAVITY",
                "ide_version": version_from_user_agent(&user_agent),
                "ide_name": "antigravity",
            }
        })
        .to_string();
        let max_attempts = 5;
        for attempt in 1..=max_attempts {
            tracing::debug!("Polling attempt {attempt}/{max_attempts}");
            let url = format!("{}/{}:onboardUser", self.endpoints.daily_api, API_VERSION);
            let resp = self
                .client
                .post(&url)
                .timeout(CREDENTIAL_ACQUISITION_TIMEOUT)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("Accept", "*/*")
                .header("Content-Type", "application/json")
                .header("User-Agent", &user_agent)
                .header("X-Goog-Api-Client", GOOG_API_CLIENT_UA)
                .body(body.clone())
                .send()
                .await
                .map_err(|e| {
                    AuthFlowError::Transport(format!("execute request: {}", e.without_url()))
                })?;
            let (status, text) = read_text(resp).await.map_err(|e| {
                AuthFlowError::Transport(format!("read response: {}", e.without_url()))
            })?;
            if status == 200 {
                let data: Value = serde_json::from_str(&text)
                    .map_err(|e| AuthFlowError::other(format!("decode response: {e}")))?;
                if data.get("done").and_then(Value::as_bool) == Some(true) {
                    let project = data
                        .get("response")
                        .map(extract_cloudaicompanion_project)
                        .unwrap_or_default();
                    if project.is_empty() {
                        return Err(AuthFlowError::other("no project_id in response"));
                    }
                    return Ok(project);
                }
                tokio::time::sleep(poll).await;
                continue;
            }
            let preview: String = text.trim().chars().take(200).collect();
            return Err(AuthFlowError::Status {
                status,
                message: format!("http {status}: {preview}"),
            });
        }
        Err(AuthFlowError::other(format!(
            "onboard user did not complete after {max_attempts} attempts"
        )))
    }

    /// Refreshes the access token. Single-flight per refresh token, 30 s bound. The real client
    /// uses Go's default User-Agent for this call.
    pub async fn refresh_tokens(&self, refresh_token: &str) -> Result<TokenResponse> {
        let refresh_token = refresh_token.trim();
        if refresh_token.is_empty() {
            return Err(AuthFlowError::Status {
                status: 401,
                message: "missing refresh token".into(),
            });
        }
        let secret = self.client_secret()?;
        let this = self.clone();
        let rt = refresh_token.to_string();
        REFRESH_FLIGHT
            .run(refresh_token, move || async move {
                match tokio::time::timeout(
                    CREDENTIAL_ACQUISITION_TIMEOUT,
                    this.refresh_single_flight(&rt, &secret),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(AuthFlowError::Transport(
                        "antigravity token refresh timed out".into(),
                    )),
                }
            })
            .await
    }

    async fn refresh_single_flight(
        &self,
        refresh_token: &str,
        secret: &str,
    ) -> Result<TokenResponse> {
        let body = encode_query(&[
            ("client_id", CLIENT_ID),
            ("client_secret", secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let resp = self
            .client
            .post(&self.endpoints.token)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", "Go-http-client/2.0")
            .body(body)
            .send()
            .await?;
        let (status, text) = read_text(resp).await?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::Status {
                status,
                message: text,
            });
        }
        serde_json::from_str(&text).map_err(|e| {
            AuthFlowError::other(format!("antigravity token refresh: decode response: {e}"))
        })
    }
}

fn status_error(prefix: &str, status: u16, body: &str) -> AuthFlowError {
    // Go reads at most 8 KiB of the error body.
    let body: String = body.chars().take(8 << 10).collect();
    let body = body.trim();
    let message = if body.is_empty() {
        format!("{prefix}: status {status}")
    } else {
        format!("{prefix}: status {status}: {body}")
    };
    AuthFlowError::Status { status, message }
}

pub fn default_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}/oauth-callback")
}

/// `cloudaicompanionProject`, `projectId` or `project`, as a string or an `{id}` object.
pub fn extract_cloudaicompanion_project(data: &Value) -> String {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        match data.get(key) {
            Some(Value::String(s)) => {
                let t = s.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
            Some(Value::Object(o)) => {
                if let Some(id) = o.get("id").and_then(Value::as_str) {
                    let t = id.trim();
                    if !t.is_empty() {
                        return t.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// Default allowed tier id, else the current tier, else `free-tier`.
pub fn default_tier_id(load_resp: &Value) -> String {
    if let Some(Value::Array(tiers)) = load_resp.get("allowedTiers") {
        for tier in tiers {
            if tier.get("isDefault").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if let Some(id) = tier.get("id").and_then(Value::as_str)
                && !id.trim().is_empty()
            {
                return id.trim().to_string();
            }
        }
    }
    if let Some(id) = load_resp
        .get("currentTier")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        && !id.trim().is_empty()
    {
        return id.trim().to_string();
    }
    "free-tier".to_string()
}

/// `antigravity-<email>.json`, or `antigravity.json` without an email.
pub fn credential_file_name(email: &str) -> String {
    let email = email.trim();
    if email.is_empty() {
        "antigravity.json".to_string()
    } else {
        format!("antigravity-{email}.json")
    }
}

/// `BuildAntigravityAuth`: metadata-only auth record for a fresh login.
pub fn build_auth(token: &TokenResponse, email: &str, project_id: &str) -> Auth {
    let now = chrono::Utc::now();
    let mut metadata = Metadata::new();
    metadata.insert("type".into(), "antigravity".into());
    metadata.insert("access_token".into(), token.access_token.clone().into());
    metadata.insert("refresh_token".into(), token.refresh_token.clone().into());
    metadata.insert("expires_in".into(), token.expires_in.into());
    metadata.insert("timestamp".into(), now.timestamp_millis().into());
    metadata.insert("expired".into(), expiry_local(token.expires_in).into());
    let email = email.trim();
    if !email.is_empty() {
        metadata.insert("email".into(), email.into());
    }
    let project_id = project_id.trim();
    if !project_id.is_empty() {
        metadata.insert("project_id".into(), project_id.into());
    }
    let file_name = credential_file_name(email);
    let mut auth = Auth::new(file_name, "antigravity");
    auth.label = if email.is_empty() {
        "antigravity".to_string()
    } else {
        email.to_string()
    };
    auth.metadata = metadata;
    auth
}

/// Executor `refreshToken` write-back (project id discovery is a separate call).
pub fn apply_refresh_to_auth(auth: &mut Auth, token: &TokenResponse) {
    let now = chrono::Utc::now();
    auth.metadata
        .insert("access_token".into(), token.access_token.clone().into());
    if !token.refresh_token.is_empty() {
        auth.metadata
            .insert("refresh_token".into(), token.refresh_token.clone().into());
    }
    auth.metadata
        .insert("expires_in".into(), token.expires_in.into());
    auth.metadata
        .insert("timestamp".into(), now.timestamp_millis().into());
    auth.metadata
        .insert("expired".into(), expiry_local(token.expires_in).into());
    auth.metadata.insert("type".into(), "antigravity".into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_has_offline_consent_and_sorted_keys() {
        let a = AntigravityAuth::with_client(reqwest::Client::new());
        let url = a.build_auth_url("st", "");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?access_type=offline&client_id=1071006060591-"));
        assert!(url.contains("&prompt=consent&redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback&response_type=code&scope="));
        assert!(url.contains(
            "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform+https%3A%2F%2F"
        ));
        assert!(url.ends_with("&state=st"));
    }

    #[test]
    fn project_extraction_and_tier_selection() {
        assert_eq!(
            extract_cloudaicompanion_project(&json!({"cloudaicompanionProject": " p1 "})),
            "p1"
        );
        assert_eq!(
            extract_cloudaicompanion_project(&json!({"project": {"id": "p2", "name": "n"}})),
            "p2"
        );
        assert_eq!(
            extract_cloudaicompanion_project(&json!({"projectId": ""})),
            ""
        );
        let load = json!({"allowedTiers": [{"id": "a"}, {"id": "legacy-tier", "isDefault": true}], "currentTier": {"id": "cur"}});
        assert_eq!(default_tier_id(&load), "legacy-tier");
        assert_eq!(
            default_tier_id(&json!({"currentTier": {"id": "cur"}})),
            "cur"
        );
        assert_eq!(default_tier_id(&json!({})), "free-tier");
    }

    #[test]
    fn user_agents_and_versions() {
        assert!(request_user_agent().starts_with("antigravity/hub/"));
        assert!(!request_user_agent().contains("google-api-nodejs-client"));
        assert!(onboard_user_agent().ends_with("google-api-nodejs-client/10.3.0"));
        assert_eq!(
            version_from_user_agent(
                "antigravity/hub/3.1.4 darwin/arm64 google-api-nodejs-client/10.3.0"
            ),
            "3.1.4"
        );
        assert_eq!(version_from_user_agent("antigravity/2.0.0"), "2.0.0");
    }

    #[test]
    fn built_auth_matches_go_metadata_shape() {
        let token = TokenResponse {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_in: 3599,
            token_type: "Bearer".into(),
        };
        let auth = build_auth(&token, " u@x.com ", "proj-1");
        assert_eq!(auth.id, "antigravity-u@x.com.json");
        assert_eq!(auth.label, "u@x.com");
        assert!(auth.storage.is_none());
        for key in [
            "type",
            "access_token",
            "refresh_token",
            "expires_in",
            "timestamp",
            "expired",
            "email",
            "project_id",
        ] {
            assert!(auth.metadata.contains_key(key), "missing {key}");
        }
        assert_eq!(auth.metadata["expires_in"], 3599);
        assert_eq!(credential_file_name(""), "antigravity.json");
        // Round trips through expiry derivation (expires_in + expired both present).
        assert!(auth.expiration_time().is_some());
    }

    #[test]
    fn missing_secret_is_a_config_error_not_a_panic() {
        // Only meaningful when the env var is unset in the test environment.
        if std::env::var(CLIENT_SECRET_ENV).is_ok() {
            return;
        }
        let a = AntigravityAuth::with_client(reqwest::Client::new());
        assert!(matches!(a.client_secret(), Err(AuthFlowError::Config(_))));
    }
}
