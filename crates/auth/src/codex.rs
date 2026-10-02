//! Codex / OpenAI OAuth (internal/auth/codex/*, sdk/auth/codex.go, sdk/auth/codex_device.go).
//!
//! Public PKCE client against auth.openai.com with a local callback on port 1455 (redirect URI is
//! fixed to `localhost:1455` even when the listener port is overridden), form-encoded code exchange
//! and refresh, `id_token` JWT identity parsing, and the device-code login variant.

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::credmeta::Metadata;
use crate::error::{AuthErrorKind, AuthFlowError, Result};
use crate::http::{build_client, read_text};
use crate::jwt::{DEFAULT_PLAN_TYPE, parse_codex_id_token};
use crate::pkce::PkceCodes;
use crate::singleflight::SingleFlight;
use crate::storage::{CodexTokenStorage, TokenStorage};
use crate::types::Auth;
use crate::util::{encode_query, expiry_local, now_rfc3339_local, sha256_hex_prefix};

pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// `CodexAuthenticator.RefreshLead()`: refresh 24 hours before expiry.
pub const REFRESH_LEAD: Duration = Duration::from_secs(24 * 3600);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

pub const LOGIN_MODE_METADATA_KEY: &str = "codex_login_mode";
pub const LOGIN_MODE_DEVICE: &str = "device";
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const DEVICE_TOKEN_EXCHANGE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEVICE_DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

static REFRESH_FLIGHT: LazyLock<SingleFlight<CodexTokenData>> =
    LazyLock::new(SingleFlight::default);

#[derive(Clone, Default, PartialEq, Eq)]
pub struct CodexTokenData {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    /// RFC3339 expiry (`expired` key of the credential file).
    pub expire: String,
    pub plan_type: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct CodexAuthBundle {
    pub token_data: CodexTokenData,
    pub last_refresh: String,
}

#[derive(Debug, Deserialize, Default)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// Endpoints, overridable for tests.
#[derive(Debug, Clone)]
pub struct CodexEndpoints {
    pub token_url: String,
    pub device_user_code_url: String,
    pub device_token_url: String,
}

impl Default for CodexEndpoints {
    fn default() -> Self {
        Self {
            token_url: TOKEN_URL.into(),
            device_user_code_url: DEVICE_USER_CODE_URL.into(),
            device_token_url: DEVICE_TOKEN_URL.into(),
        }
    }
}

#[derive(Clone)]
pub struct CodexAuth {
    client: reqwest::Client,
    endpoints: CodexEndpoints,
}

impl CodexAuth {
    pub fn new(proxy_url: &str) -> Result<Self> {
        Ok(Self::with_client(build_client(proxy_url, None)?))
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            endpoints: CodexEndpoints::default(),
        }
    }

