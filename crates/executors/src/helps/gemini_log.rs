//! Upstream request logging for the Gemini family and Antigravity (Go: the
//! `helps.RecordAPIRequest` / `RecordAPIResponseMetadata` / `RecordAPIResponseError` /
//! `AppendAPIResponseChunk` calls in those executors). One cheap, cloneable handle per upstream
//! attempt so spawned stream tasks can keep recording.

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::apilog::{ApiLogHandle, UpstreamRequestLog};
use cpa_runtime::executor::{ExecError, Options};
use http::HeaderMap;

/// The request's [`ApiLogHandle`] plus the config snapshot the recorders consult.
#[derive(Clone)]
pub struct UpstreamLog {
    handle: ApiLogHandle,
    cfg: Arc<Config>,
}

impl UpstreamLog {
    pub fn new(opts: &Options, cfg: &Arc<Config>) -> Self {
        Self { handle: opts.api_log.clone(), cfg: Arc::clone(cfg) }
    }

    /// `RecordAPIRequest` with the auth fields Go derives from `auth.AccountInfo()`; `headers`
    /// must be the headers Go's `req.Header` would hold (no default user agent).
    pub fn request(&self, auth: &Auth, provider: &str, url: &str, headers: &HeaderMap, body: &[u8]) {
        self.request_with_method(auth, provider, "POST", url, headers, body);
    }

    /// [`request`](Self::request) for a non-POST method.
    pub fn request_with_method(
        &self,
        auth: &Auth,
        provider: &str,
        method: &str,
        url: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) {
        self.handle.record_api_request(&self.cfg, UpstreamRequestLog::from_auth(provider, Some(auth), method, url, headers, body));
    }

    /// `RecordAPIResponseMetadata`.
    pub fn metadata(&self, status: u16, headers: &HeaderMap) {
        self.handle.record_api_response_metadata(&self.cfg, status, headers);
    }

    /// `RecordAPIResponseError`.
    pub fn error(&self, err: &str) {
        self.handle.record_api_response_error(&self.cfg, err);
    }

    /// Records a failed result with [`error`](Self::error) and passes it through (the Go
    /// `if err != nil { RecordAPIResponseError(...); return err }` idiom).
    pub fn tap_err<T>(&self, result: Result<T, ExecError>) -> Result<T, ExecError> {
        if let Err(err) = &result {
            self.error(&err.message);
        }
        result
    }

    /// `AppendAPIResponseChunk`.
    pub fn chunk(&self, chunk: &[u8]) {
        self.handle.append_api_response_chunk(&self.cfg, chunk);
    }
}
