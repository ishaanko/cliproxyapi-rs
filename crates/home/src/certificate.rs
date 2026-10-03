//! mTLS enrollment from a Home JWT (Go: `internal/home/certificate.go`).
//!
//! `-home-jwt` carries the target address and an enrollment secret. The first start generates a
//! client key, asks Home to sign a CSR over a plain RESP connection (`CERTIFICATE REQUEST ...`),
//! verifies the returned CA against the fingerprint in the JWT and stores key, certificate and CA
//! under `~/.cli-proxy-api`. Later starts only re-verify the stored CA.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use cpa_config::{HomeConfig, HomeTlsConfig};
use der::Decode;
use rsa::RsaPrivateKey;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey, LineEnding};
use rsa::pkcs8::DecodePrivateKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use x509_cert::builder::{Builder, RequestBuilder};
use x509_cert::name::Name;

use crate::error::HomeError;
use crate::resp::{self, Value};

const CERTIFICATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HomeJwtClaims {
    #[serde(default)]
    pub certificate_id: String,
    #[serde(default)]
    pub cluster_id: String,
    #[serde(default)]
    pub ca_fingerprint: String,
    #[serde(default)]
    pub enrollment_secret: String,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub port: i64,
    #[serde(default)]
    pub iat: i64,
}

#[derive(Deserialize)]
struct CertificateResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    certificate: String,
    #[serde(default)]
    ca: String,
}

/// Where the enrollment material lives.
#[derive(Debug, Clone)]
pub struct CertificatePaths {
    pub dir: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub ca_cert: PathBuf,
}

impl CertificatePaths {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        CertificatePaths {
            client_cert: dir.join("client-crt.pem"),
            client_key: dir.join("client-key.pem"),
            ca_cert: dir.join("home-ca-crt.pem"),
            dir,
        }
    }

    /// `~/.cli-proxy-api`.
    pub fn default_location() -> Result<Self, HomeError> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| HomeError::other("$HOME is not defined"))?;
        Ok(Self::in_dir(PathBuf::from(home).join(".cli-proxy-api")))
    }
}

fn decode_jwt_part(part: &str) -> Result<Vec<u8>, HomeError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part);
    match raw {
        Ok(v) => Ok(v),
        Err(_) => base64::engine::general_purpose::URL_SAFE.decode(part).map_err(HomeError::other),
    }
}

/// Go: `normalizeFingerprint`.
pub fn normalize_fingerprint(fp: &str) -> String {
    fp.trim().to_lowercase().replace([':', ' '], "")
}

/// Go: `parseHomeJWTClaims` (the signature is not checked; Home authenticates the enrollment
/// secret).
pub fn parse_home_jwt_claims(raw_jwt: &str) -> Result<HomeJwtClaims, HomeError> {
    let parts: Vec<&str> = raw_jwt.trim().split('.').collect();
    if parts.len() != 3 {
        return Err(HomeError::other("home jwt is invalid"));
    }
    let payload = decode_jwt_part(parts[1])?;
    let claims: HomeJwtClaims = serde_json::from_slice(&payload).map_err(HomeError::other)?;
    if claims.certificate_id.trim().is_empty() {
        return Err(HomeError::other("home jwt certificate_id is required"));
    }
    if claims.cluster_id.trim().is_empty() {
        return Err(HomeError::other("home jwt cluster_id is required"));
    }
    if normalize_fingerprint(&claims.ca_fingerprint).is_empty() {
        return Err(HomeError::other("home jwt ca_fingerprint is required"));
    }
    if claims.enrollment_secret.trim().is_empty() {
        return Err(HomeError::other("home jwt enrollment_secret is required"));
    }
    if claims.ip.trim().is_empty() || claims.port <= 0 {
        return Err(HomeError::other("home jwt target address is invalid"));
    }
    Ok(claims)
}

