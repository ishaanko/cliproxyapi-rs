//! Claude thinking signature validation (Go: signature/claude_validation.go).
//!
//! Encoding detection: the first character of a Claude signature fixes the base64 depth.
//! `E` is single-layer (decoded payload starts with 0x12), `R` is double-layer (base64 of an
//! `E...` string), `C` is the CAIS/CAQS envelope used by the newest models (decoded payload
//! starts with 0x08). Strict mode walks the protobuf tree:
//!
//! ```text
//! top-level
//! |- field 2 (bytes): container
//! |  `- field 1 (bytes): channel block
//! |     |- 1 (varint) channel_id [required]   |- 2 (varint) infra [optional]
//! |     |- 6 (bytes) model_text [optional]    `- 7 (varint) unknown [optional]
//! `- other fields skipped
//! ```
//!
//! CAIS validation is structural: only the 0x08 marker, the nested container/channel block,
//! signature bytes and (for envelope versions below 4) the `claude-` model text are required.
//! Envelope version 4 (CAQS) moves the signature bytes to container field 5 and requires a
//! block kind of `thinking` or `narration`.

use super::protowire::{self, WireType};
use super::{b64, go_quote, Result, SignatureError};

pub const MAX_CLAUDE_THINKING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

/// Controls how far Claude thinking signatures are inspected. The base validation always checks
/// the cache prefix, base64 layers and the decoded 0x12 marker; `strict` also walks the protobuf
/// tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeSignatureValidationOptions {
    /// Only check an optional cache prefix followed by an `E`/`R` first character.
    pub prefix_only: bool,
    /// Check the prefix, first character and base64 layers without the decoded marker or tree.
    pub base64_only: bool,
    /// Preserve empty thinking placeholders (no signature, no text) during strip operations.
    pub allow_empty_signature_with_empty_text: bool,
    pub strict: bool,
}

impl ClaudeSignatureValidationOptions {
    pub const STRICT: Self = Self {
        prefix_only: false,
        base64_only: false,
        allow_empty_signature_with_empty_text: false,
        strict: true,
    };
}

/// Protobuf fields used for Claude signature routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSignatureTree {
    pub encoding_layers: u32,
    pub channel_id: u64,
    pub field2: Option<u64>,
    pub routing_class: &'static str,
    pub infrastructure_class: &'static str,
    pub schema_features: &'static str,
    pub model_text: String,
    pub legacy_route_hint: &'static str,
    pub has_field7: bool,
}

/// Locally inspected structure of a Claude CAIS/CAQS signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCaisSignatureInfo {
    pub first_byte: u8,
    pub envelope_version: u64,
    pub channel_id: u64,
    pub model_text: String,
    pub block_kind: String,
    pub context_id: String,
    pub signature_len: usize,
}

/// Whether `raw_signature` is a valid Claude thinking signature under `opts`.
pub fn is_valid_claude_thinking_signature(
    raw_signature: &str,
    opts: ClaudeSignatureValidationOptions,
) -> bool {
    if opts.prefix_only {
        return has_claude_thinking_signature_prefix(raw_signature);
    }
    if opts.base64_only {
        return has_decodable_claude_thinking_signature(raw_signature);
    }
    normalize_claude_thinking_signature(raw_signature, opts).is_ok()
}

/// Claude E/R shape whose expected base64 layer(s) decode.
pub fn has_decodable_claude_thinking_signature(raw_signature: &str) -> bool {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return false;
    }
    match sig.as_bytes()[0] {
        b'E' => b64::std(sig).is_ok_and(|decoded| !decoded.is_empty()),
        b'R' => match b64::std(sig) {
            Ok(decoded) if !decoded.is_empty() && decoded[0] == b'E' => {
                b64::std(&decoded).is_ok_and(|inner| !inner.is_empty())
            }
            _ => false,
        },
        _ => false,
    }
}

/// Claude `E`/`R` first character after stripping an optional cache prefix.
pub fn has_claude_thinking_signature_prefix(raw_signature: &str) -> bool {
    matches!(
        strip_claude_signature_prefix(raw_signature).as_bytes().first(),
        Some(b'E' | b'R')
    )
}

