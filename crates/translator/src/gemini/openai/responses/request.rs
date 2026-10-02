//! OpenAI Responses request -> Gemini request (Go: gemini_openai-responses_request.go).
//!
//! The output is a top-level Gemini body (`contents` at the root, no `model` key). Input items
//! are `Res` values (gjson results); each is turned into Gemini contents following the Go state
//! machine: pending function calls, deferred developer messages, reasoning/signature carriers.

use crate::common::text_part;
use std::collections::{HashMap, HashSet};

use cpa_core::signature::{
    compatible_signature_for_provider_block, gemini_replay_signature_or_bypass, sanitize_gemini_request_thought_signatures,
    signature_provider_from_model_name, SignatureBlockKind, SignatureProvider,
};
use cpa_core::util::{
    build_gemini_function_declarations, convert_responses_tool_choice_to_gemini, map_responses_tool_name,
    qualify_responses_namespace_tool_name, sanitize_function_name,
};
use cpa_json::{json, Res, Value, J};

use super::media::{open_ai_responses_part_from_block, parse_open_ai_responses_array_output, open_ai_responses_media_from_block, gemini_responses_inline_data_part};
use super::signature_carrier::{
    gemini_responses_carrier_direction, gemini_responses_carrier_target, is_openai_responses_detached_carrier,
    normalize_gemini_responses_carriers, CARRIER_ANY, CARRIER_FUNCTION, CARRIER_NEXT, CARRIER_PREVIOUS, CARRIER_SIGNATURE_FIELD,
    CARRIER_SUMMARY_FIELD, CARRIER_TEXT,
};
use super::lenient::{collect_output_raws, parse_gjson, RawTexts};
use super::trailing_signature::restore_gemini_responses_text_signatures;
use super::web_search::{
    allows_responses_web_search_tool_choice, extract_responses_web_search_allowed_domains, has_responses_web_search_tool,
    model_supports_web_search,
};
use crate::common::{
    extract_responses_call_id, merge_adjacent_gemini_user_contents, normalize_responses_tool_call_outputs,
    contains_json_ref, system_reminder_text,
};
use crate::gemini::common::attach_default_safety_settings;

