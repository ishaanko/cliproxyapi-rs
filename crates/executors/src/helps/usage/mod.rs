//! Usage accounting for executors (Go: sdk/cliproxy/usage accounting + helps/usage_helpers.go).
//!
//! - [`accounting`]: `Detail` and the canonical token breakdown.
//! - [`parse`]: usage extraction per wire format, stream buffer and merge, SSE usage filtering.
//! - [`reporter`]: the per-attempt `UsageReporter` (TTFT, served model, publishing).

pub mod accounting;
pub mod parse;
pub mod reporter;

pub use accounting::*;
pub use parse::*;
pub use reporter::*;