    pub fn with_endpoints(mut self, endpoints: CodexEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Authorization URL with sorted query keys (`url.Values.Encode`).
    pub fn generate_auth_url(&self, state: &str, pkce: &PkceCodes) -> String {
        let query = encode_query(&[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "openid email profile offline_access"),
            ("state", state),
            ("code_challenge", &pkce.code_challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "login"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ]);
        format!("{AUTH_URL}?{query}")
    }

    pub async fn exchange_code_for_tokens(
        &self,
        code: &str,
        pkce: &PkceCodes,
    ) -> Result<CodexAuthBundle> {
        self.exchange_code_for_tokens_with_redirect(code, REDIRECT_URI, pkce)
            .await
    }

    /// Code exchange with an explicit redirect URI (device flow uses its own).
    pub async fn exchange_code_for_tokens_with_redirect(
        &self,
        code: &str,
        redirect_uri: &str,
        pkce: &PkceCodes,
    ) -> Result<CodexAuthBundle> {
        let redirect_uri = redirect_uri.trim();
        if redirect_uri.is_empty() {
            return Err(AuthFlowError::other(
                "redirect URI is required for token exchange",
            ));
        }
        let body = encode_query(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", &pkce.code_verifier),
        ]);
        let resp = self
            .client
            .post(&self.endpoints.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "token exchange request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, text) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "failed to read token response: {}",
                e.without_url()
            ))
        })?;
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "token exchange failed with status {status}: {text}"
            )));
        }
        let token: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| AuthFlowError::other(format!("failed to parse token response: {e}")))?;
        Ok(CodexAuthBundle {
            token_data: token_data_from(&token, "Failed to parse ID token"),
            last_refresh: now_rfc3339_local(),
        })
    }

    /// Refreshes tokens; single-flight per refresh token with a 30 s bound, detached from the
    /// caller's cancellation.
    pub async fn refresh_tokens(&self, refresh_token: &str) -> Result<CodexTokenData> {
        if refresh_token.is_empty() {
            return Err(AuthFlowError::other("refresh token is required"));
        }
        let this = self.clone();
        let rt = refresh_token.to_string();
        REFRESH_FLIGHT
            .run(refresh_token, move || async move {
                match tokio::time::timeout(REFRESH_TIMEOUT, this.refresh_single_flight(&rt)).await {
                    Ok(r) => r,
                    Err(_) => Err(AuthFlowError::Transport(
                        "token refresh request failed: timed out".into(),
                    )),
                }
            })
            .await
    }

    async fn refresh_single_flight(&self, refresh_token: &str) -> Result<CodexTokenData> {
        let body = encode_query(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", "openid profile email"),
        ]);
        let resp = self
            .client
            .post(&self.endpoints.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "token refresh request failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, text) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "failed to read refresh response: {}",
                e.without_url()
            ))
        })?;
        if status != 200 {
            return Err(AuthFlowError::Status {
                status,
                message: format!("token refresh failed with status {status}: {text}"),
                retry_after: None,
            });
        }
        let token: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| AuthFlowError::other(format!("failed to parse refresh response: {e}")))?;
        Ok(token_data_from(
            &token,
            "Failed to parse refreshed ID token",
        ))
    }

    /// Up to `max_retries` attempts, sleeping `attempt` seconds between; a reused refresh token is
    /// terminal and aborts immediately.
    pub async fn refresh_tokens_with_retry(
        &self,
        refresh_token: &str,
        max_retries: u32,
    ) -> Result<CodexTokenData> {
        let mut last_err = None;
        for attempt in 0..max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
            }
            match self.refresh_tokens(refresh_token).await {
                Ok(d) => return Ok(d),
                Err(e) => {
                    if is_non_retryable_refresh_err(&e) {
                        tracing::warn!(
                            "Token refresh attempt {} failed with non-retryable error: {e}",
                            attempt + 1
                        );
                        return Err(e);
                    }
                    tracing::warn!("Token refresh attempt {} failed: {e}", attempt + 1);
                    last_err = Some(e);
                }
            }
        }
        match last_err {
            Some(source) => Err(AuthFlowError::RetriesExhausted {
                attempts: max_retries,
                source: Box::new(source),
            }),
            None => Err(AuthFlowError::other(format!(
                "token refresh failed after {max_retries} attempts"
            ))),
        }
    }

    pub fn create_token_storage(&self, bundle: &CodexAuthBundle) -> CodexTokenStorage {
        let plan = bundle.token_data.plan_type.trim();
        CodexTokenStorage {
            id_token: bundle.token_data.id_token.clone(),
            access_token: bundle.token_data.access_token.clone(),
            refresh_token: bundle.token_data.refresh_token.clone(),
            account_id: bundle.token_data.account_id.clone(),
            last_refresh: bundle.last_refresh.clone(),
            email: bundle.token_data.email.clone(),
            expire: bundle.token_data.expire.clone(),
            plan_type: if plan.is_empty() {
                DEFAULT_PLAN_TYPE.to_string()
            } else {
                plan.to_string()
            },
            ..Default::default()
        }
    }

    /// Device-code step 1: asks for a user code. 404 means the endpoint is unavailable.
    pub async fn request_device_user_code(&self) -> Result<DeviceUserCode> {
        let resp = self
            .client
            .post(&self.endpoints.device_user_code_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(serde_json::json!({ "client_id": CLIENT_ID }).to_string())
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "failed to request codex device code: {}",
                    e.without_url()
                ))
            })?;
        let (status, body) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!(
                "failed to read codex device code response: {}",
                e.without_url()
            ))
        })?;
        if !(200..300).contains(&status) {
            if status == 404 {
                return Err(AuthFlowError::other(format!(
                    "codex device endpoint is unavailable (status {status})"
                )));
            }
            let trimmed = body.trim();
            let trimmed = if trimmed.is_empty() {
                "empty response body"
            } else {
                trimmed
            };
            return Err(AuthFlowError::other(format!(
                "codex device code request failed with status {status}: {trimmed}"
            )));
        }
        let parsed: Value = serde_json::from_str(&body).map_err(|e| {
            AuthFlowError::other(format!("failed to decode codex device code response: {e}"))
        })?;
        let s = |k: &str| {
            parsed
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let mut user_code = s("user_code");
        if user_code.is_empty() {
            user_code = s("usercode");
        }
        let device_auth_id = s("device_auth_id");
        if user_code.is_empty() || device_auth_id.is_empty() {
            return Err(AuthFlowError::other(
                "codex device flow did not return required fields",
            ));
        }
        Ok(DeviceUserCode {
            device_auth_id,
            user_code,
            poll_interval: parse_device_poll_interval(parsed.get("interval")),
        })
    }

    /// Device-code step 2: polls until the user approves (403/404 mean pending), 15 min cap.
    pub async fn poll_device_token(&self, code: &DeviceUserCode) -> Result<DeviceTokenResponse> {
        self.poll_device_token_for(code, DEVICE_TIMEOUT).await
    }

    pub(crate) async fn poll_device_token_for(
        &self,
        code: &DeviceUserCode,
        timeout: Duration,
    ) -> Result<DeviceTokenResponse> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(AuthFlowError::other(
                    "codex device authentication timed out after 15 minutes",
                ));
            }
            let resp = self
                .client
                .post(&self.endpoints.device_token_url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(serde_json::json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code }).to_string())
                .send()
                .await
                .map_err(|e| AuthFlowError::Transport(format!("failed to poll codex device token: {}", e.without_url())))?;
            let (status, body) = read_text(resp).await.map_err(|e| {
                AuthFlowError::Transport(format!(
                    "failed to read codex device poll response: {}",
                    e.without_url()
                ))
            })?;
            match status {
                200..=299 => {
                    let parsed: DeviceTokenResponse = serde_json::from_str(&body).map_err(|e| {
                        AuthFlowError::other(format!(
                            "failed to decode codex device token response: {e}"
                        ))
                    })?;
                    return Ok(parsed);
                }
                403 | 404 => tokio::time::sleep(code.poll_interval).await,
                _ => {
                    let trimmed = body.trim();
                    let trimmed = if trimmed.is_empty() {
                        "empty response body"
                    } else {
                        trimmed
                    };
                    return Err(AuthFlowError::other(format!(
                        "codex device token polling failed with status {status}: {trimmed}"
                    )));
                }
            }
        }
    }

    /// Device-code step 3: exchanges the returned authorization code with the device redirect.
    pub async fn exchange_device_code(
        &self,
        token: &DeviceTokenResponse,
    ) -> Result<CodexAuthBundle> {
        let code = token.authorization_code.trim();
        let verifier = token.code_verifier.trim();
        let challenge = token.code_challenge.trim();
        if code.is_empty() || verifier.is_empty() || challenge.is_empty() {
            return Err(AuthFlowError::other(
                "codex device flow token response missing required fields",
            ));
        }
        self.exchange_code_for_tokens_with_redirect(
            code,
            DEVICE_TOKEN_EXCHANGE_REDIRECT_URI,
            &PkceCodes {
                code_verifier: verifier.to_string(),
                code_challenge: challenge.to_string(),
            },
        )
        .await
        .map_err(|e| AuthFlowError::authentication(AuthErrorKind::CodeExchangeFailed, e))
    }
}

