//! HTTP/1.1 client pools that match the native Antigravity client (Go: the transport half of
//! antigravity_executor.go).
//!
//! The native client negotiates TLS without ALPN and speaks HTTP/1.1 only, one connection pool per
//! OAuth identity. Each credential (plus proxy and pool settings) gets its own `reqwest::Client`
//! from a bounded cache. By default pooling is off (connections close after each request); the
//! `antigravity.connection-pool` config turns it on.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use cpa_auth::Auth;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::{Config, GoDuration};
use sha2::{Digest, Sha256};

use super::AntigravityExecutor;
use crate::helps::proxy::{TransportCache, effective_proxy_url};

/// Pool entries are nearly free, so the cap only stops unbounded growth under key churn.
const TRANSPORT_CACHE_CAPACITY: usize = 8192;
const DEFAULT_MAX_IDLE_CONNS_PER_HOST: i64 = 2;
const MAX_ALLOWED_MAX_IDLE_CONNS_PER_HOST: i64 = 100;
/// Far below the Google frontend's 240 s idle cutoff so a half-closed connection is never reused.
const DEFAULT_IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ALLOWED_IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(210);
const ANONYMOUS_SCOPE: &str = "anonymous";

/// Resolved `antigravity.connection-pool` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PoolSettings {
    /// Close every connection after its request (the default).
    pub short_mode: bool,
    pub idle_conn_timeout: Duration,
    /// Negative means no idle connections are kept.
    pub max_idle_conns_per_host: i64,
}

pub(crate) fn resolve_pool_settings(cfg: &Config) -> PoolSettings {
    let mut s = PoolSettings { short_mode: true, idle_conn_timeout: Duration::ZERO, max_idle_conns_per_host: -1 };
    let pool = &cfg.antigravity.connection_pool;
    if pool.enabled != Some(true) {
        return s;
    }
    s.short_mode = false;
    s.idle_conn_timeout = DEFAULT_IDLE_CONN_TIMEOUT;
    s.max_idle_conns_per_host = DEFAULT_MAX_IDLE_CONNS_PER_HOST;

    let raw_timeout = pool.idle_conn_timeout.trim();
    if !raw_timeout.is_empty() {
        match GoDuration::parse(raw_timeout) {
            Err(err) => tracing::warn!(
                "antigravity executor: invalid idle-conn-timeout {raw_timeout:?}: {err}, using default {DEFAULT_IDLE_CONN_TIMEOUT:?}"
            ),
            Ok(d) => {
                if d.0 <= 0 {
                    s.short_mode = true;
                    s.max_idle_conns_per_host = -1;
                    return s;
                }
                s.idle_conn_timeout = d.to_std().min(MAX_ALLOWED_IDLE_CONN_TIMEOUT);
            }
        }
    }
    if let Some(val) = pool.max_idle_conns_per_host {
        if val < 0 {
            s.short_mode = true;
            s.max_idle_conns_per_host = -1;
            return s;
        }
        s.max_idle_conns_per_host = val.min(MAX_ALLOWED_MAX_IDLE_CONNS_PER_HOST);
    }
    if s.max_idle_conns_per_host < 0 {
        s.short_mode = true;
    }
    s
}

/// Identifies one connection pool. Settings are part of the key so a config reload never reuses a
/// pool built with stale limits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TransportKey {
    credential: String,
    proxy: String,
    short_mode: bool,
    idle_timeout_ms: u64,
    max_idle: i64,
}

static TRANSPORTS: LazyLock<TransportCache<TransportKey>> =
    LazyLock::new(|| TransportCache::new(TRANSPORT_CACHE_CAPACITY));

/// Number of cached pools.
#[allow(dead_code)]
pub(crate) fn transports_len() -> usize {
    TRANSPORTS.len()
}

/// Drops every cached pool (config hot reload).
#[allow(dead_code)]
pub(crate) fn reset_transports() {
    TRANSPORTS.purge();
}

