//! Error returned by host-side callback handlers; it becomes a `host_call_failed` envelope whose
//! `http_status` is the status the failure carries (Go: `clienterror.HTTPStatusFromError`).

use std::fmt;

/// Go `StatusClientClosedRequest`.
pub const STATUS_CLIENT_CLOSED_REQUEST: i32 = 499;

#[derive(Debug, Clone, Default)]
pub struct HostError {
    pub message: String,
    pub status: i32,
}

impl HostError {
    pub fn msg(message: impl Into<String>) -> Self {
        HostError { message: message.into(), status: 0 }
    }

    pub fn with_status(message: impl Into<String>, status: i32) -> Self {
        HostError { message: message.into(), status }
    }

    /// `context.Canceled` (499 like Go's status mapping).
    pub fn canceled() -> Self {
        HostError { message: "context canceled".into(), status: STATUS_CLIENT_CLOSED_REQUEST }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

impl From<crate::client::PluginError> for HostError {
    fn from(e: crate::client::PluginError) -> Self {
        HostError { message: e.message, status: e.status }
    }
}
