//! Request signature and turn-shape normalization (Go: the signature helpers of
//! antigravity_executor.go).

use cpa_core::cache::{signature_bypass_strict_mode, signature_cache_enabled};
use cpa_core::signature::sanitize_gemini_request_thought_signatures;
use cpa_json::J;
use cpa_translator::Format;
use cpa_translator::antigravity::claude as ag_claude;
use serde_json::Value;

use crate::helps::gemini_content_turns::{ensure_gemini_boundary_user_content, ensure_gemini_leading_user_content};

/// Gemini-family models (not Claude) use the reasoning replay ledger.
pub(crate) fn uses_reasoning_replay_cache(model_name: &str) -> bool {
    let m = model_name.to_lowercase();
    if m.contains("claude") {
        return false;
    }
    m.contains("gemini") || m.contains("flash") || m.contains("agent")
}

/// Thought signature sanitizing plus functionResponse repairs for replay-cache models.
pub(crate) fn sanitize_gemini_request_signatures(model_name: &str, raw: Vec<u8>) -> Vec<u8> {
    if !uses_reasoning_replay_cache(model_name) {
        return raw;
    }
    let raw = sanitize_gemini_request_thought_signatures(&raw, "request.contents");
    normalize_function_response_roles(raw)
}

/// Both leading and trailing user turns for Gemini targets; Claude targets stay untouched.
pub(crate) fn ensure_boundary_user_content(model_name: &str, payload: Vec<u8>) -> Vec<u8> {
    if model_name.to_lowercase().contains("claude") {
        return payload;
    }
    ensure_gemini_boundary_user_content(&payload, "request.contents")
}

/// Leading user turn only (countTokens).
pub(crate) fn ensure_leading_user_content(model_name: &str, payload: Vec<u8>) -> Vec<u8> {
    if model_name.to_lowercase().contains("claude") {
        return payload;
    }
    ensure_gemini_leading_user_content(&payload, "request.contents")
}

#[derive(Clone)]
struct FunctionRef {
    id: String,
    name: String,
}

/// Repairs functionResponse names, orders parallel responses to match their calls and forces
/// `role:"model"` on pure functionResponse turns (the shape Cloud Code expects).
pub(crate) fn normalize_function_response_roles(raw: Vec<u8>) -> Vec<u8> {
    let mut v = cpa_json::parse(&raw);
    let changed = repair_function_response_names(&mut v) | normalize_roles(&mut v);
    if changed { cpa_json::to_vec(&v) } else { raw }
}

fn repair_function_response_names(v: &mut Value) -> bool {
    let contents = v.g("request.contents");
    if !contents.is_array() {
        return false;
    }
    let mut call_id_to_name = std::collections::HashMap::new();
    for content in contents.array() {
        let parts = content.g("parts");
        if !parts.is_array() {
            continue;
        }
        for part in parts.array() {
            let fc = part.g("functionCall");
            if fc.exists() {
                let id = fc.g("id").str().trim().to_string();
                let name = fc.g("name").str().trim().to_string();
                if !id.is_empty() && !name.is_empty() && name != "unknown" {
                    call_id_to_name.insert(id, name);
                }
            }
        }
    }
    if call_id_to_name.is_empty() {
        return false;
    }
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (ci, content) in contents.array().iter().enumerate() {
        let parts = content.g("parts");
        if !parts.is_array() {
            continue;
        }
        for (pi, part) in parts.array().iter().enumerate() {
            let fr = part.g("functionResponse");
            if !fr.exists() {
                continue;
            }
            let id = fr.g("id").str().trim().to_string();
            let name = fr.g("name").str().trim().to_string();
            if id.is_empty() || (!name.is_empty() && name != "unknown") {
                continue;
            }
            if let Some(real) = call_id_to_name.get(&id) {
                edits.push((ci, pi, real.clone()));
            }
        }
    }
    drop(contents);
    let changed = !edits.is_empty();
    for (ci, pi, name) in edits {
        cpa_json::set(v, &format!("request.contents.{ci}.parts.{pi}.functionResponse.name"), name);
    }
    changed
}

