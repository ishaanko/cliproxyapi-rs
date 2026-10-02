//! Shared test helpers: unsigned JWTs and a tiny in-process HTTP mock server (no real network).

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Unsigned JWT with the given claims.
pub(crate) fn make_jwt(claims: &Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
    format!("{h}.{p}.sig")
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecordedRequest {
    pub method: String,
    /// Path plus query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn body_json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// Decoded `application/x-www-form-urlencoded` body as ordered pairs.
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    pub fn form_value(&self, key: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

pub(crate) struct MockResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }

    pub fn raw(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![],
            body,
        }
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub(crate) struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    /// Starts a server on an ephemeral loopback port; `handler` maps each request to a response.
    pub async fn start(
        handler: impl Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static,
    ) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let (handler, log) = (handler.clone(), log.clone());
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut stream).await else {
                        return;
                    };
                    log.lock().push(req.clone());
                    let resp = handler(&req);
                    let reason = if resp.status < 300 { "OK" } else { "Error" };
                    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason);
                    for (k, v) in &resp.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str(&format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n",
                        resp.body.len()
                    ));
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&resp.body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        MockServer {
            url,
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().len()
    }

    pub fn last(&self) -> RecordedRequest {
        self.requests
            .lock()
            .last()
            .cloned()
            .expect("no request recorded")
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(RecordedRequest {
        method,
        target,
        headers,
        body,
    })
}
