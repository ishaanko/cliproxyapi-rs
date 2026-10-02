//! Port of internal/translator/codex/openai/responses.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_openai_responses_request_to_codex;
pub use response::{
    convert_codex_response_to_openai_responses,
    convert_codex_response_to_openai_responses_non_stream,
};

/// Go init(): OpenaiResponse -> Codex (no token count transform).
pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::Codex,
        Some(convert_openai_responses_request_to_codex),
        ResponseFns {
            stream: Some(convert_codex_response_to_openai_responses),
            non_stream: Some(convert_codex_response_to_openai_responses_non_stream),
            token_count: None,
            finalize: None,
        },
    );
}

#[cfg(test)]
mod tests;
