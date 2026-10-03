//! Listener setup: plain TCP or TLS (`tls.enable`, ALPN h2 + http/1.1), HTTP and the Redis
//! protocol multiplexed on one port (see [`crate::mux`]), and graceful stop (Go: Server.Start /
//! Stop in internal/api/server.go).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::serve::ListenerExt;
use cpa_config::Config;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;

use crate::mux;
use crate::redis_protocol::RedisProtocol;

/// `host:port`, defaulting to port 8317 when unset. An empty host binds every interface
/// (Go's `:port`); `[::]` is dual-stack on Linux. Bare IPv6 hosts are bracketed.
pub fn listen_addr(cfg: &Config) -> String {
    let port = if cfg.port > 0 { cfg.port } else { 8317 };
    let host = cfg.host.trim();
    match host {
        "" => format!("[::]:{port}"),
        h if h.contains(':') && !h.starts_with('[') => format!("[{h}]:{port}"),
        h => format!("{h}:{port}"),
    }
}


fn load_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig, String> {
    let certs = {
        let mut reader = io::BufReader::new(std::fs::File::open(cert_path).map_err(|e| e.to_string())?);
        rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    if certs.is_empty() {
        return Err("tls: failed to find any PEM data in certificate input".into());
    }
    let key = {
        let mut reader = io::BufReader::new(std::fs::File::open(key_path).map_err(|e| e.to_string())?);
        rustls_pemfile::private_key(&mut reader)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "tls: failed to find any PEM data in key input".to_string())?
    };
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}


/// Binds and serves `app` until the future is dropped (Go closes the listener and connections
/// immediately on stop, no drain). Errors are formatted like Go's `failed to start HTTP server`.
pub async fn serve(cfg: &Config, app: Router) -> Result<(), String> {
    serve_with_redis(cfg, app, None).await
}

/// [`serve`] with the Redis-protocol usage output enabled on the same port.
pub async fn serve_with_redis(cfg: &Config, app: Router, redis: Option<Arc<RedisProtocol>>) -> Result<(), String> {
    let addr = listen_addr(cfg);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("failed to start HTTP server: {e}"))?;
    println!("API server started successfully on: {}:{}", cfg.host, cfg.port);
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    let acceptor = if cfg.tls.enable {
        let (cert, key) = (cfg.tls.cert.trim(), cfg.tls.key.trim());
        if cert.is_empty() || key.is_empty() {
            return Err("failed to start HTTPS server: tls.cert or tls.key is empty".into());
        }
        let tls = load_tls_config(cert, key).map_err(|e| format!("failed to start HTTPS server: {e}"))?;
        tracing::debug!("Starting API server on {addr} with TLS");
        Some(TlsAcceptor::from(Arc::new(tls)))
    } else {
        tracing::debug!("Starting API server on {addr}");
        None
    };
    let tls_enabled = acceptor.is_some();
    // `tap_io` lets axum derive `ConnectInfo<SocketAddr>` from the wrapped listener's address.
    let listener = mux::start(listener, acceptor, redis)
        .map_err(|e| format!("failed to start {} server: {e}", if tls_enabled { "HTTPS" } else { "HTTP" }))?
        .tap_io(|_| {});
    axum::serve(listener, service)
        .await
        .map_err(|e| format!("failed to start HTTP server: {e}"))
}
