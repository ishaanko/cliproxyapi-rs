use std::io;

/// Errors from loading, validating or saving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A validation or layout error. The message is the same text the Go implementation reports.
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),
}

impl ConfigError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io { context: context.into(), source }
    }

    /// The underlying I/O error kind, when this is an I/O failure.
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Io { source, .. } => Some(source.kind()),
            _ => None,
        }
    }
}

pub type Result<T, E = ConfigError> = std::result::Result<T, E>;
