//! Claude-specific helpers (Go: internal/runtime/executor/helps/claude_*.go, cloak_obfuscate.go).
//! The provider-independent helpers live in `crate::helps`.

use std::sync::Arc;

use http::HeaderMap;
use parking_lot::Mutex;

pub mod builtin_tools;
pub mod cli_identity_seed;
pub mod client_detection;
pub mod cloak_obfuscate;
pub mod code_session;
pub mod credential_identity;
pub mod device_profile;
pub mod diagnostics;
pub mod mcp_alias;
pub mod ratelimit;
pub mod ttft_helpers;
pub mod upstream;

pub use diagnostics::ClaudeContinuityContext;

/// Request-scoped values Go threads through `context.Context` (`WithIncomingHeaders`,
/// `WithClaudeSessionID`, `WithClaudeContinuityContext`, `WithClaudeExecutionMetadata`). Rust
/// passes this explicitly wherever a Go helper takes `ctx` and reads one of those values.
#[derive(Clone, Default)]
pub struct ClaudeCtx {
    /// Go: `IncomingHeadersFromContext` (client headers, possibly session-augmented).
    pub incoming_headers: Option<HeaderMap>,
    /// Go: `ClaudeSessionIDFromContext` ("" when unset).
    pub session_id: String,
    /// Go: `ClaudeExecutionMetadataFromContext`.
    pub execution_metadata: bool,
    /// Go: `ClaudeContinuityContextFromContext` (shared mutable pointer; `None` when absent).
    pub continuity: Option<Arc<Mutex<ClaudeContinuityContext>>>,
}
