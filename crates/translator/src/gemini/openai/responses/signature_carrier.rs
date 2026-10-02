//! Private carrier encoding for Gemini thought signatures that travel inside OpenAI Responses
//! reasoning items (Go: signature_carrier.go).
//!
//! A carrier is `cpa-gemini-responses-carrier-v1:<direction>:<target>:<base64rawstd(signature)>`.
//! Direction says which neighbour the signature binds to, target which kind of neighbour.

use cpa_core::signature::{
    b64, compatible_signature_for_provider_block, is_gemini_thought_signature_bypass,
    signature_payload_without_provider_prefix, SignatureBlockKind, SignatureProvider,
    MAX_GEMINI_THOUGHT_SIGNATURE_LEN,
};
use cpa_json::{Res, Value};

use super::request::openai_responses_assistant_visible_text;

pub(super) const CARRIER_PREFIX: &str = "cpa-gemini-responses-carrier-v1:";
pub(super) const CARRIER_NEXT: &str = "next";
pub(super) const CARRIER_PREVIOUS: &str = "previous";
pub(super) const CARRIER_STANDALONE: &str = "standalone";
pub(super) const CARRIER_TEXT: &str = "text";
pub(super) const CARRIER_FUNCTION: &str = "function";
pub(super) const CARRIER_ANY: &str = "any";

pub(super) const CARRIER_DIRECTION_FIELD: &str = "_cpa_reasoning_direction";
pub(super) const CARRIER_TARGET_FIELD: &str = "_cpa_reasoning_target";
pub(super) const CARRIER_SIGNATURE_FIELD: &str = "_cpa_reasoning_signature";
pub(super) const CARRIER_SUMMARY_FIELD: &str = "_cpa_reasoning_summary";

/// Encodes a raw signature into a carrier string; empty for a blank signature.
pub(super) fn encode_gemini_responses_carrier(raw_signature: &str, direction: &str, target_kind: &str) -> String {
    let raw_signature = raw_signature.trim();
    if raw_signature.is_empty() {
        return String::new();
    }
    format!("{CARRIER_PREFIX}{direction}:{target_kind}:{}", b64::encode_raw_std(raw_signature.as_bytes()))
}

pub(super) struct DecodedCarrier {
    pub signature: String,
    pub direction: String,
    pub target_kind: String,
    /// The string carried the carrier prefix.
    pub marked: bool,
    /// Decoding succeeded (always true for unmarked strings).
    pub ok: bool,
}

/// Go `decodeGeminiResponsesCarrier`: unmarked strings pass through as plain signatures.
pub(super) fn decode_gemini_responses_carrier(raw_signature: &str) -> DecodedCarrier {
    let raw_signature = raw_signature.trim();
    let invalid = || DecodedCarrier { signature: String::new(), direction: String::new(), target_kind: String::new(), marked: true, ok: false };
    let Some(rest) = raw_signature.strip_prefix(CARRIER_PREFIX) else {
        return DecodedCarrier { signature: raw_signature.to_string(), direction: String::new(), target_kind: String::new(), marked: false, ok: true };
    };
    if raw_signature.len() > (MAX_GEMINI_THOUGHT_SIGNATURE_LEN * 4 / 3) + 1024 {
        return invalid();
    }
    let fields: Vec<&str> = rest.splitn(3, ':').collect();
    if fields.len() != 3 {
        return invalid();
    }
    let (direction, target_kind) = (fields[0], fields[1]);
    if !matches!(direction, CARRIER_NEXT | CARRIER_PREVIOUS | CARRIER_STANDALONE) {
        return invalid();
    }
    if !matches!(target_kind, CARRIER_TEXT | CARRIER_FUNCTION | CARRIER_ANY) {
        return invalid();
    }
    let Ok(decoded) = b64::raw_std(fields[2]) else {
        return invalid();
    };
    if decoded.is_empty() || decoded.starts_with(CARRIER_PREFIX.as_bytes()) {
        return invalid();
    }
    DecodedCarrier {
        signature: String::from_utf8_lossy(&decoded).into_owned(),
        direction: direction.to_string(),
        target_kind: target_kind.to_string(),
        marked: true,
        ok: true,
    }
}

/// The Gemini-compatible form of a carried signature, excluding bypass sentinels.
pub(super) fn compatible_gemini_responses_carrier_signature(raw_signature: &str, target_kind: &str) -> Option<String> {
    let block_kind = if target_kind == CARRIER_FUNCTION { SignatureBlockKind::GeminiFunctionCall } else { SignatureBlockKind::GeminiModelPart };
    let normalized = compatible_signature_for_provider_block(SignatureProvider::Gemini, raw_signature, block_kind)?;
    if is_gemini_thought_signature_bypass(&signature_payload_without_provider_prefix(&normalized)) {
        return None;
    }
    Some(normalized)
}