/// Go: `ConfigFromJWT`: prepares a Home config from the JWT and ensures the local mTLS files.
pub async fn config_from_jwt(raw_jwt: &str) -> Result<HomeConfig, HomeError> {
    config_from_jwt_at(raw_jwt, &CertificatePaths::default_location()?).await
}

pub async fn config_from_jwt_at(raw_jwt: &str, paths: &CertificatePaths) -> Result<HomeConfig, HomeError> {
    let claims = parse_home_jwt_claims(raw_jwt)?;
    ensure_certificate_files(&claims, paths).await?;
    let path = |p: &Path| p.to_string_lossy().into_owned();
    Ok(HomeConfig {
        enabled: true,
        node_id: claims.certificate_id.trim().to_string(),
        host: claims.ip.trim().to_string(),
        port: claims.port,
        disable_cluster_discovery: false,
        tls: HomeTlsConfig {
            enable: true,
            server_name: String::new(),
            insecure_skip_verify: false,
            ca_cert: path(&paths.ca_cert),
            client_cert: path(&paths.client_cert),
            client_key: path(&paths.client_key),
            use_target_server_name: true,
        },
    })
}

fn file_exists(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| !m.is_dir())
}

fn write_file_0600(path: &Path, raw: &[u8]) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(raw)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, raw)?;
        Ok(())
    }
}

fn chmod_0600(path: &Path) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn ensure_certificate_files(claims: &HomeJwtClaims, paths: &CertificatePaths) -> Result<(), HomeError> {
    if file_exists(&paths.client_cert) && file_exists(&paths.client_key) {
        if !file_exists(&paths.ca_cert) {
            return Err(HomeError::other("home ca certificate file is missing"));
        }
        verify_ca_certificate_pem(&std::fs::read(&paths.ca_cert)?, &claims.ca_fingerprint)?;
        for p in [&paths.client_cert, &paths.client_key, &paths.ca_cert] {
            chmod_0600(p)?;
        }
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&paths.dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&paths.dir)?;
    let key = load_or_create_client_key(&paths.client_key)?;
    let csr_pem = create_client_csr(&claims.certificate_id, &key)?;
    let response = request_client_certificate(claims, &csr_pem).await?;
    if response.certificate.trim().is_empty() || response.ca.trim().is_empty() {
        return Err(HomeError::other("home certificate response is incomplete"));
    }
    verify_ca_certificate_pem(response.ca.as_bytes(), &claims.ca_fingerprint)?;
    write_file_0600(&paths.client_cert, response.certificate.as_bytes())?;
    write_file_0600(&paths.ca_cert, response.ca.as_bytes())?;
    Ok(())
}

/// First PEM block in `raw` as (label, DER), skipping leading text and ignoring anything after
/// the block, like Go's `pem.Decode` (so chains and files with trailing text are accepted).
fn decode_first_pem_block(raw: &[u8]) -> Option<(String, Vec<u8>)> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"-----END ";
    let find = |hay: &[u8], needle: &[u8], from: usize| {
        hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
    };
    let mut from = 0;
    let start = loop {
        let at = find(raw, BEGIN, from)?;
        if at == 0 || raw[at - 1] == b'\n' {
            break at;
        }
        from = at + 1;
    };
    let end = find(raw, END, start + BEGIN.len())?;
    let eol = raw[end..].iter().position(|&b| b == b'\n').map_or(raw.len(), |i| end + i);
    let (label, der_bytes) = pem_rfc7468::decode_vec(&raw[start..eol]).ok()?;
    Some((label.to_string(), der_bytes))
}

/// SHA-256 of the first PEM block's DER, which must be a certificate (Go:
/// `certificateFingerprintPEM`).
fn certificate_fingerprint_pem(raw: &[u8]) -> Result<String, HomeError> {
    let invalid = || HomeError::other("home ca certificate pem is invalid");
    let (label, der_bytes) = decode_first_pem_block(raw).ok_or_else(invalid)?;
    if label != "CERTIFICATE" {
        return Err(invalid());
    }
    x509_cert::Certificate::from_der(&der_bytes).map_err(HomeError::other)?;
    Ok(hex::encode(Sha256::digest(&der_bytes)))
}

