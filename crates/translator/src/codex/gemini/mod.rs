//! Port of internal/translator/codex/gemini.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_gemini_request_to_codex;
pub use response::{
    convert_codex_response_to_gemini, convert_codex_response_to_gemini_non_stream,
    gemini_token_count,
};

/// Go init(): Gemini -> Codex.
pub fn register(r: &mut Registry) {
    r.register(
        Format::Gemini,
        Format::Codex,
        Some(convert_gemini_request_to_codex),
        ResponseFns {
            stream: Some(convert_codex_response_to_gemini),
            non_stream: Some(convert_codex_response_to_gemini_non_stream),
            token_count: Some(gemini_token_count),
        },
    );
}