/// Trims and strips everything up to the first `#` (historical `modelGroup#` cache prefixes).
pub(crate) fn strip_claude_signature_prefix(raw_signature: &str) -> &str {
    let sig = raw_signature.trim();
    match sig.find('#') {
        Some(idx) => sig[idx + 1..].trim(),
        None => sig,
    }
}

/// Validates every thinking block signature in a Claude messages payload (JSON bytes).
pub fn validate_claude_thinking_signatures(
    input_raw_json: &[u8],
    opts: ClaudeSignatureValidationOptions,
) -> Result<()> {
    use cpa_json::J;
    let root = cpa_json::parse(input_raw_json);
    let messages = root.g("messages");
    if !messages.is_array() {
        return Ok(());
    }
    for (i, message) in messages.array().iter().enumerate() {
        let content = message.g("content");
        if !content.is_array() {
            continue;
        }
        for (j, part) in content.array().iter().enumerate() {
            if part.g("type").str() != "thinking" {
                continue;
            }
            let raw_signature = part.g("signature").str();
            let raw_signature = raw_signature.trim();
            if raw_signature.is_empty() {
                return Err(SignatureError::new(format!(
                    "messages[{i}].content[{j}]: missing thinking signature"
                )));
            }
            if let Err(err) = normalize_claude_thinking_signature(raw_signature, opts) {
                return Err(SignatureError::new(format!(
                    "messages[{i}].content[{j}]: {err}"
                )));
            }
        }
    }
    Ok(())
}

fn check_prefix_and_len(raw_signature: &str) -> Result<&str> {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() {
        return Err(SignatureError::new("empty signature"));
    }
    if sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return Err(SignatureError::new(format!(
            "signature exceeds maximum length ({MAX_CLAUDE_THINKING_SIGNATURE_LEN} bytes)"
        )));
    }
    Ok(sig)
}

fn bad_prefix_error(sig: &str) -> SignatureError {
    // Go formats `string(sig[0])`: a lone byte converted as a rune (Latin-1 for bytes >= 0x80).
    let first = char::from(sig.as_bytes()[0]);
    SignatureError::new(format!(
        "invalid signature: expected 'E' or 'R' prefix, got {}",
        go_quote(&first.to_string())
    ))
}

/// Strips any cache prefix, validates, and returns the double-layer R-form expected by
/// Antigravity bypass mode (E-form input is base64-wrapped).
pub fn normalize_claude_thinking_signature(
    raw_signature: &str,
    opts: ClaudeSignatureValidationOptions,
) -> Result<String> {
    let sig = check_prefix_and_len(raw_signature)?;
    match sig.as_bytes()[0] {
        b'R' => {
            validate_double_layer(sig, opts)?;
            Ok(sig.to_string())
        }
        b'E' => {
            validate_single_layer(sig, opts)?;
            Ok(b64::encode_std(sig.as_bytes()))
        }
        _ => Err(bad_prefix_error(sig)),
    }
}

/// Strips any cache prefix, validates, and returns the single-layer E-form expected by
/// Claude-native providers (R-form input is decoded).
pub fn normalize_claude_provider_native_thinking_signature(
    raw_signature: &str,
    opts: ClaudeSignatureValidationOptions,
) -> Result<String> {
    let sig = check_prefix_and_len(raw_signature)?;
    match sig.as_bytes()[0] {
        b'E' => {
            validate_single_layer(sig, opts)?;
            Ok(sig.to_string())
        }
        b'R' => {
            validate_double_layer(sig, opts)?;
            let decoded = b64::std(sig).map_err(|err| {
                SignatureError::new(format!(
                    "invalid double-layer signature: base64 decode failed: {err}"
                ))
            })?;
            // Go `string(decoded)`: validation above guarantees an ASCII base64 string.
            Ok(String::from_utf8_lossy(&decoded).into_owned())
        }
        _ => Err(bad_prefix_error(sig)),
    }
}

