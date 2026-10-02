//! Gemini response -> single OpenAI Responses object (Go:
//! gemini_openai-responses_response.go, non-streaming half).

use std::collections::{HashMap, HashSet};

use cpa_core::util::{restore_sanitized_tool_name, responses_tool_reverse_identity_map, sanitized_tool_name_map, unwrap_responses_custom_tool_input, ResponsesToolIdentity};
use cpa_json::{json, Value, J};

use super::lenient::{gjson_valid, parse_gjson};
use super::function_evidence::{pending_identity_error, record_function_evidence, EvidenceStore};
use super::response::{
    echo_request_fields, next_func_call_id_counter, next_response_id_counter, parse_create_time, pick_request_json, set_usage,
    unix_nanos_now, unix_now, unwrap_gemini_response_root, unwrap_request_root, with_tool_identity,
};
use super::signature_carrier::{encode_gemini_responses_carrier, CARRIER_ANY, CARRIER_FUNCTION, CARRIER_NEXT, CARRIER_PREVIOUS, CARRIER_STANDALONE, CARRIER_TEXT};
use super::web_search::{
    build_responses_url_citations_for_messages, build_responses_web_search_call_item, extract_grounding_metadata, extract_grounding_queries,
    extract_grounding_sources, extract_responses_web_search_query, go_rune_count, has_valid_web_grounding, GeminiPartMapping,
};
use crate::common::ApplyPatchCallState;
use crate::registry::{Ctx, Param};

struct ReasoningOutput {
    text: String,
    signature: String,
    direction: String,
    target_kind: String,
}

struct FunctionOutput {
    item: Value,
    signature: String,
}

struct MessageOutput {
    text: String,
    signatures: Vec<String>,
}

struct DetachedOutput {
    signature: String,
    direction: String,
    target_kind: String,
}

#[derive(Clone, Copy)]
enum OutputKind {
    Reasoning,
    Message,
    Function,
    Detached,
}

/// Accumulates parts into ordered reasoning / message / function / detached outputs.
#[derive(Default)]
struct Aggregator {
    reasoning_text: String,
    reasoning_encrypted: String,
    reasoning_direction: String,
    reasoning_target_kind: String,
    reasoning_outputs: Vec<ReasoningOutput>,
    function_outputs: Vec<FunctionOutput>,
    message_outputs: Vec<MessageOutput>,
    output_order: Vec<(OutputKind, usize)>,
    reasoning_output_signatures: HashSet<String>,
    detached_reasoning_outputs: Vec<DetachedOutput>,
    current_message_text: String,
    current_message_signatures: Vec<String>,
    part_mappings: Vec<GeminiPartMapping>,
    current_msg_rune_offset: i64,
}

impl Aggregator {
    fn flush_reasoning_output(&mut self) {
        if self.reasoning_text.is_empty() && self.reasoning_encrypted.is_empty() {
            return;
        }
        let index = self.reasoning_outputs.len();
        self.reasoning_outputs.push(ReasoningOutput {
            text: std::mem::take(&mut self.reasoning_text),
            signature: self.reasoning_encrypted.clone(),
            direction: std::mem::take(&mut self.reasoning_direction),
            target_kind: std::mem::take(&mut self.reasoning_target_kind),
        });
        self.output_order.push((OutputKind::Reasoning, index));
        if !self.reasoning_encrypted.is_empty() {
            self.reasoning_output_signatures.insert(self.reasoning_encrypted.clone());
        }
        self.reasoning_encrypted.clear();
    }

    fn flush_message_output(&mut self) {
        if self.current_message_text.is_empty() {
            return;
        }
        let index = self.message_outputs.len();
        self.message_outputs.push(MessageOutput {
            text: std::mem::take(&mut self.current_message_text),
            signatures: std::mem::take(&mut self.current_message_signatures),
        });
        self.output_order.push((OutputKind::Message, index));
        self.current_msg_rune_offset = 0;
    }