/// What a neighbouring item could be bound to: `function`, `text`, or "" for neither.
fn carrier_semantic_target(item: &Res<'_>) -> &'static str {
    match item.g("type").str().as_str() {
        "function_call" | "custom_tool_call" => return CARRIER_FUNCTION,
        "reasoning" if !item.g("summary.0.text").str().trim().is_empty() => return CARRIER_TEXT,
        _ => {}
    }
    if openai_responses_assistant_visible_text(item).is_some() {
        return CARRIER_TEXT;
    }
    ""
}

/// True when the nearest semantic item in the carrier's direction matches `target_kind`
/// (skipping other detached carriers).
fn carrier_matches_adjacent(items: &[Res<'_>], index: usize, direction: &str, target_kind: &str) -> bool {
    let step: isize = if direction == CARRIER_PREVIOUS { -1 } else { 1 };
    let mut adjacent = index as isize + step;
    while adjacent >= 0 && (adjacent as usize) < items.len() {
        let item = &items[adjacent as usize];
        let kind = carrier_semantic_target(item);
        if !kind.is_empty() {
            return target_kind == CARRIER_ANY || target_kind == kind;
        }
        if !is_openai_responses_detached_carrier(item) {
            return false;
        }
        adjacent += step;
    }
    false
}

fn has_internal_carrier_fields(item: &Res<'_>) -> bool {
    [CARRIER_DIRECTION_FIELD, CARRIER_TARGET_FIELD, CARRIER_SIGNATURE_FIELD, CARRIER_SUMMARY_FIELD]
        .iter()
        .any(|f| item.g(f).exists())
}

/// Removes the internal `_cpa_reasoning_*` fields from an item; `None` when it is not an object.
fn strip_gemini_responses_carrier_metadata(item: &Res<'_>) -> Option<Value> {
    let mut value = item.value();
    let fields = value.as_object_mut()?;
    for field in [CARRIER_DIRECTION_FIELD, CARRIER_TARGET_FIELD, CARRIER_SIGNATURE_FIELD, CARRIER_SUMMARY_FIELD] {
        fields.shift_remove(field);
    }
    Some(value)
}

/// Decodes carriers in reasoning items: valid ones get the raw signature plus internal direction
/// and target fields, invalid ones lose `encrypted_content` (or the whole item when it has no
/// summary). The flag reports whether any valid Gemini carrier or raw Gemini signature exists.
pub(super) fn normalize_gemini_responses_carriers<'a>(items: &[Res<'a>]) -> (Vec<Res<'a>>, bool) {
    let mut normalized: Vec<Res<'a>> = Vec::with_capacity(items.len());
    let mut has_valid_carrier = false;
    for (item_index, original_item) in items.iter().enumerate() {
        let mut item = original_item.clone();
        let mut item_json: Option<Value> = None;
        if has_internal_carrier_fields(original_item)
            && let Some(stripped) = strip_gemini_responses_carrier_metadata(original_item) {
                item = Res::owned(stripped.clone());
                item_json = Some(stripped);
            }
        if item.g("type").str() != "reasoning" {
            normalized.push(item);
            continue;
        }
        let mut item_json = item_json.unwrap_or_else(|| item.value());
        let raw_signature = item.g("encrypted_content").str().trim().to_string();
        let decoded = decode_gemini_responses_carrier(&raw_signature);
        if !decoded.marked {
            if !raw_signature.is_empty() {
                let compatible = compatible_gemini_responses_carrier_signature(&raw_signature, CARRIER_ANY).is_some();
                has_valid_carrier = has_valid_carrier || compatible;
            }
            normalized.push(item);
            continue;
        }
        let (direction, target_kind) = (decoded.direction, decoded.target_kind);
        let mut ok = decoded.ok;
        let mut signature = decoded.signature;
        if ok {
            match compatible_gemini_responses_carrier_signature(&signature, &target_kind) {
                Some(s) => signature = s,
                None => ok = false,
            }
        }
        if ok && direction != CARRIER_STANDALONE {
            ok = carrier_matches_adjacent(items, item_index, &direction, &target_kind);
        }
        let is_detached = is_openai_responses_detached_carrier(&item);
        let has_summary = !item.g("summary.0.text").str().trim().is_empty();
        let valid_summary_carrier = has_summary
            && ((direction == CARRIER_STANDALONE && (target_kind == CARRIER_TEXT || target_kind == CARRIER_ANY)) || direction == CARRIER_NEXT);
        if !ok || (!is_detached && !valid_summary_carrier) {
            if item.g("summary.0.text").str().trim().is_empty() {
                continue;
            }
            cpa_json::delete(&mut item_json, "encrypted_content");
            normalized.push(Res::owned(item_json));
            continue;
        }
        has_valid_carrier = true;
        cpa_json::set(&mut item_json, "encrypted_content", signature);
        cpa_json::set(&mut item_json, CARRIER_DIRECTION_FIELD, direction);
        cpa_json::set(&mut item_json, CARRIER_TARGET_FIELD, target_kind);
        normalized.push(Res::owned(item_json));
    }
    (normalized, has_valid_carrier)
}

pub(super) fn gemini_responses_carrier_direction(item: &Res<'_>) -> String {
    item.g(CARRIER_DIRECTION_FIELD).str()
}

pub(super) fn gemini_responses_carrier_target(item: &Res<'_>) -> String {
    item.g(CARRIER_TARGET_FIELD).str()
}

/// A reasoning item with a signature but no summary text.
pub(super) fn is_openai_responses_detached_carrier(item: &Res<'_>) -> bool {
    item.g("type").str() == "reasoning"
        && !item.g("encrypted_content").str().trim().is_empty()
        && item.g("summary.0.text").str().trim().is_empty()
}
