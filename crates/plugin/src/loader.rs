//! Native plugin loading through `dlopen` (Go: `loader_unix.go`, `host_callbacks_unix.go`).
//!
//! The C ABI is the one in `sdk/pluginabi`: the plugin exports `cliproxy_plugin_init`, which fills
//! a function table; the host passes a table whose `call` entry reaches [`HostCallbacks`]. All
//! unsafe code of the crate lives in `native`, which only exists on Unix.

use std::sync::Arc;

use crate::client::CallbackInstance;

/// Where plugin-initiated calls (`host.*` methods) are served. The return value is the response
/// envelope bytes (success or error); it is never empty.
pub trait HostCallbacks: Send + Sync {
    fn call_from_plugin(&self, plugin_id: &str, instance: &Arc<CallbackInstance>, method: &str, request: &[u8]) -> Vec<u8>;
}

#[cfg(unix)]
mod native;
#[cfg(unix)]
pub use native::DynClient;
