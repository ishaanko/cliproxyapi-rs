//! User-defined upstream headers from auth attributes (Go: util/header_helpers.go).
//!
//! Go resolves the session id and client headers from `context.Context` / gin. Here the caller
//! passes them explicitly: `client_headers` is the inbound request's header map and `session_id`
//! mirrors the context annotation (`Some("")` is an explicit clear, `None` means "not annotated",
//! which falls back to the registered resolver).

use std::collections::HashMap;

use http::header::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::RwLock;

/// Pluggable fallback for the internal session id: `(client_headers) -> session_id`.
pub type SessionIdResolver = Box<dyn Fn(Option<&HeaderMap>) -> String + Send + Sync>;

static SESSION_ID_RESOLVER: RwLock<Option<SessionIdResolver>> = RwLock::new(None);

/// Installs (or clears) the global session id resolver (Go: the `SessionIDResolver` variable).
pub fn set_session_id_resolver(resolver: Option<SessionIdResolver>) {
    *SESSION_ID_RESOLVER.write() = resolver;
}

fn resolve_cpa_session_id(session_id: Option<&str>, client_headers: Option<&HeaderMap>) -> String {
    if let Some(explicit) = session_id {
        // An explicit annotation (even an empty one that clears) is authoritative.
        return explicit.trim().to_string();
    }
    match SESSION_ID_RESOLVER.read().as_ref() {
        Some(resolver) => resolver(client_headers).trim().to_string(),
        None => String::new(),
    }
}

/// Replaces every case-insensitive `$CPA-SESSION-ID` occurrence in `val`.
fn replace_cpa_session_id(val: &str, session_id: &str) -> String {
    const TARGET: &str = "$CPA-SESSION-ID";
    let bytes = val.as_bytes();
    if bytes.len() < TARGET.len() {
        return val.to_string();
    }
    let mut out = String::with_capacity(val.len());
    let (mut start, mut i) = (0, 0);
    while i + TARGET.len() <= bytes.len() {
        if bytes[i] == b'$' && bytes[i..i + TARGET.len()].eq_ignore_ascii_case(TARGET.as_bytes()) {
            out.push_str(&val[start..i]);
            out.push_str(session_id);
            i += TARGET.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start == 0 {
        return val.to_string();
    }
    out.push_str(&val[start..]);
    out
}

/// Collects the `header:<Name>` attributes into a header map, resolving `$Var` values from the
/// client headers and `$CPA-SESSION-ID` from the session id. Headers whose variable cannot be
/// resolved are omitted. Empty when nothing applies.
pub fn extract_custom_headers(
    attrs: &HashMap<String, String>,
    client_headers: Option<&HeaderMap>,
    session_id: Option<&str>,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for (k, v) in attrs {
        let Some(name) = k
            .strip_prefix("header:")
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let mut val = v.trim().to_string();
        if val.is_empty() {
            continue;
        }
        let var_name = val.strip_prefix('$').map(str::trim);
        if var_name.is_some_and(|n| n.eq_ignore_ascii_case("CPA-SESSION-ID")) {
            let id = resolve_cpa_session_id(session_id, client_headers);
            if id.is_empty() {
                continue;
            }
            val = id;
        } else if val.to_uppercase().contains("$CPA-SESSION-ID") {
            let id = resolve_cpa_session_id(session_id, client_headers);
            if id.is_empty() {
                continue;
            }
            val = replace_cpa_session_id(&val, &id);
        } else if let Some(var_name) = var_name {
            if var_name.is_empty() {
                continue;
            }
            let Some(client_val) = client_headers
                .and_then(|h| h.get(var_name))
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            val = client_val.to_string();
        }
        headers.insert(name.to_string(), val);
    }
    headers
}

/// Applies user-defined headers from auth attributes onto `headers` (custom values override
/// built-in defaults). See [`extract_custom_headers`] for variable resolution. A `Host` entry is
/// set as an ordinary header; callers building the real request should also use it as the host.
pub fn apply_custom_headers_from_attrs(
    headers: &mut HeaderMap,
    attrs: &HashMap<String, String>,
    client_headers: Option<&HeaderMap>,
    session_id: Option<&str>,
) {
    let mut custom: Vec<(String, String)> =
        extract_custom_headers(attrs, client_headers, session_id)
            .into_iter()
            .collect();
    custom.sort();
    for (k, v) in custom {
        if k.is_empty() || v.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(name, value);
        }
    }
}
