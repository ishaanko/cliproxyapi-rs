//! Local ephemeral Realtime keys (`ek_...`) bound to a normalized session (Go:
//! client_secret.go).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cpa_core::util::{GoJsonStyle, go_json_canonicalize, go_json_sorted};
use parking_lot::Mutex;
use rand::RngCore;
use serde_json::{Map, Value, json};

use crate::reply::{Reply, realtime_error};
use crate::util::{self, codex_realtime_model};
use crate::{Caller, Handler};

pub const CLIENT_SECRET_PREFIX: &str = "ek_";
const DEFAULT_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MIN_LIFETIME_SECS: i64 = 10;
const MAX_LIFETIME_SECS: i64 = 2 * 60 * 60;
pub const CLIENT_SECRET_MAX_BODY: usize = 64 << 10;
const MAX_ENTRIES: usize = 1024;
const MAX_ENTRIES_PER_ISSUER: usize = 64;

pub const ERR_INVALID_CLIENT_SECRET: &str = "Realtime client secret is invalid or expired";
const ERR_CAPACITY: &str = "Realtime client secret capacity exhausted";
const ERR_UNSUPPORTED_SESSION_TYPE: &str = "Realtime session type is not supported";

/// The local session configuration associated with an ephemeral key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientSecretAuthorization {
    pub principal: String,
    pub issuer_principal: String,
    pub issuer_provider: String,
    /// Canonical upstream session JSON.
    pub session: String,
}

struct Entry {
    authorization: ClientSecretAuthorization,
    expires_at: SystemTime,
}

pub(crate) enum CreateError {
    Capacity,
}

/// Clock override for tests.
pub(crate) type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

pub struct ClientSecretStore {
    entries: Mutex<HashMap<String, Entry>>,
    now: Mutex<Clock>,
}

impl Default for ClientSecretStore {
    fn default() -> Self {
        ClientSecretStore { entries: Mutex::new(HashMap::new()), now: Mutex::new(Arc::new(SystemTime::now)) }
    }
}

/// `randomRealtimeID`: prefix plus `size` random bytes, base64url without padding.
pub(crate) fn random_realtime_id(prefix: &str, size: usize) -> String {
    let mut payload = vec![0u8; size];
    rand::rng().fill_bytes(&mut payload);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(payload))
}

impl ClientSecretStore {
    #[cfg(test)]
    pub(crate) fn set_clock(&self, clock: Clock) {
        *self.now.lock() = clock;
    }

    fn current_time(&self) -> SystemTime {
        (self.now.lock().clone())()
    }

    pub(crate) fn create(
        &self,
        session: &str,
        lifetime: Duration,
        issuer_principal: &str,
        issuer_provider: &str,
    ) -> Result<(String, ClientSecretAuthorization, SystemTime), CreateError> {
        let token = random_realtime_id(CLIENT_SECRET_PREFIX, 32);
        let session_id = random_realtime_id("sess_", 18);
        let authorization = ClientSecretAuthorization {
            principal: session_id,
            issuer_principal: issuer_principal.trim().to_string(),
            issuer_provider: issuer_provider.trim().to_string(),
            session: session.to_string(),
        };
        let now = self.current_time();
        let expires_at = now + lifetime;
        let mut entries = self.entries.lock();
        entries.retain(|_, e| e.expires_at > now);
        if entries.len() >= MAX_ENTRIES {
            return Err(CreateError::Capacity);
        }
        if !authorization.issuer_principal.is_empty() {
            let issuer_entries = entries
                .values()
                .filter(|e| {
                    e.authorization.issuer_principal == authorization.issuer_principal
                        && e.authorization.issuer_provider == authorization.issuer_provider
                })
                .count();
            if issuer_entries >= MAX_ENTRIES_PER_ISSUER {
                return Err(CreateError::Capacity);
            }
        }
        entries.insert(token.clone(), Entry { authorization: authorization.clone(), expires_at });
        Ok((token, authorization, expires_at))
    }

