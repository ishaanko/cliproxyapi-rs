//! Port of internal/translator/gemini/claude (Claude client, Gemini upstream).

mod raw;
mod request;
mod response;

pub(crate) use raw::{set_function_response_from_text, RawDoc};

pub use request::{convert_claude_request_to_gemini, convert_claude_request_to_gemini_with_compat};
pub use response::{
    claude_token_count, convert_gemini_response_to_claude, convert_gemini_response_to_claude_non_stream, Params,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Claude,
        Format::Gemini,
        Some(convert_claude_request_to_gemini),
        ResponseFns {
            stream: Some(convert_gemini_response_to_claude),
            non_stream: Some(convert_gemini_response_to_claude_non_stream),
            token_count: Some(claude_token_count),
        },
    );
}
