//! Anthropic / Claude OAuth (internal/auth/claude/*, sdk/auth/claude.go).
//!
//! PKCE authorization-code flow against claude.ai with a local callback on port 54545, JSON token
//! exchange at platform.claude.com, profile/roles companion lookups, token refresh with
//! single-flight, 429 backoff blocking and retry, credential file naming and legacy migration.
//!
//! TLS fingerprint gap: Go uses a uTLS Firefox transport (`utls_transport.go`) for these hosts.
//! This port uses reqwest + rustls, so the ClientHello is not fingerprinted and header order is not
//! pinned; Cloudflare may treat it differently from the Go build.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client_ext, read_text};
use crate::pkce::PkceCodes;
use crate::singleflight::SingleFlight;
use crate::storage::ClaudeTokenStorage;
use crate::store::Store;
use crate::types::Auth;
use crate::util::{encode_query, expiry_local, now_rfc3339_local, random_hex, sha256_hex_prefix};

pub const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
/// Claude Code 2.1.220 posts the code exchange to platform.claude.com, not api.anthropic.com.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REFRESH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const ROLES_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/roles";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
pub const OAUTH_SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const DEFAULT_CALLBACK_PORT: u16 = 54545;
/// `ClaudeAuthenticator.RefreshLead()`: refresh 4 hours before expiry.
pub const REFRESH_LEAD: Duration = Duration::from_secs(4 * 3600);

const REFRESH_MIN_BACKOFF: Duration = Duration::from_secs(5);
const REFRESH_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub const DEVICE_IDS_METADATA_KEY: &str = "claude_device_ids";
const DEVICE_POOL_SIZE: usize = 1;
const DEVICE_ID_BYTES: usize = 32;

/// Per-refresh-token "blocked until" after a 429.
static REFRESH_BLOCK: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static REFRESH_FLIGHT: LazyLock<SingleFlight<ClaudeTokenData>> = LazyLock::new(SingleFlight::default);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeTokenData {
    pub access_token: String,
    pub refresh_token: String,
    pub email: String,
    pub account_uuid: String,
    pub organization_uuid: String,
    pub organization_name: String,
    /// RFC3339 expiry (`expired` key of the credential file).
    pub expire: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeAuthBundle {
    pub token_data: ClaudeTokenData,
    pub device_ids: Vec<String>,
    pub last_refresh: String,
}

#[derive(Debug, Deserialize, Default)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    organization: Org,
    #[serde(default)]
    account: Account,
}

#[derive(Debug, Deserialize, Default)]
struct Org {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize, Default)]
struct Account {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    email_address: String,
}

/// Field order mirrors the key order observed in native Claude Code 2.1.220 traffic.
#[derive(Serialize)]
struct CodeExchangeRequest<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

/// Declared alphabetically: Go marshals this body from a map.
#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
    scope: &'a str,
}