    pub(crate) fn authenticate(&self, token: &str) -> Result<ClientSecretAuthorization, String> {
        if !token.starts_with(CLIENT_SECRET_PREFIX) {
            return Err(ERR_INVALID_CLIENT_SECRET.into());
        }
        let now = self.current_time();
        let mut entries = self.entries.lock();
        match entries.get(token) {
            Some(entry) if entry.expires_at > now => Ok(entry.authorization.clone()),
            _ => {
                entries.remove(token);
                Err(ERR_INVALID_CLIENT_SECRET.into())
            }
        }
    }

    pub(crate) fn close(&self) {
        self.entries.lock().clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) enum SessionError {
    Unsupported(String),
    Invalid(String),
}

/// `normalizeClientSecretSession`: `(client session, upstream session)` as canonical JSON text.
pub(crate) fn normalize_client_secret_session(session: &str) -> Result<(String, String), SessionError> {
    let trimmed = session.trim();
    let text = if trimmed.is_empty() || trimmed == "null" {
        r#"{"type":"realtime","model":"gpt-realtime"}"#
    } else {
        session
    };
    let invalid = || SessionError::Invalid("session must be a valid JSON object".into());
    let Some(canonical) = go_json_canonicalize(text) else {
        return Err(invalid());
    };
    let Ok(Value::Object(mut client)) = serde_json::from_str::<Value>(&canonical) else {
        return Err(invalid());
    };
    let session_type = match client.get("type") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => {
            client.insert("type".into(), json!("realtime"));
            "realtime".to_string()
        }
    };
    if session_type != "realtime" {
        return Err(SessionError::Unsupported(format!("{ERR_UNSUPPORTED_SESSION_TYPE} by the Codex OAuth upstream: {session_type:?}")));
    }
    let model = match client.get("model") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => {
            client.insert("model".into(), json!("gpt-realtime"));
            "gpt-realtime".to_string()
        }
    };
    let encode = |map: &Map<String, Value>| go_json_sorted(&Value::Object(map.clone()), GoJsonStyle::MARSHAL_ANY).ok_or_else(invalid);
    let client_encoded = encode(&client)?;
    client.insert("model".into(), json!(codex_realtime_model(&model)));
    let upstream_encoded = encode(&client)?;
    Ok((client_encoded, upstream_encoded))
}

/// `realtimeSessionResponse`: the client session plus `id`, `object` and `expires_at`.
fn realtime_session_response(session: &str, session_id: &str, expires_at: SystemTime) -> Option<Map<String, Value>> {
    let Ok(Value::Object(mut response)) = serde_json::from_str::<Value>(session) else {
        return None;
    };
    response.insert("id".into(), json!(session_id));
    response.insert("object".into(), json!("realtime.session"));
    response.insert("expires_at".into(), json!(unix_secs(expires_at)));
    Some(response)
}

/// `clientSecretLifetime`.
fn client_secret_lifetime(expires_after: Option<&ExpiresAfter>) -> Result<Duration, String> {
    let Some(expires_after) = expires_after else {
        return Ok(DEFAULT_LIFETIME);
    };
    if !expires_after.anchor.is_empty() && expires_after.anchor != "created_at" {
        return Err("expires_after.anchor must be created_at".into());
    }
    if expires_after.seconds < MIN_LIFETIME_SECS || expires_after.seconds > MAX_LIFETIME_SECS {
        return Err(format!("expires_after.seconds must be between {MIN_LIFETIME_SECS} and {MAX_LIFETIME_SECS}"));
    }
    Ok(Duration::from_secs(expires_after.seconds as u64))
}

struct ExpiresAfter {
    anchor: String,
    seconds: i64,
}

/// Decodes `clientSecretCreateRequest`; `Err` for syntax or type errors.
fn decode_create_request(body: &[u8]) -> Result<(String, Option<ExpiresAfter>), ()> {
    let value: Value = serde_json::from_slice(body).map_err(|_| ())?;
    let root = match value {
        Value::Null => return Ok((String::new(), None)),
        Value::Object(o) => o,
        _ => return Err(()),
    };
    let session = match util::json_member(&root, "session") {
        Some(v) => serde_json::to_string(v).map_err(|_| ())?,
        None => String::new(),
    };
    let expires_after = match util::json_member(&root, "expires_after") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => {
            let anchor = match util::json_member(o, "anchor") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(()),
            };
            let seconds = match util::json_member(o, "seconds") {
                None | Some(Value::Null) => 0,
                Some(Value::Number(n)) => n.to_string().parse::<i64>().map_err(|_| ())?,
                Some(_) => return Err(()),
            };
            Some(ExpiresAfter { anchor, seconds })
        }
        Some(_) => return Err(()),
    };
    Ok((session, expires_after))
}

