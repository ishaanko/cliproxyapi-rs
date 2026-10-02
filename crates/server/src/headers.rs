//! Upstream response header filtering and passthrough (Go: sdk/api/handlers/header_filter.go).

use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// Header name prefixes injected by AI gateway proxies; stripped so clients cannot detect them.
const GATEWAY_PREFIXES: &[&str] = &["x-litellm-", "helicone-", "x-portkey-", "cf-aig-", "x-kong-", "x-bt-"];

/// RFC 7230 hop-by-hop headers plus security-sensitive and handler-managed ones (lowercase).
const BLOCKED: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
    "content-length",
    "content-encoding",
];

/// Headers owned by CPA (CORS and the trace id); upstream values never reach the client.
const RESERVED: &[&str] = &[
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-allow-origin",
    "access-control-expose-headers",
    "access-control-max-age",
    "x-cpa-trace-id",
];

/// `IsCPAReservedResponseHeader`.
pub fn is_cpa_reserved_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESERVED.contains(&lower.as_str())
}

/// Names listed in the upstream `Connection` header are hop-by-hop too.
fn connection_scoped(src: &HeaderMap) -> Vec<String> {
    src.get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// `FilterUpstreamHeaders`: copy of `src` without hop-by-hop, reserved, connection-scoped and
/// gateway-detection headers.
pub fn filter_upstream_headers(src: &HeaderMap) -> HeaderMap {
    let scoped = connection_scoped(src);
    let mut dst = HeaderMap::new();
    for (name, value) in src {
        let key = name.as_str();
        if BLOCKED.contains(&key) || RESERVED.contains(&key) || scoped.iter().any(|s| s == key) {
            continue;
        }
        if GATEWAY_PREFIXES.iter().any(|p| key.starts_with(p)) {
            continue;
        }
        dst.append(name.clone(), value.clone());
    }
    dst
}

/// `WriteUpstreamHeaders`: adds every value of `src` unless `dst` already has a value for that
/// key (so handler-set `Content-Type` wins).
pub fn write_upstream_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for name in src.keys() {
        if dst.get(name).is_some_and(|v| !v.is_empty()) {
            continue;
        }
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// Replace semantics used by the error paths: existing values for the key are removed first.
pub fn replace_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for name in src.keys() {
        if is_cpa_reserved_response_header(name.as_str()) {
            continue;
        }
        dst.remove(name);
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// Header value from a static string (panics only on invalid constants).
pub fn static_value(value: &'static str) -> HeaderValue {
    HeaderValue::from_static(value)
}

/// Sets a header from runtime text; invalid values are skipped.
pub fn set_header(map: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        map.insert(HeaderName::from_static(name), v);
    }
}

/// `strings.TrimSpace(c.GetHeader(name))`.
pub fn header_trimmed(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn filters_hop_by_hop_reserved_and_gateway_headers() {
        let src = map(&[
            ("Content-Length", "3"),
            ("Connection", "X-Hop, close"),
            ("X-Hop", "1"),
            ("X-Litellm-Cost", "1"),
            ("Helicone-Id", "2"),
            ("Access-Control-Allow-Origin", "evil"),
            ("X-Cpa-Trace-Id", "t"),
            ("Set-Cookie", "a=b"),
            ("X-Request-Id", "keep"),
        ]);
        let out = filter_upstream_headers(&src);
        let names: Vec<_> = out.keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(names, vec!["x-request-id"]);
    }

    #[test]
    fn upstream_headers_do_not_override_existing() {
        let mut dst = map(&[("Content-Type", "application/json")]);
        write_upstream_headers(&mut dst, &map(&[("Content-Type", "text/plain"), ("X-A", "1"), ("X-A", "2")]));
        assert_eq!(dst.get("content-type").unwrap(), "application/json");
        assert_eq!(dst.get_all("x-a").iter().count(), 2);
    }
}
