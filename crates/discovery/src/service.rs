//! Service spec construction from config (Go: internal/discovery/service.go).

use std::net::IpAddr;
use std::path::PathBuf;

use cpa_config::Config;

use crate::id::{DEFAULT_INSTANCE_PREFIX, format_instance_name, get_or_generate_instance_id};
use crate::interfaces::{Interface, extract_interface_ips, filter_interfaces, ip_is_loopback, ip_is_unspecified};
use crate::txt::{TxtOptions, build_txt_records};
use crate::types::{
    DEFAULT_DOMAIN, DEFAULT_SERVICE_TYPE, Error, SUBTYPE_CHAT_COMPLETIONS, SUBTYPE_GENERATE_CONTENT, SUBTYPE_INTERACTIONS,
    SUBTYPE_MESSAGES, SUBTYPE_RESPONSES, ServiceSpec,
};

/// Absolute state directory for discovery metadata (the persistent instance ID), or "" when no
/// writable location can be found. Avoids writing relative paths into the working directory.
pub fn resolve_discovery_state_dir() -> String {
    let writable = cpa_core::util::writable_path();
    if !writable.is_empty() {
        return PathBuf::from(writable).join("discovery").to_string_lossy().into_owned();
    }
    if let Some(config_dir) = user_config_dir() {
        return config_dir.join("cpa").join("discovery").to_string_lossy().into_owned();
    }
    if let Some(home) = user_home_dir() {
        return home.join(".config").join("cpa").join("discovery").to_string_lossy().into_owned();
    }
    String::new()
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Go `os.UserConfigDir()`.
fn user_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return user_home_dir().map(|h| h.join("Library").join("Application Support"));
    }
    if cfg!(windows) {
        return std::env::var("APPDATA").ok().filter(|d| !d.is_empty()).map(PathBuf::from);
    }
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => {
            // Go rejects a relative XDG_CONFIG_HOME instead of falling back to $HOME/.config.
            let path = PathBuf::from(dir);
            path.is_absolute().then_some(path)
        }
        _ => user_home_dir().map(|h| h.join(".config")),
    }
}

/// Checks `st` against RFC 6763 / RFC 6335. CPA only exposes a TCP listener, so advertised types
/// must end with `._tcp`.
pub fn validate_service_type(st: &str) -> Result<(), Error> {
    let st = st.trim();
    if st.is_empty() {
        return Err(Error::new("service type cannot be empty"));
    }
    if st.len() > 63 {
        return Err(Error::new(format!("service type {st:?} exceeds 63 characters")));
    }
    let Some(prefix) = st.strip_suffix("._tcp") else {
        return Err(Error::new(format!("service type {st:?} must end with ._tcp")));
    };
    let Some(name) = prefix.strip_prefix('_') else {
        return Err(Error::new(format!("service type {st:?} must start with an underscore")));
    };
    if name.is_empty() || name.len() > 15 {
        return Err(Error::new(format!("service name {name:?} must be between 1 and 15 characters (RFC 6335)")));
    }
    let last = name.len() - 1;
    for (i, c) in name.char_indices() {
        if !(c.is_ascii_alphanumeric() || c == '-') {
            return Err(Error::new(format!("service type contains invalid character {c:?} (RFC 6335)")));
        }
        if (i == 0 || i == last) && c == '-' {
            return Err(Error::new("service name cannot start or end with a hyphen"));
        }
    }
    Ok(())
}

/// Validates and normalizes a subtype label (RFC 6763 section 7.1). Returns the label prefixed
/// with `_` (e.g. `_responses`) or an empty string when invalid.
pub fn sanitize_subtype(sub: &str) -> String {
    let sub = sub.trim();
    if sub.is_empty() || sub.contains('.') {
        return String::new();
    }
    let sub = if sub.starts_with('_') { sub.to_string() } else { format!("_{sub}") };
    let label = &sub[1..];
    if label.is_empty() || label.len() > 62 {
        return String::new();
    }
    // Alphanumeric and hyphen only, no leading or trailing hyphen, no internal underscores.
    if label.starts_with('-') || label.ends_with('-') {
        return String::new();
    }
    if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return String::new();
    }
    sub
}

/// Drops trailing characters until `s` fits in `max_bytes` without splitting a UTF-8 sequence.
pub(crate) fn truncate_runes_to(s: &str, max_bytes: usize) -> &str {
    if max_bytes == 0 {
        return "";
    }
    let mut end = s.len();
    while end > max_bytes {
        end -= 1;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
    }
    &s[..end]
}

/// Limits a name to 63 bytes on rune boundaries and removes control characters.
pub(crate) fn sanitize_instance_name(name: &str) -> String {
    let filtered: String = name.chars().filter(|&c| c as u32 >= 32 && c as u32 != 127).collect();
    truncate_runes_to(filtered.trim(), 63).to_string()
}

