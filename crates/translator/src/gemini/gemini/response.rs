//! Gemini passthrough responses (Go: gemini/gemini/gemini_gemini_response.go).

use crate::common::gemini_token_count_json;
use crate::registry::{Ctx, Param};

/// Forwards a Gemini stream line unchanged, minus an SSE `data:` prefix. `[DONE]` yields nothing.
pub fn passthrough_gemini_response_stream(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Vec<Vec<u8>> {
    let mut raw = raw;
    if let Some(rest) = raw.strip_prefix(b"data:") {
        raw = rest.trim_ascii();
    }
    if raw == b"[DONE]" {
        return vec![];
    }
    vec![raw.to_vec()]
}

/// Forwards a complete Gemini response unchanged.
pub fn passthrough_gemini_response_non_stream(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    Some(raw.to_vec())
}

/// Gemini `countTokens` response for a token count.
pub fn gemini_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    gemini_token_count_json(count)
}