/// Account identity from `/api/oauth/profile`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct OAuthProfile {
    #[serde(default)]
    pub account: ProfileAccount,
    #[serde(default)]
    pub organization: ProfileOrg,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ProfileAccount {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub email: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ProfileOrg {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
}

/// Claude OAuth client. Cheap to construct; holds a reqwest client with the proxy applied.
#[derive(Clone)]
pub struct ClaudeAuth {
    client: reqwest::Client,
    token_url: String,
    refresh_url: String,
    profile_url: String,
    roles_url: String,
}

impl ClaudeAuth {
    /// `proxy_url`: per-auth override, else the global proxy (`""` inherits the environment).
    pub fn new(proxy_url: &str) -> Result<Self> {
        let client = build_client_ext(proxy_url, None, Some(HANDSHAKE_TIMEOUT))?;
        Ok(Self::with_client(client))
    }

    /// Uses a caller-supplied client (tests, shared transports).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url: TOKEN_URL.to_string(),
            refresh_url: REFRESH_TOKEN_URL.to_string(),
            profile_url: PROFILE_URL.to_string(),
            roles_url: ROLES_URL.to_string(),
        }
    }

    /// Redirects every endpoint to a mock server (tests).
    pub fn with_endpoints(mut self, token_url: &str, refresh_url: &str, profile_url: &str, roles_url: &str) -> Self {
        self.token_url = token_url.to_string();
        self.refresh_url = refresh_url.to_string();
        self.profile_url = profile_url.to_string();
        self.roles_url = roles_url.to_string();
        self
    }

    /// Authorization URL; query keys are sorted like Go's `url.Values.Encode`.
    pub fn generate_auth_url(&self, state: &str, pkce: &PkceCodes) -> String {
        let query = encode_query(&[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", OAUTH_SCOPE),
            ("code_challenge", &pkce.code_challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ]);
        format!("{AUTH_URL}?{query}")
    }

    /// `code` may arrive as `<code>#<state>`: split on `#`, the fragment state wins.
    pub fn parse_code_and_state(code: &str) -> (String, String) {
        let mut parts = code.split('#');
        let parsed_code = parts.next().unwrap_or("").to_string();
        let parsed_state = parts.next().unwrap_or("").to_string();
        (parsed_code, parsed_state)
    }

    /// Exchanges the authorization code for tokens, then replays the advisory profile + roles
    /// lookups (failures only logged) and lets the profile identity win.
    pub async fn exchange_code_for_tokens(&self, code: &str, state: &str, pkce: &PkceCodes) -> Result<ClaudeAuthBundle> {
        let (new_code, new_state) = Self::parse_code_and_state(code);
        let state = if new_state.is_empty() { state } else { new_state.as_str() };
        let body = serde_json::to_string(&CodeExchangeRequest {
            grant_type: "authorization_code",
            code: &new_code,
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: &pkce.code_verifier,
            state,
        })
        .map_err(|e| AuthFlowError::other(format!("failed to marshal request body: {e}")))?;

        let resp = axios_headers(self.client.post(&self.token_url)).body(body).send().await.map_err(|e| {
            AuthFlowError::Transport(format!("token exchange request failed: {}", e.without_url()))
        })?;
        let (status, text) = read_text(resp).await.map_err(|e| AuthFlowError::Transport(format!("failed to read token response: {}", e.without_url())))?;
        if status != 200 {
            return Err(AuthFlowError::other(format!("token exchange failed with status {status}: {text}")));
        }
        let token: TokenResponse =
            serde_json::from_str(&text).map_err(|e| AuthFlowError::other(format!("failed to parse token response: {e}")))?;

        let mut data = ClaudeTokenData {
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            email: token.account.email_address.clone(),
            account_uuid: token.account.uuid.clone(),
            organization_uuid: token.organization.uuid.clone(),
            organization_name: token.organization.name.clone(),
            expire: expiry_local(token.expires_in),
        };

        if let Some(profile) = self.inspect_oauth_account(&token.access_token).await {
            let set = |dst: &mut String, v: &str| {
                let v = v.trim();
                if !v.is_empty() {
                    *dst = v.to_string();
                }
            };
            set(&mut data.account_uuid, &profile.account.uuid);
            set(&mut data.email, &profile.account.email);
            set(&mut data.organization_uuid, &profile.organization.uuid);
            set(&mut data.organization_name, &profile.organization.name);
        }

        Ok(ClaudeAuthBundle { token_data: data, device_ids: generate_device_id_pool(), last_refresh: now_rfc3339_local() })
    }

    /// Profile then roles lookup the native client issues right after login. Both advisory.
    async fn inspect_oauth_account(&self, access_token: &str) -> Option<OAuthProfile> {
        let profile = match self.fetch_oauth_profile(access_token).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("fetch Claude OAuth profile after token exchange: {e}");
                None
            }
        };
        if let Err(e) = self.fetch_oauth_roles(access_token).await {
            tracing::warn!("fetch Claude OAuth claude_cli roles after token exchange: {e}");
        }
        profile
    }

    async fn fetch_control_plane(&self, endpoint: &str, access_token: &str, label: &str) -> Result<String> {
        let access_token = access_token.trim();
        if access_token.is_empty() {
            return Err(AuthFlowError::other(format!("fetch Claude OAuth {label}: access token is empty")));
        }
        let req = axios_headers(self.client.get(endpoint))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Cache-Control", "no-cache");
        let resp = req
            .send()
            .await
            .map_err(|e| AuthFlowError::Transport(format!("fetch Claude OAuth {label}: {}", e.without_url())))?;
        let (status, body) = read_text(resp)
            .await
            .map_err(|e| AuthFlowError::Transport(format!("read Claude OAuth {label} response: {}", e.without_url())))?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::Status { status, message: format!("fetch Claude OAuth {label} failed with status {status}") });
        }
        Ok(body)
    }

    /// Account identity of an access token; requires a non-empty account uuid.
    pub async fn fetch_oauth_profile(&self, access_token: &str) -> Result<OAuthProfile> {
        let body = self.fetch_control_plane(&self.profile_url, access_token, "profile").await?;
        let profile: OAuthProfile =
            serde_json::from_str(&body).map_err(|e| AuthFlowError::other(format!("parse Claude OAuth profile response: {e}")))?;
        if profile.account.uuid.trim().is_empty() {
            return Err(AuthFlowError::other("fetch Claude OAuth profile: response account UUID is empty"));
        }
        Ok(profile)
    }

    /// `claude_cli` roles lookup; the payload stays opaque.
    pub async fn fetch_oauth_roles(&self, access_token: &str) -> Result<Value> {
        let body = self.fetch_control_plane(&self.roles_url, access_token, "claude_cli roles").await?;
        serde_json::from_str(&body)
            .map_err(|_| AuthFlowError::other("parse Claude OAuth claude_cli roles response: body is not valid JSON"))
    }

    /// Refreshes tokens. Single-flight per refresh token, detached from caller cancellation, fails
    /// fast while a 429 block is active.
    pub async fn refresh_tokens(&self, refresh_token: &str) -> Result<ClaudeTokenData> {
        if refresh_token.is_empty() {
            return Err(AuthFlowError::other("refresh token is required"));
        }
        if let Some(err) = blocked_error(refresh_token) {
            return Err(err);
        }
        let this = self.clone();
        let rt = refresh_token.to_string();
        REFRESH_FLIGHT
            .run(refresh_token, move || async move {
                match tokio::time::timeout(REFRESH_TIMEOUT, this.refresh_single_flight(&rt)).await {
                    Ok(r) => r,
                    Err(_) => Err(AuthFlowError::Transport("token refresh request failed: timed out".into())),
                }
            })
            .await
    }

    async fn refresh_single_flight(&self, refresh_token: &str) -> Result<ClaudeTokenData> {
        if let Some(err) = blocked_error(refresh_token) {
            return Err(err);
        }
        let body = serde_json::to_string(&RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
            scope: OAUTH_SCOPE,
        })
        .map_err(|e| AuthFlowError::other(format!("failed to marshal request body: {e}")))?;

        let resp = axios_headers(self.client.post(&self.refresh_url))
            .body(body)
            .send()
            .await
            .map_err(|e| AuthFlowError::Transport(format!("token refresh request failed: {}", e.without_url())))?;
        let retry_after = parse_retry_after(resp.headers());
        let (status, text) = read_text(resp)
            .await
            .map_err(|e| AuthFlowError::Transport(format!("failed to read refresh response: {}", e.without_url())))?;

        if status != 200 {
            if status == 429 {
                REFRESH_BLOCK.lock().insert(refresh_token.to_string(), Instant::now() + retry_after);
                return Err(AuthFlowError::Refresh { status, message: text, retryable: false });
            }
            return Err(AuthFlowError::Refresh { status, message: text, retryable: status >= 500 });
        }

        let token: TokenResponse =
            serde_json::from_str(&text).map_err(|e| AuthFlowError::other(format!("failed to parse token response: {e}")))?;
        REFRESH_BLOCK.lock().remove(refresh_token);
        let new_refresh = if token.refresh_token.trim().is_empty() { refresh_token.to_string() } else { token.refresh_token.clone() };
        let mut data = ClaudeTokenData {
            access_token: token.access_token.clone(),
            refresh_token: new_refresh,
            expire: expiry_local(token.expires_in),
            ..Default::default()
        };
        match self.fetch_oauth_profile(&token.access_token).await {
            Ok(profile) => {
                data.email = profile.account.email;
                data.account_uuid = profile.account.uuid;
                data.organization_uuid = profile.organization.uuid;
                data.organization_name = profile.organization.name;
            }
            Err(e) => tracing::warn!("fetch Claude OAuth profile after refresh: {e}"),
        }
        Ok(data)
    }

    /// `RefreshTokensWithRetry`: attempt n waits n seconds; stops on a non-retryable error.
    pub async fn refresh_tokens_with_retry(&self, refresh_token: &str, max_retries: u32) -> Result<ClaudeTokenData> {
        let mut last_err = None;
        for attempt in 0..max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
            }
            match self.refresh_tokens(refresh_token).await {
                Ok(d) => return Ok(d),
                Err(e) => {
                    tracing::warn!("Token refresh attempt {} failed: {e}", attempt + 1);
                    let retry = e.is_retryable_refresh();
                    last_err = Some(e);
                    if !retry {
                        break;
                    }
                }
            }
        }
        let cause = last_err.map(|e| e.to_string()).unwrap_or_default();
        Err(AuthFlowError::Other(format!("token refresh failed after {max_retries} attempts: {cause}")))
    }

    /// Builds the credential file struct from a login bundle.
    pub fn create_token_storage(&self, bundle: &ClaudeAuthBundle) -> ClaudeTokenStorage {
        ClaudeTokenStorage {
            access_token: bundle.token_data.access_token.clone(),
            refresh_token: bundle.token_data.refresh_token.clone(),
            last_refresh: bundle.last_refresh.clone(),
            email: bundle.token_data.email.clone(),
            account_uuid: bundle.token_data.account_uuid.clone(),
            organization_uuid: bundle.token_data.organization_uuid.clone(),
            organization_name: bundle.token_data.organization_name.clone(),
            device_ids: bundle.device_ids.clone(),
            expire: bundle.token_data.expire.clone(),
            ..Default::default()
        }
    }
}

