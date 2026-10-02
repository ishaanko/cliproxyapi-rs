//! HTTP server of the proxy: routes, middleware, per-dialect handlers (OpenAI chat/completions,
//! Responses incl. websocket, Claude messages, Gemini, model lists), client API-key auth, request
//! logging and the CLI.
//!
//! Wiring contract: build an [`AppState`] (config receiver, `Manager`, auth store, OAuth sessions,
//! usage tracker, optional model catalog) and call [`build_router`] /
//! [`build_router_with_management`], then [`serve::serve`].
//!
//! Not ported: realtime/live endpoints, image and video generation (501), `/v1/alpha/search`,
//! the Redis protocol multiplexer, Home mode, TUI, pprof, plugins, the AI Studio `/v1/ws` relay.

// `ErrorMessage` mirrors Go's `interfaces.ErrorMessage` (status, text, headers); errors are the rare path.
#![allow(clippy::result_large_err)]

pub mod access;
pub mod body;
pub mod bodytee;
pub mod cli;
pub mod clientip;
pub mod codex_models;
pub mod error;
pub mod exec;
pub mod forward;
pub mod handlers;
pub mod headers;
pub mod logging;
pub mod middleware;
pub mod models;
pub mod reply;
pub mod req;
pub mod reqlog;
pub mod responses_error;
pub mod responses_framer;
pub mod router;
pub mod safemode;
pub mod serve;
pub mod sse_validate;
pub mod state;
pub mod thinking;
pub mod ui;
pub mod ws;

pub use router::{apply_global_layers, build_router, build_router_with_management};
pub use state::{AppState, BuildInfo, KeepAlive};
