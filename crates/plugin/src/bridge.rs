//! Resource bridges between plugins and the host (Go: `callback_contexts.go`,
//! `http_operation_bridge.go`, `http_stream_bridge.go`, `stream_bridge.go`,
//! `model_stream_bridge.go`). They hand out opaque string ids a plugin passes back through
//! `host.*` callbacks, and release everything when the owning callback scope or plugin closes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::client::CallbackInstance;
use crate::ctx::CallCtx;

/// The five bridges a host owns.
#[derive(Default)]
pub struct Bridges {
    pub contexts: Arc<Contexts>,
    pub http_ops: Arc<HttpOperations>,
    pub http_streams: Arc<HttpStreams>,
    pub streams: Arc<StreamBridge>,
    pub model_streams: Arc<ModelStreams>,
}

pub(crate) fn same_instance(a: &Option<Arc<CallbackInstance>>, b: &Option<Arc<CallbackInstance>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

fn instance_key(i: &Option<Arc<CallbackInstance>>) -> usize {
    i.as_ref().map(|a| Arc::as_ptr(a) as usize).unwrap_or(0)
}

type Cleanup = Box<dyn FnOnce() + Send>;

// ---- callback contexts ----

struct ContextEntry {
    ctx: CallCtx,
    plugin_id: String,
    instance: Option<Arc<CallbackInstance>>,
    cleanups: Vec<(u64, Cleanup)>,
}

/// Open callback scopes: while a plugin call is running the host keeps its request context under
/// an id the plugin echoes in callbacks (`host_callback_id`).
#[derive(Default)]
pub struct Contexts {
    next: AtomicU64,
    next_cleanup: AtomicU64,
    map: Mutex<HashMap<String, ContextEntry>>,
}

/// Closing the guard runs the scope's cleanups (Go: the closer returned by `open`).
pub struct CallbackGuard {
    id: String,
    contexts: Arc<Contexts>,
}

impl CallbackGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        let entry = self.contexts.map.lock().remove(&self.id);
        if let Some(entry) = entry {
            for (_, f) in entry.cleanups {
                f();
            }
        }
    }
}

impl Contexts {
    pub fn open(self: &Arc<Self>, ctx: &CallCtx, plugin_id: &str, instance: Option<Arc<CallbackInstance>>) -> CallbackGuard {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        self.map.lock().insert(
            id.clone(),
            ContextEntry { ctx: ctx.clone(), plugin_id: plugin_id.trim().to_string(), instance, cleanups: Vec::new() },
        );
        CallbackGuard { id, contexts: self.clone() }
    }

    pub fn lookup(&self, id: &str) -> Option<(CallCtx, String, Option<Arc<CallbackInstance>>)> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        self.map.lock().get(id).map(|e| (e.ctx.clone(), e.plugin_id.clone(), e.instance.clone()))
    }

    pub fn plugin_id(&self, id: &str) -> String {
        self.lookup(id).map(|(_, p, _)| p).unwrap_or_default()
    }

    /// The scope's context, or `fallback` when the id is unknown.
    pub fn resolve(&self, id: &str, fallback: &CallCtx) -> CallCtx {
        if id.is_empty() {
            return fallback.clone();
        }
        self.map.lock().get(id).map(|e| e.ctx.clone()).unwrap_or_else(|| fallback.clone())
    }

    /// Registers a cleanup run when the scope closes. Runs it immediately and returns `None`
    /// when the scope is already gone; otherwise returns a handle that unregisters it.
    pub fn add_cleanup(self: &Arc<Self>, id: &str, cleanup: Cleanup) -> Option<Box<dyn FnOnce() + Send>> {
        let id = id.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let cid = self.next_cleanup.fetch_add(1, Ordering::SeqCst) + 1;
        let mut map = self.map.lock();
        match map.get_mut(&id) {
            Some(entry) => {
                entry.cleanups.push((cid, cleanup));
                drop(map);
                let me = self.clone();
                Some(Box::new(move || {
                    if let Some(entry) = me.map.lock().get_mut(&id) {
                        entry.cleanups.retain(|(c, _)| *c != cid);
                    }
                }))
            }
            None => {
                drop(map);
                cleanup();
                None
            }
        }
    }
}

// ---- host HTTP operations (cancellable in-flight requests) ----

struct Operation {
    ctx: CallCtx,
    instance: Option<Arc<CallbackInstance>>,
    callback_id: String,
    started: bool,
    cleanup: Option<Cleanup>,
    scope_cleanup: Option<Box<dyn FnOnce() + Send>>,
}

#[derive(Default)]
pub struct HttpOperations {
    next: AtomicU64,
    map: Mutex<HashMap<(String, String), Operation>>,
}

