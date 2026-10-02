//! Port of internal/translator/codex/openai/chat-completions.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_openai_request_to_codex;
pub use response::{convert_codex_response_to_openai, convert_codex_response_to_openai_non_stream};

/// Go init(): OpenAI -> Codex (no token count transform).
pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::Codex,
        Some(convert_openai_request_to_codex),
        ResponseFns {
            stream: Some(convert_codex_response_to_openai),
            non_stream: Some(convert_codex_response_to_openai_non_stream),
            token_count: None,
        },
    );
}
