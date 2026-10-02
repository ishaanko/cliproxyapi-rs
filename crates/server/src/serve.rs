//! Listener setup: plain TCP or TLS (`tls.enable`, ALPN h2 + http/1.1) and graceful stop
//! (Go: Server.Start / Stop in internal/api/server.go; the RESP multiplexer is not ported).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::serve::{Listener, ListenerExt};
use cpa_config::Config;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;

/// `host:port`, defaulting to port 8317 when unset.
pub fn listen_addr(cfg: &Config) -> String {
    let port = if cfg.port > 0 { cfg.port } else { 8317 };
    format!("{}:{}", cfg.host, port)
}

/// TLS listener: handshakes inside `accept` (bounded) and yields decrypted streams.
struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (tcp, addr) = match self.inner.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            match tokio::time::timeout(Duration::from_secs(10), self.acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => return (tls, addr),
                Ok(Err(e)) => tracing::debug!("tls handshake with {addr} failed: {e}"),
                Err(_) => tracing::debug!("tls handshake with {addr} timed out"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
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
    let addr = listen_addr(cfg);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("failed to start HTTP server: {e}"))?;
    println!("API server started successfully on: {addr}");
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    if cfg.tls.enable {
        let (cert, key) = (cfg.tls.cert.trim(), cfg.tls.key.trim());
        if cert.is_empty() || key.is_empty() {
            return Err("failed to start HTTPS server: tls.cert or tls.key is empty".into());
        }
        let tls = load_tls_config(cert, key).map_err(|e| format!("failed to start HTTPS server: {e}"))?;
        tracing::debug!("Starting API server on {addr} with TLS");
        // `tap_io` lets axum derive `ConnectInfo<SocketAddr>` from the wrapped listener's address.
        let listener = TlsListener {
            inner: listener,
            acceptor: TlsAcceptor::from(Arc::new(tls)),
        }
        .tap_io(|_| {});
        axum::serve(listener, service)
            .await
            .map_err(|e| format!("failed to start HTTP server: {e}"))
    } else {
        tracing::debug!("Starting API server on {addr}");
        axum::serve(listener, service)
            .await
            .map_err(|e| format!("failed to start HTTP server: {e}"))
    }
}
