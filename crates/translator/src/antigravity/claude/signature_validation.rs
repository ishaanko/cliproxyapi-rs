//! Claude thinking signature validation wrappers for Antigravity bypass mode
//! (Go: antigravity/claude/signature_validation.go).

use cpa_core::cache;
use cpa_core::signature::{
    self, b64, ClaudeSignatureValidationOptions, SignatureBlockKind, SignatureProvider,
    MAX_GEMINI_THOUGHT_SIGNATURE_LEN,
};
use cpa_json::{J, Value};

use crate::common::join_raw_array;

/// Gemini carrier envelopes exist only on the Claude-facing wire. The request translator
/// validates and unwraps them before writing native Gemini parts.
pub const CARRIER_PREFIX: &str = "cpa-gemini-carrier-v1:";
pub const CARRIER_NEXT: &str = "next";
pub const CARRIER_PREVIOUS: &str = "previous";
pub const CARRIER_STANDALONE: &str = "standalone";
pub const CARRIER_TEXT: &str = "text";
pub const CARRIER_FUNCTION: &str = "function";
pub const CARRIER_ANY: &str = "any";

/// `rawSignature` wrapped as `cpa-gemini-carrier-v1:<direction>:<kind>:<rawb64>`; "" for blank input.
pub fn encode_gemini_claude_carrier_signature(raw_signature: &str, direction: &str, target_kind: &str) -> String {
    let raw_signature = raw_signature.trim();
    if raw_signature.is_empty() {
        return String::new();
    }
    format!("{CARRIER_PREFIX}{direction}:{target_kind}:{}", b64::encode_raw_std(raw_signature.as_bytes()))
}

/// Result of [`decode_gemini_claude_carrier_signature`] (Go returns a 5-tuple).
#[derive(Debug, Clone, Default)]
pub struct Carrier {
    pub signature: String,
    pub direction: String,
    pub target_kind: String,
    /// The input carried the carrier prefix.
    pub marked: bool,
    /// Unmarked signatures are always ok; marked ones only when the envelope validates.
    pub ok: bool,
}

fn rejected() -> Carrier {
    Carrier { marked: true, ..Default::default() }
}

pub fn decode_gemini_claude_carrier_signature(raw_signature: &str) -> Carrier {
    let raw_signature = raw_signature.trim();
    let Some(rest) = raw_signature.strip_prefix(CARRIER_PREFIX) else {
        return Carrier { signature: raw_signature.to_string(), ok: true, ..Default::default() };
    };
    if raw_signature.len() > (MAX_GEMINI_THOUGHT_SIGNATURE_LEN * 4 / 3) + 1024 {
        return rejected();
    }
    let fields: Vec<&str> = rest.splitn(3, ':').collect();
    if fields.len() != 3 {
        return rejected();
    }
    let (direction, target_kind) = (fields[0], fields[1]);
    if !matches!(direction, CARRIER_NEXT | CARRIER_PREVIOUS | CARRIER_STANDALONE) {
        return rejected();
    }
    if !matches!(target_kind, CARRIER_TEXT | CARRIER_FUNCTION | CARRIER_ANY) {
        return rejected();
    }
    let Some(decoded) = b64::raw_std(fields[2]).ok() else {
        return rejected();
    };
    let decoded = String::from_utf8_lossy(&decoded).into_owned();
    if decoded.is_empty() || decoded.starts_with(CARRIER_PREFIX) {
        return rejected();
    }
    let block_kind = if target_kind == CARRIER_FUNCTION {
        SignatureBlockKind::GeminiFunctionCall
    } else {
        SignatureBlockKind::GeminiModelPart
    };
    let Some(normalized) = signature::compatible_signature_for_provider_block(SignatureProvider::Gemini, &decoded, block_kind) else {
        return rejected();
    };
    if signature::is_gemini_thought_signature_bypass(&signature::signature_payload_without_provider_prefix(&normalized)) {
        return rejected();
    }
    Carrier {
        signature: normalized,
        direction: direction.to_string(),
        target_kind: target_kind.to_string(),
        marked: true,
        ok: true,
    }
}