/// `UpdateTokenStorage`: applies refreshed tokens, never erasing identity fields.
pub fn update_token_storage(storage: &mut ClaudeTokenStorage, data: &ClaudeTokenData) {
    storage.access_token = data.access_token.clone();
    storage.refresh_token = data.refresh_token.clone();
    storage.last_refresh = now_rfc3339_local();
    if !data.email.is_empty() {
        storage.email = data.email.clone();
    }
    if !data.account_uuid.is_empty() {
        storage.account_uuid = data.account_uuid.clone();
    }
    if !data.organization_uuid.is_empty() {
        storage.organization_uuid = data.organization_uuid.clone();
    }
    if !data.organization_name.is_empty() {
        storage.organization_name = data.organization_name.clone();
    }
    storage.expire = data.expire.clone();
}

/// Executor `Refresh` metadata write-back: tokens always, identity only when non-empty.
pub fn apply_refresh_to_auth(auth: &mut Auth, td: &ClaudeTokenData) {
    let store_str = |meta: &mut Metadata, key: &str, value: &str| {
        if !value.trim().is_empty() {
            meta.insert(key.to_string(), Value::String(value.to_string()));
        }
    };
    let meta = &mut auth.metadata;
    meta.insert("access_token".into(), Value::String(td.access_token.clone()));
    store_str(meta, "refresh_token", &td.refresh_token);
    store_str(meta, "email", &td.email);
    store_str(meta, "account_uuid", &td.account_uuid);
    store_str(meta, "organization_uuid", &td.organization_uuid);
    store_str(meta, "organization_name", &td.organization_name);
    meta.insert("expired".into(), Value::String(td.expire.clone()));
    meta.insert("type".into(), Value::String("claude".into()));
    meta.insert("last_refresh".into(), Value::String(now_rfc3339_local()));
}