pub(super) const GEMINI_RESPONSES_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Converts an OpenAI Responses request body into a Gemini `generateContent` body. Also used by
/// the antigravity translator, which wraps the result in its envelope.
pub fn convert_openai_responses_request_to_gemini(model_name: &str, input_raw_json: &[u8], _stream: bool) -> Vec<u8> {
    let mut use_native_layout = signature_provider_from_model_name(model_name) == SignatureProvider::Gemini;

    // Base Gemini template; thinkingConfig is only added when requested.
    let mut out = json!({"contents": []});
    let root = cpa_json::parse(input_raw_json);
    let raw_outputs = collect_output_raws(input_raw_json, &root);

    // Tools and the forward map are computed first so contents and declarations agree on names.
    let (function_declarations, forward_map, _) = build_gemini_function_declarations(&root);
    let mut tool_blocks: Vec<Value> = Vec::new();
    if has_responses_web_search_tool(&root) && model_supports_web_search(model_name) && allows_responses_web_search_tool_choice(&root) {
        let mut google_search = json!({"googleSearch": {}});
        let allowed_domains = extract_responses_web_search_allowed_domains(&root);
        if !allowed_domains.is_empty() {
            cpa_json::set(&mut google_search, "googleSearch.includedDomains", allowed_domains);
        }
        tool_blocks.push(google_search);
    }
    if !function_declarations.is_empty() {
        tool_blocks.push(json!({"functionDeclarations": function_declarations}));
    }
    if !tool_blocks.is_empty() {
        cpa_json::set(&mut out, "tools", Value::Array(tool_blocks));
    }

    // tool_choice only applies when function declarations exist.
    if !function_declarations.is_empty() {
        let tool_choice = root.g("tool_choice");
        if let Some(tc) = tool_choice.v()
            && let Some(tool_config) = convert_responses_tool_choice_to_gemini(Some(tc), &forward_map) {
                cpa_json::set(&mut out, "toolConfig.functionCallingConfig", tool_config);
            }
    }

    // System instruction from "instructions".
    let mut system_parts: Vec<Value> = Vec::with_capacity(2);
    let instructions = root.g("instructions");
    if instructions.exists() {
        system_parts.push(text_part(&instructions.str()));
    }

    // Input messages to Gemini contents.
    let input = root.g("input");
    if input.is_array() {
        let restored = restore_gemini_responses_text_signatures(model_name, input.array());
        let (input_items, has_gemini_carrier) = normalize_gemini_responses_carriers(&restored);
        if has_gemini_carrier {
            use_native_layout = true;
        }
        let input_items = normalize_responses_tool_call_outputs(&input_items);
        let items = pair_openai_responses_reasoning_with_function_calls(&input_items);
        let mut content_items: Vec<Value> = Vec::with_capacity(items.len());
        let mut function_names_by_call_id: HashMap<String, String> = HashMap::new();
        let mut pending_function_call_ids: Vec<String> = Vec::new();
        for item in &items {
            let item_type = item.g("type").str();
            if item_type == "function_call" || item_type == "custom_tool_call" {
                let call_id = extract_responses_call_id(item);
                function_names_by_call_id.entry(call_id).or_insert_with(|| {
                    let mut name = item.g("name").str();
                    let ns = item.g("namespace").str();
                    if !ns.is_empty() {
                        name = qualify_responses_namespace_tool_name(&ns, &name);
                    }
                    map_responses_tool_name(&forward_map, &name)
                });
            }
        }

        let normalized = if use_native_layout { reorder_openai_responses_detached_reasoning(&items) } else { items };
        let mut consumed_function_output_indexes: HashSet<usize> = HashSet::new();
        let mut has_encountered_conversation = false;
        let mut pending_developer_parts: Vec<Value> = Vec::new();
        // Go's `for i := 0; ...; i++` post statement also runs on `continue`.
        let mut i = usize::MAX;
        loop {
            i = i.wrapping_add(1);
            if i >= normalized.len() {
                break;
            }
            if consumed_function_output_indexes.contains(&i) {
                continue;
            }
            let item = normalized[i].clone();
            let mut item_type = item.g("type").str();
            let mut item_role = item.g("role").str();
            if item_type.is_empty() && !item_role.is_empty() {
                item_type = "message".to_string();
            } else if is_responses_content_part_type(&item_type) && item_role.is_empty() {
                item_type = "message".to_string();
                item_role = "user".to_string();
            }

            match item_type.as_str() {
                "message" => {
                    if item_role.eq_ignore_ascii_case("system") || item_role.eq_ignore_ascii_case("developer") {
                        let content = item.g("content");
                        if !has_encountered_conversation {
                            pending_function_call_ids.clear();
                            if content.exists() {
                                if content.is_array() {
                                    for content_item in content.array() {
                                        system_parts.push(text_part(&content_item.g("text").str()));
                                    }
                                } else if content.is_string() {
                                    system_parts.push(text_part(&content.str()));
                                }
                            }
                            continue;
                        }

                        let mut dev_parts: Vec<Value> = Vec::new();
                        if content.exists() {
                            if content.is_array() {
                                let mut texts: Vec<String> = Vec::new();
                                for content_item in content.array() {
                                    let mut text = content_item.g("text").str();
                                    if text.is_empty() && content_item.is_string() {
                                        text = content_item.str();
                                    }
                                    if !text.is_empty() {
                                        texts.push(text);
                                    }
                                }
                                if !texts.is_empty() {
                                    let joined = texts.join("\n");
                                    if !joined.trim().is_empty() {
                                        dev_parts.push(text_part(&system_reminder_text(&joined)));
                                    }
                                }
                            } else if content.is_string() && !content.str().is_empty() {
                                let text = content.str();
                                if !text.trim().is_empty() {
                                    dev_parts.push(text_part(&system_reminder_text(&text)));
                                }
                            }
                        }
                        if !dev_parts.is_empty() {
                            if !pending_function_call_ids.is_empty() {
                                pending_developer_parts.extend(dev_parts);
                            } else {
                                content_items.push(gemini_content("user", dev_parts));
                            }
                        }
                        continue;
                    }

                    has_encountered_conversation = true;
                    if openai_responses_assistant_visible_text(&item).is_none() {
                        if !pending_function_call_ids.is_empty() {
                            let any_has_future_output = pending_function_call_ids.iter().any(|call_id| responses_has_matching_output(&normalized[i..], call_id));
                            if !any_has_future_output {
                                let synthesized: Vec<Value> = pending_function_call_ids
                                    .iter()
                                    .map(|call_id| build_openai_responses_synthesized_function_response_part(call_id, &function_names_by_call_id))
                                    .collect();
                                if !synthesized.is_empty() {
                                    content_items.push(gemini_content("user", synthesized));
                                }
                                pending_function_call_ids.clear();
                            }
                        }
                        if !pending_developer_parts.is_empty() {
                            content_items.push(gemini_content("user", std::mem::take(&mut pending_developer_parts)));
                        }
                    }

                    // Model outputs may appear as content items with type "output_text" even when
                    // message.role is "user"; those are split into distinct Gemini messages with
                    // roles derived from the content type.
                    let content_array = item.g("content");
                    let mut parts_to_process: Vec<Res<'_>> = Vec::new();
                    if content_array.exists() && content_array.is_array() {
                        parts_to_process = content_array.array().into_iter().map(|r| Res::owned(r.value())).collect();
                    } else if is_responses_content_part_type(&item.g("type").str()) {
                        parts_to_process.push(item.clone());
                        while i + 1 < normalized.len() {
                            let next_item = &normalized[i + 1];
                            let next_type = next_item.g("type").str();
                            let next_role = next_item.g("role").str();
                            if next_role.is_empty() && is_responses_content_part_type(&next_type) {
                                parts_to_process.push(next_item.clone());
                                i += 1;
                            } else {
                                break;
                            }
                        }
                    }

                    if !parts_to_process.is_empty() {
                        let mut current_role = String::new();
                        let mut current_parts: Vec<Value> = Vec::new();
                        let flush = |content_items: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>| {
                            if !role.is_empty() && !parts.is_empty() {
                                content_items.push(gemini_content(role, std::mem::take(parts)));
                            }
                            parts.clear();
                        };

                        for content_item in &parts_to_process {
                            let mut content_type = content_item.g("type").str();
                            if content_type.is_empty() {
                                content_type = "input_text".to_string();
                            }

                            let mut eff_role = "user".to_string();
                            if !item_role.is_empty() {
                                eff_role = match item_role.to_lowercase().as_str() {
                                    "assistant" | "model" => "model".to_string(),
                                    other => other.to_string(),
                                };
                            }
                            if content_type == "output_text" {
                                eff_role = "model".to_string();
                            }
                            if eff_role == "assistant" {
                                eff_role = "model".to_string();
                            }

                            if !current_role.is_empty() && eff_role != current_role {
                                flush(&mut content_items, &current_role, &mut current_parts);
                                current_role.clear();
                            }
                            if current_role.is_empty() {
                                current_role = eff_role;
                            }

                            let part_json: Option<Value> = match content_type.as_str() {
                                "input_text" | "output_text" | "text" => {
                                    let text = content_item.g("text");
                                    text.exists().then(|| text_part(&text.str()))
                                }
                                _ => open_ai_responses_part_from_block(content_item),
                            };
                            if let Some(part) = part_json {
                                current_parts.push(part);
                            }
                        }
                        flush(&mut content_items, &current_role, &mut current_parts);
                    } else if content_array.is_string() {
                        let mut eff_role = "user".to_string();
                        if !item_role.is_empty() {
                            eff_role = match item_role.to_lowercase().as_str() {
                                "assistant" | "model" => "model".to_string(),
                                other => other.to_string(),
                            };
                        }
                        content_items.push(gemini_content(&eff_role, vec![text_part(&content_array.str())]));
                    }
                }

                "function_call" | "custom_tool_call" => {
                    has_encountered_conversation = true;
                    let mut signature = GEMINI_RESPONSES_THOUGHT_SIGNATURE.to_string();
                    let raw_signature = item.g(CARRIER_SIGNATURE_FIELD).str().trim().to_string();
                    if !raw_signature.is_empty() {
                        signature = gemini_replay_signature_or_bypass(&raw_signature, SignatureBlockKind::GeminiFunctionCall);
                    }
                    let thought_text = item.g(CARRIER_SUMMARY_FIELD).str();
                    if !thought_text.is_empty() {
                        content_items.push(build_openai_responses_reasoning_function_call_model_content(&thought_text, &item, &signature, &forward_map));
                    } else if !use_native_layout && !item.g(CARRIER_SIGNATURE_FIELD).str().trim().is_empty() {
                        content_items.push(build_openai_responses_empty_reasoning_function_call_model_content(&item, &signature, &forward_map));
                    } else {
                        content_items.push(build_openai_responses_function_call_model_content(&item, &signature, &forward_map));
                    }
                    let call_id = extract_responses_call_id(&item);
                    if !call_id.is_empty() {
                        pending_function_call_ids.push(call_id);
                    }
                }

                "function_call_output" | "custom_tool_call_output" => {
                    has_encountered_conversation = true;
                    let (ordered_outputs, consumed_end) = collect_openai_responses_function_call_outputs(&normalized, i, &pending_function_call_ids);
                    for consumed_index in i..consumed_end {
                        consumed_function_output_indexes.insert(consumed_index);
                    }
                    let end = consumed_end;
                    let has_subsequent = responses_has_subsequent_turn(&normalized[end..]);

                    let mut output_by_call_id: HashMap<String, Res<'_>> = HashMap::new();
                    let mut extra_outputs: Vec<Res<'_>> = Vec::new();
                    let mut any_matched = false;
                    for out in &ordered_outputs {
                        let id = extract_responses_call_id(out);
                        if !id.is_empty() {
                            output_by_call_id.insert(id, out.clone());
                        } else {
                            extra_outputs.push(out.clone());
                        }
                    }
                    for pending_id in &pending_function_call_ids {
                        if output_by_call_id.contains_key(pending_id) {
                            any_matched = true;
                            break;
                        }
                    }

                    let mut response_parts: Vec<Value> = Vec::with_capacity(pending_function_call_ids.len() + extra_outputs.len());
                    let mut still_pending: Vec<String> = Vec::with_capacity(pending_function_call_ids.len());

                    for pending_id in &pending_function_call_ids {
                        if let Some(out) = output_by_call_id.remove(pending_id) {
                            response_parts.extend(build_openai_responses_function_response_parts(&out, &function_names_by_call_id, &raw_outputs));
                        } else if (has_subsequent || any_matched) && !responses_has_matching_output(&normalized[end..], pending_id) {
                            response_parts.push(build_openai_responses_synthesized_function_response_part(pending_id, &function_names_by_call_id));
                        } else {
                            still_pending.push(pending_id.clone());
                        }
                    }

                    // Orphan outputs (no matching function_call) must not become unpaired
                    // functionResponse parts; they surface as user text instead.
                    let mut standalone_output_contents: Vec<Value> = Vec::new();
                    let mut append_standalone_output = |out: &Res<'_>| {
                        let parts = build_openai_responses_standalone_tool_output_text_parts(out);
                        if !parts.is_empty() {
                            standalone_output_contents.push(gemini_content("user", parts));
                        }
                    };
                    for out in &ordered_outputs {
                        let id = extract_responses_call_id(out);
                        if output_by_call_id.remove(&id).is_some() {
                            append_standalone_output(out);
                        }
                    }
                    for out in &extra_outputs {
                        append_standalone_output(out);
                    }

                    pending_function_call_ids = still_pending;
                    if !response_parts.is_empty() {
                        content_items.push(gemini_content("user", response_parts));
                    }
                    content_items.extend(standalone_output_contents);
                    if pending_function_call_ids.is_empty() && !pending_developer_parts.is_empty() {
                        content_items.push(gemini_content("user", std::mem::take(&mut pending_developer_parts)));
                    }
                }

                "reasoning" => {
                    has_encountered_conversation = true;
                    let thought_text = item.g("summary.0.text").str();
                    let mut raw_signature = item.g("encrypted_content").str();
                    let carrier_direction = gemini_responses_carrier_direction(&item);
                    let carrier_target = gemini_responses_carrier_target(&item);
                    if raw_signature.trim().is_empty() && i + 1 < normalized.len() {
                        let next_reasoning = &normalized[i + 1];
                        if next_reasoning.g("type").str() == "reasoning"
                            && next_reasoning.g("id").str().contains("_detached_after_")
                            && next_reasoning.g("summary.0.text").str().trim().is_empty()
                            && !next_reasoning.g("encrypted_content").str().trim().is_empty()
                        {
                            raw_signature = next_reasoning.g("encrypted_content").str();
                            i += 1;
                        }
                    }
                    let mut signature = String::new();
                    if !raw_signature.trim().is_empty() {
                        signature = open_ai_responses_gemini_thought_signature(&raw_signature);
                    }

                    let mut visible_text = String::new();
                    if use_native_layout && i + 1 < normalized.len() {
                        let next = normalized[i + 1].clone();
                        let can_bind_text = (carrier_direction.is_empty() || carrier_direction == CARRIER_NEXT)
                            && (carrier_target.is_empty() || carrier_target == CARRIER_TEXT || carrier_target == CARRIER_ANY);
                        let can_bind_function = (carrier_direction.is_empty() || carrier_direction == CARRIER_NEXT)
                            && (carrier_target.is_empty() || carrier_target == CARRIER_FUNCTION || carrier_target == CARRIER_ANY);
                        let next_visible = openai_responses_assistant_visible_text(&next);
                        let next_type = next.g("type").str();
                        if let (Some(visible), true) = (next_visible, can_bind_text) {
                            visible_text = visible;
                            i += 1;
                        } else if (next_type == "function_call" || next_type == "custom_tool_call")
                            && can_bind_function
                            && next.g(CARRIER_SIGNATURE_FIELD).str().trim().is_empty()
                        {
                            let func_sig = if signature.is_empty() { GEMINI_RESPONSES_THOUGHT_SIGNATURE.to_string() } else { signature.clone() };
                            content_items.push(build_openai_responses_reasoning_function_call_model_content(&thought_text, &next, &func_sig, &forward_map));
                            let call_id = extract_responses_call_id(&next);
                            if !call_id.is_empty() {
                                pending_function_call_ids.push(call_id);
                            }
                            i += 1;
                            continue;
                        }
                    }

                    if let Some(model_content) = build_openai_responses_reasoning_model_content(&thought_text, &visible_text, &signature, use_native_layout) {
                        content_items.push(model_content);
                    }
                }
                _ => {}
            }
        }
        if !pending_developer_parts.is_empty() {
            content_items.push(gemini_content("user", std::mem::take(&mut pending_developer_parts)));
        }
        let content_items = coalesce_adjacent_openai_responses_model_contents(content_items);
        let content_bytes: Vec<Vec<u8>> = content_items.iter().map(cpa_json::to_vec).collect();
        let merged = merge_adjacent_gemini_user_contents(&content_bytes);
        cpa_json::set(&mut out, "contents", Value::Array(merged.iter().map(|c| cpa_json::parse(c)).collect()));
    } else if input.exists() && input.is_string() {
        // Simple string input becomes a single user message.
        cpa_json::set(&mut out, "contents", Value::Array(vec![gemini_content("user", vec![text_part(&input.str())])]));
    }
    if !system_parts.is_empty() {
        cpa_json::set(&mut out, "systemInstruction", json!({"parts": system_parts}));
    }

    // Generation config.
    let max_output_tokens = root.g("max_output_tokens");
    if max_output_tokens.exists() {
        // Replaces any generationConfig built so far.
        cpa_json::set(&mut out, "generationConfig", json!({"maxOutputTokens": max_output_tokens.int()}));
    }
    let temperature = root.g("temperature");
    if temperature.exists() {
        cpa_json::set(&mut out, "generationConfig.temperature", cpa_json::num_f64(temperature.float()));
    }
    let top_p = root.g("top_p");
    if top_p.exists() {
        cpa_json::set(&mut out, "generationConfig.topP", cpa_json::num_f64(top_p.float()));
    }
    let stop_sequences = root.g("stop_sequences");
    if stop_sequences.exists() && stop_sequences.is_array() {
        let sequences: Vec<String> = stop_sequences.array().iter().map(|s| s.str()).collect();
        // Go marshals a nil slice (empty input array) as null.
        let value = if sequences.is_empty() { Value::Null } else { Value::from(sequences) };
        cpa_json::set(&mut out, "generationConfig.stopSequences", value);
    }

    apply_openai_responses_text_format_to_gemini(&mut out, &root);

    // reasoning.effort -> thinkingConfig (translation only; capability checks happen later).
    let re = root.g("reasoning.effort");
    if re.exists() {
        let effort = re.str().trim().to_lowercase();
        if !effort.is_empty() {
            let thinking_path = "generationConfig.thinkingConfig";
            if effort == "auto" {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingBudget"), -1);
            } else {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingLevel"), effort);
            }
        }
    }

    let mut result = attach_default_safety_settings(&cpa_json::to_vec(&out), "safetySettings");
    if use_native_layout {
        result = sanitize_gemini_request_thought_signatures(&result, "contents");
    }
    strip_trailing_openai_responses_model_prefill(&result)
}