fn block_kind_for(marked: bool, target_kind: &str) -> SignatureBlockKind {
    if marked && target_kind == CARRIER_FUNCTION {
        SignatureBlockKind::GeminiFunctionCall
    } else {
        SignatureBlockKind::GeminiModelPart
    }
}

/// Visible-content kind of a Claude block as seen by carrier placement ("" for none).
fn semantic_target_kind(block: &Value) -> &'static str {
    match block.g("type").str().as_str() {
        "text" => CARRIER_TEXT,
        "tool_use" => CARRIER_FUNCTION,
        "thinking" if !block.g("thinking").str().trim().is_empty() => CARRIER_TEXT,
        _ => "",
    }
}

/// Whether the nearest semantic neighbour of `blocks[index]` in `direction` can take a carrier of
/// `target_kind`, skipping only empty thinking carriers in between.
pub fn carrier_matches_adjacent(blocks: &[&Value], index: usize, direction: &str, target_kind: &str) -> bool {
    let step: isize = if direction == CARRIER_PREVIOUS { -1 } else { 1 };
    let mut adjacent = index as isize + step;
    while adjacent >= 0 && (adjacent as usize) < blocks.len() {
        let block = &blocks[adjacent as usize];
        let kind = semantic_target_kind(block);
        if !kind.is_empty() {
            return target_kind == CARRIER_ANY || target_kind == kind;
        }
        if block.g("type").str() != "thinking" || !block.g("thinking").str().trim().is_empty() {
            return false;
        }
        adjacent += step;
    }
    false
}

fn is_valid_non_empty_thinking(raw_signature: &str, next_semantic_kind: &str) -> bool {
    if raw_signature.is_empty() {
        return false;
    }
    let c = decode_gemini_claude_carrier_signature(raw_signature);
    let block_kind = block_kind_for(c.marked, &c.target_kind);
    if c.ok {
        if signature::compatible_signature_for_provider_block(SignatureProvider::Gemini, &c.signature, block_kind).is_none() {
            return false;
        }
        if c.marked {
            if c.direction == CARRIER_PREVIOUS {
                return false;
            }
            if c.direction == CARRIER_STANDALONE && c.target_kind == CARRIER_FUNCTION {
                return false;
            }
            if c.direction == CARRIER_NEXT
                && (next_semantic_kind.is_empty() || (c.target_kind != CARRIER_ANY && c.target_kind != next_semantic_kind))
            {
                return false;
            }
        }
        return true;
    }
    signature::compatible_signature_for_provider_block(SignatureProvider::Gemini, raw_signature, SignatureBlockKind::GeminiModelPart).is_some()
}

struct CarrierContext {
    next_semantic_kind: Vec<&'static str>,
    has_trailing_previous_carrier: Vec<bool>,
}

