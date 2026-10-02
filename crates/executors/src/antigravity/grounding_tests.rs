//! Grounding URL gating and pass-through behavior (Go: antigravity_executor_buildrequest_test.go).

use cpa_translator::Format;

use super::grounding::{resolve_grounding_urls, should_resolve_grounding_urls};

const SEARCH_REQUEST: &[u8] = br#"{"request":{"tools":[{"googleSearch":{}}]}}"#;

#[test]
fn grounding_resolution_needs_a_typed_web_search_tool_and_a_search_request() {
    let claude_typed = br#"{"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#;
    let claude_plain = br#"{"tools":[{"name":"bash","input_schema":{}}]}"#;
    let responses_typed = br#"{"tools":[{"type":"web_search_preview"}]}"#;
    assert!(should_resolve_grounding_urls(Format::Claude, claude_typed, SEARCH_REQUEST));
    assert!(should_resolve_grounding_urls(Format::OpenAIResponse, responses_typed, SEARCH_REQUEST));
    assert!(!should_resolve_grounding_urls(Format::Claude, claude_plain, SEARCH_REQUEST));
    assert!(!should_resolve_grounding_urls(Format::Claude, claude_typed, br#"{"request":{"tools":[]}}"#));
    assert!(!should_resolve_grounding_urls(Format::OpenAI, responses_typed, SEARCH_REQUEST));
    assert!(!should_resolve_grounding_urls(Format::Gemini, claude_typed, SEARCH_REQUEST));
}

#[tokio::test]
async fn non_redirect_grounding_urls_are_left_alone() {
    let body = br#"{"response":{"candidates":[{"groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://example.com/a"}},{"web":{}}]}}]}}"#.to_vec();
    assert_eq!(resolve_grounding_urls("", body.clone()).await, body);
    assert_eq!(resolve_grounding_urls("", b"{}".to_vec()).await, b"{}".to_vec());
}