pub fn verify_ca_certificate_pem(raw: &[u8], expected_fingerprint: &str) -> Result<(), HomeError> {
    let actual = certificate_fingerprint_pem(raw)?;
    let expected = normalize_fingerprint(expected_fingerprint);
    if expected.is_empty() {
        return Err(HomeError::other("home ca fingerprint is required"));
    }
    if actual != expected {
        return Err(HomeError::other("home ca fingerprint mismatch"));
    }
    Ok(())
}

fn parse_rsa_private_key_pem(raw: &[u8]) -> Result<RsaPrivateKey, HomeError> {
    let (label, der_bytes) = decode_first_pem_block(raw).ok_or_else(|| HomeError::other("client key pem is invalid"))?;
    match label.as_str() {
        "RSA PRIVATE KEY" => RsaPrivateKey::from_pkcs1_der(&der_bytes).map_err(HomeError::other),
        "PRIVATE KEY" => RsaPrivateKey::from_pkcs8_der(&der_bytes).map_err(|_| HomeError::other("client key is not rsa")),
        other => Err(HomeError::Other(format!("client key pem type {other:?} is unsupported"))),
    }
}

fn load_or_create_client_key(path: &Path) -> Result<RsaPrivateKey, HomeError> {
    if file_exists(path) {
        let key = parse_rsa_private_key_pem(&std::fs::read(path)?)?;
        chmod_0600(path)?;
        return Ok(key);
    }
    let key = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).map_err(HomeError::other)?;
    let pem = key.to_pkcs1_pem(LineEnding::LF).map_err(HomeError::other)?;
    write_file_0600(path, pem.as_bytes())?;
    Ok(key)
}

/// A PEM `CERTIFICATE REQUEST` with the certificate id as common name (SHA256WithRSA).
fn create_client_csr(certificate_id: &str, key: &RsaPrivateKey) -> Result<String, HomeError> {
    use std::str::FromStr;
    let certificate_id = certificate_id.trim();
    if certificate_id.is_empty() {
        return Err(HomeError::other("certificate id is required"));
    }
    let escaped: String = certificate_id
        .chars()
        .flat_map(|c| if matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';' | '#' | '=') { vec!['\\', c] } else { vec![c] })
        .collect();
    let subject = Name::from_str(&format!("CN={escaped}")).map_err(HomeError::other)?;
    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key.clone());
    let builder = RequestBuilder::new(subject, &signer).map_err(HomeError::other)?;
    let csr = builder.build::<rsa::pkcs1v15::Signature>().map_err(HomeError::other)?;
    let der_bytes = der::Encode::to_der(&csr).map_err(HomeError::other)?;
    pem_rfc7468::encode_string("CERTIFICATE REQUEST", pem_rfc7468::LineEnding::LF, &der_bytes).map_err(HomeError::other)
}

