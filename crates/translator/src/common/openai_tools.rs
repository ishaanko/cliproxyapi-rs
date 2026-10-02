//! OpenAI tool call message ordering (Go: common/openai_tools.go).

use std::collections::{HashMap, HashSet};

use cpa_json::J;

struct AssistantRecord {
    msg_index: usize,
    call_ids: Vec<String>,
    has_invalid_or_empty_id: bool,
}

/// Reorders tool result messages to immediately follow the assistant message that issued their
/// matching `tool_calls`, by `tool_call_id`. Original message order, content parts and numeric
/// precision are preserved, and ambiguous (duplicate/empty ids, `extra_ambiguous_ids`), orphan
/// and incomplete histories are left untouched.
pub fn align_openai_tool_call_messages(messages: &[Vec<u8>], extra_ambiguous_ids: &[&str]) -> Vec<Vec<u8>> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }

    let mut assistants: Vec<AssistantRecord> = Vec::new();
    let mut assistant_by_call_id: HashMap<String, usize> = HashMap::new();
    let mut ambiguous_call_ids: HashSet<String> = extra_ambiguous_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    let mut tool_msg_indices_by_call_id: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, raw) in messages.iter().enumerate() {
        let parsed = cpa_json::parse(raw);
        match parsed.g("role").str().as_str() {
            "assistant" => {
                let tool_calls = parsed.g("tool_calls");
                if !tool_calls.is_array() {
                    continue;
                }
                let raw_calls = tool_calls.array();
                if raw_calls.is_empty() {
                    continue;
                }
                let mut call_ids = Vec::with_capacity(raw_calls.len());
                let mut has_empty_call_id = false;
                for tc in &raw_calls {
                    let call_id = tc.g("id").str();
                    if call_id.is_empty() {
                        // An empty tool_call_id cannot be safely matched.
                        ambiguous_call_ids.insert(String::new());
                        has_empty_call_id = true;
                        continue;
                    }
                    if assistant_by_call_id.contains_key(&call_id) {
                        ambiguous_call_ids.insert(call_id.clone());
                    }
                    assistant_by_call_id.insert(call_id.clone(), i);
                    call_ids.push(call_id);
                }
                if !call_ids.is_empty() || has_empty_call_id {
                    assistants.push(AssistantRecord {
                        msg_index: i,
                        call_ids,
                        has_invalid_or_empty_id: has_empty_call_id,
                    });
                }
            }
            "tool" => {
                let call_id = parsed.g("tool_call_id").str();
                if call_id.is_empty() {
                    ambiguous_call_ids.insert(String::new());
                } else {
                    let indices = tool_msg_indices_by_call_id.entry(call_id.clone()).or_default();
                    indices.push(i);
                    if indices.len() > 1 {
                        ambiguous_call_ids.insert(call_id);
                    }
                }
            }
            _ => {}
        }
    }

    if assistants.is_empty() {
        return messages.to_vec();
    }

    struct ReorderGroup {
        assistant_index: usize,
        tool_indices: Vec<usize>,
    }

    let mut groups: Vec<ReorderGroup> = Vec::new();
    let mut needs_reorder = false;

    for ast in &assistants {
        if ast.has_invalid_or_empty_id {
            continue;
        }
        // Verify completeness and ambiguity for this assistant.
        let mut matched_tool_indices: Vec<usize> = Vec::with_capacity(ast.call_ids.len());
        let mut is_eligible = true;
        for call_id in &ast.call_ids {
            if ambiguous_call_ids.contains(call_id) {
                is_eligible = false;
                break;
            }
            let indices = tool_msg_indices_by_call_id.get(call_id).map(Vec::as_slice).unwrap_or_default();
            // Exactly one tool message must match (else incomplete or orphan).
            let [tool_idx] = indices else {
                is_eligible = false;
                break;
            };
            if *tool_idx <= ast.msg_index {
                // Causal order violation: tool result before assistant.
                is_eligible = false;
                break;
            }
            matched_tool_indices.push(*tool_idx);
        }
        if !is_eligible {
            continue;
        }

        // Keep the tool messages' relative order.
        matched_tool_indices.sort_unstable();

        let already_adjacent = matched_tool_indices
            .iter()
            .enumerate()
            .all(|(offset, &tool_idx)| tool_idx == ast.msg_index + offset + 1);
        if !already_adjacent {
            needs_reorder = true;
            groups.push(ReorderGroup {
                assistant_index: ast.msg_index,
                tool_indices: matched_tool_indices,
            });
        }
    }

    if !needs_reorder {
        return messages.to_vec();
    }

    let mut moved_tool_indices: HashSet<usize> = HashSet::new();
    let mut tools_to_insert: HashMap<usize, Vec<&Vec<u8>>> = HashMap::new();
    for g in &groups {
        let mut tool_list = Vec::with_capacity(g.tool_indices.len());
        for &idx in &g.tool_indices {
            moved_tool_indices.insert(idx);
            tool_list.push(&messages[idx]);
        }
        tools_to_insert.insert(g.assistant_index, tool_list);
    }

    let mut reordered = Vec::with_capacity(messages.len());
    for (i, message) in messages.iter().enumerate() {
        if moved_tool_indices.contains(&i) {
            continue;
        }
        reordered.push(message.clone());
        if let Some(tools) = tools_to_insert.get(&i) {
            reordered.extend(tools.iter().map(|t| (*t).clone()));
        }
    }
    reordered
}