fn gemini_content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

/// Merges consecutive model contents into one (parts concatenated).
fn coalesce_adjacent_openai_responses_model_contents(contents: Vec<Value>) -> Vec<Value> {
    let is_model = |c: &Value| c.g("role").str().trim().eq_ignore_ascii_case("model");
    let mut coalesced: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        if !is_model(&content) || coalesced.is_empty() {
            coalesced.push(content);
            continue;
        }
        let last_index = coalesced.len() - 1;
        if !is_model(&coalesced[last_index]) {
            coalesced.push(content);
            continue;
        }
        let Some(extra_parts) = content.get("parts").and_then(Value::as_array).cloned() else {
            coalesced.push(content);
            continue;
        };
        if !extra_parts.is_empty() {
            let mut existing_parts: Vec<Value> = coalesced[last_index].g("parts").array().iter().map(|p| p.value()).collect();
            existing_parts.extend(extra_parts);
            cpa_json::set(&mut coalesced[last_index], "parts", Value::Array(existing_parts));
        }
    }
    coalesced
}

/// Drops a trailing assistant prefill (last model content without thought, functionCall or
/// signature), which Gemini rejects as the final turn.
fn strip_trailing_openai_responses_model_prefill(payload: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let contents = root.g("contents");
    if !contents.is_array() {
        return payload.to_vec();
    }
    let content_array = contents.array();
    let Some(last) = content_array.last() else { return payload.to_vec() };
    if !should_strip_trailing_openai_responses_model_prefill(last) {
        return payload.to_vec();
    }
    let items: Vec<Value> = content_array[..content_array.len() - 1].iter().map(|c| c.value()).collect();
    cpa_json::set(&mut root, "contents", Value::Array(items));
    cpa_json::to_vec(&root)
}