fn blocked_error(refresh_token: &str) -> Option<AuthFlowError> {
    let until = REFRESH_BLOCK.lock().get(refresh_token).copied()?;
    if until <= Instant::now() {
        return None;
    }
    Some(AuthFlowError::Refresh {
        status: 429,
        message: "refresh temporarily blocked".to_string(),
        retryable: false,
    })
}

fn clamp_backoff(d: Duration) -> Duration {
    d.clamp(REFRESH_MIN_BACKOFF, REFRESH_MAX_BACKOFF)
}

/// `Retry-After` (seconds or HTTP date), then `Retry-After-Ms`, clamped to [5 s, 5 min].
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(raw) = get("retry-after") {
        if let Ok(secs) = raw.parse::<f64>() {
            return clamp_backoff(Duration::from_secs_f64(secs.max(0.0)));
        }
        if let Ok(when) = chrono::DateTime::parse_from_rfc2822(&raw) {
            let delta = (when.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or_default();
            return clamp_backoff(delta);
        }
    }
    if let Some(raw) = get("retry-after-ms") {
        if let Ok(ms) = raw.parse::<f64>() {
            return clamp_backoff(Duration::from_secs_f64((ms / 1000.0).max(0.0)));
        }
    }
    REFRESH_MIN_BACKOFF
}

/// Axios-shaped OAuth control plane headers (values only; no wire-order control in reqwest).
fn axios_headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", "axios/1.15.2")
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("Connection", "close")
}

// ---- Credential file naming and legacy migration ----

