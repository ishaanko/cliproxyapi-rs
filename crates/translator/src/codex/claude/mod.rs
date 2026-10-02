//! Port of internal/translator/codex/claude.

mod request;
mod response;
mod web_search;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::{convert_claude_request_to_codex, convert_claude_request_to_codex_with_compat};
pub use response::{
    claude_token_count, convert_codex_response_to_claude,
    convert_codex_response_to_claude_non_stream,
};

/// Go init(): Claude -> Codex.
pub fn register(r: &mut Registry) {
    r.register(
        Format::Claude,
        Format::Codex,
        Some(convert_claude_request_to_codex),
        ResponseFns {
            stream: Some(convert_codex_response_to_claude),
            non_stream: Some(convert_codex_response_to_claude_non_stream),
            token_count: Some(claude_token_count),
            finalize: None,
        },
    );
}

#[cfg(test)]
mod tests;
