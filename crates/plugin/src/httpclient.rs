//! HTTP client handed to plugins through `host.http.*` (Go: `http_bridge.go`).
//!
//! Requests use the host's proxy-aware transport. A wire profile (HTTP/1.1 only, no automatic
//! compression, header order) selects a dedicated client; header-name order is not reproducible
//! with reqwest, so a header profile implies HTTP/1.1 only.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_executors::helps::proxy::{ProxySetting, effective_proxy_setting, new_proxy_aware_http_client};
use cpa_pluginapi::api::{HttpRequest, HttpResponse, HttpWireProfile};
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue, Method};
use tokio::sync::mpsc;

use crate::bridge::HttpChunk;
use crate::convert::headers_to_go;
use crate::ctx::CallCtx;
use crate::error::HostError;

/// Result of a streaming host HTTP call.
pub struct HttpStream {
    pub status: u16,
    pub headers: cpa_pluginapi::api::Header,
    pub chunks: mpsc::Receiver<HttpChunk>,
}

/// Go `hostHTTPClient`: bound to an optional auth (proxy and log attribution) and provider.
pub struct HostHttpClient {
    pub cfg: Option<Arc<Config>>,
    pub auth: Option<Auth>,
    pub request_proxy_url: String,
}

fn wire_profile_active(p: Option<&HttpWireProfile>) -> bool {
    p.is_some_and(|p| p.http1_only || p.disable_auto_compression || !p.header_profile.is_empty())
}

impl HostHttpClient {
    fn client_for(&self, profile: Option<&HttpWireProfile>) -> Result<reqwest::Client, HostError> {
        if !wire_profile_active(profile) {
            return Ok(new_proxy_aware_http_client(&self.request_proxy_url, self.cfg.as_deref(), self.auth.as_ref(), None));
        }
        let Some(profile) = profile else { unreachable!("checked by wire_profile_active") };
        let mut builder = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_max_idle_per_host(0)
            .no_brotli()
            .no_deflate()
            .no_zstd();
        if profile.http1_only || !profile.header_profile.is_empty() {
            builder = builder.http1_only();
        }
        if profile.disable_auto_compression {
            builder = builder.no_gzip();
        }
        let setting = effective_proxy_setting(&self.request_proxy_url, self.auth.as_ref(), self.cfg.as_deref())
            .map_err(|e| HostError::msg(format!("pluginhost: parse proxy: {e}")))?;
        match setting {
            ProxySetting::Inherit => {}
            ProxySetting::Direct => builder = builder.no_proxy(),
            ProxySetting::Proxy(url) => {
                let url = match url.strip_prefix("socks5://") {
                    Some(rest) => format!("socks5h://{rest}"),
                    None => url,
                };
                let proxy = reqwest::Proxy::all(url).map_err(|e| HostError::msg(format!("pluginhost: build proxy transport: {e}")))?;
                builder = builder.proxy(proxy);
            }
        }
        builder.build().map_err(|e| HostError::msg(format!("pluginhost: build http client: {e}")))
    }

    async fn send(&self, ctx: &CallCtx, req: HttpRequest) -> Result<reqwest::Response, HostError> {
        let method = if req.method.is_empty() {
            Method::GET
        } else {
            Method::from_bytes(req.method.as_bytes()).map_err(|e| HostError::msg(format!("create host http request: net/http: invalid method {:?}: {e}", req.method)))?
        };
        let client = self.client_for(req.wire_profile.as_ref())?;
        let mut builder = client.request(method, &req.url);
        for (key, values) in &req.headers {
            let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else { continue };
            for v in values {
                if let Ok(value) = HeaderValue::from_str(v) {
                    builder = builder.header(name.clone(), value);
                }
            }
        }
        if !req.body.is_empty() {
            builder = builder.body(req.body.clone());
        }
        let built = builder.build().map_err(|e| HostError::msg(format!("create host http request: {e}")))?;
        tokio::select! {
            r = client.execute(built) => r.map_err(|e| HostError::msg(format!("execute host http request: {}", error_chain(&e)))),
            () = ctx.cancelled() => Err(HostError::canceled()),
        }
    }

    /// Non-streaming call (Go: `Do`).
    pub async fn do_request(&self, ctx: &CallCtx, req: HttpRequest) -> Result<HttpResponse, HostError> {
        let resp = self.send(ctx, req).await?;
        let status = resp.status().as_u16();
        let headers = headers_to_go(resp.headers());
        let body = tokio::select! {
            b = resp.bytes() => b.map_err(|e| HostError::msg(format!("read host http response: {}", error_chain(&e))))?,
            () = ctx.cancelled() => return Err(HostError::canceled()),
        };
        Ok(HttpResponse { status_code: i64::from(status), headers, body: body.to_vec() })
    }

    /// Streaming call (Go: `DoStream`): chunks arrive on the returned channel until it closes.
    pub async fn do_stream(&self, ctx: &CallCtx, req: HttpRequest) -> Result<HttpStream, HostError> {
        let resp = self.send(ctx, req).await?;
        let status = resp.status().as_u16();
        let headers = headers_to_go(resp.headers());
        let (tx, rx) = mpsc::channel::<HttpChunk>(1);
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            loop {
                let next = tokio::select! {
                    () = ctx.cancelled() => return,
                    n = stream.next() => n,
                };
                match next {
                    None => return,
                    Some(Ok(bytes)) => {
                        for part in split_chunks(bytes) {
                            let sent = tokio::select! {
                                () = ctx.cancelled() => return,
                                r = tx.send(HttpChunk { payload: part, err: None }) => r,
                            };
                            if sent.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(HttpChunk { payload: Bytes::new(), err: Some(error_chain(&e)) }).await;
                        return;
                    }
                }
            }
        });
        Ok(HttpStream { status, headers, chunks: rx })
    }
}

/// Go reads response bodies in 32 KiB buffers.
fn split_chunks(bytes: Bytes) -> Vec<Bytes> {
    const MAX: usize = 32 * 1024;
    if bytes.len() <= MAX {
        return vec![bytes];
    }
    bytes.chunks(MAX).map(Bytes::copy_from_slice).collect()
}

/// Message plus sources, close to what Go's `url.Error` chain prints.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}
