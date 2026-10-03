//! A minimal Go `context.Context` stand-in: cancellation plus an optional deadline.

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Why a [`Ctx`] is done (Go: `context.Canceled` / `context.DeadlineExceeded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CtxError {
    #[error("context canceled")]
    Canceled,
    #[error("context deadline exceeded")]
    DeadlineExceeded,
}

/// Cancellation scope handed to advertisers and browsers. Cloning shares the cancellation state.
#[derive(Debug, Clone)]
pub struct Ctx {
    token: CancellationToken,
    deadline: Option<Instant>,
}

impl Ctx {
    /// Go `context.Background()`: never done.
    pub fn background() -> Self {
        Self { token: CancellationToken::new(), deadline: None }
    }

    /// Go `context.WithCancel`: a child that is done when it or `self` is canceled.
    pub fn with_cancel(&self) -> Ctx {
        Ctx { token: self.token.child_token(), deadline: self.deadline }
    }

    /// Go `context.WithTimeout`: a cancelable child that is also done after `timeout`.
    pub fn with_timeout(&self, timeout: Duration) -> Ctx {
        let deadline = Instant::now() + timeout;
        let deadline = match self.deadline {
            Some(existing) if existing < deadline => existing,
            _ => deadline,
        };
        Ctx { token: self.token.child_token(), deadline: Some(deadline) }
    }

    /// Cancels this context and its children.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Remaining time until the deadline, if one is set.
    pub fn time_remaining(&self) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Go `ctx.Err()`.
    pub fn err(&self) -> Option<CtxError> {
        if self.token.is_cancelled() {
            return Some(CtxError::Canceled);
        }
        match self.deadline {
            Some(d) if Instant::now() >= d => Some(CtxError::DeadlineExceeded),
            _ => None,
        }
    }

    /// Go `<-ctx.Done()`: resolves with the reason once canceled or past the deadline.
    pub async fn done(&self) -> CtxError {
        match self.deadline {
            Some(deadline) => tokio::select! {
                _ = self.token.cancelled() => CtxError::Canceled,
                _ = tokio::time::sleep_until(deadline) => CtxError::DeadlineExceeded,
            },
            None => {
                self.token.cancelled().await;
                CtxError::Canceled
            }
        }
    }
}
