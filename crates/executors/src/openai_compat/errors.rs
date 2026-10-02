//! Transport failure rendering shared by the OpenAI-compatible and xAI executors.
//!
//! The conductor classifies status-less failures by message (Go: `isTransientTransportMessage`),
//! so the text must carry the underlying cause the way Go's `net` errors do: a truncated body
//! reads `unexpected EOF`, a refused connection keeps `connection refused`.

use cpa_runtime::executor::ExecError;

/// The error and its sources joined with `: `.
fn chain_text(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = s.source();
    }
    text
}

/// Go-style message of a failed request or body read.
pub fn transport_message(err: &reqwest::Error) -> String {
    let text = chain_text(err);
    let lower = text.to_lowercase();
    if err.is_body() || err.is_decode() {
        let truncated = lower.contains("connection closed before message completed")
            || lower.contains("unexpected eof")
            || lower.contains("end of file")
            || lower.contains("incomplete");
        if truncated {
            return "unexpected EOF".to_string();
        }
    }
    text
}

/// Status-less [`ExecError`] for a failed request or body read.
pub fn transport_error(err: &reqwest::Error) -> ExecError {
    ExecError::new(0, transport_message(err))
}
