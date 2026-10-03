//! Request-scoped call context handed to plugin calls (Go: `context.Context` values the plugin
//! host reads) and the helper that runs blocking plugin calls from async code.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

/// Cancellation plus the request identity host callbacks need. Clone is cheap.
#[derive(Clone, Default)]
pub struct CallCtx {
    cancel: CancellationToken,
    /// Inbound request id (trace id) used in plugin logs.
    pub request_id: String,
    /// Opaque embedder data (the server stores its request info here so nested model
    /// executions can reuse it).
    pub ext: Option<Arc<dyn Any + Send + Sync>>,
    /// Set once a plugin ran a nested host model execution under this context (Go: the nested
    /// execution tracker), so the outer call skips duplicate usage reporting.
    nested: Arc<AtomicBool>,
    /// Set once an upstream HTTP attempt was made through the host (Go: `MarkUpstreamAttempt`).
    attempted: Arc<AtomicBool>,
}

impl CallCtx {
    pub fn background() -> Self {
        CallCtx::default()
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = id.into();
        self
    }

    pub fn with_ext(mut self, ext: Arc<dyn Any + Send + Sync>) -> Self {
        self.ext = Some(ext);
        self
    }

    /// A child that is canceled with its parent but can also be canceled on its own.
    pub fn child(&self) -> CallCtx {
        CallCtx {
            cancel: self.cancel.child_token(),
            request_id: self.request_id.clone(),
            ext: self.ext.clone(),
            nested: self.nested.clone(),
            attempted: self.attempted.clone(),
        }
    }

    /// Same values, cancellation detached from the parent (Go: `context.WithoutCancel`).
    pub fn detached(&self) -> CallCtx {
        CallCtx {
            cancel: CancellationToken::new(),
            request_id: self.request_id.clone(),
            ext: self.ext.clone(),
            nested: self.nested.clone(),
            attempted: self.attempted.clone(),
        }
    }

    pub fn mark_nested(&self) {
        self.nested.store(true, Ordering::SeqCst);
    }

    pub fn has_nested(&self) -> bool {
        self.nested.load(Ordering::SeqCst)
    }

    pub fn mark_upstream_attempt(&self) {
        self.attempted.store(true, Ordering::SeqCst);
    }

    pub fn upstream_attempted(&self) -> bool {
        self.attempted.load(Ordering::SeqCst)
    }

    pub fn is_canceled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }
}

/// Runs a blocking plugin call from synchronous code that may sit on a tokio worker thread.
pub fn block_here<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Request facts a conductor-driven plugin call carries in [`CallCtx::ext`]: the execution
/// metadata (client ip, api key, trace id, ...) nested host model executions reuse.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta(pub std::collections::HashMap<String, serde_json::Value>);
