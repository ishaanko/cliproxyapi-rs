//! Execution core.
//!
//! - [`executor`]: the provider executor contract every upstream integration implements.
//! - `conductor`: credential selection, cooldown, retry and failover (Go sdk/cliproxy/auth).
//! - [`service`]: wiring config + auth store + registry into a running manager, auth synthesis,
//!   model registration and the `/v1/models` payloads.
//! - `usage`: usage accounting.

// `ExecError` carries the upstream status, headers and body (Go: `*Error`); errors are the rare path.
#![allow(clippy::result_large_err, clippy::large_enum_variant)]

pub mod apilog;
pub mod conductor;
pub mod executor;
pub mod service;
pub mod usage;
pub mod usage_queue;
