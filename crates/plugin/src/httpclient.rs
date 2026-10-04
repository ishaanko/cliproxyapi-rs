//! HTTP client handed to plugins through `host.http.*` (Go: `http_bridge.go`).
//!
//! Requests use the host's proxy-aware transport. A wire profile selects a dedicated client:
//! `http1_only` and `disable_auto_compression` configure a reqwest client, and a `header_profile`
//! (header names, casing and order on the wire) goes through the ordered HTTP/1.1 client of
//! `cpa-tlsfp`, because reqwest cannot reproduce header order. Every call is recorded in the
//! upstream log of the inbound request it serves, like the Go bridge.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::http::ProxySetting;
use cpa_config::Config;
use cpa_executors::helps::proxy::{effective_proxy_setting, new_proxy_aware_http_client};
use cpa_pluginapi::api::{HttpRequest, HttpResponse, HttpWireProfile};
use cpa_runtime::apilog::UpstreamRequestLog;
use cpa_tlsfp::{WireClient, WireConfig, WireProxy, WireRequest};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
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
        if profile.http1_only {
            builder = builder.http1_only();
        }
        if profile.disable_auto_compression {
            builder = builder.no_gzip();
        }
        match self.proxy_setting()? {
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

    /// Request override, then credential, then config proxy (Go: the priority comment in
    /// `newHTTPClientForRequest`).
    fn proxy_setting(&self) -> Result<ProxySetting, HostError> {
        effective_proxy_setting(&self.request_proxy_url, self.auth.as_ref(), self.cfg.as_deref())
            .map_err(|e| HostError::msg(format!("pluginhost: parse proxy: {e}")))
    }

    /// The ordered HTTP/1.1 client for a header profile.
    fn wire_client(&self, profile: &HttpWireProfile) -> Result<WireClient, HostError> {
        let proxy = match self.proxy_setting()? {
            ProxySetting::Inherit => WireProxy::Inherit,
            ProxySetting::Direct => WireProxy::Direct,
            ProxySetting::Proxy(url) => WireProxy::Url(url),
        };
        Ok(WireClient::new(WireConfig {
            header_profile: profile.header_profile.clone(),
            disable_auto_compression: profile.disable_auto_compression,
            proxy,
            auth_proxy: self.auth.as_ref().map(|a| a.proxy_url.clone()).unwrap_or_default(),
            config_proxy: self.cfg.as_ref().map(|c| c.proxy_url.clone()).unwrap_or_default(),
            extra_roots: Vec::new(),
        }))
    }

    /// `recordHTTPRequest`: the outbound request goes into the inbound request's upstream log.
    fn record_request(&self, ctx: &CallCtx, method: &str, req: &HttpRequest) {
        let Some(cfg) = self.cfg.as_deref() else { return };
        let mut headers = HeaderMap::new();
        for (key, values) in &req.headers {
            let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else { continue };
            for v in values {
                if let Ok(value) = HeaderValue::from_bytes(v.as_bytes()) {
                    headers.append(name.clone(), value);
                }
            }
        }
        let provider = self.auth.as_ref().map(|a| a.provider.as_str()).unwrap_or_default();
        let info = UpstreamRequestLog::from_auth(provider, self.auth.as_ref(), method, &req.url, &headers, &req.body);
        ctx.api_log.record_api_request(cfg, info);
    }

    async fn send(&self, ctx: &CallCtx, req: HttpRequest) -> Result<reqwest::Response, HostError> {
        let method = if req.method.is_empty() {
            Method::GET
        } else {
            Method::from_bytes(req.method.as_bytes()).map_err(|e| HostError::msg(format!("create host http request: net/http: invalid method {:?}: {e}", req.method)))?
        };
        let url = reqwest::Url::parse(&req.url).map_err(|e| HostError::msg(format!("create host http request: parse {:?}: {e}", req.url)))?;
        self.record_request(ctx, method.as_str(), &req);
        match req.wire_profile.as_ref().filter(|p| !p.header_profile.is_empty()) {
            Some(profile) => self.send_ordered(ctx, profile, method, req.clone()).await,
            None => self.send_plain(ctx, method, url, &req).await,
        }
    }

    /// A failure of the HTTP exchange itself (not of building the request) is also logged.
    fn exchange_failed(&self, ctx: &CallCtx, e: HostError) -> HostError {
        if let Some(cfg) = self.cfg.as_deref() {
            ctx.api_log.record_api_response_error(cfg, &e.message);
        }
        e
    }

    /// Reqwest path: the proxy-aware client, or the profile's HTTP/1-only / no-compression one.
    async fn send_plain(&self, ctx: &CallCtx, method: Method, url: reqwest::Url, req: &HttpRequest) -> Result<reqwest::Response, HostError> {
        let client = self.client_for(req.wire_profile.as_ref())?;
        let mut builder = client.request(method, url);
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
        ctx.mark_upstream_attempt();
        tokio::select! {
            r = client.execute(built) => r.map_err(|e| self.exchange_failed(ctx, HostError::msg(format!("execute host http request: {}", error_chain(&e))))),
            () = ctx.cancelled() => Err(self.exchange_failed(ctx, HostError::canceled())),
        }
    }

    /// Header-profile path: the plugin's header names and order reach the wire.
    async fn send_ordered(&self, ctx: &CallCtx, profile: &HttpWireProfile, method: Method, req: HttpRequest) -> Result<reqwest::Response, HostError> {
        let client = self.wire_client(profile)?;
        let wire = WireRequest { method: method.as_str().to_string(), url: req.url, headers: req.headers, body: Bytes::from(req.body) };
        ctx.mark_upstream_attempt();
        tokio::select! {
            r = client.execute(wire) => r.map_err(|e| self.exchange_failed(ctx, HostError::msg(format!("execute host http request: {e}")))),
            () = ctx.cancelled() => Err(self.exchange_failed(ctx, HostError::canceled())),
        }
    }

    /// Non-streaming call (Go: `Do`).
    pub async fn do_request(&self, ctx: &CallCtx, req: HttpRequest) -> Result<HttpResponse, HostError> {
        let resp = self.send(ctx, req).await?;
        let status = resp.status().as_u16();
        let headers = headers_to_go(resp.headers());
        let cfg = self.cfg.as_deref();
        if let Some(cfg) = cfg {
            ctx.api_log.record_api_response_metadata(cfg, status, resp.headers());
        }
        let body = tokio::select! {
            b = resp.bytes() => b,
            () = ctx.cancelled() => return Err(HostError::canceled()),
        };
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                let message = error_chain(&e);
                if let Some(cfg) = cfg {
                    ctx.api_log.record_api_response_error(cfg, &message);
                }
                return Err(HostError::msg(format!("read host http response: {message}")));
            }
        };
        if let (Some(cfg), false) = (cfg, body.is_empty()) {
            ctx.api_log.append_api_response_chunk(cfg, &body);
        }
        Ok(HttpResponse { status_code: i64::from(status), headers, body: body.to_vec() })
    }

    /// Streaming call (Go: `DoStream`): chunks arrive on the returned channel until it closes.
    pub async fn do_stream(&self, ctx: &CallCtx, req: HttpRequest) -> Result<HttpStream, HostError> {
        let resp = self.send(ctx, req).await?;
        let status = resp.status().as_u16();
        let headers = headers_to_go(resp.headers());
        let cfg = self.cfg.clone();
        if let Some(cfg) = cfg.as_deref() {
            ctx.api_log.record_api_response_metadata(cfg, status, resp.headers());
        }
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
                            if let Some(cfg) = cfg.as_deref() {
                                ctx.api_log.append_api_response_chunk(cfg, &part);
                            }
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
                        let message = error_chain(&e);
                        if let Some(cfg) = cfg.as_deref() {
                            ctx.api_log.record_api_response_error(cfg, &message);
                        }
                        let _ = tx.send(HttpChunk { payload: Bytes::new(), err: Some(message) }).await;
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
