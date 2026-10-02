//! Port of internal/translator/openai/claude (Claude client -> OpenAI Chat Completions upstream).

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::{convert_claude_request_to_openai, convert_claude_request_to_openai_with_compat};
pub use response::{
    claude_token_count, convert_openai_response_to_claude, convert_openai_response_to_claude_non_stream,
};

pub fn register(r: &mut Registry) {
    r.register(
        Format::Claude,
        Format::OpenAI,
        Some(convert_claude_request_to_openai),
        ResponseFns {
            stream: Some(convert_openai_response_to_claude),
            non_stream: Some(convert_openai_response_to_claude_non_stream),
            token_count: Some(claude_token_count),
        },
    );
}
