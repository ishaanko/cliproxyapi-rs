//! Port of internal/translator/claude/gemini: Gemini clients served by a Claude upstream.

mod request;
mod response;

use cpa_core::format::Format;

use crate::common::gemini_token_count_json;
use crate::registry::{Ctx, Registry, ResponseFns};

pub use request::convert_gemini_request_to_claude;
pub use response::{convert_claude_response_to_gemini, convert_claude_response_to_gemini_non_stream};

/// Gemini countTokens response for a Claude upstream.
pub fn gemini_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    gemini_token_count_json(count)
}

pub fn register(r: &mut Registry) {
    r.register(
        Format::Gemini,
        Format::Claude,
        Some(convert_gemini_request_to_claude),
        ResponseFns {
            stream: Some(convert_claude_response_to_gemini),
            non_stream: Some(convert_claude_response_to_gemini_non_stream),
            token_count: Some(gemini_token_count),
        },
    );
}
