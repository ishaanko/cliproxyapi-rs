//! Gemini replay policy for thought signatures (Go: signature/gemini_sanitize.go).

use cpa_json::{J, Res, Value};

use super::gemini::{
    gemini_part_thought_signature, has_normalized_gemini_part_thought_signature,
    is_gemini_thought_signature_bypass, GEMINI_PART_THOUGHT_SIGNATURE_PATHS,
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
};
use super::provider::{
    compatible_signature_for_provider_block, decide_signature_compatibility,
    detect_signature_provider_for_block, signature_payload_without_provider_prefix,
    SignatureBlockKind, SignatureCompatibilityAction, SignatureCompatibilityDecision,
    SignatureProvider,
};

/// A Gemini-replayable thoughtSignature: compatible signatures are normalized and preserved;
/// missing, unknown or cross-provider signatures become Gemini's bypass sentinel.
pub fn gemini_replay_signature_or_bypass(raw_signature: &str, block_kind: SignatureBlockKind) -> String {
    if let Some(signature) =
        compatible_signature_for_provider_block(SignatureProvider::Gemini, raw_signature, block_kind)
    {
        return signature;
    }
    let decision = decide_signature_compatibility(SignatureProvider::Gemini, raw_signature, block_kind);
    if decision.action == SignatureCompatibilityAction::ReplaceWithGeminiBypass
        && !decision.replacement_signature.is_empty()
    {
        return decision.replacement_signature;
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
}

#[derive(PartialEq, Eq)]
struct SanitizeLogKey {
    action: SignatureCompatibilityAction,
    reason: String,
    block_kind: SignatureBlockKind,
    detected_provider: SignatureProvider,
}

/// Applies Gemini replay policy to a Gemini-shaped request (JSON bytes). Existing provider
/// signatures stay on their original model parts and server-side tool blocks (toolCall,
/// toolResponse) are echoed untouched. Only a missing or incompatible first functionCall gets the
/// bypass sentinel; unsigned sibling calls stay unsigned, and functionResponse parts never carry
/// signatures. `contents_path` defaults to `contents` when blank.
pub fn sanitize_gemini_request_thought_signatures(payload: &[u8], contents_path: &str) -> Vec<u8> {
    let contents_path = match contents_path.trim() {
        "" => "contents",
        trimmed => trimmed,
    };

    let mut root = cpa_json::parse(payload);
    let contents = root.g(contents_path);
    if !contents.is_array() || !contents_thought_signatures_need_sanitize(&contents) {
        return payload.to_vec();
    }

    let mut contents_changed = false;
    let mut content_items: Vec<Value> = Vec::new();
    // Insertion-ordered (key, count) pairs, logged after a successful rewrite.
    let mut sanitize_counts: Vec<(SanitizeLogKey, usize)> = Vec::new();
    let mut count_sanitize = |decision: &SignatureCompatibilityDecision| {
        let key = SanitizeLogKey {
            action: decision.action,
            reason: decision.reason.clone(),
            block_kind: decision.block_kind,
            detected_provider: decision.detected_provider,
        };
        match sanitize_counts.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, count)) => *count += 1,
            None => sanitize_counts.push((key, 1)),
        }
    };

    for content in contents.array() {
        let parts = content.g("parts");
        if !parts.is_array() {
            content_items.push(content.value());
            continue;
        }

        let is_model_turn = content.g("role").str() == "model";
        let mut first_function_call_seen = false;
        let mut parts_changed = false;
        let mut part_items: Vec<Value> = Vec::new();
        for part in parts.array() {
            let mut part_json = part.value();
            let (raw_signature, has_signature) = gemini_part_thought_signature(&part);
            if part.g("functionResponse").exists() {
                if has_signature {
                    delete_gemini_part_thought_signature_fields(&mut part_json);
                    parts_changed = true;
                    count_sanitize(&SignatureCompatibilityDecision {
                        target_provider: SignatureProvider::Gemini,
                        detected_provider: detect_signature_provider_for_block(
                            &raw_signature,
                            SignatureBlockKind::GeminiModelPart,
                        ),
                        block_kind: SignatureBlockKind::GeminiModelPart,
                        action: SignatureCompatibilityAction::DropSignature,
                        reason: "tool responses cannot carry signatures".to_string(),
                        ..Default::default()
                    });
                }
                part_items.push(part_json);
                continue;
            }
            if !is_model_turn || is_server_tool_part(&part) {
                part_items.push(part_json);
                continue;
            }

            let has_function_call = part.g("functionCall").exists();
            let is_first_function_call = has_function_call && !first_function_call_seen;
            if has_function_call {
                first_function_call_seen = true;
            }
            if !has_function_call && !has_signature {
                part_items.push(part_json);
                continue;
            }

            let block_kind = if has_function_call {
                SignatureBlockKind::GeminiFunctionCall
            } else {
                SignatureBlockKind::GeminiModelPart
            };
            let mut decision =
                decide_signature_compatibility(SignatureProvider::Gemini, &raw_signature, block_kind);
            let mut replay_signature = String::new();
            if is_first_function_call {
                replay_signature = gemini_replay_signature_or_bypass(&raw_signature, block_kind);
                if decision.action == SignatureCompatibilityAction::ReplaceWithGeminiBypass {
                    decision.reason = "missing or incompatible signature".to_string();
                }
            } else if has_signature
                && decision.action == SignatureCompatibilityAction::Preserve
                && !is_gemini_thought_signature_bypass(&signature_payload_without_provider_prefix(
                    &raw_signature,
                ))
            {
                replay_signature = decision.normalized_signature.clone();
            } else if has_signature {
                decision.action = SignatureCompatibilityAction::DropSignature;
                decision.replacement_signature = String::new();
                decision.reason = if has_function_call {
                    "sibling calls must be unsigned"
                } else {
                    "text parts cannot carry signatures"
                }
                .to_string();
            }

            let mut part_changed = false;
            if !replay_signature.is_empty() {
                if !has_normalized_gemini_part_thought_signature(&part, &replay_signature) {
                    delete_gemini_part_thought_signature_fields(&mut part_json);
                    cpa_json::set(&mut part_json, "thoughtSignature", replay_signature.as_str());
                    part_changed = true;
                }
            } else if has_signature {
                delete_gemini_part_thought_signature_fields(&mut part_json);
                part_changed = true;
            }
            if part_changed {
                parts_changed = true;
                if decision.action != SignatureCompatibilityAction::Preserve {
                    count_sanitize(&decision);
                }
            }
            part_items.push(part_json);
        }

        let mut content_json = content.value();
        if parts_changed {
            cpa_json::set(&mut content_json, "parts", Value::Array(part_items));
            contents_changed = true;
        }
        content_items.push(content_json);
    }

    if !contents_changed {
        return payload.to_vec();
    }
    cpa_json::set(&mut root, contents_path, Value::Array(content_items));
    for (key, count) in &sanitize_counts {
        let noun = if *count > 1 { "thoughtSignatures" } else { "thoughtSignature" };
        tracing::debug!(
            "gemini request: sanitized {count} {noun} action={} part={} sig_type={} path={contents_path} reason={:?}",
            sanitize_action_name(key.action),
            sanitize_kind_name(key.block_kind),
            sanitize_sig_type_name(key.detected_provider),
            key.reason,
        );
    }
    cpa_json::to_vec(&root)
}

