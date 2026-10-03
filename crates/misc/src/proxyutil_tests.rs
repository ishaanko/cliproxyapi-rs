use super::*;

#[test]
fn parse_modes() {
    for (input, want, err) in [
        ("", Mode::Inherit, false),
        ("direct", Mode::Direct, false),
        ("none", Mode::Direct, false),
        ("http://proxy.example.com:8080", Mode::Proxy, false),
        ("https://proxy.example.com:8443", Mode::Proxy, false),
        ("socks5://proxy.example.com:1080", Mode::Proxy, false),
        ("socks5h://proxy.example.com:1080", Mode::Proxy, false),
        ("bad-value", Mode::Invalid, true),
    ] {
        let (mode, is_err) = match parse(input) {
            Ok(s) => (s.mode, false),
            Err((s, _)) => (s.mode, true),
        };
        assert_eq!((mode, is_err), (want, err), "{input}");
    }
}

#[test]
fn parse_error_does_not_expose_credentials() {
    let (_, err) = parse("http://user:secret%@proxy.example.com:8080").unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("user") && !text.contains("secret"), "{text}");
}

#[test]
fn parse_follows_go_url_rules() {
    assert_eq!(parse("ftp://h:1").unwrap_err().1, ParseError::UnsupportedScheme("ftp".into()));
    assert_eq!(parse("http://").unwrap_err().1, ParseError::MissingSchemeHost);
    assert_eq!(parse("localhost:8080").unwrap_err().1, ParseError::MissingSchemeHost);
    assert_eq!(parse("1.2.3.4:80").unwrap_err().1, ParseError::Parse);
    assert_eq!(parse("http://h:abc").unwrap_err().1, ParseError::Parse);
    // Go accepts an out-of-range port at parse time; ValidRequestProxy rejects it.
    let ok = parse(" HTTP://u:p%40w@Proxy.Example.com:99999/x ").unwrap();
    let url = ok.url.unwrap();
    assert_eq!((url.scheme.as_str(), url.host.as_str(), url.port()), ("http", "Proxy.Example.com:99999", "99999"));
    assert_eq!((url.username.as_deref(), url.password.as_deref()), (Some("u"), Some("p@w")));
}

#[test]
fn valid_request_proxy_checks_host_and_port() {
    assert!(valid_request_proxy("http://proxy.example.com"));
    assert!(valid_request_proxy("socks5://[::1]:1080"));
    assert!(!valid_request_proxy("http://proxy.example.com:0"));
    assert!(!valid_request_proxy("http://proxy.example.com:70000"));
    assert!(!valid_request_proxy("direct"));
    assert!(!valid_request_proxy(""));
    assert!(!valid_request_proxy("bad-value"));
}

#[test]
fn redact_proxy_url() {
    assert_eq!(redact("http://user:pass@proxy.example.com:8080/path?token=secret"), "http://redacted@proxy.example.com:8080");
    assert_eq!(redact("socks5://proxy.example.com:1080"), "socks5://proxy.example.com:1080");
    assert_eq!(redact("bad-value"), "<invalid proxy URL>");
    assert_eq!(redact("  "), "");
}
