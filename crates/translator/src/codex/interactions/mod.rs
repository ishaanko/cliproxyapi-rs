//! Port of internal/translator/codex/interactions.

mod request;
mod response;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_interactions_request_to_codex;
pub use response::{convert_codex_response_to_interactions, convert_codex_response_to_interactions_non_stream};

/// Go init(): Interactions -> Codex (no token count transform).
pub fn register(r: &mut Registry) {
    r.register(
        Format::Interactions,
        Format::Codex,
        Some(convert_interactions_request_to_codex),
        ResponseFns {
            stream: Some(convert_codex_response_to_interactions),
            non_stream: Some(convert_codex_response_to_interactions_non_stream),
            token_count: None,
        },
    );
}