fn credential_scope_digest(prefix: &str, secret: &str) -> String {
    format!("{prefix}{}", hex::encode(&Sha256::digest(secret.as_bytes())[..8]))
}

/// Pool scope of one credential: id, source path, then a digest of the refresh (stable across
/// rotation) or access token. Labels are deliberately not used: they carry no uniqueness.
pub(crate) fn transport_scope(auth: &Auth) -> String {
    let id = auth.id.trim();
    if !id.is_empty() {
        return format!("id:{id}");
    }
    let path = auth.attr("path");
    if !path.trim().is_empty() {
        return format!("path:{}", path.trim());
    }
    let source = auth.attr("source");
    if !source.trim().is_empty() {
        return format!("source:{}", source.trim());
    }
    let meta = |k: &str| super::auth::meta_string(auth, k);
    let refresh = meta("refresh_token");
    if !refresh.is_empty() {
        return credential_scope_digest("refresh:", &refresh);
    }
    let access = meta("access_token");
    if !access.is_empty() {
        return credential_scope_digest("token:", &access);
    }
    ANONYMOUS_SCOPE.to_string()
}

/// Closes the idle connections of the credential's pools (after a 429 or cooldown decision).
pub(crate) fn close_auth_idle_transports(auth: &Auth) {
    let scope = transport_scope(auth);
    if scope == ANONYMOUS_SCOPE {
        return;
    }
    TRANSPORTS.close_matching(|k| k.credential == scope);
}

/// rustls config with no ALPN protocols, so the handshake matches the native client.
fn tls_config() -> Option<Arc<rustls::ClientConfig>> {
    static CONFIG: LazyLock<Option<Arc<rustls::ClientConfig>>> = LazyLock::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = Vec::new();
        Some(Arc::new(config))
    });
    CONFIG.clone()
}

fn build_client(setting: &ProxySetting, pool: PoolSettings) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder();
    if let Some(tls) = tls_config() {
        builder = builder.use_preconfigured_tls((*tls).clone());
    }
    builder = builder
        .http1_only()
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        // Go's transport asks for gzip only.
        .no_brotli()
        .no_deflate();
    if pool.short_mode || pool.max_idle_conns_per_host <= 0 {
        builder = builder.pool_max_idle_per_host(0);
    } else {
        builder = builder
            .pool_max_idle_per_host(pool.max_idle_conns_per_host as usize)
            .pool_idle_timeout(pool.idle_conn_timeout);
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

impl AntigravityExecutor {
    /// The credential's HTTP/1.1 client. Proxy precedence: request override, credential, global
    /// config. An invalid proxy setting falls back to a direct pool.
    pub(crate) fn client(&self, cfg: &Config, auth: &Auth, request_proxy: &str) -> reqwest::Client {
        let proxy_url = effective_proxy_url(request_proxy, Some(auth), Some(cfg));
        let (setting, key_proxy) = match parse_proxy(&proxy_url) {
            Ok(ProxySetting::Inherit) => (ProxySetting::Inherit, String::new()),
            Ok(ProxySetting::Direct) => (ProxySetting::Direct, "direct".to_string()),
            Ok(ProxySetting::Proxy(p)) => (ProxySetting::Proxy(p.clone()), p),
            Err(err) => {
                tracing::debug!("antigravity executor: invalid proxy setting, using direct pool ({err})");
                (ProxySetting::Inherit, String::new())
            }
        };
        let pool = resolve_pool_settings(cfg);
        let key = TransportKey {
            credential: transport_scope(auth),
            proxy: key_proxy,
            short_mode: pool.short_mode,
            idle_timeout_ms: u64::try_from(pool.idle_conn_timeout.as_millis()).unwrap_or(u64::MAX),
            max_idle: pool.max_idle_conns_per_host,
        };
        match TRANSPORTS.get_or_build(key, || build_client(&setting, pool)) {
            Ok(client) => client,
            Err(err) => {
                tracing::error!("antigravity executor: failed to build http client: {err}");
                reqwest::Client::new()
            }
        }
    }
}
