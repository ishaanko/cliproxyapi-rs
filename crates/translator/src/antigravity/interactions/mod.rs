//! Port of internal/translator/antigravity/interactions: Interactions client -> Antigravity upstream.

mod request;
mod response;

pub use request::convert_interactions_request_to_antigravity;
pub use response::{
    convert_antigravity_response_to_interactions, convert_antigravity_response_to_interactions_non_stream,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Interactions,
        Format::Antigravity,
        Some(convert_interactions_request_to_antigravity),
        ResponseFns {
            stream: Some(convert_antigravity_response_to_interactions),
            non_stream: Some(convert_antigravity_response_to_interactions_non_stream),
            token_count: None,
            finalize: None,
        },
    );
}
