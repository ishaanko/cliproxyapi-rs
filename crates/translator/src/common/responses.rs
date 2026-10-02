//! OpenAI Responses tool call identity and output pairing (Go: common/responses.go).

use std::collections::HashMap;

use cpa_json::Res;

/// Writes a resolved Responses tool name and namespace on an item (at `item_path`, or the root
/// when empty). An empty namespace removes the field.
pub fn set_responses_tool_call_identity(item: &[u8], name: &str, namespace: &str, item_path: &str) -> Vec<u8> {
    let (name_path, namespace_path) = if item_path.is_empty() {
        ("name".to_string(), "namespace".to_string())
    } else {
        (format!("{item_path}.name"), format!("{item_path}.namespace"))
    };
    let mut root = cpa_json::parse(item);
    cpa_json::set(&mut root, &name_path, name);
    if namespace.is_empty() {
        cpa_json::delete(&mut root, &namespace_path);
    } else {
        cpa_json::set(&mut root, &namespace_path, namespace);
    }
    cpa_json::to_vec(&root)
}

/// The tool call ID of a Responses API item, preferring dedicated tool call references over
/// generic item IDs: `call_id`, `tool_call_id`, `callId`, then `id` (excluding `fco_` output item
/// IDs).
pub fn extract_responses_call_id(node: &Res<'_>) -> String {
    for path in ["call_id", "tool_call_id", "callId"] {
        let id = node.g(path).str().trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    let id = node.g("id").str().trim().to_string();
    if id.starts_with("fco_") {
        return String::new();
    }
    id
}

fn is_output_item(item: &Res<'_>) -> bool {
    matches!(
        item.g("type").str().as_str(),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// Pairs `function_call_output` / `custom_tool_call_output` items with preceding pending tool
/// calls and assigns missing `call_id`s to outputs, in three passes per run of outputs:
/// 1. exact explicit call ID match (reserving calls for outputs that explicitly reference them
///    anywhere in the conversation);
/// 2. function name match for outputs that omit a call ID (skipping calls reserved by later
///    explicit outputs);
/// 3. FIFO fallback for the remaining outputs without a call ID (same reservation rule).
///
/// Outputs carrying an explicit call ID that matches no pending call are never rewritten.
pub fn normalize_responses_tool_call_outputs<'a>(items: &[Res<'a>]) -> Vec<Res<'a>> {
    if items.is_empty() {
        return items.to_vec();
    }

    let mut normalized: Vec<Res<'a>> = items.to_vec();

    let mut explicit_output_counts: HashMap<String, i64> = HashMap::new();
    for item in items {
        if is_output_item(item) {
            let id = extract_responses_call_id(item);
            if !id.is_empty() {
                *explicit_output_counts.entry(id).or_insert(0) += 1;
            }
        }
    }

    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut pending_call_names: HashMap<String, String> = HashMap::new();

    let mut i = 0;
    while i < normalized.len() {
        let item = normalized[i].clone();
        match item.g("type").str().as_str() {
            "function_call" | "custom_tool_call" => {
                let call_id = extract_responses_call_id(&item);
                if !call_id.is_empty() {
                    pending_call_ids.push(call_id.clone());
                    pending_call_names.insert(call_id, item.g("name").str());
                }
                i += 1;
            }
            "function_call_output" | "custom_tool_call_output" => {
                let start = i;
                while i < normalized.len() && is_output_item(&normalized[i]) {
                    i += 1;
                }
                let outputs: Vec<Res<'a>> = normalized[start..i].to_vec();

                if pending_call_ids.is_empty() {
                    continue;
                }
                let mut used = vec![false; outputs.len()];
                let mut matched_for_pending: Vec<Option<usize>> = vec![None; pending_call_ids.len()];

                // Pass 1: exact explicit call ID match.
                for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                    for (out_idx, out) in outputs.iter().enumerate() {
                        if !used[out_idx] && extract_responses_call_id(out) == *pending_id {
                            used[out_idx] = true;
                            matched_for_pending[pending_idx] = Some(out_idx);
                            *explicit_output_counts.entry(pending_id.clone()).or_insert(0) -= 1;
                            break;
                        }
                    }
                }

                // Pass 2: match by function name for outputs without an explicit call ID
                // (skipping pending IDs reserved by later explicit outputs).
                for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                    if matched_for_pending[pending_idx].is_some()
                        || explicit_output_counts.get(pending_id).copied().unwrap_or(0) > 0
                    {
                        continue;
                    }
                    let expected_name = pending_call_names.get(pending_id).cloned().unwrap_or_default();
                    if expected_name.is_empty() {
                        continue;
                    }
                    for (out_idx, out) in outputs.iter().enumerate() {
                        if !used[out_idx] && extract_responses_call_id(out).is_empty() {
                            let out_name = out.g("name").str().trim().to_string();
                            if !out_name.is_empty() && out_name == expected_name {
                                used[out_idx] = true;
                                matched_for_pending[pending_idx] = Some(out_idx);
                                break;
                            }
                        }
                    }
                }

                // Pass 3: FIFO fallback for outputs without an explicit call ID.
                for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                    if matched_for_pending[pending_idx].is_some()
                        || explicit_output_counts.get(pending_id).copied().unwrap_or(0) > 0
                    {
                        continue;
                    }
                    let expected_name = pending_call_names.get(pending_id).cloned().unwrap_or_default();
                    for (out_idx, out) in outputs.iter().enumerate() {
                        if !used[out_idx] && extract_responses_call_id(out).is_empty() {
                            let out_name = out.g("name").str().trim().to_string();
                            if out_name.is_empty() || expected_name.is_empty() || out_name == expected_name {
                                used[out_idx] = true;
                                matched_for_pending[pending_idx] = Some(out_idx);
                                break;
                            }
                        }
                    }
                }

                // Apply matched call_ids to outputs.
                let mut remaining_pending: Vec<String> = Vec::new();
                for (pending_idx, pending_id) in pending_call_ids.iter().enumerate() {
                    let Some(out_idx) = matched_for_pending[pending_idx] else {
                        remaining_pending.push(pending_id.clone());
                        continue;
                    };
                    let matched_out = &outputs[out_idx];
                    if matched_out.g("call_id").str() != *pending_id {
                        let mut raw = matched_out.value();
                        cpa_json::set(&mut raw, "call_id", pending_id.as_str());
                        normalized[start + out_idx] = Res::owned(raw);
                    }
                }
                pending_call_ids = remaining_pending;
            }
            _ => i += 1,
        }
    }

    normalized
}
