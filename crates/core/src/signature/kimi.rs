//! Kimi thinking signature validation (Go: signature/kimi_validation.go).
//!
//! Kimi signatures carry no envelope; every byte is indistinguishable from uniform random data.
//! The only observable is size: the raw length is fixed per protocol mode (12946 characters for
//! non-streaming, 4340 for the streaming `signature_delta`). This probe is observed regularity,
//! not a protocol contract, so it must run after every self-describing validator has declined.

use super::claude::{
    is_valid_claude_cais_signature, is_valid_claude_thinking_signature,
    ClaudeSignatureValidationOptions,
};
use super::gemini::{is_valid_gemini_thought_signature, GeminiThoughtSignatureValidationOptions};
use super::gpt::first_invalid_char;
use super::grok::{byte_entropy_ratio, is_unpadded_std_base64_char};
use super::provider::{maybe_self_describing_signature_envelope, split_signature_provider_prefix};
use super::{b64, Result, SignatureError};

/// Raw character length Kimi emits for non-streaming Messages responses.
pub const KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN: usize = 12946;
/// Raw character length Kimi emits in the streaming signature_delta event.
pub const KIMI_THINKING_SIGNATURE_STREAMING_LEN: usize = 4340;

/// Keeps same-length attacker-supplied filler from claiming the family.
pub const MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO: f64 = 0.85;

/// Upstream code path that produced a signature, derived from length alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiThinkingSignatureMode {
    NonStreaming,
    Streaming,
}

impl KimiThinkingSignatureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonStreaming => "non_streaming",
            Self::Streaming => "streaming",
        }
    }
}

fn mode_for_len(len: usize) -> Option<KimiThinkingSignatureMode> {
    match len {
        KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN => Some(KimiThinkingSignatureMode::NonStreaming),
        KIMI_THINKING_SIGNATURE_STREAMING_LEN => Some(KimiThinkingSignatureMode::Streaming),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KimiThinkingSignatureInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
    pub mode: KimiThinkingSignatureMode,
}

/// Validates the transport shape of a Kimi Messages thinking signature (size, character class,
/// entropy). Proves nothing about the payload.
pub fn inspect_kimi_thinking_signature(raw: &str) -> Result<KimiThinkingSignatureInfo> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(SignatureError::new("empty Kimi thinking signature"));
    }
    if sig != raw {
        return Err(SignatureError::new(
            "Kimi thinking signature has leading or trailing whitespace",
        ));
    }
    let Some(mode) = mode_for_len(sig.len()) else {
        return Err(SignatureError::new(format!(
            "invalid Kimi thinking signature: unexpected length {}",
            sig.len()
        )));
    };
    if sig.contains('=') {
        return Err(SignatureError::new(
            "invalid Kimi thinking signature: expected unpadded standard base64",
        ));
    }
    if let Some((index, c)) = first_invalid_char(sig, is_unpadded_std_base64_char) {
        return Err(SignatureError::new(format!(
            "invalid Kimi thinking signature: contains non-base64 character U+{:04X} at byte {index}",
            u32::from(c)
        )));
    }
    if split_signature_provider_prefix(sig).is_some() {
        return Err(SignatureError::new(
            "invalid Kimi thinking signature: carries another provider's cache prefix",
        ));
    }
    // Defense in depth: a foreign envelope of coincidentally matching length is not Kimi.
    if maybe_self_describing_signature_envelope(sig) {
        if sig.starts_with("gAAAA") {
            return Err(SignatureError::new(
                "Kimi thinking signature looks like GPT/Codex reasoning signature",
            ));
        }
        if is_valid_claude_cais_signature(sig) {
            return Err(SignatureError::new(
                "Kimi thinking signature looks like Claude CAIS thinking signature",
            ));
        }
        if is_valid_claude_thinking_signature(sig, ClaudeSignatureValidationOptions::STRICT) {
            return Err(SignatureError::new(
                "Kimi thinking signature looks like Claude thinking signature",
            ));
        }
        if is_valid_gemini_thought_signature(sig, GeminiThoughtSignatureValidationOptions::KNOWN_ENVELOPE)
        {
            return Err(SignatureError::new(
                "Kimi thinking signature looks like Gemini thoughtSignature",
            ));
        }
    }
    let decoded = b64::raw_std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid Kimi thinking signature: base64 decode failed: {err}"
        ))
    })?;
    let entropy_ratio = byte_entropy_ratio(&decoded);
    if entropy_ratio < MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO {
        return Err(SignatureError::new(format!(
            "invalid Kimi thinking signature: decoded payload entropy ratio {entropy_ratio:.3} below {MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO:.3}"
        )));
    }
    Ok(KimiThinkingSignatureInfo {
        raw_len: sig.len(),
        decoded_len: decoded.len(),
        mode,
    })
}

/// Whether `raw` has the transport shape of a Kimi thinking signature.
pub fn is_valid_kimi_thinking_signature(raw: &str) -> bool {
    inspect_kimi_thinking_signature(raw).is_ok()
}