impl Handler {
    /// `AuthenticateClientSecret`: `matched` is false when the request carries no `ek_` bearer.
    pub fn authenticate_client_secret(
        &self,
        headers: &http::HeaderMap,
    ) -> (Option<ClientSecretAuthorization>, bool, Option<String>) {
        let token = util::bearer_token(headers);
        if !token.starts_with(CLIENT_SECRET_PREFIX) {
            return (None, false, None);
        }
        match self.inner.secrets.authenticate(&token) {
            Ok(authorization) => (Some(authorization), true, None),
            Err(e) => (None, true, Some(e)),
        }
    }

    /// `CreateClientSecret`: `POST /v1/realtime/client_secrets`.
    pub fn create_client_secret(&self, caller: &Caller, body: &[u8]) -> Reply {
        if body.len() > CLIENT_SECRET_MAX_BODY {
            return realtime_error(413, "Codex live request body too large", "invalid_request_error", "invalid_request");
        }
        let mut session = String::new();
        let mut expires_after = None;
        if !String::from_utf8_lossy(body).trim().is_empty() {
            match decode_create_request(body) {
                Ok((s, e)) => (session, expires_after) = (s, e),
                Err(()) => {
                    return realtime_error(400, "Invalid Realtime client secret request", "invalid_request_error", "invalid_request");
                }
            }
        }
        self.issue_client_secret(caller, &session, expires_after.as_ref(), false)
    }

    /// `CreateLegacySession`: deprecated `POST /v1/realtime/sessions`; the body is the session.
    pub fn create_legacy_session(&self, caller: &Caller, body: &[u8]) -> Reply {
        if body.len() > CLIENT_SECRET_MAX_BODY {
            return realtime_error(413, "Codex live request body too large", "invalid_request_error", "invalid_request");
        }
        self.issue_client_secret(caller, &String::from_utf8_lossy(body), None, true)
    }

    fn issue_client_secret(&self, caller: &Caller, session: &str, expires_after: Option<&ExpiresAfter>, legacy: bool) -> Reply {
        let lifetime = match client_secret_lifetime(expires_after) {
            Ok(l) => l,
            Err(e) => return realtime_error(400, &e, "invalid_request_error", "invalid_expires_after"),
        };
        let (client_session, upstream_session) = match normalize_client_secret_session(session) {
            Ok(v) => v,
            Err(SessionError::Unsupported(e)) => {
                return realtime_error(501, &e, "not_supported_error", "realtime_capability_not_supported");
            }
            Err(SessionError::Invalid(e)) => return realtime_error(400, &e, "invalid_request_error", "invalid_session"),
        };
        let (token, authorization, expires_at) =
            match self.inner.secrets.create(&upstream_session, lifetime, &caller.principal, &caller.provider) {
                Ok(v) => v,
                Err(CreateError::Capacity) => {
                    let mut reply = realtime_error(
                        429,
                        ERR_CAPACITY,
                        "rate_limit_error",
                        "realtime_client_secret_capacity_exhausted",
                    );
                    reply.set("retry-after", "1");
                    return reply;
                }
            };
        let Some(mut response) = realtime_session_response(&client_session, &authorization.principal, expires_at) else {
            return realtime_error(500, "Failed to encode Realtime session", "server_error", "realtime_session_failed");
        };
        let mut reply = if legacy {
            response.insert("client_secret".into(), json!({"value": token, "expires_at": unix_secs(expires_at)}));
            Reply::json(200, &Value::Object(response))
        } else {
            let session_text = go_json_sorted(&Value::Object(response), GoJsonStyle::MARSHAL_ANY).unwrap_or_else(|| "{}".into());
            Reply::json_text(
                200,
                format!(
                    r#"{{"value":{},"expires_at":{},"session":{}}}"#,
                    util::json_string(&token),
                    unix_secs(expires_at),
                    session_text
                ),
            )
        };
        reply.set("cache-control", "no-store");
        reply
    }
}
