//! Port of internal/translator/interactions/claude: Claude clients served by an Interactions
//! upstream.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::{convert_claude_request_to_interactions, convert_claude_request_to_interactions_with_compat};
pub use response::{convert_interactions_response_to_claude, convert_interactions_response_to_claude_non_stream};

pub fn register(r: &mut Registry) {
    r.register(
        Format::Claude,
        Format::Interactions,
        Some(convert_claude_request_to_interactions),
        ResponseFns {
            stream: Some(convert_interactions_response_to_claude),
            non_stream: Some(convert_interactions_response_to_claude_non_stream),
            token_count: None,
        },
    );
}