fn should_strip_trailing_openai_responses_model_prefill(last_content: &Res<'_>) -> bool {
    if last_content.g("role").str() != "model" {
        return false;
    }
    let parts = last_content.g("parts");
    if !parts.is_array() {
        return false;
    }
    !parts.array().iter().any(|part| {
        part.g("thought").bool() || part.g("functionCall").exists() || !part.g("thoughtSignature").str().trim().is_empty()
    })
}

/// The visible text of an assistant message item, joined with newlines. `output_text` parts mark
/// model content even when `role` is "user". `None` for anything else.
pub(super) fn openai_responses_assistant_visible_text(item: &Res<'_>) -> Option<String> {
    let mut item_type = item.g("type").str();
    let item_role = item.g("role").str();
    if item_type.is_empty() && !item_role.is_empty() {
        item_type = "message".to_string();
    }
    if item_type != "message" {
        return None;
    }
    let content = item.g("content");
    if !content.exists() {
        return None;
    }
    if content.is_string() {
        return match item_role.trim().to_lowercase().as_str() {
            "assistant" | "model" => Some(content.str()),
            _ => None,
        };
    }
    if !content.is_array() {
        return None;
    }
    let mut text_parts: Vec<String> = Vec::new();
    let mut has_output_text = false;
    for content_item in content.array() {
        let mut content_type = content_item.g("type").str();
        if content_type.is_empty() {
            content_type = "input_text".to_string();
        }
        if content_type != "output_text" {
            continue;
        }
        has_output_text = true;
        text_parts.push(content_item.g("text").str());
    }
    if !has_output_text {
        return None;
    }
    Some(text_parts.join("\n"))
}

