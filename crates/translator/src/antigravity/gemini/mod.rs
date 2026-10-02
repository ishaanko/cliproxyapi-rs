//! Port of internal/translator/antigravity/gemini: Gemini client -> Antigravity upstream.

mod request;
mod response;

pub use request::{convert_gemini_request_to_antigravity, sanitize_antigravity_claude_gemini_request_signatures};
pub(crate) use response::has_response_payload;
pub use response::{
    convert_antigravity_response_to_gemini, convert_antigravity_response_to_gemini_non_stream, gemini_token_count,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Gemini,
        Format::Antigravity,
        Some(convert_gemini_request_to_antigravity),
        ResponseFns {
            stream: Some(convert_antigravity_response_to_gemini),
            non_stream: Some(convert_antigravity_response_to_gemini_non_stream),
            token_count: Some(gemini_token_count),
        },
    );
}
