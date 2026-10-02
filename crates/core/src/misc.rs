//! Miscellaneous helpers and embedded data (Go: internal/misc).
//!
//! Not ported: `AsyncPrompt` (goroutine/channel CLI helper), `CopyConfigTemplate` (plain file copy
//! used by the CLI bootstrap), and the Antigravity version updater loop. The updater's network fetch
//! belongs to the caller; [`set_antigravity_version`] and [`parse_antigravity_manifest_version`]
//! cover the state and parsing halves.

mod mime_types;

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use http::header::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::util::filepath_clean;

// ---------------------------------------------------------------- mime types

/// Look up a MIME type by file extension (no leading dot, lowercase; exact match like the Go map).
pub fn mime_type_for_extension(ext: &str) -> Option<&'static str> {
    let table = mime_types::MIME_TYPES;
    table
        .binary_search_by(|(k, _)| (*k).cmp(ext))
        .ok()
        .map(|i| table[i].1)
}

// ---------------------------------------------------------------- embedded instructions

/// Claude Code system instruction block (JSON array text), embedded from claude_code_instructions.txt.
pub const CLAUDE_CODE_INSTRUCTIONS: &str = include_str!("../assets/claude_code_instructions.txt");

// ---------------------------------------------------------------- credentials

/// Prints the "Saving credentials to <path>" line (cleaned path); no-op for an empty path.
pub fn log_saving_credentials(path: &str) {
    if path.is_empty() {
        return;
    }
    println!("Saving credentials to {}", filepath_clean(path));
}

/// Emits the visual separator used to group auth processing logs (debug level).
pub fn log_credential_separator() {
    tracing::debug!("{}", "-".repeat(67));
}

