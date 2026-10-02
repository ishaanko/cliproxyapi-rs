//! Proxy-aware reqwest clients with connection reuse (Go: helps/proxy_helpers.go,
//! transport_cache.go and the sdk/proxyutil settings they use).
//!
//! Proxy priority: execution-scoped override (`Options::proxy_url`) > `Auth::proxy_url` >
//! `Config::proxy_url`. An empty setting inherits the environment (like Go's default transport),
//! `direct` / `none` bypasses every proxy, and `socks5`, `socks5h`, `http`, `https` URLs select a
//! proxy. An invalid setting is logged and falls back to the default (inherit) client.
//!
//! Go keeps one `http.Transport` (one connection pool) per proxy URL in a bounded LRU; a reqwest
//! `Client` owns its pool and clones share it, so [`TransportCache`] caches clients under a key
//! of proxy setting, timeout and compression mode. No uTLS: TLS fingerprinting is not available
//! with rustls.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::LazyLock;
use std::time::Duration;

use cpa_auth::Auth;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::Config;
use parking_lot::Mutex;

/// Bounds how many clients a [`TransportCache`] keeps alive; every cached client owns an
/// independent connection pool, so unbounded keys would let idle sockets grow without limit.
pub const DEFAULT_TRANSPORT_CACHE_CAPACITY: usize = 64;

/// A bounded LRU keyed by `K`. Evicted entries are dropped: their idle connections close once
/// no in-flight request still holds a clone.
pub struct BoundedLru<K, V> {
    inner: Mutex<LruState<K, V>>,
}

struct LruState<K, V> {
    capacity: usize,
    /// Most recently used key at the back.
    order: VecDeque<K>,
    items: HashMap<K, V>,
}

impl<K: Hash + Eq + Clone, V: Clone> BoundedLru<K, V> {
    /// A non-positive capacity falls back to [`DEFAULT_TRANSPORT_CACHE_CAPACITY`].
    pub fn new(capacity: usize) -> Self {
        let capacity = if capacity == 0 { DEFAULT_TRANSPORT_CACHE_CAPACITY } else { capacity };
        Self {
            inner: Mutex::new(LruState { capacity, order: VecDeque::new(), items: HashMap::new() }),
        }
    }

    /// Returns the value cached under `key`, calling `build` on first use. A build error is
    /// propagated and never cached. `build` must not call back into this cache.
    pub fn get_or_build<E>(&self, key: K, build: impl FnOnce() -> Result<V, E>) -> Result<V, E> {
        let mut state = self.inner.lock();
        if let Some(value) = state.items.get(&key).cloned() {
            state.touch(&key);
            return Ok(value);
        }
        let value = build()?;
        state.items.insert(key.clone(), value.clone());
        state.order.push_back(key);
        while state.order.len() > state.capacity {
            if let Some(oldest) = state.order.pop_front() {
                state.items.remove(&oldest);
            }
        }
        Ok(value)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, key: &K) -> bool {
        self.inner.lock().items.contains_key(key)
    }

    /// Removes the entry under `key`; true when it existed.
    pub fn close_key(&self, key: &K) -> bool {
        let mut state = self.inner.lock();
        if state.items.remove(key).is_none() {
            return false;
        }
        state.order.retain(|k| k != key);
        true
    }

    /// Removes every entry whose key satisfies `predicate`; returns how many were removed.
    pub fn close_matching(&self, predicate: impl Fn(&K) -> bool) -> usize {
        let mut state = self.inner.lock();
        let doomed: Vec<K> = state.order.iter().filter(|k| predicate(k)).cloned().collect();
        for key in &doomed {
            state.items.remove(key);
        }
        state.order.retain(|k| !doomed.contains(k));
        doomed.len()
    }

    /// Drops every entry.
    pub fn purge(&self) {
        let mut state = self.inner.lock();
        state.items.clear();
        state.order.clear();
    }
}

impl<K: Hash + Eq + Clone, V> LruState<K, V> {
    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key)
            && let Some(k) = self.order.remove(pos) {
                self.order.push_back(k);
            }
    }
}

/// Client cache (Go: TransportCache).
pub type TransportCache<K> = BoundedLru<K, reqwest::Client>;

