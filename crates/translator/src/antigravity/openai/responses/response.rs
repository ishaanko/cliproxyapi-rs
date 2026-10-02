//! Antigravity response -> OpenAI Responses response (Go: antigravity_openai-responses_response.go).
//!
//! Antigravity wraps Gemini responses in `{"response": ...}`; both converters unwrap it and hand
//! over to the Gemini -> Responses converters (whose state lives in the shared `Param`).

use crate::gemini::openai::responses as gemini_responses;
use crate::registry::{Ctx, Param};

/// The inner `response` object as bytes when present, else the input unchanged.
fn unwrap_bytes(raw: &[u8]) -> Vec<u8> {
    // The original text is kept (Go passes `Result.Raw`), since downstream copies raw argument text.
    match cpa_json::raw_at(raw, "response") {
        Some(response) => response.as_bytes().to_vec(),
        None => raw.to_vec(),
    }
}

/// `root.request` as bytes when present, else the input unchanged.
fn rebase_to_request(raw: &[u8]) -> Vec<u8> {
    match cpa_json::raw_at(raw, "request") {
        Some(request) => request.as_bytes().to_vec(),
        None => raw.to_vec(),
    }
}

/// Go: `ConvertAntigravityResponseToOpenAIResponses`.
pub fn convert_antigravity_response_to_openai_responses(
    ctx: &Ctx,
    model: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let raw = unwrap_bytes(raw_json);
    gemini_responses::convert_gemini_response_to_openai_responses(ctx, model, original_request_raw_json, request_raw_json, &raw, param)
}

/// Go: `ConvertAntigravityResponseToOpenAIResponsesNonStream`. The original and translated
/// requests are rebased to their `request` sub-object when they carry the Antigravity envelope.
pub fn convert_antigravity_response_to_openai_responses_non_stream(
    ctx: &Ctx,
    model: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let raw = unwrap_bytes(raw_json);
    let original = rebase_to_request(original_request_raw_json);
    let request = rebase_to_request(request_raw_json);
    gemini_responses::convert_gemini_response_to_openai_responses_non_stream(ctx, model, &original, &request, &raw, param)
}
