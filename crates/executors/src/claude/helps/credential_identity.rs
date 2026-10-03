//! Claude credential identity: agent session UUID derivation, device pool bootstrap and the
//! `metadata.user_id` rewrite shared by native and cloaked OAuth requests (Go:
//! helps/claude_credential_identity.go). In Home mode the device pool is coordinated through Home
//! KV; the Go device pool mutex is not needed because callers hold `&mut Auth`.

use crate::helps::home_kv::client;
use cpa_auth::Auth;
use cpa_auth::claude::{
    DEVICE_IDS_METADATA_KEY, ensure_device_id_pool, generate_device_id_pool, normalize_device_id_pool,
};
use cpa_home::KvSetOptions;
use cpa_home::kv::hash_key_part;
use cpa_json::J;
use cpa_runtime::conductor::session::session_ids;
use cpa_runtime::executor::{ExecError, ErrorCode, Metadata, meta};
use http::HeaderMap;
use serde_json::Value;
use uuid::Uuid;

/// Go: `ClaudeAgentSessionUUID`: maps the downstream agent conversation to one stable UUID,
/// preserving native Claude Code session signals.
pub fn claude_agent_session_uuid(
    headers: &HeaderMap,
    original_payload: &[u8],
    translated_payload: &[u8],
    metadata_sets: &[&Metadata],
) -> String {
    let mut metadata = merge_claude_session_metadata(metadata_sets);
    let mut identity = session_ids(headers, original_payload, &mut metadata).0;
    if identity.is_empty() && !translated_payload.is_empty() {
        identity = session_ids(headers, translated_payload, &mut metadata).0;
    }
    if identity.is_empty() {
        return Uuid::new_v4().to_string();
    }
    if let Some(rest) = identity.strip_prefix("claude:")
        && let Ok(parsed) = Uuid::parse_str(rest)
    {
        return parsed.to_string();
    }
    if let Ok(parsed) = Uuid::parse_str(&identity) {
        return parsed.to_string();
    }
    let stable_input = format!("cli-proxy-api\0claude\0agent-conversation\0{identity}");
    Uuid::new_v5(&Uuid::NAMESPACE_OID, stable_input.as_bytes()).to_string()
}

/// Go: `ClaudeAgentSessionUUIDForRequest`: Claude-specific session signals (the session header and
/// `metadata.user_id`) only count for a confirmed native caller.
pub fn claude_agent_session_uuid_for_request(
    headers: &HeaderMap,
    original_payload: &[u8],
    translated_payload: &[u8],
    confirmed_claude_code: bool,
    metadata_sets: &[&Metadata],
) -> String {
    if confirmed_claude_code {
        return claude_agent_session_uuid(headers, original_payload, translated_payload, metadata_sets);
    }
    let mut headers = headers.clone();
    headers.remove("x-claude-code-session-id");
    let original = without_claude_metadata_user_id(original_payload);
    let translated = without_claude_metadata_user_id(translated_payload);
    claude_agent_session_uuid(&headers, &original, &translated, metadata_sets)
}

/// Go: `ClaudeRequestHasExecutionMetadata`: an explicit, non-blank execution session metadata key.
pub fn claude_request_has_execution_metadata(metadata_sets: &[&Metadata]) -> bool {
    metadata_sets.iter().any(|m| {
        m.get(meta::EXECUTION_SESSION_ID).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty())
    })
}

/// Go: `withoutClaudeMetadataUserID`; the payload is returned untouched when there is nothing to
/// delete or it is not valid JSON.
fn without_claude_metadata_user_id(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() || !crate::helps::parse_cache::valid(payload) {
        return payload.to_vec();
    }
    let mut root = cpa_json::parse(payload);
    if !root.g("metadata.user_id").exists() {
        return payload.to_vec();
    }
    cpa_json::delete(&mut root, "metadata.user_id");
    cpa_json::to_vec(&root)
}