/// The proxy setting that applies to an execution: request override, else the auth's, else the
/// global one (all trimmed); "" when none is configured.
pub fn effective_proxy_url(request_proxy: &str, auth: Option<&Auth>, cfg: Option<&Config>) -> String {
    let request_proxy = request_proxy.trim();
    if !request_proxy.is_empty() {
        return request_proxy.to_string();
    }
    if let Some(auth) = auth {
        let proxy = auth.proxy_url.trim();
        if !proxy.is_empty() {
            return proxy.to_string();
        }
    }
    cfg.map(|c| c.proxy_url.trim().to_string()).unwrap_or_default()
}

/// [`effective_proxy_url`] parsed into inherit / direct / proxy (Go: proxyutil.Parse); used by
/// executors that dial themselves (websocket upstreams).
pub fn effective_proxy_setting(
    request_proxy: &str,
    auth: Option<&Auth>,
    cfg: Option<&Config>,
) -> Result<ProxySetting, String> {
    parse_proxy(&effective_proxy_url(request_proxy, auth, cfg))
}

/// Cache identity of a built client.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    /// Normalized proxy setting: "" inherit, "direct", or the proxy URL.
    proxy: String,
    timeout_ms: u64,
    no_compression: bool,
}

static CLIENTS: LazyLock<TransportCache<ClientKey>> =
    LazyLock::new(|| TransportCache::new(DEFAULT_TRANSPORT_CACHE_CAPACITY));

/// Builds the reqwest client for a proxy setting (Go: buildProxyTransport + default transport
/// settings). socks5 URLs resolve host names on the proxy side, as Go's SOCKS5 dialer does.
fn build_client(
    setting: &ProxySetting,
    timeout: Option<Duration>,
    no_compression: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90));
    // Go's transport requests and decodes gzip only (unless compression is disabled).
    builder = builder.no_brotli().no_deflate();
    if no_compression {
        builder = builder.no_gzip();
    }
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    match setting {
        ProxySetting::Inherit => {}
        ProxySetting::Direct => builder = builder.no_proxy(),
        ProxySetting::Proxy(url) => {
            let url = match url.strip_prefix("socks5://") {
                Some(rest) => format!("socks5h://{rest}"),
                None => url.clone(),
            };
            builder = builder.proxy(reqwest::Proxy::all(url)?);
        }
    }
    builder.build()
}

