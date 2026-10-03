//! Native plugin loading through `dlopen` (Go: `loader_unix.go`, `host_callbacks_unix.go`).
//!
//! The C ABI is the one in `sdk/pluginabi`: the plugin exports `cliproxy_plugin_init`, which fills
//! a function table; the host passes a table whose `call` entry reaches [`HostCallbacks`]. All
//! unsafe code of the crate lives in this file.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use cpa_pluginapi::abi::{self, ABI_VERSION};
use crate::client::{CallbackInstance, PluginError, PluginResult, RawClient};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

#[repr(C)]
struct Buffer {
    ptr: *mut c_void,
    len: usize,
}

type HostCallFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut Buffer) -> c_int;
type HostFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginCallFn = unsafe extern "C" fn(*const c_char, *const u8, usize, *mut Buffer) -> c_int;
type PluginFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginShutdownFn = unsafe extern "C" fn();
type PluginInitFn = unsafe extern "C" fn(*const HostApi, *mut PluginApi) -> c_int;

#[repr(C)]
struct HostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: HostCallFn,
    free_buffer: HostFreeFn,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PluginApi {
    abi_version: u32,
    call: Option<PluginCallFn>,
    free_buffer: Option<PluginFreeFn>,
    shutdown: Option<PluginShutdownFn>,
}

/// Where plugin-initiated calls (`host.*` methods) are served. The return value is the response
/// envelope bytes (success or error); it is never empty.
pub trait HostCallbacks: Send + Sync {
    fn call_from_plugin(&self, plugin_id: &str, instance: &Arc<CallbackInstance>, method: &str, request: &[u8]) -> Vec<u8>;
}

struct HostEntry {
    host: Arc<dyn HostCallbacks>,
    plugin_id: String,
    instance: Arc<CallbackInstance>,
}

static NEXT_HOST_ID: AtomicUsize = AtomicUsize::new(1);
static HOST_ENTRIES: Lazy<Mutex<HashMap<usize, Arc<HostEntry>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Entry point the plugin calls for `host.*` methods. Never unwinds into the plugin.
unsafe extern "C" fn host_call(
    host_ctx: *mut c_void,
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut Buffer,
) -> c_int {
    if !response.is_null() {
        unsafe {
            (*response).ptr = std::ptr::null_mut();
            (*response).len = 0;
        }
    }
    if host_ctx.is_null() || method.is_null() {
        return 1;
    }
    let id = host_ctx as usize;
    let Some(entry) = HOST_ENTRIES.lock().get(&id).cloned() else {
        return 1;
    };
    let method = unsafe { CStr::from_ptr(method) }.to_string_lossy().into_owned();
    let request: &[u8] =
        if request.is_null() || request_len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(request, request_len) } };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if entry.instance.is_closed() {
            return abi::error_envelope("host_call_failed", "host plugin callback instance is closed", 0);
        }
        entry.host.call_from_plugin(&entry.plugin_id, &entry.instance, &method, request)
    }));
    let Ok(resp) = outcome else {
        return 1;
    };
    if resp.is_empty() || response.is_null() {
        return 0;
    }
    let ptr = unsafe { malloc(resp.len()) };
    if ptr.is_null() {
        return 1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(resp.as_ptr(), ptr.cast::<u8>(), resp.len());
        (*response).ptr = ptr;
        (*response).len = resp.len();
    }
    0
}

unsafe extern "C" fn host_free(ptr: *mut c_void, _len: usize) {
    if !ptr.is_null() {
        unsafe { free(ptr) };
    }
}

/// An opened shared library with its negotiated function table.
pub struct DynClient {
    lib: Mutex<Option<Library>>,
    api: Mutex<PluginApi>,
    // Boxed so the pointer handed to the plugin stays valid for the library's lifetime.
    _host_api: Box<HostApi>,
    id: usize,
    instance: Arc<CallbackInstance>,
}

// SAFETY: the raw pointers in `HostApi` are an opaque id and function pointers; the plugin
// function table is plain function pointers. All mutable state sits behind mutexes.
unsafe impl Send for DynClient {}
unsafe impl Sync for DynClient {}

