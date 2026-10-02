//! Shared helpers used by translators, thinking, signature, executors and the server
//! (Go: internal/util). Everything is re-exported flat, like the single Go package.
//!
//! Conventions: Go functions taking/returning JSON bytes take `&[u8]` / return `Vec<u8>`; the
//! gjson `Result` parameters become `&serde_json::Value` (absent results are `Option<&Value>`).
//! Schema cleaners stay string-in/string-out like Go.
//!
//! Not ported:
//! - `proxy.go` (`SetProxy`), `ssh_helper.go` (public IP lookup, SSH tunnel instructions): HTTP/CLI
//!   concerns outside the base layer (proxyutil / sdk/config).
//! - `SetLogLevel` (needs config + logrus), `CountAuthFiles` (generic over the auth store),
//!   `IsOpenAICompatibilityAlias` / `GetOpenAICompatibilityConfig` (need `config.Config`).
//! - `GetGJSONBytesNoCopy` / `ParseGJSONBytesNoCopy` (unsafe zero-copy gjson aliases; Rust code
//!   parses once into a `Value`).
//! - `WithSessionID` / `SessionIDFromContext` / `HasExplicitSessionID` (Go `context.Context`
//!   plumbing): callers pass the session id explicitly, see [`headers`].

mod claude;
mod claude_schema;
mod gemini_schema;
mod gojson;
mod headers;
mod provider;
mod responses_tools;
mod translator;

use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

pub use claude::*;
pub use claude_schema::*;
pub use gemini_schema::*;
pub use gojson::*;
pub use headers::*;
pub use provider::*;
pub use responses_tools::*;
pub use translator::*;

use cpa_json::J;

/// Default auth directory (Go: config.DefaultAuthDir).
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

static FUNCTION_NAME_SANITIZER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_.:-]").expect("static regex"));

/// Makes a function name valid for Gemini/Vertex: invalid chars (`[^a-zA-Z0-9_.:-]`) become `_`,
/// the name must start with a letter or `_` (else `_` is prepended), and it is capped at 64 bytes.
/// Empty input stays empty.
pub fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // The replacement is ASCII only, so byte truncation below is always on a char boundary.
    let mut sanitized = FUNCTION_NAME_SANITIZER.replace_all(name, "_").into_owned();
    match sanitized.as_bytes().first() {
        Some(first) if !(first.is_ascii_alphabetic() || *first == b'_') => {
            // Truncate first so the prepended underscore stays within the 64 byte limit.
            if sanitized.len() >= 64 {
                sanitized.truncate(63);
            }
            sanitized.insert(0, '_');
        }
        Some(_) => {}
        None => sanitized = "_".to_string(),
    }
    sanitized.truncate(64);
    sanitized
}

/// Lexical path cleaning (Go: filepath.Clean, Unix semantics).
pub fn filepath_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

fn user_home_dir() -> Result<PathBuf, String> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "$HOME is not defined".to_string())
}

/// Normalizes the auth directory: expands a leading `~` to the home directory and cleans the path.
/// An empty input defaults to `~/.cli-proxy-api`.
pub fn resolve_auth_dir(auth_dir: &str) -> Result<String, String> {
    let auth_dir = if auth_dir.is_empty() {
        DEFAULT_AUTH_DIR
    } else {
        auth_dir
    };
    let Some(rest) = auth_dir.strip_prefix('~') else {
        return Ok(filepath_clean(auth_dir));
    };
    let home = user_home_dir().map_err(|e| format!("resolve auth dir: {e}"))?;
    let home = filepath_clean(&home.to_string_lossy());
    let remainder = rest.trim_start_matches(['/', '\\']);
    if remainder.is_empty() {
        return Ok(home);
    }
    let normalized = remainder.replace('\\', "/");
    Ok(filepath_clean(&format!("{home}/{normalized}")))
}

/// Cleaned `WRITABLE_PATH` (or `writable_path`) environment value, or "" when unset/blank.
pub fn writable_path() -> String {
    for key in ["WRITABLE_PATH", "writable_path"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return filepath_clean(trimmed);
            }
        }
    }
    String::new()
}

/// GitHub API token in priority order: `GITHUB_TOKEN`, `github_token`, then `GITSTORE_GIT_TOKEN`
/// (only when `GITSTORE_GIT_URL` points at github.com). Empty when none is set.
pub fn resolve_github_token() -> String {
    let env_trimmed = |name: &str| {
        std::env::var(name)
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    for name in ["GITHUB_TOKEN", "github_token"] {
        let token = env_trimmed(name);
        if !token.is_empty() {
            return token;
        }
    }
    if !env_trimmed("GITSTORE_GIT_URL")
        .to_lowercase()
        .contains("github.com")
    {
        return String::new();
    }
    env_trimmed("GITSTORE_GIT_TOKEN")
}

/// Recognizes the native Codex "responses lite" header and its websocket metadata mirror in the
/// request body (`client_metadata.ws_request_header_x_openai_internal_codex_responses_lite`).
pub fn is_codex_responses_lite_request(body: &Value, headers: &http::HeaderMap) -> bool {
    let header = headers
        .get("x-openai-internal-codex-responses-lite")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    if header {
        return true;
    }
    match body
        .g("client_metadata.ws_request_header_x_openai_internal_codex_responses_lite")
        .v()
    {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Base64 (standard alphabet) PNG of a solid white image sized for a Gemini image aspect ratio
/// (`"1:1"`, `"16:9"`, ...); unknown ratios use 1024x1024. Pixel-identical to Go's output; the
/// compressed bytes may differ since the deflate encoders differ.
pub fn create_white_image_base64(aspect_ratio: &str) -> Result<String, String> {
    use base64::Engine;
    let (width, height): (u32, u32) = match aspect_ratio {
        "2:3" => (832, 1248),
        "3:2" => (1248, 832),
        "3:4" => (864, 1184),
        "4:3" => (1184, 864),
        "4:5" => (896, 1152),
        "5:4" => (1152, 896),
        "9:16" => (768, 1344),
        "16:9" => (1344, 768),
        "21:9" => (1536, 672),
        _ => (1024, 1024),
    };
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(&vec![0xFF; width as usize * height as usize * 4])
        .map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}

#[cfg(test)]
mod tests;
