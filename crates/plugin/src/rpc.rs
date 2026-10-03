//! Typed RPC helpers on the host (Go: the `rpcPluginAdapter` methods of `rpc_client.go`): plain
//! calls, calls inside a host-callback scope, and blocking variants for synchronous hooks.

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::caps::{Record, WithHost};
use crate::client::{PluginResult, call_plugin, call_plugin_blocking};
use crate::ctx::CallCtx;
use crate::host::Host;

impl Host {
    /// True when `rec` may still be called: not fused and still the active plugin version.
    pub(crate) fn usable(&self, rec: &Record) -> bool {
        !self.is_plugin_fused(&rec.id) && self.record_current(rec)
    }

    fn note_panic<T>(&self, rec: &Record, method: &str, result: &PluginResult<T>) {
        if let Err(e) = result
            && e.panicked
        {
            self.fuse_plugin(&rec.id, method, &e.message);
        }
    }

    /// One RPC to `rec`.
    pub(crate) async fn rpc<T: DeserializeOwned + Default>(
        &self,
        rec: &Record,
        ctx: &CallCtx,
        method: &str,
        req: &impl Serialize,
    ) -> PluginResult<T> {
        let out = call_plugin::<T>(&rec.client, ctx, method, req).await;
        self.note_panic(rec, method, &out);
        out
    }

    /// One RPC inside a host-callback scope: the plugin may call back into the host using the
    /// `host_callback_id` it receives, and everything it opens is released when the call ends.
    pub(crate) async fn rpc_cb<T: DeserializeOwned + Default>(
        self: &Arc<Self>,
        rec: &Record,
        ctx: &CallCtx,
        method: &str,
        req: &impl Serialize,
    ) -> PluginResult<T> {
        let guard = self.bridges.contexts.open(ctx, &rec.id, Some(rec.client.instance()));
        let wrapped = WithHost::new(req, guard.id().to_string());
        let out = call_plugin::<T>(&rec.client, ctx, method, &wrapped).await;
        self.note_panic(rec, method, &out);
        drop(guard);
        out
    }

    /// Like [`Host::rpc_cb`] with a stream id the plugin emits chunks to.
    pub(crate) async fn rpc_cb_stream<T: DeserializeOwned + Default>(
        self: &Arc<Self>,
        rec: &Record,
        ctx: &CallCtx,
        method: &str,
        req: &impl Serialize,
        stream_id: &str,
        guard: &crate::bridge::CallbackGuard,
    ) -> PluginResult<T> {
        let mut wrapped = WithHost::new(req, guard.id().to_string());
        wrapped.stream_id = stream_id.to_string();
        let out = call_plugin::<T>(&rec.client, ctx, method, &wrapped).await;
        self.note_panic(rec, method, &out);
        out
    }

    /// Blocking RPC for synchronous hooks (translator, thinking).
    pub(crate) fn rpc_blocking<T: DeserializeOwned + Default>(&self, rec: &Record, method: &str, req: &impl Serialize) -> PluginResult<T> {
        let out = call_plugin_blocking::<T>(&rec.client, method, req);
        self.note_panic(rec, method, &out);
        out
    }

    /// Blocking RPC inside a callback scope.
    pub(crate) fn rpc_cb_blocking<T: DeserializeOwned + Default>(
        self: &Arc<Self>,
        rec: &Record,
        ctx: &CallCtx,
        method: &str,
        req: &impl Serialize,
    ) -> PluginResult<T> {
        let guard = self.bridges.contexts.open(ctx, &rec.id, Some(rec.client.instance()));
        let wrapped = WithHost::new(req, guard.id().to_string());
        let out = call_plugin_blocking::<T>(&rec.client, method, &wrapped);
        self.note_panic(rec, method, &out);
        drop(guard);
        out
    }

    pub(crate) fn auth_identifier(&self, rec: &Record) -> Option<String> {
        if !rec.caps().auth_provider || self.is_plugin_fused(&rec.id) {
            return None;
        }
        Some(rec.info.auth_identifier.clone())
    }

    pub(crate) fn quota_identifier(&self, rec: &Record) -> Option<String> {
        if !rec.caps().quota_provider || self.is_plugin_fused(&rec.id) {
            return None;
        }
        let id = rec.info.quota_identifier.clone();
        (!id.is_empty()).then_some(id)
    }
}