fn cached_client(proxy_url: &str, timeout: Option<Duration>, no_compression: bool) -> reqwest::Client {
    let (setting, key_proxy) = match parse_proxy(proxy_url) {
        Ok(ProxySetting::Inherit) => (ProxySetting::Inherit, String::new()),
        Ok(ProxySetting::Direct) => (ProxySetting::Direct, "direct".to_string()),
        Ok(ProxySetting::Proxy(p)) => (ProxySetting::Proxy(p.clone()), p),
        Err(err) => {
            tracing::debug!("failed to setup proxy from URL: {}, falling back to default transport ({err})", redact(proxy_url));
            (ProxySetting::Inherit, String::new())
        }
    };
    let key = ClientKey {
        proxy: key_proxy,
        timeout_ms: timeout.map_or(0, |t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        no_compression,
    };
    let built = CLIENTS.get_or_build(key, || build_client(&setting, timeout, no_compression));
    match built {
        Ok(client) => client,
        Err(err) => {
            tracing::error!("failed to build http client: {err}");
            reqwest::Client::new()
        }
    }
}

/// Masks proxy credentials for logging (`scheme://***@host`).
fn redact(raw: &str) -> String {
    match url::Url::parse(raw.trim()) {
        Ok(mut u) => {
            if !u.username().is_empty() || u.password().is_some() {
                let _ = u.set_username("***");
                let _ = u.set_password(None);
            }
            u.to_string()
        }
        Err(_) => "<unparseable proxy url>".to_string(),
    }
}

/// HTTP client honoring the proxy priority described in the module docs (Go:
/// NewProxyAwareHTTPClient). `timeout` of `None` means no timeout (streaming). Clients are cached
/// per (proxy, timeout), so repeated calls reuse connections.
pub fn new_proxy_aware_http_client(
    request_proxy: &str,
    cfg: Option<&Config>,
    auth: Option<&Auth>,
    timeout: Option<Duration>,
) -> reqwest::Client {
    cached_client(&effective_proxy_url(request_proxy, auth, cfg), timeout, false)
}

/// Client for the Devin Connect-RPC upstream (Go: NewDevinHTTPClient): same proxy priority, but
/// automatic response decompression is off so no `Accept-Encoding` is advertised.
pub fn new_devin_http_client(
    request_proxy: &str,
    cfg: Option<&Config>,
    auth: Option<&Auth>,
    timeout: Option<Duration>,
) -> reqwest::Client {
    cached_client(&effective_proxy_url(request_proxy, auth, cfg), timeout, true)
}

/// Drops every cached client whose proxy setting equals `proxy_url`, for example after a
/// credential's proxy was changed. Returns how many were dropped.
pub fn close_cached_clients_for_proxy(proxy_url: &str) -> usize {
    let proxy = proxy_url.trim();
    CLIENTS.close_matching(|k| k.proxy == proxy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with_proxy(proxy: &str) -> Auth {
        let mut auth = Auth::new("a", "claude");
        auth.proxy_url = proxy.into();
        auth
    }

    fn cfg_with_proxy(proxy: &str) -> Config {
        Config { proxy_url: proxy.into(), ..Config::default() }
    }

    #[test]
    fn proxy_priority_request_then_auth_then_global() {
        let cfg = cfg_with_proxy(" http://global.example:8080 ");
        let auth = auth_with_proxy("http://auth.example:8080");
        assert_eq!(
            effective_proxy_url(" http://request.example:8081 ", Some(&auth), Some(&cfg)),
            "http://request.example:8081"
        );
        assert_eq!(effective_proxy_url("", Some(&auth), Some(&cfg)), "http://auth.example:8080");
        assert_eq!(effective_proxy_url("", Some(&auth_with_proxy("  ")), Some(&cfg)), "http://global.example:8080");
        assert_eq!(effective_proxy_url("", None, None), "");
    }

    #[test]
    fn direct_auth_overrides_global_proxy() {
        let cfg = cfg_with_proxy("http://global.example:8080");
        let auth = auth_with_proxy("direct");
        assert_eq!(effective_proxy_setting("", Some(&auth), Some(&cfg)), Ok(ProxySetting::Direct));
        assert_eq!(
            effective_proxy_setting("", None, Some(&cfg)),
            Ok(ProxySetting::Proxy("http://global.example:8080".into()))
        );
        assert!(effective_proxy_setting("ftp://x:1", None, None).is_err());
    }

    #[test]
    fn clients_are_cached_per_setting() {
        // Other tests share the global cache, so assert on this test's own keys only.
        let key = |proxy: &str, no_compression: bool| ClientKey { proxy: proxy.into(), timeout_ms: 0, no_compression };
        let http = key("http://cache-test.example:3128", false);
        let socks_devin = key("socks5://u:p@cache-test.example:1080", true);
        let _ = new_proxy_aware_http_client("http://cache-test.example:3128", None, None, None);
        let _ = new_proxy_aware_http_client("http://cache-test.example:3128", None, None, None);
        let _ = new_devin_http_client("socks5://u:p@cache-test.example:1080", None, None, None);
        // The devin variant is a separate (no-gzip) entry from the plain client.
        assert!(CLIENTS.contains(&http));
        assert!(CLIENTS.contains(&socks_devin));
        assert!(!CLIENTS.contains(&key("socks5://u:p@cache-test.example:1080", false)));
        assert_eq!(close_cached_clients_for_proxy("http://cache-test.example:3128"), 1);
        assert!(!CLIENTS.contains(&http));
        // Invalid proxies fall back to the default client without panicking.
        let _ = new_proxy_aware_http_client("ftp://bad", None, None, None);
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let cache: BoundedLru<u32, u32> = BoundedLru::new(2);
        let build = |v: u32| move || Ok::<_, ()>(v);
        assert_eq!(cache.get_or_build(1, build(10)), Ok(10));
        assert_eq!(cache.get_or_build(2, build(20)), Ok(20));
        assert_eq!(cache.get_or_build(1, build(11)), Ok(10)); // hit, key 1 becomes most recent
        assert_eq!(cache.get_or_build(3, build(30)), Ok(30)); // evicts key 2
        assert!(cache.contains(&1) && cache.contains(&3) && !cache.contains(&2));
        assert_eq!(cache.get_or_build(4, || Err::<u32, _>("boom")), Err("boom"));
        assert!(!cache.contains(&4));
        assert_eq!(cache.close_matching(|k| *k == 3), 1);
        assert_eq!(cache.len(), 1);
    }
}
