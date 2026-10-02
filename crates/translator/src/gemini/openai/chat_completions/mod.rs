//! Port of internal/translator/gemini/openai/chat-completions (OpenAI Chat Completions client,
//! Gemini upstream). The non-stream response converter is shared with the antigravity chat
//! translator.

mod request;
mod response;

pub use request::convert_openai_request_to_gemini;
pub use response::{convert_gemini_response_to_openai, convert_gemini_response_to_openai_non_stream};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::Gemini,
        Some(convert_openai_request_to_gemini),
        ResponseFns {
            stream: Some(convert_gemini_response_to_openai),
            non_stream: Some(convert_gemini_response_to_openai_non_stream),
            token_count: None,
            finalize: None,
        },
    );
}
