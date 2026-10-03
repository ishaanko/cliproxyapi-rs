//! Error type and cancellation context shared by the plugin store modules.
//!
//! Go wraps errors with `%w` and later probes them with `errors.Is`/`errors.As`; [`Error`]
//! keeps the same shape: a wrapped chain whose Display matches Go's `"ctx: inner"` text.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use crate::ratelimit::RateLimitError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub enum Error {
    /// Plain message (Go `fmt.Errorf` without `%w`).
    Msg(String),
    /// GitHub API cooldown.
    RateLimit(RateLimitError),
    /// Go `ErrLoadedPluginLocked`.
    LoadedPluginLocked,
    /// Go `context.Canceled`.
    Canceled,
    /// `fmt.Errorf("<context>: %w", source)`.
    Wrap(String, Box<Error>),
    /// `errors.Join`: messages separated by newlines.
    Join(Vec<Error>),
}

pub const LOADED_PLUGIN_LOCKED_MSG: &str =
    "loaded plugin library cannot be overwritten while the server is running";

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error::Msg(message.into())
    }

    /// Wraps the error with a prefix, keeping the chain inspectable.
    pub fn wrap(self, context: impl Into<String>) -> Self {
        Error::Wrap(context.into(), Box::new(self))
    }

    /// `errors.Join`: `None` when there is nothing to join.
    pub fn join(errors: Vec<Error>) -> Option<Error> {
        if errors.is_empty() {
            None
        } else {
            Some(Error::Join(errors))
        }
    }

    /// `errors.As(err, *RateLimitError)`.
    pub fn rate_limit(&self) -> Option<&RateLimitError> {
        match self {
            Error::RateLimit(err) => Some(err),
            Error::Wrap(_, inner) => inner.rate_limit(),
            Error::Join(items) => items.iter().find_map(Error::rate_limit),
            _ => None,
        }
    }

    /// `errors.Is(err, ErrLoadedPluginLocked)`.
    pub fn is_loaded_plugin_locked(&self) -> bool {
        match self {
            Error::LoadedPluginLocked => true,
            Error::Wrap(_, inner) => inner.is_loaded_plugin_locked(),
            Error::Join(items) => items.iter().any(Error::is_loaded_plugin_locked),
            _ => false,
        }
    }

    /// `errors.Is(err, context.Canceled)`.
    pub fn is_canceled(&self) -> bool {
        match self {
            Error::Canceled => true,
            Error::Wrap(_, inner) => inner.is_canceled(),
            Error::Join(items) => items.iter().any(Error::is_canceled),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Msg(message) => f.write_str(message),
            Error::RateLimit(err) => err.fmt(f),
            Error::LoadedPluginLocked => f.write_str(LOADED_PLUGIN_LOCKED_MSG),
            Error::Canceled => f.write_str("context canceled"),
            Error::Wrap(context, inner) => write!(f, "{context}: {inner}"),
            Error::Join(items) => {
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_str("\n")?;
                    }
                    item.fmt(f)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<RateLimitError> for Error {
    fn from(err: RateLimitError) -> Self {
        Error::RateLimit(err)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::Msg(message)
    }
}

/// Go `fmt.Errorf` shorthand returning `Error::Msg`.
#[macro_export]
macro_rules! errf {
    ($($arg:tt)*) => { $crate::error::Error::Msg(format!($($arg)*)) };
}

/// Cancellation handle standing in for Go's `context.Context`. Clones share state.
#[derive(Debug, Clone, Default)]
pub struct Context {
    inner: Arc<ContextInner>,
}

#[derive(Debug, Default)]
struct ContextInner {
    canceled: AtomicBool,
    notify: Notify,
}

impl Context {
    /// `context.Background()`: never canceled unless [`Context::cancel`] is called.
    pub fn background() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.canceled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// `ctx.Err()`.
    pub fn err(&self) -> Option<Error> {
        self.is_canceled().then_some(Error::Canceled)
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.load(Ordering::SeqCst)
    }

    /// True when both handles share the same cancellation state.
    pub fn same_as(&self, other: &Context) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Resolves once the context is canceled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}
