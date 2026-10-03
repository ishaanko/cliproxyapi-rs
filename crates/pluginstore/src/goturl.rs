//! Minimal port of Go's `net/url` parsing, covering what the plugin store inspects
//! (scheme, userinfo presence, host with port, decoded/escaped path, raw query, fragment).
//! Using Go's rules (rather than the WHATWG `url` crate) keeps host/port matching and
//! validation errors identical to the reference.

use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GoUrl {
    pub scheme: String,
    pub opaque: String,
    pub has_user: bool,
    /// Host including any `:port`.
    pub host: String,
    /// Decoded path.
    pub path: String,
    raw_path: String,
    pub raw_query: String,
    pub force_query: bool,
    pub fragment: String,
}

/// Go `url.Parse` error text, e.g. `parse "x y": first path segment ...`.
pub type ParseError = String;

fn quote(s: &str) -> String {
    format!("{s:?}")
}

fn wrap(raw: &str, err: &str) -> ParseError {
    format!("parse {}: {}", quote(raw), err)
}

fn ishex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Path,
    PathSegment,
    Host,
    Query,
}

/// Go `unescape`: percent-decodes (`+` only in query mode); errors on bad escapes.
fn unescape(s: &str, mode: Mode) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex_ok = i + 2 < bytes.len() && ishex(bytes[i + 1]) && ishex(bytes[i + 2]);
                if !hex_ok {
                    let mut rest = &s[i..];
                    if rest.len() > 3 {
                        let mut end = 3;
                        while !rest.is_char_boundary(end) {
                            end -= 1;
                        }
                        rest = &rest[..end];
                    }
                    return Err(format!("invalid URL escape {}", quote(rest)));
                }
                out.push((unhex(bytes[i + 1]) << 4) | unhex(bytes[i + 2]));
                i += 3;
            }
            b'+' if mode == Mode::Query => {
                out.push(b' ');
                i += 1;
            }
            c => {
                if mode == Mode::Host && c < 0x80 && host_should_escape(c) {
                    return Err(format!("invalid character {} in host name", quote(&(c as char).to_string())));
                }
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn host_should_escape(c: u8) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    !matches!(
        c,
        b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b','
            | b';' | b'=' | b':' | b'[' | b']' | b'<' | b'>' | b'"' | b'%'
    )
}

/// `url.PathUnescape`.
pub fn path_unescape(s: &str) -> Result<String, String> {
    unescape(s, Mode::PathSegment)
}

/// `url.PathEscape`.
pub fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Go `escape(s, encodePath)`.
fn escape_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        let keep = c.is_ascii_alphanumeric()
            || matches!(c, b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'@');
        if keep {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Go `validEncoded(s, encodePath)`.
fn valid_encoded_path(s: &str) -> bool {
    s.bytes().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+'
                    | b',' | b';' | b'=' | b':' | b'@' | b'[' | b']' | b'%' | b'/'
            )
    })
}

fn valid_optional_port(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => digits.bytes().all(|c| c.is_ascii_digit()),
    }
}

fn valid_userinfo(s: &str) -> bool {
    s.bytes().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'.' | b'_' | b':' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*'
                    | b'+' | b',' | b';' | b'=' | b'%' | b'@'
            )
    })
}

impl GoUrl {
    /// Go `url.Parse`.
    pub fn parse(raw: &str) -> Result<GoUrl, ParseError> {
        let (without_fragment, fragment) = match raw.split_once('#') {
            Some((u, f)) => (u, f),
            None => (raw, ""),
        };
        let mut url = Self::parse_inner(without_fragment).map_err(|e| wrap(without_fragment, &e))?;
        url.fragment = unescape(fragment, Mode::PathSegment)
            .map(|_| fragment.to_string())
            .map_err(|e| wrap(raw, &e))?;
        Ok(url)
    }

    fn parse_inner(raw: &str) -> Result<GoUrl, String> {
        if raw.bytes().any(|c| c < 0x20 || c == 0x7f) {
            return Err("net/url: invalid control character in URL".to_string());
        }
        let mut url = GoUrl::default();
        if raw == "*" {
            url.path = "*".to_string();
            return Ok(url);
        }
        let mut rest = raw;
        // getScheme
        let mut scheme_end = None;
        for (i, c) in raw.char_indices() {
            match c {
                'a'..='z' | 'A'..='Z' => {}
                '0'..='9' | '+' | '-' | '.' => {
                    if i == 0 {
                        break;
                    }
                }
                ':' => {
                    if i == 0 {
                        return Err("missing protocol scheme".to_string());
                    }
                    scheme_end = Some(i);
                    break;
                }
                _ => break,
            }
        }
        if let Some(i) = scheme_end {
            url.scheme = raw[..i].to_ascii_lowercase();
            rest = &raw[i + 1..];
        }
        if rest.ends_with('?') && rest.matches('?').count() == 1 {
            url.force_query = true;
            rest = &rest[..rest.len() - 1];
        } else if let Some((before, query)) = rest.split_once('?') {
            url.raw_query = query.to_string();
            rest = before;
        }
        if !rest.starts_with('/') {
            if !url.scheme.is_empty() {
                url.opaque = rest.to_string();
                return Ok(url);
            }
            let segment = rest.split('/').next().unwrap_or("");
            if segment.contains(':') {
                return Err("first path segment in URL cannot contain colon".to_string());
            }
        }
        if (!url.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
            let after = &rest[2..];
            let (authority, remainder) = match after.find('/') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            rest = remainder;
            let host_part = match authority.rfind('@') {
                Some(i) => {
                    if !valid_userinfo(&authority[..i]) {
                        return Err("net/url: invalid userinfo".to_string());
                    }
                    url.has_user = true;
                    &authority[i + 1..]
                }
                None => authority,
            };
            url.host = parse_host(host_part)?;
        }
        url.path = unescape(rest, Mode::Path)?;
        url.raw_path = if escape_path(&url.path) == rest { String::new() } else { rest.to_string() };
        Ok(url)
    }

