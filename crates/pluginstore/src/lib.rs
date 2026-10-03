//! Plugin store: registry fetch, GitHub release / direct artifact install, store auth and
//! the CLIProxyAPIHome plugin sync/delete reports.
//!
//! Ported from Go `internal/pluginstore`, `sdk/pluginstore` and `internal/homeplugins`.
//! The embedder-facing surface lives in [`sdk`]; the Home sync/delete orchestration in
//! [`homeplugins`].

pub mod auth;
pub mod checksum;
pub mod direct;
pub mod error;
pub mod github;
mod gotime;
pub mod goturl;
pub mod home_sync;
pub mod homeplugins;
pub mod http;
pub mod install;
pub mod manifest;
pub mod ratelimit;
pub mod registry;
pub mod request_identity;
pub mod sdk;
pub mod version;

pub use error::{Context, Error, Result};
pub use github::{Client, Release, ReleaseAsset, release_version};
pub use install::{InstallOptions, InstallResult};
pub use manifest::Manifest;
pub use registry::*;

#[cfg(test)]
mod testutil;
