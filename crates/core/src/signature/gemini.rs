//! Gemini thought signature validation (Go: signature/gemini_validation.go).
//!
//! Gemini thought signatures are opaque provider state, so local validation checks only the
//! transport shape: bounded length, standard base64, and (when the caller needs provider
//! compatibility) the known protobuf envelope `field 2 -> field 1`. Bare base64 UUIDs are
//! classified separately and are never replay-safe. Two documented bypass sentinels exist for
//! synthetic history; this repo emits `skip_thought_signature_validator`.
//!
//! Tool pairing: a functionResponse must match the preceding functionCall id/name, and the valid
//! shape is all model functionCalls first, then their responses.

use cpa_json::{J, Res, Value};

use super::claude::is_valid_claude_cais_signature;
use super::protowire::{self, WireType};
use super::{b64, go_quote, Result, SignatureError};

pub const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

pub const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
pub const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";

/// How much local validation is applied to Gemini thought signatures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiThoughtSignatureValidationOptions {
    /// Accept Gemini's documented synthetic-history bypass sentinels.
    pub allow_bypass_sentinel: bool,
    /// Require the decoded payload to match the observed protobuf envelope (rejects base64 UUIDs).
    pub require_known_envelope: bool,
    /// Require the decoded payload to start with 0x12.
    pub require_observed_marker: bool,
}

impl GeminiThoughtSignatureValidationOptions {
    pub const KNOWN_ENVELOPE: Self = Self {
        allow_bypass_sentinel: false,
        require_known_envelope: true,
        require_observed_marker: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeminiThoughtSignatureEnvelope {
    /// Go's zero value: unset, as on bypass-sentinel results.
    #[default]
    None,
    Unknown,
    /// The only replay-safe Gemini envelope.
    ProtobufField2,
    AsciiUuid,
}

impl GeminiThoughtSignatureEnvelope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Unknown => "unknown",
            Self::ProtobufField2 => "protobuf_field_2",
            Self::AsciiUuid => "ascii_uuid",
        }
    }
}

/// Locally inspectable properties of an opaque Gemini thought signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeminiThoughtSignatureInfo {
    pub is_bypass_sentinel: bool,
    pub bypass_sentinel: String,
    pub decoded_len: usize,
    pub first_byte: u8,
    pub has_observed_marker: bool,
    pub known_envelope: bool,
    pub envelope: GeminiThoughtSignatureEnvelope,
    pub record_count: usize,
    pub opaque_payload_len: usize,
}

