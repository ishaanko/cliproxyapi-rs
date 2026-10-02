//! Credential metadata helpers (sdk/cliproxy/auth: metadata_keys.go, weight.go, priority.go,
//! custom_headers.go, metadata_merge.go and internal/credentialweight).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::types::Auth;

pub type Metadata = Map<String, Value>;

pub const ATTRIBUTE_WEIGHT: &str = "weight";
pub const ATTRIBUTE_FILE_PRIORITY: &str = "file_priority";

/// Legacy hyphenated key to canonical snake_case key.
pub fn canonical_credential_metadata_key(key: &str) -> &str {
    match key {
        "api-key" => "api_key",
        "base-url" => "base_url",
        "disable-cooling" => "disable_cooling",
        "excluded-models" => "excluded_models",
        "fingerprint-profile" => "fingerprint_profile",
        "model-aliases" => "model_aliases",
        "proxy-url" => "proxy_url",
        "request-retry" => "request_retry",
        "request-scoped-errors" => "request_scoped_errors",
        "tool-prefix-disabled" => "tool_prefix_disabled",
        other => other,
    }
}

/// Rewrites legacy hyphenated keys to snake_case in place; the canonical key wins when both exist.
pub fn normalize_credential_metadata(metadata: &mut Metadata) {
    let legacy: Vec<String> = metadata
        .keys()
        .filter(|k| canonical_credential_metadata_key(k) != k.as_str())
        .cloned()
        .collect();
    for key in legacy {
        let canonical = canonical_credential_metadata_key(&key).to_string();
        if let Some(value) = metadata.shift_remove(&key)
            && !metadata.contains_key(&canonical)
        {
            metadata.insert(canonical, value);
        }
    }
}

/// Sets `key` only when `value` is non-empty (the common "never erase" write-back rule).
pub fn set_if_nonempty(meta: &mut Metadata, key: &str, value: &str) {
    if !value.is_empty() {
        meta.insert(key.to_string(), Value::String(value.to_string()));
    }
}

// ---- Weight (internal/credentialweight) ----

pub const WEIGHT_DEFAULT: i64 = 1;
pub const WEIGHT_MAX: i64 = 1_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WeightError {
    #[error("weight must not exceed {WEIGHT_MAX}")]
    TooLarge,
    #[error("weight must be an integer")]
    NotInteger,
}

fn normalize_weight(w: i64) -> Result<i64, WeightError> {
    if w <= 0 {
        return Ok(0);
    }
    if w > WEIGHT_MAX {
        return Err(WeightError::TooLarge);
    }
    Ok(w)
}

/// Parses a scheduler attribute; empty means the default weight.
pub fn parse_weight_string(raw: &str) -> Result<i64, WeightError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(WEIGHT_DEFAULT);
    }
    let w = raw.parse::<i64>().map_err(|_| WeightError::NotInteger)?;
    normalize_weight(w)
}

/// Parses a JSON metadata weight value (number or numeric string).
pub fn parse_weight_value(value: &Value) -> Result<i64, WeightError> {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return normalize_weight(i);
            }
            if let Some(u) = n.as_u64() {
                return if u > WEIGHT_MAX as u64 {
                    Err(WeightError::TooLarge)
                } else {
                    Ok(u as i64)
                };
            }
            let f = n.as_f64().ok_or(WeightError::NotInteger)?;
            if !f.is_finite() || f.trunc() != f {
                return Err(WeightError::NotInteger);
            }
            if f <= 0.0 {
                return Ok(0);
            }
            if f > WEIGHT_MAX as f64 {
                return Err(WeightError::TooLarge);
            }
            Ok(f as i64)
        }
        Value::String(s) => parse_weight_string(s),
        _ => Err(WeightError::NotInteger),
    }
}

