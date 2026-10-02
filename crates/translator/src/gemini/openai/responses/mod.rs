//! Port of internal/translator/gemini/openai/responses: OpenAI Responses clients, Gemini upstream.
//!
//! The request converter, both response converters, the stream finalizer and the web search
//! helpers are public because the antigravity Responses translator reuses them.

mod function_evidence;
mod lenient;
mod media;
mod request;
mod response;
mod response_nonstream;
mod signature_carrier;
mod trailing_signature;
mod web_search;

use cpa_core::format::Format;

use crate::registry::{Registry, ResponseFns};

pub use request::convert_openai_responses_request_to_gemini;
pub use response::{convert_gemini_response_to_openai_responses, finalize_tool_input, GeminiToResponsesState};
pub use response_nonstream::convert_gemini_response_to_openai_responses_non_stream;
pub use web_search::{
    allows_responses_web_search_tool_choice, build_responses_url_citations, build_responses_url_citations_for_messages,
    build_responses_web_search_call_item, extract_grounding_metadata, extract_grounding_queries, extract_grounding_sources,
    extract_responses_web_search_allowed_domains, extract_responses_web_search_query, has_only_responses_web_search_tools,
    has_responses_web_search_tool, has_valid_web_grounding, merge_citation_annotations, merge_grounding_metadata,
    model_supports_web_search, GeminiPartMapping, MessageRuneRange,
};

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::Gemini,
        Some(convert_openai_responses_request_to_gemini),
        ResponseFns {
            stream: Some(convert_gemini_response_to_openai_responses),
            non_stream: Some(convert_gemini_response_to_openai_responses_non_stream),
            token_count: None,
        },
    );
}

#[cfg(test)]
mod tests;
