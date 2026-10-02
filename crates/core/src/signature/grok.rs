//! xAI/Grok `encrypted_content` validation (Go: signature/grok_validation.go).
//!
//! Not a provider classifier: xAI emits no envelope, so every high-entropy unpadded standard
//! base64 blob passes. Callers must establish provenance first (provider cache prefix or a
//! confirmed xAI target model) and treat this as a replay-safety check.

use super::claude::{
    is_valid_claude_cais_signature, is_valid_claude_thinking_signature,
    ClaudeSignatureValidationOptions,
};
use super::gemini::{inspect_gemini_thought_signature, GeminiThoughtSignatureValidationOptions};
use super::gpt::first_invalid_char;
use super::kimi::is_valid_kimi_thinking_signature;
use super::provider::{maybe_self_describing_signature_envelope, split_signature_provider_prefix};
use super::{b64, Result, SignatureError};

/// Transport safety cap for opaque replay blobs.
pub const MAX_GROK_ENCRYPTED_CONTENT_LEN: usize = 8 * 1024 * 1024;
/// Deliberately loose floor; the entropy check does the real filtering.
pub const MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN: usize = 32;
/// Rejects obvious non-ciphertext payloads (native samples are >= 0.892).
pub const MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO: f64 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokEncryptedContentInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
}

/// Validates the transport shape of xAI/Grok reasoning or compaction `encrypted_content`.
pub fn inspect_grok_encrypted_content(raw: &str) -> Result<GrokEncryptedContentInfo> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(SignatureError::new("empty Grok encrypted_content"));
    }
    if sig.len() > MAX_GROK_ENCRYPTED_CONTENT_LEN {
        return Err(SignatureError::new(format!(
            "Grok encrypted_content exceeds maximum length ({MAX_GROK_ENCRYPTED_CONTENT_LEN} bytes)"
        )));
    }
    if sig != raw {
        return Err(SignatureError::new(
            "Grok encrypted_content has leading or trailing whitespace",
        ));
    }
    if sig.contains('=') {
        return Err(SignatureError::new(
            "invalid Grok encrypted_content: expected unpadded standard base64",
        ));
    }
    if let Some((index, c)) = first_invalid_char(sig, is_unpadded_std_base64_char) {
        return Err(SignatureError::new(format!(
            "invalid Grok encrypted_content: contains non-base64 character U+{:04X} at byte {index}",
            u32::from(c)
        )));
    }
    if split_signature_provider_prefix(sig).is_some() {
        return Err(SignatureError::new(
            "invalid Grok encrypted_content: carries another provider's cache prefix",
        ));
    }
    // Foreign-envelope rejection only matters for the base64 first characters a self-describing
    // envelope can produce; native xAI ciphertext skips this chain for ~92% of traffic.
    if maybe_self_describing_signature_envelope(sig) {
        if sig.starts_with("gAAAA") {
            return Err(SignatureError::new(
                "Grok encrypted_content looks like GPT/Codex reasoning signature",
            ));
        }
        if is_valid_claude_thinking_signature(sig, ClaudeSignatureValidationOptions::STRICT) {
            return Err(SignatureError::new(
                "Grok encrypted_content looks like Claude thinking signature",
            ));
        }
        if is_valid_claude_cais_signature(sig) {
            return Err(SignatureError::new(
                "Grok encrypted_content looks like Claude CAIS thinking signature",
            ));
        }
        if inspect_gemini_thought_signature(sig, GeminiThoughtSignatureValidationOptions::KNOWN_ENVELOPE)
            .is_ok()
        {
            return Err(SignatureError::new(
                "Grok encrypted_content looks like Gemini thoughtSignature",
            ));
        }
    }
    // Kimi emits no envelope either, so this runs unconditionally; the two fixed Kimi lengths
    // never occur in native Grok traffic.
    if is_valid_kimi_thinking_signature(sig) {
        return Err(SignatureError::new(
            "Grok encrypted_content has a Kimi thinking signature length",
        ));
    }

    let decoded = b64::raw_std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid Grok encrypted_content: base64 decode failed: {err}"
        ))
    })?;
    if decoded.len() < MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN {
        return Err(SignatureError::new(format!(
            "invalid Grok encrypted_content: decoded payload too short ({} bytes)",
            decoded.len()
        )));
    }
    let entropy_ratio = byte_entropy_ratio(&decoded);
    if entropy_ratio < MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO {
        return Err(SignatureError::new(format!(
            "invalid Grok encrypted_content: decoded payload entropy ratio {entropy_ratio:.3} below {MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO:.3}"
        )));
    }
    Ok(GrokEncryptedContentInfo {
        raw_len: sig.len(),
        decoded_len: decoded.len(),
    })
}

pub fn is_valid_grok_encrypted_content(raw: &str) -> bool {
    inspect_grok_encrypted_content(raw).is_ok()
}

/// Unpadded standard base64 alphabet (shared with the Kimi validator).
pub(super) fn is_unpadded_std_base64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/')
}

/// Shannon entropy of `buf` divided by the entropy ceiling for its size
/// (`log2(min(len, 256))`); 0 for buffers of at most one byte.
pub(super) fn byte_entropy_ratio(buf: &[u8]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in buf {
        counts[usize::from(b)] += 1;
    }
    let n = buf.len() as f64;
    let mut entropy = 0.0;
    for &count in counts.iter().filter(|&&c| c != 0) {
        let p = count as f64 / n;
        entropy -= p * p.log2();
    }
    let max_symbols = buf.len().min(256);
    if max_symbols <= 1 {
        return 0.0;
    }
    entropy / (max_symbols as f64).log2()
}
