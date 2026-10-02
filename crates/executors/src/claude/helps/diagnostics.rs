//! (skeleton) Go: helps/claude_diagnostics.go.

/// Go: `ClaudeContinuityContext`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeContinuityContext {
    pub key: String,
    pub sequence: u64,
    pub previous_message_id: String,
    pub previous_request_id: String,
    pub prompt_id: String,
    pub initialized: bool,
}