    fn push_detached(&mut self, signature: &str, direction: &str, target_kind: &str) {
        let index = self.detached_reasoning_outputs.len();
        self.detached_reasoning_outputs.push(DetachedOutput { signature: signature.to_string(), direction: direction.to_string(), target_kind: target_kind.to_string() });
        self.output_order.push((OutputKind::Detached, index));
    }
}

/// Aggregates Gemini response JSON into a single OpenAI Responses JSON object. `None` mirrors a
/// failed apply_patch conversion (the error is stored in `param.tool_input_error`).
pub fn convert_gemini_response_to_openai_responses_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let valid_json = gjson_valid(raw_json);
    let (root, wrapped) = unwrap_gemini_response_root(parse_gjson(raw_json).unwrap_or(Value::Null));
    let root_raw: &[u8] = if wrapped { cpa_json::raw_at(raw_json, "response").map(str::as_bytes).unwrap_or(raw_json) } else { raw_json };
    let req_json = pick_request_json(original_request_raw_json, request_raw_json);
    let req_value: Option<Value> = req_json.map(cpa_json::parse);
    let sanitized_name_map = sanitized_tool_name_map(original_request_raw_json);
    let tool_identity_map: HashMap<String, ResponsesToolIdentity> = responses_tool_reverse_identity_map(req_json.unwrap_or_default());

    // Base response scaffold.
    let mut resp = json!({"id": "", "object": "response", "created_at": 0, "status": "completed", "background": false, "error": null, "incomplete_details": null});

    // id: prefer provider responseId, otherwise synthesize; normalized to resp_ prefix.
    let mut id = root.g("responseId").str();
    if id.is_empty() {
        id = format!("resp_{:x}_{}", unix_nanos_now(), next_response_id_counter());
    }
    if !id.starts_with("resp_") {
        id = format!("resp_{id}");
    }
    cpa_json::set(&mut resp, "id", id.as_str());

    let mut created_at = unix_now();
    let create_time = root.g("createTime");
    if create_time.exists() {
        if let Some(t) = parse_create_time(&create_time.str()) {
            created_at = t;
        }
    }
    cpa_json::set(&mut resp, "created_at", created_at);

    // Echo request fields when present; the model falls back to the response modelVersion.
    if let Some(req_value) = &req_value {
        let req = unwrap_request_root(req_value);
        let model = req.g("model");
        let model_version = root.g("modelVersion");
        if model.exists() {
            cpa_json::set(&mut resp, "model", model.str());
        } else if model_version.exists() {
            cpa_json::set(&mut resp, "model", model_version.str());
        }
        echo_request_fields(&mut resp, "", req, false);
    } else {
        let model_version = root.g("modelVersion");
        if model_version.exists() {
            cpa_json::set(&mut resp, "model", model_version.str());
        }
    }

    // Outputs from candidates[0].content.parts.
    let mut agg = Aggregator::default();
    let mut tool_input_error: Option<String> = None;
    let mut evidence_state = EvidenceStore::default();
    let mut outputs: Vec<Value> = Vec::new();
    let mut detached_output_index = 0usize;
    let mut seen_detached_outputs: HashSet<String> = HashSet::new();
    let rid = id.strip_prefix("resp_").unwrap_or(&id).to_string();

    let mut append_detached_output = |outputs: &mut Vec<Value>, signature: &str, direction: &str, target_kind: &str| {
        if signature.is_empty() || !seen_detached_outputs.insert(signature.to_string()) {
            return;
        }
        let placement = if direction == CARRIER_PREVIOUS { "after" } else { "before" };
        outputs.push(json!({
            "id": format!("rs_{rid}_detached_{placement}_{detached_output_index}"),
            "type": "reasoning",
            "encrypted_content": encode_gemini_responses_carrier(signature, direction, target_kind),
            "summary": [],
        }));
        detached_output_index += 1;
    };

    let parts = root.g("candidates.0.content.parts");
    if parts.exists() && parts.is_array() {
        for (key, p) in parts.array().iter().enumerate() {
            let mut part_idx = key as i64;
            let p_idx = p.g("partIndex");
            if p_idx.exists() {
                part_idx = p_idx.int();
            } else {
                let p_idx = p.g("index");
                if p_idx.exists() {
                    part_idx = p_idx.int();
                }
            }
            let mut signature = p.g("thoughtSignature").str().trim().to_string();
            if signature.is_empty() {
                signature = p.g("thought_signature").str().trim().to_string();
            }
            if p.g("thought").bool() {
                agg.flush_message_output();
                agg.current_msg_rune_offset = 0;
                if !signature.is_empty() && !agg.reasoning_encrypted.is_empty() && signature != agg.reasoning_encrypted {
                    agg.flush_reasoning_output();
                }
                let t = p.g("text");
                if t.exists() {
                    agg.reasoning_text.push_str(&t.str());
                }
                if !signature.is_empty() {
                    agg.reasoning_encrypted = signature;
                    agg.reasoning_direction = CARRIER_STANDALONE.to_string();
                    agg.reasoning_target_kind = CARRIER_TEXT.to_string();
                }
                continue;
            }
            let t = p.g("text");
            if t.exists() && !t.str().is_empty() {
                let mut message_signature = String::new();
                if !signature.is_empty() {
                    if !agg.reasoning_text.is_empty() && agg.reasoning_encrypted.is_empty() {
                        agg.reasoning_encrypted = signature.clone();
                        agg.reasoning_direction = CARRIER_NEXT.to_string();
                        agg.reasoning_target_kind = CARRIER_TEXT.to_string();
                    } else {
                        message_signature = signature.clone();
                    }
                }
                agg.flush_reasoning_output();
                if !agg.current_message_signatures.is_empty()
                    && (message_signature.is_empty() || agg.current_message_signatures.last() != Some(&message_signature))
                {
                    agg.flush_message_output();
                    agg.current_msg_rune_offset = 0;
                }
                let part_text = t.str();
                agg.part_mappings.push(GeminiPartMapping {
                    part_index: part_idx,
                    message_index: agg.message_outputs.len() as i64,
                    start_rune_in_msg: agg.current_msg_rune_offset,
                    part_text: part_text.clone(),
                });
                agg.current_msg_rune_offset += go_rune_count(part_text.as_bytes());
                agg.current_message_text.push_str(&part_text);
                if !message_signature.is_empty() && agg.current_message_signatures.last() != Some(&message_signature) {
                    agg.current_message_signatures.push(message_signature);
                }
                continue;
            }
            let fc = p.g("functionCall");
            if fc.exists() {
                if !agg.reasoning_text.is_empty() && agg.reasoning_encrypted.is_empty() && !signature.is_empty() {
                    agg.reasoning_encrypted = std::mem::take(&mut signature);
                    agg.reasoning_direction = CARRIER_NEXT.to_string();
                    agg.reasoning_target_kind = CARRIER_FUNCTION.to_string();
                }
                agg.flush_reasoning_output();
                agg.flush_message_output();
                agg.current_msg_rune_offset = 0;

                let mut explicit_index: i64 = -1;
                if p.g("partIndex").exists() {
                    explicit_index = p.g("partIndex").int();
                } else if p.g("index").exists() {
                    explicit_index = p.g("index").int();
                }
                // Raw argument text as sent (gjson `Raw`): whitespace and duplicate keys preserved.
                let args = fc.g("args");
                let args_str = if args.exists() {
                    cpa_json::raw_at(root_raw, &format!("candidates.0.content.parts.{key}.functionCall.args")).map(str::to_string).unwrap_or_else(|| args.raw())
                } else {
                    String::new()
                };
                let evidence_idx = record_function_evidence(&mut evidence_state, &tool_identity_map, &fc, &args_str, explicit_index, valid_json);
                {
                    let evidence = &evidence_state.entries[evidence_idx];
                    if evidence.apply_patch && evidence.err.is_some() {
                        tool_input_error = evidence.err.clone();
                        break;
                    }
                    if evidence.raw_name.is_empty() {
                        continue;
                    }
                }
                let (evidence_apply_patch, evidence_raw_name, evidence_upstream_id) = {
                    let e = &evidence_state.entries[evidence_idx];
                    (e.apply_patch, e.raw_name.clone(), e.upstream_id.clone())
                };
                let mut raw_name = fc.g("name").str();
                if evidence_apply_patch {
                    raw_name = evidence_raw_name;
                }
                let identity = match tool_identity_map.get(&raw_name) {
                    Some(i) => i.clone(),
                    None => ResponsesToolIdentity { name: restore_sanitized_tool_name(&sanitized_name_map, &raw_name), ..Default::default() },
                };
                let name = identity.name.clone();
                let namespace = identity.namespace.clone();
                let is_custom = identity.custom;
                if identity.apply_patch {
                    if let Some(patch_call) = evidence_state.entries[evidence_idx].patch_call.as_mut() {
                        if let Err(err) = patch_call.finish_arguments(&args_str) {
                            tool_input_error = Some(err);
                            break;
                        }
                        continue;
                    }
                }

                let mut call_id = format!("call_{:x}_{}", unix_nanos_now(), next_func_call_id_counter());
                if identity.apply_patch && !evidence_upstream_id.is_empty() {
                    call_id = evidence_upstream_id;
                }
                let item_json: Value;
                if is_custom {
                    let mut input_str = unwrap_responses_custom_tool_input(&args_str);
                    if identity.apply_patch {
                        let mut patch_call = ApplyPatchCallState::default();
                        let finished = patch_call.finish_arguments(&args_str);
                        let finished = if !valid_json { Err("invalid Gemini apply_patch response JSON".to_string()) } else { finished };
                        match finished {
                            Err(err) => {
                                tool_input_error = Some(err);
                                break;
                            }
                            Ok((_, input)) => input_str = input,
                        }
                        evidence_state.entries[evidence_idx].patch_call = Some(patch_call);
                    }
                    item_json = with_tool_identity(
                        json!({"id": format!("ctc_{call_id}"), "type": "custom_tool_call", "status": "completed", "input": input_str, "call_id": call_id, "name": ""}),
                        &name,
                        &namespace,
                    );
                } else {
                    item_json = with_tool_identity(
                        json!({"id": format!("fc_{call_id}"), "type": "function_call", "status": "completed", "arguments": args_str, "call_id": call_id, "name": ""}),
                        &name,
                        &namespace,
                    );
                }
                let function_index = agg.function_outputs.len();
                agg.function_outputs.push(FunctionOutput { item: item_json, signature });
                agg.output_order.push((OutputKind::Function, function_index));
                continue;
            }
            if !signature.is_empty() {
                if !agg.reasoning_text.is_empty() {
                    if agg.reasoning_encrypted.is_empty() {
                        agg.reasoning_encrypted = signature;
                        agg.reasoning_direction = CARRIER_STANDALONE.to_string();
                        agg.reasoning_target_kind = CARRIER_TEXT.to_string();
                    } else if agg.reasoning_encrypted != signature {
                        agg.flush_reasoning_output();
                        agg.push_detached(&signature, CARRIER_PREVIOUS, CARRIER_TEXT);
                    }
                } else if !agg.current_message_text.is_empty() {
                    if agg.current_message_signatures.is_empty() {
                        agg.current_message_signatures.push(signature);
                    } else if agg.current_message_signatures.last() != Some(&signature) {
                        agg.flush_message_output();
                        agg.current_msg_rune_offset = 0;
                        agg.push_detached(&signature, CARRIER_PREVIOUS, CARRIER_TEXT);
                    }
                } else if !agg.function_outputs.is_empty() {
                    agg.push_detached(&signature, CARRIER_PREVIOUS, CARRIER_FUNCTION);
                } else {
                    agg.push_detached(&signature, CARRIER_NEXT, CARRIER_ANY);
                }
            }
        }
    }

    if tool_input_error.is_none() {
        tool_input_error = pending_identity_error(&evidence_state, &tool_identity_map);
    }
    if let Some(err) = tool_input_error {
        param.tool_input_error = Some(err);
        return None;
    }
    agg.flush_reasoning_output();
    agg.flush_message_output();

    // Web search from groundingMetadata.
    let grounding_metadata = extract_grounding_metadata(&root).unwrap_or(Value::Null);
    let has_grounding = has_valid_web_grounding(&grounding_metadata);
    let mut ws_item: Option<Value> = None;
    let mut message_citations = Default::default();
    if has_grounding {
        let queries = extract_grounding_queries(&grounding_metadata);
        let mut query = queries.first().cloned().unwrap_or_default();
        if query.is_empty() {
            if let Some(req_value) = &req_value {
                query = extract_responses_web_search_query(unwrap_request_root(req_value));
            }
        }
        let sources = extract_grounding_sources(&grounding_metadata);
        let ws_id = format!("ws_{rid}");
        ws_item = Some(build_responses_web_search_call_item(&ws_id, &query, &queries, &sources));

        let message_texts: Vec<String> = agg.message_outputs.iter().map(|m| m.text.clone()).collect();
        message_citations = build_responses_url_citations_for_messages(&grounding_metadata, &agg.part_mappings, &message_texts);
    }

    let mut ws_appended = false;
    for (kind, index) in agg.output_order.iter().copied() {
        match kind {
            OutputKind::Detached => {
                let Some(detached) = agg.detached_reasoning_outputs.get(index) else { continue };
                if !agg.reasoning_output_signatures.contains(&detached.signature) {
                    append_detached_output(&mut outputs, &detached.signature, &detached.direction, &detached.target_kind);
                }
            }
            OutputKind::Reasoning => {
                let Some(reasoning_output) = agg.reasoning_outputs.get(index) else { continue };
                let mut reasoning_id = format!("rs_{rid}");
                if agg.reasoning_outputs.len() > 1 {
                    reasoning_id = format!("rs_{rid}_{index}");
                }
                let mut encrypted_content = reasoning_output.signature.clone();
                if !encrypted_content.is_empty() && !reasoning_output.direction.is_empty() {
                    encrypted_content = encode_gemini_responses_carrier(&encrypted_content, &reasoning_output.direction, &reasoning_output.target_kind);
                }
                let mut item = json!({"id": reasoning_id, "type": "reasoning", "encrypted_content": encrypted_content});
                if !reasoning_output.text.is_empty() {
                    cpa_json::set(&mut item, "summary", json!([{"type": "summary_text", "text": reasoning_output.text}]));
                }
                outputs.push(item);
            }
            OutputKind::Message => {
                if has_grounding && !ws_appended {
                    if let Some(ws) = &ws_item {
                        outputs.push(ws.clone());
                    }
                    ws_appended = true;
                }
                let Some(message_output) = agg.message_outputs.get(index) else { continue };
                for signature in &message_output.signatures {
                    if !agg.reasoning_output_signatures.contains(signature) {
                        append_detached_output(&mut outputs, signature, CARRIER_NEXT, CARRIER_TEXT);
                    }
                }
                let mut item = json!({"id": format!("msg_{rid}_{index}"), "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": message_output.text}], "role": "assistant"});
                if let Some(c) = message_citations.get(&(index as i64)).filter(|c: &&Vec<Value>| !c.is_empty()) {
                    cpa_json::set(&mut item, "content.0.annotations", Value::Array(c.clone()));
                }
                outputs.push(item);
            }
            OutputKind::Function => {
                let Some(function_output) = agg.function_outputs.get(index) else { continue };
                append_detached_output(&mut outputs, &function_output.signature, CARRIER_NEXT, CARRIER_FUNCTION);
                outputs.push(function_output.item.clone());
            }
        }
    }

    if has_grounding && !ws_appended {
        if let Some(ws) = ws_item {
            outputs.push(ws);
        }
    }

    if !outputs.is_empty() {
        cpa_json::set(&mut resp, "output", Value::Array(outputs));
    }
    if has_grounding {
        cpa_json::set(&mut resp, "tool_usage.web_search.num_requests", 1);
    }

    let um = root.g("usageMetadata");
    if um.exists() {
        set_usage(&mut resp, "usage", &um, false);
    }

    Some(cpa_json::to_vec(&resp))
}
