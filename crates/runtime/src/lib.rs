//! Execution core.
//!
//! - [`executor`]: the provider executor contract every upstream integration implements.
//! - `conductor`: credential selection, cooldown, retry and failover (Go sdk/cliproxy/auth).
//! - [`service`]: wiring config + auth store + registry into a running manager, auth synthesis,
//!   model registration and the `/v1/models` payloads.
//! - `usage`: usage accounting.
//! - [`pipeline`]: the SDK-facing execution context and hook contract (Go sdk/cliproxy/pipeline).

pub mod conductor;
pub mod executor;
pub mod pipeline;
pub mod service;
pub mod usage;