fn normalize_roles(v: &mut Value) -> bool {
    let contents = v.g("request.contents");
    if !contents.is_array() {
        return false;
    }
    let mut pending: Vec<FunctionRef> = Vec::new();
    // (content index, new parts, new role)
    let mut edits: Vec<(usize, Option<Vec<Value>>, bool)> = Vec::new();
    for (index, content) in contents.array().iter().enumerate() {
        let parts = content.g("parts");
        if !parts.is_array() {
            pending.clear();
            continue;
        }
        let mut calls = Vec::new();
        let mut responses: Vec<FunctionRef> = Vec::new();
        let mut response_parts: Vec<Value> = Vec::new();
        let mut other_parts: Vec<Value> = Vec::new();
        let mut part_count = 0usize;
        let mut has_other = false;
        for part in parts.array() {
            part_count += 1;
            if part.g("functionCall").exists() {
                calls.push(FunctionRef { id: part.g("functionCall.id").str(), name: part.g("functionCall.name").str() });
            } else if part.g("functionResponse").exists() {
                responses.push(FunctionRef {
                    id: part.g("functionResponse.id").str(),
                    name: part.g("functionResponse.name").str(),
                });
                response_parts.push(part.value());
            } else {
                has_other = true;
                other_parts.push(part.value());
            }
        }
        if part_count == 0 {
            pending.clear();
            continue;
        }
        if !calls.is_empty() && responses.is_empty() {
            pending = calls;
            continue;
        }
        if responses.is_empty() {
            if has_other {
                pending.clear();
            }
            continue;
        }
        if !calls.is_empty() {
            pending.clear();
            continue;
        }

        let mut new_parts: Option<Vec<Value>> = None;
        if !pending.is_empty() {
            let mut ordered: Vec<Value> = Vec::with_capacity(part_count);
            let mut used = vec![false; responses.len()];
            for call in &pending {
                for (ri, response) in responses.iter().enumerate() {
                    if used[ri] {
                        continue;
                    }
                    if (!call.id.is_empty() && response.id == call.id)
                        || (call.id.is_empty() && !call.name.is_empty() && response.name == call.name)
                    {
                        used[ri] = true;
                        ordered.push(response_parts[ri].clone());
                        break;
                    }
                }
            }
            for (ri, part) in response_parts.iter().enumerate() {
                if !used[ri] {
                    ordered.push(part.clone());
                }
            }
            if ordered.len() == response_parts.len() {
                ordered.extend(other_parts.iter().cloned());
                if parts.v() != Some(&Value::Array(ordered.clone())) {
                    new_parts = Some(ordered);
                }
            }
        }
        pending.clear();
        let set_role = !has_other && content.g("role").str() != "model";
        if new_parts.is_some() || set_role {
            edits.push((index, new_parts, set_role));
        }
    }
    drop(contents);
    let changed = !edits.is_empty();
    for (index, parts, set_role) in edits {
        if let Some(parts) = parts {
            cpa_json::set(v, &format!("request.contents.{index}.parts"), Value::Array(parts));
        }
        if set_role {
            cpa_json::set(v, &format!("request.contents.{index}.role"), "model");
        }
    }
    changed
}

fn count_claude_thinking_blocks(raw: &[u8]) -> usize {
    let v = cpa_json::parse(raw);
    let messages = v.g("messages");
    if !messages.is_array() {
        return 0;
    }
    messages
        .array()
        .iter()
        .map(|m| {
            let content = m.g("content");
            if content.is_array() {
                content.array().iter().filter(|p| p.g("type").str() == "thinking").count()
            } else {
                0
            }
        })
        .sum()
}

fn log_signature_strip(before: usize, after: usize, stage: &str, reason: &str) {
    let removed = before.saturating_sub(after);
    if removed == 0 {
        return;
    }
    tracing::debug!(
        component = "signature_sanitizer",
        executor = "antigravity",
        target_provider = "claude",
        action = "drop_thinking_blocks",
        stage,
        reason,
        count = removed,
        "antigravity executor: dropped Claude thinking blocks with invalid signatures"
    );
}

/// Strips Claude thinking blocks the target cannot accept (Go: validateAntigravityRequestSignatures).
pub(crate) fn validate_request_signatures(model_name: &str, from: Format, raw: Vec<u8>) -> Vec<u8> {
    if from != Format::Claude {
        return raw;
    }
    let before = count_claude_thinking_blocks(&raw);
    if uses_reasoning_replay_cache(model_name) {
        let raw = ag_claude::strip_invalid_gemini_signature_thinking_blocks(&raw);
        log_signature_strip(before, count_claude_thinking_blocks(&raw), "provider_cleanup", "empty_or_non_gemini_signature");
        return raw;
    }
    // Claude models accept only Claude-format thinking signatures.
    let raw = ag_claude::strip_empty_signature_thinking_blocks(&raw);
    log_signature_strip(before, count_claude_thinking_blocks(&raw), "prefix_cleanup", "empty_or_non_claude_signature");
    if signature_cache_enabled() || !signature_bypass_strict_mode() {
        return raw;
    }
    let before = count_claude_thinking_blocks(&raw);
    let raw = ag_claude::strip_invalid_bypass_signature_thinking_blocks(&raw);
    log_signature_strip(before, count_claude_thinking_blocks(&raw), "strict_bypass", "invalid_antigravity_claude_signature");
    raw
}
