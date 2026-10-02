//! OpenAI Responses request -> Antigravity request (Go: antigravity_openai-responses_request.go).

use cpa_core::registry::{global_registry, ModelInfo};
use cpa_core::signature::{self, SignatureProvider};
use cpa_core::thinking::{self, SummaryConfig, SummaryMode};
use cpa_core::util::{go_json_sorted, GoJsonStyle};
use cpa_json::{json, J, Value};

use crate::gemini::openai::responses as gemini_responses;
use crate::antigravity::claude::WEB_SEARCH_SYSTEM_INSTRUCTION;
use crate::antigravity::gemini::convert_gemini_request_to_antigravity;
use crate::registry::{Ctx, RequestEnvelope};

/// Whether `model` supports native Google Search on Antigravity: the request-scoped model info
/// wins, else the locally registered Antigravity model of the same (suffix-stripped) id, where an
/// explicit catalog `web_search: false` vetoes the probed support flag.
fn supports_native_responses_web_search(model: &str, model_info: Option<&ModelInfo>) -> bool {
    if let Some(web_search) = model_info.and_then(|m| m.native_capabilities.as_ref()).and_then(|c| c.web_search) {
        return web_search;
    }
    let model = thinking::parse_suffix(model.trim()).model_name.trim().to_string();
    if model.is_empty() {
        return false;
    }
    for local_info in global_registry().get_available_models_by_provider("antigravity") {
        let local_model = thinking::parse_suffix(local_info.id.trim()).model_name.trim().to_string();
        if local_model.to_lowercase() != model.to_lowercase() {
            continue;
        }
        if local_info.native_capabilities.as_ref().and_then(|c| c.web_search) == Some(false) {
            return false;
        }
        return local_info.supports_web_search;
    }
    false
}

fn should_build_web_search_request(model: &str, payload: &[u8], model_info: Option<&ModelInfo>) -> bool {
    let root = cpa_json::parse(payload);
    gemini_responses::has_only_responses_web_search_tools(&root)
        && supports_native_responses_web_search(model, model_info)
        && gemini_responses::allows_responses_web_search_tool_choice(&root)
}

fn build_web_search_request(model: &str, payload: &[u8], stream: bool) -> Vec<u8> {
    let included_domains = gemini_responses::extract_responses_web_search_allowed_domains(&cpa_json::parse(payload));
    let raw = gemini_responses::convert_openai_responses_request_to_gemini(model, payload, stream);
    let raw = rewrite_reasoning_for_antigravity_claude(model, payload, raw);
    let out = convert_gemini_request_to_antigravity(model, &raw, stream);
    let mut root = cpa_json::parse(&out);
    cpa_json::set(&mut root, "requestType", "web_search");
    ensure_web_search_tool(&mut root, &included_domains);
    ensure_web_search_system_instruction(&mut root);
    enable_thinking_summary(payload, cpa_json::to_vec(&root))
}

/// Makes `request.tools` carry exactly one `googleSearch` tool, replacing an existing one in place
/// or prepending it.
fn ensure_web_search_tool(payload: &mut Value, included_domains: &[String]) {
    let mut google_search_tool = json!({"googleSearch": {"enhancedContent": {"imageSearch": {"maxResultCount": 5}}}});
    if !included_domains.is_empty() {
        cpa_json::set(&mut google_search_tool, "googleSearch.includedDomains", json!(included_domains));
    }

    let tools = payload.g("request.tools");
    if !tools.is_array() {
        cpa_json::set(payload, "request.tools", Value::Array(vec![google_search_tool]));
        return;
    }
    let mut replaced = false;
    let mut filtered: Vec<Value> = Vec::new();
    for tool in tools.array() {
        if tool.g("googleSearch").exists() {
            if !replaced {
                filtered.push(google_search_tool.clone());
                replaced = true;
            }
            continue;
        }
        filtered.push(tool.value());
    }
    if !replaced {
        filtered.insert(0, google_search_tool);
    }
    cpa_json::set(payload, "request.tools", Value::Array(filtered));
}