/// An acquired operation: its cancelable context plus the finish hook.
pub struct OperationHandle {
    pub ctx: CallCtx,
    pub plugin_id: String,
    pub operation_id: String,
    pub instance: Option<Arc<CallbackInstance>>,
    ops: Arc<HttpOperations>,
    finished: bool,
}

impl OperationHandle {
    /// Releases the operation: removes it and cancels its context.
    pub fn finish(mut self) {
        self.finish_inner();
    }

    fn finish_inner(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ops.finish(&self.plugin_id, &self.operation_id);
        self.ctx.cancel();
    }

    /// Keeps the operation open (a stream owns it now); `finish` must be called by the owner.
    pub fn detach(mut self) -> (CallCtx, String, String) {
        self.finished = true;
        (self.ctx.clone(), self.plugin_id.clone(), self.operation_id.clone())
    }
}

impl Drop for OperationHandle {
    fn drop(&mut self) {
        self.finish_inner();
    }
}

impl HttpOperations {
    pub fn open(
        self: &Arc<Self>,
        plugin_id: &str,
        instance: Option<Arc<CallbackInstance>>,
        callback_id: &str,
        parent: &CallCtx,
        started: bool,
    ) -> Option<(String, CallCtx)> {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let ctx = parent.child();
        if instance.as_ref().is_some_and(|i| i.is_closed()) {
            ctx.cancel();
            return None;
        }
        let key = (plugin_id.trim().to_string(), id.clone());
        let mut map = self.map.lock();
        if map.contains_key(&key) {
            return None;
        }
        map.insert(
            key,
            Operation { ctx: ctx.clone(), instance, callback_id: callback_id.trim().to_string(), started, cleanup: None, scope_cleanup: None },
        );
        Some((id, ctx))
    }

    /// Marks a pre-opened operation as in use by one HTTP callback.
    pub fn claim(self: &Arc<Self>, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, operation_id: &str, callback_id: &str) -> Option<OperationHandle> {
        let operation_id = operation_id.trim();
        if operation_id.is_empty() {
            return None;
        }
        let key = (plugin_id.trim().to_string(), operation_id.to_string());
        let mut map = self.map.lock();
        let entry = map.get_mut(&key)?;
        if entry.started || !same_instance(&entry.instance, instance) || entry.callback_id != callback_id.trim() {
            return None;
        }
        entry.started = true;
        Some(OperationHandle {
            ctx: entry.ctx.clone(),
            plugin_id: key.0,
            operation_id: key.1,
            instance: entry.instance.clone(),
            ops: self.clone(),
            finished: false,
        })
    }

    pub fn handle(self: &Arc<Self>, plugin_id: &str, operation_id: &str, ctx: CallCtx, instance: Option<Arc<CallbackInstance>>) -> OperationHandle {
        OperationHandle { ctx, plugin_id: plugin_id.trim().to_string(), operation_id: operation_id.to_string(), instance, ops: self.clone(), finished: false }
    }

    /// Registers the cleanup run if the operation is canceled (a stream bound to it). Runs it
    /// right away when the operation is no longer cancelable.
    pub fn set_cleanup(&self, plugin_id: &str, operation_id: &str, cleanup: Cleanup) -> bool {
        let key = (plugin_id.trim().to_string(), operation_id.trim().to_string());
        let mut map = self.map.lock();
        if let Some(entry) = map.get_mut(&key)
            && entry.cleanup.is_none()
            && !entry.ctx.is_canceled()
        {
            entry.cleanup = Some(cleanup);
            return true;
        }
        drop(map);
        cleanup();
        false
    }

    pub fn set_scope_cleanup(&self, plugin_id: &str, operation_id: &str, stop: Box<dyn FnOnce() + Send>) -> bool {
        let key = (plugin_id.trim().to_string(), operation_id.trim().to_string());
        let mut map = self.map.lock();
        if let Some(entry) = map.get_mut(&key)
            && entry.scope_cleanup.is_none()
        {
            entry.scope_cleanup = Some(stop);
            return true;
        }
        drop(map);
        stop();
        false
    }

    fn finish(&self, plugin_id: &str, operation_id: &str) {
        let removed = self.map.lock().remove(&(plugin_id.trim().to_string(), operation_id.trim().to_string()));
        if let Some(mut op) = removed
            && let Some(stop) = op.scope_cleanup.take()
        {
            stop();
        }
    }

    /// Cancels one operation owned by `plugin_id` with the same callback instance.
    pub fn cancel(&self, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, operation_id: &str) {
        let operation_id = operation_id.trim();
        if operation_id.is_empty() {
            return;
        }
        let key = (plugin_id.trim().to_string(), operation_id.to_string());
        let entry = {
            let mut map = self.map.lock();
            match map.get(&key) {
                Some(e) if same_instance(&e.instance, instance) => map.remove(&key),
                _ => None,
            }
        };
        cancel_operation(entry);
    }

