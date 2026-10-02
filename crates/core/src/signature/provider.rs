//! Provider detection and replay-compatibility policy for signed reasoning blocks
//! (Go: signature/provider_compatibility.go).

use super::claude::{
    inspect_claude_cais_signature, is_valid_claude_cais_signature,
    is_valid_claude_thinking_signature, normalize_claude_provider_native_thinking_signature,
    normalize_claude_thinking_signature, ClaudeSignatureValidationOptions,
};
use super::gemini::{
    is_gemini_thought_signature_bypass, is_valid_gemini_thought_signature,
    GeminiThoughtSignatureValidationOptions, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
};
use super::gpt::is_valid_gpt_reasoning_signature;
use super::grok::is_valid_grok_encrypted_content;
use super::kimi::is_valid_kimi_thinking_signature;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SignatureProvider {
    #[default]
    Unknown,
    Claude,
    Gemini,
    GeminiBypass,
    Gpt,
    /// Identified by fixed signature size rather than by an envelope.
    Kimi,
    /// Target-only family: detection never returns it (xAI emits no envelope). Establish the
    /// target from the model or route, then use `inspect_grok_encrypted_content` as a
    /// replay-safety check.
    Grok,
    /// Cognition's SWE family emitting `sealed.v1` envelopes.
    Swe,
}

impl SignatureProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::GeminiBypass => "gemini_bypass",
            Self::Gpt => "gpt",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Swe => "swe",
        }
    }
}

impl std::fmt::Display for SignatureProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SignatureBlockKind {
    #[default]
    Unknown,
    ClaudeThinking,
    GeminiModelPart,
    GeminiFunctionCall,
    GptReasoning,
}

impl SignatureBlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ClaudeThinking => "claude_thinking",
            Self::GeminiModelPart => "gemini_model_part",
            Self::GeminiFunctionCall => "gemini_function_call",
            Self::GptReasoning => "gpt_reasoning",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SignatureCompatibilityAction {
    /// Go's zero value (empty string) before a decision is made.
    #[default]
    None,
    Preserve,
    DropBlock,
    DropSignature,
    ReplaceWithGeminiBypass,
    NoCompatibleReplacement,
}

impl SignatureCompatibilityAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Preserve => "preserve",
            Self::DropBlock => "drop_block",
            Self::DropSignature => "drop_signature",
            Self::ReplaceWithGeminiBypass => "replace_with_gemini_bypass",
            Self::NoCompatibleReplacement => "no_compatible_replacement",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureCompatibilityDecision {
    pub target_provider: SignatureProvider,
    pub detected_provider: SignatureProvider,
    pub block_kind: SignatureBlockKind,
    pub compatible: bool,
    pub action: SignatureCompatibilityAction,
    pub replacement_signature: String,
    pub normalized_signature: String,
    pub reason: String,
}

/// Maps common model names to the provider family whose signed history can be safely replayed.
pub fn signature_provider_from_model_name(model_name: &str) -> SignatureProvider {
    let lower = model_name.trim().to_lowercase();
    if lower.contains("claude") {
        SignatureProvider::Claude
    } else if lower.contains("gemini") {
        SignatureProvider::Gemini
    } else if lower.contains("gpt")
        || lower.contains("openai")
        || lower.contains("codex")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        SignatureProvider::Gpt
    } else if lower.contains("kimi")
        || lower.contains("moonshot")
        || lower.starts_with("k2")
        || lower.starts_with("k3")
    {
        SignatureProvider::Kimi
    } else if lower.contains("grok") {
        SignatureProvider::Grok
    } else if lower.contains("swe-") {
        SignatureProvider::Swe
    } else {
        SignatureProvider::Unknown
    }
}

/// Base64 first characters a self-describing envelope can produce (the first character is the
/// first payload byte shifted right by two): `C` Claude CAIS (0x08), `E` Claude single-layer and
/// Gemini field-2 envelope (0x12), `R` Claude double-layer, `g` GPT Fernet (0x80). Gemini's
/// ascii_uuid form is deliberately absent: it is never replay-safe.
const SELF_DESCRIBING_SIGNATURE_FIRST_CHARS: &[u8] = b"CERg";

/// Structural pre-filter: a false result is conclusive, true only narrows the candidates. Opaque
/// ciphertext (Grok) is rejected with one comparison and no allocation.
pub(super) fn maybe_self_describing_signature_envelope(raw_signature: &str) -> bool {
    raw_signature
        .as_bytes()
        .first()
        .is_some_and(|b| SELF_DESCRIBING_SIGNATURE_FIRST_CHARS.contains(b))
}