fn is_server_tool_part(part: &Res<'_>) -> bool {
    ["toolCall", "tool_call", "toolResponse", "tool_response"]
        .iter()
        .any(|key| part.g(key).exists())
}

fn sanitize_action_name(action: SignatureCompatibilityAction) -> &'static str {
    match action {
        SignatureCompatibilityAction::DropSignature => "drop",
        SignatureCompatibilityAction::ReplaceWithGeminiBypass => "replace_bypass",
        SignatureCompatibilityAction::DropBlock => "drop_block",
        other => other.as_str(),
    }
}

fn sanitize_kind_name(kind: SignatureBlockKind) -> &'static str {
    match kind {
        SignatureBlockKind::GeminiFunctionCall => "function_call",
        SignatureBlockKind::GeminiModelPart => "model_part",
        other => {
            let name = other.as_str();
            name.strip_prefix("gemini_").unwrap_or(name)
        }
    }
}

fn sanitize_sig_type_name(provider: SignatureProvider) -> &'static str {
    match provider {
        SignatureProvider::GeminiBypass => "bypass",
        other => other.as_str(),
    }
}

/// Dry run of the sanitizer: true when any part would change.
fn contents_thought_signatures_need_sanitize(contents: &Res<'_>) -> bool {
    for content in contents.array() {
        let parts = content.g("parts");
        if !parts.is_array() {
            continue;
        }
        let is_model_turn = content.g("role").str() == "model";
        let mut first_function_call_seen = false;
        for part in parts.array() {
            let (raw_signature, has_signature) = gemini_part_thought_signature(&part);
            if part.g("functionResponse").exists() {
                // Go overwrites the flag per part, so only the last functionResponse decides.
                if has_signature {
                    return true;
                }
                continue;
            }
            if !is_model_turn || is_server_tool_part(&part) {
                continue;
            }
            let has_function_call = part.g("functionCall").exists();
            let is_first_function_call = has_function_call && !first_function_call_seen;
            if has_function_call {
                first_function_call_seen = true;
            }
            if is_first_function_call {
                let replay_signature =
                    gemini_replay_signature_or_bypass(&raw_signature, SignatureBlockKind::GeminiFunctionCall);
                if !has_normalized_gemini_part_thought_signature(&part, &replay_signature) {
                    return true;
                }
                continue;
            }
            if !has_signature {
                continue;
            }
            let block_kind = if has_function_call {
                SignatureBlockKind::GeminiFunctionCall
            } else {
                SignatureBlockKind::GeminiModelPart
            };
            let decision = decide_signature_compatibility(SignatureProvider::Gemini, &raw_signature, block_kind);
            if decision.action != SignatureCompatibilityAction::Preserve
                || is_gemini_thought_signature_bypass(&signature_payload_without_provider_prefix(&raw_signature))
            {
                return true;
            }
            if !has_normalized_gemini_part_thought_signature(&part, &decision.normalized_signature) {
                return true;
            }
        }
    }
    false
}

/// Removes every thoughtSignature alias from a part (each path repeatedly, as sjson deletes only
/// the first of duplicate keys).
fn delete_gemini_part_thought_signature_fields(part: &mut Value) {
    for path in GEMINI_PART_THOUGHT_SIGNATURE_PATHS {
        while part.g(path).exists() {
            let before = part.clone();
            cpa_json::delete(part, path);
            if *part == before {
                break;
            }
        }
    }
}
