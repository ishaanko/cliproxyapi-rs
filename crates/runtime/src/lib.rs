//! Execution core.
//!
//! - [`executor`]: the provider executor contract every upstream integration implements.
//! - `conductor`: credential selection, cooldown, retry and failover (Go sdk/cliproxy/auth).
//! - `service`: wiring config + auth store + registry into a running manager.
//! - `usage`: usage accounting.

pub mod conductor;
pub mod executor;
pub mod usage;
