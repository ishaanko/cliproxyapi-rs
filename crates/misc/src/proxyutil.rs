//! Proxy setting parsing and redaction (Go: sdk/proxyutil).
//!
//! The Go package also builds `http.Transport`s and tunnel dialers; in Rust the HTTP clients are
//! reqwest clients configured by `cpa_auth::http` and `cpa_executors::helps::proxy`, which handle
//! HTTP CONNECT and SOCKS5 themselves, so only the pieces that are not reqwest specific live
//! here: [`parse`] (with Go's `net/url` acceptance rules), [`valid_request_proxy`] and [`redact`].

/// How a proxy setting should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No explicit proxy behavior was configured.
    Inherit,
    /// Outbound requests must bypass proxies explicitly.
    Direct,
    /// A concrete proxy URL was configured.
    Proxy,
    /// The setting is present but malformed or unsupported.
    Invalid,
}

/// The normalized interpretation of a proxy configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub raw: String,
    pub mode: Mode,
    pub url: Option<ProxyUrl>,
}

/// The parts of a proxy URL the dialers need (Go `*url.URL`, unescaped like Go).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUrl {
    pub scheme: String,
    /// `Some` when the URL has a userinfo section.
    pub username: Option<String>,
    pub password: Option<String>,
    /// `host[:port]` as written (brackets kept).
    pub host: String,
}

impl ProxyUrl {
    /// `URL.Hostname()`.
    pub fn hostname(&self) -> &str {
        let host = split_host_port(&self.host).0;
        host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
    }

    /// `URL.Port()`.
    pub fn port(&self) -> &str {
        split_host_port(&self.host).1
    }
}

/// `splitHostPort`: a trailing `:digits` is the port.
fn split_host_port(host: &str) -> (&str, &str) {
    if let Some(colon) = host.rfind(':')
        && valid_optional_port(&host[colon..])
    {
        return (&host[..colon], &host[colon + 1..]);
    }
    (host, "")
}

fn valid_optional_port(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => digits.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// Errors from [`parse`]; the messages never include the proxy URL (it may hold credentials).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("parse proxy URL failed")]
    Parse,
    #[error("proxy URL missing scheme/host")]
    MissingSchemeHost,
    #[error("unsupported proxy scheme: {0}")]
    UnsupportedScheme(String),
}

/// Normalizes a proxy configuration value into inherit, direct, or proxy modes. On error the
/// setting (with `Mode::Invalid`) is returned alongside it, as Go does.
#[allow(clippy::result_large_err)] // Go returns the setting next to the error
pub fn parse(raw: &str) -> Result<Setting, (Setting, ParseError)> {
    let trimmed = raw.trim();
    let mut setting = Setting { raw: trimmed.to_string(), mode: Mode::Inherit, url: None };
    if trimmed.is_empty() {
        return Ok(setting);
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        setting.mode = Mode::Direct;
        return Ok(setting);
    }
    setting.mode = Mode::Invalid;
    let Some(parsed) = parse_go_url(trimmed) else {
        return Err((setting, ParseError::Parse));
    };
    if parsed.scheme.is_empty() || parsed.host.is_empty() {
        return Err((setting, ParseError::MissingSchemeHost));
    }
    match parsed.scheme.as_str() {
        "socks5" | "socks5h" | "http" | "https" => {
            setting.mode = Mode::Proxy;
            setting.url = Some(parsed);
            Ok(setting)
        }
        other => {
            let err = ParseError::UnsupportedScheme(other.to_string());
            Err((setting, err))
        }
    }
}

/// Reports whether `raw` is a concrete execution proxy override: a proxy URL with a host and, when
/// a port is given, one in 1-65535.
pub fn valid_request_proxy(raw: &str) -> bool {
    let Ok(setting) = parse(raw) else { return false };
    let Some(url) = setting.url.as_ref().filter(|_| setting.mode == Mode::Proxy) else {
        return false;
    };
    if url.hostname().trim().is_empty() {
        return false;
    }
    let port = url.port();
    if port.is_empty() {
        return true;
    }
    // Go's Atoi: an optional sign then digits; the port here is digits only.
    port.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
}

