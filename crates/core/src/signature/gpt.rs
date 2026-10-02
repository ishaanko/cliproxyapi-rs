//! GPT/Codex reasoning `encrypted_content` validation (Go: signature/gpt_validation.go).

use super::{b64, Result, SignatureError};

pub const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GptReasoningSignatureInfo {
    pub decoded_len: usize,
    pub ciphertext_len: usize,
}

pub fn is_valid_gpt_reasoning_signature(raw_signature: &str) -> bool {
    inspect_gpt_reasoning_signature(raw_signature).is_ok()
}

/// Validates the Fernet-like outer format of GPT/Codex reasoning `encrypted_content`: `gAAAA`
/// prefix, base64url alphabet, version byte 0x80, and an AES-block-multiple ciphertext. This is a
/// transport-shape check only.
pub fn inspect_gpt_reasoning_signature(raw_signature: &str) -> Result<GptReasoningSignatureInfo> {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return Err(SignatureError::new("empty GPT reasoning signature"));
    }
    if sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return Err(SignatureError::new(format!(
            "GPT reasoning signature exceeds maximum length ({MAX_GPT_REASONING_SIGNATURE_LEN} bytes)"
        )));
    }
    // The literal prefix is the cheapest discriminator, so it runs before the charset scan.
    if !sig.starts_with("gAAAA") {
        return Err(SignatureError::new(
            "invalid GPT reasoning signature: expected gAAAA prefix",
        ));
    }
    if let Some((index, c)) = first_invalid_char(sig, is_base64url_char) {
        return Err(SignatureError::new(format!(
            "invalid GPT reasoning signature: contains non-base64url character U+{:04X} at byte {index}",
            u32::from(c)
        )));
    }

    let decoded = decode(sig)?;
    if decoded.len() < 73 {
        return Err(SignatureError::new(
            "invalid GPT reasoning signature: decoded payload too short",
        ));
    }
    if decoded[0] != 0x80 {
        return Err(SignatureError::new(format!(
            "invalid GPT reasoning signature: expected version 0x80, got 0x{:02x}",
            decoded[0]
        )));
    }

    let ciphertext_len = decoded.len() as i64 - 1 - 8 - 16 - 32;
    if ciphertext_len <= 0 || ciphertext_len % 16 != 0 {
        return Err(SignatureError::new(format!(
            "invalid GPT reasoning signature: ciphertext length {ciphertext_len} is not a positive AES block multiple"
        )));
    }

    Ok(GptReasoningSignatureInfo {
        decoded_len: decoded.len(),
        ciphertext_len: ciphertext_len as usize,
    })
}

fn decode(sig: &str) -> Result<Vec<u8>> {
    if let Ok(decoded) = b64::raw_url(sig) {
        return Ok(decoded);
    }
    if let Ok(decoded) = b64::url(sig) {
        return Ok(decoded);
    }
    Err(SignatureError::new(
        "invalid GPT reasoning signature: base64url decode failed",
    ))
}

/// Base64url alphabet, padding included.
fn is_base64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=')
}

/// Byte offset and character of the first byte outside the alphabet. Every legal character is
/// ASCII, so the offending character is decoded only for the error message.
pub(super) fn first_invalid_char(sig: &str, allowed: fn(u8) -> bool) -> Option<(usize, char)> {
    let index = sig.bytes().position(|b| !allowed(b))?;
    // A non-ASCII byte is never allowed, so `index` is always a char boundary.
    sig[index..].chars().next().map(|c| (index, c))
}