    /// Go `URL.Hostname`.
    pub fn hostname(&self) -> &str {
        split_host_port(&self.host).0
    }

    /// Go `URL.Port`.
    pub fn port(&self) -> &str {
        split_host_port(&self.host).1
    }

    /// Go `URL.EscapedPath`.
    pub fn escaped_path(&self) -> String {
        if !self.raw_path.is_empty()
            && valid_encoded_path(&self.raw_path)
            && unescape(&self.raw_path, Mode::Path).map(|p| p == self.path).unwrap_or(false)
        {
            return self.raw_path.clone();
        }
        if self.path == "*" {
            return "*".to_string();
        }
        escape_path(&self.path)
    }

    /// Keys of `URL.Query()` (pairs with bad escapes or `;` are dropped like `ParseQuery`).
    pub fn query_keys(&self) -> HashSet<String> {
        let mut keys = HashSet::new();
        for pair in self.raw_query.split('&') {
            if pair.is_empty() || pair.contains(';') {
                continue;
            }
            let key = pair.split_once('=').map_or(pair, |(k, _)| k);
            if let Ok(key) = unescape(key, Mode::Query) {
                keys.insert(key);
            }
        }
        keys
    }

    /// Go `URL.String` for URLs without userinfo.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.scheme.is_empty() {
            out.push_str(&self.scheme);
            out.push(':');
        }
        if !self.opaque.is_empty() {
            out.push_str(&self.opaque);
        } else {
            if !self.scheme.is_empty() || !self.host.is_empty() || self.has_user {
                if !self.host.is_empty() || !self.path.is_empty() || self.has_user {
                    out.push_str("//");
                }
                out.push_str(&self.host);
            }
            let path = self.escaped_path();
            if !path.is_empty() && !path.starts_with('/') && !self.host.is_empty() {
                out.push('/');
            }
            out.push_str(&path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            out.push('?');
            out.push_str(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            out.push('#');
            out.push_str(&self.fragment);
        }
        out
    }
}

fn parse_host(host: &str) -> Result<String, String> {
    if host.starts_with('[') {
        let Some(end) = host.rfind(']') else {
            return Err("missing ']' in host".to_string());
        };
        let colon_port = &host[end + 1..];
        if !valid_optional_port(colon_port) {
            return Err(format!("invalid port {} after host", quote(colon_port)));
        }
    } else if let Some(i) = host.rfind(':') {
        let colon_port = &host[i..];
        if !valid_optional_port(colon_port) {
            return Err(format!("invalid port {} after host", quote(colon_port)));
        }
    }
    unescape(host, Mode::Host)
}

/// Go `splitHostPort` (as used by `Hostname`/`Port`).
fn split_host_port(host: &str) -> (&str, &str) {
    let mut hostname = host;
    let mut port = "";
    if let Some(colon) = host.rfind(':')
        && valid_optional_port(&host[colon..]) {
            hostname = &host[..colon];
            port = &host[colon + 1..];
        }
    if hostname.starts_with('[') && hostname.ends_with(']') {
        hostname = &hostname[1..hostname.len() - 1];
    }
    (hostname, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_go() {
        let url = GoUrl::parse("https://user:pw@Example.com:8443/a%20b/c?x=1#frag").expect("parse");
        assert_eq!(url.scheme, "https");
        assert!(url.has_user);
        assert_eq!(url.host, "Example.com:8443");
        assert_eq!(url.hostname(), "Example.com");
        assert_eq!(url.port(), "8443");
        assert_eq!(url.path, "/a b/c");
        assert_eq!(url.escaped_path(), "/a%20b/c");
        assert_eq!(url.raw_query, "x=1");
        assert!(GoUrl::parse("downloads.example/x").expect("parse").scheme.is_empty());
        assert!(GoUrl::parse("https://host:port/").is_err());
        assert!(GoUrl::parse("https://h/%zz").is_err());
        assert!(GoUrl::parse("https://h/\u{1}").is_err());
    }
}