fn is_openai_responses_tool_call(item: &Res<'_>) -> bool {
    matches!(item.g("type").str().as_str(), "function_call" | "custom_tool_call")
}

fn is_openai_responses_tool_output(item: &Res<'_>) -> bool {
    matches!(item.g("type").str().as_str(), "function_call_output" | "custom_tool_call_output")
}

/// Binds detached signature carriers to adjacent function calls (as `_cpa_reasoning_signature`
/// and `_cpa_reasoning_summary` fields) so each call replays with its own signature.
fn pair_openai_responses_reasoning_with_function_calls<'a>(items: &[Res<'a>]) -> Vec<Res<'a>> {
    let mut post_call_signature: HashMap<usize, String> = HashMap::new();
    let mut post_call_carrier: HashSet<usize> = HashSet::new();
    let mut consumed_post_call_carrier: HashSet<usize> = HashSet::new();
    let mut group_start = 0;
    while group_start < items.len() {
        if !is_openai_responses_tool_call(&items[group_start]) && !is_openai_responses_detached_carrier(&items[group_start]) {
            group_start += 1;
            continue;
        }
        let mut group_end = group_start;
        let mut has_function_call = false;
        while group_end < items.len() && (is_openai_responses_tool_call(&items[group_end]) || is_openai_responses_detached_carrier(&items[group_end])) {
            has_function_call = has_function_call || is_openai_responses_tool_call(&items[group_end]);
            group_end += 1;
        }
        if !has_function_call || group_end >= items.len() || !is_openai_responses_tool_output(&items[group_end]) {
            group_start = group_end;
            continue;
        }
        let mut output_end = group_end;
        while output_end < items.len() && is_openai_responses_tool_output(&items[output_end]) {
            output_end += 1;
        }
        // A run beginning with a carrier uses leading-carrier semantics. A run beginning with a
        // call uses post-call semantics. This preserves both carrier,call,carrier,call and
        // call,carrier,call,carrier histories.
        if is_openai_responses_tool_call(&items[group_start]) {
            for call_index in group_start..group_end {
                let item = &items[call_index];
                if !is_openai_responses_tool_call(item)
                    || !item.g(CARRIER_SIGNATURE_FIELD).str().trim().is_empty()
                    || call_index + 1 >= group_end
                    || !is_openai_responses_detached_carrier(&items[call_index + 1])
                {
                    continue;
                }
                let carrier_direction = gemini_responses_carrier_direction(&items[call_index + 1]);
                let carrier_target = gemini_responses_carrier_target(&items[call_index + 1]);
                if !carrier_direction.is_empty() && (carrier_direction != CARRIER_PREVIOUS || (carrier_target != CARRIER_FUNCTION && carrier_target != CARRIER_ANY)) {
                    continue;
                }
                let mut carrier_end = call_index + 1;
                while carrier_end < group_end && is_openai_responses_detached_carrier(&items[carrier_end]) {
                    post_call_carrier.insert(carrier_end);
                    carrier_end += 1;
                }
                let call_id = extract_responses_call_id(item);
                if call_id.is_empty() {
                    continue;
                }
                for output_index in group_end..output_end {
                    if extract_responses_call_id(&items[output_index]) == call_id {
                        post_call_signature.insert(call_index, items[call_index + 1].g("encrypted_content").str().trim().to_string());
                        consumed_post_call_carrier.insert(call_index + 1);
                        break;
                    }
                }
            }
        }
        group_start = output_end;
    }

    let mut paired: Vec<Res<'a>> = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        let item = &items[index];
        if let Some(signature) = post_call_signature.get(&index).filter(|s| !s.is_empty()) {
            let mut function_call = item.value();
            cpa_json::set(&mut function_call, CARRIER_SIGNATURE_FIELD, signature.as_str());
            paired.push(Res::owned(function_call));
            index += 1;
            continue;
        }
        if consumed_post_call_carrier.contains(&index) {
            index += 1;
            continue;
        }
        let carrier_direction = gemini_responses_carrier_direction(item);
        let carrier_target = gemini_responses_carrier_target(item);
        let can_bind_following_call = carrier_direction.is_empty()
            || (carrier_direction == CARRIER_NEXT && (carrier_target == CARRIER_FUNCTION || carrier_target == CARRIER_ANY));
        if item.g("type").str() == "reasoning"
            && !post_call_carrier.contains(&index)
            && can_bind_following_call
            && !item.g("id").str().contains("_detached_after_")
            && index + 1 < items.len()
            && is_openai_responses_tool_call(&items[index + 1])
        {
            let raw_signature = item.g("encrypted_content").str().trim().to_string();
            if !raw_signature.is_empty() {
                let mut function_call = items[index + 1].value();
                cpa_json::set(&mut function_call, CARRIER_SIGNATURE_FIELD, raw_signature);
                let summary = item.g("summary.0.text").str();
                if !summary.is_empty() {
                    cpa_json::set(&mut function_call, CARRIER_SUMMARY_FIELD, summary);
                }
                paired.push(Res::owned(function_call));
                index += 2;
                continue;
            }
        }
        paired.push(item.clone());
        index += 1;
    }
    paired
}