impl DynClient {
    /// Loads `path`, runs `cliproxy_plugin_init` and validates the returned table.
    pub fn open(
        path: &Path,
        plugin_id: &str,
        host: Arc<dyn HostCallbacks>,
        instance: Arc<CallbackInstance>,
    ) -> PluginResult<Arc<DynClient>> {
        let lib = unsafe { Library::open(Some(path), RTLD_NOW | RTLD_LOCAL) }
            .map_err(|e| PluginError::msg(format!("dlopen {}: {e}", path.display())))?;
        let init: PluginInitFn = {
            let sym = unsafe { lib.get::<PluginInitFn>(b"cliproxy_plugin_init\0") }
                .map_err(|e| PluginError::msg(format!("missing cliproxy_plugin_init: {e}")))?;
            *sym
        };
        let id = NEXT_HOST_ID.fetch_add(1, Ordering::SeqCst);
        HOST_ENTRIES.lock().insert(id, Arc::new(HostEntry { host, plugin_id: plugin_id.to_string(), instance: instance.clone() }));
        let host_api = Box::new(HostApi {
            abi_version: ABI_VERSION,
            host_ctx: id as *mut c_void,
            call: host_call,
            free_buffer: host_free,
        });
        let mut api = PluginApi { abi_version: 0, call: None, free_buffer: None, shutdown: None };
        let client = Arc::new(DynClient { lib: Mutex::new(Some(lib)), api: Mutex::new(api), _host_api: host_api, id, instance });
        let rc = unsafe { init(&*client._host_api, &mut api) };
        *client.api.lock() = api;
        if rc != 0 {
            client.shutdown();
            return Err(PluginError::msg(format!("cliproxy_plugin_init returned {rc}")));
        }
        if api.abi_version != ABI_VERSION {
            client.shutdown();
            return Err(PluginError::msg(format!("plugin ABI version {} is not supported", api.abi_version)));
        }
        if api.call.is_none() || api.free_buffer.is_none() {
            client.shutdown();
            return Err(PluginError::msg("plugin function table is incomplete"));
        }
        Ok(client)
    }
}

impl RawClient for DynClient {
    fn call(&self, method: &str, request: &[u8]) -> PluginResult<Vec<u8>> {
        let api = *self.api.lock();
        let (Some(call), Some(free_buffer)) = (api.call, api.free_buffer) else {
            return Err(PluginError::msg("plugin client is closed"));
        };
        let c_method = CString::new(method).map_err(|_| PluginError::msg("plugin method contains NUL"))?;
        let mut response = Buffer { ptr: std::ptr::null_mut(), len: 0 };
        let req_ptr = if request.is_empty() { std::ptr::null() } else { request.as_ptr() };
        let rc = unsafe { call(c_method.as_ptr(), req_ptr, request.len(), &mut response) };
        let mut out = Vec::new();
        if !response.ptr.is_null() {
            if response.len > 0 {
                out = unsafe { std::slice::from_raw_parts(response.ptr.cast::<u8>(), response.len) }.to_vec();
            }
            unsafe { free_buffer(response.ptr, response.len) };
        }
        if rc != 0 {
            if abi::is_error_envelope(&out) {
                return Ok(out);
            }
            return Err(PluginError::msg(format!("plugin call {method} returned {rc}: {}", String::from_utf8_lossy(&out))));
        }
        Ok(out)
    }

    fn shutdown(&self) {
        self.instance.closed.store(true, Ordering::SeqCst);
        let shutdown = {
            let mut api = self.api.lock();
            let s = api.shutdown.take();
            api.call = None;
            s
        };
        if let Some(shutdown) = shutdown {
            unsafe { shutdown() };
        }
        HOST_ENTRIES.lock().remove(&self.id);
        // Dropping the library runs dlclose (Go: cliproxy_dlclose).
        self.lib.lock().take();
    }

    fn callback_instance(&self) -> Arc<CallbackInstance> {
        self.instance.clone()
    }
}