/// Serializes `source` into a JSON object and overlays `metadata` on top of it (metadata wins).
/// `source` may be any `Serialize` value (a `Value::Null` stands for Go's nil). Errors when the
/// serialized source is neither an object nor null. A nil result in Go is an empty map here.
pub fn merge_metadata(
    source: &impl Serialize,
    metadata: Option<&Map<String, Value>>,
) -> anyhow::Result<Map<String, Value>> {
    let mut data = match serde_json::to_value(source)
        .map_err(|e| anyhow::anyhow!("failed to marshal source: {e}"))?
    {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => anyhow::bail!(
            "failed to unmarshal to map: cannot decode {} into an object",
            json_kind(&other)
        ),
    };
    if let Some(metadata) = metadata {
        for (k, v) in metadata {
            data.insert(k.clone(), v.clone());
        }
    }
    Ok(data)
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------- headers

/// Removes headers that reveal proxy infrastructure, client identity or browser fingerprints from
/// an outgoing request so it looks like it came from a native client (Go: ScrubProxyAndFingerprintHeaders).
pub fn scrub_proxy_and_fingerprint_headers(headers: &mut HeaderMap) {
    const SCRUBBED: [&str; 24] = [
        // Proxy tracing headers.
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-port",
        "x-real-ip",
        "forwarded",
        "via",
        // Client identity headers.
        "x-title",
        "x-stainless-lang",
        "x-stainless-package-version",
        "x-stainless-os",
        "x-stainless-arch",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "http-referer",
        "referer",
        // Browser / Chromium fingerprint headers.
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
        "sec-fetch-mode",
        "sec-fetch-site",
        "sec-fetch-dest",
        "priority",
        // Encoding negotiation (Node.js sends "gzip, deflate, br"; "zstd" would be a fingerprint).
        "accept-encoding",
    ];
    for name in SCRUBBED {
        headers.remove(name);
    }
}

/// Ensures `key` exists in `target`: a non-blank value from `source` wins (and overwrites), else an
/// existing non-blank target value is kept, else a non-blank `default_value` is set.
pub fn ensure_header(
    target: &mut HeaderMap,
    source: Option<&HeaderMap>,
    key: &str,
    default_value: &str,
) {
    let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else {
        return;
    };
    let header_text = |h: &HeaderMap| {
        h.get(&name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
    };
    if let Some(val) = source.and_then(header_text).filter(|v| !v.is_empty()) {
        if let Ok(v) = HeaderValue::from_str(&val) {
            target.insert(name, v);
        }
        return;
    }
    if header_text(target).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let val = default_value.trim();
    if !val.is_empty()
        && let Ok(v) = HeaderValue::from_str(val)
    {
        target.insert(name, v);
    }
}

// ---------------------------------------------------------------- oauth

/// Random 128-bit hex string for the OAuth2 `state` parameter (Go: GenerateRandomState).
pub fn generate_random_state() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Parsed OAuth callback parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub error: String,
    pub error_description: String,
}

/// Extracts OAuth parameters from a callback URL or a bare query/fragment string.
/// `Ok(None)` for blank input. Mirrors the Go normalization of partial inputs (`?code=..`,
/// `host:port/path`, `code=..&state=..`), fragment fallbacks and `code#state` splitting.
pub fn parse_oauth_callback(input: &str) -> Result<Option<OAuthCallback>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.starts_with('?') {
        format!("http://localhost{trimmed}")
    } else if trimmed.contains(['/', '?', '#']) || trimmed.contains(':') {
        format!("http://{trimmed}")
    } else if trimmed.contains('=') {
        format!("http://localhost/?{trimmed}")
    } else {
        return Err("invalid callback URL".into());
    };

    let parsed = url::Url::parse(&candidate).map_err(|e| e.to_string())?;
    let query_value = |key: &str| {
        parsed
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let mut code = query_value("code");
    let mut state = query_value("state");
    let mut err_code = query_value("error");
    let mut err_desc = query_value("error_description");

    // Go's url.URL.Fragment is already percent-decoded, and ParseQuery decodes a second time.
    if let Some(fragment) = parsed.fragment().filter(|f| !f.is_empty()) {
        let decoded = percent_encoding::percent_decode_str(fragment)
            .decode_utf8_lossy()
            .into_owned();
        let frag_value = |key: &str| {
            url::form_urlencoded::parse(decoded.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        if code.is_empty() {
            code = frag_value("code");
        }
        if state.is_empty() {
            state = frag_value("state");
        }
        if err_code.is_empty() {
            err_code = frag_value("error");
        }
        if err_desc.is_empty() {
            err_desc = frag_value("error_description");
        }
    }

    if !code.is_empty()
        && state.is_empty()
        && let Some((c, s)) = code.split_once('#')
    {
        let (c, s) = (c.to_string(), s.to_string());
        code = c;
        state = s;
    }

    if err_code.is_empty() && !err_desc.is_empty() {
        err_code = std::mem::take(&mut err_desc);
    }

    if code.is_empty() && err_code.is_empty() {
        return Err("callback URL missing code".into());
    }

    Ok(Some(OAuthCallback {
        code,
        state,
        error: err_code,
        error_description: err_desc,
    }))
}

// ---------------------------------------------------------------- antigravity user agent / version

/// Client version reported before the hub manifest has been fetched. Cloud Code rejects newer
/// models for clients below 2.9.0, so this floor must stay at or above that version.
pub const ANTIGRAVITY_FALLBACK_VERSION: &str = "2.9.1";
const ANTIGRAVITY_HUB_PLATFORM: &str = "darwin/arm64";
/// How long a fetched version stays authoritative (the updater refreshes at half this interval).
pub const ANTIGRAVITY_VERSION_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Timeout the caller should apply when fetching [`ANTIGRAVITY_HUB_LATEST_MANIFEST_URL`].
pub const ANTIGRAVITY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
pub const ANTIGRAVITY_NODE_API_CLIENT_UA: &str = "google-api-nodejs-client/10.3.0";
pub const ANTIGRAVITY_GOOG_API_CLIENT_UA: &str = "gl-node/22.21.1";
/// Manifest to fetch (headers `User-Agent: electron-builder`, `Cache-Control: no-cache`; 4 KiB cap).
pub const ANTIGRAVITY_HUB_LATEST_MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";

struct VersionCache {
    version: String,
    expiry: Option<Instant>,
}

static ANTIGRAVITY_VERSION: LazyLock<RwLock<VersionCache>> = LazyLock::new(|| {
    RwLock::new(VersionCache {
        version: ANTIGRAVITY_FALLBACK_VERSION.to_string(),
        expiry: None,
    })
});

/// Stores a freshly fetched version (valid for [`ANTIGRAVITY_VERSION_CACHE_TTL`]). The caller's
/// updater task calls this after a successful fetch and [`parse_antigravity_manifest_version`].
pub fn set_antigravity_version(version: &str) {
    let mut cache = ANTIGRAVITY_VERSION.write();
    cache.version = version.to_string();
    cache.expiry = Some(Instant::now() + ANTIGRAVITY_VERSION_CACHE_TTL);
}

/// Cached Antigravity version, or the fallback when the cache is empty or stale.
pub fn antigravity_latest_version() -> String {
    let cache = ANTIGRAVITY_VERSION.read();
    match cache.expiry {
        Some(expiry) if !cache.version.is_empty() && Instant::now() < expiry => {
            cache.version.clone()
        }
        _ => ANTIGRAVITY_FALLBACK_VERSION.to_string(),
    }
}

/// User-Agent of the Antigravity Hub family, e.g. `antigravity/hub/2.9.1 darwin/arm64`.
pub fn antigravity_user_agent() -> String {
    format!(
        "antigravity/hub/{} {}",
        antigravity_latest_version(),
        ANTIGRAVITY_HUB_PLATFORM
    )
}

fn is_antigravity_family_user_agent(lower: &str) -> bool {
    lower.starts_with("antigravity/hub/") || lower.starts_with("antigravity/")
}

fn antigravity_base_user_agent(user_agent: &str) -> String {
    let user_agent = user_agent.trim();
    if user_agent.is_empty() {
        return antigravity_user_agent();
    }
    let lower = user_agent.to_ascii_lowercase();
    if is_antigravity_family_user_agent(&lower)
        && let Some(idx) = lower.find(" google-api-nodejs-client/")
    {
        let trimmed = user_agent[..idx].trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    user_agent.to_string()
}

/// Short Antigravity runtime UA used by generate/stream/model-list requests.
pub fn antigravity_request_user_agent(user_agent: &str) -> String {
    antigravity_base_user_agent(user_agent)
}

/// Short Antigravity UA used by loadCodeAssist requests.
pub fn antigravity_load_code_assist_user_agent(user_agent: &str) -> String {
    antigravity_request_user_agent(user_agent)
}

/// Long Antigravity control-plane UA used by onboardUser requests.
pub fn antigravity_onboard_user_user_agent(user_agent: &str) -> String {
    let user_agent = user_agent.trim();
    if user_agent.is_empty() {
        return format!(
            "{} {}",
            antigravity_user_agent(),
            ANTIGRAVITY_NODE_API_CLIENT_UA
        );
    }
    let lower = user_agent.to_ascii_lowercase();
    if !is_antigravity_family_user_agent(&lower) || lower.contains("google-api-nodejs-client/") {
        return user_agent.to_string();
    }
    format!(
        "{} {}",
        antigravity_base_user_agent(user_agent),
        ANTIGRAVITY_NODE_API_CLIENT_UA
    )
}

/// Extracts the version from the short or long Antigravity UA forms; the latest cached version
/// when the UA is not an Antigravity one or carries no version.
pub fn antigravity_version_from_user_agent(user_agent: &str) -> String {
    let base = antigravity_base_user_agent(user_agent);
    let lower = base.to_ascii_lowercase();
    let rest = ["antigravity/hub/", "antigravity/"]
        .into_iter()
        .find(|prefix| lower.starts_with(prefix))
        .map(|prefix| &base[prefix.len()..]);
    let Some(rest) = rest else {
        return antigravity_latest_version();
    };
    let rest = rest.split([' ', '\t']).next().unwrap_or("").trim();
    if rest.is_empty() {
        antigravity_latest_version()
    } else {
        rest.to_string()
    }
}

/// Extracts and validates the `version:` field of the hub updater manifest (YAML bytes).
/// Only top-level `version: x.y.z` lines are understood, which is all the manifest carries.
pub fn parse_antigravity_manifest_version(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("version:"))
        .map(|v| {
            let v = v.split(" #").next().unwrap_or("").trim();
            v.trim_matches(|c| c == '"' || c == '\'').trim().to_string()
        })
        .unwrap_or_default();
    if value.is_empty() {
        return Err("antigravity Hub updater manifest returned empty version".into());
    }
    if !is_valid_antigravity_sem_version(&value) {
        return Err(format!(
            "antigravity Hub updater manifest returned invalid version {value:?}"
        ));
    }
    Ok(value)
}

fn is_valid_antigravity_sem_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_lookup() {
        assert_eq!(mime_type_for_extension("json"), Some("application/json"));
        assert_eq!(mime_type_for_extension("png"), Some("image/png"));
        assert_eq!(mime_type_for_extension("nope"), None);
    }

    #[test]
    fn mime_table_sorted_for_binary_search() {
        assert!(mime_types::MIME_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn user_agent_forms() {
        assert_eq!(
            antigravity_version_from_user_agent("antigravity/hub/2.2.1 darwin/arm64"),
            "2.2.1"
        );
        assert_eq!(
            antigravity_version_from_user_agent("antigravity/1.23.2 windows/amd64"),
            "1.23.2"
        );
        let long = "antigravity/hub/2.2.1 darwin/arm64 google-api-nodejs-client/10.3.0";
        assert_eq!(
            antigravity_request_user_agent(long),
            "antigravity/hub/2.2.1 darwin/arm64"
        );
        assert_eq!(
            antigravity_onboard_user_user_agent("antigravity/hub/2.2.1 darwin/arm64"),
            long
        );
        assert_eq!(
            antigravity_onboard_user_user_agent("custom/1.0"),
            "custom/1.0"
        );
    }

    #[test]
    fn manifest_version() {
        assert_eq!(
            parse_antigravity_manifest_version(b"version: 2.2.1\npath: x.zip\n").unwrap(),
            "2.2.1"
        );
        assert!(parse_antigravity_manifest_version(b"version: 2.2\n").is_err());
        assert!(parse_antigravity_manifest_version(b"path: x\n").is_err());
    }

    #[test]
    fn oauth_callback_forms() {
        let cb = parse_oauth_callback("http://localhost:1455/auth/callback?code=abc&state=xyz")
            .unwrap()
            .unwrap();
        assert_eq!((cb.code.as_str(), cb.state.as_str()), ("abc", "xyz"));
        let cb = parse_oauth_callback("code=abc%23st").unwrap().unwrap();
        assert_eq!((cb.code.as_str(), cb.state.as_str()), ("abc", "st"));
        let cb = parse_oauth_callback("?error_description=denied")
            .unwrap()
            .unwrap();
        assert_eq!(
            (cb.error.as_str(), cb.error_description.as_str()),
            ("denied", "")
        );
        assert!(parse_oauth_callback("garbage").is_err());
        assert!(parse_oauth_callback("   ").unwrap().is_none());
    }

    #[test]
    fn ensure_header_priority() {
        let mut target = HeaderMap::new();
        let mut source = HeaderMap::new();
        source.insert("x-a", HeaderValue::from_static(" from-source "));
        ensure_header(&mut target, Some(&source), "X-A", "default");
        assert_eq!(target["x-a"], "from-source");
        ensure_header(&mut target, None, "X-B", " dflt ");
        assert_eq!(target["x-b"], "dflt");
        ensure_header(&mut target, None, "X-B", "other");
        assert_eq!(target["x-b"], "dflt");
    }
}