/// Gemini native layout: moves detached carriers so each sits before the item it binds to.
fn reorder_openai_responses_detached_reasoning<'a>(items: &[Res<'a>]) -> Vec<Res<'a>> {
    use super::signature_carrier::CARRIER_DIRECTION_FIELD;
    let mut reordered: Vec<Res<'a>> = Vec::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        let is_reasoning_carrier = is_openai_responses_detached_carrier(item);
        let marked_detached = item.g("id").str().contains("_detached_after_");
        if is_reasoning_carrier && !reordered.is_empty() {
            let previous = reordered[reordered.len() - 1].clone();
            let mut previous_type = previous.g("type").str();
            if previous_type.is_empty() && !previous.g("role").str().is_empty() {
                previous_type = "message".to_string();
            }
            let mut is_assistant_message = false;
            if previous_type == "message" {
                is_assistant_message = openai_responses_assistant_visible_text(&previous).is_some();
            }

            let direction = gemini_responses_carrier_direction(item);
            let target_kind = gemini_responses_carrier_target(item);
            if !direction.is_empty() {
                let mut already_paired_text = false;
                let mut already_paired_function = false;
                if reordered.len() > 1 {
                    let prior = &reordered[reordered.len() - 2];
                    let prior_direction = gemini_responses_carrier_direction(prior);
                    let prior_target = gemini_responses_carrier_target(prior);
                    let prior_binds_following =
                        is_openai_responses_detached_carrier(prior) && (prior_direction == CARRIER_NEXT || prior_direction == CARRIER_PREVIOUS);
                    already_paired_text = prior_binds_following && (prior_target == CARRIER_TEXT || prior_target == CARRIER_ANY);
                    already_paired_function = prior_binds_following && (prior_target == CARRIER_FUNCTION || prior_target == CARRIER_ANY);
                }
                let bind_previous_message = direction == CARRIER_PREVIOUS
                    && (target_kind == CARRIER_TEXT || target_kind == CARRIER_ANY)
                    && is_assistant_message
                    && !already_paired_text;
                let bind_previous_function = direction == CARRIER_PREVIOUS
                    && (target_kind == CARRIER_FUNCTION || target_kind == CARRIER_ANY)
                    && (previous_type == "function_call" || previous_type == "custom_tool_call")
                    && previous.g(CARRIER_SIGNATURE_FIELD).str().trim().is_empty()
                    && !already_paired_function;
                if bind_previous_message || bind_previous_function {
                    let mut moved = item.value();
                    cpa_json::set(&mut moved, CARRIER_DIRECTION_FIELD, CARRIER_NEXT);
                    let last = reordered.len() - 1;
                    reordered[last] = Res::owned(moved);
                    reordered.push(previous);
                    continue;
                }
                reordered.push(item.clone());
                continue;
            }

            if is_assistant_message && !marked_detached && item_index + 1 < items.len() {
                let next_is_assistant_message = openai_responses_assistant_visible_text(&items[item_index + 1]).is_some();
                is_assistant_message = !next_is_assistant_message;
            }
            let mut already_paired = false;
            if reordered.len() > 1 {
                let prior = &reordered[reordered.len() - 2];
                already_paired = is_openai_responses_detached_carrier(prior) && prior.g("id").str().contains("_detached_after_");
            }
            if !already_paired
                && (is_assistant_message
                    || (marked_detached
                        && (previous_type == "function_call" || previous_type == "custom_tool_call")
                        && previous.g(CARRIER_SIGNATURE_FIELD).str().trim().is_empty()))
            {
                let last = reordered.len() - 1;
                reordered[last] = item.clone();
                reordered.push(previous);
                continue;
            }
        }
        reordered.push(item.clone());
    }
    reordered
}

fn build_openai_responses_function_call_part(item: &Res<'_>, signature: &str, forward_map: &HashMap<String, String>) -> Value {
    let mut name = item.g("name").str();
    let ns = item.g("namespace").str();
    if !ns.is_empty() {
        name = qualify_responses_namespace_tool_name(&ns, &name);
    }
    let name = map_responses_tool_name(forward_map, &name);
    let mut function_call = json!({"functionCall": {"name": "", "args": {}}});
    cpa_json::set(&mut function_call, "functionCall.name", name);
    cpa_json::set(&mut function_call, "thoughtSignature", signature);
    cpa_json::set(&mut function_call, "functionCall.id", extract_responses_call_id(item));

    if item.g("type").str() == "custom_tool_call" {
        let input_val = item.g("input");
        match input_val.v() {
            Some(Value::String(s)) => {
                cpa_json::set(&mut function_call, "functionCall.args.input", s.as_str());
            }
            Some(v) => {
                cpa_json::set(&mut function_call, "functionCall.args.input", v.clone());
            }
            None => {
                cpa_json::set(&mut function_call, "functionCall.args.input", "");
            }
        }
    } else {
        let arguments = item.g("arguments").str();
        if !arguments.is_empty() {
            let args_result = cpa_json::parse_str(&arguments);
            if args_result.is_object() || args_result.is_array() {
                cpa_json::set(&mut function_call, "functionCall.args", args_result);
            } else {
                cpa_json::set(&mut function_call, "functionCall.args.arguments", arguments);
            }
        }
    }
    function_call
}