/// Classifies the provider family that can replay `raw_signature`.
pub fn detect_signature_provider(raw_signature: &str) -> SignatureProvider {
    detect_signature_provider_for_block(raw_signature, SignatureBlockKind::Unknown)
}

/// Classifies `raw_signature` with block-kind context. UUID-shaped payloads are deliberately not
/// classified as replay-safe provider signatures (Gemini callers replace them with the bypass
/// sentinel). Probe order: provider prefix, `#` present, bypass sentinel, `sealed.v1.`, then (for
/// first characters in `CERg`) GPT, Claude CAIS, Claude strict, Gemini envelope, and finally the
/// Kimi length probe.
pub fn detect_signature_provider_for_block(
    raw_signature: &str,
    block_kind: SignatureBlockKind,
) -> SignatureProvider {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return SignatureProvider::Unknown;
    }

    if let Some((prefixed_provider, unprefixed)) = split_signature_provider_prefix(sig) {
        match prefixed_provider {
            SignatureProvider::Gemini => {
                if is_gemini_thought_signature_bypass(&unprefixed) {
                    return SignatureProvider::GeminiBypass;
                }
                if is_recognized_gemini_provider_signature(&unprefixed, block_kind) {
                    return SignatureProvider::Gemini;
                }
            }
            SignatureProvider::Claude => {
                if is_valid_claude_thinking_signature(&unprefixed, ClaudeSignatureValidationOptions::STRICT)
                    || is_valid_claude_cais_signature(&unprefixed)
                {
                    return SignatureProvider::Claude;
                }
            }
            SignatureProvider::Gpt if is_valid_gpt_reasoning_signature(&unprefixed) => {
                return SignatureProvider::Gpt;
            }
            SignatureProvider::Swe if unprefixed.starts_with("sealed.v1.") => {
                return SignatureProvider::Swe;
            }
            _ => {}
        }
        return SignatureProvider::Unknown;
    }
    if sig.contains('#') {
        return SignatureProvider::Unknown;
    }

    // The bypass sentinel is a plain literal, matched before the structural pre-filter.
    if is_gemini_thought_signature_bypass(sig) {
        return SignatureProvider::GeminiBypass;
    }
    if sig.starts_with("sealed.v1.") {
        return SignatureProvider::Swe;
    }
    // Probes run from the strongest marker to the weakest. The envelope pre-filter gates only the
    // envelope probes: Kimi's uniformly distributed base64 starts with one of "CERg" about 6% of
    // the time and must still reach the size probe.
    if maybe_self_describing_signature_envelope(sig) {
        if is_valid_gpt_reasoning_signature(sig) {
            return SignatureProvider::Gpt;
        }
        if is_valid_claude_cais_signature(sig) {
            return SignatureProvider::Claude;
        }
        if is_valid_claude_thinking_signature(sig, ClaudeSignatureValidationOptions::STRICT) {
            return SignatureProvider::Claude;
        }
        if is_recognized_gemini_provider_signature(sig, block_kind) {
            return SignatureProvider::Gemini;
        }
    }
    // Kimi carries no envelope, so it is claimed last: a length coincidence can never capture
    // another provider's signature.
    if is_valid_kimi_thinking_signature(sig) {
        return SignatureProvider::Kimi;
    }
    SignatureProvider::Unknown
}

pub fn is_signature_compatible_with_provider(
    target_provider: SignatureProvider,
    raw_signature: &str,
) -> bool {
    decide_signature_compatibility(target_provider, raw_signature, SignatureBlockKind::Unknown)
        .compatible
}

/// The safe handling policy for replaying a signed block into `target_provider`.
pub fn decide_signature_compatibility(
    target_provider: SignatureProvider,
    raw_signature: &str,
    block_kind: SignatureBlockKind,
) -> SignatureCompatibilityDecision {
    decide_signature_compatibility_for_model(target_provider, "", raw_signature, block_kind)
}

