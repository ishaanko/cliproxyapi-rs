//! OAuth helpers shared by every flow (internal/misc/oauth.go, management `ValidateOAuthState`).

use crate::util::random_hex;

/// `GenerateRandomState`: 16 random bytes as 32 hex chars.
pub fn generate_state() -> String {
    random_hex(16)
}

/// `ValidateOAuthState`: trimmed, non-empty, at most 128 chars of `[A-Za-z0-9_.-]`, no `..`.
pub fn validate_oauth_state(state: &str) -> Result<(), &'static str> {
    let s = state.trim();
    if s.is_empty() {
        return Err("empty state");
    }
    if s.len() > 128 {
        return Err("state too long");
    }
    if s.contains("..") || s.contains('/') || s.contains('\\') {
        return Err("invalid state");
    }
    if !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')) {
        return Err("invalid state");
    }
    Ok(())
}

fn query_get(u: &url::Url, key: &str) -> String {
    u.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.trim().to_string()).unwrap_or_default()
}

/// Parsed OAuth redirect parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub error: String,
    pub error_description: String,
}

/// `ParseOAuthCallback`: accepts a full URL, `?query`, `host:port/path?query`, or a bare `k=v`
/// string; reads `code`, `state`, `error`, `error_description` from the query then the fragment;
/// supports the Claude `code#state` paste form. `Ok(None)` for empty input.
pub fn parse_oauth_callback(input: &str) -> Result<Option<OAuthCallback>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.starts_with('?') {
        format!("http://localhost{trimmed}")
    } else if trimmed.contains(['/', '?', '#']) || trimmed.contains(':') {
        format!("http://{trimmed}")
    } else if trimmed.contains('=') {
        format!("http://localhost/?{trimmed}")
    } else {
        return Err("invalid callback URL".into());
    };

    let parsed = url::Url::parse(&candidate).map_err(|e| e.to_string())?;
    let mut code = query_get(&parsed, "code");
    let mut state = query_get(&parsed, "state");
    let mut err_code = query_get(&parsed, "error");
    let mut err_desc = query_get(&parsed, "error_description");

    if let Some(fragment) = parsed.fragment().filter(|f| !f.is_empty()) {
        let frag_get = |key: &str| {
            url::form_urlencoded::parse(fragment.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        if code.is_empty() {
            code = frag_get("code");
        }
        if state.is_empty() {
            state = frag_get("state");
        }
        if err_code.is_empty() {
            err_code = frag_get("error");
        }
        if err_desc.is_empty() {
            err_desc = frag_get("error_description");
        }
    }

    if !code.is_empty() && state.is_empty() {
        if let Some((c, s)) = code.split_once('#') {
            let (c, s) = (c.to_string(), s.to_string());
            code = c;
            state = s;
        }
    }

    if err_code.is_empty() && !err_desc.is_empty() {
        err_code = std::mem::take(&mut err_desc);
    }

    if code.is_empty() && err_code.is_empty() {
        return Err("callback URL missing code".into());
    }

    Ok(Some(OAuthCallback { code, state, error: err_code, error_description: err_desc }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_query_fragment_and_paste_forms() {
        let p = parse_oauth_callback("http://localhost:54545/callback?code=abc&state=xyz").unwrap().unwrap();
        assert_eq!((p.code.as_str(), p.state.as_str()), ("abc", "xyz"));

        let p = parse_oauth_callback("?code=abc%2B1&state=s").unwrap().unwrap();
        assert_eq!(p.code, "abc+1");

        let p = parse_oauth_callback("localhost:1455/auth/callback#code=c1&state=s1").unwrap().unwrap();
        assert_eq!((p.code.as_str(), p.state.as_str()), ("c1", "s1"));

        let p = parse_oauth_callback("code=c2&state=s2").unwrap().unwrap();
        assert_eq!(p.code, "c2");

        // Claude "code#state" paste form.
        let p = parse_oauth_callback("http://x/cb?code=thecode%23thestate").unwrap().unwrap();
        assert_eq!((p.code.as_str(), p.state.as_str()), ("thecode", "thestate"));
    }

    #[test]
    fn error_description_promoted_when_error_missing() {
        let p = parse_oauth_callback("?error_description=denied").unwrap().unwrap();
        assert_eq!((p.error.as_str(), p.error_description.as_str()), ("denied", ""));
        let p = parse_oauth_callback("?error=access_denied&error_description=nope").unwrap().unwrap();
        assert_eq!((p.error.as_str(), p.error_description.as_str()), ("access_denied", "nope"));
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert_eq!(parse_oauth_callback("   ").unwrap(), None);
        assert!(parse_oauth_callback("justtext").is_err());
        assert!(parse_oauth_callback("http://x/cb?foo=bar").is_err());
    }

    #[test]
    fn state_rules() {
        assert!(validate_oauth_state(&generate_state()).is_ok());
        assert!(validate_oauth_state("").is_err());
        assert!(validate_oauth_state("a/b").is_err());
        assert!(validate_oauth_state("a..b").is_err());
        assert!(validate_oauth_state(&"a".repeat(129)).is_err());
        assert_eq!(generate_state().len(), 32);
    }
}