/// Validates `attributes.weight` and `metadata.weight`.
pub fn validate_auth_weight(auth: &Auth) -> Result<(), String> {
    if let Some(raw) = auth.attributes.get(ATTRIBUTE_WEIGHT) {
        parse_weight_string(raw).map_err(|e| format!("invalid attributes weight: {e}"))?;
    }
    validate_metadata_weight(&auth.metadata)
}

/// Validates only the `weight` field of a metadata map (used before an Auth exists).
pub fn validate_metadata_weight(metadata: &Metadata) -> Result<(), String> {
    if let Some(raw) = metadata.get(ATTRIBUTE_WEIGHT) {
        parse_weight_value(raw).map_err(|e| format!("invalid metadata weight: {e}"))?;
    }
    Ok(())
}

/// `ApplyAuthWeightMetadata`: validates and mirrors `metadata.weight` into `attributes.weight`.
pub fn apply_auth_weight_metadata(auth: &mut Auth, metadata: &Metadata) -> Result<(), String> {
    validate_auth_weight(auth)?;
    let Some(raw) = metadata.get(ATTRIBUTE_WEIGHT) else {
        return Ok(());
    };
    let w = parse_weight_value(raw).map_err(|e| format!("invalid metadata weight: {e}"))?;
    auth.attributes
        .insert(ATTRIBUTE_WEIGHT.to_string(), w.to_string());
    Ok(())
}

// ---- Priority ----

/// `ApplyAuthPriorityMetadata`: numeric or integer-string `priority` becomes `attributes.priority`
/// plus `file_priority="true"`.
pub fn apply_auth_priority_metadata(auth: &mut Auth, metadata: &Metadata) {
    auth.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
    let Some(raw) = metadata.get("priority") else {
        return;
    };
    let priority = match raw {
        Value::Number(n) => {
            // Go decodes JSON numbers as float64 and truncates through int().
            let f = n.as_f64().unwrap_or(0.0);
            (f as i64).to_string()
        }
        Value::String(s) => {
            let t = s.trim().to_string();
            if t.parse::<i64>().is_err() {
                return;
            }
            t
        }
        _ => return,
    };
    auth.metadata.insert("priority".to_string(), raw.clone());
    auth.attributes.insert("priority".to_string(), priority);
    auth.attributes
        .insert(ATTRIBUTE_FILE_PRIORITY.to_string(), "true".to_string());
}

// ---- Custom headers ----

/// `headers{}` of a credential file with blank names/values dropped.
pub fn extract_custom_headers(metadata: &Metadata) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(Value::Object(headers)) = metadata.get("headers") else {
        return out;
    };
    for (key, value) in headers {
        let name = key.trim();
        if name.is_empty() {
            continue;
        }
        let Some(raw) = value.as_str() else { continue };
        let val = raw.trim();
        if val.is_empty() {
            continue;
        }
        out.insert(name.to_string(), val.to_string());
    }
    out
}

/// Copies `headers{}` into `attributes["header:<Name>"]`.
pub fn apply_custom_headers_from_metadata(auth: &mut Auth) {
    if auth.metadata.is_empty() {
        return;
    }
    for (name, value) in extract_custom_headers(&auth.metadata) {
        auth.attributes.insert(format!("header:{name}"), value);
    }
}

// ---- Loose type coercion (parseBoolAny / parseIntAny) ----

pub fn parse_bool_any(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => {
            let t = s.trim();
            match t {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
                "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
                _ => None,
            }
        }
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                Some(f != 0.0)
            } else {
                n.as_i64().map(|i| i != 0)
            }
        }
        _ => None,
    }
}

pub fn parse_int_any(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<i64>().ok()
            }
        }
        _ => None,
    }
}

// ---- Merge on re-login ----

/// Credential/token lifecycle fields that must not overwrite freshly acquired OAuth credentials.
pub fn is_auth_token_payload_key(key: &str) -> bool {
    matches!(
        key.trim().to_lowercase().as_str(),
        "access_token"
            | "refresh_token"
            | "id_token"
            | "session_id"
            | "expired"
            | "last_refresh"
            | "expires_in"
            | "timestamp"
            | "token_type"
            | "user_code"
            | "verification_uri"
            | "verification_uri_complete"
    )
}