fn synthesized_response_name(call_id: &str, names: &HashMap<String, String>) -> String {
    match names.get(call_id) {
        Some(matched) if !matched.is_empty() => matched.clone(),
        _ => "unknown".to_string(),
    }
}

fn build_openai_responses_synthesized_function_response_part(call_id: &str, function_names_by_call_id: &HashMap<String, String>) -> Value {
    let function_name = synthesized_response_name(call_id, function_names_by_call_id);
    let mut function_response = json!({"functionResponse": {"name": "", "response": {"result": "call interrupted, no output"}}});
    cpa_json::set(&mut function_response, "functionResponse.name", sanitize_function_name(&function_name));
    if !call_id.is_empty() {
        cpa_json::set(&mut function_response, "functionResponse.id", call_id);
    }
    function_response
}

/// True when a later function output carries `call_id`.
fn responses_has_matching_output(items: &[Res<'_>], call_id: &str) -> bool {
    if call_id.is_empty() {
        return false;
    }
    items.iter().any(|item| is_openai_responses_tool_output(item) && extract_responses_call_id(item) == call_id)
}

fn responses_has_subsequent_turn(items: &[Res<'_>]) -> bool {
    items.iter().any(|item| {
        let typ = item.g("type").str();
        let role = item.g("role").str();
        typ == "message" || (typ.is_empty() && !role.is_empty()) || typ == "function_call" || typ == "custom_tool_call"
    })
}

/// Text parts for a tool output whose call is unknown.
fn build_openai_responses_standalone_tool_output_text_parts(item: &Res<'_>) -> Vec<Value> {
    let output = item.g("output");
    if !output.exists() {
        return Vec::new();
    }
    if output.is_array() {
        return output
            .array()
            .iter()
            .map(|part| part.g("text").str())
            .filter(|text| !text.trim().is_empty())
            .map(|text| text_part(&text))
            .collect();
    }
    let text = output.str();
    if text.trim().is_empty() {
        return Vec::new();
    }
    vec![text_part(&text)]
}

/// The `functionResponse` part for a tool output (the Go helper returns a one-element list).
fn build_openai_responses_function_response_parts(item: &Res<'_>, function_names_by_call_id: &HashMap<String, String>, raws: &RawTexts<'_>) -> Vec<Value> {
    let call_id = extract_responses_call_id(item);
    let function_name = if let Some(matched) = function_names_by_call_id.get(&call_id) {
        matched.clone()
    } else {
        let name = item.g("name").str().trim().to_string();
        if name.is_empty() { "unknown".to_string() } else { name }
    };
    let mut function_response = json!({"functionResponse": {"name": "", "response": {}}});
    cpa_json::set(&mut function_response, "functionResponse.name", sanitize_function_name(&function_name));
    cpa_json::set(&mut function_response, "functionResponse.id", call_id);

    let output_result = item.g("output");
    if output_result.is_string() {
        let s = output_result.str();
        if s.is_empty() || s == "null" {
            return vec![function_response];
        }
        // Kept as a string: parsing it as JSON may trigger an upstream 400 error.
        cpa_json::set(&mut function_response, "functionResponse.response.result", s);
        return vec![function_response];
    }

    let mut image_parts: Vec<Value> = Vec::new();
    if output_result.is_array() {
        let (result, is_raw, images) = parse_open_ai_responses_array_output(&output_result, raws);
        image_parts = images;
        if is_raw {
            set_function_response_result_raw(&mut function_response, "functionResponse.response.result", &result);
        } else {
            cpa_json::set(&mut function_response, "functionResponse.response.result", result);
        }
    } else if output_result.is_object() {
        if let Some((mime_type, data)) = open_ai_responses_media_from_block(&output_result) {
            image_parts.push(gemini_responses_inline_data_part(&mime_type, &data));
            cpa_json::set(&mut function_response, "functionResponse.response.result", "");
        } else {
            let raw = raws.restore(&output_result);
            set_function_response_result_raw(&mut function_response, "functionResponse.response.result", &raw);
        }
    } else if output_result.exists() && !output_result.is_null() {
        cpa_json::set(&mut function_response, "functionResponse.response.result", output_result.str());
    }

    for part in &image_parts {
        let inline = part.g("inline_data");
        let file_data = part.g("file_data");
        if inline.exists() {
            let mut inline_data = json!({"inlineData": {"mimeType": "", "data": ""}});
            cpa_json::set(&mut inline_data, "inlineData.mimeType", inline.g("mime_type").str());
            cpa_json::set(&mut inline_data, "inlineData.data", inline.g("data").str());
            cpa_json::set(&mut function_response, "functionResponse.parts.-1", inline_data);
        } else if file_data.exists() {
            let mut file_data_obj = json!({"fileData": {"mimeType": "", "fileUri": ""}});
            cpa_json::set(&mut file_data_obj, "fileData.mimeType", file_data.g("mime_type").str());
            cpa_json::set(&mut file_data_obj, "fileData.fileUri", file_data.g("file_uri").str());
            cpa_json::set(&mut function_response, "functionResponse.parts.-1", file_data_obj);
        }
    }
    vec![function_response]
}

/// Go `SetGeminiFunctionResponseRaw` over the original raw text: a result containing a string
/// `$ref` is stored as that text (so Gemini does not read it as a media part reference); anything
/// else is stored as JSON. Blank or unparsable text stores the empty string.
fn set_function_response_result_raw(function_response: &mut Value, path: &str, raw_json: &str) {
    let trimmed = raw_json.trim();
    let parsed = if trimmed.is_empty() { None } else { parse_gjson(trimmed.as_bytes()) };
    let Some(parsed) = parsed else {
        cpa_json::set(function_response, path, "");
        return;
    };
    if contains_json_ref(&Res::of(&parsed)) {
        let target = if path.ends_with("response") { format!("{path}.result") } else { path.to_string() };
        cpa_json::set(function_response, &target, trimmed);
    } else {
        cpa_json::set(function_response, path, parsed);
    }
}

/// The outputs in the run of output items starting at `start`, ordered like `pending_call_ids`
/// (unmatched outputs last), and the exclusive end of that run.
fn collect_openai_responses_function_call_outputs<'a>(items: &[Res<'a>], start: usize, pending_call_ids: &[String]) -> (Vec<Res<'a>>, usize) {
    let mut end = start + 1;
    while end < items.len() && is_openai_responses_tool_output(&items[end]) {
        end += 1;
    }
    let outputs = &items[start..end];
    let mut ordered: Vec<Res<'a>> = Vec::with_capacity(outputs.len());
    let mut used = vec![false; outputs.len()];
    for pending_id in pending_call_ids {
        let found = outputs.iter().enumerate().position(|(idx, o)| !used[idx] && extract_responses_call_id(o) == *pending_id);
        if let Some(idx) = found {
            used[idx] = true;
            ordered.push(outputs[idx].clone());
        }
    }
    for (idx, output) in outputs.iter().enumerate() {
        if !used[idx] {
            ordered.push(output.clone());
        }
    }
    (ordered, end)
}

