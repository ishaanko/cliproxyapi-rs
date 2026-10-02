//! Port of internal/translator/antigravity/openai/responses: OpenAI Responses client ->
//! Antigravity upstream.

mod request;
mod response;

pub use request::{
    convert_openai_responses_request_envelope_to_antigravity, convert_openai_responses_request_to_antigravity,
};
pub use response::{
    convert_antigravity_response_to_openai_responses, convert_antigravity_response_to_openai_responses_non_stream,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::Antigravity,
        Some(convert_openai_responses_request_to_antigravity),
        ResponseFns {
            stream: Some(convert_antigravity_response_to_openai_responses),
            non_stream: Some(convert_antigravity_response_to_openai_responses_non_stream),
            token_count: None,
        },
    );
    // The envelope variant overrides the plain request transform to see request-scoped ModelInfo.
    r.register_request_envelope(
        Format::OpenAIResponse,
        Format::Antigravity,
        convert_openai_responses_request_envelope_to_antigravity,
    );
}
