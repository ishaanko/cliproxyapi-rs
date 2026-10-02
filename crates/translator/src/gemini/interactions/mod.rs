//! Port of internal/translator/gemini/interactions.
//!
//! Registers three pairs: interactions->interactions (passthrough), interactions client with a
//! Gemini upstream, and Gemini client with an Interactions upstream.

mod gemini_response;
mod interactions_response;
mod request;
mod shared;

pub use gemini_response::{
    convert_gemini_response_to_interactions, convert_gemini_response_to_interactions_non_stream,
    convert_gemini_response_to_interactions_stream, StreamState,
};
pub use interactions_response::{
    convert_interactions_request_to_interactions, convert_interactions_response_passthrough,
    convert_interactions_response_passthrough_non_stream, convert_interactions_response_to_gemini,
    convert_interactions_response_to_gemini_non_stream,
};
pub use request::{convert_gemini_request_to_interactions, convert_interactions_request_to_gemini};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Interactions,
        Format::Interactions,
        Some(convert_interactions_request_to_interactions),
        ResponseFns {
            stream: Some(convert_interactions_response_passthrough),
            non_stream: Some(convert_interactions_response_passthrough_non_stream),
            token_count: None,
        },
    );
    r.register(
        Format::Interactions,
        Format::Gemini,
        Some(convert_interactions_request_to_gemini),
        ResponseFns {
            stream: Some(convert_gemini_response_to_interactions),
            non_stream: Some(convert_gemini_response_to_interactions_non_stream),
            token_count: None,
        },
    );
    r.register(
        Format::Gemini,
        Format::Interactions,
        Some(convert_gemini_request_to_interactions),
        ResponseFns {
            stream: Some(convert_interactions_response_to_gemini),
            non_stream: Some(convert_interactions_response_to_gemini_non_stream),
            token_count: None,
        },
    );
}