/// Backward scan computing, per block, the kind of the next semantic block and whether a trailing
/// `previous` carrier binds to it.
fn precompute_carrier_context(blocks: &[Value]) -> CarrierContext {
    let n = blocks.len();
    let mut ctx = CarrierContext { next_semantic_kind: vec![""; n], has_trailing_previous_carrier: vec![false; n] };
    let mut active_kind = String::new();
    let mut active_valid = false;
    let mut latest_semantic: Option<usize> = None;
    let mut current_next: &'static str = "";

    for i in (0..n).rev() {
        let block = &blocks[i];
        let block_type = block.g("type").str();
        ctx.next_semantic_kind[i] = current_next;
        match block_type.as_str() {
            "thinking" => {
                let thinking_text = block.g("thinking").str().trim().to_string();
                let raw_sig = block.g("signature").str().trim().to_string();
                if thinking_text.is_empty() {
                    let c = decode_gemini_claude_carrier_signature(&raw_sig);
                    if c.ok && c.marked && c.direction == CARRIER_PREVIOUS {
                        let block_kind = block_kind_for(true, &c.target_kind);
                        if signature::compatible_signature_for_provider_block(SignatureProvider::Gemini, &c.signature, block_kind).is_some() {
                            active_kind = c.target_kind;
                            active_valid = true;
                            continue;
                        }
                    }
                    active_kind.clear();
                    active_valid = false;
                    continue;
                }
                if !raw_sig.is_empty() && !is_valid_non_empty_thinking(&raw_sig, current_next) {
                    active_kind.clear();
                    active_valid = false;
                    latest_semantic = None;
                    current_next = "";
                    continue;
                }
                current_next = CARRIER_TEXT;
                if active_valid && (active_kind == CARRIER_ANY || active_kind == CARRIER_TEXT) {
                    ctx.has_trailing_previous_carrier[i] = true;
                } else if latest_semantic.is_some_and(|l| ctx.has_trailing_previous_carrier[l]) {
                    ctx.has_trailing_previous_carrier[i] = true;
                }
                active_kind.clear();
                active_valid = false;
                latest_semantic = Some(i);
            }
            "text" | "tool_use" => {
                let semantic_kind = if block_type == "tool_use" { CARRIER_FUNCTION } else { CARRIER_TEXT };
                current_next = semantic_kind;
                if active_valid && (active_kind == CARRIER_ANY || active_kind == semantic_kind) {
                    ctx.has_trailing_previous_carrier[i] = true;
                }
                active_kind.clear();
                active_valid = false;
                latest_semantic = Some(i);
            }
            _ => {
                active_kind.clear();
                active_valid = false;
                latest_semantic = None;
                current_next = "";
            }
        }
    }
    ctx
}

/// Removes thinking blocks whose signatures are empty or not valid Claude thinking signatures.
/// These usually come from proxy-generated responses where no real Claude signature exists.
pub fn strip_empty_signature_thinking_blocks(payload: &[u8]) -> Vec<u8> {
    signature::strip_invalid_claude_thinking_blocks(
        payload,
        ClaudeSignatureValidationOptions { prefix_only: true, ..Default::default() },
    )
}

