//! PKCE (RFC 7636, S256) code generation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// `base64url(sha256(verifier))`, unpadded.
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn generate_with(random_bytes: usize) -> PkceCodes {
    let mut buf = vec![0u8; random_bytes];
    rand::rng().fill_bytes(&mut buf);
    let code_verifier = URL_SAFE_NO_PAD.encode(&buf);
    let code_challenge = code_challenge(&code_verifier);
    PkceCodes { code_verifier, code_challenge }
}

/// Claude / Codex: 96 random bytes, 128-char verifier.
pub fn generate_pkce_codes() -> PkceCodes {
    generate_with(96)
}

/// Devin: 64 random bytes, 86-char verifier.
pub fn generate_pkce_codes_short() -> PkceCodes {
    generate_with(64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_lengths_and_alphabet() {
        let c = generate_pkce_codes();
        assert_eq!(c.code_verifier.len(), 128);
        assert_eq!(c.code_challenge.len(), 43);
        assert!(c.code_verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(c.code_challenge, code_challenge(&c.code_verifier));
        assert_eq!(generate_pkce_codes_short().code_verifier.len(), 86);
        assert_ne!(generate_pkce_codes().code_verifier, generate_pkce_codes().code_verifier);
    }
}