/// `claude-<email>.json` without identity (legacy), else `claude-<hash8>-<email>.json` where hash8
/// is the first 8 hex of sha256(org uuid, falling back to account uuid).
pub fn credential_file_name(email: &str, organization_uuid: &str, account_uuid: &str) -> String {
    let email = email.trim();
    let mut identity = organization_uuid.trim();
    if identity.is_empty() {
        identity = account_uuid.trim();
    }
    if identity.is_empty() {
        return format!("claude-{email}.json");
    }
    format!("claude-{}-{email}.json", sha256_hex_prefix(identity, 8))
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    crate::util::trimmed_str(metadata.get(key))
}

fn is_hashed_credential_target(target: &Auth) -> bool {
    if !target.provider.trim().eq_ignore_ascii_case("claude") {
        return false;
    }
    let email = metadata_string(&target.metadata, "email");
    let org = metadata_string(&target.metadata, "organization_uuid");
    let account = metadata_string(&target.metadata, "account_uuid");
    if email.is_empty() || (org.is_empty() && account.is_empty()) {
        return false;
    }
    let mut name = target.file_name.trim();
    if name.is_empty() {
        name = target.id.trim();
    }
    base_name(name).eq_ignore_ascii_case(&credential_file_name(&email, &org, &account))
}

fn base_name(p: &str) -> &str {
    std::path::Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p)
}

