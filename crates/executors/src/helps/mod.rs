//! Provider-independent executor helpers (Go: internal/runtime/executor/helps plus the shared
//! helpers of internal/runtime/executor/*.go).
//!
//! - HTTP: [`proxy`] (proxy-aware cached reqwest clients), [`sse`] (`bufio.Scanner` style line
//!   reader), [`status`] (upstream non-2xx to `ExecError`, `Retry-After`).
//! - Payload: [`payload`] (config payload rules), [`codex_tool_integers`], [`openai_compat`]
//!   (max tokens, tool results), [`openai_responses_signature`], [`responses_usage`].
//! - Thinking: [`thinking`] (glue over `cpa_core::thinking`).
//! - Usage: [`usage`] (accounting, per-format parsing, reporter), [`token_count`] (tiktoken),
//!   [`response_model`] and [`stream_response_model_observer`], [`ttft`].
//! - Observability: [`logging`] (request/response capture), [`websocket_observer`].
//! - Identity: [`id_cache`] (session/user ids, Codex prompt cache), [`session`], [`oauth_scope`].
//! - Codex apply_patch bridge: [`apply_patch`], [`apply_patch_responses`].
//!
//! Provider-specific helps (claude_*, codex_*, antigravity_*, gemini_*, devin_*, kimi_*, meta_*,
//! vertex_*, cloak_*, utls_client, home_refresh, plugin_executor_usage) belong to the provider
//! modules.

pub mod apply_patch;
pub mod apply_patch_responses;
pub mod codex_tool_integers;
pub mod id_cache;
pub mod json_retry;
pub mod logging;
pub mod oauth_scope;
pub mod openai_compat;
pub mod openai_responses_signature;
pub mod payload;
pub mod proxy;
pub mod response_model;
pub mod responses_usage;
pub mod session;
pub mod sse;
pub mod status;
pub mod stream_response_model_observer;
pub mod text;
pub mod thinking;
pub mod token_count;
pub mod ttft;
pub mod usage;
pub mod websocket_observer;
