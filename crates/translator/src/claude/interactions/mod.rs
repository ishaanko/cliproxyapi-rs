//! Port of internal/translator/claude/interactions: Interactions API clients served by a Claude
//! upstream.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_interactions_request_to_claude;
pub use response::{convert_claude_response_to_interactions, convert_claude_response_to_interactions_non_stream};

pub fn register(r: &mut Registry) {
    r.register(
        Format::Interactions,
        Format::Claude,
        Some(convert_interactions_request_to_claude),
        ResponseFns {
            stream: Some(convert_claude_response_to_interactions),
            non_stream: Some(convert_claude_response_to_interactions_non_stream),
            token_count: None,
        },
    );
}
