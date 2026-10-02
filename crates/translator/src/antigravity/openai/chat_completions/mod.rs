//! Port of internal/translator/antigravity/openai/chat-completions: OpenAI Chat Completions
//! client -> Antigravity upstream.

mod request;
mod response;

pub use request::convert_openai_request_to_antigravity;
pub use response::{convert_antigravity_response_to_openai, convert_antigravity_response_to_openai_non_stream};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::Antigravity,
        Some(convert_openai_request_to_antigravity),
        ResponseFns {
            stream: Some(convert_antigravity_response_to_openai),
            non_stream: Some(convert_antigravity_response_to_openai_non_stream),
            token_count: None,
        },
    );
}
