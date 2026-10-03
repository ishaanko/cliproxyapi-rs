//! Minimal S3-compatible client (path-style, SigV4) covering what the object store needs:
//! bucket exists/create, stat/get/put/delete object and recursive prefix listing. Stands in for
//! minio-go and works against AWS S3, MinIO and other S3 gateways.

use std::fmt;
use std::sync::Arc;

use chrono::Utc;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_REGION: &str = "us-east-1";
/// RFC 3986 unreserved characters stay literal; everything else is percent-encoded.
const ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// A failed S3 call: HTTP status plus the `<Code>` / `<Message>` of the error body when present.
#[derive(Debug, Clone)]
pub(crate) struct S3Error {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl S3Error {
    fn transport(err: impl fmt::Display) -> Self {
        Self { status: 0, code: String::new(), message: err.to_string() }
    }

    /// `isObjectNotFound`.
    pub(crate) fn is_not_found(&self) -> bool {
        self.status == 404 || matches!(self.code.as_str(), "NoSuchKey" | "NotFound" | "NoSuchBucket")
    }
}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.message.is_empty() {
            f.write_str(&self.message)
        } else if !self.code.is_empty() {
            f.write_str(&self.code)
        } else {
            write!(f, "s3 request failed with status {}", self.status)
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct S3Config {
    /// `host[:port][/path]` without scheme.
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub use_ssl: bool,
}

pub(crate) struct S3Client {
    http: reqwest::Client,
    cfg: S3Config,
    /// Resolved bucket region (configured, discovered via `?location`, or us-east-1).
    region: Mutex<Option<String>>,
}

fn encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, ENCODE).to_string()
}

fn encode_path(key: &str) -> String {
    key.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so `new_from_slice` cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

impl S3Client {
    pub(crate) fn new(cfg: S3Config) -> Result<Arc<Self>, String> {
        let http = reqwest::Client::builder().build().map_err(|e| e.to_string())?;
        let region = (!cfg.region.is_empty()).then(|| cfg.region.clone());
        Ok(Arc::new(Self { http, cfg, region: Mutex::new(region) }))
    }

    fn base(&self) -> String {
        let scheme = if self.cfg.use_ssl { "https" } else { "http" };
        format!("{scheme}://{}", self.cfg.endpoint.trim_end_matches('/'))
    }

    /// Sends a SigV4-signed request. `object` is the key (None for bucket-level calls); `query`
    /// pairs are sorted and encoded into the canonical query.
    async fn send(
        &self,
        method: Method,
        object: Option<&str>,
        query: &[(&str, &str)],
        body: Vec<u8>,
        content_type: Option<&str>,
        region: &str,
    ) -> Result<reqwest::Response, S3Error> {
        let mut path = format!("/{}", encode(&self.cfg.bucket));
        if let Some(key) = object {
            path.push('/');
            path.push_str(&encode_path(key));
        }
        let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (encode(k), encode(v))).collect();
        pairs.sort();
        let canonical_query = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let mut url = format!("{}{}", self.base(), path);
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }
        let parsed = url::Url::parse(&url).map_err(S3Error::transport)?;
        let mut host = parsed.host_str().unwrap_or_default().to_string();
        if let Some(port) = parsed.port() {
            host = format!("{host}:{port}");
        }
        // The endpoint may carry a path prefix; the signed path is the full request path.
        let signed_path = parsed.path().to_string();

        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let day = now.format("%Y%m%d").to_string();
        let payload_hash = sha256_hex(&body);
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{signed_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            method.as_str()
        );
        let scope = format!("{day}/{region}/s3/aws4_request");
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
        let k_date = hmac(format!("AWS4{}", self.cfg.secret_key).as_bytes(), day.as_bytes());
        let k_region = hmac(&k_date, region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.cfg.access_key
        );

        let mut req = self
            .http
            .request(method, parsed)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header("authorization", authorization);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        req.send().await.map_err(S3Error::transport)
    }

    /// Reads an error response into an [`S3Error`].
    async fn error_from(resp: reqwest::Response) -> S3Error {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let code = xml_text(&body, "Code").unwrap_or_default();
        let message = xml_text(&body, "Message").unwrap_or_default();
        S3Error { status, code, message }
    }

    /// Signing region: configured, else discovered once from `?location`, else us-east-1.
    async fn region(&self) -> String {
        if let Some(r) = self.region.lock().clone() {
            return r;
        }
        let discovered = match self.send(Method::GET, None, &[("location", "")], Vec::new(), None, DEFAULT_REGION).await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                xml_text(&body, "LocationConstraint").filter(|r| !r.is_empty())
            }
            _ => None,
        };
        let region = discovered.unwrap_or_else(|| DEFAULT_REGION.to_string());
        *self.region.lock() = Some(region.clone());
        region
    }

    /// `BucketExists`.
    pub(crate) async fn bucket_exists(&self) -> Result<bool, S3Error> {
        let region = self.region().await;
        let resp = self.send(Method::HEAD, None, &[], Vec::new(), None, &region).await?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(Self::error_from(resp).await),
        }
    }

    /// `MakeBucket`.
    pub(crate) async fn make_bucket(&self) -> Result<(), S3Error> {
        let region = self.region().await;
        let body = if region != DEFAULT_REGION {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
            )
            .into_bytes()
        } else {
            Vec::new()
        };
        let resp = self.send(Method::PUT, None, &[], body, None, &region).await?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::error_from(resp).await) }
    }

    /// `StatObject`: Ok when the object exists.
    pub(crate) async fn stat_object(&self, key: &str) -> Result<(), S3Error> {
        let region = self.region().await;
        let resp = self.send(Method::HEAD, Some(key), &[], Vec::new(), None, &region).await?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::error_from(resp).await) }
    }

    /// `GetObject` + `io.ReadAll`.
    pub(crate) async fn get_object(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let region = self.region().await;
        let resp = self.send(Method::GET, Some(key), &[], Vec::new(), None, &region).await?;
        if !resp.status().is_success() {
            return Err(Self::error_from(resp).await);
        }
        resp.bytes().await.map(|b| b.to_vec()).map_err(S3Error::transport)
    }

    /// `PutObject`.
    pub(crate) async fn put_object(&self, key: &str, data: Vec<u8>, content_type: &str) -> Result<(), S3Error> {
        let region = self.region().await;
        let resp = self.send(Method::PUT, Some(key), &[], data, Some(content_type), &region).await?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::error_from(resp).await) }
    }

    /// `RemoveObject`: S3 answers 204 for missing keys; 404 is reported to the caller.
    pub(crate) async fn remove_object(&self, key: &str) -> Result<(), S3Error> {
        let region = self.region().await;
        let resp = self.send(Method::DELETE, Some(key), &[], Vec::new(), None, &region).await?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::error_from(resp).await) }
    }

    /// `ListObjects` (recursive, ListObjectsV2): every key under `prefix`.
    pub(crate) async fn list_objects(&self, prefix: &str) -> Result<Vec<String>, S3Error> {
        let region = self.region().await;
        let mut keys = Vec::new();
        let mut token = String::new();
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if !token.is_empty() {
                query.push(("continuation-token", token.as_str()));
            }
            let resp = self.send(Method::GET, None, &query, Vec::new(), None, &region).await?;
            if !resp.status().is_success() {
                return Err(Self::error_from(resp).await);
            }
            let body = resp.text().await.map_err(S3Error::transport)?;
            keys.extend(xml_all_text(&body, "Contents", "Key"));
            let truncated = xml_text(&body, "IsTruncated").is_some_and(|v| v == "true");
            match xml_text(&body, "NextContinuationToken") {
                Some(next) if truncated && !next.is_empty() => token = next,
                _ => return Ok(keys),
            }
        }
    }
}

