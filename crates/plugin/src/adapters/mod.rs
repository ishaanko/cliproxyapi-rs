//! Adapters that turn plugin capabilities into host hooks (Go: `adapters*.go`).

pub mod access;
pub mod auth_provider;
pub mod executors;
pub mod interceptors;
pub mod models;
pub mod quota;
pub mod refresh_compat;
pub mod routing;
pub mod translation;
