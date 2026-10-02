//! Kimi request headers (Go: applyKimiHeaders / applyKimiHeadersWithAuth).

use std::collections::HashMap;
use std::path::PathBuf;

use cpa_auth::Auth;
use cpa_core::util::apply_custom_headers_from_attrs;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};

use super::resolve_device_id;

const DEFAULT_DEVICE_ID: &str = "cli-proxy-api-device";

fn set(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

/// Machine hostname (Go: os.Hostname), `unknown` when unreadable.
fn hostname() -> String {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(v) = std::fs::read_to_string(path) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    "unknown".to_string()
}

/// `<GOOS> <GOARCH>` in Go's spelling, matching the kimi-cli format.
fn device_model() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    format!("{os} {arch}")
}

/// kimi-cli's `device_id` file when present (platform specific location), else a fixed id.
fn default_device_id() -> String {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let Some(home) = home else {
        return DEFAULT_DEVICE_ID.to_string();
    };
    let share_dir = match std::env::consts::OS {
        "macos" => home.join("Library").join("Application Support").join("kimi"),
        "windows" => {
            let app_data = std::env::var_os("APPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Roaming"));
            app_data.join("kimi")
        }
        _ => home.join(".local").join("share").join("kimi"),
    };
    match std::fs::read_to_string(share_dir.join("device_id")) {
        Ok(v) => v.trim().to_string(),
        Err(_) => DEFAULT_DEVICE_ID.to_string(),
    }
}

/// Headers for one upstream request: identity headers, the per-credential device id, then the
/// credential's custom `header:<Name>` attributes (which win).
pub(super) fn kimi_headers(token: &str, stream: bool, auth: &Auth, client_headers: &HeaderMap) -> HeaderMap {
    let version = cpa_auth::client_version();
    let mut h = HeaderMap::new();
    set(&mut h, CONTENT_TYPE, "application/json");
    set(&mut h, AUTHORIZATION, &format!("Bearer {token}"));
    set(&mut h, USER_AGENT, &format!("CLIProxyAPI/{version}"));
    set(&mut h, HeaderName::from_static("x-msh-platform"), "CLIProxyAPI");
    set(&mut h, HeaderName::from_static("x-msh-version"), &version);
    set(&mut h, HeaderName::from_static("x-msh-device-name"), &hostname());
    set(&mut h, HeaderName::from_static("x-msh-device-model"), &device_model());
    let device_id = resolve_device_id(auth);
    let device_id = if device_id.is_empty() { default_device_id() } else { device_id };
    set(&mut h, HeaderName::from_static("x-msh-device-id"), &device_id);
    set(&mut h, ACCEPT, if stream { "text/event-stream" } else { "application/json" });

    let attrs: HashMap<String, String> = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    apply_custom_headers_from_attrs(&mut h, &attrs, Some(client_headers), None);
    h
}
