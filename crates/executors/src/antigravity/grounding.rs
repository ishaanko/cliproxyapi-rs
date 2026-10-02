//! Grounding URL resolution for native web search (Go: helps/antigravity_grounding_urls.go and
//! the `shouldResolveAntigravityWebSearchGroundingURLs` family in antigravity_executor.go).
//!
//! Search grounding chunks point at `vertexaisearch.cloud.google.com/grounding-api-redirect/...`
//! redirects. When the client used a typed web search tool, each redirect is resolved with a
//! HEAD request (no redirect following) and the `Location` target replaces it.

use std::collections::HashMap;
use std::sync::LazyLock;

use cpa_json::J;
use cpa_translator::Format;
use parking_lot::Mutex;

/// Whether `raw` is a Vertex Search redirect URL.
fn is_vertex_search_redirect(raw: &str) -> bool {
    match url::Url::parse(raw) {
        Ok(u) => {
            u.scheme() == "https"
                && u.host_str() == Some("vertexaisearch.cloud.google.com")
                && u.path().starts_with("/grounding-api-redirect/")
        }
        Err(_) => false,
    }
}

static NO_REDIRECT_CLIENTS: LazyLock<Mutex<HashMap<String, reqwest::Client>>> =
    LazyLock::new(Default::default);

/// Proxy-aware client that never follows redirects, cached per proxy setting.
fn no_redirect_client(proxy_url: &str) -> reqwest::Client {
    let mut cache = NO_REDIRECT_CLIENTS.lock();
    if let Some(c) = cache.get(proxy_url) {
        return c.clone();
    }
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none());
    match cpa_auth::http::parse_proxy(proxy_url) {
        Ok(cpa_auth::http::ProxySetting::Direct) => builder = builder.no_proxy(),
        Ok(cpa_auth::http::ProxySetting::Proxy(p)) => {
            let p = match p.strip_prefix("socks5://") {
                Some(rest) => format!("socks5h://{rest}"),
                None => p,
            };
            if let Ok(proxy) = reqwest::Proxy::all(p) {
                builder = builder.proxy(proxy);
            }
        }
        _ => {}
    }
    let client = builder.build().unwrap_or_default();
    cache.insert(proxy_url.to_string(), client.clone());
    client
}

async fn resolve_one(proxy_url: &str, raw: &str) -> String {
    if !is_vertex_search_redirect(raw) {
        return raw.to_string();
    }
    let resp = match no_redirect_client(proxy_url).head(raw).send().await {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!("antigravity grounding url: resolve redirect failed: {}", err.without_url());
            return raw.to_string();
        }
    };
    let status = resp.status().as_u16();
    if !(300..400).contains(&status) {
        return raw.to_string();
    }
    let location = resp
        .headers()
        .get(http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    if location.is_empty() {
        return raw.to_string();
    }
    match url::Url::parse(location) {
        Ok(u) if u.scheme() == "https" && u.host_str().is_some_and(|h| !h.is_empty()) => location.to_string(),
        _ => raw.to_string(),
    }
}

/// Replaces redirect URLs in `groundingChunks[].web.uri` (envelope or bare candidates).
pub async fn resolve_grounding_urls(proxy_url: &str, payload: Vec<u8>) -> Vec<u8> {
    if payload.is_empty() {
        return payload;
    }
    let mut v = cpa_json::parse(&payload);
    let mut base = "response.candidates.0.groundingMetadata.groundingChunks";
    if !v.g(base).is_array() {
        base = "candidates.0.groundingMetadata.groundingChunks";
    }
    let chunks = v.g(base);
    if !chunks.is_array() {
        return payload;
    }
    let uris: Vec<String> = chunks.array().iter().map(|c| c.g("web.uri").str().trim().to_string()).collect();
    drop(chunks);
    let mut resolved: HashMap<String, String> = HashMap::new();
    let mut changed = false;
    for (i, uri) in uris.iter().enumerate() {
        if uri.is_empty() {
            continue;
        }
        let target = match resolved.get(uri) {
            Some(t) => t.clone(),
            None => {
                let t = resolve_one(proxy_url, uri).await;
                resolved.insert(uri.clone(), t.clone());
                t
            }
        };
        if &target == uri {
            continue;
        }
        cpa_json::set(&mut v, &format!("{base}.{i}.web.uri"), target);
        changed = true;
    }
    if changed { cpa_json::to_vec(&v) } else { payload }
}

fn has_claude_typed_web_search_tool(payload: &[u8]) -> bool {
    let v = cpa_json::parse(payload);
    let tools = v.g("tools");
    tools.is_array()
        && tools
            .array()
            .iter()
            .any(|t| matches!(t.g("type").str().as_str(), "web_search_20250305" | "web_search_20260209"))
}

fn has_google_search_tool(payload: &[u8]) -> bool {
    let v = cpa_json::parse(payload);
    let tools = v.g("request.tools");
    tools.is_array() && tools.array().iter().any(|t| t.g("googleSearch").exists())
}

fn has_responses_web_search_tool(payload: &[u8]) -> bool {
    let v = cpa_json::parse(payload);
    let tools = v.g("tools");
    tools.is_array()
        && tools.array().iter().any(|t| {
            matches!(
                t.g("type").str().as_str(),
                "web_search" | "web_search_2025_08_26" | "web_search_preview" | "web_search_preview_2025_03_11"
            )
        })
}

/// Whether response grounding URLs should be resolved: the upstream request carries a native
/// `googleSearch` tool and the client asked for a typed web search tool.
pub fn should_resolve_grounding_urls(from: Format, original_request: &[u8], request: &[u8]) -> bool {
    if !has_google_search_tool(request) {
        return false;
    }
    match from {
        Format::Claude => has_claude_typed_web_search_tool(original_request),
        Format::OpenAIResponse => has_responses_web_search_tool(original_request),
        _ => false,
    }
}