/// Text of the first `<tag>` element.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut inside = false;
    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == tag.as_bytes() => inside = true,
            Ok(Event::Text(t)) if inside => out.push_str(&t.unescape().ok()?),
            Ok(Event::End(e)) if inside && e.name().as_ref() == tag.as_bytes() => return Some(out),
            Ok(Event::Empty(e)) if e.name().as_ref() == tag.as_bytes() => return Some(String::new()),
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Text of every `<child>` inside each `<parent>` element, in document order.
fn xml_all_text(xml: &str, parent: &str, child: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let (mut in_parent, mut in_child) = (false, false);
    let mut cur = String::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                if name.as_ref() == parent.as_bytes() {
                    in_parent = true;
                } else if in_parent && name.as_ref() == child.as_bytes() {
                    in_child = true;
                    cur.clear();
                }
            }
            Ok(Event::Text(t)) if in_child => match t.unescape() {
                Ok(s) => cur.push_str(&s),
                Err(_) => in_child = false,
            },
            Ok(Event::End(e)) => {
                let name = e.name();
                if in_child && name.as_ref() == child.as_bytes() {
                    out.push(std::mem::take(&mut cur));
                    in_child = false;
                } else if name.as_ref() == parent.as_bytes() {
                    in_parent = false;
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_and_error_xml() {
        let list = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>t1</NextContinuationToken>\
            <Contents><Key>auths/a.json</Key></Contents><Contents><Key>auths/b&amp;c.json</Key></Contents></ListBucketResult>";
        assert_eq!(xml_all_text(list, "Contents", "Key"), ["auths/a.json", "auths/b&c.json"]);
        assert_eq!(xml_text(list, "NextContinuationToken").as_deref(), Some("t1"));
        let err = "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>";
        assert_eq!(xml_text(err, "Code").as_deref(), Some("NoSuchKey"));
    }

    #[test]
    fn key_encoding_keeps_slashes() {
        assert_eq!(encode_path("auths/a b+c.json"), "auths/a%20b%2Bc.json");
    }

    // AWS documents this signing-key derivation example (GET object, 20130524).
    #[test]
    fn signing_key_matches_aws_example() {
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", b"20130524");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let sts = "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972";
        assert_eq!(
            hex::encode(hmac(&k_signing, sts.as_bytes())),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
