//! Port of internal/translator/gemini/gemini (Gemini client, Gemini upstream: request
//! normalizer plus passthrough responses).

mod request;
mod response;

pub use request::convert_gemini_request_to_gemini;
pub use response::{
    gemini_token_count, passthrough_gemini_response_non_stream, passthrough_gemini_response_stream,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Gemini,
        Format::Gemini,
        Some(convert_gemini_request_to_gemini),
        ResponseFns {
            stream: Some(passthrough_gemini_response_stream),
            non_stream: Some(passthrough_gemini_response_non_stream),
            token_count: Some(gemini_token_count),
        },
    );
}
