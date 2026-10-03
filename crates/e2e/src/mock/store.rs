//! Hermetic plugin store. The server reaches the official registry over HTTPS through a proxy
//! (`proxy-url`); the mock accepts the `CONNECT`, terminates TLS with a leaf signed by the
//! checked-in test CA (which the server is told to trust via `SSL_CERT_FILE`) and serves a
//! fixed catalog. Anything else under the tunnel answers 404, so nothing can reach the network.

use std::convert::Infallible;
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio_rustls::TlsAcceptor;

/// The CA the server must trust to talk to the mock as the store hosts.
pub const CA_PEM: &str = include_str!("../../certs/ca.pem");
const LEAF_CERT: &[u8] = include_bytes!("../../certs/leaf.pem");
const LEAF_KEY: &[u8] = include_bytes!("../../certs/leaf.key");
/// The fixed catalog served as the official registry.
const REGISTRY: &str = include_str!("../../fixtures/plugin-store-registry.json");
const REGISTRY_PATH: &str = "/router-for-me/CLIProxyAPI-Plugins-Store/main/registry.json";

/// TLS acceptor for the tunnel; `None` if the embedded certificate material is unusable.
fn acceptor() -> Option<&'static TlsAcceptor> {
    static ACCEPTOR: OnceLock<Option<TlsAcceptor>> = OnceLock::new();
    ACCEPTOR
        .get_or_init(|| {
            let certs = rustls_pemfile::certs(&mut &*LEAF_CERT).collect::<Result<Vec<_>, _>>().ok()?;
            let key = rustls_pemfile::private_key(&mut &*LEAF_KEY).ok()??;
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .ok()?
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .ok()?;
            Some(TlsAcceptor::from(Arc::new(config)))
        })
        .as_ref()
}

fn reply(status: StatusCode, body: &'static str) -> hyper::Response<Body> {
    let mut resp = hyper::Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    resp
}

/// Answers one request inside the tunnel.
fn store_reply(req: &hyper::Request<hyper::body::Incoming>) -> hyper::Response<Body> {
    if req.method() == hyper::Method::GET && req.uri().path() == REGISTRY_PATH {
        return reply(StatusCode::OK, REGISTRY);
    }
    reply(StatusCode::NOT_FOUND, r#"{"message":"Not Found"}"#)
}

/// Handles a proxy `CONNECT`: acknowledges it, then serves the TLS tunnel in the background.
pub fn connect(req: Request) -> Response {
    let upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        let (Ok(upgraded), Some(acceptor)) = (upgrade.await, acceptor()) else { return };
        let Ok(tls) = acceptor.accept(TokioIo::new(upgraded)).await else { return };
        let svc = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move { Ok::<_, Infallible>(store_reply(&req)) });
        let _ = auto::Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(tls), svc).await;
    });
    StatusCode::OK.into_response()
}
