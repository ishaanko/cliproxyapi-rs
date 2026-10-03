//! The `Auth` credential record (sdk/cliproxy/auth/types.go, classification.go, status.go):
//! struct fields, status, attributes/metadata conventions, stable index and expiry derivation.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credmeta::{Metadata, parse_bool_any, parse_int_any};
use crate::jwt::{normalise_unix, parse_jwt_exp};
use crate::storage::TokenStorage;
use crate::util::{abs_clean, zero_time};

// ---- Well-known attribute names and kinds ----

pub const AUTH_KIND_API_KEY: &str = "apikey";
pub const AUTH_KIND_OAUTH: &str = "oauth";

pub const AUTH_SOURCE_CONFIG: &str = "config";
pub const AUTH_SOURCE_FILE: &str = "file";
pub const AUTH_SOURCE_GIT: &str = "git";
pub const AUTH_SOURCE_MEMORY: &str = "memory";
pub const AUTH_SOURCE_OBJECT_STORE: &str = "objectstore";
pub const AUTH_SOURCE_POSTGRES: &str = "postgres";

pub const ATTRIBUTE_API_KEY: &str = "api_key";
pub const ATTRIBUTE_AUTH_KIND: &str = "auth_kind";
pub const ATTRIBUTE_PATH: &str = "path";
pub const ATTRIBUTE_RUNTIME_ONLY: &str = "runtime_only";
pub const ATTRIBUTE_SOURCE: &str = "source";
pub const ATTRIBUTE_SOURCE_BACKEND: &str = "source_backend";
pub const ATTRIBUTE_AUTH_INDEX_SEED: &str = "auth_index_seed";
pub const ATTRIBUTE_PLUGIN_VIRTUAL: &str = "plugin_virtual";
pub const ATTRIBUTE_VIRTUAL_SOURCE: &str = "virtual_source";

/// Lifecycle status managed by the conductor. Serialized as the lowercase Go string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Unknown,
    Active,
    Pending,
    Refreshing,
    Error,
    Disabled,
}

/// `auth.Error`: provider-agnostic failure description.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthError {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub http_status: i32,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

/// serde adapter for Go `time.Time`: unset is `0001-01-01T00:00:00Z` on the wire.
pub mod go_time {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        let t = t.unwrap_or_else(zero_time);
        s.serialize_str(&t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        let Some(raw) = raw else { return Ok(None) };
        let parsed = DateTime::parse_from_rfc3339(raw.trim())
            .map(|t| t.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)?;
        Ok(if parsed == zero_time() {
            None
        } else {
            Some(parsed)
        })
    }
}

/// Credential-wide (or per-model) quota state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaState {
    #[serde(default)]
    pub exceeded: bool,
    /// `""`, `quota`, `credential_quota`, `cloudflare challenge`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, with = "go_time")]
    pub next_recover_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub backoff_level: i32,
    #[serde(default, with = "go_time")]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub signals: BTreeMap<String, String>,
}

/// Per-model runtime availability state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelState {
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_message: String,
    #[serde(default)]
    pub unavailable: bool,
    #[serde(default, with = "go_time")]
    pub next_retry_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<AuthError>,
    #[serde(default)]
    pub quota: QuotaState,
    #[serde(default, with = "go_time")]
    pub updated_at: Option<DateTime<Utc>>,
}

const RECENT_BUCKET_SECONDS: i64 = 10 * 60;
const RECENT_BUCKET_COUNT: usize = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecentBucket {
    bucket_id: i64,
    success: i64,
    failed: i64,
}

/// One entry of the 20 x 10 minute request ring shown in the management UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentRequestBucket {
    pub time: String,
    pub success: i64,
    pub failed: i64,
}

