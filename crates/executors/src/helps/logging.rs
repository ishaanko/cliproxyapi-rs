//! Upstream request logging helpers (Go: helps/logging_helpers.go). The capture itself lives in
//! `cpa_runtime::apilog` (re-exported here) so `Options` can carry it; this module keeps the
//! URL and error-body helpers.

use cpa_json::J;
pub use cpa_runtime::apilog::*;

/// Converts a websocket URL back to its HTTP handshake URL for logging (`ws` to `http`, `wss`
/// to `https`).
pub fn websocket_upgrade_request_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(mut parsed) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let scheme = match parsed.scheme().to_lowercase().as_str() {
        "ws" => Some("http"),
        "wss" => Some("https"),
        _ => None,
    };
    if let Some(scheme) = scheme {
        let _ = parsed.set_scheme(scheme);
    }
    parsed.to_string()
}

/// One-line summary of an upstream error body: the HTML `<title>` (or `[html body omitted]`),
/// the JSON `error.message`, else the body text.
pub fn summarize_error_body(content_type: &str, body: &[u8]) -> String {
    let mut is_html = content_type.to_lowercase().contains("text/html");
    if !is_html {
        let lowered = super::text::trim_space(body).to_ascii_lowercase();
        is_html = lowered.starts_with(b"<!doctype html") || lowered.starts_with(b"<html");
    }
    if is_html {
        let title = extract_html_title(body);
        return if title.is_empty() { "[html body omitted]".into() } else { title };
    }
    let message = cpa_json::parse(body).g("error.message").str();
    if !message.is_empty() {
        return message;
    }
    String::from_utf8_lossy(body).into_owned()
}

fn extract_html_title(body: &[u8]) -> String {
    let lower = body.to_ascii_lowercase();
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let Some(start) = find(&lower, b"<title") else {
        return String::new();
    };
    let Some(gt) = lower[start..].iter().position(|b| *b == b'>') else {
        return String::new();
    };
    let start = start + gt + 1;
    let Some(end) = find(&lower[start..], b"</title>") else {
        return String::new();
    };
    let title = unescape_html(&String::from_utf8_lossy(&body[start..start + end]));
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The common named entities plus numeric references (enough for page titles).
fn unescape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls() {
        assert_eq!(websocket_upgrade_request_url("wss://h.example/v1/responses?x=1"), "https://h.example/v1/responses?x=1");
        assert_eq!(websocket_upgrade_request_url(" "), "");
    }

    #[test]
    fn error_body_summaries() {
        let html = b"<!DOCTYPE html><html><head><title> Just a\n moment... &amp; wait </title></head></html>";
        assert_eq!(summarize_error_body("text/html", html), "Just a moment... & wait");
        assert_eq!(summarize_error_body("text/html", b"<html></html>"), "[html body omitted]");
        assert_eq!(summarize_error_body("application/json", br#"{"error":{"message":"bad key"}}"#), "bad key");
        assert_eq!(summarize_error_body("text/plain", b"nope"), "nope");
    }
}