    fn cancel_matching(&self, plugin_id: &str, instance: Option<&Arc<CallbackInstance>>, all: bool) {
        let plugin_id = plugin_id.trim();
        let entries: Vec<Operation> = {
            let mut map = self.map.lock();
            let keys: Vec<_> = map
                .iter()
                .filter(|(k, e)| {
                    all || (k.0 == plugin_id && instance.is_none_or(|i| e.instance.as_ref().is_some_and(|x| Arc::ptr_eq(x, i))))
                })
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        for e in entries {
            if all && let Some(i) = &e.instance {
                i.closed.store(true, Ordering::SeqCst);
            }
            cancel_operation(Some(e));
        }
    }

    pub fn close_instance(&self, plugin_id: &str, instance: Option<&Arc<CallbackInstance>>) {
        match instance {
            None => self.cancel_plugin(plugin_id),
            Some(i) => {
                i.closed.store(true, Ordering::SeqCst);
                self.cancel_matching(plugin_id, Some(i), false);
            }
        }
    }

    pub fn cancel_plugin(&self, plugin_id: &str) {
        if !plugin_id.trim().is_empty() {
            self.cancel_matching(plugin_id, None, false);
        }
    }

    pub fn cancel_all(&self) {
        self.cancel_matching("", None, true);
    }
}

fn cancel_operation(entry: Option<Operation>) {
    let Some(mut e) = entry else { return };
    if let Some(stop) = e.scope_cleanup.take() {
        stop();
    }
    e.ctx.cancel();
    if let Some(c) = e.cleanup.take() {
        c();
    }
}

// ---- host HTTP streams ----

/// One chunk or terminal error of a host HTTP stream.
pub struct HttpChunk {
    pub payload: Bytes,
    pub err: Option<String>,
}

struct HttpStreamEntry {
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<HttpChunk>>>,
    cancel: CallCtx,
    on_close: Option<Cleanup>,
}

#[derive(Default)]
pub struct HttpStreams {
    next: AtomicU64,
    map: Mutex<HashMap<(String, usize, String), HttpStreamEntry>>,
}

impl HttpStreams {
    pub fn open(&self, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, rx: mpsc::Receiver<HttpChunk>, cancel: CallCtx, on_close: Cleanup) -> String {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        self.map.lock().insert(
            (plugin_id.trim().to_string(), instance_key(instance), id.clone()),
            HttpStreamEntry { rx: Arc::new(tokio::sync::Mutex::new(rx)), cancel, on_close: Some(on_close) },
        );
        id
    }

    /// Next chunk: `(chunk, done)`.
    pub async fn read(&self, ctx: &CallCtx, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, id: &str) -> Result<(Option<HttpChunk>, bool), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("http stream id is required".into());
        }
        let key = (plugin_id.trim().to_string(), instance_key(instance), id.to_string());
        let rx = self.map.lock().get(&key).map(|e| e.rx.clone());
        let Some(rx) = rx else { return Err(format!("http stream {id} is not open")) };
        let mut rx = rx.lock().await;
        let next = tokio::select! {
            () = ctx.cancelled() => {
                drop(rx);
                self.close(plugin_id, instance, id);
                return Err("context canceled".into());
            }
            c = rx.recv() => c,
        };
        drop(rx);
        match next {
            None => {
                self.close(plugin_id, instance, id);
                Ok((None, true))
            }
            Some(chunk) if chunk.err.is_some() => {
                self.close(plugin_id, instance, id);
                Ok((Some(chunk), true))
            }
            Some(chunk) => Ok((Some(chunk), false)),
        }
    }

    pub fn close(&self, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, id: &str) {
        let entry = self.map.lock().remove(&(plugin_id.trim().to_string(), instance_key(instance), id.trim().to_string()));
        close_http_stream(entry);
    }

    fn close_matching(&self, plugin_id: &str, instance: Option<&Arc<CallbackInstance>>, all: bool) {
        let plugin_id = plugin_id.trim();
        let entries: Vec<HttpStreamEntry> = {
            let mut map = self.map.lock();
            let keys: Vec<_> = map
                .keys()
                .filter(|k| all || (k.0 == plugin_id && instance.is_none_or(|i| k.1 == Arc::as_ptr(i) as usize)))
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        for e in entries {
            close_http_stream(Some(e));
        }
    }

    pub fn close_instance(&self, plugin_id: &str, instance: &Arc<CallbackInstance>) {
        self.close_matching(plugin_id, Some(instance), false);
    }

    pub fn close_plugin(&self, plugin_id: &str) {
        if !plugin_id.trim().is_empty() {
            self.close_matching(plugin_id, None, false);
        }
    }

    pub fn close_all(&self) {
        self.close_matching("", None, true);
    }
}