/// The safe handling policy for replaying a signed block into `target_provider` for
/// `target_model`.
pub fn decide_signature_compatibility_for_model(
    target_provider: SignatureProvider,
    target_model: &str,
    raw_signature: &str,
    block_kind: SignatureBlockKind,
) -> SignatureCompatibilityDecision {
    let target_provider = normalize_signature_target_provider(target_provider);

    let detected = detect_signature_provider_for_block(raw_signature, block_kind);
    let mut decision = SignatureCompatibilityDecision {
        target_provider,
        detected_provider: detected,
        block_kind,
        ..Default::default()
    };

    if signature_provider_matches_target(target_provider, detected) {
        decision.compatible = true;
        decision.action = SignatureCompatibilityAction::Preserve;
        decision.normalized_signature =
            normalize_compatible_signature_for_provider(target_provider, raw_signature, block_kind);
        decision.reason =
            claude_compatible_signature_reason(target_provider, raw_signature, target_model);
        return decision;
    }

    decision.compatible = false;
    match target_provider {
        SignatureProvider::Gemini => {
            if matches!(
                block_kind,
                SignatureBlockKind::GeminiFunctionCall
                    | SignatureBlockKind::GeminiModelPart
                    | SignatureBlockKind::Unknown
            ) {
                decision.action = SignatureCompatibilityAction::ReplaceWithGeminiBypass;
                decision.replacement_signature = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string();
                decision.reason = "missing or incompatible signature".to_string();
                return decision;
            }
            decision.action = SignatureCompatibilityAction::DropBlock;
            decision.reason = "signature is not compatible with Gemini and this block is not a bypass-safe Gemini model part".to_string();
        }
        SignatureProvider::Claude => {
            decision.action = SignatureCompatibilityAction::DropBlock;
            decision.reason =
                "Claude has no cross-provider bypass sentinel for thinking blocks".to_string();
        }
        SignatureProvider::Gpt => {
            decision.action = SignatureCompatibilityAction::DropBlock;
            decision.reason = "GPT reasoning encrypted_content cannot be synthesized from another provider signature".to_string();
        }
        SignatureProvider::Swe => {
            decision.action = SignatureCompatibilityAction::DropBlock;
            decision.reason = "SWE requires sealed.v1 signature from its own backend".to_string();
        }
        SignatureProvider::Kimi => {
            // Kimi's Messages endpoint never reads the signature back, so only the signature is
            // dropped and the recoverable thinking text survives.
            decision.action = SignatureCompatibilityAction::DropSignature;
            decision.reason = "Kimi does not validate replayed thinking signatures, so the block survives without one".to_string();
        }
        SignatureProvider::Grok => {
            // xAI rejects foreign or mutated blobs with 400 "Could not decrypt".
            decision.action = SignatureCompatibilityAction::DropBlock;
            decision.reason =
                "xAI verifies encrypted_content on replay and rejects foreign or mutated blobs"
                    .to_string();
        }
        _ => {
            decision.action = SignatureCompatibilityAction::NoCompatibleReplacement;
            decision.reason = "unknown target provider".to_string();
        }
    }
    decision
}

/// Splits this repo's `provider#payload` cache envelope. `None` when there is no `#` or the
/// prefix is not an accepted provider prefix.
pub fn split_signature_provider_prefix(raw_signature: &str) -> Option<(SignatureProvider, String)> {
    let (prefix, rest) = raw_signature.trim().split_once('#')?;
    let provider = signature_provider_from_cache_prefix(prefix);
    if provider == SignatureProvider::Unknown {
        return None;
    }
    Some((provider, rest.trim().to_string()))
}

/// Maps the explicit provider-prefix envelope to a provider family. Stricter than
/// `signature_provider_from_model_name`, so model names such as `claude-cache#...` cannot be
/// mistaken for trusted provenance.
pub fn signature_provider_from_cache_prefix(prefix: &str) -> SignatureProvider {
    match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => SignatureProvider::Claude,
        "gemini" | "google" => SignatureProvider::Gemini,
        "openai" | "gpt" | "codex" => SignatureProvider::Gpt,
        "swe" | "sealed" => SignatureProvider::Swe,
        _ => SignatureProvider::Unknown,
    }
}

/// Strips the provider cache prefix when present; the result is what should be replayed upstream.
pub fn signature_payload_without_provider_prefix(raw_signature: &str) -> String {
    match split_signature_provider_prefix(raw_signature) {
        Some((_, unprefixed)) => unprefixed,
        None => raw_signature.trim().to_string(),
    }
}

/// A replayable provider-native signature for `target_provider` (prefix stripped, Claude
/// signatures normalized to the target's format).
pub fn compatible_signature_for_provider(
    target_provider: SignatureProvider,
    raw_signature: &str,
) -> Option<String> {
    compatible_signature_for_provider_block(target_provider, raw_signature, SignatureBlockKind::Unknown)
}

/// As [`compatible_signature_for_provider`] when the source block kind is known.
pub fn compatible_signature_for_provider_block(
    target_provider: SignatureProvider,
    raw_signature: &str,
    block_kind: SignatureBlockKind,
) -> Option<String> {
    let decision = decide_signature_compatibility(target_provider, raw_signature, block_kind);
    if !decision.compatible || decision.normalized_signature.is_empty() {
        return None;
    }
    Some(decision.normalized_signature)
}