/// First device-code response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceUserCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub poll_interval: Duration,
}

#[derive(Clone, Default, Deserialize)]
pub struct DeviceTokenResponse {
    #[serde(default)]
    pub authorization_code: String,
    #[serde(default)]
    pub code_verifier: String,
    #[serde(default)]
    pub code_challenge: String,
}

/// `interval` arrives as a string or an int; default 5 s.
fn parse_device_poll_interval(raw: Option<&Value>) -> Duration {
    let secs = match raw {
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    };
    match secs {
        Some(s) if s > 0 => Duration::from_secs(s as u64),
        _ => Duration::from_secs(DEVICE_DEFAULT_POLL_INTERVAL_SECS),
    }
}

fn is_non_retryable_refresh_err(err: &AuthFlowError) -> bool {
    err.to_string()
        .to_lowercase()
        .contains("refresh_token_reused")
}

/// Builds token data, taking account id / email / plan from the `id_token` (plan defaults to free;
/// an unparseable id_token is only logged).
fn token_data_from(token: &TokenResponse, parse_fail_msg: &str) -> CodexTokenData {
    let (mut account_id, mut email, mut plan_type) =
        (String::new(), String::new(), DEFAULT_PLAN_TYPE.to_string());
    match parse_codex_id_token(&token.id_token) {
        Ok(claims) => {
            account_id = claims.account_id().to_string();
            email = claims.user_email().to_string();
            plan_type = claims.plan_type();
        }
        Err(e) => tracing::warn!("{parse_fail_msg}: {e}"),
    }
    CodexTokenData {
        id_token: token.id_token.clone(),
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        account_id,
        email,
        plan_type,
        expire: expiry_local(token.expires_in),
    }
}

