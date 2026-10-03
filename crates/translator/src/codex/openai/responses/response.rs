//! Codex Responses -> OpenAI Responses (Go: codex_openai-responses_response.go).

use cpa_json::J;

use crate::common::{ApplyPatchResponsesBridge, request_model_name};
use crate::registry::{Ctx, Param};

/// Go: ConvertCodexResponseToOpenAIResponses. Codex already speaks Responses SSE; only the
/// response `model` is filled in, and an executor-owned apply_patch bridge is applied if present.
pub fn convert_codex_response_to_openai_responses(
    _ctx: &Ctx,
    model_name: &str,
    original_request: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let original_event = raw;
    let sse = raw.starts_with(b"data:");
    let raw = if sse { raw[5..].trim_ascii() } else { raw };
    // Only an executor-owned param can enable bridging on this shared translator.
    if param.get::<ApplyPatchResponsesBridge>().is_none() && !may_need_model(raw) {
        return vec![original_event.to_vec()];
    }
    let updated = set_responses_model(raw, model_name, original_request, request);
    let Some(bridge) = param.get::<ApplyPatchResponsesBridge>() else {
        // Native Codex never opts in, even when configuration supplies the bridge schema.
        return match updated {
            None => vec![original_event.to_vec()],
            Some(mut updated) => {
                if sse {
                    updated.splice(0..0, b"data: ".iter().copied());
                }
                vec![updated]
            }
        };
    };
    let input = updated.as_deref().unwrap_or(raw);
    let (mut outputs, _) = bridge.transform(input);
    let tool_input_error = bridge.tool_input_error().map(str::to_string);
    if sse {
        for out in outputs.iter_mut() {
            out.splice(0..0, b"data: ".iter().copied());
        }
    }
    // Go exposes the bridge's error through the param's ToolInputError contract.
    if tool_input_error.is_some() {
        param.tool_input_error = tool_input_error;
    }
    outputs
}

/// False when `raw` certainly is not a `response.created`/`response.in_progress` event, so the
/// model fill (a full parse) can be skipped: text without any JSON container (`event:` lines,
/// blank keep-alives), or an object whose leading `"type"` is another plain string. Anything
/// else (type not first, escapes, unusual spacing) answers true and takes the parsing path.
fn may_need_model(raw: &[u8]) -> bool {
    if !raw.iter().any(|&b| b == b'{' || b == b'[') {
        return false;
    }
    const LEAD: &[u8] = br#"{"type":""#;
    let Some(rest) = raw.strip_prefix(LEAD) else { return true };
    match rest.iter().position(|&b| b == b'"' || b == b'\\') {
        Some(end) if rest[end] == b'"' => matches!(&rest[..end], b"response.created" | b"response.in_progress"),
        _ => true,
    }
}

/// Fills `response.model` on created/in_progress events lacking it. `None` when unchanged.
fn set_responses_model(
    raw: &[u8],
    model_name: &str,
    original_request: &[u8],
    request: &[u8],
) -> Option<Vec<u8>> {
    let mut root = cpa_json::parse(raw);
    let event_type = root.g("type").str();
    if event_type != "response.created" && event_type != "response.in_progress" {
        return None;
    }
    if root.g("response.model").exists() {
        return None;
    }
    let mut name = request_model_name(original_request, request);
    if name.is_empty() {
        name = model_name.to_string();
    }
    if name.is_empty() {
        return None;
    }
    cpa_json::set(&mut root, "response.model", name);
    Some(cpa_json::to_vec(&root))
}

/// Go: ConvertCodexResponseToOpenAIResponsesNonStream. Extracts `response` from a terminal
/// event; an empty body means the event was not terminal.
pub fn convert_codex_response_to_openai_responses_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    _original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let converted = match param.get::<ApplyPatchResponsesBridge>() {
        Some(bridge) => match bridge.transform_non_stream(raw) {
            Ok(c) => c,
            Err(err) => {
                param.tool_input_error = Some(err);
                return None;
            }
        },
        None => raw.to_vec(),
    };
    let root = cpa_json::parse(&converted);
    // Verify this is a terminal response event.
    let response_type = root.g("type").str();
    if response_type.is_empty() && root.g("output").is_array() {
        return Some(converted);
    }
    if response_type != "response.completed" && response_type != "response.incomplete" {
        return Some(Vec::new());
    }
    Some(root.g("response").raw().into_bytes())
}