/// A single credential with its runtime state. Login flows produce one, the file store round-trips
/// it, the conductor mutates clones of it.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Auth {
    /// Stable id; file auths use the path relative to auth-dir.
    pub id: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub registration_epoch: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub generation: u64,
    /// Runtime-only stable index (see [`Auth::ensure_index`]).
    #[serde(skip)]
    pub index: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// Runtime-only: basename or id of the backing file.
    #[serde(skip)]
    pub file_name: String,
    /// Provider token struct used by login flows to write the credential file.
    #[serde(skip)]
    pub storage: Option<TokenStorage>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_message: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub unavailable: bool,
    /// `""` inherit, `direct`/`none` bypass, else a proxy URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proxy_url: String,
    /// Immutable config-ish, string-typed values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
    /// Mutable provider state; for file auths this is the JSON file body.
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
    #[serde(default)]
    pub quota: QuotaState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<AuthError>,
    #[serde(default, with = "go_time")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, with = "go_time")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, with = "go_time")]
    pub last_refreshed_at: Option<DateTime<Utc>>,
    #[serde(default, with = "go_time")]
    pub next_refresh_after: Option<DateTime<Utc>>,
    /// Consecutive refresh failures (in-memory only).
    #[serde(skip)]
    pub refresh_failures: i32,
    #[serde(default, with = "go_time")]
    pub next_retry_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_states: BTreeMap<String, ModelState>,
    /// Executor-owned opaque data; never serialized.
    #[serde(skip)]
    pub runtime: Option<Arc<dyn Any + Send + Sync>>,
    #[serde(skip)]
    pub success: i64,
    #[serde(skip)]
    pub failed: i64,
    #[serde(skip)]
    pub(crate) recent_requests: [RecentBucket; RECENT_BUCKET_COUNT],
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl std::fmt::Debug for Auth {
    // Never prints metadata: it holds tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("status", &self.status)
            .field("disabled", &self.disabled)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// A blank auth with `id` (also used as the file name) and `provider` set.
    pub fn new(id: impl Into<String>, provider: impl Into<String>) -> Self {
        let id = id.into();
        Auth {
            file_name: id.clone(),
            id,
            provider: provider.into(),
            ..Default::default()
        }
    }

    pub fn attr(&self, key: &str) -> String {
        self.attr_ref(key).to_string()
    }

    /// [`Auth::attr`] without allocating (selection scans call this per credential per request).
    pub fn attr_ref(&self, key: &str) -> &str {
        self.attributes.get(key).map_or("", |v| v.trim())
    }

    /// Trimmed string metadata value, `""` for missing or non-string.
    pub fn meta_str(&self, key: &str) -> String {
        self.meta_ref(key).to_string()
    }

    /// [`Auth::meta_str`] without allocating.
    pub fn meta_ref(&self, key: &str) -> &str {
        self.metadata.get(key).and_then(Value::as_str).map_or("", str::trim)
    }

    /// `access_token`, falling back to the legacy `accessToken` spelling.
    pub fn access_token(&self) -> String {
        let t = self.meta_str("access_token");
        if !t.is_empty() {
            t
        } else {
            self.meta_str("accessToken")
        }
    }

    /// `refresh_token`, falling back to `refreshToken`.
    pub fn refresh_token(&self) -> String {
        let t = self.meta_str("refresh_token");
        if !t.is_empty() {
            t
        } else {
            self.meta_str("refreshToken")
        }
    }

    // ---- Classification ----

    /// `AuthKind()`: `apikey`, `oauth` or `""`.
    pub fn auth_kind(&self) -> &'static str {
        if let Some(k) = normalize_auth_kind(self.attr_ref(ATTRIBUTE_AUTH_KIND)) {
            return k;
        }
        if let Some(k) = normalize_auth_kind(self.meta_ref(ATTRIBUTE_AUTH_KIND)) {
            return k;
        }
        if !self.attr_ref(ATTRIBUTE_API_KEY).is_empty() {
            return AUTH_KIND_API_KEY;
        }
        if self.has_oauth_metadata() {
            return AUTH_KIND_OAUTH;
        }
        ""
    }

    fn has_oauth_metadata(&self) -> bool {
        if self.metadata.is_empty() {
            return false;
        }
        const KEYS: [&str; 7] = [
            "access_token",
            "refresh_token",
            "id_token",
            "email",
            "token_type",
            "expires_at",
            "expired",
        ];
        if KEYS.iter().any(|k| !self.meta_ref(k).is_empty()) {
            return true;
        }
        matches!(self.metadata.get("token"), Some(Value::Object(m)) if !m.is_empty())
    }

    /// `AuthSourceKind()`: where the credential came from.
    pub fn auth_source_kind(&self) -> &'static str {
        if self
            .attr_ref(ATTRIBUTE_RUNTIME_ONLY)
            .eq_ignore_ascii_case("true")
        {
            return AUTH_SOURCE_MEMORY;
        }
        if let Some(s) = normalize_auth_source_kind(self.attr_ref(ATTRIBUTE_SOURCE_BACKEND)) {
            return s;
        }
        let source = self.attr_ref(ATTRIBUTE_SOURCE);
        if !source.is_empty() {
            if source.to_lowercase().starts_with("config:") {
                return AUTH_SOURCE_CONFIG;
            }
            return normalize_auth_source_kind(source).unwrap_or(AUTH_SOURCE_FILE);
        }
        if !self.attr_ref(ATTRIBUTE_PATH).is_empty() || !self.file_name.trim().is_empty() {
            return AUTH_SOURCE_FILE;
        }
        ""
    }

    /// `IsConfigAPIKeyAuth`: an API-key auth synthesized from config (never persisted).
    pub fn is_config_api_key(&self) -> bool {
        self.auth_kind() == AUTH_KIND_API_KEY && self.auth_source_kind() == AUTH_SOURCE_CONFIG
    }

    /// `(kind, value)` shown by the management UI: oauth email or api key.
    pub fn account_info(&self) -> (&'static str, String) {
        match self.auth_kind() {
            AUTH_KIND_OAUTH => ("oauth", self.meta_str("email")),
            AUTH_KIND_API_KEY => ("api_key", self.attr(ATTRIBUTE_API_KEY)),
            _ => ("", String::new()),
        }
    }

    // ---- Stable index ----

    fn index_seed(&self) -> String {
        let seed = self.attr(ATTRIBUTE_AUTH_INDEX_SEED);
        if !seed.is_empty() {
            return format!("{ATTRIBUTE_AUTH_INDEX_SEED}:{seed}");
        }
        let provider = self.provider.trim().to_lowercase();
        let compat_name = self.attr("compat_name");
        let base_url = self.attr("base_url");
        let api_key = self.attr(ATTRIBUTE_API_KEY);
        let mut file_path = self.attr(ATTRIBUTE_PATH);
        if file_path.is_empty() {
            file_path = self.attr(ATTRIBUTE_SOURCE);
        }
        if file_path.is_empty() {
            file_path = self.file_name.trim().to_string();
        }
        if file_path.is_empty() {
            file_path = self.id.trim().to_string();
        }

        if !file_path.is_empty() && file_path.to_lowercase().ends_with(".json") {
            let file_path = abs_clean(&file_path);
            let mut auth_type = self.meta_str("type");
            if auth_type.is_empty() {
                auth_type = provider.clone();
            }
            let auth_type = auth_type.trim().to_lowercase();
            if !auth_type.is_empty() {
                return format!("{auth_type}:{file_path}");
            }
        }

        if !api_key.is_empty() {
            let api_prefix = if !compat_name.is_empty() || provider == "openai-compatibility" {
                "openai-compatibility"
            } else {
                match provider.as_str() {
                    "gemini" => "gemini-api-key",
                    "gemini-interactions" => "interactions-api-key",
                    "codex" => "codex-api-key",
                    "xai" => "xai-api-key",
                    "claude" => "claude-api-key",
                    "meta" => "meta-api-key",
                    _ => "",
                }
            };
            if !api_prefix.is_empty() {
                return format!("{api_prefix}:{base_url}+{api_key}");
            }
        }

        let id = self.id.trim();
        if !id.is_empty() {
            return format!("id:{id}");
        }
        String::new()
    }

    /// Stable 16-hex-char index (first 8 bytes of sha256 of the seed), cached in `self.index`.
    /// Must stay identical to the Go value: the management API and usage stats key on it.
    pub fn ensure_index(&mut self) -> String {
        let existing = self.index.trim().to_string();
        if !existing.is_empty() {
            self.index = existing.clone();
            return existing;
        }
        let seed = self.index_seed();
        let seed = seed.trim();
        if seed.is_empty() {
            return String::new();
        }
        let idx = crate::util::sha256_hex_prefix(seed, 16);
        self.index = idx.clone();
        idx
    }

    // ---- Expiry ----

    /// `ExpirationTime()`: JWT `exp` of the access token first, then the legacy expiry keys,
    /// `expires_in` + `timestamp`, then nested `token` objects.
    pub fn expiration_time(&self) -> Option<DateTime<Utc>> {
        let token = self.access_token();
        if !token.is_empty()
            && let Some(exp) = parse_jwt_exp(&token)
        {
            return Some(exp);
        }
        expiration_from_map(&self.metadata)
    }

    /// Expiry of the access token itself; a JWT `exp` strictly wins.
    pub fn access_token_expiration_time(&self) -> Option<DateTime<Utc>> {
        let token = self.access_token();
        if token.is_empty() {
            return None;
        }
        if let Some(exp) = parse_jwt_exp(&token) {
            return Some(exp);
        }
        self.expiration_time()
    }

    /// Non-empty access token that is unexpired at `now` (or has no known expiry).
    pub fn has_valid_access_token(&self, now: DateTime<Utc>) -> bool {
        if self.access_token().is_empty() {
            return false;
        }
        match self.access_token_expiration_time() {
            Some(exp) => exp > now,
            None => true,
        }
    }

    // ---- Per-auth overrides read from metadata ----

    /// `disable_cooling` override (also the legacy hyphenated key). `None` when absent.
    pub fn disable_cooling_override(&self) -> Option<bool> {
        for key in ["disable_cooling", "disable-cooling"] {
            if let Some(b) = self.metadata.get(key).and_then(parse_bool_any) {
                return Some(b);
            }
        }
        None
    }

    pub fn tool_prefix_disabled(&self) -> bool {
        for key in ["tool_prefix_disabled", "tool-prefix-disabled"] {
            if let Some(b) = self.metadata.get(key).and_then(parse_bool_any) {
                return b;
            }
        }
        false
    }

    /// `request_retry` override; negative counts as unset.
    pub fn request_retry_override(&self) -> Option<i64> {
        for key in ["request_retry", "request-retry"] {
            if let Some(n) = self.metadata.get(key).and_then(parse_int_any) {
                return if n < 0 { None } else { Some(n) };
            }
        }
        None
    }

    /// `ProxyInfo()`: short description used in logs.
    pub fn proxy_info(&self) -> String {
        let p = self.proxy_url.trim();
        if p.is_empty() {
            return String::new();
        }
        match p.find("://") {
            Some(idx) if idx > 0 => format!("via {} proxy", &p[..idx]),
            _ => "via proxy".to_string(),
        }
    }

    // ---- Plugin virtual auths ----

    pub fn is_plugin_virtual(&self) -> bool {
        self.attr(ATTRIBUTE_PLUGIN_VIRTUAL)
            .eq_ignore_ascii_case("true")
    }

    /// Marks an auth expanded from a plugin-owned file and derives its index seed.
    pub fn mark_plugin_virtual(&mut self, source_path: &str, ordinal: usize) {
        self.attributes
            .insert(ATTRIBUTE_PLUGIN_VIRTUAL.into(), "true".into());
        let source_path = source_path.trim();
        if !source_path.is_empty() {
            self.attributes
                .insert(ATTRIBUTE_VIRTUAL_SOURCE.into(), source_path.into());
        }
        let mut seed_id = self.id.trim().to_string();
        if seed_id.is_empty() {
            seed_id = self.file_name.trim().to_string();
        }
        if seed_id.is_empty() {
            seed_id = ordinal.to_string();
        }
        let seed = [
            self.provider.trim().to_lowercase(),
            source_path.to_string(),
            seed_id,
            ordinal.to_string(),
        ]
        .join("|");
        self.attributes
            .insert(ATTRIBUTE_AUTH_INDEX_SEED.into(), seed);
    }

    // ---- Recent request ring ----

    /// Records one request outcome into the 20 x 10 minute ring.
    pub fn record_recent_request(&mut self, now: DateTime<Utc>, success: bool) {
        let id = bucket_id(now);
        let b = &mut self.recent_requests[bucket_index(id)];
        if b.bucket_id != id {
            *b = RecentBucket {
                bucket_id: id,
                success: 0,
                failed: 0,
            };
        }
        if success {
            b.success += 1
        } else {
            b.failed += 1
        }
    }

    /// Oldest-to-newest snapshot with local `HH:MM-HH:MM` labels.
    pub fn recent_requests_snapshot(&self, now: DateTime<Utc>) -> Vec<RecentRequestBucket> {
        let current = bucket_id(now);
        (0..RECENT_BUCKET_COUNT as i64)
            .rev()
            .map(|i| {
                let id = current - i;
                let b = self.recent_requests[bucket_index(id)];
                let (success, failed) = if b.bucket_id == id {
                    (b.success, b.failed)
                } else {
                    (0, 0)
                };
                RecentRequestBucket {
                    time: bucket_label(id),
                    success,
                    failed,
                }
            })
            .collect()
    }
}