/// `UpdateTokenStorage`.
pub fn update_token_storage(storage: &mut CodexTokenStorage, td: &CodexTokenData) {
    storage.id_token = td.id_token.clone();
    storage.access_token = td.access_token.clone();
    storage.refresh_token = td.refresh_token.clone();
    storage.account_id = td.account_id.clone();
    storage.last_refresh = now_rfc3339_local();
    storage.email = td.email.clone();
    storage.expire = td.expire.clone();
    let plan = td.plan_type.trim();
    storage.plan_type = if plan.is_empty() {
        DEFAULT_PLAN_TYPE.to_string()
    } else {
        plan.to_string()
    };
}

/// `codex-{hash8}-{email}-{plan}.json`; fallbacks drop the hash then the plan. The provider prefix
/// is optional (the login path always includes it).
pub fn credential_file_name(
    email: &str,
    plan_type: &str,
    hash_account_id: &str,
    include_provider_prefix: bool,
) -> String {
    let email = email.trim();
    let plan = normalize_plan_type_for_filename(plan_type);
    let hash = hash_account_id.trim();
    let prefix = if include_provider_prefix { "codex" } else { "" };
    match (hash.is_empty(), plan.is_empty()) {
        (false, true) => format!("{prefix}-{hash}-{email}.json"),
        (false, false) => format!("{prefix}-{hash}-{email}-{plan}.json"),
        (true, true) => format!("{prefix}-{email}.json"),
        (true, false) => format!("{prefix}-{email}-{plan}.json"),
    }
}

/// Splits on non-alphanumerics, lowercases and joins with `-`.
fn normalize_plan_type_for_filename(plan_type: &str) -> String {
    plan_type
        .trim()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
        .map(|p| p.trim().to_lowercase())
        .collect::<Vec<_>>()
        .join("-")
}

/// `buildAuthRecord`: requires an email; plan and account hash come from the id_token claims.
/// The management flow also records `account_id` in the metadata (`include_account_id`).
pub fn build_auth_record(
    auth_svc: &CodexAuth,
    bundle: &CodexAuthBundle,
    include_account_id: bool,
) -> Result<Auth> {
    let mut storage = auth_svc.create_token_storage(bundle);
    if storage.email.is_empty() {
        return Err(AuthFlowError::other(
            "codex token storage missing account information",
        ));
    }
    let mut plan_type = storage.plan_type.clone();
    let mut hash_account_id = String::new();
    if !storage.id_token.is_empty()
        && let Ok(claims) = parse_codex_id_token(&storage.id_token)
    {
        let pt = claims.codex_auth_info.chatgpt_plan_type.trim();
        if !pt.is_empty() {
            plan_type = pt.to_string();
        }
        let account_id = claims.account_id().trim();
        if !account_id.is_empty() {
            hash_account_id = sha256_hex_prefix(account_id, 8);
        }
    }
    if plan_type.is_empty() {
        plan_type = DEFAULT_PLAN_TYPE.to_string();
    }
    storage.plan_type = plan_type.clone();

    let file_name = credential_file_name(&storage.email, &plan_type, &hash_account_id, true);
    let mut metadata = Metadata::new();
    metadata.insert("email".into(), storage.email.clone().into());
    if include_account_id {
        metadata.insert("account_id".into(), storage.account_id.clone().into());
    }
    metadata.insert("plan_type".into(), plan_type.clone().into());
    let mut auth = Auth::new(file_name, "codex");
    auth.storage = Some(TokenStorage::Codex(storage));
    auth.metadata = metadata;
    auth.attributes.insert("plan_type".into(), plan_type);
    Ok(auth)
}