/// Decodes the outer layer of an R-form signature and checks that the inner string starts with
/// `E`; returns the inner base64 bytes.
fn decode_double_layer(sig: &str) -> Result<Vec<u8>> {
    let decoded = b64::std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid double-layer signature: base64 decode failed: {err}"
        ))
    })?;
    if decoded.is_empty() {
        return Err(SignatureError::new(
            "invalid double-layer signature: empty after decode",
        ));
    }
    if decoded[0] != b'E' {
        return Err(SignatureError::new(format!(
            "invalid double-layer signature: inner does not start with 'E', got 0x{:02x}",
            decoded[0]
        )));
    }
    Ok(decoded)
}

fn validate_double_layer(sig: &str, opt: ClaudeSignatureValidationOptions) -> Result<()> {
    let decoded = decode_double_layer(sig)?;
    validate_single_layer_content(&decoded, 2, opt)
}

fn validate_single_layer(sig: &str, opt: ClaudeSignatureValidationOptions) -> Result<()> {
    validate_single_layer_content(sig.as_bytes(), 1, opt)
}

fn validate_single_layer_content(
    sig: &[u8],
    encoding_layers: u32,
    opt: ClaudeSignatureValidationOptions,
) -> Result<()> {
    let decoded = b64::std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid single-layer signature: base64 decode failed: {err}"
        ))
    })?;
    if decoded.is_empty() {
        return Err(SignatureError::new(
            "invalid single-layer signature: empty after decode",
        ));
    }
    if decoded[0] != 0x12 {
        return Err(SignatureError::new(format!(
            "invalid Claude signature: expected first byte 0x12, got 0x{:02x}",
            decoded[0]
        )));
    }
    if !opt.strict {
        return Ok(());
    }
    inspect_claude_signature_payload(&decoded, encoding_layers).map(|_| ())
}

/// Decodes and inspects a double-layer Claude thinking signature.
pub fn inspect_claude_double_layer_signature(sig: &str) -> Result<ClaudeSignatureTree> {
    let decoded = decode_double_layer(sig)?;
    inspect_single_layer_with_layers(&decoded, 2)
}

/// Decodes and inspects a single-layer Claude thinking signature.
pub fn inspect_claude_single_layer_signature(sig: &str) -> Result<ClaudeSignatureTree> {
    inspect_single_layer_with_layers(sig.as_bytes(), 1)
}

fn inspect_single_layer_with_layers(sig: &[u8], encoding_layers: u32) -> Result<ClaudeSignatureTree> {
    let decoded = b64::std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid single-layer signature: base64 decode failed: {err}"
        ))
    })?;
    if decoded.is_empty() {
        return Err(SignatureError::new(
            "invalid single-layer signature: empty after decode",
        ));
    }
    inspect_claude_signature_payload(&decoded, encoding_layers)
}

/// Inspects the decoded Claude thinking signature protobuf payload.
pub fn inspect_claude_signature_payload(
    payload: &[u8],
    encoding_layers: u32,
) -> Result<ClaudeSignatureTree> {
    if payload.is_empty() {
        return Err(SignatureError::new("invalid Claude signature: empty payload"));
    }
    if payload[0] != 0x12 {
        return Err(SignatureError::new(format!(
            "invalid Claude signature: expected first byte 0x12, got 0x{:02x}",
            payload[0]
        )));
    }
    let container = extract_claude_bytes_field(payload, 2, "top-level protobuf")?;
    let channel_block = extract_claude_bytes_field(container, 1, "Claude Field 2 container")?;
    inspect_claude_channel_block(channel_block, encoding_layers)
}