fn bucket_id(now: DateTime<Utc>) -> i64 {
    if now == zero_time() {
        0
    } else {
        now.timestamp().div_euclid(RECENT_BUCKET_SECONDS)
    }
}

fn bucket_index(id: i64) -> usize {
    id.rem_euclid(RECENT_BUCKET_COUNT as i64) as usize
}

fn bucket_label(id: i64) -> String {
    let start = DateTime::from_timestamp(id * RECENT_BUCKET_SECONDS, 0)
        .unwrap_or_else(zero_time)
        .with_timezone(&Local);
    let end = start + chrono::Duration::seconds(RECENT_BUCKET_SECONDS);
    format!("{}-{}", start.format("%H:%M"), end.format("%H:%M"))
}

/// `eq_ignore_ascii_case` against any of `names`; non-ASCII input folds like `to_lowercase`.
fn eq_any_folded(s: &str, names: &[&str]) -> bool {
    let s = s.trim();
    if s.is_ascii() {
        names.iter().any(|n| s.eq_ignore_ascii_case(n))
    } else {
        let lower = s.to_lowercase();
        names.iter().any(|n| lower == *n)
    }
}

fn normalize_auth_kind(kind: &str) -> Option<&'static str> {
    if eq_any_folded(kind, &["apikey", "api_key", "api-key"]) {
        Some(AUTH_KIND_API_KEY)
    } else if eq_any_folded(kind, &["oauth", "oauth2"]) {
        Some(AUTH_KIND_OAUTH)
    } else {
        None
    }
}