async fn request_client_certificate(claims: &HomeJwtClaims, csr_pem: &str) -> Result<CertificateResponse, HomeError> {
    let addr = if claims.ip.trim().contains(':') && !claims.ip.trim().starts_with('[') {
        format!("[{}]:{}", claims.ip.trim(), claims.port)
    } else {
        format!("{}:{}", claims.ip.trim(), claims.port)
    };
    let exchange = async {
        let mut stream = TcpStream::connect(&addr).await?;
        let args = ["CERTIFICATE", "REQUEST", claims.certificate_id.as_str(), claims.enrollment_secret.as_str(), csr_pem];
        stream.write_all(&resp::encode_command(&args)).await?;
        let mut reader = BufReader::new(stream);
        let value = resp::read_value(&mut reader).await.map_err(|e| HomeError::Io(e.to_string()))?;
        Ok::<_, HomeError>(value)
    };
    let value = tokio::time::timeout(CERTIFICATE_REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| HomeError::Timeout)??;
    let raw = match value {
        Value::Bulk(b) => b,
        Value::Nil => return Err(HomeError::other("home certificate request returned nil")),
        Value::Error(e) => return Err(HomeError::Other(e.trim().to_string())),
        other => {
            return Err(HomeError::Other(format!("home certificate request returned unsupported resp value {other:?}")));
        }
    };
    let response: CertificateResponse = serde_json::from_slice(&raw).map_err(HomeError::other)?;
    if !response.ok {
        return Err(HomeError::other("home certificate request failed"));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    const CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBhTCCASugAwIBAgIUHl50eua7i0cDOr+S9mjBSKwEiYIwCgYIKoZIzj0EAwIw\nFzEVMBMGA1UEAwwMdGVzdC1ob21lLWNhMCAXDTI2MTAwMzAxMjUxOVoYDzIxMjYw\nOTA5MDEyNTE5WjAXMRUwEwYDVQQDDAx0ZXN0LWhvbWUtY2EwWTATBgcqhkjOPQIB\nBggqhkjOPQMBBwNCAASGysHBsVlIVt1n7cZjb805aQkUuvrkk2s5lgCkG8LeOL8d\nLwizFoubVgxB4CsmKVVayLq3l/o8tFY/oN0KF0Myo1MwUTAdBgNVHQ4EFgQU3iWw\nKbbkcCRufSS+IpV5nXx6v/wwHwYDVR0jBBgwFoAU3iWwKbbkcCRufSS+IpV5nXx6\nv/wwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiByL/uTjuRJAOJ6\n8NwbT2Xd7jya1PMsnSxxf0KLsI06pwIhANZ7vNhdy9y0PemQBuJzkT+IWtHO26qM\nfWQFrrWABNvu\n-----END CERTIFICATE-----\n";
    const CA_FINGERPRINT: &str = "9d100d0c253a92f993c53c65accefc3bf6df69485dcc07a9ca13c6dd00eae6a6";

    fn jwt(claims: serde_json::Value) -> String {
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", enc(b"{\"alg\":\"none\"}"), enc(claims.to_string().as_bytes()))
    }

    fn full_claims(port: i64) -> serde_json::Value {
        serde_json::json!({
            "certificate_id": "node-1", "cluster_id": "c1", "ca_fingerprint": CA_FINGERPRINT.to_uppercase(),
            "enrollment_secret": "s3cret", "ip": "127.0.0.1", "port": port, "iat": 1
        })
    }

    #[test]
    fn claims_require_every_field() {
        assert!(parse_home_jwt_claims(&jwt(full_claims(8327))).is_ok());
        assert_eq!(parse_home_jwt_claims("a.b").unwrap_err().to_string(), "home jwt is invalid");
        for (field, message) in [
            ("certificate_id", "home jwt certificate_id is required"),
            ("cluster_id", "home jwt cluster_id is required"),
            ("ca_fingerprint", "home jwt ca_fingerprint is required"),
            ("enrollment_secret", "home jwt enrollment_secret is required"),
            ("ip", "home jwt target address is invalid"),
            ("port", "home jwt target address is invalid"),
        ] {
            let mut c = full_claims(8327);
            c.as_object_mut().unwrap().remove(field);
            assert_eq!(parse_home_jwt_claims(&jwt(c)).unwrap_err().to_string(), message, "{field}");
        }
    }

    #[test]
    fn fingerprints_normalize_and_ca_is_verified() {
        assert_eq!(normalize_fingerprint(" AB:cd ef "), "abcdef");
        assert!(verify_ca_certificate_pem(CA_PEM.as_bytes(), &CA_FINGERPRINT.to_uppercase()).is_ok());
        let colons: String = CA_FINGERPRINT.as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(":");
        assert!(verify_ca_certificate_pem(CA_PEM.as_bytes(), &colons).is_ok());
        assert_eq!(verify_ca_certificate_pem(CA_PEM.as_bytes(), "00").unwrap_err().to_string(), "home ca fingerprint mismatch");
        assert_eq!(verify_ca_certificate_pem(b"nope", CA_FINGERPRINT).unwrap_err().to_string(), "home ca certificate pem is invalid");
        let wrong_type = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert!(verify_ca_certificate_pem(wrong_type.as_bytes(), CA_FINGERPRINT).is_err());
    }

    /// Mock Home answering the enrollment command with `response`; returns the port and the
    /// received command.
    async fn mock_enrollment(response: serde_json::Value) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = tokio::io::split(stream);
            let mut reader = BufReader::new(r);
            let args = resp::read_command(&mut reader).await.unwrap();
            let body = response.to_string();
            w.write_all(format!("${}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
            args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect()
        });
        (port, task)
    }

    #[tokio::test]
    async fn first_run_enrolls_then_later_runs_only_verify() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CertificatePaths::in_dir(dir.path().join(".cli-proxy-api"));
        let (port, task) = mock_enrollment(serde_json::json!({"ok": true, "certificate": "CLIENT-CERT", "ca": CA_PEM})).await;
        let cfg = config_from_jwt_at(&jwt(full_claims(i64::from(port))), &paths).await.unwrap();
        let cmd = task.await.unwrap();
        assert_eq!((cmd[0].as_str(), cmd[1].as_str(), cmd[2].as_str(), cmd[3].as_str()), ("CERTIFICATE", "REQUEST", "node-1", "s3cret"));
        assert!(cmd[4].starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
        assert!(cfg.enabled && cfg.tls.enable && cfg.tls.use_target_server_name);
        assert_eq!((cfg.node_id.as_str(), cfg.host.as_str(), cfg.port), ("node-1", "127.0.0.1", i64::from(port)));
        assert_eq!(std::fs::read_to_string(&paths.client_cert).unwrap(), "CLIENT-CERT");
        assert!(std::fs::read_to_string(&paths.client_key).unwrap().starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&paths.client_cert, &paths.client_key, &paths.ca_cert] {
                assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        // Second run: files exist, no network needed (port 1 would fail to connect).
        assert!(config_from_jwt_at(&jwt(full_claims(1)), &paths).await.is_ok());
        // A different pinned fingerprint is rejected.
        let mut other = full_claims(1);
        other["ca_fingerprint"] = serde_json::json!("00ff");
        assert_eq!(config_from_jwt_at(&jwt(other), &paths).await.unwrap_err().to_string(), "home ca fingerprint mismatch");
    }

    #[tokio::test]
    async fn enrollment_failures_are_reported() {
        for (response, message) in [
            (serde_json::json!({"ok": false}), "home certificate request failed"),
            (serde_json::json!({"ok": true, "certificate": "", "ca": ""}), "home certificate response is incomplete"),
            (serde_json::json!({"ok": true, "certificate": "c", "ca": CA_PEM}), "home ca fingerprint mismatch"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let paths = CertificatePaths::in_dir(dir.path().join("p"));
            let (port, task) = mock_enrollment(response).await;
            let mut claims = full_claims(i64::from(port));
            if message == "home ca fingerprint mismatch" {
                claims["ca_fingerprint"] = serde_json::json!("deadbeef");
            }
            let err = config_from_jwt_at(&jwt(claims), &paths).await.unwrap_err();
            let _ = task.await;
            assert_eq!(err.to_string(), message);
            assert!(!paths.client_cert.exists(), "nothing but the key may be written on failure");
        }
    }

    #[test]
    fn existing_keys_in_pkcs1_and_pkcs8_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");
        let key = load_or_create_client_key(&path).unwrap();
        let again = load_or_create_client_key(&path).unwrap();
        assert_eq!(key, again);
        let csr = create_client_csr("node-1", &key).unwrap();
        assert!(csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
        assert!(create_client_csr("  ", &key).is_err());
        std::fs::write(&path, "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n").unwrap();
        assert!(load_or_create_client_key(&path).unwrap_err().to_string().contains("unsupported"));
    }
}