fn build_openai_responses_function_call_model_content(item: &Res<'_>, signature: &str, forward_map: &HashMap<String, String>) -> Value {
    json!({"role": "model", "parts": [build_openai_responses_function_call_part(item, signature, forward_map)]})
}

fn build_openai_responses_empty_reasoning_function_call_model_content(item: &Res<'_>, signature: &str, forward_map: &HashMap<String, String>) -> Value {
    let thought = json!({"text": "", "thought": true, "thoughtSignature": signature});
    json!({"role": "model", "parts": [thought, build_openai_responses_function_call_part(item, signature, forward_map)]})
}

fn build_openai_responses_reasoning_function_call_model_content(thought_text: &str, item: &Res<'_>, signature: &str, forward_map: &HashMap<String, String>) -> Value {
    let mut parts: Vec<Value> = Vec::with_capacity(2);
    if !thought_text.is_empty() {
        parts.push(json!({"text": thought_text, "thought": true}));
    }
    parts.push(build_openai_responses_function_call_part(item, signature, forward_map));
    json!({"role": "model", "parts": parts})
}

/// Model content for a reasoning item (thought text, optional bound visible text, signature).
/// `None` when native layout has nothing to emit.
fn build_openai_responses_reasoning_model_content(thought_text: &str, visible_text: &str, signature: &str, use_native_layout: bool) -> Option<Value> {
    let has_real_signature = !signature.is_empty() && signature != GEMINI_RESPONSES_THOUGHT_SIGNATURE;
    if use_native_layout {
        if thought_text.is_empty() && visible_text.is_empty() {
            if !has_real_signature {
                return None;
            }
            let carrier = json!({"text": "", "thoughtSignature": signature});
            return Some(json!({"role": "model", "parts": [carrier]}));
        }
        let mut parts: Vec<Value> = Vec::new();
        if !thought_text.is_empty() {
            let mut thought = json!({"text": thought_text, "thought": true});
            if visible_text.is_empty() && has_real_signature {
                cpa_json::set(&mut thought, "thoughtSignature", signature);
            }
            parts.push(thought);
        }
        if !visible_text.is_empty() {
            let mut visible = json!({"text": visible_text});
            if has_real_signature {
                cpa_json::set(&mut visible, "thoughtSignature", signature);
            }
            parts.push(visible);
        }
        // Go's SetRawArrayItems is a no-op for an empty list, leaving `"parts":[]`; parts is
        // never empty here.
        return Some(json!({"role": "model", "parts": parts}));
    }

    let mut thought = json!({"text": thought_text, "thought": true});
    if has_real_signature {
        cpa_json::set(&mut thought, "thoughtSignature", signature);
    }
    Some(json!({"role": "model", "parts": [thought]}))
}

fn open_ai_responses_gemini_thought_signature(raw_signature: &str) -> String {
    compatible_signature_for_provider_block(SignatureProvider::Gemini, raw_signature, SignatureBlockKind::GeminiModelPart).unwrap_or_default()
}

/// `text.format` json_object / json_schema -> response MIME type and JSON schema.
fn apply_openai_responses_text_format_to_gemini(out: &mut Value, root: &Value) {
    let text_format = root.g("text.format");
    if !text_format.exists() {
        return;
    }
    match text_format.g("type").str().trim().to_lowercase().as_str() {
        "json_object" => {
            cpa_json::set(out, "generationConfig.responseMimeType", "application/json");
        }
        "json_schema" => {
            cpa_json::set(out, "generationConfig.responseMimeType", "application/json");
            let mut schema = text_format.g("schema");
            if !schema.exists() {
                schema = text_format.g("json_schema.schema");
            }
            if let Some(v) = schema.v() {
                cpa_json::set(out, "generationConfig.responseJsonSchema", v.clone());
            }
        }
        _ => {}
    }
}

/// Content part types that can appear directly as input items without a message wrapper.
pub(super) fn is_responses_content_part_type(item_type: &str) -> bool {
    matches!(
        item_type.trim().to_lowercase().as_str(),
        "input_text"
            | "output_text"
            | "text"
            | "input_image"
            | "image_url"
            | "image"
            | "input_audio"
            | "audio"
            | "input_video"
            | "video_url"
            | "video"
            | "input_file"
            | "file"
    )
}