/// The double-layer R-form required by Antigravity Claude replay. Only strictly Claude-identifiable
/// signatures qualify (Gemini E-prefixed envelopes and CAIS signatures are rejected).
pub fn compatible_antigravity_claude_thinking_signature(raw_signature: &str) -> Option<String> {
    if detect_signature_provider_for_block(raw_signature, SignatureBlockKind::ClaudeThinking)
        != SignatureProvider::Claude
    {
        return None;
    }
    normalize_claude_thinking_signature(
        &signature_payload_without_provider_prefix(raw_signature),
        ClaudeSignatureValidationOptions::STRICT,
    )
    .ok()
}

/// Explains why a matching signature is replayable. CAIS signatures carry the issuing model, so
/// both the embedded and the target model are reported.
fn claude_compatible_signature_reason(
    target_provider: SignatureProvider,
    raw_signature: &str,
    target_model: &str,
) -> String {
    const GENERIC_REASON: &str = "signature provider matches target provider";
    if target_provider != SignatureProvider::Claude {
        return GENERIC_REASON.to_string();
    }
    let Ok(info) =
        inspect_claude_cais_signature(&signature_payload_without_provider_prefix(raw_signature))
    else {
        return GENERIC_REASON.to_string();
    };
    let mut reason = if !info.model_text.is_empty() {
        format!(
            "valid Claude CAIS signature with embedded model {} is compatible with any Claude target",
            info.model_text
        )
    } else if info.envelope_version >= 4 {
        "valid Claude CAQS signature is compatible with any Claude target".to_string()
    } else {
        "valid Claude CAIS signature is compatible with any Claude target".to_string()
    };
    let trimmed_model = target_model.trim();
    if !trimmed_model.is_empty() {
        reason.push_str(", including target model ");
        reason.push_str(trimmed_model);
    }
    reason
}

pub(super) fn normalize_signature_target_provider(provider: SignatureProvider) -> SignatureProvider {
    match provider {
        SignatureProvider::GeminiBypass => SignatureProvider::Gemini,
        other => other,
    }
}

fn signature_provider_matches_target(target: SignatureProvider, detected: SignatureProvider) -> bool {
    match target {
        SignatureProvider::Gemini => {
            matches!(detected, SignatureProvider::Gemini | SignatureProvider::GeminiBypass)
        }
        SignatureProvider::Claude => detected == SignatureProvider::Claude,
        SignatureProvider::Gpt => detected == SignatureProvider::Gpt,
        SignatureProvider::Swe => detected == SignatureProvider::Swe,
        SignatureProvider::Kimi => detected == SignatureProvider::Kimi,
        // Grok is deliberately absent: detection never yields it, so a Grok target decides
        // replay safety from provenance plus `inspect_grok_encrypted_content`.
        _ => false,
    }
}

fn normalize_compatible_signature_for_provider(
    target_provider: SignatureProvider,
    raw_signature: &str,
    block_kind: SignatureBlockKind,
) -> String {
    let payload = signature_payload_without_provider_prefix(raw_signature);
    match normalize_signature_target_provider(target_provider) {
        SignatureProvider::Claude => {
            if is_valid_claude_cais_signature(&payload) {
                return payload;
            }
            return normalize_claude_provider_native_thinking_signature(
                &payload,
                ClaudeSignatureValidationOptions::default(),
            )
            .unwrap_or_default();
        }
        SignatureProvider::Gemini => {
            if is_gemini_thought_signature_bypass(&payload)
                || is_recognized_gemini_provider_signature(&payload, block_kind)
            {
                return payload;
            }
        }
        SignatureProvider::Gpt => {
            if is_valid_gpt_reasoning_signature(&payload) {
                return payload;
            }
        }
        SignatureProvider::Swe => {
            if payload.starts_with("sealed.v1.") {
                return payload;
            }
        }
        SignatureProvider::Kimi if is_valid_kimi_thinking_signature(&payload) => return payload,
        _ => {}
    }
    String::new()
}

pub(super) fn is_recognized_gemini_provider_signature(
    raw_signature: &str,
    _block_kind: SignatureBlockKind,
) -> bool {
    if is_valid_claude_cais_signature(raw_signature) {
        return false;
    }
    is_valid_gemini_thought_signature(
        raw_signature,
        GeminiThoughtSignatureValidationOptions::KNOWN_ENVELOPE,
    )
}

/// Whether `raw_signature` is a structurally valid reasoning signature or `encrypted_content`
/// payload from any known provider (GPT, Claude, Gemini, Kimi, Grok, Devin).
pub fn is_recognized_reasoning_signature(raw_signature: &str) -> bool {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return false;
    }
    if detect_signature_provider(sig) != SignatureProvider::Unknown {
        return true;
    }
    is_valid_grok_encrypted_content(sig)
}
