//! Service-account access tokens for Vertex AI (Go: vertexAccessToken, which delegates to
//! `golang.org/x/oauth2/google` with the `cloud-platform` scope).
//!
//! The flow is the standard two-legged JWT bearer grant: sign an RS256 assertion with the service
//! account key and exchange it at `token_uri`. Go builds a fresh token source per request; here
//! tokens are cached until shortly before expiry, which is invisible to callers.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Tokens are refreshed this long before they expire (oauth2's `expiryDelta` is 10s; extra margin
/// covers slow requests that start right before expiry).
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const ASSERTION_LIFETIME_SECS: i64 = 3600;

static TOKEN_CACHE: LazyLock<Mutex<HashMap<String, (String, Instant)>>> = LazyLock::new(Default::default);

/// Access token for the (already normalized) service account JSON object, minted via `client`.
pub(crate) async fn access_token(
    client: &reqwest::Client,
    service_account: &Map<String, Value>,
) -> Result<String, String> {
    let str_field = |key: &str| service_account.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let client_email = str_field("client_email");
    let private_key = str_field("private_key");
    let key_id = str_field("private_key_id");
    let mut token_url = str_field("token_uri");
    if token_url.is_empty() {
        token_url = DEFAULT_TOKEN_URL.to_string();
    }

    let cache_key =
        hex::encode(Sha256::digest(format!("{client_email}\n{key_id}\n{token_url}\n{private_key}").as_bytes()));
    if let Some((token, expires)) = TOKEN_CACHE.lock().get(&cache_key)
        && Instant::now() + EXPIRY_MARGIN < *expires
    {
        return Ok(token.clone());
    }

    let assertion = sign_assertion(&client_email, &key_id, &private_key, &token_url)?;
    let resp = client
        .post(&token_url)
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", assertion.as_str())])
        .send()
        .await
        .map_err(|e| format!("oauth2: cannot fetch token: {e}"))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| format!("oauth2: cannot fetch token: {e}"))?;
    if !status.is_success() {
        return Err(format!("oauth2: cannot fetch token: {status}\nResponse: {}", String::from_utf8_lossy(&body)));
    }
    let parsed: Value = serde_json::from_slice(&body).map_err(|e| format!("oauth2: cannot parse json: {e}"))?;
    let token = parsed.get("access_token").and_then(Value::as_str).unwrap_or("").to_string();
    if token.is_empty() {
        return Err("oauth2: server response missing access_token".into());
    }
    if let Some(secs) = parsed.get("expires_in").and_then(Value::as_u64).filter(|s| *s > 0) {
        TOKEN_CACHE.lock().insert(cache_key, (token.clone(), Instant::now() + Duration::from_secs(secs)));
    }
    Ok(token)
}

/// Builds the RS256 JWT assertion for the token endpoint.
fn sign_assertion(client_email: &str, key_id: &str, private_key_pem: &str, token_url: &str) -> Result<String, String> {
    let der = pem_to_der(private_key_pem)?;
    let key = RsaKeyPair::from_der(&der)
        .or_else(|_| RsaKeyPair::from_pkcs8(&der))
        .map_err(|e| format!("private key parse error: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    let mut header = json!({"alg": "RS256", "typ": "JWT"});
    if !key_id.is_empty() {
        header["kid"] = json!(key_id);
    }
    let claims = json!({
        "iss": client_email,
        "scope": SCOPE,
        "aud": token_url,
        "exp": now + ASSERTION_LIFETIME_SECS,
        "iat": now,
    });
    let encode = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let signing_input = format!("{}.{}", encode(&header), encode(&claims));
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| "failed to sign jwt assertion".to_string())?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// Base64 body of the first PEM block.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("-----")).collect();
    STANDARD.decode(body).map_err(|e| format!("private key is not valid pem: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::test_support::{json_reply, mock_upstream};
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Runs openssl with `stdin`, `None` when openssl is unavailable.
    fn openssl(args: &[&str], stdin: &[u8]) -> Option<Vec<u8>> {
        let mut child = Command::new("openssl")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin).ok()?;
        let out = child.wait_with_output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    #[test]
    fn pem_body_is_decoded_and_garbage_rejected() {
        let pem =
            format!("-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\n", STANDARD.encode(b"abc"));
        assert_eq!(pem_to_der(&pem).unwrap(), b"abc");
        assert!(pem_to_der("-----BEGIN X-----\n!!!\n-----END X-----").is_err());
        assert!(sign_assertion("a@b", "", "-----BEGIN X-----\nYWJj\n-----END X-----", DEFAULT_TOKEN_URL).is_err());
    }

    /// The assertion is a verifiable RS256 JWT carrying the service account identity, and the
    /// minted token is reused until shortly before it expires.
    #[tokio::test]
    async fn token_exchange_signs_a_verifiable_jwt_and_caches() {
        let Some(key_pem) = openssl(&["genrsa", "-traditional", "2048"], b"") else {
            eprintln!("openssl unavailable, skipping");
            return;
        };
        let public_der = openssl(&["rsa", "-RSAPublicKey_out", "-outform", "DER"], &key_pem).expect("public key");
        let (base, mut seen) = mock_upstream(vec![json_reply(r#"{"access_token":"tok-1","expires_in":3600}"#)]).await;
        let token_uri = format!("{base}/token");
        let mut sa = Map::new();
        sa.insert("client_email".into(), json!("svc@proj.iam.gserviceaccount.com"));
        sa.insert("private_key_id".into(), json!("kid-1"));
        sa.insert("private_key".into(), json!(String::from_utf8(key_pem).unwrap()));
        sa.insert("token_uri".into(), json!(token_uri));

        let client = reqwest::Client::new();
        assert_eq!(access_token(&client, &sa).await.unwrap(), "tok-1");
        let captured = seen.recv().await.unwrap();
        assert_eq!(captured.path(), "/token");
        let form: HashMap<String, String> = url::form_urlencoded::parse(&captured.body).into_owned().collect();
        assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
        let jwt = &form["assertion"];
        let parts: Vec<&str> = jwt.split('.').collect();
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            (header["alg"].as_str(), header["typ"].as_str(), header["kid"].as_str()),
            (Some("RS256"), Some("JWT"), Some("kid-1"))
        );
        assert_eq!(claims["iss"], "svc@proj.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["aud"], token_uri.as_str());
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_der)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies");

        assert_eq!(access_token(&client, &sa).await.unwrap(), "tok-1");
        assert!(seen.try_recv().is_err(), "second call must come from the cache");
    }
}
