//! Errors of the Home client (Go: the `Err*` variables and `DispatchError` in client.go).

use std::fmt;

/// Failure of a Home command. [`HomeError::Redis`] is an error *reply* from Home (the request
/// was processed); [`HomeError::Io`] and [`HomeError::Timeout`] are transport failures.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HomeError {
    #[error("home client disabled")]
    Disabled,
    #[error("home not connected")]
    NotConnected,
    #[error("home returned empty response")]
    EmptyResponse,
    #[error("home auth not found")]
    AuthNotFound,
    #[error("home config not found")]
    ConfigNotFound,
    #[error("home models not found")]
    ModelsNotFound,
    #[error("home plugin sync is unsupported: {0}")]
    PluginSyncUnsupported(String),
    #[error("home auth dispatch is fenced")]
    DispatchFenced,
    /// Home predates the `CAS` command.
    #[error("home compare-and-swap is unsupported")]
    CompareAndSwapUnsupported,
    /// Cluster discovery failed at the transport level (Go: `errClusterDiscoveryTransport`).
    #[error("home cluster discovery transport failed: {0}")]
    ClusterDiscoveryTransport(Box<HomeError>),
    /// An auth dispatch whose delivery result is unknown (Go: `DispatchError{Ambiguous: true}`).
    #[error("{0}")]
    AmbiguousDispatch(Box<HomeError>),
    /// Error reply from Home, without the leading `-` (e.g. `ERR unknown command 'cas'`).
    #[error("{0}")]
    Redis(String),
    #[error("{0}")]
    Io(String),
    #[error("i/o timeout")]
    Timeout,
    #[error("{0}")]
    Other(String),
}

impl HomeError {
    pub fn other(msg: impl fmt::Display) -> Self {
        HomeError::Other(msg.to_string())
    }

    /// Whether Home answered with an error reply (Go: `errors.As(err, &redis.Error)`).
    pub fn is_redis_reply(&self) -> bool {
        match self {
            HomeError::Redis(_) => true,
            HomeError::ClusterDiscoveryTransport(inner) | HomeError::AmbiguousDispatch(inner) => {
                inner.is_redis_reply()
            }
            _ => false,
        }
    }

    /// Go: `IsAmbiguousDispatchError`.
    pub fn is_ambiguous_dispatch(&self) -> bool {
        matches!(self, HomeError::AmbiguousDispatch(_))
    }

    /// Whether a transport timeout caused this error (Go: `isTimeoutError`).
    pub fn is_timeout(&self) -> bool {
        match self {
            HomeError::Timeout => true,
            HomeError::ClusterDiscoveryTransport(inner) | HomeError::AmbiguousDispatch(inner) => {
                inner.is_timeout()
            }
            _ => false,
        }
    }

    /// Go: `isHomeCommandUnsupported`: Home rejected a command it does not implement.
    pub fn is_command_unsupported(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message.contains("unknown command") || message.contains("unsupported command")
    }

    /// Go: `IsMembershipTakeoverUnavailableError`.
    pub fn is_membership_takeover_unavailable(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message == "membership_takeover_unavailable" || message == "err membership_takeover_unavailable"
    }

    /// Go: `IsLegacyMembershipProtocolError`.
    pub fn is_legacy_membership_protocol(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message == "wrong number of arguments for 'subscribe' command"
            || message == "err wrong number of arguments for 'subscribe' command"
    }
}

impl From<std::io::Error> for HomeError {
    fn from(e: std::io::Error) -> Self {
        HomeError::Io(e.to_string())
    }
}