fn normalize_auth_source_kind(source: &str) -> Option<&'static str> {
    const KINDS: [(&[&str], &str); 6] = [
        (&["config"], AUTH_SOURCE_CONFIG),
        (&["file", "filesystem"], AUTH_SOURCE_FILE),
        (&["git"], AUTH_SOURCE_GIT),
        (&["memory", "runtime", "runtime_only"], AUTH_SOURCE_MEMORY),
        (&["objectstore", "object-store"], AUTH_SOURCE_OBJECT_STORE),
        (&["postgres", "postgresql", "database", "db"], AUTH_SOURCE_POSTGRES),
    ];
    KINDS.iter().find(|(names, _)| eq_any_folded(source, names)).map(|(_, k)| *k)
}

// ---- Expiry parsing ----

const EXPIRE_KEYS: [&str; 6] = [
    "expired",
    "expire",
    "expires_at",
    "expiresAt",
    "expiry",
    "expires",
];

fn expiration_from_map(meta: &Metadata) -> Option<DateTime<Utc>> {
    for key in EXPIRE_KEYS {
        if let Some(v) = meta.get(key)
            && let Some(ts) = parse_time_value(v)
        {
            return Some(ts);
        }
    }
    if let Some(expires_in) = relative_expiry_seconds(meta)
        && let Some(ts) = relative_expiry_timestamp(meta)
    {
        return Some(crate::util::add_secs(ts, expires_in));
    }
    for nested in ["token", "Token"] {
        if let Some(Value::Object(m)) = meta.get(nested)
            && let Some(ts) = expiration_from_map(m)
        {
            return Some(ts);
        }
    }
    None
}

