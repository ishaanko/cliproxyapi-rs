//! Remote-backed token stores (Go: internal/store). Each backend keeps config and credentials in
//! a remote system (Postgres, an object bucket, a repository remote) and mirrors them into a local
//! spool directory, so the watcher and file-based flows run unchanged on the spool.

mod common;
pub mod object;
pub mod gitstore;
mod pgconn;
pub mod postgres;
mod postgres_cooldown;
mod rt;
mod s3;
mod select;

pub use common::copy_config_template;
pub use object::{ObjectStoreConfig, ObjectTokenStore};
pub use select::{OpenedStore, open_from_env};
pub use gitstore::GitTokenStore;
pub use postgres::{PostgresStore, PostgresStoreConfig};
