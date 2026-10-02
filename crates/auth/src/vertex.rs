//! Vertex AI service-account credentials (internal/auth/vertex/*, plus the import logic of
//! internal/cmd/vertex_import.go and the management `vertex/import` handler).
//!
//! There is no OAuth flow: a service-account JSON is normalized (the PEM `private_key` is repaired
//! and rewritten as PKCS#1 `RSA PRIVATE KEY`) and stored as `vertex-<project>.json`.
//!
//! Gap vs Go: key validation checks the ASN.1 structure (PKCS#1 / PKCS#8 RSA) but does not run
//! Go's arithmetic `Validate()` (prime product check).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pkcs1::RsaPrivateKey;
use pkcs8::PrivateKeyInfo;
use serde_json::Value;

use crate::credmeta::Metadata;
use crate::error::{AuthFlowError, Result};
use crate::storage::{TokenStorage, VertexCredentialStorage};
use crate::types::Auth;
use crate::util::marshal_compact;

/// `rsaEncryption` algorithm OID.
const RSA_ENCRYPTION_OID: &str = "1.2.840.113549.1.1.1";
pub const DEFAULT_LOCATION: &str = "us-central1";

/// `NormalizeServiceAccountJSON`: parse, repair the private key, re-marshal (sorted compact).
pub fn normalize_service_account_json(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let payload: Value = serde_json::from_slice(raw)
        .map_err(|e| AuthFlowError::other(format!("invalid service account json: {e}")))?;
    let Value::Object(map) = payload else {
        return Err(AuthFlowError::other("service account payload is empty"));
    };
    let normalized = normalize_service_account_map(&map)?;
    marshal_compact(&Value::Object(normalized))
        .map(String::into_bytes)
        .map_err(|e| AuthFlowError::other(e.to_string()))
}

/// `NormalizeServiceAccountMap`: returns a copy with `private_key` rewritten as PKCS#1 PEM.
pub fn normalize_service_account_map(sa: &Metadata) -> Result<Metadata> {
    let pk = sa.get("private_key").and_then(Value::as_str).unwrap_or("");
    if pk.trim().is_empty() {
        return Err(AuthFlowError::other("service account missing private_key"));
    }
    let normalized = sanitize_private_key(pk)?;
    let mut clone = sa.clone();
    clone.insert("private_key".into(), Value::String(normalized));
    Ok(clone)
}

fn sanitize_private_key(raw: &str) -> Result<String> {
    let mut pk = raw.replace("\r\n", "\n").replace('\r', "\n");
    pk = strip_ansi_escape(&pk);
    pk = pk.trim().to_string();

    let normalized = if pem_decode(&pk).is_some() {
        pk
    } else {
        rebuild_pem(&pk)
            .map_err(|e| AuthFlowError::other(format!("private_key is not valid pem: {e}")))?
    };
    let (_, der) = pem_decode(&normalized)
        .ok_or_else(|| AuthFlowError::other("private_key pem decode failed"))?;
    let (kind, pkcs1_der) =
        ensure_rsa_private_key(&pem_type(&normalized).unwrap_or_default(), &der)?;
    Ok(pem_encode(kind, &pkcs1_der))
}

/// Returns the PEM type of the first block.
fn pem_type(text: &str) -> Option<String> {
    pem_decode(text).map(|(t, _)| t)
}

/// Converts any accepted RSA key encoding to `("RSA PRIVATE KEY", pkcs1_der)`.
fn ensure_rsa_private_key(block_type: &str, der: &[u8]) -> Result<(&'static str, Vec<u8>)> {
    const KIND: &str = "RSA PRIVATE KEY";
    match block_type {
        "RSA PRIVATE KEY" => {
            RsaPrivateKey::try_from(der)
                .map_err(|e| AuthFlowError::other(format!("private_key invalid rsa: {e}")))?;
            Ok((KIND, der.to_vec()))
        }
        "PRIVATE KEY" => {
            let info = PrivateKeyInfo::try_from(der)
                .map_err(|e| AuthFlowError::other(format!("private_key invalid pkcs8: {e}")))?;
            if info.algorithm.oid.to_string() != RSA_ENCRYPTION_OID {
                return Err(AuthFlowError::other("private_key is not an RSA key"));
            }
            RsaPrivateKey::try_from(info.private_key)
                .map_err(|e| AuthFlowError::other(format!("private_key invalid rsa: {e}")))?;
            Ok((KIND, info.private_key.to_vec()))
        }
        _ => {
            if RsaPrivateKey::try_from(der).is_ok() {
                return Ok((KIND, der.to_vec()));
            }
            if let Ok(info) = PrivateKeyInfo::try_from(der)
                && info.algorithm.oid.to_string() == RSA_ENCRYPTION_OID
                && RsaPrivateKey::try_from(info.private_key).is_ok()
            {
                return Ok((KIND, info.private_key.to_vec()));
            }
            Err(AuthFlowError::other("private_key uses unsupported format"))
        }
    }
}