fn relative_expiry_seconds(meta: &Metadata) -> Option<i64> {
    ["expires_in", "expiresIn"]
        .iter()
        .filter_map(|k| meta.get(*k).and_then(parse_int_any))
        .find(|s| *s > 0)
}

fn relative_expiry_timestamp(meta: &Metadata) -> Option<DateTime<Utc>> {
    ["timestamp", "issued_at", "issuedAt"]
        .iter()
        .filter_map(|k| meta.get(*k).and_then(parse_time_value))
        .find(|t| *t != zero_time())
}

/// `parseTimeValue`: RFC3339 (with or without fraction), `YYYY-MM-DD HH:MM[:SS]`, unix seconds or
/// milliseconds (string or number). Non-positive unix values parse to the zero time, like Go.
pub fn parse_time_value(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(s) {
                return Some(t.with_timezone(&Utc));
            }
            for layout in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, layout) {
                    return Some(t.and_utc());
                }
            }
            s.parse::<i64>().ok().map(normalise_unix)
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(normalise_unix(i));
            }
            n.as_f64().map(|f| normalise_unix(f as i64))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_jwt;
    use serde_json::json;

    fn auth_with(meta: Value) -> Auth {
        let mut a = Auth::default();
        if let Value::Object(m) = meta {
            a.metadata = m;
        }
        a
    }

    #[test]
    fn expiry_prefers_jwt_then_keys_then_relative() {
        let jwt = make_jwt(&json!({"exp": 1_900_000_000}));
        let a = auth_with(json!({"access_token": jwt, "expired": "2001-01-01T00:00:00Z"}));
        assert_eq!(a.expiration_time().unwrap().timestamp(), 1_900_000_000);

        let a =
            auth_with(json!({"access_token": "opaque", "expired": "2030-01-02T03:04:05+02:00"}));
        assert_eq!(
            a.expiration_time().unwrap().to_rfc3339(),
            "2030-01-02T01:04:05+00:00"
        );

        let a = auth_with(json!({"expires_in": 3600, "timestamp": 1_700_000_000_000i64}));
        assert_eq!(a.expiration_time().unwrap().timestamp(), 1_700_003_600);

        let a = auth_with(json!({"token": {"expiry": "2030-01-01 10:00"}}));
        assert_eq!(a.expiration_time().unwrap().timestamp(), 1_893_492_000);

        assert!(auth_with(json!({"foo": 1})).expiration_time().is_none());
    }

    #[test]
    fn auth_kind_derivation() {
        assert_eq!(auth_with(json!({"access_token": "x"})).auth_kind(), "oauth");
        assert_eq!(
            auth_with(json!({"auth_kind": "OAuth2"})).auth_kind(),
            "oauth"
        );
        let mut a = Auth::default();
        a.attributes.insert("api_key".into(), "k".into());
        assert_eq!(a.auth_kind(), "apikey");
        assert_eq!(auth_with(json!({"note": "x"})).auth_kind(), "");
    }

    #[test]
    fn index_seed_for_file_and_api_key_auths() {
        let mut a = Auth::default();
        a.id = "claude-a@b.json".into();
        a.provider = "Claude".into();
        a.attributes
            .insert("path".into(), "/auth/claude-a@b.json".into());
        a.metadata.insert("type".into(), json!("claude"));
        assert_eq!(a.index_seed(), "claude:/auth/claude-a@b.json");
        let idx = a.ensure_index();
        assert_eq!(idx.len(), 16);
        assert_eq!(
            idx,
            crate::util::sha256_hex_prefix("claude:/auth/claude-a@b.json", 16)
        );

        let mut k = Auth::default();
        k.id = "claude:abc".into();
        k.provider = "claude".into();
        k.attributes.insert("api_key".into(), "sk".into());
        k.attributes.insert("base_url".into(), "https://x".into());
        assert_eq!(k.index_seed(), "claude-api-key:https://x+sk");
        let mut plain = Auth::default();
        plain.id = "x".into();
        assert_eq!(plain.index_seed(), "id:x");
    }

    #[test]
    fn serde_roundtrip_uses_go_tags_and_zero_time() {
        let mut a = Auth::default();
        a.id = "a.json".into();
        a.provider = "codex".into();
        a.status = Status::Active;
        a.metadata.insert("email".into(), json!("e@x"));
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["status"], "active");
        assert_eq!(v["created_at"], "0001-01-01T00:00:00Z");
        assert!(v.get("index").is_none() && v.get("storage").is_none());
        let back: Auth = serde_json::from_value(v).unwrap();
        assert_eq!(back.created_at, None);
        assert_eq!(back.metadata.get("email"), Some(&json!("e@x")));
    }

    #[test]
    fn recent_request_ring_counts_per_bucket() {
        let mut a = Auth::default();
        let now = Utc::now();
        a.record_recent_request(now, true);
        a.record_recent_request(now, false);
        a.record_recent_request(now, true);
        let snap = a.recent_requests_snapshot(now);
        assert_eq!(snap.len(), 20);
        let last = snap.last().unwrap();
        assert_eq!((last.success, last.failed), (2, 1));
    }
}
