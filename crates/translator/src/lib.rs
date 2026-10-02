//! Protocol translators between client dialects and upstream provider dialects.
//!
//! Module layout mirrors Go's `internal/translator/<upstream>/<client>` packages. Each
//! leaf module exposes `register(&mut Registry)`, called once from [`registry::global`].
//!
//! Direction conventions (same as Go):
//! - requests: `translate_request(client, upstream, ..)`
//! - responses: `translate_stream(upstream, client, ..)`, one raw upstream line per call,
//!   with a [`Param`] carrying per-stream state across calls.

pub mod common;
pub mod registry;

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod interactions;
pub mod openai;

pub use cpa_core::format::Format;
pub use registry::{
    global, translate_finalize, translate_non_stream, translate_request, translate_request_envelope, translate_stream,
    translate_token_count, Ctx, Param, Registry, RequestEnvelope,
};
