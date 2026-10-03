//! Plugin client abstraction: a raw synchronous RPC transport, the guard that tracks in-flight
//! calls (Go: `client_guard.go`), the typed call helper (Go: `callPlugin`) and the error type.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use serde::de::DeserializeOwned;

use cpa_pluginapi::abi::{self, Envelope};
use crate::ctx::CallCtx;

/// Failure of a plugin call. `code` is set for errors the plugin reported itself; `status` is the
/// HTTP status the plugin asked to surface (0 when unset).
#[derive(Debug, Clone, Default)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    pub status: i32,
    /// The call panicked inside the host (Go: a recovered panic, which fuses the plugin).
    pub panicked: bool,
}

impl PluginError {
    pub fn msg(message: impl Into<String>) -> Self {
        PluginError { code: String::new(), message: message.into(), status: 0, panicked: false }
    }
    /// `context.Canceled`; Go maps it to 499.
    pub fn canceled() -> Self {
        PluginError { code: String::new(), message: "context canceled".into(), status: 499, panicked: false }
    }
    pub fn is_canceled(&self) -> bool {
        self.message == "context canceled"
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PluginError {}

pub type PluginResult<T> = Result<T, PluginError>;

/// Marks a host callback instance closed once its plugin client shuts down (Go:
/// `hostCallbackInstance`); callbacks arriving afterwards are rejected.
#[derive(Debug, Default)]
pub struct CallbackInstance {
    pub closed: AtomicBool,
}

impl CallbackInstance {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Synchronous plugin transport (Go: `pluginClient`). `call` blocks the calling thread.
pub trait RawClient: Send + Sync {
    fn call(&self, method: &str, request: &[u8]) -> PluginResult<Vec<u8>>;
    fn shutdown(&self);
    fn callback_instance(&self) -> Arc<CallbackInstance>;
}

/// Wraps a [`RawClient`], counting in-flight calls so shutdown can wait for them and refuse new
/// ones. Cheap to share.
pub struct GuardedClient {
    inner: Mutex<Option<Arc<dyn RawClient>>>,
    instance: Arc<CallbackInstance>,
    calls: Mutex<usize>,
    drained: Condvar,
    in_flight: AtomicUsize,
    shutdown_done: Mutex<bool>,
    shutdown_cv: Condvar,
}

impl GuardedClient {
    pub fn new(inner: Arc<dyn RawClient>) -> Arc<Self> {
        let instance = inner.callback_instance();
        Arc::new(GuardedClient {
            inner: Mutex::new(Some(inner)),
            instance,
            calls: Mutex::new(0),
            drained: Condvar::new(),
            in_flight: AtomicUsize::new(0),
            shutdown_done: Mutex::new(false),
            shutdown_cv: Condvar::new(),
        })
    }

    pub fn instance(&self) -> Arc<CallbackInstance> {
        self.instance.clone()
    }

    fn acquire(&self) -> PluginResult<Arc<dyn RawClient>> {
        let guard = self.inner.lock();
        match guard.as_ref() {
            Some(inner) => {
                *self.calls.lock() += 1;
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                Ok(inner.clone())
            }
            None => Err(PluginError::msg("plugin client is closed")),
        }
    }

    fn release(&self) {
        let mut calls = self.calls.lock();
        *calls -= 1;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if *calls == 0 {
            self.drained.notify_all();
        }
    }

    /// Blocking call on the current thread.
    pub fn call_blocking(&self, method: &str, request: &[u8]) -> PluginResult<Vec<u8>> {
        let inner = self.acquire()?;
        let _release = scopeguard(|| self.release());
        inner.call(method, request)
    }

    /// Runs the call on the blocking pool and returns early when `ctx` is canceled; the call
    /// itself keeps running to completion in the background (Go: guarded client semantics).
    pub async fn call(self: &Arc<Self>, ctx: &CallCtx, method: &str, request: Vec<u8>) -> PluginResult<Vec<u8>> {
        if ctx.is_canceled() {
            return Err(PluginError::canceled());
        }
        let inner = self.acquire()?;
        let me = self.clone();
        let method = method.to_string();
        let task = tokio::task::spawn_blocking(move || {
            let _release = scopeguard(|| me.release());
            inner.call(&method, &request)
        });
        tokio::select! {
            joined = task => joined.map_err(|e| PluginError { panicked: true, ..PluginError::msg(format!("plugin call panicked: {e}")) })?,
            () = ctx.cancelled() => Err(PluginError::canceled()),
        }
    }

    /// Detaches the client immediately; the transport shuts down once active calls finish.
    /// Waits for that at most until `wait` returns (use `None` to wait for completion).
    pub fn shutdown(self: &Arc<Self>, wait: Option<std::time::Duration>) {
        let inner = {
            let mut guard = self.inner.lock();
            guard.take()
        };
        let Some(inner) = inner else {
            self.wait_done(wait);
            return;
        };
        let me = self.clone();
        std::thread::spawn(move || {
            {
                let mut calls = me.calls.lock();
                while *calls > 0 {
                    me.drained.wait(&mut calls);
                }
            }
            inner.shutdown();
            *me.shutdown_done.lock() = true;
            me.shutdown_cv.notify_all();
        });
        self.wait_done(wait);
    }

    fn wait_done(&self, wait: Option<std::time::Duration>) {
        let mut done = self.shutdown_done.lock();
        match wait {
            Some(d) => {
                let deadline = std::time::Instant::now() + d;
                while !*done {
                    if self.shutdown_cv.wait_until(&mut done, deadline).timed_out() {
                        break;
                    }
                }
            }
            None => {
                while !*done {
                    self.shutdown_cv.wait(&mut done);
                }
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().is_none()
    }
}

struct ScopeGuard<F: FnMut()>(F);
impl<F: FnMut()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
fn scopeguard<F: FnMut()>(f: F) -> ScopeGuard<F> {
    ScopeGuard(f)
}

/// Decodes a response envelope into `T` (Go: `decodeEnvelopeResult`). A missing result decodes
/// as `T::default()`.
pub fn decode_envelope<T: DeserializeOwned + Default>(raw: &[u8], method: &str) -> PluginResult<T> {
    let envelope: Envelope =
        serde_json::from_slice(raw).map_err(|e| PluginError::msg(format!("decode plugin envelope {method}: {e}")))?;
    if !envelope.ok {
        return Err(match envelope.error {
            Some(err) => {
                let message = err.message.trim().to_string();
                PluginError {
                    code: err.code.trim().to_string(),
                    message: if message.is_empty() { "plugin call failed".into() } else { message },
                    status: err.http_status,
                    panicked: false,
                }
            }
            None => PluginError::msg("plugin call failed"),
        });
    }
    match envelope.result {
        None => Ok(T::default()),
        Some(raw) => serde_json::from_str::<T>(raw.get())
            .map_err(|e| PluginError::msg(format!("decode plugin result {method}: {e}"))),
    }
}

/// Typed async RPC (Go: `callPlugin`).
pub async fn call_plugin<T: DeserializeOwned + Default>(
    client: &Arc<GuardedClient>,
    ctx: &CallCtx,
    method: &str,
    request: &impl Serialize,
) -> PluginResult<T> {
    let raw = serde_json::to_vec(request)
        .map_err(|e| PluginError::msg(format!("marshal plugin request {method}: {e}")))?;
    let out = client.call(ctx, method, raw).await?;
    decode_envelope(&out, method)
}

/// Typed blocking RPC for synchronous callers (translator hooks, CLI bootstrap).
pub fn call_plugin_blocking<T: DeserializeOwned + Default>(
    client: &GuardedClient,
    method: &str,
    request: &impl Serialize,
) -> PluginResult<T> {
    let raw = serde_json::to_vec(request)
        .map_err(|e| PluginError::msg(format!("marshal plugin request {method}: {e}")))?;
    let out = crate::ctx::block_here(|| client.call_blocking(method, &raw))?;
    decode_envelope(&out, method)
}

/// Serialized `{}` request for calls without parameters.
pub fn empty_request() -> serde_json::Value {
    serde_json::json!({})
}

/// Empty RPC result type (Go: `rpcEmptyResponse`).
#[derive(Debug, Default, serde::Deserialize)]
pub struct Empty {}

#[allow(dead_code)]
fn _assert_abi_used() -> u32 {
    abi::ABI_VERSION
}
