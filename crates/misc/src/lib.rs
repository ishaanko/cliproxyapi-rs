//! Small standalone helpers ported from CLIProxyAPI.
//!
//! `internal/httpwire` lives in `cpa-tlsfp` and `internal/htmlsanitize` in the plugin host, which
//! are their only users.

pub mod grokbuild;
pub mod httpfetch;
pub mod proxyutil;