/// Preserves only thinking carriers whose signatures can be replayed to Gemini. Claude Code uses
/// these carriers to return provider-native signatures from prior translated responses.
pub fn strip_invalid_gemini_signature_thinking_blocks(payload: &[u8]) -> Vec<u8> {
    let root = cpa_json::parse(payload);
    let messages = root.g("messages");
    if !messages.is_array() {
        return payload.to_vec();
    }
    let mut changed = false;
    let mut message_items: Vec<Vec<u8>> = Vec::new();
    for message in messages.array() {
        let message_value = message.value();
        let content = message.g("content");
        if !content.is_array() {
            message_items.push(cpa_json::to_vec(&message_value));
            continue;
        }
        let mut content_changed = false;
        let assistant_message = message.g("role").str().eq_ignore_ascii_case("assistant");
        let content_blocks: Vec<Value> = content.array().iter().map(|b| b.value()).collect();
        let carrier_ctx = precompute_carrier_context(&content_blocks);
        let mut content_items: Vec<Vec<u8>> = Vec::with_capacity(content_blocks.len());
        let mut pending_carrier_target_kind = String::new();
        let mut current_prev_semantic_kind = "";
        for (block_index, block) in content_blocks.iter().enumerate() {
            let block_type = block.g("type").str();
            if block_type == "thinking" {
                let raw_signature = block.g("signature").str().trim().to_string();
                let thinking_text = block.g("thinking").str().trim().to_string();
                if assistant_message
                    && raw_signature.is_empty()
                    && !thinking_text.is_empty()
                    && (pending_carrier_target_kind == CARRIER_ANY
                        || pending_carrier_target_kind == CARRIER_TEXT
                        || carrier_ctx.has_trailing_previous_carrier[block_index])
                {
                    pending_carrier_target_kind.clear();
                    current_prev_semantic_kind = CARRIER_TEXT;
                    content_items.push(cpa_json::to_vec(block));
                    continue;
                }
                let c = decode_gemini_claude_carrier_signature(&raw_signature);
                let block_kind = block_kind_for(c.marked, &c.target_kind);
                let mut invalid_marked_placement = false;
                if c.marked {
                    match c.direction.as_str() {
                        CARRIER_NEXT => {
                            let next_kind = carrier_ctx.next_semantic_kind[block_index];
                            invalid_marked_placement = next_kind.is_empty() || (c.target_kind != CARRIER_ANY && c.target_kind != next_kind);
                        }
                        CARRIER_PREVIOUS => {
                            invalid_marked_placement = current_prev_semantic_kind.is_empty()
                                || (c.target_kind != CARRIER_ANY && c.target_kind != current_prev_semantic_kind);
                        }
                        CARRIER_STANDALONE => {
                            invalid_marked_placement = !thinking_text.is_empty() && c.target_kind == CARRIER_FUNCTION;
                        }
                        _ => {}
                    }
                    if !thinking_text.is_empty() && c.direction == CARRIER_PREVIOUS {
                        invalid_marked_placement = true;
                    }
                }
                if !c.ok || !assistant_message || invalid_marked_placement {
                    pending_carrier_target_kind.clear();
                    if !thinking_text.is_empty() {
                        current_prev_semantic_kind = "";
                    }
                    content_changed = true;
                    continue;
                }
                let inner_signature = if c.marked { c.signature.clone() } else { raw_signature.clone() };
                if signature::compatible_signature_for_provider_block(SignatureProvider::Gemini, &inner_signature, block_kind).is_none() {
                    pending_carrier_target_kind.clear();
                    if !thinking_text.is_empty() {
                        current_prev_semantic_kind = "";
                    }
                    content_changed = true;
                    continue;
                }
                if c.marked && c.direction == CARRIER_NEXT {
                    pending_carrier_target_kind = c.target_kind.clone();
                } else {
                    pending_carrier_target_kind.clear();
                }
                if !thinking_text.is_empty() {
                    current_prev_semantic_kind = CARRIER_TEXT;
                }
            } else {
                pending_carrier_target_kind.clear();
                current_prev_semantic_kind = match block_type.as_str() {
                    "tool_use" => CARRIER_FUNCTION,
                    "text" => CARRIER_TEXT,
                    _ => "",
                };
            }
            content_items.push(cpa_json::to_vec(block));
        }
        let mut message_json = message_value;
        if content_changed {
            cpa_json::set(&mut message_json, "content", cpa_json::parse(&join_raw_array(&content_items)));
            changed = true;
        }
        message_items.push(cpa_json::to_vec(&message_json));
    }
    if !changed {
        return payload.to_vec();
    }
    let mut updated = root.clone();
    cpa_json::set(&mut updated, "messages", cpa_json::parse(&join_raw_array(&message_items)));
    cpa_json::to_vec(&updated)
}

pub fn strip_invalid_bypass_signature_thinking_blocks(payload: &[u8]) -> Vec<u8> {
    signature::strip_invalid_claude_thinking_blocks(payload, bypass_validation_options())
}

pub fn validate_claude_bypass_signatures(input_raw_json: &[u8]) -> Result<(), signature::SignatureError> {
    signature::validate_claude_thinking_signatures(input_raw_json, bypass_validation_options())
}

pub fn normalize_claude_bypass_signature(raw_signature: &str) -> Result<String, signature::SignatureError> {
    signature::normalize_claude_thinking_signature(raw_signature, bypass_validation_options())
}

fn bypass_validation_options() -> ClaudeSignatureValidationOptions {
    ClaudeSignatureValidationOptions { strict: cache::signature_bypass_strict_mode(), ..Default::default() }
}