/// Appends the search-bot instruction to `request.systemInstruction.parts` unless present.
fn ensure_web_search_system_instruction(payload: &mut Value) {
    let search_part = json!({"text": WEB_SEARCH_SYSTEM_INSTRUCTION});
    let sys = payload.g("request.systemInstruction");
    if !sys.exists() {
        cpa_json::set(payload, "request.systemInstruction", json!({"role": "user", "parts": [search_part]}));
        return;
    }
    let mut parts: Vec<Value> = Vec::new();
    let mut already_present = false;
    let sys_parts = sys.g("parts");
    if sys_parts.is_array() {
        for part in sys_parts.array() {
            if part.g("text").str() == WEB_SEARCH_SYSTEM_INSTRUCTION {
                already_present = true;
            }
            parts.push(part.value());
        }
    }
    if !already_present {
        parts.push(search_part);
    }
    cpa_json::set(payload, "request.systemInstruction.parts", Value::Array(parts));
}

/// Go: `ConvertOpenAIResponsesRequestToAntigravity`: translates with locally registered
/// Antigravity capabilities (no request-scoped model info).
pub fn convert_openai_responses_request_to_antigravity(model: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let req = RequestEnvelope { model: model.to_string(), stream, body: input_raw_json.to_vec(), ..Default::default() };
    convert_openai_responses_request_envelope_to_antigravity(&Ctx::default(), req).body
}

/// Go: `ConvertOpenAIResponsesRequestEnvelopeToAntigravity`: like the plain converter but consumes
/// request-scoped model capabilities from the envelope.
pub fn convert_openai_responses_request_envelope_to_antigravity(_ctx: &Ctx, mut req: RequestEnvelope) -> RequestEnvelope {
    if should_build_web_search_request(&req.model, &req.body, req.model_info.as_ref()) {
        req.body = build_web_search_request(&req.model, &req.body, req.stream);
        return req;
    }
    let input_raw_json = std::mem::take(&mut req.body);
    let mut body = gemini_responses::convert_openai_responses_request_to_gemini(&req.model, &input_raw_json, req.stream);
    body = strip_google_search(body);
    body = rewrite_reasoning_for_antigravity_claude(&req.model, &input_raw_json, body);
    body = convert_gemini_request_to_antigravity(&req.model, &body, req.stream);
    body = strip_google_search(body);
    req.body = enable_thinking_summary(&input_raw_json, body);
    req
}

/// Removes native `googleSearch` tools when falling back to a normal chat request: Antigravity only
/// supports native search in dedicated web_search envelopes and rejects mixed googleSearch and
/// function declarations.
fn strip_google_search(payload: Vec<u8>) -> Vec<u8> {
    let mut root = cpa_json::parse(&payload);
    let mut changed = false;
    for path in ["tools", "request.tools"] {
        let tools = root.g(path);
        if !tools.is_array() {
            continue;
        }
        let mut filtered: Vec<Value> = Vec::new();
        let mut has_google_search = false;
        for tool in tools.array() {
            if tool.g("googleSearch").exists() {
                has_google_search = true;
                continue;
            }
            filtered.push(tool.value());
        }
        if has_google_search {
            changed = true;
            if filtered.is_empty() {
                cpa_json::delete(&mut root, path);
            } else {
                cpa_json::set(&mut root, path, Value::Array(filtered));
            }
        }
    }
    if changed { cpa_json::to_vec(&root) } else { payload }
}

/// OpenAI Responses separates reasoning effort from summary visibility. Antigravity needs
/// includeThoughts to emit thought parts, so effort alone would spend thinking tokens without
/// returning a visible summary.
fn enable_thinking_summary(input_raw_json: &[u8], translated: Vec<u8>) -> Vec<u8> {
    let input = cpa_json::parse(input_raw_json);
    let effort = input.g("reasoning.effort");
    if !effort.is_string() {
        return translated;
    }
    let effort_val = effort.str().trim().to_lowercase();
    if effort_val.is_empty() || effort_val == "none" {
        return translated;
    }
    let mut summary_config = thinking::extract_summary_config(input_raw_json, "openai-response");
    if summary_config.mode == SummaryMode::Unspecified {
        // Effort set but summary visibility omitted: enable summaries by default so Antigravity
        // emits visible thought parts (#5508).
        summary_config = SummaryConfig { mode: SummaryMode::Enabled, detail: "auto".to_string() };
    }
    thinking::apply_summary_config(translated, "antigravity", &summary_config)
}

