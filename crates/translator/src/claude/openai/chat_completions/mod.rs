//! Port of internal/translator/claude/openai/chat-completions.

mod raw_json;
mod request;
mod response;

pub(crate) use raw_json::{raw_at, structured_output_instruction};
pub(crate) use request::apply_reasoning_effort;
pub use request::{convert_openai_request_to_claude, convert_openai_request_to_claude_with_compat};
pub use response::{convert_claude_response_to_openai, convert_claude_response_to_openai_non_stream};

use crate::Format;
use crate::registry::{Registry, ResponseFns};

/// Client OpenAI Chat Completions, upstream Claude.
pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::Claude,
        Some(convert_openai_request_to_claude),
        ResponseFns {
            stream: Some(convert_claude_response_to_openai),
            non_stream: Some(convert_claude_response_to_openai_non_stream),
            token_count: None,
        },
    );
}

#[cfg(test)]
mod tests;
