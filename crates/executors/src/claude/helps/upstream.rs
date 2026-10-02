//! First-party Anthropic upstream gate (Go: helps/claude_upstream.go and `isAnthropicUpstreamBase`
//! in executor/claude_executor_request.go).

use url::Url;

/// Whether a resolved request targets Anthropic's first-party API origin (Go:
/// `IsAnthropicUpstreamURL`): https, host `api.anthropic.com`, port empty or 443, no userinfo.
/// Every Claude-specific body, header, HTTP and TLS rule must key on this.
pub fn is_anthropic_upstream_url(u: Option<&Url>) -> bool {
    let Some(u) = u else { return false };
    if !u.scheme().eq_ignore_ascii_case("https") || !u.host_str().is_some_and(|h| h.eq_ignore_ascii_case("api.anthropic.com")) {
        return false;
    }
    // The url crate drops default ports and empty userinfo; inspect the authority text as Go does.
    let authority = authority_text(u.as_str());
    if authority.contains('@') {
        return false;
    }
    match authority.rsplit_once(':') {
        Some((_, port)) => port.is_empty() || port == "443",
        None => true,
    }
}

/// Text between `scheme://` and the first `/`, `?` or `#` of a serialized URL.
fn authority_text(serialized: &str) -> &str {
    let rest = serialized.split_once("://").map_or(serialized, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// Whether a configured base URL targets Anthropic's first-party API (Go: `isAnthropicUpstreamBase`).
/// Used before the outgoing request exists; unparsable input is not first-party.
pub fn is_anthropic_upstream_base(base_url: &str) -> bool {
    let base_url = base_url.trim();
    // An empty userinfo (`https://@host`) disappears when the url crate re-serializes.
    if authority_text(base_url).contains('@') {
        return false;
    }
    match Url::parse(base_url) {
        Ok(parsed) => is_anthropic_upstream_url(Some(&parsed)),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_url_gate() {
        let cases = [
            ("https://api.anthropic.com/v1/messages", true),
            ("https://api.anthropic.com:443/v1/messages", true),
            ("https://API.ANTHROPIC.COM/v1/messages", true),
            ("http://api.anthropic.com/v1/messages", false),
            ("https://api.anthropic.com:8443/v1/messages", false),
            ("https://caller@api.anthropic.com/v1/messages", false),
            ("https://@api.anthropic.com/v1/messages", false),
            ("https://api.anthropic.com.example/v1/messages", false),
        ];
        for (target, want) in cases {
            let parsed = Url::parse(target).expect("test url parses");
            // `https://@host` is only detectable from the raw string (base check).
            if !target.contains("//@") {
                assert_eq!(is_anthropic_upstream_url(Some(&parsed)), want, "{target}");
            }
            assert_eq!(is_anthropic_upstream_base(target), want, "{target}");
        }
        assert!(!is_anthropic_upstream_url(None));
        assert!(!is_anthropic_upstream_base("not a url"));
        assert!(is_anthropic_upstream_base("  https://api.anthropic.com  "));
    }
}
