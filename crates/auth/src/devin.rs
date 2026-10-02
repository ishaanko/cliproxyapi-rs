//! Devin (Cognition / Windsurf) login (internal/auth/devin/*, sdk/auth/devin.go).
//!
//! PKCE browser flow through app.devin.ai with a loopback callback (ephemeral port), code exchange
//! at api.devin.ai, profile lookup, and a Connect-protocol `GetUserStatus` call whose protobuf
//! request/response are hand-encoded here (no protobuf dependency). The credential is a session
//! token; there is no refresh.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::http::{build_client_ext, read_text};
use crate::oauth::parse_oauth_callback;
use crate::types::{Auth, QuotaState, Status};
use crate::util::{format_rfc3339_utc, random_hex};

pub const DEFAULT_APP_BASE_URL: &str = "https://app.devin.ai";
pub const DEFAULT_API_BASE_URL: &str = "https://api.devin.ai";
pub const DEFAULT_SERVER_URL: &str = "https://server.codeium.com";
pub const GET_USER_STATUS_PATH: &str =
    "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const TOKEN_PREFIX: &str = "devin-session-token$";
const FINGERPRINT_HEX_LEN: usize = 732;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// `FormatSessionToken`: raw JWT-ish tokens (`eyJ...`) get the `devin-session-token$` prefix.
pub fn format_session_token(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with(TOKEN_PREFIX) {
        return t.to_string();
    }
    if t.starts_with("eyJ") {
        return format!("{TOKEN_PREFIX}{t}");
    }
    t.to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevinUserStatus {
    pub email: String,
    pub user_name: String,
    pub user_id: String,
    pub team_id: String,
    pub org_id: String,
    pub org_name: String,
    pub plan: String,
    pub daily_quota_remaining_percent: i64,
    pub weekly_quota_remaining_percent: i64,
    pub daily_quota_reset_at: Option<DateTime<Utc>>,
    pub weekly_quota_reset_at: Option<DateTime<Utc>>,
    pub plan_start: Option<DateTime<Utc>>,
    pub plan_end: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct DevinAuthService {
    client: reqwest::Client,
    app_base_url: String,
    api_base_url: String,
    server_base_url: String,
}

impl DevinAuthService {
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
            app_base_url: DEFAULT_APP_BASE_URL.into(),
            api_base_url: DEFAULT_API_BASE_URL.into(),
            server_base_url: DEFAULT_SERVER_URL.into(),
        }
    }

    pub fn set_app_base_url(&mut self, url: &str) {
        if !url.trim().is_empty() {
            self.app_base_url = url.trim().trim_end_matches('/').to_string();
        }
    }

    pub fn set_api_base_url(&mut self, url: &str) {
        if !url.trim().is_empty() {
            self.api_base_url = url.trim().trim_end_matches('/').to_string();
        }
    }

    pub fn set_server_base_url(&mut self, url: &str) {
        if !url.trim().is_empty() {
            self.server_base_url = url.trim().trim_end_matches('/').to_string();
        }
    }

    /// `<app>/auth/cli/continue?...`. An empty `redirect_uri` is the paste-the-code variant, which
    /// adds `cli_pkce_marker=1`. Parameter order is part of the Go output.
    pub fn build_authorization_url(
        &self,
        redirect_uri: &str,
        code_challenge: &str,
        state: &str,
    ) -> String {
        use crate::util::query_escape as esc;
        let redirect = redirect_uri.trim();
        let mut parts = Vec::new();
        if !redirect.is_empty() {
            parts.push(format!("redirect_uri={}", esc(redirect)));
        }
        if !state.is_empty() {
            parts.push(format!("state={}", esc(state)));
        }
        parts.push("prompt=select_account".to_string());
        parts.push(format!("code_challenge={}", esc(code_challenge)));
        parts.push("code_challenge_method=S256".to_string());
        if redirect.is_empty() {
            parts.push("cli_pkce_marker=1".to_string());
        }
        format!(
            "{}/auth/cli/continue?{}",
            self.app_base_url.trim_end_matches('/'),
            parts.join("&")
        )
    }

    /// Exchanges an authorization code for a session token.
    pub async fn exchange_code_for_token(&self, code: &str, code_verifier: &str) -> Result<String> {
        let body =
            serde_json::json!({ "code": code.trim(), "code_verifier": code_verifier.trim() })
                .to_string();
        let resp = self
            .client
            .post(format!(
                "{}/auth/cli/token",
                self.api_base_url.trim_end_matches('/')
            ))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                AuthFlowError::Transport(format!(
                    "devin token exchange failed: {}",
                    e.without_url()
                ))
            })?;
        let (status, text) = read_text(resp).await.map_err(|e| {
            AuthFlowError::Transport(format!("read token exchange response: {}", e.without_url()))
        })?;
        if !(200..300).contains(&status) {
            return Err(AuthFlowError::other(format!(
                "token exchange failed with status {status}: {text}"
            )));
        }
        let token = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("token").map(value_text))
            .unwrap_or_default()
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(AuthFlowError::other(format!(
                "response did not contain a valid token: {text}"
            )));
        }
        Ok(token)
    }

    /// `(user_name, user_id, org_id)` from `/v3/self`; empty strings on any non-200.
    pub async fn fetch_self_profile(
        &self,
        session_token: &str,
    ) -> Result<(String, String, String)> {
        let resp = self
            .client
            .get(format!(
                "{}/v3/self",
                self.api_base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {session_token}"))
            .header("Accept", "application/json")
            .send()
            .await?;
        let (status, text) = read_text(resp).await?;
        if status != 200 {
            return Ok(Default::default());
        }
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let get = |k: &str| v.get(k).map(value_text).unwrap_or_default();
        Ok((get("user_name"), get("user_id"), get("org_id")))
    }

    /// Connect-protocol `GetUserStatus`: plan, quota percentages and reset times.
    pub async fn fetch_user_status(
        &self,
        session_token: &str,
        device_seed: &str,
    ) -> Result<DevinUserStatus> {
        let session_token = session_token.trim();
        if session_token.is_empty() {
            return Err(AuthFlowError::other(
                "devin auth service: session token is required",
            ));
        }
        let request =
            build_get_user_status_request(session_token, &generate_device_fingerprint(device_seed));
        let resp = self
            .client
            .post(format!(
                "{}{}",
                self.server_base_url.trim_end_matches('/'),
                GET_USER_STATUS_PATH
            ))
            .header(
                "Authorization",
                format!("Basic {session_token}-{session_token}"),
            )
            .header("Connect-Protocol-Version", "1")
            .header("Content-Type", "application/proto")
            .header("Accept", "*/*")
            .header("User-Agent", "")
            .body(request)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        if status != 200 {
            return Err(AuthFlowError::other(format!(
                "devin seat management error (status {status}): {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        parse_get_user_status_response(&bytes)
    }

    /// `CreateAuthRecord`: profile and status lookups are advisory (failures only logged).
    pub async fn create_auth_record(&self, token: &str) -> Result<Auth> {
        let session_token = format_session_token(token);
        if session_token.is_empty() {
            return Err(AuthFlowError::other("devin session token is required"));
        }
        let (mut user_name, mut user_id, mut org_id) =
            match self.fetch_self_profile(&session_token).await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!("failed to fetch devin user profile");
                    Default::default()
                }
            };
        let user_status = match self.fetch_user_status(&session_token, "").await {
            Ok(s) => Some(s),
            Err(_) => {
                tracing::warn!("failed to fetch devin user status and quota");
                None
            }
        };
        Ok(build_auth_record(
            &session_token,
            &mut user_name,
            &mut user_id,
            &mut org_id,
            user_status.as_ref(),
        ))
    }
}

/// gjson-style `.String()`: strings as-is, other scalars as their JSON text.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn build_auth_record(
    session_token: &str,
    user_name: &mut String,
    user_id: &mut String,
    org_id: &mut String,
    status: Option<&DevinUserStatus>,
) -> Auth {
    let (mut email, mut plan) = (String::new(), String::new());
    if let Some(s) = status {
        if user_name.is_empty() {
            *user_name = s.user_name.clone();
        }
        if user_id.is_empty() {
            *user_id = s.user_id.clone();
        }
        if org_id.is_empty() {
            *org_id = s.org_id.clone();
        }
        email = s.email.clone();
        plan = s.plan.clone();
    }

    let mut identifier = user_name.clone();
    if identifier.is_empty() {
        identifier = user_id.clone();
    }
    if identifier.is_empty() {
        let digest = Sha256::digest(session_token.as_bytes());
        identifier = format!("user-{}", hex::encode(&digest[..8]));
    }
    let mut file_identifier: String = identifier
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if file_identifier != identifier || file_identifier.len() > 160 {
        let digest = Sha256::digest(identifier.as_bytes());
        file_identifier = format!("user-{}", hex::encode(&digest[..8]));
    }
    let file_name = format!("devin-{file_identifier}.json");
    let label = if email.is_empty() {
        format!("Devin ({identifier})")
    } else {
        format!("Devin ({identifier} - {email})")
    };

    let mut attributes = std::collections::BTreeMap::new();
    for (k, v) in [
        ("api_key", session_token),
        ("session_token", session_token),
        ("user_name", user_name.as_str()),
        ("user_id", user_id.as_str()),
        ("org_id", org_id.as_str()),
        ("base_url", DEFAULT_SERVER_URL),
        ("auth_kind", "oauth"),
    ] {
        attributes.insert(k.to_string(), v.to_string());
    }
    let mut metadata = Metadata::new();
    for (k, v) in [
        ("type", "devin"),
        ("api_key", session_token),
        ("session_token", session_token),
        ("user_name", user_name.as_str()),
        ("user_id", user_id.as_str()),
        ("org_id", org_id.as_str()),
        ("auth_kind", "oauth"),
    ] {
        metadata.insert(k.to_string(), v.into());
    }
    if !email.is_empty() {
        attributes.insert("email".into(), email.clone());
        metadata.insert("email".into(), email.into());
    }
    if !plan.is_empty() {
        attributes.insert("plan".into(), plan.clone());
        metadata.insert("plan".into(), plan.clone().into());
    }

    let mut signals = std::collections::BTreeMap::new();
    if !plan.is_empty() {
        signals.insert("plan".to_string(), plan);
    }
    if let Some(s) = status {
        signals.insert(
            "daily_quota_remaining_percent".into(),
            format!("{}%", s.daily_quota_remaining_percent),
        );
        signals.insert(
            "weekly_quota_remaining_percent".into(),
            format!("{}%", s.weekly_quota_remaining_percent),
        );
        for (key, t) in [
            ("daily_quota_reset_at", s.daily_quota_reset_at),
            ("weekly_quota_reset_at", s.weekly_quota_reset_at),
            ("plan_start", s.plan_start),
            ("plan_end", s.plan_end),
        ] {
            if let Some(t) = t {
                signals.insert(key.into(), format_rfc3339_utc(t));
            }
        }
    }

    let mut auth = Auth::new(file_name, "devin");
    auth.label = label;
    auth.status = Status::Active;
    auth.attributes = attributes;
    auth.metadata = metadata;
    auth.quota = QuotaState {
        observed_at: Some(Utc::now()),
        signals,
        ..Default::default()
    };
    auth
}

// ---- Manual paste handling (no-browser / prompt fallback) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualPaste {
    /// Empty input.
    Empty,
    /// A raw session token pasted directly.
    Token(String),
    /// An authorization code (bare or from a callback URL).
    Code(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManualPasteError {
    /// Input is neither a token, a callback URL nor a bare code: keep waiting.
    #[error("unrecognized devin authorization code or token format")]
    Unrecognized,
    #[error("devin oauth error: {0}")]
    OAuth(String),
    #[error("devin oauth state mismatch (possible CSRF)")]
    StateMismatch,
}

/// `parseDevinManualPaste`: token, callback URL or bare authorization code.
pub fn parse_manual_paste(
    input: &str,
    expected_state: &str,
) -> std::result::Result<ManualPaste, ManualPasteError> {
    let trimmed = input.trim().trim_matches(['"', '\'']).trim();
    if trimmed.is_empty() {
        return Ok(ManualPaste::Empty);
    }
    if trimmed.starts_with(TOKEN_PREFIX) || trimmed.starts_with("eyJ") {
        return Ok(ManualPaste::Token(trimmed.to_string()));
    }
    if let Ok(Some(parsed)) = parse_oauth_callback(trimmed) {
        let mut err = parsed.error.trim().to_string();
        if !err.is_empty() {
            let desc = parsed.error_description.trim();
            if !desc.is_empty() {
                err = format!("{err}: {desc}");
            }
            return Err(ManualPasteError::OAuth(err));
        }
        if !parsed.code.is_empty() {
            if !expected_state.is_empty()
                && !parsed.state.is_empty()
                && parsed.state != expected_state
            {
                return Err(ManualPasteError::StateMismatch);
            }
            return Ok(ManualPaste::Code(parsed.code));
        }
    }
    if !trimmed.contains([' ', '\t', '\r', '\n', '/', '?', '#', '=']) {
        return Ok(ManualPaste::Code(trimmed.to_string()));
    }
    Err(ManualPasteError::Unrecognized)
}

// ---- Device fingerprint and protobuf ----

/// 732 hex chars: random when `seed` is empty, otherwise sha256(`seed-counter`) chained.
pub fn generate_device_fingerprint(seed: &str) -> String {
    if seed.is_empty() {
        return random_hex(FINGERPRINT_HEX_LEN / 2);
    }
    let mut out = String::new();
    let mut counter = 0;
    while out.len() < FINGERPRINT_HEX_LEN {
        out.push_str(&hex::encode(Sha256::digest(
            format!("{seed}-{counter}").as_bytes(),
        )));
        counter += 1;
    }
    out.truncate(FINGERPRINT_HEX_LEN);
    out
}

pub(crate) mod pb {
    //! Just enough protobuf wire format for the GetUserStatus messages.

    pub const VARINT: u8 = 0;
    pub const FIXED64: u8 = 1;
    pub const BYTES: u8 = 2;
    pub const FIXED32: u8 = 5;

    pub fn append_varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    pub fn append_string_field(buf: &mut Vec<u8>, num: u32, value: &str) {
        append_varint(buf, ((num as u64) << 3) | BYTES as u64);
        append_varint(buf, value.len() as u64);
        buf.extend_from_slice(value.as_bytes());
    }

    pub fn append_bytes_field(buf: &mut Vec<u8>, num: u32, value: &[u8]) {
        append_varint(buf, ((num as u64) << 3) | BYTES as u64);
        append_varint(buf, value.len() as u64);
        buf.extend_from_slice(value);
    }

    /// Reads a varint, returning `(value, bytes_consumed)`.
    pub fn consume_varint(data: &[u8]) -> Option<(u64, usize)> {
        let mut v = 0u64;
        for (i, b) in data.iter().take(10).enumerate() {
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Some((v, i + 1));
            }
        }
        None
    }

    /// `(field number, wire type, bytes consumed)`.
    pub fn consume_tag(data: &[u8]) -> Option<(u32, u8, usize)> {
        let (tag, n) = consume_varint(data)?;
        if tag >> 3 == 0 {
            return None;
        }
        Some(((tag >> 3) as u32, (tag & 7) as u8, n))
    }

    /// Length-delimited payload and total bytes consumed.
    pub fn consume_bytes(data: &[u8]) -> Option<(&[u8], usize)> {
        let (len, n) = consume_varint(data)?;
        let end = n.checked_add(usize::try_from(len).ok()?)?;
        if end > data.len() {
            return None;
        }
        Some((&data[n..end], end))
    }

    /// Skips one field value of the given wire type; returns bytes consumed.
    pub fn consume_field_value(wire: u8, data: &[u8]) -> Option<usize> {
        match wire {
            VARINT => consume_varint(data).map(|(_, n)| n),
            FIXED64 => (data.len() >= 8).then_some(8),
            FIXED32 => (data.len() >= 4).then_some(4),
            BYTES => consume_bytes(data).map(|(_, n)| n),
            _ => None,
        }
    }
}

fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Protobuf `GetUserStatusRequest` as sent by the Windsurf client.
pub fn build_get_user_status_request(session_token: &str, device_fingerprint: &str) -> Vec<u8> {
    let fingerprint = if device_fingerprint.is_empty() {
        generate_device_fingerprint(session_token)
    } else {
        device_fingerprint.to_string()
    };
    let mut f1 = Vec::new();
    pb::append_string_field(&mut f1, 1, "chisel");
    pb::append_string_field(&mut f1, 2, "3000.10.21");
    pb::append_string_field(&mut f1, 3, session_token);
    pb::append_string_field(&mut f1, 4, "en");
    pb::append_string_field(&mut f1, 5, go_os());
    pb::append_string_field(&mut f1, 7, "3000.10.21");
    pb::append_string_field(&mut f1, 12, "chisel");
    pb::append_string_field(&mut f1, 31, &fingerprint);
    let mut req = Vec::new();
    pb::append_bytes_field(&mut req, 1, &f1);
    req
}

/// Parses a `GetUserStatusResponse` (field 1 holds the user status message).
pub fn parse_get_user_status_response(data: &[u8]) -> Result<DevinUserStatus> {
    if data.is_empty() {
        return Err(AuthFlowError::other("empty response data"));
    }
    let mut status = DevinUserStatus::default();
    let mut rem = data;
    while !rem.is_empty() {
        let (num, wire, n) = pb::consume_tag(rem)
            .ok_or_else(|| AuthFlowError::other("proto: cannot parse invalid wire-format data"))?;
        rem = &rem[n..];
        if num == 1 && wire == pb::BYTES {
            let (bytes, m) = pb::consume_bytes(rem).ok_or_else(|| {
                AuthFlowError::other("proto: cannot parse invalid wire-format data")
            })?;
            rem = &rem[m..];
            parse_user_status(bytes, &mut status);
        } else {
            let m = pb::consume_field_value(wire, rem).ok_or_else(|| {
                AuthFlowError::other("proto: cannot parse invalid wire-format data")
            })?;
            rem = &rem[m..];
        }
    }
    Ok(status)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn unix_utc(secs: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(secs, 0)
}

/// Walks a message, calling `on_field(num, wire, payload)` for each field. Stops silently on
/// malformed data, like the Go helpers.
fn for_each_field<'a>(data: &'a [u8], mut on_field: impl FnMut(u32, Field<'a>)) {
    let mut rem = data;
    while !rem.is_empty() {
        let Some((num, wire, n)) = pb::consume_tag(rem) else {
            return;
        };
        rem = &rem[n..];
        match wire {
            pb::BYTES => {
                let Some((bytes, m)) = pb::consume_bytes(rem) else {
                    return;
                };
                rem = &rem[m..];
                on_field(num, Field::Bytes(bytes));
            }
            pb::VARINT => {
                let Some((v, m)) = pb::consume_varint(rem) else {
                    return;
                };
                rem = &rem[m..];
                on_field(num, Field::Varint(v));
            }
            other => {
                let Some(m) = pb::consume_field_value(other, rem) else {
                    return;
                };
                rem = &rem[m..];
            }
        }
    }
}

enum Field<'a> {
    Bytes(&'a [u8]),
    Varint(u64),
}

fn parse_user_status(data: &[u8], status: &mut DevinUserStatus) {
    for_each_field(data, |num, f| {
        if let Field::Bytes(b) = f {
            match num {
                3 => status.user_name = text(b),
                5 => status.team_id = text(b),
                7 => status.email = text(b),
                13 => parse_plan_status(b, status),
                36 => status.user_id = text(b),
                _ => {}
            }
        }
    });
}

fn parse_plan_status(data: &[u8], status: &mut DevinUserStatus) {
    for_each_field(data, |num, f| match f {
        Field::Bytes(b) => match num {
            1 => parse_plan_info(b, status),
            2 => {
                let sec = parse_seconds_subfield(b);
                if sec > 0 {
                    status.plan_start = unix_utc(sec);
                }
            }
            3 => {
                let sec = parse_seconds_subfield(b);
                if sec > 0 {
                    status.plan_end = unix_utc(sec);
                }
            }
            _ => {}
        },
        Field::Varint(v) => match num {
            14 => status.daily_quota_remaining_percent = v as i64,
            15 => status.weekly_quota_remaining_percent = v as i64,
            17 if v > 0 => status.daily_quota_reset_at = unix_utc(v as i64),
            18 if v > 0 => status.weekly_quota_reset_at = unix_utc(v as i64),
            _ => {}
        },
    });
}

fn parse_plan_info(data: &[u8], status: &mut DevinUserStatus) {
    for_each_field(data, |num, f| {
        if let Field::Bytes(b) = f {
            match num {
                2 => status.plan = text(b),
                33 => parse_plan_info_org(b, status),
                _ => {}
            }
        }
    });
}

fn parse_plan_info_org(data: &[u8], status: &mut DevinUserStatus) {
    for_each_field(data, |num, f| {
        if let Field::Bytes(b) = f {
            match num {
                4 => status.org_id = text(b),
                8 => status.org_name = text(b),
                _ => {}
            }
        }
    });
}

/// `google.protobuf.Timestamp.seconds` (field 1 varint) of a sub-message.
fn parse_seconds_subfield(data: &[u8]) -> i64 {
    let mut out = 0;
    let mut found = false;
    for_each_field(data, |num, f| {
        if let (false, 1, Field::Varint(v)) = (found, num, f) {
            out = v as i64;
            found = true;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_field(num: u32, v: &str) -> Vec<u8> {
        let mut b = Vec::new();
        pb::append_string_field(&mut b, num, v);
        b
    }

    fn varint_field(num: u32, v: u64) -> Vec<u8> {
        let mut b = Vec::new();
        pb::append_varint(&mut b, (num as u64) << 3);
        pb::append_varint(&mut b, v);
        b
    }

    fn bytes_field(num: u32, v: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        pb::append_bytes_field(&mut b, num, v);
        b
    }

    #[test]
    fn session_token_formatting() {
        assert_eq!(
            format_session_token(" eyJabc "),
            "devin-session-token$eyJabc"
        );
        assert_eq!(
            format_session_token("devin-session-token$x"),
            "devin-session-token$x"
        );
        assert_eq!(format_session_token("plain"), "plain");
    }

    #[test]
    fn authorization_url_param_order() {
        let svc = DevinAuthService::with_client(reqwest::Client::new());
        assert_eq!(
            svc.build_authorization_url("http://127.0.0.1:5000/callback", "chal", "st"),
            "https://app.devin.ai/auth/cli/continue?redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fcallback&state=st&prompt=select_account&code_challenge=chal&code_challenge_method=S256"
        );
        assert_eq!(
            svc.build_authorization_url("", "chal", "st"),
            "https://app.devin.ai/auth/cli/continue?state=st&prompt=select_account&code_challenge=chal&code_challenge_method=S256&cli_pkce_marker=1"
        );
    }

    #[test]
    fn request_encoding_has_expected_fields() {
        let req = build_get_user_status_request("tok", "fp");
        // Outer: field 1 (tag 0x0a) wrapping the inner message.
        assert_eq!(req[0], 0x0a);
        let (inner, _) = pb::consume_bytes(&req[1..]).unwrap();
        let mut fields = Vec::new();
        for_each_field(inner, |n, f| {
            if let Field::Bytes(b) = f {
                fields.push((n, text(b)));
            }
        });
        assert_eq!(fields[0], (1, "chisel".to_string()));
        assert_eq!(fields[2], (3, "tok".to_string()));
        assert_eq!(fields.last().unwrap(), &(31, "fp".to_string()));
        assert_eq!(fields.len(), 8);
        assert_eq!(
            generate_device_fingerprint("seed").len(),
            FINGERPRINT_HEX_LEN
        );
        assert_eq!(
            generate_device_fingerprint("seed"),
            generate_device_fingerprint("seed")
        );
        assert_eq!(generate_device_fingerprint("").len(), FINGERPRINT_HEX_LEN);
    }

    #[test]
    fn parses_user_status_response() {
        let ts = |secs: u64| varint_field(1, secs);
        let plan_org = [string_field(4, "org-1"), string_field(8, "Org One")].concat();
        let plan_info = [string_field(2, "Pro"), bytes_field(33, &plan_org)].concat();
        let plan_status = [
            bytes_field(1, &plan_info),
            bytes_field(2, &ts(1_700_000_000)),
            bytes_field(3, &ts(1_800_000_000)),
            varint_field(14, 80),
            varint_field(15, 55),
            varint_field(17, 1_700_086_400),
            varint_field(18, 1_700_600_000),
        ]
        .concat();
        let user = [
            string_field(3, "alice"),
            string_field(7, "a@x.io"),
            bytes_field(13, &plan_status),
            string_field(36, "uid-9"),
        ]
        .concat();
        let resp = bytes_field(1, &user);

        let s = parse_get_user_status_response(&resp).unwrap();
        assert_eq!(
            (s.user_name.as_str(), s.email.as_str(), s.user_id.as_str()),
            ("alice", "a@x.io", "uid-9")
        );
        assert_eq!(
            (s.plan.as_str(), s.org_id.as_str(), s.org_name.as_str()),
            ("Pro", "org-1", "Org One")
        );
        assert_eq!(
            (
                s.daily_quota_remaining_percent,
                s.weekly_quota_remaining_percent
            ),
            (80, 55)
        );
        assert_eq!(s.plan_start.unwrap().timestamp(), 1_700_000_000);
        assert_eq!(s.plan_end.unwrap().timestamp(), 1_800_000_000);
        assert_eq!(s.daily_quota_reset_at.unwrap().timestamp(), 1_700_086_400);

        assert!(parse_get_user_status_response(&[]).is_err());
        assert!(parse_get_user_status_response(&[0x0a, 0xff]).is_err());
    }

    #[test]
    fn auth_record_naming_and_signals() {
        let status = DevinUserStatus {
            email: "a@x.io".into(),
            plan: "Pro".into(),
            daily_quota_remaining_percent: 80,
            weekly_quota_remaining_percent: 55,
            ..Default::default()
        };
        let auth = build_auth_record(
            "devin-session-token$eyJ",
            &mut "alice".to_string(),
            &mut "u1".to_string(),
            &mut "o1".to_string(),
            Some(&status),
        );
        assert_eq!(auth.id, "devin-alice.json");
        assert_eq!(auth.label, "Devin (alice - a@x.io)");
        assert_eq!(auth.attr("api_key"), "devin-session-token$eyJ");
        assert_eq!(auth.quota.signals["daily_quota_remaining_percent"], "80%");
        assert_eq!(auth.quota.signals["plan"], "Pro");
        assert_eq!(auth.status, Status::Active);

        // Unsafe identifiers fall back to a hash; no identity at all hashes the token.
        let weird = build_auth_record(
            "tok",
            &mut "a b/c".to_string(),
            &mut String::new(),
            &mut String::new(),
            None,
        );
        assert!(
            weird.id.starts_with("devin-user-")
                && weird.id.len() == "devin-user-".len() + 16 + ".json".len()
        );
        let anon = build_auth_record(
            "tok",
            &mut String::new(),
            &mut String::new(),
            &mut String::new(),
            None,
        );
        assert!(anon.id.starts_with("devin-user-"));
    }

    #[test]
    fn manual_paste_variants() {
        assert_eq!(parse_manual_paste("  ", "s").unwrap(), ManualPaste::Empty);
        assert_eq!(
            parse_manual_paste("\"eyJabc\"", "s").unwrap(),
            ManualPaste::Token("eyJabc".into())
        );
        assert_eq!(
            parse_manual_paste("http://127.0.0.1:1/callback?code=c1&state=s", "s").unwrap(),
            ManualPaste::Code("c1".into())
        );
        assert_eq!(
            parse_manual_paste("http://x/cb?code=c1&state=other", "s"),
            Err(ManualPasteError::StateMismatch)
        );
        assert_eq!(
            parse_manual_paste("?error=access_denied&error_description=no", "s"),
            Err(ManualPasteError::OAuth("access_denied: no".into()))
        );
        assert_eq!(
            parse_manual_paste("barecode123", "s").unwrap(),
            ManualPaste::Code("barecode123".into())
        );
        assert_eq!(
            parse_manual_paste("two words", "s"),
            Err(ManualPasteError::Unrecognized)
        );
    }
}