/// `MergeExistingAuthMetadata`: keeps operator-set fields (proxy_url, priority, prefix, ...) of an
/// existing credential file across re-login. Token fields are never copied, and fields already set
/// on the target win.
pub fn merge_existing_auth_metadata(target: &mut Auth, existing: &Metadata) {
    if existing.is_empty() {
        return;
    }
    if !target.metadata.contains_key("disabled")
        && let Some(Value::Bool(d)) = existing.get("disabled")
    {
        target.disabled = *d;
    }
    let is_meta = target.provider.trim().eq_ignore_ascii_case("meta");
    for (k, v) in existing {
        if is_auth_token_payload_key(k) {
            continue;
        }
        if is_meta
            && matches!(
                canonical_credential_metadata_key(k),
                "api_key" | "dca_token" | "dca_expired" | "dca_expires_at"
            )
        {
            continue;
        }
        if !target.metadata.contains_key(k) {
            target.metadata.insert(k.clone(), v.clone());
        }
    }
    // Mirrors the Go call to storage.SetMetadata: storage reads auth.metadata at save time here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: Value) -> Metadata {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn legacy_keys_are_rewritten_and_canonical_wins() {
        let mut m = meta(
            json!({"proxy-url": "http://a", "proxy_url": "http://b", "request-retry": 2, "x": 1}),
        );
        normalize_credential_metadata(&mut m);
        assert_eq!(m.get("proxy_url"), Some(&json!("http://b")));
        assert_eq!(m.get("request_retry"), Some(&json!(2)));
        assert!(!m.contains_key("proxy-url") && !m.contains_key("request-retry"));
    }

    #[test]
    fn weight_rules() {
        assert_eq!(parse_weight_value(&json!("")).unwrap(), 1);
        assert_eq!(parse_weight_value(&json!(-5)).unwrap(), 0);
        assert_eq!(parse_weight_value(&json!(1_000_000)).unwrap(), 1_000_000);
        assert_eq!(
            parse_weight_value(&json!(1_000_001)),
            Err(WeightError::TooLarge)
        );
        assert_eq!(
            parse_weight_value(&json!(1.5)),
            Err(WeightError::NotInteger)
        );
        assert_eq!(
            parse_weight_value(&json!(true)),
            Err(WeightError::NotInteger)
        );
    }

    #[test]
    fn re_login_merge_keeps_operator_fields_not_tokens() {
        let mut target = Auth::default();
        target.provider = "claude".into();
        target.metadata = meta(json!({"email": "a@b.c"}));
        let existing = meta(json!({
            "email": "old@b.c", "access_token": "stale", "proxy_url": "http://p", "priority": 3, "disabled": true
        }));
        merge_existing_auth_metadata(&mut target, &existing);
        assert_eq!(target.metadata.get("email"), Some(&json!("a@b.c")));
        assert_eq!(target.metadata.get("proxy_url"), Some(&json!("http://p")));
        assert!(!target.metadata.contains_key("access_token"));
        assert!(target.disabled);
    }

    #[test]
    fn priority_and_headers_become_attributes() {
        let mut a = Auth::default();
        let m = meta(json!({"priority": "7", "headers": {" X-A ": " v ", "": "x", "B": ""}}));
        apply_auth_priority_metadata(&mut a, &m);
        a.metadata = m;
        apply_custom_headers_from_metadata(&mut a);
        assert_eq!(a.attributes.get("priority").map(String::as_str), Some("7"));
        assert_eq!(
            a.attributes.get("file_priority").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            a.attributes.get("header:X-A").map(String::as_str),
            Some("v")
        );
        assert_eq!(a.attributes.len(), 3);
    }
}
