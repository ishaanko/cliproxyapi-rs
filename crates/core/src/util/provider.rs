//! Provider resolution and credential masking (Go: util/provider.go).
//!
//! Not ported: `IsOpenAICompatibilityAlias` / `GetOpenAICompatibilityConfig` (they read
//! `config.Config`; the config crate owns those lookups).

use crate::registry::global_registry;

const OPENAI_COMPATIBLE_PROVIDER_PREFIX: &str = "openai-compatible-";

/// Internal provider key for an OpenAI-compatible provider: lowercased name prefixed with
/// `openai-compatible-` (empty -> `openai-compatibility`; already-prefixed names pass through).
pub fn openai_compatible_provider_key(name: &str) -> String {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return "openai-compatibility".into();
    }
    if name == "openai-compatibility" || name.starts_with(OPENAI_COMPATIBLE_PROVIDER_PREFIX) {
        return name;
    }
    format!("{OPENAI_COMPATIBLE_PROVIDER_PREFIX}{name}")
}

/// Providers able to serve a registered model, most available first (global registry lookup; a
/// lowercase retry when the exact name is unknown). Empty when the model is not registered.
pub fn get_provider_name(model_name: &str) -> Vec<String> {
    if model_name.is_empty() {
        return Vec::new();
    }
    let registry = global_registry();
    let mut providers: Vec<String> = Vec::new();
    let push_unique = |providers: &mut Vec<String>, found: Vec<String>| {
        for name in found {
            if !name.is_empty() && !providers.contains(&name) {
                providers.push(name);
            }
        }
    };
    push_unique(&mut providers, registry.get_model_providers(model_name));
    let lower = model_name.to_lowercase();
    if providers.is_empty() && lower != model_name {
        push_unique(&mut providers, registry.get_model_providers(&lower));
    }
    providers
}

/// Resolves the model name `auto` to the newest available registered model; any other name (or a
/// failed resolution) is returned unchanged.
pub fn resolve_auto_model(model_name: &str) -> String {
    if model_name != "auto" {
        return model_name.to_string();
    }
    match global_registry().get_first_available_model("") {
        Ok(first) => {
            tracing::info!("Resolved 'auto' model to: {first}");
            first
        }
        Err(err) => {
            tracing::warn!(
                "Failed to resolve 'auto' model: {err}, falling back to original model name"
            );
            model_name.to_string()
        }
    }
}

/// Whether `needle` is in `haystack` (Go: InArray).
pub fn in_array(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|item| item == needle)
}

/// Obscures an API key for logs, showing only the first and last few bytes.
pub fn hide_api_key(api_key: &str) -> String {
    String::from_utf8_lossy(&hide_api_key_bytes(api_key.as_bytes())).into_owned()
}

// Byte-wise like Go's slicing (which may cut inside a multi-byte character).
fn hide_api_key_bytes(key: &[u8]) -> Vec<u8> {
    let n = key.len();
    let edge = match n {
        9.. => 4,
        5..=8 => 2,
        3..=4 => 1,
        _ => return key.to_vec(),
    };
    [&key[..edge], b"...", &key[n - edge..]].concat()
}

/// Masks the credential of an Authorization header value, keeping the scheme prefix
/// (`Bearer abcd...wxyz`).
pub fn mask_authorization_header(value: &str) -> String {
    match value.trim().split_once(' ') {
        Some((scheme, credential)) => format!("{scheme} {}", hide_api_key(credential)),
        None => hide_api_key(value),
    }
}

/// Masks sensitive header values by header name (case-insensitive): `authorization` keeps its
/// scheme prefix; names containing api-key/apikey/token/secret are fully masked; others pass
/// through.
pub fn mask_sensitive_header_value(key: &str, value: &str) -> String {
    let lower = key.trim().to_lowercase();
    if lower.contains("authorization") {
        mask_authorization_header(value)
    } else if ["api-key", "apikey", "token", "secret"]
        .iter()
        .any(|s| lower.contains(s))
    {
        hide_api_key(value)
    } else {
        value.to_string()
    }
}

/// Masks sensitive query parameters (`key`, `*api-key*`, `*token*`, `*secret*`, ...) in a raw
/// query string, leaving the rest untouched.
pub fn mask_sensitive_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut changed = false;
    let parts: Vec<String> = raw
        .split('&')
        .map(|part| {
            if part.is_empty() {
                return part.to_string();
            }
            let (key_part, value_part) = part.split_once('=').unwrap_or((part, ""));
            let decoded_key =
                query_unescape(key_part).unwrap_or_else(|| key_part.as_bytes().to_vec());
            if !should_mask_query_param(&String::from_utf8_lossy(&decoded_key)) {
                return part.to_string();
            }
            let decoded_value =
                query_unescape(value_part).unwrap_or_else(|| value_part.as_bytes().to_vec());
            let masked = hide_api_key_bytes(decoded_value.trim_ascii());
            changed = true;
            format!("{key_part}={}", query_escape(&masked))
        })
        .collect();
    if changed {
        parts.join("&")
    } else {
        raw.to_string()
    }
}

fn should_mask_query_param(key: &str) -> bool {
    let key = key.trim().to_lowercase();
    if key.is_empty() {
        return false;
    }
    let key = key.strip_suffix("[]").unwrap_or(&key);
    key == "key"
        || ["api-key", "apikey", "api_key", "token", "secret"]
            .iter()
            .any(|s| key.contains(s))
}

/// Go `url.QueryEscape` over raw bytes.
fn query_escape(s: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `url.QueryUnescape`; `None` on a malformed percent escape.
fn query_unescape(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let hi = (hex[0] as char).to_digit(16)?;
                let lo = (hex[1] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(out)
}
