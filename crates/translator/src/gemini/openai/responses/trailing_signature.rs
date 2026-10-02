//! Text signatures that arrive after visible text are cached instead of exposed (Go:
//! trailing_signature.go). The replay cache bounds their lifetime and size.

use std::collections::HashSet;

use cpa_core::cache::{cache_antigravity_reasoning_replay_items, get_antigravity_reasoning_replay_items};
use cpa_core::thinking::parse_suffix;
use cpa_json::{Res, J};
use sha2::{Digest, Sha256};

use super::request::openai_responses_assistant_visible_text;
use super::signature_carrier::{
    compatible_gemini_responses_carrier_signature, decode_gemini_responses_carrier, encode_gemini_responses_carrier,
    is_openai_responses_detached_carrier, CARRIER_PREVIOUS, CARRIER_TEXT,
};

fn text_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Stores the text signatures of one assistant message in the replay cache. False when the
/// message id or text is empty, a signature is not Gemini-compatible, or the cache refuses.
pub(super) fn cache_gemini_responses_text_signatures(model_name: &str, message_id: &str, text: &str, signatures: &[String]) -> bool {
    if message_id.is_empty() || text.is_empty() {
        return false;
    }
    let text_hash = text_hash(text);
    let mut items: Vec<Vec<u8>> = Vec::with_capacity(signatures.len());
    for signature in signatures {
        if compatible_gemini_responses_carrier_signature(signature, CARRIER_TEXT).is_none() {
            return false;
        }
        let mut item = cpa_json::parse_str(r#"{"type":"thought_signature","targetKind":"text"}"#);
        cpa_json::set(&mut item, "thoughtSignature", signature.as_str());
        cpa_json::set(&mut item, "targetHash", text_hash.as_str());
        items.push(cpa_json::to_vec(&item));
    }
    cache_antigravity_reasoning_replay_items(&parse_suffix(model_name).model_name, &format!("gemini-responses-text:{message_id}"), &items)
}

/// Re-injects cached text signatures after the assistant messages they belong to, as
/// previous-direction text carriers (the hash of the message text must match).
pub(super) fn restore_gemini_responses_text_signatures<'a>(model_name: &str, items: Vec<Res<'a>>) -> Vec<Res<'a>> {
    let mut restored: Vec<Res<'a>> = Vec::with_capacity(items.len());
    let mut skip: HashSet<usize> = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        restored.push(item.clone());
        let Some(text) = openai_responses_assistant_visible_text(item) else { continue };
        let message_id = item.g("id").str().trim().to_string();
        if message_id.is_empty() {
            continue;
        }
        let Some(cached) = get_antigravity_reasoning_replay_items(&parse_suffix(model_name).model_name, &format!("gemini-responses-text:{message_id}")) else {
            continue;
        };
        // Replay the cached prefix in its original order, then retain uncached explicit
        // carriers. Skipping cached entries instead would reorder A,B into B,A when the
        // client also supplied an explicit carrier for A.
        let mut replayed: HashSet<String> = HashSet::new();
        let hash = text_hash(&text);
        for raw in &cached {
            let entry = cpa_json::parse(raw);
            let signature = entry.g("thoughtSignature").str();
            if entry.g("targetHash").str() != hash {
                continue;
            }
            let mut carrier = cpa_json::parse_str(r#"{"type":"reasoning","summary":[]}"#);
            cpa_json::set(&mut carrier, "encrypted_content", encode_gemini_responses_carrier(&signature, CARRIER_PREVIOUS, CARRIER_TEXT));
            restored.push(Res::owned(carrier));
            replayed.insert(signature);
        }
        let mut adjacent = index + 1;
        while adjacent < items.len() && is_openai_responses_detached_carrier(&items[adjacent]) {
            let decoded = decode_gemini_responses_carrier(&items[adjacent].g("encrypted_content").str());
            if decoded.ok && decoded.direction == CARRIER_PREVIOUS && decoded.target_kind == CARRIER_TEXT && replayed.contains(&decoded.signature) {
                skip.insert(adjacent);
            }
            adjacent += 1;
        }
    }
    restored
}
