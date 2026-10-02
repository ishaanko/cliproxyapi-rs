//! Port of internal/translator/openai/openai/chat-completions (OpenAI -> OpenAI passthrough).

use cpa_core::format::Format;
use cpa_json::J;

use crate::registry::{Ctx, Param, Registry, ResponseFns};

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::OpenAI,
        Some(convert_openai_request_to_openai),
        ResponseFns {
            stream: Some(convert_openai_response_to_openai),
            non_stream: Some(convert_openai_response_to_openai_non_stream),
            token_count: None,
            finalize: None,
        },
    );
}

/// Rewrites the request `model` to `model_name`. The body is re-serialized compactly (Go's sjson
/// edit keeps the client's bytes) and returned as sent when the model already matches or cannot be set.
pub fn convert_openai_request_to_openai(model_name: &str, input: &[u8], _stream: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(input);
    let current = root.g("model");
    if current.is_string() && current.str() == model_name {
        return input.to_vec();
    }
    if !cpa_json::set(&mut root, "model", model_name) {
        return input.to_vec();
    }
    cpa_json::to_vec(&root)
}

/// Strips the SSE `data:` prefix from a chunk and drops `[DONE]` plus anything after it.
pub fn convert_openai_response_to_openai(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    if param.get::<bool>().is_some_and(|done| *done) {
        return vec![];
    }
    let mut raw = raw;
    if let Some(rest) = raw.strip_prefix(b"data:") {
        raw = rest.trim_ascii();
    }
    if raw == b"[DONE]" {
        *param.state(|| true) = true;
        return vec![];
    }
    vec![raw.to_vec()]
}

/// Non-streaming responses are already OpenAI shaped.
pub fn convert_openai_response_to_openai_non_stream(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    Some(raw.to_vec())
}
