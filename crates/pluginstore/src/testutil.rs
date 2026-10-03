//! Shared fixtures for unit tests: in-memory doers, zip builder and a tiny HTTP server.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use zip::write::SimpleFileOptions;

use crate::http::{BodyReader, DoError, Headers, HttpDoer, HttpRequest, HttpResponse};

/// Builds a zip with the given (name, content) entries, in order.
pub(crate) fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in files {
        writer.start_file(*name, SimpleFileOptions::default()).expect("start zip entry");
        writer.write_all(content.as_bytes()).expect("write zip entry");
    }
    writer.finish().expect("finish zip").into_inner()
}

/// URL to body map; unknown URLs answer 404 "not found" (Go `mapHTTPDoer`).
pub(crate) struct MapDoer(pub(crate) HashMap<String, Vec<u8>>);

impl MapDoer {
    pub(crate) fn arc(entries: Vec<(String, Vec<u8>)>) -> Arc<dyn HttpDoer> {
        Arc::new(MapDoer(entries.into_iter().collect()))
    }
}

#[async_trait]
impl HttpDoer for MapDoer {
    async fn get(&self, request: HttpRequest) -> Result<HttpResponse, DoError> {
        Ok(match self.0.get(&request.url) {
            Some(body) => HttpResponse::from_bytes(200, Headers::new(), body.clone()),
            None => HttpResponse::from_bytes(404, Headers::new(), "not found"),
        })
    }
}

/// Doer that answers through a closure.
pub(crate) struct FnDoer<F>(pub(crate) F);

#[async_trait]
impl<F> HttpDoer for FnDoer<F>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse, DoError> + Send + Sync,
{
    async fn get(&self, request: HttpRequest) -> Result<HttpResponse, DoError> {
        (self.0)(&request)
    }
}

pub(crate) fn fn_doer<F>(f: F) -> Arc<dyn HttpDoer>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse, DoError> + Send + Sync + 'static,
{
    Arc::new(FnDoer(f))
}

/// Always fails like an unreachable network.
pub(crate) fn failing_doer() -> Arc<dyn HttpDoer> {
    fn_doer(|_| Err(DoError("network unavailable".to_string())))
}

/// Fixed environment for auth lookups (tests cannot mutate the process env safely).
pub(crate) fn env_fn(pairs: &[(&str, &str)]) -> crate::auth::EnvFn {
    let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Arc::new(move |name| map.get(name).cloned().unwrap_or_default())
}

pub(crate) fn headers(values: &[(&str, &str)]) -> Headers {
    let mut headers = Headers::new();
    for (name, value) in values {
        headers.set(name, *value);
    }
    headers
}

/// Serves `url` only when `Authorization` equals `want_auth` (Go `authCheckingHTTPDoer`).
pub(crate) fn auth_checking_doer(url: &str, want_auth: &str, body: Vec<u8>) -> Arc<dyn HttpDoer> {
    let (url, want_auth) = (url.to_string(), want_auth.to_string());
    fn_doer(move |request| {
        if request.url != url {
            return Ok(HttpResponse::from_bytes(404, Headers::new(), "not found"));
        }
        if request.headers.get("Authorization") != want_auth {
            return Ok(HttpResponse::from_bytes(401, Headers::new(), "bad auth"));
        }
        Ok(HttpResponse::from_bytes(200, Headers::new(), body.clone()))
    })
}

/// Body yielding the data in fixed-size chunks and recording how much was consumed.
pub(crate) struct TrackingBody {
    pub(crate) data: Vec<u8>,
    pub(crate) offset: Arc<parking_lot::Mutex<usize>>,
    pub(crate) chunk_size: usize,
}

#[async_trait]
impl BodyReader for TrackingBody {
    async fn chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        let mut offset = self.offset.lock();
        if *offset >= self.data.len() {
            return Ok(None);
        }
        let end = (*offset + self.chunk_size).min(self.data.len());
        let chunk = Bytes::copy_from_slice(&self.data[*offset..end]);
        *offset = end;
        Ok(Some(chunk))
    }
}

/// Minimal HTTP/1.1 server on localhost for exercising the reqwest transport. The handler
/// maps (path, headers) to (status, extra headers, body). Returns the base URL.
pub(crate) async fn spawn_server<F>(handler: F) -> String
where
    F: Fn(&str, &Headers) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let mut lines = text.split("\r\n");
                let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_string();
                let mut request_headers = Headers::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        request_headers.add(name.trim(), value.trim());
                    }
                }
                let (status, extra, body) = handler(&path, &request_headers);
                let mut response = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (name, value) in extra {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}
