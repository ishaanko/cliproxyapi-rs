//! Port of internal/translator/antigravity/claude: Claude Messages client -> Antigravity upstream.

mod request;
mod response;
mod signature_validation;
mod web_search;

pub use request::{convert_claude_request_to_antigravity, require_cached_thinking_signatures};
pub use response::{
    claude_token_count, convert_antigravity_response_to_claude,
    convert_antigravity_response_to_claude_non_stream,
};
pub use web_search::WEB_SEARCH_SYSTEM_INSTRUCTION;
pub use signature_validation::{
    strip_empty_signature_thinking_blocks, strip_invalid_bypass_signature_thinking_blocks,
    strip_invalid_gemini_signature_thinking_blocks, validate_claude_bypass_signatures,
};

use crate::registry::{Registry, ResponseFns};
use crate::Format;

pub fn register(r: &mut Registry) {
    r.register(
        Format::Claude,
        Format::Antigravity,
        Some(convert_claude_request_to_antigravity),
        ResponseFns {
            stream: Some(convert_antigravity_response_to_claude),
            non_stream: Some(convert_antigravity_response_to_claude_non_stream),
            token_count: Some(claude_token_count),
            finalize: None,
        },
    );
}

#[cfg(test)]
mod tests;