/// Executor `Refresh` write-back: metadata, `plan_type` attribute and token storage.
pub fn apply_refresh_to_auth(auth: &mut Auth, td: &CodexTokenData) {
    let meta = &mut auth.metadata;
    meta.insert("id_token".into(), td.id_token.clone().into());
    meta.insert("access_token".into(), td.access_token.clone().into());
    if !td.refresh_token.is_empty() {
        meta.insert("refresh_token".into(), td.refresh_token.clone().into());
    }
    if !td.account_id.is_empty() {
        meta.insert("account_id".into(), td.account_id.clone().into());
    }
    meta.insert("email".into(), td.email.clone().into());
    meta.insert("expired".into(), td.expire.clone().into());
    meta.insert("type".into(), "codex".into());
    meta.insert("last_refresh".into(), now_rfc3339_local().into());

    let mut plan = td.plan_type.trim().to_string();
    if plan.is_empty()
        && !td.id_token.is_empty()
        && let Ok(claims) = parse_codex_id_token(&td.id_token)
    {
        plan = claims.plan_type();
    }
    if plan.is_empty() {
        plan = DEFAULT_PLAN_TYPE.to_string();
    }
    auth.metadata
        .insert("plan_type".into(), plan.clone().into());
    auth.attributes.insert("plan_type".into(), plan.clone());
    if let Some(TokenStorage::Codex(s)) = auth.storage.as_mut() {
        update_token_storage(s, td);
        s.plan_type = plan;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_jwt;
    use serde_json::json;

    #[test]
    fn auth_url_matches_go_encoding() {
        let auth = CodexAuth::with_client(reqwest::Client::new());
        let url = auth.generate_auth_url(
            "st",
            &PkceCodes {
                code_verifier: "v".into(),
                code_challenge: "ch".into(),
            },
        );
        assert_eq!(
            url,
            "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann&code_challenge=ch&code_challenge_method=S256&codex_cli_simplified_flow=true&id_token_add_organizations=true&prompt=login&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&response_type=code&scope=openid+email+profile+offline_access&state=st"
        );
    }

    #[test]
    fn file_names_follow_fallback_chain() {
        assert_eq!(
            credential_file_name("a@b.c", "Plus", "deadbeef", true),
            "codex-deadbeef-a@b.c-plus.json"
        );
        assert_eq!(
            credential_file_name("a@b.c", "team/pro  plan", "", true),
            "codex-a@b.c-team-pro-plan.json"
        );
        assert_eq!(
            credential_file_name("a@b.c", "", "deadbeef", true),
            "codex-deadbeef-a@b.c.json"
        );
        assert_eq!(
            credential_file_name("a@b.c", "  ", "", true),
            "codex-a@b.c.json"
        );
        assert_eq!(
            credential_file_name("a@b.c", "free", "", false),
            "-a@b.c-free.json"
        );
    }

    #[test]
    fn auth_record_from_id_token_claims() {
        let id_token = make_jwt(&json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-9", "chatgpt_plan_type": "pro"}
        }));
        let token = TokenResponse {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token,
            expires_in: 3600,
        };
        let bundle = CodexAuthBundle {
            token_data: token_data_from(&token, "x"),
            last_refresh: "now".into(),
        };
        assert_eq!(bundle.token_data.account_id, "acct-9");

        let svc = CodexAuth::with_client(reqwest::Client::new());
        let auth = build_auth_record(&svc, &bundle, true).unwrap();
        assert_eq!(auth.metadata["account_id"], "acct-9");
        let hash = sha256_hex_prefix("acct-9", 8);
        assert_eq!(auth.id, format!("codex-{hash}-dev@example.com-pro.json"));
        assert_eq!(auth.attr("plan_type"), "pro");
        assert_eq!(auth.metadata["plan_type"], "pro");
        let Some(TokenStorage::Codex(s)) = &auth.storage else {
            panic!("storage")
        };
        assert_eq!(
            (s.email.as_str(), s.plan_type.as_str()),
            ("dev@example.com", "pro")
        );

        // Missing email is rejected.
        let empty = CodexAuthBundle::default();
        assert!(build_auth_record(&svc, &empty, false).is_err());
    }

    #[test]
    fn device_interval_accepts_string_or_int() {
        assert_eq!(
            parse_device_poll_interval(Some(&json!("7"))),
            Duration::from_secs(7)
        );
        assert_eq!(
            parse_device_poll_interval(Some(&json!(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            parse_device_poll_interval(Some(&json!("0"))),
            Duration::from_secs(5)
        );
        assert_eq!(parse_device_poll_interval(None), Duration::from_secs(5));
    }

    #[test]
    fn reused_refresh_token_is_terminal() {
        let e = AuthFlowError::other(
            "token refresh failed with status 400: {\"error\":\"refresh_token_reused\"}",
        );
        assert!(is_non_retryable_refresh_err(&e));
        assert!(!is_non_retryable_refresh_err(&AuthFlowError::other("boom")));
    }
}
