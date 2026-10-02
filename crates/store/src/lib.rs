//! Remote-backed token stores (Go: internal/store). Each backend keeps config and credentials in
//! a remote system (Postgres, an object bucket, a repository remote) and mirrors them into a local
//! spool directory, so the watcher and file-based flows run unchanged on the spool.

mod common;
mod pgconn;
pub mod postgres;
mod postgres_cooldown;
mod rt;

pub use common::copy_config_template;
pub use postgres::{PostgresStore, PostgresStoreConfig};