/// Go: `mergeClaudeSessionMetadata`: first set wins per key.
fn merge_claude_session_metadata(metadata_sets: &[&Metadata]) -> Metadata {
    let mut merged = Metadata::new();
    for metadata in metadata_sets {
        for (key, value) in metadata.iter() {
            merged.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    merged
}

/// Go: `claudeauth.HasCanonicalDeviceIDPool`: exactly one stored string that is already in
/// canonical (trimmed, lowercase, valid) form.
pub(crate) fn has_canonical_device_id_pool(raw: Option<&Value>) -> bool {
    let Some(Value::Array(items)) = raw else { return false };
    let [Value::String(only)] = items.as_slice() else { return false };
    let normalized = normalize_device_id_pool(raw);
    normalized.len() == 1 && *only == normalized[0]
}

/// Go: `claudeauth.SelectDeviceID`: the credential's sole device id, validating the session id.
fn select_device_id(device_ids: &[String], session_id: &str) -> Result<String, String> {
    let normalized = normalize_device_id_pool(Some(&Value::Array(
        device_ids.iter().cloned().map(Value::String).collect(),
    )));
    if normalized.len() != 1 {
        return Err(format!(
            "select Claude device ID: device pool has {} entries, want 1",
            normalized.len()
        ));
    }
    if session_id.trim().is_empty() {
        return Err("select Claude device ID: session ID is empty".to_string());
    }
    Ok(normalized[0].clone())
}

/// Go: `EnsureClaudeCredentialDevicePoolRequired`: returns the credential's canonical
/// single-device pool, creating or repairing it in `auth.metadata` when needed. An already
/// canonical pool is returned as is. Otherwise the pool is built locally, or, in Home mode,
/// coordinated through Home KV (`cpa:claude:credential-device-pool:<hash of the credential
/// identity>`) so every node of a remote dispatch clone agrees on one device id.
pub async fn ensure_claude_credential_device_pool_required(auth: &mut Auth) -> Result<Vec<String>, ExecError> {
    const PREFIX: &str = "ensure Claude credential device pool";
    let fail = |detail: String| ExecError::new(0, format!("{PREFIX}: {detail}"));

    let raw = auth.metadata.get(DEVICE_IDS_METADATA_KEY);
    if has_canonical_device_id_pool(raw) {
        return Ok(normalize_device_id_pool(raw));
    }
    let candidate = normalize_device_id_pool(raw);

    let client = match client() {
        Ok(None) => return Ok(ensure_device_id_pool(&mut auth.metadata).0),
        Ok(Some(client)) => client,
        Err(e) => return Err(fail(format!("Home KV client: {e}"))),
    };
    let mut identity = auth.ensure_index().trim().to_string();
    if identity.is_empty() {
        identity = auth.id.trim().to_string();
    }
    if identity.is_empty() {
        return Err(fail("credential identity is empty".into()));
    }
    let key = format!("cpa:claude:credential-device-pool:{}", hash_key_part(&identity));
    let store = |auth: &mut Auth, ids: &[String]| {
        auth.metadata.insert(
            DEVICE_IDS_METADATA_KEY.into(),
            Value::Array(ids.iter().cloned().map(Value::String).collect()),
        );
    };

    match client.kv_get(&key).await {
        Err(e) => return Err(fail(format!("Home KV get: {e}"))),
        Ok(Some(raw)) => {
            if let Some(stored) = decode_device_pool(&raw) {
                let device_ids = normalize_device_id_pool(Some(&stored));
                if device_ids.len() == DEVICE_POOL_SIZE {
                    if !has_canonical_device_id_pool(Some(&stored)) {
                        let canonical = serde_json::to_vec(&device_ids)
                            .map_err(|e| fail(format!("marshal canonical Home KV value: {e}")))?;
                        let opts = KvSetOptions { xx: true, ..Default::default() };
                        match client.kv_set(&key, &canonical, opts).await {
                            Err(e) => return Err(fail(format!("canonicalize Home KV value: {e}"))),
                            Ok(false) => return Err(fail("canonical Home KV value was not written".into())),
                            Ok(true) => {}
                        }
                    }
                    store(auth, &device_ids);
                    return Ok(device_ids);
                }
            }
        }
        Ok(None) => {}
    }

    let device_ids = if candidate.len() == DEVICE_POOL_SIZE { candidate } else { generate_device_id_pool() };
    let raw = serde_json::to_vec(&device_ids).map_err(|e| fail(format!("marshal Home KV value: {e}")))?;
    let opts = KvSetOptions { nx: true, ..Default::default() };
    if let Err(e) = client.kv_set(&key, &raw, opts).await {
        return Err(fail(format!("Home KV set: {e}")));
    }
    let raw = match client.kv_get(&key).await {
        Err(e) => return Err(fail(format!("Home KV reread: {e}"))),
        Ok(None) => return Err(fail("Home KV value missing after set".into())),
        Ok(Some(raw)) => raw,
    };
    let Some(stored) = decode_device_pool(&raw) else {
        return Err(fail("decode Home KV value: not a JSON array of strings".into()));
    };
    let device_ids = normalize_device_id_pool(Some(&stored));
    if device_ids.len() != DEVICE_POOL_SIZE {
        return Err(fail(format!("Home KV pool has {} entries, want {DEVICE_POOL_SIZE}", device_ids.len())));
    }
    store(auth, &device_ids);
    Ok(device_ids)
}

/// Size of a Claude credential's device pool (Go: `claudeauth.ClaudeDevicePoolSize`).
const DEVICE_POOL_SIZE: usize = 1;

/// A stored pool as a JSON array of strings (Go decodes into `[]string`: `null` reads as an empty
/// pool and `null` elements as empty strings); `None` when it does not decode.
fn decode_device_pool(raw: &[u8]) -> Option<Value> {
    let items: Option<Vec<Option<String>>> = serde_json::from_slice(raw).ok()?;
    let items = items.unwrap_or_default();
    Some(Value::Array(items.into_iter().map(|s| Value::String(s.unwrap_or_default())).collect()))
}

/// Go: `ClaudeCredentialAccountUUID`: `account_uuid` (or legacy `accountUuid`) of the credential.
pub fn claude_credential_account_uuid(auth: &Auth) -> String {
    for key in ["account_uuid", "accountUuid"] {
        if let Some(Value::String(value)) = auth.metadata.get(key) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// Failure of [`apply_claude_credential_metadata`]. `RequestScoped` (HTTP 400) is caused by the
/// caller's body; `Credential` is a credential-side problem (Go's plain errors).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClaudeCredentialMetadataError {
    #[error("{0}")]
    RequestScoped(String),
    #[error("{0}")]
    Credential(String),
}

impl ClaudeCredentialMetadataError {
    /// Go: `IsRequestScoped`.
    pub fn is_request_scoped(&self) -> bool {
        matches!(self, Self::RequestScoped(_))
    }

    /// Go: `StatusCode` (400 for request-scoped errors, 0 otherwise).
    pub fn status_code(&self) -> u16 {
        if self.is_request_scoped() { 400 } else { 0 }
    }

    /// Converts to the executor error type; request-scoped errors carry
    /// [`ErrorCode::RequestScoped`] and status 400.
    pub fn into_exec_error(self) -> ExecError {
        match self {
            Self::RequestScoped(msg) => ExecError::new(400, msg).with_code(ErrorCode::RequestScoped),
            Self::Credential(msg) => {
                let mut err = ExecError::new(0, msg);
                err.upstream_attempted = false;
                err
            }
        }
    }
}

/// Go: `ApplyClaudeCredentialMetadata`: rewrites `metadata.user_id` to
/// `{"device_id","account_uuid","session_id"}` from the credential (plus any extra keys of an
/// existing user_id object) and returns the updated body with the selected device id.
pub fn apply_claude_credential_metadata(
    payload: &[u8],
    auth: &mut Auth,
    session_id: &str,
) -> Result<(Vec<u8>, String), ClaudeCredentialMetadataError> {
    use ClaudeCredentialMetadataError::{Credential, RequestScoped};
    let request_err = |prefix: &str, e: String| RequestScoped(format!("apply Claude credential metadata: {prefix}{e}"));

    let root = go_trim_space(payload);
    let top = scan_claude_json_object(root, "metadata").map_err(|e| request_err("", e))?;
    let mut existing = String::new();
    let mut metadata_scan: Option<ObjectScan> = None;
    if let Some((start, end)) = top.member {
        let metadata = &root[start..end];
        if metadata.len() >= 2 && metadata[0] == b'{' {
            let scan = scan_claude_json_object(metadata, "user_id").map_err(|e| request_err("metadata: ", e))?;
            if let Some((us, ue)) = scan.member
                && let Value::String(s) = cpa_json::parse(&metadata[us..ue])
            {
                existing = s;
            }
            metadata_scan = Some(scan);
        }
    }

    let (device_ids, _) = ensure_device_id_pool(&mut auth.metadata);
    let device_id = select_device_id(&device_ids, session_id).map_err(Credential)?;
    let account_uuid = claude_credential_account_uuid(auth);
    if account_uuid.is_empty() {
        return Err(Credential("apply Claude credential metadata: account UUID is empty".to_string()));
    }

    let encoded = rebuild_claude_metadata_user_id(&existing, &device_id, &account_uuid, session_id)
        .map_err(|e| request_err("", e))?;
    let mut user_id_json = Vec::with_capacity(encoded.len() + 16);
    write_claude_json_quoted(&mut user_id_json, &String::from_utf8_lossy(&encoded));
    let updated = set_metadata_user_id(root, &top, metadata_scan.as_ref(), &user_id_json)
        .map_err(|e| Credential(format!("set Claude credential metadata: {e}")))?;
    Ok((updated, device_id))
}

/// Byte-level `sjson.SetBytes(payload, "metadata.user_id", <string>)` over a trimmed, valid
/// object: replaces an existing value in place, otherwise inserts a member just before the
/// closing brace (with a comma unless the object is empty), leaving every other byte intact.
fn set_metadata_user_id(
    root: &[u8],
    top: &ObjectScan,
    metadata_scan: Option<&ObjectScan>,
    user_id_json: &[u8],
) -> Result<Vec<u8>, String> {
    let splice = |range: std::ops::Range<usize>, replacement: &[u8]| {
        let mut out = Vec::with_capacity(root.len() + replacement.len());
        out.extend_from_slice(&root[..range.start]);
        out.extend_from_slice(replacement);
        out.extend_from_slice(&root[range.end..]);
        out
    };
    let member = |scan: &ObjectScan, key: &str, value: &[u8]| {
        let mut m = Vec::with_capacity(value.len() + key.len() + 4);
        if !scan.empty {
            m.push(b',');
        }
        m.push(b'"');
        m.extend_from_slice(key.as_bytes());
        m.extend_from_slice(b"\":");
        m.extend_from_slice(value);
        m
    };
    let wrapped_user_id = || [b"{\"user_id\":".as_slice(), user_id_json, b"}"].concat();

    let Some((start, end)) = top.member else {
        let m = member(top, "metadata", &wrapped_user_id());
        return Ok(splice(top.close..top.close, &m));
    };
    match metadata_scan {
        Some(scan) => match scan.member {
            Some((us, ue)) => Ok(splice(start + us..start + ue, user_id_json)),
            None => {
                let at = start + scan.close;
                Ok(splice(at..at, &member(scan, "user_id", user_id_json)))
            }
        },
        None if root[start] == b'[' => Err("cannot set array element for non-numeric key 'user_id'".to_string()),
        None => Ok(splice(start..end, &wrapped_user_id())),
    }
}

/// Go's `bytes.TrimSpace` (Unicode white space; ASCII only when the input is not valid UTF-8).
fn go_trim_space(raw: &[u8]) -> &[u8] {
    match std::str::from_utf8(raw) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => {
            let start = raw.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(raw.len());
            let end = raw.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
            &raw[start..end]
        }
    }
}

/// Where a member sits inside a scanned top-level object (offsets into the scanned slice).
pub(crate) struct ObjectScan {
    /// Start and end of the raw value of the target member, if present.
    member: Option<(usize, usize)>,
    /// Index of the closing brace.
    close: usize,
    /// True when the object has no members.
    empty: bool,
}

/// Go: `uniqueClaudeJSONObjectMember` (offsets form): locates top-level member `target` of a valid
/// JSON object `raw` (already trimmed). A duplicated `target` key is an error (other keys may
/// repeat).
fn scan_claude_json_object(raw: &[u8], target: &str) -> Result<ObjectScan, String> {
    if !go_json_valid(raw) || raw.len() < 2 || raw[0] != b'{' {
        return Err("request must be a JSON object".to_string());
    }

    let mut position = 1;
    let mut value: Option<(usize, usize)> = None;
    let mut empty = true;
    loop {
        position = skip_claude_json_whitespace(raw, position);
        if position >= raw.len() {
            return Err("unterminated JSON object".to_string());
        }
        if raw[position] == b'}' {
            break;
        }
        empty = false;
        let key_start = position;
        let key_end = skip_claude_json_string(raw, key_start);
        let Value::String(key) = cpa_json::parse(&raw[key_start..key_end]) else {
            return Err("decode JSON object key: not a string".to_string());
        };
        position = skip_claude_json_whitespace(raw, key_end);
        if position >= raw.len() || raw[position] != b':' {
            return Err(format!("JSON object key {key:?} is missing a value"));
        }
        position = skip_claude_json_whitespace(raw, position + 1);
        let value_start = position;
        position = skip_claude_json_value(raw, position);
        if key == target {
            if value.is_some() {
                return Err(format!("duplicate JSON object key {target:?}"));
            }
            value = Some((value_start, position));
        }
        position = skip_claude_json_whitespace(raw, position);
        if position < raw.len() && raw[position] == b',' {
            position += 1;
            continue;
        }
        if position >= raw.len() || raw[position] != b'}' {
            return Err(format!("JSON object key {key:?} has an invalid terminator"));
        }
    }
    Ok(ObjectScan { member: value, close: position, empty })
}

/// Go: `encoding/json.Valid`: strict JSON grammar, but (unlike serde_json) lone surrogate escapes
/// and invalid UTF-8 inside strings are accepted. Nesting is limited to 10000 like Go.
pub(crate) fn go_json_valid(b: &[u8]) -> bool {
    fn ws(b: &[u8], mut i: usize) -> usize {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        i
    }
    // Index just past the string starting at `i`.
    fn string(b: &[u8], mut i: usize) -> Option<usize> {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        loop {
            match *b.get(i)? {
                b'"' => return Some(i + 1),
                b'\\' => match *b.get(i + 1)? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                    b'u' => {
                        if !b.get(i + 2..i + 6)?.iter().all(u8::is_ascii_hexdigit) {
                            return None;
                        }
                        i += 6;
                    }
                    _ => return None,
                },
                c if c < 0x20 => return None,
                _ => i += 1,
            }
        }
    }
    // Index just past the scalar (number or literal) starting at `i`.
    fn scalar(b: &[u8], mut i: usize) -> Option<usize> {
        for lit in [&b"true"[..], b"false", b"null"] {
            if b[i..].starts_with(lit) {
                return Some(i + lit.len());
            }
        }
        let digits = |mut j: usize| {
            let start = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            (j > start).then_some(j)
        };
        if b.get(i) == Some(&b'-') {
            i += 1;
        }
        match b.get(i)? {
            b'0' => i += 1,
            b'1'..=b'9' => i = digits(i)?,
            _ => return None,
        }
        if b.get(i) == Some(&b'.') {
            i = digits(i + 1)?;
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            i = digits(i)?;
        }
        Some(i)
    }

    const MAX_DEPTH: usize = 10_000;
    let mut stack: Vec<u8> = Vec::new();
    let mut i = ws(b, 0);
    'value: loop {
        // Parse one value starting at `i`; containers push and, unless empty, parse the first
        // child next.
        match b.get(i) {
            Some(b'{') => {
                stack.push(b'{');
                if stack.len() > MAX_DEPTH {
                    return false;
                }
                i = ws(b, i + 1);
                if b.get(i) == Some(&b'}') {
                    stack.pop();
                    i += 1;
                } else {
                    let Some(end) = string(b, i) else { return false };
                    i = ws(b, end);
                    if b.get(i) != Some(&b':') {
                        return false;
                    }
                    i = ws(b, i + 1);
                    continue 'value;
                }
            }
            Some(b'[') => {
                stack.push(b'[');
                if stack.len() > MAX_DEPTH {
                    return false;
                }
                i = ws(b, i + 1);
                if b.get(i) == Some(&b']') {
                    stack.pop();
                    i += 1;
                } else {
                    continue 'value;
                }
            }
            Some(b'"') => match string(b, i) {
                Some(end) => i = end,
                None => return false,
            },
            Some(_) => match scalar(b, i) {
                Some(end) => i = end,
                None => return false,
            },
            None => return false,
        }
        // A value just ended: close containers or move to the next sibling.
        loop {
            i = ws(b, i);
            match (stack.last(), b.get(i)) {
                (None, _) => return i == b.len(),
                (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']')) => {
                    stack.pop();
                    i += 1;
                }
                (Some(b'{'), Some(b',')) => {
                    i = ws(b, i + 1);
                    let Some(end) = string(b, i) else { return false };
                    i = ws(b, end);
                    if b.get(i) != Some(&b':') {
                        return false;
                    }
                    i = ws(b, i + 1);
                    continue 'value;
                }
                (Some(b'['), Some(b',')) => {
                    i = ws(b, i + 1);
                    continue 'value;
                }
                _ => return false,
            }
        }
    }
}