fn inspect_claude_channel_block(
    channel_block: &[u8],
    encoding_layers: u32,
) -> Result<ClaudeSignatureTree> {
    let mut tree = ClaudeSignatureTree {
        encoding_layers,
        channel_id: 0,
        field2: None,
        routing_class: "unknown",
        infrastructure_class: "infra_unknown",
        schema_features: "unknown_schema_features",
        model_text: String::new(),
        legacy_route_hint: "",
        has_field7: false,
    };
    let mut have_channel_id = false;
    let mut has_field6 = false;
    let mut has_field7 = false;

    walk_claude_protobuf_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                if typ != WireType::Varint {
                    return Err(SignatureError::new(
                        "invalid Claude signature: Field 2.1.1 channel_id must be varint",
                    ));
                }
                tree.channel_id = decode_claude_varint_field(raw, "Field 2.1.1 channel_id")?;
                have_channel_id = true;
            }
            2 => {
                if typ != WireType::Varint {
                    return Err(SignatureError::new(
                        "invalid Claude signature: Field 2.1.2 field2 must be varint",
                    ));
                }
                tree.field2 = Some(decode_claude_varint_field(raw, "Field 2.1.2 field2")?);
            }
            6 => {
                if typ != WireType::Bytes {
                    return Err(SignatureError::new(
                        "invalid Claude signature: Field 2.1.6 model_text must be bytes",
                    ));
                }
                let model_bytes = decode_claude_bytes_field(raw, "Field 2.1.6 model_text")?;
                let Ok(text) = std::str::from_utf8(model_bytes) else {
                    return Err(SignatureError::new(
                        "invalid Claude signature: Field 2.1.6 model_text is not valid UTF-8",
                    ));
                };
                tree.model_text = text.to_string();
                has_field6 = true;
            }
            7 => {
                if typ != WireType::Varint {
                    return Err(SignatureError::new(
                        "invalid Claude signature: Field 2.1.7 must be varint",
                    ));
                }
                decode_claude_varint_field(raw, "Field 2.1.7")?;
                has_field7 = true;
                tree.has_field7 = true;
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_channel_id {
        return Err(SignatureError::new(
            "invalid Claude signature: missing Field 2.1.1 channel_id",
        ));
    }

    match tree.channel_id {
        11 => tree.routing_class = "routing_class_11",
        12 => tree.routing_class = "routing_class_12",
        _ => {}
    }

    tree.infrastructure_class = match tree.field2 {
        None => "infra_default",
        Some(1) => "infra_aws",
        Some(2) => "infra_google",
        Some(_) => "infra_unknown",
    };

    if has_field6 {
        tree.schema_features = "extended_model_tagged_schema";
    } else if !has_field7 && (70..=72).contains(&channel_block.len()) {
        tree.schema_features = "compact_schema";
    }

    if tree.channel_id == 11 {
        tree.legacy_route_hint = match (tree.field2, tree.encoding_layers) {
            (None, _) => "legacy_default_group",
            (Some(1), _) => "legacy_aws_group",
            (Some(2), 2) => "legacy_vertex_direct",
            (Some(2), 1) => "legacy_vertex_proxy",
            _ => "",
        };
    }

    Ok(tree)
}

/// Last occurrence of a bytes field; missing and wrong-wire-type fields are errors.
fn extract_claude_bytes_field<'a>(msg: &'a [u8], field_num: u32, scope: &str) -> Result<&'a [u8]> {
    let mut value: Option<&'a [u8]> = None;
    walk_claude_protobuf_fields(msg, |num, typ, raw| {
        if num != field_num {
            return Ok(());
        }
        if typ != WireType::Bytes {
            return Err(SignatureError::new(format!(
                "invalid Claude signature: {scope} field {field_num} must be bytes"
            )));
        }
        value = Some(decode_claude_bytes_field(
            raw,
            &format!("{scope} field {field_num}"),
        )?);
        Ok(())
    })?;
    value.ok_or_else(|| {
        SignatureError::new(format!(
            "invalid Claude signature: missing {scope} field {field_num}"
        ))
    })
}

/// Visits every top-level field of `msg` with its raw value bytes (including any length prefix).
fn walk_claude_protobuf_fields<'a>(
    msg: &'a [u8],
    mut visit: impl FnMut(u32, WireType, &'a [u8]) -> Result<()>,
) -> Result<()> {
    let mut offset = 0;
    while offset < msg.len() {
        let (num, typ, n) = protowire::consume_tag(&msg[offset..]).map_err(|err| {
            SignatureError::new(format!(
                "invalid Claude signature: malformed protobuf tag: {err}"
            ))
        })?;
        offset += n;
        let value_len = protowire::consume_field_value(num, typ, &msg[offset..]).map_err(|err| {
            SignatureError::new(format!(
                "invalid Claude signature: malformed protobuf field {num}: {err}"
            ))
        })?;
        visit(num, typ, &msg[offset..offset + value_len])?;
        offset += value_len;
    }
    Ok(())
}