fn interface_has_ip(iface: &Interface, target: IpAddr) -> bool {
    iface.addrs.iter().any(|ip| ip.to_canonical() == target.to_canonical())
}

/// Creates a [`ServiceSpec`] from configuration, listen port and TLS setting.
pub fn build_service_spec(cfg: &Config, port: i64, tls_enabled: bool) -> Result<ServiceSpec, Error> {
    if !(1..=65535).contains(&port) {
        return Err(Error::new(format!("discovery: invalid service port {port} (must be between 1 and 65535)")));
    }
    let disc_cfg = &cfg.discovery;

    // 1. Persistent instance ID.
    let state_dir = resolve_discovery_state_dir();
    let instance_id = get_or_generate_instance_id(&state_dir);

    // 2. Instance name (CPA-<ShortID> or <custom>-<ShortID>) within DNS label limits.
    let mut instance_name = format_instance_name(&disc_cfg.service_name, &instance_id);
    if instance_name.is_empty() {
        instance_name = format!("{DEFAULT_INSTANCE_PREFIX}{instance_id}");
    }

    // 3. Service type validation.
    let service_type = disc_cfg.service_type.trim();
    let service_type = if service_type.is_empty() {
        DEFAULT_SERVICE_TYPE.to_string()
    } else {
        validate_service_type(service_type).map_err(|e| Error::new(format!("discovery: invalid service-type: {e}")))?;
        service_type.to_string()
    };

    // 4. Subtypes.
    let raw_subtypes: Vec<String> = if disc_cfg.subtypes.is_empty() {
        [SUBTYPE_CHAT_COMPLETIONS, SUBTYPE_RESPONSES, SUBTYPE_MESSAGES, SUBTYPE_GENERATE_CONTENT, SUBTYPE_INTERACTIONS]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        disc_cfg.subtypes.clone()
    };
    let mut subtypes: Vec<String> = raw_subtypes.iter().map(|raw| sanitize_subtype(raw)).filter(|s| !s.is_empty()).collect();
    if subtypes.is_empty() {
        subtypes = vec![SUBTYPE_CHAT_COMPLETIONS.to_string()];
    }

    // 5. Interface filtering.
    let mut ifaces = match filter_interfaces(&disc_cfg.interfaces.include, &disc_cfg.interfaces.exclude) {
        Ok(ifaces) => ifaces,
        Err(err) => {
            tracing::warn!("discovery: failed to filter interfaces: {err}");
            Vec::new()
        }
    };
    if ifaces.is_empty() {
        return Err(Error::new(
            "discovery: no qualified physical interfaces found matching filters (refusing fallback to all interfaces)",
        ));
    }
    let bind_host = cfg.host.trim();
    let mut bind_ip: Option<IpAddr> = None;
    if !bind_host.is_empty() {
        let Ok(ip) = bind_host.parse::<IpAddr>() else {
            return Err(Error::new(format!("discovery: refusing LAN advertising for non-IP bind host {bind_host:?}")));
        };
        if ip_is_loopback(ip) {
            return Err(Error::new(format!("discovery: LAN advertising is unavailable for loopback bind host {bind_host:?}")));
        }
        bind_ip = Some(ip);
    }
    let specific_bind = bind_ip.filter(|ip| !ip_is_unspecified(*ip));
    if let Some(bind) = specific_bind {
        ifaces.retain(|iface| interface_has_ip(iface, bind));
        if ifaces.is_empty() {
            return Err(Error::new(format!("discovery: no interface owns bind host {bind_host:?}")));
        }
    }
    let mut advertised_ips = extract_interface_ips(&ifaces);
    if let Some(bind) = specific_bind {
        advertised_ips.retain(|ip| ip.parse::<IpAddr>().is_ok_and(|parsed| parsed.to_canonical() == bind.to_canonical()));
    }
    if advertised_ips.is_empty() {
        return Err(Error::new(format!("discovery: no advertised address matches bind host {bind_host:?}")));
    }

    // 6. TXT records.
    let mut txt_opts = TxtOptions::defaults();
    txt_opts.instance_id = instance_id;
    txt_opts.tls = tls_enabled;
    txt_opts.advertise_management = disc_cfg.advertise_management;
    txt_opts.auth_required = disc_cfg.auth_required.unwrap_or(true);
    let text_records = build_txt_records(txt_opts);

    Ok(ServiceSpec {
        instance_name,
        service_type,
        domain: DEFAULT_DOMAIN.to_string(),
        port,
        subtypes,
        text_records,
        interfaces: ifaces,
        advertised_ips,
    })
}
