//! Plugin ABI and API payloads (Go: sdk/pluginabi, sdk/pluginapi).
//!
//! [`abi`] holds the version constants, RPC method names and the response envelope; [`api`] holds
//! every JSON payload exchanged with plugins, encoded exactly like Go's `encoding/json` ([`wire`]).

pub mod abi;
pub mod api;
pub mod host_api;
pub mod wire;
