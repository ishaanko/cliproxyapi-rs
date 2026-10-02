//! `gin.Context.ClientIP()`: forwarded-for headers are honored only when the immediate peer is a
//! configured trusted proxy (`trusted-proxies`, IPs or CIDRs).

use std::net::IpAddr;

use axum::http::HeaderMap;

fn mask_matches(candidate: &[u8], net: &[u8], prefix: u32) -> bool {
    if candidate.len() != net.len() {
        return false;
    }
    let full = (prefix / 8) as usize;
    if candidate[..full] != net[..full] {
        return false;
    }
    let rem = prefix % 8;
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    (candidate[full] & mask) == (net[full] & mask)
}

fn octets(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.octets().to_vec(),
            None => v6.octets().to_vec(),
        },
    }
}

/// Whether `ip` equals or falls inside one of the trusted entries.
pub fn is_trusted(ip: IpAddr, trusted: &[String]) -> bool {
    let bytes = octets(ip);
    trusted.iter().any(|entry| {
        let entry = entry.trim();
        if let Some((addr, len)) = entry.split_once('/') {
            let (Ok(addr), Ok(len)) = (addr.parse::<IpAddr>(), len.parse::<u32>()) else {
                return false;
            };
            let net = octets(addr);
            len as usize <= net.len() * 8 && mask_matches(&bytes, &net, len)
        } else {
            entry.parse::<IpAddr>().is_ok_and(|a| octets(a) == bytes)
        }
    })
}

fn validate_header(value: &str, trusted: &[String]) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let items: Vec<&str> = value.split(',').collect();
    for i in (0..items.len()).rev() {
        let ip_str = items[i].trim();
        let ip = ip_str.parse::<IpAddr>().ok()?;
        if i == 0 || !is_trusted(ip, trusted) {
            return Some(ip_str.to_string());
        }
    }
    None
}

/// Resolved client address as gin reports it.
pub fn resolve(remote: Option<IpAddr>, headers: &HeaderMap, trusted: &[String]) -> String {
    let Some(remote) = remote else {
        return String::new();
    };
    if is_trusted(remote, trusted) {
        for name in ["x-forwarded-for", "x-real-ip"] {
            let value = headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
            if let Some(ip) = validate_header(value, trusted) {
                return ip;
            }
        }
    }
    remote.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("9.9.9.9"));
        assert_eq!(resolve(Some("1.2.3.4".parse().unwrap()), &h, &[]), "1.2.3.4");
    }

    #[test]
    fn trusted_peer_walks_forwarded_for_from_the_right() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("9.9.9.9, 10.0.0.5, 10.0.0.6"));
        let trusted = vec!["10.0.0.0/24".to_string(), "127.0.0.1".to_string()];
        assert_eq!(resolve(Some("127.0.0.1".parse().unwrap()), &h, &trusted), "9.9.9.9");
    }
}