/// A log-safe proxy URL with credentials and path-like data removed.
pub fn redact(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match parse_go_url(trimmed) {
        Some(u) if !u.scheme.is_empty() && !u.host.is_empty() => {
            let user = if u.username.is_some() { "redacted@" } else { "" };
            format!("{}://{}{}", u.scheme, user, escape_host(&u.host))
        }
        _ => "<invalid proxy URL>".to_string(),
    }
}

/// `URL.String()`'s host escaping: bytes outside the host-safe set are percent-encoded.
fn escape_host(host: &str) -> String {
    let mut out = String::with_capacity(host.len());
    for c in host.chars() {
        match u8::try_from(c) {
            Ok(b) if b < 0x80 && !host_byte_ok(b) => out.push_str(&format!("%{b:02X}")),
            _ => out.push(c),
        }
    }
    out
}

fn host_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:[]<>\"".contains(&b)
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn unhex(b: u8) -> u8 {
    (b as char).to_digit(16).unwrap_or(0) as u8
}

/// `url.unescape` validation (and decoding) for the userinfo and host modes. `None` when Go
/// would return an error.
fn unescape(s: &str, host_mode: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                if !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
                    return None;
                }
                let v = unhex(bytes[i + 1]) << 4 | unhex(bytes[i + 2]);
                if host_mode && unhex(bytes[i + 1]) < 8 && &bytes[i..i + 3] != b"%25" {
                    return None;
                }
                out.push(v);
                i += 3;
            }
            b if host_mode && b < 0x80 && !host_byte_ok(b) => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn parse_host(host: &str) -> Option<String> {
    if host.starts_with('[') {
        let i = host.rfind(']')?;
        if !valid_optional_port(&host[i + 1..]) {
            return None;
        }
        return unescape(host, true);
    }
    if let Some(i) = host.rfind(':')
        && !valid_optional_port(&host[i..])
    {
        return None;
    }
    unescape(host, true)
}

fn valid_userinfo(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&b))
}

/// The subset of Go's `url.Parse` the proxy code relies on: scheme, authority and the validation
/// errors Go reports for them (bad escapes, ports, host characters). Path, query and fragment are
/// only checked for escapes where Go checks them.
fn parse_go_url(raw: &str) -> Option<ProxyUrl> {
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let (main, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    unescape_ok(fragment)?;
    // getScheme
    let mut scheme = String::new();
    let mut rest = main;
    for (i, c) in main.char_indices() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || matches!(c, '+' | '-' | '.') {
            if i == 0 {
                break;
            }
            continue;
        }
        if c == ':' {
            if i == 0 {
                return None;
            }
            scheme = main[..i].to_ascii_lowercase();
            rest = &main[i + 1..];
        }
        break;
    }
    let (rest, _query) = rest.split_once('?').map_or((rest, ""), |(a, b)| (a, b));
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // Opaque URL: no host.
            return Some(ProxyUrl { scheme, username: None, password: None, host: String::new() });
        }
        let first_segment = rest.split('/').next().unwrap_or("");
        if first_segment.contains(':') {
            return None;
        }
    }
    let mut url = ProxyUrl { scheme, username: None, password: None, host: String::new() };
    let authority_path = rest.strip_prefix("//");
    let path = match authority_path {
        Some(after) if !url.scheme.is_empty() || !rest.starts_with("///") => {
            let (authority, path) = match after.find('/') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            parse_authority(authority, &mut url)?;
            path
        }
        _ => rest,
    };
    unescape_ok(path)?;
    Some(url)
}

fn unescape_ok(s: &str) -> Option<()> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            if !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
                return None;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Some(())
}

fn parse_authority(authority: &str, url: &mut ProxyUrl) -> Option<()> {
    let (userinfo, host) = match authority.rfind('@') {
        None => (None, authority),
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
    };
    url.host = parse_host(host)?;
    if let Some(userinfo) = userinfo {
        if !valid_userinfo(userinfo) {
            return None;
        }
        match userinfo.split_once(':') {
            None => url.username = Some(unescape(userinfo, false)?),
            Some((user, pass)) => {
                url.username = Some(unescape(user, false)?);
                url.password = Some(unescape(pass, false)?);
            }
        }
    }
    Some(())
}

#[cfg(test)]
#[path = "proxyutil_tests.rs"]
mod tests;