/// Whether `raw_signature` is one of Gemini's documented bypass sentinels.
pub fn is_gemini_thought_signature_bypass(raw_signature: &str) -> bool {
    matches!(
        raw_signature.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

/// Whether `raw_signature` has a valid local Gemini thought-signature shape under `opts`.
pub fn is_valid_gemini_thought_signature(
    raw_signature: &str,
    opts: GeminiThoughtSignatureValidationOptions,
) -> bool {
    inspect_gemini_thought_signature(raw_signature, opts).is_ok()
}

/// Validates and inspects the local transport shape of a Gemini thought signature.
pub fn inspect_gemini_thought_signature(
    raw_signature: &str,
    opt: GeminiThoughtSignatureValidationOptions,
) -> Result<GeminiThoughtSignatureInfo> {
    let sig = raw_signature.trim();
    if sig.is_empty() {
        return Err(SignatureError::new("empty Gemini thought signature"));
    }

    if is_valid_claude_cais_signature(sig) {
        return Err(SignatureError::new(
            "invalid Gemini thought signature: detected Claude CAIS signature",
        ));
    }

    if is_gemini_thought_signature_bypass(sig) {
        if !opt.allow_bypass_sentinel {
            return Err(SignatureError::new(
                "Gemini thought signature bypass sentinel is not allowed",
            ));
        }
        return Ok(GeminiThoughtSignatureInfo {
            is_bypass_sentinel: true,
            bypass_sentinel: sig.to_string(),
            ..Default::default()
        });
    }

    let decoded = decode_gemini_thought_signature(sig)?;
    if decoded.is_empty() {
        return Err(SignatureError::new(
            "invalid Gemini thought signature: empty decoded payload",
        ));
    }

    let mut info = GeminiThoughtSignatureInfo {
        decoded_len: decoded.len(),
        first_byte: decoded[0],
        has_observed_marker: decoded[0] == 0x12,
        ..Default::default()
    };
    (info.envelope, info.known_envelope) = classify_envelope(&decoded);
    (info.record_count, info.opaque_payload_len) = inspect_envelope(&decoded, info.envelope);
    if opt.require_known_envelope && !info.known_envelope {
        return Err(SignatureError::new(format!(
            "invalid Gemini thought signature: unknown envelope {}",
            go_quote(info.envelope.as_str())
        )));
    }
    if opt.require_observed_marker && !info.has_observed_marker {
        return Err(SignatureError::new(format!(
            "invalid Gemini thought signature: expected observed marker 0x12, got 0x{:02x}",
            info.first_byte
        )));
    }

    Ok(info)
}

/// Validates thoughtSignature fields in a Gemini native payload (JSON bytes). The first
/// functionCall in each model Content must carry a valid provider signature or an allowed
/// sentinel; later parallel calls may be unsigned but any signature they carry must be valid.
pub fn validate_gemini_thought_signatures(
    input_raw_json: &[u8],
    opts: GeminiThoughtSignatureValidationOptions,
) -> Result<()> {
    let root = cpa_json::parse(input_raw_json);
    let (contents, contents_path) = gemini_contents(&root);
    if !contents.is_array() {
        return Ok(());
    }

    for (i, content) in contents.array().iter().enumerate() {
        let parts = content.g("parts");
        if !parts.is_array() {
            continue;
        }

        let is_model_turn = content.g("role").str().trim().eq_ignore_ascii_case("model");
        let mut first_function_call_seen = false;
        for (j, part) in parts.array().iter().enumerate() {
            let has_function_call = part.g("functionCall").exists();
            let is_first_function_call = is_model_turn && has_function_call && !first_function_call_seen;
            if is_model_turn && has_function_call {
                first_function_call_seen = true;
            }
            let (raw_signature, has_signature) = gemini_part_thought_signature(part);
            if !has_function_call && !has_signature {
                continue;
            }

            let part_path = format!("{contents_path}[{i}].parts[{j}]");
            let raw_signature = raw_signature.trim();
            if part.g("functionResponse").exists() && has_signature {
                return Err(SignatureError::new(format!(
                    "{part_path}: functionResponse must not carry thoughtSignature"
                )));
            }
            if raw_signature.is_empty() {
                if is_first_function_call {
                    return Err(SignatureError::new(format!(
                        "{part_path}: missing thoughtSignature on first functionCall"
                    )));
                }
                if has_signature {
                    return Err(SignatureError::new(format!(
                        "{part_path}: empty thoughtSignature"
                    )));
                }
                continue;
            }
            if is_gemini_thought_signature_bypass(raw_signature) && !is_first_function_call {
                return Err(SignatureError::new(format!(
                    "{part_path}: Gemini bypass sentinel is allowed only on the first model functionCall"
                )));
            }
            if !has_normalized_gemini_part_thought_signature(part, raw_signature) {
                return Err(SignatureError::new(format!(
                    "{part_path}: thoughtSignature must use one canonical top-level field"
                )));
            }
            if let Err(err) = inspect_gemini_thought_signature(raw_signature, opts) {
                return Err(SignatureError::new(format!("{part_path}: {err}")));
            }
        }
    }

    Ok(())
}

struct FunctionCallRef {
    id: String,
    name: String,
    path: String,
}

/// Validates the replay shape around Gemini functionCall and functionResponse parts (JSON
/// bytes): id/name pairing, and no response parts interleaved with calls in one content. A
/// final pending functionCall group is allowed (a freshly returned model step has no outputs).
pub fn validate_gemini_function_call_pairing(input_raw_json: &[u8]) -> Result<()> {
    let root = cpa_json::parse(input_raw_json);
    let (contents, contents_path) = gemini_contents(&root);
    if !contents.is_array() {
        return Ok(());
    }

    let mut pending: Vec<FunctionCallRef> = Vec::new();
    for (i, content) in contents.array().iter().enumerate() {
        let parts = content.g("parts");
        let part_list = parts.array();
        if !parts.is_array() || part_list.is_empty() {
            if !pending.is_empty() {
                return Err(SignatureError::new(format!(
                    "{contents_path}[{i}]: content appears before {} pending functionResponse part(s)",
                    pending.len()
                )));
            }
            continue;
        }

        let mut calls: Vec<FunctionCallRef> = Vec::new();
        let mut responses: Vec<(&Res<'_>, String)> = Vec::new();
        for (j, part) in part_list.iter().enumerate() {
            let part_path = format!("{contents_path}[{i}].parts[{j}]");
            let call = part.g("functionCall");
            if call.exists() {
                if call.g("name").str().is_empty() {
                    return Err(SignatureError::new(format!(
                        "{part_path}: missing functionCall.name"
                    )));
                }
                calls.push(FunctionCallRef {
                    id: call.g("id").str(),
                    name: call.g("name").str(),
                    path: part_path.clone(),
                });
            }
            if part.g("functionResponse").exists() {
                responses.push((part, part_path));
            }
        }

        if !calls.is_empty() && !responses.is_empty() {
            return Err(SignatureError::new(format!(
                "{contents_path}[{i}]: functionCall and functionResponse parts must not be interleaved in the same content"
            )));
        }
        if !calls.is_empty() && !pending.is_empty() {
            return Err(SignatureError::new(format!(
                "{contents_path}[{i}]: functionCall appears before {} pending functionResponse part(s)",
                pending.len()
            )));
        }
        if !calls.is_empty() {
            pending = calls;
            continue;
        }
        if responses.is_empty() {
            if !pending.is_empty() {
                // Intervening user content (reminders, notices, user turns) may precede the
                // pending functionResponse turn; only a model turn breaks turn ownership.
                let role = content.g("role").str().trim().to_lowercase();
                if role == "model" {
                    return Err(SignatureError::new(format!(
                        "{contents_path}[{i}]: model content appears before {} pending functionResponse part(s)",
                        pending.len()
                    )));
                }
            }
            continue;
        }
        if pending.is_empty() {
            return Err(SignatureError::new(format!(
                "{contents_path}[{i}]: functionResponse without preceding functionCall"
            )));
        }
        if responses.len() != pending.len() {
            return Err(SignatureError::new(format!(
                "{contents_path}[{i}]: functionResponse count {} does not match pending functionCall count {}",
                responses.len(),
                pending.len()
            )));
        }

        for ((part, part_path), call) in responses.iter().zip(&pending) {
            let response = part.g("functionResponse");
            let response_id = response.g("id").str();
            let response_name = response.g("name").str();

            if !call.id.is_empty() && response_id.is_empty() {
                return Err(SignatureError::new(format!(
                    "{part_path}: missing functionResponse.id for {}",
                    call.path
                )));
            }
            if !call.id.is_empty() && response_id != call.id {
                return Err(SignatureError::new(format!(
                    "{part_path}: functionResponse.id {} does not match functionCall.id {} at {}",
                    go_quote(&response_id),
                    go_quote(&call.id),
                    call.path
                )));
            }
            if response_name.is_empty() {
                return Err(SignatureError::new(format!(
                    "{part_path}: missing functionResponse.name"
                )));
            }
            if !call.name.is_empty() && response_name != call.name {
                return Err(SignatureError::new(format!(
                    "{part_path}: functionResponse.name {} does not match functionCall.name {} at {}",
                    go_quote(&response_name),
                    go_quote(&call.name),
                    call.path
                )));
            }
        }

        pending.clear();
    }
    Ok(())
}

fn decode_gemini_thought_signature(sig: &str) -> Result<Vec<u8>> {
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return Err(SignatureError::new(format!(
            "Gemini thought signature exceeds maximum length ({MAX_GEMINI_THOUGHT_SIGNATURE_LEN} bytes)"
        )));
    }
    match b64::std(sig) {
        Ok(decoded) => Ok(decoded),
        Err(err) => match b64::raw_std(sig) {
            Ok(decoded) => Ok(decoded),
            Err(_) => Err(SignatureError::new(format!(
                "invalid Gemini thought signature: base64 decode failed: {err}"
            ))),
        },
    }
}

fn classify_envelope(decoded: &[u8]) -> (GeminiThoughtSignatureEnvelope, bool) {
    if decoded.is_empty() {
        return (GeminiThoughtSignatureEnvelope::Unknown, false);
    }
    if is_ascii_uuid_bytes(decoded) {
        return (GeminiThoughtSignatureEnvelope::AsciiUuid, false);
    }
    if inspect_field2_envelope(decoded).is_some_and(|(records, opaque)| records == 1 && opaque > 0) {
        return (GeminiThoughtSignatureEnvelope::ProtobufField2, true);
    }
    (GeminiThoughtSignatureEnvelope::Unknown, false)
}

fn inspect_envelope(decoded: &[u8], envelope: GeminiThoughtSignatureEnvelope) -> (usize, usize) {
    if envelope == GeminiThoughtSignatureEnvelope::ProtobufField2
        && let Some(info) = inspect_field2_envelope(decoded)
    {
        return info;
    }
    (0, 0)
}

/// `(record_count, opaque_payload_len)` of a `field 2 -> field 1` envelope whose body looks like
/// Tink ciphertext, a UUID, or a tool-invocation message.
fn inspect_field2_envelope(decoded: &[u8]) -> Option<(usize, usize)> {
    let value = consume_field2_field1_value(decoded)?;
    if !is_likely_opaque_payload(value)
        && !is_ascii_uuid_bytes(value)
        && !is_likely_tool_invocation_payload(value)
    {
        return None;
    }
    Some((1, value.len()))
}

fn consume_field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let (num, typ, n) = protowire::consume_tag(decoded).ok()?;
    if num != 2 || typ != WireType::Bytes {
        return None;
    }
    let mut offset = n;
    let (container, n) = protowire::consume_bytes(&decoded[offset..]).ok()?;
    offset += n;
    if offset != decoded.len() {
        return None;
    }

    let (num, typ, n) = protowire::consume_tag(container).ok()?;
    if num != 1 || typ != WireType::Bytes {
        return None;
    }
    let mut container_offset = n;
    let (value, n) = protowire::consume_bytes(&container[container_offset..]).ok()?;
    container_offset += n;
    if container_offset != container.len() {
        return None;
    }
    Some(value)
}

/// The envelope body is a Google Tink output: one prefix-type byte (0x01 selects the TINK
/// prefix), a four-byte big-endian key id, then ciphertext. Only the prefix-type byte is a format
/// constant (the key id rotates), so it is the only anchor (a 1/256 false-positive rate).
fn is_likely_opaque_payload(value: &[u8]) -> bool {
    value.first() == Some(&0x01)
}

/// Server-side tool blocks (toolCall/toolResponse) wrap the Tink ciphertext in a protobuf message
/// with at least one length-delimited field that is a valid Tink payload.
fn is_likely_tool_invocation_payload(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut offset = 0;
    let mut has_tink_field = false;
    while offset < value.len() {
        let Ok((_, typ, n)) = protowire::consume_tag(&value[offset..]) else {
            return false;
        };
        offset += n;
        let consumed = match typ {
            WireType::Varint => protowire::consume_varint(&value[offset..]).map(|(_, n)| n),
            WireType::Bytes => protowire::consume_bytes(&value[offset..]).map(|(bytes, n)| {
                if is_likely_opaque_payload(bytes) {
                    has_tink_field = true;
                }
                n
            }),
            WireType::Fixed32 => protowire::consume_fixed32(&value[offset..]).map(|(_, n)| n),
            WireType::Fixed64 => protowire::consume_fixed64(&value[offset..]).map(|(_, n)| n),
            _ => return false,
        };
        match consumed {
            Ok(n) => offset += n,
            Err(_) => return false,
        }
    }
    has_tink_field && offset == value.len()
}

fn is_ascii_uuid_bytes(decoded: &[u8]) -> bool {
    super::claude::is_canonical_uuid(decoded)
}

/// `contents` when present, else `request.contents` (the returned path names the lookup used).
pub(crate) fn gemini_contents(root: &Value) -> (Res<'_>, &'static str) {
    let contents = root.g("contents");
    if contents.exists() {
        return (contents, "contents");
    }
    (root.g("request.contents"), "request.contents")
}

/// Paths a thought signature can live at inside a Gemini part (first existing wins).
pub(crate) const GEMINI_PART_THOUGHT_SIGNATURE_PATHS: [&str; 7] = [
    "thoughtSignature",
    "thought_signature",
    "functionCall.thoughtSignature",
    "functionCall.thought_signature",
    "functionResponse.thoughtSignature",
    "functionResponse.thought_signature",
    "extra_content.google.thought_signature",
];

pub(crate) fn gemini_part_thought_signature(part: &Res<'_>) -> (String, bool) {
    for path in GEMINI_PART_THOUGHT_SIGNATURE_PATHS {
        let result = part.g(path);
        if result.exists() {
            return (result.str(), true);
        }
    }
    (String::new(), false)
}

/// True when the part carries exactly one top-level `thoughtSignature` string equal to
/// `replay_signature` and no alias field.
pub(crate) fn has_normalized_gemini_part_thought_signature(
    part: &Res<'_>,
    replay_signature: &str,
) -> bool {
    let canonical_count = part
        .entries()
        .iter()
        .filter(|(key, _)| *key == "thoughtSignature")
        .count();
    let canonical = part.g("thoughtSignature");
    if canonical_count != 1 || !canonical.is_string() || canonical.str() != replay_signature {
        return false;
    }
    !GEMINI_PART_THOUGHT_SIGNATURE_PATHS[1..]
        .iter()
        .any(|path| part.g(path).exists())
}
