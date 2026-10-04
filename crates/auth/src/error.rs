//! Error types for login and refresh flows (internal/auth/{claude,codex}/errors.go and the ad hoc
//! `fmt.Errorf` errors of the other providers).

use std::fmt;

/// The fixed `AuthenticationError` kinds shared by the Claude and Codex flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthErrorKind {
    InvalidState,
    CodeExchangeFailed,
    ServerStartFailed,
    PortInUse,
    CallbackTimeout,
}

impl AuthErrorKind {
    /// `Type` string of the Go error.
    pub fn type_name(self) -> &'static str {
        match self {
            AuthErrorKind::InvalidState => "invalid_state",
            AuthErrorKind::CodeExchangeFailed => "code_exchange_failed",
            AuthErrorKind::ServerStartFailed => "server_start_failed",
            AuthErrorKind::PortInUse => "port_in_use",
            AuthErrorKind::CallbackTimeout => "callback_timeout",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            AuthErrorKind::InvalidState => "OAuth state parameter is invalid",
            AuthErrorKind::CodeExchangeFailed => "Failed to exchange authorization code for tokens",
            AuthErrorKind::ServerStartFailed => "Failed to start OAuth callback server",
            AuthErrorKind::PortInUse => "OAuth callback port is already in use",
            AuthErrorKind::CallbackTimeout => "Timeout waiting for OAuth callback",
        }
    }

    /// `Code` of the Go error (HTTP-like; 13 for port-in-use).
    pub fn code(self) -> u16 {
        match self {
            AuthErrorKind::InvalidState | AuthErrorKind::CodeExchangeFailed => 400,
            AuthErrorKind::ServerStartFailed => 500,
            AuthErrorKind::PortInUse => 13,
            AuthErrorKind::CallbackTimeout => 408,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthFlowError {
    /// `AuthenticationError`: `"<type>: <message> (caused by: <cause>)"`.
    #[error("{}", format_authentication(*.kind, .cause))]
    Authentication {
        kind: AuthErrorKind,
        cause: Option<String>,
    },
    /// `OAuthError` returned by the provider (`error` / `error_description`).
    #[error("{}", format_oauth(.code, .description))]
    OAuth { code: String, description: String },
    /// Non-2xx from a provider endpoint (Antigravity `HTTPStatusError`, token endpoints).
    /// `retry_after` carries a server-provided 429 delay when one was parsed.
    #[error("{message}")]
    Status {
        status: u16,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// Claude refresh failure carrying retry semantics (`refreshHTTPError`).
    #[error("token refresh failed with status {status}: {message}")]
    Refresh {
        status: u16,
        message: String,
        retryable: bool,
    },
    /// A refresh that failed on every allowed attempt (`token refresh failed after N attempts: %w`);
    /// status, retry hint and retryability are those of the last error.
    #[error("token refresh failed after {attempts} attempts: {source}")]
    RetriesExhausted {
        attempts: u32,
        source: Box<AuthFlowError>,
    },
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Other(String),
    #[error("login cancelled")]
    Cancelled,
    /// Network-level failure (connect, TLS, timeout, body read).
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Storage(String),
}

impl From<reqwest::Error> for AuthFlowError {
    fn from(e: reqwest::Error) -> Self {
        // Strip the URL so tokens in query strings never reach logs or API responses.
        AuthFlowError::Transport(e.without_url().to_string())
    }
}

impl From<crate::storage::StorageError> for AuthFlowError {
    fn from(e: crate::storage::StorageError) -> Self {
        AuthFlowError::Storage(e.to_string())
    }
}

fn format_authentication(kind: AuthErrorKind, cause: &Option<String>) -> String {
    match cause {
        Some(c) => format!(
            "{}: {} (caused by: {})",
            kind.type_name(),
            kind.message(),
            c
        ),
        None => format!("{}: {}", kind.type_name(), kind.message()),
    }
}

fn format_oauth(code: &str, description: &str) -> String {
    if description.is_empty() {
        format!("OAuth error: {code}")
    } else {
        format!("OAuth error {code}: {description}")
    }
}

impl AuthFlowError {
    pub fn authentication(kind: AuthErrorKind, cause: impl fmt::Display) -> Self {
        AuthFlowError::Authentication {
            kind,
            cause: Some(cause.to_string()),
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        AuthFlowError::Other(msg.into())
    }

    /// A non-2xx response without a retry hint.
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        AuthFlowError::Status {
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Server-requested retry delay (`retryAfter` of Go's status errors), if any.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            AuthFlowError::Status { retry_after, .. } => *retry_after,
            AuthFlowError::RetriesExhausted { source, .. } => source.retry_after(),
            _ => None,
        }
    }

    /// HTTP status carried by the error, when it has one (`StatusCode()` in Go).
    pub fn status_code(&self) -> Option<u16> {
        match self {
            AuthFlowError::Status { status, .. } | AuthFlowError::Refresh { status, .. } => {
                Some(*status)
            }
            AuthFlowError::RetriesExhausted { source, .. } => source.status_code(),
            _ => None,
        }
    }

    /// Whether retrying the same refresh might succeed (Claude: retryable HTTP responses only; transport and decode errors are ambiguous because the single-use refresh token may already be consumed).
    pub fn is_retryable_refresh(&self) -> bool {
        match self {
            AuthFlowError::Refresh { retryable, .. } => *retryable,
            AuthFlowError::RetriesExhausted { source, .. } => source.is_retryable_refresh(),
            _ => false,
        }
    }

    /// `GetUserFriendlyMessage`.
    pub fn user_friendly_message(&self) -> String {
        match self {
            AuthFlowError::Authentication { kind, .. } => match kind {
                AuthErrorKind::PortInUse => {
                    "The required port is already in use. Please close any applications using port 3000 and try again."
                }
                AuthErrorKind::CallbackTimeout => "Authentication timed out. Please try again.",
                _ => "Authentication failed. Please try again.",
            }
            .to_string(),
            AuthFlowError::OAuth { code, description } => match code.as_str() {
                "access_denied" => "Authentication was cancelled or denied.".to_string(),
                "invalid_request" => "Invalid authentication request. Please try again.".to_string(),
                "server_error" => "Authentication server error. Please try again later.".to_string(),
                _ => format!("Authentication failed: {description}"),
            },
            _ => "An unexpected error occurred. Please try again.".to_string(),
        }
    }
}

pub type Result<T, E = AuthFlowError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_go_formats() {
        let e = AuthFlowError::authentication(AuthErrorKind::CodeExchangeFailed, "boom");
        assert_eq!(
            e.to_string(),
            "code_exchange_failed: Failed to exchange authorization code for tokens (caused by: boom)"
        );
        let e = AuthFlowError::OAuth {
            code: "access_denied".into(),
            description: "".into(),
        };
        assert_eq!(e.to_string(), "OAuth error: access_denied");
        assert_eq!(
            e.user_friendly_message(),
            "Authentication was cancelled or denied."
        );
    }
}