/// Go: `skipClaudeJSONWhitespace`.
pub(crate) fn skip_claude_json_whitespace(raw: &[u8], mut position: usize) -> usize {
    while position < raw.len() && matches!(raw[position], b' ' | b'\t' | b'\r' | b'\n') {
        position += 1;
    }
    position
}

/// Go: `skipClaudeJSONString`: index just past the string starting at `position`.
fn skip_claude_json_string(raw: &[u8], mut position: usize) -> usize {
    if position >= raw.len() || raw[position] != b'"' {
        return position;
    }
    position += 1;
    while position < raw.len() {
        match raw[position] {
            b'\\' => position += 2,
            b'"' => return position + 1,
            _ => position += 1,
        }
    }
    position
}

/// Go: `skipClaudeJSONValue`: index just past the JSON value starting at `position`.
pub(crate) fn skip_claude_json_value(raw: &[u8], mut position: usize) -> usize {
    if position >= raw.len() {
        return position;
    }
    match raw[position] {
        b'"' => skip_claude_json_string(raw, position),
        b'{' | b'[' => {
            let mut depth = 1usize;
            position += 1;
            while position < raw.len() && depth > 0 {
                match raw[position] {
                    b'"' => {
                        position = skip_claude_json_string(raw, position);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    _ => {}
                }
                position += 1;
            }
            position
        }
        _ => {
            while position < raw.len()
                && !matches!(raw[position], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            {
                position += 1;
            }
            position
        }
    }
}

/// Go: `rebuildClaudeMetadataUserID`: the identity JSON text with the credential fields first
/// and the extra keys of an existing object user_id appended verbatim (duplicates rejected).
fn rebuild_claude_metadata_user_id(
    existing: &str,
    device_id: &str,
    account_uuid: &str,
    session_id: &str,
) -> Result<Vec<u8>, String> {
    let raw_existing = existing.trim().as_bytes();
    let mut extras: Vec<(String, &[u8])> = Vec::new();
    if raw_existing.len() >= 2 && raw_existing[0] == b'{' && go_json_valid(raw_existing) {
        let mut position = 1;
        let mut seen: Vec<String> = Vec::new();
        loop {
            position = skip_claude_json_whitespace(raw_existing, position);
            if position >= raw_existing.len() || raw_existing[position] == b'}' {
                break;
            }
            let key_end = skip_claude_json_string(raw_existing, position);
            let Value::String(key) = cpa_json::parse(&raw_existing[position..key_end]) else {
                return Err("metadata.user_id contains a non-string key".to_string());
            };
            if seen.contains(&key) {
                return Err(format!("metadata.user_id contains duplicate key {key:?}"));
            }
            position = skip_claude_json_whitespace(raw_existing, key_end);
            position = skip_claude_json_whitespace(raw_existing, position + 1);
            let value_end = skip_claude_json_value(raw_existing, position);
            let value = &raw_existing[position..value_end];
            position = skip_claude_json_whitespace(raw_existing, value_end);
            if raw_existing.get(position) == Some(&b',') {
                position += 1;
            }
            seen.push(key.clone());
            if !matches!(key.as_str(), "device_id" | "account_uuid" | "session_id") {
                extras.push((key, value));
            }
        }
    }

    let mut output: Vec<u8> = Vec::with_capacity(128);
    output.extend_from_slice(br#"{"device_id":"#);
    write_claude_json_quoted(&mut output, device_id);
    output.extend_from_slice(br#","account_uuid":"#);
    write_claude_json_quoted(&mut output, account_uuid);
    output.extend_from_slice(br#","session_id":"#);
    write_claude_json_quoted(&mut output, session_id);
    for (key, value) in extras {
        output.push(b',');
        write_claude_json_quoted(&mut output, &key);
        output.push(b':');
        output.extend_from_slice(value);
    }
    output.push(b'}');
    Ok(output)
}

/// How sjson writes a string value: plain `"..."` unless it holds a control char, a non-ASCII
/// byte, `"` or `\`, in which case it is `json.Marshal`ed (HTML-escaped).
pub(crate) fn sjson_string(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    if value.bytes().any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\') {
        write_claude_json_quoted(&mut out, value);
    } else {
        out.push(b'"');
        out.extend_from_slice(value.as_bytes());
        out.push(b'"');
    }
    out
}

/// Go: `writeClaudeJSONQuoted` (`json.Marshal` of a string: HTML-escapes `<`, `>`, `&` and
/// escapes U+2028/U+2029).
fn write_claude_json_quoted(output: &mut Vec<u8>, value: &str) {
    output.push(b'"');
    for ch in value.chars() {
        match ch {
            '"' => output.extend_from_slice(b"\\\""),
            '\\' => output.extend_from_slice(b"\\\\"),
            '\u{8}' => output.extend_from_slice(b"\\b"),
            '\u{c}' => output.extend_from_slice(b"\\f"),
            '\n' => output.extend_from_slice(b"\\n"),
            '\r' => output.extend_from_slice(b"\\r"),
            '\t' => output.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                output.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                output.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    output.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_auth::claude::DEVICE_IDS_METADATA_KEY;
    use serde_json::json;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";
    const ACCOUNT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const DEVICE0: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn auth_with(account: Option<&str>, device: Option<&str>) -> Auth {
        let mut auth = Auth::new("cred", "claude");
        if let Some(a) = account {
            auth.metadata.insert("account_uuid".into(), json!(a));
        }
        if let Some(d) = device {
            auth.metadata.insert(DEVICE_IDS_METADATA_KEY.into(), json!([d]));
        }
        auth
    }

    fn headers_with_session(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", value.parse().expect("header value"));
        h
    }

    fn metadata(key: &str, value: &str) -> Metadata {
        Metadata::from([(key.to_string(), json!(value))])
    }

    #[test]
    fn session_uuid_preserves_native_session_only_when_confirmed() {
        let h = headers_with_session(SESSION);
        assert_eq!(claude_agent_session_uuid_for_request(&h, b"", b"", true, &[]), SESSION);

        let md = metadata(meta::EXECUTION_SESSION_ID, "non-native-conversation");
        let payload = format!(
            r#"{{"metadata":{{"user_id":"{{\"device_id\":\"{DEVICE0}\",\"session_id\":\"{SESSION}\"}}"}}}}"#
        );
        let got = claude_agent_session_uuid_for_request(&h, payload.as_bytes(), b"", false, &[&md]);
        assert_ne!(got, SESSION);
        let repeated = claude_agent_session_uuid_for_request(&HeaderMap::new(), b"", b"", false, &[&md]);
        assert_eq!(repeated, got);
    }

    #[test]
    fn session_uuid_is_stable_for_execution_and_derived_identity() {
        for md in [
            metadata(meta::EXECUTION_SESSION_ID, "agent-run-1"),
            metadata(meta::DERIVED_SESSION_ID, "ctx:v1:conversation-root"),
        ] {
            let first = claude_agent_session_uuid(&HeaderMap::new(), b"", b"", &[&md]);
            let second = claude_agent_session_uuid(&HeaderMap::new(), b"", b"", &[&md]);
            assert!(!first.is_empty());
            assert_eq!(first, second);
        }
        assert!(claude_request_has_execution_metadata(&[&metadata(meta::EXECUTION_SESSION_ID, "x")]));
        assert!(!claude_request_has_execution_metadata(&[&metadata(meta::EXECUTION_SESSION_ID, "  ")]));
    }

    #[test]
    fn apply_uses_credential_identity_and_preserves_extras() {
        let mut auth = auth_with(Some(ACCOUNT), Some(DEVICE0));
        let body = br#"{"messages":[{"role":"user","content":"x"}],"metadata":{"user_id":"{\"device_id\":\"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\",\"account_uuid\":\"downstream-account\",\"session_id\":\"downstream-session\",\"parent_session_id\":\"parent-1\",\"extra\":true}"}}"#;
        let (updated, device) = apply_claude_credential_metadata(body, &mut auth, SESSION).expect("apply");
        assert_eq!(device, DEVICE0);
        let user_id = cpa_json::parse(&updated).g("metadata.user_id").str();
        assert_eq!(
            user_id,
            format!(
                r#"{{"device_id":"{DEVICE0}","account_uuid":"{ACCOUNT}","session_id":"{SESSION}","parent_session_id":"parent-1","extra":true}}"#
            )
        );
    }

    #[test]
    fn apply_rejects_duplicate_identity_containers_as_request_scoped() {
        let mut auth = auth_with(Some(ACCOUNT), Some(DEVICE0));
        for body in [
            r#"{"messages":[],"metadata":"#,
            r#"{"messages":[],"metadata":{"user_id":"{}"},"metadata":{"user_id":"{}"}}"#,
            r#"{"messages":[],"metadata":{"user_id":"{}","user_id":"{}"}}"#,
            r#"{"messages":[],"metadata":{"user_id":"{\"account_uuid\":\"first\",\"account_uuid\":\"last\"}"}}"#,
        ] {
            let err = apply_claude_credential_metadata(body.as_bytes(), &mut auth, SESSION).expect_err(body);
            assert!(err.is_request_scoped(), "{body}: {err}");
            assert_eq!(err.status_code(), 400);
        }
    }

    #[test]
    fn apply_requires_account_uuid_as_credential_error() {
        let mut auth = auth_with(None, Some(DEVICE0));
        let err = apply_claude_credential_metadata(br#"{"messages":[]}"#, &mut auth, SESSION).expect_err("no account");
        assert!(!err.is_request_scoped());
    }

    /// Expected bytes recorded from the Go implementation (`~` stands for a backslash, `ID` for the
    /// double-encoded identity string, `EX` for its extras).
    #[test]
    fn apply_output_bytes_match_go() {
        let id = |extras: &str| {
            format!(
                r#""{{~"device_id~":~"{DEVICE0}~",~"account_uuid~":~"{ACCOUNT}~",~"session_id~":~"{SESSION}~"{extras}}}""#
            )
        };
        let plain = id("");
        let cases: Vec<(&str, String)> = vec![
            (r#"{"messages":[]}"#, format!(r#"{{"messages":[],"metadata":{{"user_id":{plain}}}}}"#)),
            (r#"{"metadata":{"a":1}}"#, format!(r#"{{"metadata":{{"a":1,"user_id":{plain}}}}}"#)),
            (r#"{"metadata":"x"}"#, format!(r#"{{"metadata":{{"user_id":{plain}}}}}"#)),
            ("{ }", format!(r#"{{ "metadata":{{"user_id":{plain}}}}}"#)),
            (r#"{"a":1 }"#, format!(r#"{{"a":1 ,"metadata":{{"user_id":{plain}}}}}"#)),
            (r#"  {"a":1}  "#, format!(r#"{{"a":1,"metadata":{{"user_id":{plain}}}}}"#)),
            (r#"{"metadata":{ }}"#, format!(r#"{{"metadata":{{ "user_id":{plain}}}}}"#)),
            (
                r#"{"metadata":{"a":1,"user_id":"x"  ,"b":2}}"#,
                format!(r#"{{"metadata":{{"a":1,"user_id":{plain}  ,"b":2}}}}"#),
            ),
            (r#"{"metadata":  "x"  ,"z":1}"#, format!(r#"{{"metadata":  {{"user_id":{plain}}}  ,"z":1}}"#)),
            (r#"{"z":{"metadata":{"user_id":"q"}}}"#, format!(r#"{{"z":{{"metadata":{{"user_id":"q"}}}},"metadata":{{"user_id":{plain}}}}}"#)),
            (
                "{\n  \"model\": \"m\",\n  \"metadata\" : { \"user_id\" : \"{}\" } ,\n  \"n\": 1.50\n}",
                format!("{{\n  \"model\": \"m\",\n  \"metadata\" : {{ \"user_id\" : {plain} }} ,\n  \"n\": 1.50\n}}"),
            ),
            (
                r#"{"metadata":{"user_id":"{ ~"extra~" : {~"a~": [1, 2 ] } , ~"x~":~"<&>~" }"}}"#,
                format!(
                    r#"{{"metadata":{{"user_id":{}}}}}"#,
                    id(r#",~"extra~":{~"a~": [1, 2 ] },~"x~":~"~u003c~u0026~u003e~""#)
                ),
            ),
            (
                // Lone surrogate escapes are valid for Go's encoding/json; extras are kept raw.
                r#"{"metadata":{"user_id":"{~"a~":~"~~ud800~"}"}}"#,
                format!(r#"{{"metadata":{{"user_id":{}}}}}"#, id(r#",~"a~":~"~~ud800~""#)),
            ),
        ];
        for (input, expected) in cases {
            let (input, expected) = (input.replace('~', "\\"), expected.replace('~', "\\"));
            let mut auth = auth_with(Some(ACCOUNT), Some(DEVICE0));
            let (updated, _) = apply_claude_credential_metadata(input.as_bytes(), &mut auth, SESSION).expect(&input);
            assert_eq!(String::from_utf8_lossy(&updated), expected, "input: {input}");
        }

        // An array `metadata` cannot take a named member (credential-scoped error in Go).
        let mut auth = auth_with(Some(ACCOUNT), Some(DEVICE0));
        let err = apply_claude_credential_metadata(br#"{"metadata":[1]}"#, &mut auth, SESSION).expect_err("array");
        assert!(!err.is_request_scoped());
        assert_eq!(
            err.to_string(),
            "set Claude credential metadata: cannot set array element for non-numeric key 'user_id'"
        );
    }

    #[test]
    fn go_json_valid_matches_encoding_json() {
        for ok in [
            "{}",
            " [1, -2.5e+3, true, false, null, \"x\", {\"a\":[]}] ",
            "\"plain\"",
            r#"{"a":"~ud800"}"#,
        ] {
            assert!(go_json_valid(ok.replace('~', "\\").as_bytes()), "{ok}");
        }
        for bad in ["", "{", "{\"a\":1,}", "[1,]", "01", "-", "1.", "tru", "{\"a\" 1}", "\"a\nb\"", "{} x", "[\"\\x\"]", "{\"a\":1}}"] {
            assert!(!go_json_valid(bad.as_bytes()), "{bad:?}");
        }
        assert!(!go_json_valid("[".repeat(10_001).as_bytes()));
    }

    #[tokio::test]
    async fn device_pool_bootstrap_is_stable() {
        let mut auth = Auth::new("shared", "claude");
        let first = ensure_claude_credential_device_pool_required(&mut auth).await.expect("pool");
        assert_eq!(first.len(), 1);
        assert_eq!(ensure_claude_credential_device_pool_required(&mut auth).await.expect("pool"), first);
        assert!(has_canonical_device_id_pool(auth.metadata.get(DEVICE_IDS_METADATA_KEY)));
    }
}