/// Compatible Claude signatures of the request's `reasoning` input items, in order ("" when an
/// item has none).
fn claude_reasoning_signatures(input_raw_json: &[u8]) -> Vec<String> {
    let root = cpa_json::parse(input_raw_json);
    let input = root.g("input");
    if !input.is_array() {
        return vec![];
    }
    let mut signatures = Vec::new();
    for item in input.array() {
        let mut item_type = item.g("type").str();
        if item_type.is_empty() && item.g("role").exists() {
            item_type = "message".to_string();
        }
        if item_type != "reasoning" {
            continue;
        }
        let raw_signature = item.g("encrypted_content").str();
        signatures.push(signature::compatible_antigravity_claude_thinking_signature(&raw_signature).unwrap_or_default());
    }
    signatures
}

/// For Claude targets, replays reasoning signatures positionally: the i-th `thought` part of the
/// Gemini request takes the i-th reasoning item's compatible signature; parts without a valid
/// signature or text are dropped, as are contents left with no parts. The body is re-serialized
/// like Go's `map[string]any` round trip (sorted keys, float64 numbers) only when something changed.
fn rewrite_reasoning_for_antigravity_claude(model: &str, input_raw_json: &[u8], gemini_json: Vec<u8>) -> Vec<u8> {
    if signature::signature_provider_from_model_name(model) != SignatureProvider::Claude {
        return gemini_json;
    }
    let reasoning_signatures = claude_reasoning_signatures(input_raw_json);
    if reasoning_signatures.is_empty() {
        return gemini_json;
    }
    let mut root = cpa_json::parse(&gemini_json);
    if !root.is_object() {
        return gemini_json;
    }
    let Some(Value::Array(contents)) = root.get_mut("contents") else {
        return gemini_json;
    };

    let mut reasoning_index = 0usize;
    let mut changed = false;
    let mut rewritten_contents: Vec<Value> = Vec::with_capacity(contents.len());
    for content_value in std::mem::take(contents) {
        let Value::Object(mut content) = content_value else {
            rewritten_contents.push(content_value);
            continue;
        };
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            rewritten_contents.push(Value::Object(content));
            continue;
        };
        let mut rewritten_parts: Vec<Value> = Vec::with_capacity(parts.len());
        for part_value in std::mem::take(parts) {
            let is_thought = matches!(&part_value, Value::Object(m) if m.get("thought") == Some(&Value::Bool(true)));
            let Value::Object(mut part) = part_value else {
                rewritten_parts.push(part_value);
                continue;
            };
            if !is_thought {
                rewritten_parts.push(Value::Object(part));
                continue;
            }
            let reasoning_sig = reasoning_signatures.get(reasoning_index).cloned().unwrap_or_default();
            reasoning_index += 1;

            if reasoning_sig.is_empty() {
                changed = true;
                continue;
            }
            if part.get("text").and_then(Value::as_str).unwrap_or_default().trim().is_empty() {
                changed = true;
                continue;
            }
            if part.get("thoughtSignature").and_then(Value::as_str).unwrap_or_default() != reasoning_sig {
                changed = true;
            }
            part.insert("thoughtSignature".to_string(), Value::String(reasoning_sig));
            rewritten_parts.push(Value::Object(part));
        }
        if rewritten_parts.is_empty() {
            changed = true;
            continue;
        }
        content.insert("parts".to_string(), Value::Array(rewritten_parts));
        rewritten_contents.push(Value::Object(content));
    }

    if !changed {
        return gemini_json;
    }
    cpa_json::set(&mut root, "contents", Value::Array(rewritten_contents));
    match go_json_sorted(&root, GoJsonStyle::MARSHAL_ANY) {
        Some(s) => s.into_bytes(),
        None => gemini_json,
    }
}
