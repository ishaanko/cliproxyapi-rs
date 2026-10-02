//! Token-event detection for Gemini and Antigravity streaming chunks (Go: helps/gemini_ttft_helpers.go).

use cpa_json::J;

use crate::helps::text::trim_space;
use crate::helps::usage::UsageReporter;

/// Whether a streaming chunk (raw JSON or an SSE `data:` line) carries substantive output: text,
/// thought text, a function call, inline data, an error, or a terminal `finishReason`. Metadata-only
/// chunks such as a standalone `usageMetadata` do not count.
pub fn is_gemini_token_event(payload: &[u8]) -> bool {
    let mut payload = trim_space(payload);
    if payload.is_empty() {
        return false;
    }
    if payload.starts_with(b"data:") {
        let prefix = if payload.starts_with(b"data: ") { 6 } else { 5 };
        payload = trim_space(&payload[prefix..]);
        if payload.is_empty() {
            return false;
        }
    }
    let v = cpa_json::parse(payload);
    if v.g("error.message").exists() || v.g("error").exists() || v.g("response.error").exists() {
        return true;
    }
    let mut candidates = v.g("candidates");
    if !candidates.exists() {
        candidates = v.g("response.candidates");
    }
    for candidate in candidates.array() {
        for part in candidate.g("content.parts").array() {
            if !part.g("text").str().is_empty()
                || !part.g("thoughtText").str().is_empty()
                || part.g("thought").as_str().is_some_and(|s| !s.is_empty())
                || !part.g("functionCall.name").str().is_empty()
                || !part.g("inlineData.data").str().is_empty()
            {
                return true;
            }
        }
        if !candidate.g("finishReason").str().is_empty() {
            return true;
        }
    }
    false
}

/// Records the served model and the first-token TTFT mark for a streaming chunk; a no-op for the
/// token mark once effective TTFT is set (Go: ObserveGeminiTokenEvent).
pub fn observe_gemini_token_event(reporter: &UsageReporter, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reporter.observe_response_model(payload);
    if reporter.is_ttft_set() {
        return;
    }
    reporter.observe_token_event(is_gemini_token_event(payload));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_events() {
        assert!(is_gemini_token_event(br#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#));
        assert!(is_gemini_token_event(br#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f"}}]}}]}}"#));
        assert!(is_gemini_token_event(br#"{"candidates":[{"finishReason":"STOP"}]}"#));
        assert!(is_gemini_token_event(br#"{"error":{"message":"x"}}"#));
        assert!(!is_gemini_token_event(br#"{"usageMetadata":{"totalTokenCount":3}}"#));
        assert!(!is_gemini_token_event(br#"{"candidates":[{"content":{"parts":[{"text":""}]}}]}"#));
        assert!(!is_gemini_token_event(b"data:"));
    }
}