fn decode_claude_varint_field(raw: &[u8], label: &str) -> Result<u64> {
    protowire::consume_varint(raw).map(|(v, _)| v).map_err(|err| {
        SignatureError::new(format!(
            "invalid Claude signature: failed to decode {label}: {err}"
        ))
    })
}

fn decode_claude_bytes_field<'a>(raw: &'a [u8], label: &str) -> Result<&'a [u8]> {
    protowire::consume_bytes(raw).map(|(v, _)| v).map_err(|err| {
        SignatureError::new(format!(
            "invalid Claude signature: failed to decode {label}: {err}"
        ))
    })
}

/// Decoded first byte identifying the CAIS envelope (tag for top-level field 1, varint).
const CLAUDE_CAIS_SIGNATURE_MARKER: u8 = 0x08;

/// Model text prefix that distinguishes a CAIS channel block from an arbitrary payload.
const CLAUDE_CAIS_MODEL_TEXT_PREFIX: &str = "claude-";

/// Whether `raw_signature` is a valid Claude CAIS thinking signature.
pub fn is_valid_claude_cais_signature(raw_signature: &str) -> bool {
    inspect_claude_cais_signature(raw_signature).is_ok()
}

/// Decodes and validates a Claude CAIS/CAQS thinking signature (see module docs).
pub fn inspect_claude_cais_signature(raw_signature: &str) -> Result<ClaudeCaisSignatureInfo> {
    let sig = strip_claude_signature_prefix(raw_signature);
    if sig.is_empty() {
        return Err(SignatureError::new("empty signature"));
    }
    if sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return Err(SignatureError::new(format!(
            "signature exceeds maximum length ({MAX_CLAUDE_THINKING_SIGNATURE_LEN} bytes)"
        )));
    }
    // A payload starting with 0x08 always base64-encodes to a string starting with 'C', so this
    // rejects classic E/R and Gemini envelopes without decoding.
    if sig.as_bytes()[0] != b'C' {
        let first = char::from(sig.as_bytes()[0]);
        return Err(SignatureError::new(format!(
            "invalid Claude CAIS signature: expected 'C' prefix, got {}",
            go_quote(&first.to_string())
        )));
    }

    let decoded = b64::std(sig).map_err(|err| {
        SignatureError::new(format!(
            "invalid Claude CAIS signature: base64 decode failed: {err}"
        ))
    })?;
    if decoded.is_empty() {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: empty after decode",
        ));
    }
    if decoded[0] != CLAUDE_CAIS_SIGNATURE_MARKER {
        return Err(SignatureError::new(format!(
            "invalid Claude CAIS signature: expected first byte 0x{:02x}, got 0x{:02x}",
            CLAUDE_CAIS_SIGNATURE_MARKER, decoded[0]
        )));
    }

    let mut info = ClaudeCaisSignatureInfo {
        first_byte: decoded[0],
        ..Default::default()
    };

    let mut container: Option<&[u8]> = None;
    let mut container_signature_bytes: Option<&[u8]> = None;
    walk_claude_protobuf_fields(&decoded, |num, typ, raw| {
        match num {
            1 => info.envelope_version = cais_varint(raw, typ, "CAIS top-level field 1 envelope version")?,
            2 => container = Some(cais_bytes(raw, typ, "CAIS top-level field 2 container")?),
            3 => {
                cais_varint(raw, typ, "CAIS top-level field 3 trailer")?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let Some(container) = container else {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: missing top-level field 2 container",
        ));
    };

    let mut channel_block: Option<&[u8]> = None;
    walk_claude_protobuf_fields(container, |num, typ, raw| {
        match num {
            1 => channel_block = Some(cais_bytes(raw, typ, "CAIS container field 1 channel block")?),
            5 => {
                container_signature_bytes =
                    Some(cais_bytes(raw, typ, "CAIS container field 5 signature bytes")?)
            }
            _ => {}
        }
        Ok(())
    })?;
    let Some(channel_block) = channel_block else {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: missing container field 1 channel block",
        ));
    };

    let (mut have_channel_id, mut have_signature_bytes, mut have_model_text) = (false, false, false);
    walk_claude_protobuf_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                info.channel_id = cais_varint(raw, typ, "CAIS channel field 1 channel_id")?;
                have_channel_id = true;
            }
            3 => {
                cais_varint(raw, typ, "CAIS channel field 3 version")?;
            }
            5 => {
                let value = cais_bytes(raw, typ, "CAIS channel field 5 signature bytes")?;
                if value.is_empty() {
                    return Err(SignatureError::new(
                        "invalid Claude CAIS signature: channel field 5 signature bytes must not be empty",
                    ));
                }
                info.signature_len = value.len();
                have_signature_bytes = true;
            }
            6 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 6 model_text")?;
                if !value.starts_with(CLAUDE_CAIS_MODEL_TEXT_PREFIX) {
                    return Err(SignatureError::new(format!(
                        "invalid Claude CAIS signature: channel field 6 model_text must start with {}, got {}",
                        go_quote(CLAUDE_CAIS_MODEL_TEXT_PREFIX),
                        go_quote(&value)
                    )));
                }
                info.model_text = value;
                have_model_text = true;
            }
            7 => {
                cais_varint(raw, typ, "CAIS channel field 7")?;
            }
            8 => info.block_kind = cais_utf8(raw, typ, "CAIS channel field 8 block kind")?,
            11 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 11 context id")?;
                if !is_canonical_uuid(value.as_bytes()) {
                    return Err(SignatureError::new(format!(
                        "invalid Claude CAIS signature: channel field 11 context id must be a canonical UUID, got {}",
                        go_quote(&value)
                    )));
                }
                info.context_id = value;
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_signature_bytes
        && info.envelope_version >= 4
        && container_signature_bytes.is_some_and(|b| !b.is_empty())
    {
        info.signature_len = container_signature_bytes.map_or(0, <[u8]>::len);
        have_signature_bytes = true;
    }
    if !have_channel_id {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: missing channel field 1 channel_id",
        ));
    }
    if !have_signature_bytes {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: missing signature bytes",
        ));
    }
    if !have_model_text && info.envelope_version < 4 {
        return Err(SignatureError::new(
            "invalid Claude CAIS signature: missing channel field 6 model_text",
        ));
    }
    if info.envelope_version >= 4 && info.block_kind != "thinking" && info.block_kind != "narration"
    {
        return Err(SignatureError::new(format!(
            "invalid Claude CAQS signature: expected block kind \"thinking\" or \"narration\", got {}",
            go_quote(&info.block_kind)
        )));
    }

    Ok(info)
}

fn cais_varint(raw: &[u8], typ: WireType, label: &str) -> Result<u64> {
    if typ != WireType::Varint {
        return Err(SignatureError::new(format!(
            "invalid Claude CAIS signature: {label} must be varint"
        )));
    }
    decode_claude_varint_field(raw, label)
}

fn cais_bytes<'a>(raw: &'a [u8], typ: WireType, label: &str) -> Result<&'a [u8]> {
    if typ != WireType::Bytes {
        return Err(SignatureError::new(format!(
            "invalid Claude CAIS signature: {label} must be bytes"
        )));
    }
    decode_claude_bytes_field(raw, label)
}

fn cais_utf8(raw: &[u8], typ: WireType, label: &str) -> Result<String> {
    let value = cais_bytes(raw, typ, label)?;
    std::str::from_utf8(value).map(str::to_string).map_err(|_| {
        SignatureError::new(format!(
            "invalid Claude CAIS signature: {label} must be valid UTF-8"
        ))
    })
}

/// 36-byte `8-4-4-4-12` hex UUID (Go: isCanonicalUUID).
pub(crate) fn is_canonical_uuid(s: &[u8]) -> bool {
    s.len() == 36
        && s.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}
