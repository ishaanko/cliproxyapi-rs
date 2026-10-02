//! Port of internal/translator/common (shared helpers used by every translator).
//!
//! Everything is re-exported flat, like the single Go package.
//!
//! Conventions (same as `cpa_core::util`): Go `[]byte` JSON bodies are `&[u8]` in and `Vec<u8>`
//! out (edit helpers re-serialize compactly; key order is preserved). Raw JSON item lists
//! (`[][]byte`) are `Vec<Vec<u8>>` / `&[Vec<u8>]`. A `gjson.Result` parameter is a `&Res<'_>` (get
//! one with `value.g("path")`, or `Res::of(&value)` for a whole document); absent results are
//! `Res::NONE`. Go `error` returns are `String` messages with Go's text.
//!
//! Known gaps against Go, inherent to the `Value` based JSON model: raw sub-values are
//! re-serialized compactly rather than copied byte for byte; duplicate object keys collapse;
//! `SetStringWithoutHTMLEscape` cannot emit the ` `/` ` escapes Go's encoder adds.

mod antigravity_tools;
mod apply_patch;
mod bytes;
mod cache_control;
mod claude_messages;
mod claude_system;
mod claude_user_id;
mod devin_tools;
mod file_data;
mod gemini;
mod interactions_usage;
mod openai_tools;
mod raw;
mod request;
mod responses;

pub use antigravity_tools::*;
pub use apply_patch::*;
pub use bytes::*;
pub use cache_control::*;
pub use claude_messages::*;
pub use claude_system::*;
pub use claude_user_id::*;
pub use devin_tools::*;
pub use file_data::*;
pub use gemini::*;
pub use interactions_usage::*;
pub use openai_tools::*;
pub use raw::*;
pub use request::*;
pub use responses::*;

#[cfg(test)]
mod tests;
