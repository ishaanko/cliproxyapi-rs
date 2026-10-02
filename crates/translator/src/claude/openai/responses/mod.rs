//! Port of internal/translator/claude/openai/responses.

mod request;
mod response;
mod tool_names;
mod web_search;

pub use request::{convert_openai_responses_request_to_claude, convert_openai_responses_request_to_claude_with_compat};
pub use response::{
    CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX, convert_claude_response_to_openai_responses,
    convert_claude_response_to_openai_responses_non_stream, finalize_tool_input,
};

use crate::Format;
use crate::registry::{Registry, ResponseFns};

/// Client OpenAI Responses, upstream Claude.
pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::Claude,
        Some(convert_openai_responses_request_to_claude),
        ResponseFns {
            stream: Some(convert_claude_response_to_openai_responses),
            non_stream: Some(convert_claude_response_to_openai_responses_non_stream),
            token_count: None,
        },
    );
}

#[cfg(test)]
mod tests;
