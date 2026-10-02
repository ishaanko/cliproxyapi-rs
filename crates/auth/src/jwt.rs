//! Unverified JWT parsing: generic `exp` extraction (used for credential expiry) and the Codex /
//! OpenAI `id_token` claims (internal/auth/codex/jwt_parser.go).

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::util::zero_time;

/// URL-safe base64 that accepts both padded and unpadded input, like Go's `URLEncoding` after the
/// padding fix-up (and tolerant of non-canonical trailing bits, as Go is).
const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Decodes one base64url JWT segment, padded or not.
pub fn decode_segment(seg: &str) -> Option<Vec<u8>> {
    URL_SAFE_LENIENT.decode(seg).ok()
}

/// Go `normaliseUnix`: non-positive values are the zero time, values above 1e12 are milliseconds.
pub fn normalise_unix(raw: i64) -> DateTime<Utc> {
    if raw <= 0 {
        return zero_time();
    }
    if raw > 1_000_000_000_000 {
        return DateTime::from_timestamp_millis(raw).unwrap_or_else(zero_time);
    }
    DateTime::from_timestamp(raw, 0).unwrap_or_else(zero_time)
}

/// `exp` claim of a JWT (number or numeric string), without signature verification.
pub fn parse_jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = decode_segment(parts[1])?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    match claims.get("exp")? {
        Value::Number(n) => {
            let secs = n.as_f64()?;
            (secs > 0.0).then(|| normalise_unix(secs as i64))
        }
        Value::String(s) => {
            let secs = s.trim().parse::<i64>().ok()?;
            (secs > 0).then(|| normalise_unix(secs))
        }
        _ => None,
    }
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Organization {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub is_default: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub role: String,
    #[serde(default, deserialize_with = "null_default")]
    pub title: String,
}

/// The `https://api.openai.com/auth` claim namespace.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexAuthInfo {
    #[serde(default, deserialize_with = "null_default")]
    pub chatgpt_account_id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub chatgpt_plan_type: String,
    #[serde(default)]
    pub chatgpt_subscription_active_start: Value,
    #[serde(default)]
    pub chatgpt_subscription_active_until: Value,
    #[serde(default)]
    pub chatgpt_subscription_last_checked: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "null_default")]
    pub chatgpt_user_id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub groups: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub organizations: Vec<Organization>,
    #[serde(default, deserialize_with = "null_default")]
    pub user_id: String,
}

/// Codex `id_token` claims (`JWTClaims`). Only `email` and the auth namespace are consumed by the
/// login and refresh paths; the rest is parsed for parity.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexClaims {
    #[serde(default, deserialize_with = "null_default")]
    pub at_hash: String,
    #[serde(default, deserialize_with = "null_default")]
    pub aud: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub auth_provider: String,
    #[serde(default, deserialize_with = "null_default")]
    pub auth_time: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub email: String,
    #[serde(default, deserialize_with = "null_default")]
    pub email_verified: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub exp: i64,
    #[serde(
        default,
        rename = "https://api.openai.com/auth",
        deserialize_with = "null_default"
    )]
    pub codex_auth_info: CodexAuthInfo,
    #[serde(default, deserialize_with = "null_default")]
    pub iat: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub iss: String,
    #[serde(default, deserialize_with = "null_default")]
    pub jti: String,
    #[serde(default, deserialize_with = "null_default")]
    pub rat: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub sid: String,
    #[serde(default, deserialize_with = "null_default")]
    pub sub: String,
}

pub const DEFAULT_PLAN_TYPE: &str = "free";

impl CodexClaims {
    pub fn user_email(&self) -> &str {
        &self.email
    }

    pub fn account_id(&self) -> &str {
        &self.codex_auth_info.chatgpt_account_id
    }

    /// Plan type, defaulting to `free` when the claim is empty.
    pub fn plan_type(&self) -> String {
        let pt = self.codex_auth_info.chatgpt_plan_type.trim();
        if pt.is_empty() {
            DEFAULT_PLAN_TYPE.to_string()
        } else {
            pt.to_string()
        }
    }
}

/// Parses a Codex `id_token` without verifying the signature.
pub fn parse_codex_id_token(token: &str) -> Result<CodexClaims, JwtError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(JwtError::Format(parts.len()));
    }
    let data = decode_segment(parts[1]).ok_or(JwtError::Decode)?;
    serde_json::from_slice(&data).map_err(|e| JwtError::Claims(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("invalid JWT token format: expected 3 parts, got {0}")]
    Format(usize),
    #[error("failed to decode JWT claims")]
    Decode,
    #[error("failed to unmarshal JWT claims: {0}")]
    Claims(String),
}

/// Unverified claims as a loose map (xAI identity extraction: `email`, `sub`).
pub fn parse_claims_map(token: &str) -> Option<serde_json::Map<String, Value>> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let raw = decode_segment(payload)?;
    match serde_json::from_slice::<Value>(&raw).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_jwt;
    use serde_json::json;

    #[test]
    fn exp_accepts_number_string_and_millis() {
        let t = make_jwt(&json!({"exp": 1_900_000_000}));
        assert_eq!(parse_jwt_exp(&t).unwrap().timestamp(), 1_900_000_000);
        let t = make_jwt(&json!({"exp": "1900000000"}));
        assert_eq!(parse_jwt_exp(&t).unwrap().timestamp(), 1_900_000_000);
        let t = make_jwt(&json!({"exp": 1_900_000_000_000i64}));
        assert_eq!(parse_jwt_exp(&t).unwrap().timestamp(), 1_900_000_000);
        assert!(parse_jwt_exp(&make_jwt(&json!({"exp": 0}))).is_none());
        assert!(parse_jwt_exp("not.a.jwt!").is_none());
        assert!(parse_jwt_exp("a.b").is_none());
    }

    #[test]
    fn codex_claims_and_plan_default() {
        let t = make_jwt(&json!({
            "email": "a@b.com",
            "aud": ["app"],
            "exp": 1,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-1", "chatgpt_plan_type": "plus"}
        }));
        let c = parse_codex_id_token(&t).unwrap();
        assert_eq!(c.user_email(), "a@b.com");
        assert_eq!(c.account_id(), "acc-1");
        assert_eq!(c.plan_type(), "plus");

        let t = make_jwt(&json!({"email": "x@y.z"}));
        assert_eq!(parse_codex_id_token(&t).unwrap().plan_type(), "free");
        assert!(parse_codex_id_token("only.two").is_err());
    }
}
