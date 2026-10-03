//! BoringSSL based TLS ClientHello impersonation (see Cargo.toml for build requirements).
//!
//! Known gap: HTTP/2 pseudo-headers (Chrome profile, when ALPN picks `h2`) are written by the
//! `h2` crate as `:method, :scheme, :authority, :path`; Go sends `:authority, :method, :path,
//! :scheme`. The order is not configurable without forking `h2`.

pub mod client;
pub mod dial;
pub mod ordered;
pub mod profile;

pub use client::{ClientConfig, Error, FingerprintClient};