fn close_http_stream(entry: Option<HttpStreamEntry>) {
    let Some(mut e) = entry else { return };
    e.cancel.cancel();
    if let Some(c) = e.on_close.take() {
        c();
    }
}

impl Bridges {
    pub fn close_http_callback_instance(&self, plugin_id: &str, instance: Option<&Arc<CallbackInstance>>) {
        if let Some(i) = instance {
            i.closed.store(true, Ordering::SeqCst);
        }
        self.http_ops.close_instance(plugin_id, instance);
        if let Some(i) = instance {
            self.http_streams.close_instance(plugin_id, i);
        }
    }

    pub fn close_http_plugin_resources(&self, plugin_id: &str, instance: Option<&Arc<CallbackInstance>>) {
        self.close_http_callback_instance(plugin_id, instance);
        self.http_ops.cancel_plugin(plugin_id);
        self.http_streams.close_plugin(plugin_id);
    }

    pub fn cancel_all_http(&self) {
        self.http_ops.cancel_all();
        self.http_streams.close_all();
    }
}

// ---- executor stream bridge (host.stream.emit / host.stream.close) ----

/// One item produced by a plugin executor stream.
pub type StreamItem = Result<Bytes, String>;

const STREAM_BUFFER: usize = 16;

#[derive(Default)]
pub struct StreamBridge {
    next: AtomicU64,
    map: Mutex<HashMap<String, mpsc::Sender<StreamItem>>>,
}

impl StreamBridge {
    /// Opens a stream: its id, the receiving end and a cleanup that aborts it. A canceled `ctx`
    /// aborts it too (Go: streams canceled before ExecuteStream installs its cleanup).
    pub fn open(self: &Arc<Self>, ctx: &CallCtx) -> (String, mpsc::Receiver<StreamItem>, Arc<dyn Fn() + Send + Sync>) {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        self.map.lock().insert(id.clone(), tx);
        let me = self.clone();
        let cid = id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            me.map.lock().remove(&cid);
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let c = cleanup.clone();
            let ctx = ctx.clone();
            handle.spawn(async move {
                ctx.cancelled().await;
                c();
            });
        }
        (id, rx, cleanup)
    }

    pub fn sender(&self, id: &str) -> Option<mpsc::Sender<StreamItem>> {
        self.map.lock().get(id).cloned()
    }

    /// `host.stream.close`: ends the stream, delivering `error` as the last item when set.
    pub async fn close(&self, id: &str, error: &str) {
        let tx = self.map.lock().remove(id);
        if let Some(tx) = tx
            && !error.is_empty()
        {
            let _ = tx.send(Err(error.to_string())).await;
        }
    }
}

// ---- host model streams (host.model.execute_stream) ----

/// One chunk of a host model stream.
pub struct ModelChunk {
    pub payload: Bytes,
    pub err: Option<crate::callbacks::ModelStreamError>,
}

struct ModelStreamEntry {
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<ModelChunk>>>,
    cancel: CallCtx,
}

#[derive(Default)]
pub struct ModelStreams {
    next: AtomicU64,
    map: Mutex<HashMap<String, ModelStreamEntry>>,
}

impl ModelStreams {
    pub fn open(&self, rx: mpsc::Receiver<ModelChunk>, cancel: CallCtx) -> String {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        self.map.lock().insert(id.clone(), ModelStreamEntry { rx: Arc::new(tokio::sync::Mutex::new(rx)), cancel });
        id
    }

    /// Next chunk; an unknown id reads as an ended stream.
    pub async fn read(&self, ctx: &CallCtx, id: &str) -> Result<(Option<ModelChunk>, bool), String> {
        if id.is_empty() {
            return Err("model stream id is required".into());
        }
        let rx = self.map.lock().get(id).map(|e| e.rx.clone());
        let Some(rx) = rx else { return Ok((None, true)) };
        let mut rx = rx.lock().await;
        let next = tokio::select! {
            () = ctx.cancelled() => {
                drop(rx);
                self.close(id);
                return Err("context canceled".into());
            }
            c = rx.recv() => c,
        };
        drop(rx);
        match next {
            None => {
                self.close(id);
                Ok((None, true))
            }
            Some(chunk) if chunk.err.is_some() => {
                self.close(id);
                Ok((Some(chunk), true))
            }
            Some(chunk) => Ok((Some(chunk), false)),
        }
    }

    pub fn close(&self, id: &str) {
        if let Some(e) = self.map.lock().remove(id) {
            e.cancel.cancel();
        }
    }
}
