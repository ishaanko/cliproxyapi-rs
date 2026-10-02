//! Port of internal/translator/openai/gemini (Gemini client -> OpenAI Chat Completions upstream).

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_gemini_request_to_openai;
pub use response::{convert_openai_response_to_gemini, convert_openai_response_to_gemini_non_stream, gemini_token_count};

pub fn register(r: &mut Registry) {
    r.register(
        Format::Gemini,
        Format::OpenAI,
        Some(convert_gemini_request_to_openai),
        ResponseFns {
            stream: Some(convert_openai_response_to_gemini),
            non_stream: Some(convert_openai_response_to_gemini_non_stream),
            token_count: Some(gemini_token_count),
        },
    );
}