/// Finds the older email-only or account-hashed credential file for the same identity, so a new
/// org-hashed login can absorb its operator fields and replace it.
pub fn find_matching_legacy_credential(store: &dyn Store, target: &Auth) -> Result<Option<Auth>> {
    if !is_hashed_credential_target(target) {
        return Ok(None);
    }
    let records = match store.list() {
        Ok(r) => r,
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(AuthFlowError::other(format!("list Claude credentials for legacy migration: {e}"))),
    };

    let target_email = metadata_string(&target.metadata, "email");
    let target_org = metadata_string(&target.metadata, "organization_uuid");
    let target_account = metadata_string(&target.metadata, "account_uuid");
    let legacy_name = credential_file_name(&target_email, "", "");
    let account_name = if !target_org.is_empty() && !target_account.is_empty() {
        credential_file_name(&target_email, "", &target_account)
    } else {
        String::new()
    };

    for candidate in records {
        if !candidate.provider.trim().eq_ignore_ascii_case("claude") {
            continue;
        }
        let mut name = candidate.file_name.trim().to_string();
        if name.is_empty() {
            name = candidate.id.trim().to_string();
        }
        let base = base_name(&name);
        let is_email_legacy = base.eq_ignore_ascii_case(&legacy_name);
        let is_account_predecessor = !account_name.is_empty() && base.eq_ignore_ascii_case(&account_name);
        if !is_email_legacy && !is_account_predecessor {
            continue;
        }
        let cand_org = metadata_string(&candidate.metadata, "organization_uuid");
        let cand_account = metadata_string(&candidate.metadata, "account_uuid");
        if !target_org.is_empty() {
            if !cand_org.is_empty() && cand_org.eq_ignore_ascii_case(&target_org) {
                return Ok(Some(candidate));
            }
            if cand_org.is_empty()
                && is_account_predecessor
                && !cand_account.is_empty()
                && cand_account.eq_ignore_ascii_case(&target_account)
            {
                return Ok(Some(candidate));
            }
        } else if is_email_legacy
            && cand_org.is_empty()
            && !target_account.is_empty()
            && !cand_account.is_empty()
            && cand_account.eq_ignore_ascii_case(&target_account)
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

// ---- Device id pool ----

fn generate_device_id() -> String {
    random_hex(DEVICE_ID_BYTES)
}

/// A fresh pool (one 64-hex-char id).
pub fn generate_device_id_pool() -> Vec<String> {
    (0..DEVICE_POOL_SIZE).map(|_| generate_device_id()).collect()
}

/// 64 lowercase hex chars.
pub fn valid_device_id(value: &str) -> bool {
    value.len() == DEVICE_ID_BYTES * 2 && value == value.to_lowercase() && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Trims, lowercases, validates and dedups ids; non-array input yields an empty pool.
pub fn normalize_device_id_pool(raw: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = raw else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for v in items {
        let Some(s) = v.as_str() else { continue };
        let id = s.trim().to_lowercase();
        if valid_device_id(&id) && !out.contains(&id) {
            out.push(id);
            if out.len() == DEVICE_POOL_SIZE {
                break;
            }
        }
    }
    out
}

fn has_canonical_device_id_pool(raw: Option<&Value>) -> bool {
    let Some(Value::Array(items)) = raw else { return false };
    let Some(strs) = items.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else { return false };
    let normalized = normalize_device_id_pool(raw);
    strs.len() == DEVICE_POOL_SIZE && normalized.len() == DEVICE_POOL_SIZE && strs[0] == normalized[0]
}

/// `EnsureDeviceIDPool`: returns the pool, topping it up and rewriting the metadata key when it was
/// missing or not canonical. The bool is true when metadata changed.
pub fn ensure_device_id_pool(metadata: &mut Metadata) -> (Vec<String>, bool) {
    let raw = metadata.get(DEVICE_IDS_METADATA_KEY);
    let mut ids = normalize_device_id_pool(raw);
    let changed = !has_canonical_device_id_pool(raw);
    while ids.len() < DEVICE_POOL_SIZE {
        let id = generate_device_id();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if changed {
        metadata.insert(DEVICE_IDS_METADATA_KEY.into(), Value::Array(ids.iter().cloned().map(Value::String).collect()));
    }
    (ids, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkce::PkceCodes;
    use serde_json::json;

    #[test]
    fn auth_url_has_sorted_keys_and_encoded_scope() {
        let auth = ClaudeAuth::with_client(reqwest::Client::new());
        let url = auth.generate_auth_url("st", &PkceCodes { code_verifier: "v".into(), code_challenge: "chal".into() });
        assert_eq!(
            url,
            "https://claude.ai/oauth/authorize?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&code=true&code_challenge=chal&code_challenge_method=S256&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&response_type=code&scope=user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload&state=st"
        );
    }

    #[test]
    fn code_hash_state_split() {
        assert_eq!(ClaudeAuth::parse_code_and_state("abc#def"), ("abc".into(), "def".into()));
        assert_eq!(ClaudeAuth::parse_code_and_state("abc"), ("abc".into(), "".into()));
    }

    #[test]
    fn credential_file_names() {
        assert_eq!(credential_file_name(" a@b.c ", "", ""), "claude-a@b.c.json");
        let hash = sha256_hex_prefix("org-1", 8);
        assert_eq!(credential_file_name("a@b.c", "org-1", "acc"), format!("claude-{hash}-a@b.c.json"));
        // Falls back to the account uuid when the org is empty.
        let hash = sha256_hex_prefix("acc", 8);
        assert_eq!(credential_file_name("a@b.c", " ", "acc"), format!("claude-{hash}-a@b.c.json"));
    }

    #[test]
    fn device_pool_normalization() {
        let good = "ab".repeat(32);
        let raw = json!([format!("  {}  ", good.to_uppercase()), "short", good]);
        assert_eq!(normalize_device_id_pool(Some(&raw)), vec![good.clone()]);

        let mut meta = Metadata::new();
        let (ids, changed) = ensure_device_id_pool(&mut meta);
        assert!(changed && ids.len() == 1 && valid_device_id(&ids[0]));
        let (ids2, changed2) = ensure_device_id_pool(&mut meta);
        assert!(!changed2);
        assert_eq!(ids, ids2);
    }

    #[test]
    fn retry_after_parsing_is_clamped() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), Duration::from_secs(5));
        h.insert("retry-after", "1".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Duration::from_secs(5));
        h.insert("retry-after", "120".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Duration::from_secs(120));
        h.insert("retry-after", "99999".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Duration::from_secs(300));
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after-ms", "30000".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Duration::from_secs(30));
    }

    #[test]
    fn refresh_write_back_never_erases_identity() {
        let mut auth = Auth::default();
        auth.metadata.insert("email".into(), json!("keep@x"));
        auth.metadata.insert("account_uuid".into(), json!("acc"));
        let td = ClaudeTokenData {
            access_token: "new-at".into(),
            refresh_token: "new-rt".into(),
            expire: "2030-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        apply_refresh_to_auth(&mut auth, &td);
        assert_eq!(auth.metadata["email"], "keep@x");
        assert_eq!(auth.metadata["account_uuid"], "acc");
        assert_eq!(auth.metadata["access_token"], "new-at");
        assert_eq!(auth.metadata["type"], "claude");
        assert_eq!(auth.metadata["expired"], "2030-01-01T00:00:00Z");
    }
}
