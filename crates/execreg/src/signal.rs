//! A one-shot "done" flag any number of tasks can await (Go: a closed `chan struct{}`).

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Signal(Arc<watch::Sender<bool>>);

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

impl Signal {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Marks the signal done; idempotent.
    pub fn fire(&self) {
        self.0.send_replace(true);
    }

    pub fn is_fired(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the signal has fired (immediately if it already did).
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

/// Fires its signal on drop, so waiters are released even if the owning task panics or is dropped.
pub(crate) struct FireOnDrop(pub Signal);

impl Drop for FireOnDrop {
    fn drop(&mut self) {
        self.0.fire();
    }
}