/// Rebuilds a PEM from mangled text: finds the BEGIN/END markers, keeps only base64 characters in
/// between and re-encodes.
fn rebuild_pem(raw: &str) -> std::result::Result<String, String> {
    let kind = if raw.contains("RSA PRIVATE KEY") {
        "RSA PRIVATE KEY"
    } else {
        "PRIVATE KEY"
    };
    let header = format!("-----BEGIN {kind}-----");
    let footer = format!("-----END {kind}-----");
    let start = raw.find(&header);
    let end = raw.find(&footer);
    let (Some(start), Some(end)) = (start, end) else {
        return Err("missing pem markers".into());
    };
    if end <= start {
        return Err("missing pem markers".into());
    }
    let body = &raw[start + header.len()..end];
    let payload: String = body
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    if payload.is_empty() {
        return Err("private_key base64 payload empty".into());
    }
    let der = STANDARD
        .decode(payload)
        .map_err(|e| format!("private_key base64 decode failed: {e}"))?;
    Ok(pem_encode(kind, &der))
}

/// First PEM block: `(type, der)`. Ignores text before the block; fails on bad base64.
fn pem_decode(text: &str) -> Option<(String, Vec<u8>)> {
    let begin_idx = text.find("-----BEGIN ")?;
    let rest = &text[begin_idx + "-----BEGIN ".len()..];
    let type_end = rest.find("-----")?;
    let kind = rest[..type_end].to_string();
    let after_header = &rest[type_end + 5..];
    let footer = format!("-----END {kind}-----");
    let end_idx = after_header.find(&footer)?;
    let body = &after_header[..end_idx];
    // Header lines (`Key: value`) before the base64 body are skipped like Go's pem.Decode.
    let b64: String = body
        .lines()
        .filter(|l| !l.contains(':'))
        .flat_map(|l| l.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    STANDARD.decode(b64).ok().map(|der| (kind, der))
}

/// PEM-encodes with 64-column lines and a trailing newline (Go `pem.EncodeToMemory`).
fn pem_encode(kind: &str, der: &[u8]) -> String {
    let b64 = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {kind}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {kind}-----\n"));
    out
}

/// Removes OSC (`ESC ]`), CSI (`ESC [`) and lone ESC sequences, e.g. from copy/pasted terminal output.
fn strip_ansi_escape(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\u{1b}' {
            out.push(c);
            i += 1;
            continue;
        }
        if i + 1 >= chars.len() {
            i += 1;
            continue;
        }
        match chars[i + 1] {
            ']' => {
                i += 2;
                while i < chars.len() {
                    if chars[i] == '\u{7}' {
                        break;
                    }
                    if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '[' => {
                i += 2;
                while i < chars.len() && !chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Result of importing a service account.
#[derive(Debug, Clone)]
pub struct VertexImport {
    pub auth: Auth,
    pub project_id: String,
    pub email: String,
    pub location: String,
}

/// Imports a service-account JSON as an `Auth` ready to save: management (`prefix = None`) and CLI
/// (`prefix = Some(..)`) variants. `location` defaults to `us-central1`.
pub fn import_service_account(
    raw: &[u8],
    location: &str,
    prefix: Option<&str>,
) -> Result<VertexImport> {
    let parsed: Value = serde_json::from_slice(raw)
        .map_err(|e| AuthFlowError::other(format!("invalid json: {e}")))?;
    let Value::Object(sa) = parsed else {
        return Err(AuthFlowError::other(
            "invalid service account: service account payload is empty",
        ));
    };
    let sa = normalize_service_account_map(&sa)
        .map_err(|e| AuthFlowError::other(format!("invalid service account: {e}")))?;

    let value_as_string = |k: &str| match sa.get(k) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let project_id = value_as_string("project_id").trim().to_string();
    if project_id.is_empty() {
        return Err(AuthFlowError::other("project_id missing"));
    }
    let email = value_as_string("client_email").trim().to_string();
    let location = if location.trim().is_empty() {
        DEFAULT_LOCATION.to_string()
    } else {
        location.trim().to_string()
    };

    let prefix_clean = prefix.map(|p| p.trim().trim_matches('/').to_string());
    if let Some(p) = &prefix_clean
        && p.contains('/')
    {
        return Err(AuthFlowError::other(format!(
            "prefix must be a single segment (no '/' allowed): {p:?}"
        )));
    }

    let mut base_name = sanitize_file_part(&project_id);
    if base_name.is_empty() {
        base_name = "vertex".to_string();
    }
    if let Some(p) = prefix_clean.as_deref().filter(|p| !p.is_empty()) {
        base_name = format!(
            "{}-{}",
            sanitize_file_part(p),
            sanitize_file_part(&project_id)
        );
    }
    let file_name = format!("vertex-{base_name}.json");
    let label = label_for_vertex(&project_id, &email);

    let storage = VertexCredentialStorage {
        service_account: sa.clone(),
        project_id: project_id.clone(),
        email: email.clone(),
        location: location.clone(),
        type_: "vertex".into(),
        prefix: prefix_clean.clone().unwrap_or_default(),
    };
    let mut metadata = Metadata::new();
    metadata.insert("service_account".into(), Value::Object(sa));
    metadata.insert("project_id".into(), project_id.clone().into());
    metadata.insert("email".into(), email.clone().into());
    metadata.insert("location".into(), location.clone().into());
    metadata.insert("type".into(), "vertex".into());
    if let Some(p) = &prefix_clean {
        metadata.insert("prefix".into(), p.clone().into());
    }
    metadata.insert("label".into(), label.clone().into());

    let mut auth = Auth::new(file_name, "vertex");
    auth.label = label;
    auth.storage = Some(TokenStorage::Vertex(storage));
    auth.metadata = metadata;
    Ok(VertexImport {
        auth,
        project_id,
        email,
        location,
    })
}

/// `/ \ :` become `_`, spaces become `-`.
fn sanitize_file_part(s: &str) -> String {
    s.trim().replace(['/', '\\', ':'], "_").replace(' ', "-")
}

fn label_for_vertex(project_id: &str, email: &str) -> String {
    let (p, e) = (project_id.trim(), email.trim());
    match (p.is_empty(), e.is_empty()) {
        (false, false) => format!("{p} ({e})"),
        (false, true) => p.to_string(),
        (true, false) => e.to_string(),
        (true, true) => "vertex".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// PKCS#1 DER of a tiny but structurally valid RSAPrivateKey (all-ones integers); enough for
    /// structure validation, not for crypto.
    fn fake_pkcs1_der() -> Vec<u8> {
        // SEQUENCE { INTEGER 0, then eight INTEGER 1 }
        let mut body = vec![0x02, 0x01, 0x00];
        for _ in 0..8 {
            body.extend_from_slice(&[0x02, 0x01, 0x01]);
        }
        let mut der = vec![0x30, body.len() as u8];
        der.extend(body);
        der
    }

    fn pkcs8_wrap(pkcs1: &[u8]) -> Vec<u8> {
        // SEQUENCE { INTEGER 0, SEQUENCE { OID rsaEncryption, NULL }, OCTET STRING pkcs1 }
        let alg = [
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
            0x00,
        ];
        let mut body = vec![0x02, 0x01, 0x00];
        body.extend_from_slice(&alg);
        body.push(0x04);
        body.push(pkcs1.len() as u8);
        body.extend_from_slice(pkcs1);
        let mut der = vec![0x30, body.len() as u8];
        der.extend(body);
        der
    }

    #[test]
    fn pkcs8_key_is_rewritten_as_pkcs1() {
        let pkcs1 = fake_pkcs1_der();
        let pem = pem_encode("PRIVATE KEY", &pkcs8_wrap(&pkcs1));
        let out = sanitize_private_key(&pem).unwrap();
        assert!(
            out.starts_with("-----BEGIN RSA PRIVATE KEY-----\n")
                && out.ends_with("-----END RSA PRIVATE KEY-----\n")
        );
        let (kind, der) = pem_decode(&out).unwrap();
        assert_eq!(kind, "RSA PRIVATE KEY");
        assert_eq!(der, pkcs1);
    }

    #[test]
    fn mangled_pem_is_rebuilt() {
        let pkcs1 = fake_pkcs1_der();
        let b64 = STANDARD.encode(&pkcs1);
        // One long line with ANSI noise, stray spaces and CRLF endings.
        let spaced = b64.replace('A', "A ");
        let mangled = format!(
            "\u{1b}[0m-----BEGIN RSA PRIVATE KEY-----  {spaced}\r\n  -----END RSA PRIVATE KEY-----\r\n"
        );
        let out = sanitize_private_key(&mangled).unwrap();
        assert_eq!(pem_decode(&out).unwrap().1, pkcs1);
    }

    #[test]
    fn rejects_non_rsa_and_garbage() {
        assert!(sanitize_private_key("not a key").is_err());
        let ec_like = pem_encode("PRIVATE KEY", &[0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(sanitize_private_key(&ec_like).is_err());
        assert!(normalize_service_account_map(&Metadata::new()).is_err());
    }

    #[test]
    fn import_builds_auth_like_management_handler() {
        let pem = pem_encode("RSA PRIVATE KEY", &fake_pkcs1_der());
        let sa = json!({"type": "service_account", "project_id": "my proj/1", "client_email": "sa@p.iam", "private_key": pem});
        let imp = import_service_account(sa.to_string().as_bytes(), "", None).unwrap();
        assert_eq!(imp.auth.id, "vertex-my-proj_1.json");
        assert_eq!(imp.auth.label, "my proj/1 (sa@p.iam)");
        assert_eq!(imp.location, "us-central1");
        assert!(!imp.auth.metadata.contains_key("prefix"));

        let imp =
            import_service_account(sa.to_string().as_bytes(), "europe-west4", Some(" /team/ "))
                .unwrap();
        assert_eq!(imp.auth.id, "vertex-team-my-proj_1.json");
        assert_eq!(imp.auth.metadata["prefix"], "team");

        let missing = json!({"private_key": pem});
        assert!(
            import_service_account(missing.to_string().as_bytes(), "", None)
                .unwrap_err()
                .to_string()
                .contains("project_id missing")
        );
        assert!(import_service_account(b"{", "", None).is_err());
    }
}
