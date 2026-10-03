//! HTTP abstraction: a Go `http.Header` equivalent and the injectable [`HttpDoer`]
//! (Go `httpfetch.Doer`) so callers can supply proxy-aware clients.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;

/// Multi-valued header map with Go's canonical key form (`textproto.CanonicalMIMEHeaderKey`).
/// Keys are kept sorted so iteration and [`Headers::write_to`] match Go's `Header.Write`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Headers(BTreeMap<String, Vec<String>>);

fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

/// Go `textproto.CanonicalMIMEHeaderKey`; keys with invalid bytes are returned unchanged.
pub fn canonical_header_key(key: &str) -> String {
    if !key.bytes().all(is_token_byte) {
        return key.to_string();
    }
    let mut upper = true;
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        upper = c == '-';
    }
    out
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// First value for the key, or empty (Go `Header.Get`).
    pub fn get(&self, key: &str) -> &str {
        self.0
            .get(&canonical_header_key(key))
            .and_then(|values| values.first())
            .map_or("", String::as_str)
    }

    /// Replaces all values (Go `Header.Set`).
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(canonical_header_key(key), vec![value.into()]);
    }

    /// Appends a value (Go `Header.Add`).
    pub fn add(&mut self, key: &str, value: impl Into<String>) {
        self.0.entry(canonical_header_key(key)).or_default().push(value.into());
    }

    pub fn del(&mut self, key: &str) {
        self.0.remove(&canonical_header_key(key));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .flat_map(|(key, values)| values.iter().map(move |value| (key.as_str(), value.as_str())))
    }

    /// Go `Header.Write`: `Key: value\r\n` per value, keys sorted, newlines in values
    /// replaced by spaces and surrounding whitespace trimmed.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        for (key, values) in &self.0 {
            if key.is_empty() || !key.bytes().all(is_token_byte) {
                continue;
            }
            for value in values {
                let cleaned = value.replace(['\n', '\r'], " ");
                out.extend_from_slice(key.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(cleaned.trim_matches([' ', '\t']).as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
    }
}

/// A GET request: the URL and the headers to send. Doers must not follow redirects.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Headers,
}

/// Streaming response body. Dropping the body closes it.
#[async_trait]
pub trait BodyReader: Send {
    /// Next chunk, or `None` at end of body.
    async fn chunk(&mut self) -> io::Result<Option<Bytes>>;
}

/// Fully buffered body for in-memory doers.
pub struct BytesBody(Option<Bytes>);

impl BytesBody {
    pub fn new(data: impl Into<Bytes>) -> Self {
        Self(Some(data.into()))
    }
}

#[async_trait]
impl BodyReader for BytesBody {
    async fn chunk(&mut self) -> io::Result<Option<Bytes>> {
        Ok(self.0.take().filter(|data| !data.is_empty()))
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Headers,
    pub body: Box<dyn BodyReader>,
}

impl HttpResponse {
    pub fn from_bytes(status: u16, headers: Headers, body: impl Into<Bytes>) -> Self {
        Self { status, headers, body: Box::new(BytesBody::new(body)) }
    }
}

/// Transport failure (Go's `*url.Error` cause). Display carries no URL.
#[derive(Debug, Clone)]
pub struct DoError(pub String);

impl fmt::Display for DoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DoError {}

/// Executes one HTTP GET without following redirects (Go `HTTPDoer`).
#[async_trait]
pub trait HttpDoer: Send + Sync {
    async fn get(&self, request: HttpRequest) -> Result<HttpResponse, DoError>;
}

/// [`HttpDoer`] backed by `reqwest`. The wrapped client must be built with
/// `redirect::Policy::none()` (see [`ReqwestDoer::client_builder`]) because the plugin
/// store follows redirects itself so it can re-evaluate credentials per hop.
#[derive(Clone)]
pub struct ReqwestDoer {
    client: reqwest::Client,
}

impl ReqwestDoer {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Client builder with redirects disabled; add proxy settings and call `build()`.
    pub fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
    }
}

/// Renders a reqwest error and its causes without the request URL.
fn reqwest_error_text(err: reqwest::Error) -> String {
    let err = err.without_url();
    let mut text = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

struct ReqwestBody(reqwest::Response);

#[async_trait]
impl BodyReader for ReqwestBody {
    async fn chunk(&mut self) -> io::Result<Option<Bytes>> {
        self.0.chunk().await.map_err(|err| io::Error::other(reqwest_error_text(err)))
    }
}

#[async_trait]
impl HttpDoer for ReqwestDoer {
    async fn get(&self, request: HttpRequest) -> Result<HttpResponse, DoError> {
        let mut builder = self.client.get(&request.url);
        for (name, value) in request.headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|err| DoError(reqwest_error_text(err)))?;
        let mut headers = Headers::new();
        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                headers.add(name.as_str(), value);
            }
        }
        Ok(HttpResponse {
            status: response.status().as_u16(),
            headers,
            body: Box::new(ReqwestBody(response)),
        })
    }
}

/// Process-wide fallback used when a client has no doer (Go `http.DefaultClient`).
pub(crate) fn default_doer() -> Arc<dyn HttpDoer> {
    static DEFAULT: OnceLock<Arc<dyn HttpDoer>> = OnceLock::new();
    DEFAULT
        .get_or_init(|| match ReqwestDoer::client_builder().build() {
            Ok(client) => Arc::new(ReqwestDoer::new(client)),
            Err(err) => Arc::new(FailedDoer(err.to_string())),
        })
        .clone()
}

struct FailedDoer(String);

#[async_trait]
impl HttpDoer for FailedDoer {
    async fn get(&self, _request: HttpRequest) -> Result<HttpResponse, DoError> {
        Err(DoError(format!("http client unavailable: {}", self.0)))
    }
}
