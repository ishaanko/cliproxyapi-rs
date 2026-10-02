//! Base layer shared by translators, executors and the server.
//!
//! Each module mirrors one Go package from CLIProxyAPI:
//! `format` (sdk/translator formats + internal/constant), `misc`, `registry`, `util`,
//! `signature`, `cache`, `thinking`, `applypatch` (internal/client/codex/apply-patch).

pub mod applypatch;
pub mod cache;
pub mod format;
pub mod misc;
pub mod registry;
pub mod signature;
pub mod thinking;
pub mod util;
