//! Example-API-key safe mode (Go: internal/safemode). While `api-keys` still holds template
//! values the proxy endpoints answer 403 and `/` + `/management.html` show a setup warning.

const EXAMPLE_API_KEYS: &[&str] = &["your-api-key-1", "your-api-key-2", "your-api-key-3"];

/// `ExampleAPIKeys`: configured keys that are still template values (de-duplicated).
pub fn example_api_keys(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in keys {
        let trimmed = key.trim();
        if EXAMPLE_API_KEYS.contains(&trimmed) && !out.iter().any(|k| k == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// `HasExampleAPIKeys`.
pub fn has_example_api_keys(keys: &[String]) -> bool {
    !example_api_keys(keys).is_empty()
}

/// `html.EscapeString`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
}

/// `ExampleAPIKeyWarningPageHTML`.
pub fn warning_page_html(keys: &[String], management_path: &str) -> String {
    let mut b = String::new();
    b.push_str(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Example API key detected</title><style>body{margin:0;font-family:Arial,sans-serif;background:#f6f8fa;color:#1f2328}.wrap{max-width:760px;margin:12vh auto;padding:0 24px}.panel{background:#fff;border:1px solid #d0d7de;border-radius:8px;padding:28px;box-shadow:0 8px 24px rgba(140,149,159,.2)}h1{margin:0 0 12px;font-size:28px;line-height:1.25}p{font-size:16px;line-height:1.55}code{background:#f6f8fa;border:1px solid #d0d7de;border-radius:4px;padding:2px 5px}.keys{margin:16px 0;padding-left:22px}.actions{margin-top:24px}.button{display:inline-block;border-radius:6px;background:#0969da;color:#fff;text-decoration:none;font-weight:600;padding:10px 16px}.button:hover{background:#0759b8}</style></head><body><main class="wrap"><section class="panel"><h1>Example API key detected</h1><p>Proxy API endpoints are disabled because the top-level <code>api-keys</code> configuration still contains template values.</p>"#);
    if !keys.is_empty() {
        b.push_str(r#"<p>Replace these values before using the proxy:</p><ul class="keys">"#);
        for key in keys {
            b.push_str("<li><code>");
            b.push_str(&html_escape(key));
            b.push_str("</code></li>");
        }
        b.push_str("</ul>");
    }
    b.push_str("<p>Set strong random API keys, then retry the proxy endpoint.</p>");
    let trimmed = management_path.trim();
    if !trimmed.is_empty() {
        b.push_str(r#"<div class="actions"><a class="button" href=""#);
        b.push_str(&html_escape(trimmed));
        b.push_str(r#"">Open Management</a></div>"#);
    }
    b.push_str("</section></main></body></html>");
    b
}

/// `isExampleAPIKeySafeModeProxyPath`.
pub fn is_proxy_path(path: &str) -> bool {
    ["/v1", "/v1beta", "/openai/v1", "/backend-api/codex"]
        .iter()
        .any(|p| path == *p || path.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_template_keys() {
        let keys = vec![" your-api-key-1 ".to_string(), "real".to_string(), "your-api-key-1".to_string()];
        assert_eq!(example_api_keys(&keys), vec!["your-api-key-1"]);
        assert!(!has_example_api_keys(&["real".to_string()]));
    }

    #[test]
    fn proxy_paths() {
        for p in ["/v1", "/v1/models", "/v1beta/models/x", "/openai/v1/videos", "/backend-api/codex/responses"] {
            assert!(is_proxy_path(p), "{p}");
        }
        for p in ["/", "/healthz", "/v10", "/management.html", "/v1x/models"] {
            assert!(!is_proxy_path(p), "{p}");
        }
    }

    #[test]
    fn warning_page_escapes_keys() {
        let html = warning_page_html(&["<k>".into()], "/management.html?safe-mode=configure");
        assert!(html.contains("<li><code>&lt;k&gt;</code></li>"));
        assert!(html.contains(r#"href="/management.html?safe-mode=configure""#));
    }
}
