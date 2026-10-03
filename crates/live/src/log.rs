//! Upstream capture hooks for request logging (Go: `helps.RecordAPI*`). The server supplies an
//! implementation bound to the inbound request's log; the default does nothing.

use cpa_executors::helps::logging::UpstreamRequestLog;
use http::HeaderMap;

pub trait UpstreamLog: Send + Sync {
    fn request(&self, _entry: UpstreamRequestLog) {}
    fn response_metadata(&self, _status: u16, _headers: &HeaderMap) {}
    fn response_chunk(&self, _data: &[u8]) {}
    fn response_error(&self, _err: &str) {}
    fn websocket_request(&self, _entry: UpstreamRequestLog) {}
    fn websocket_handshake(&self, _status: u16, _headers: &HeaderMap) {}
    fn websocket_response(&self, _data: &[u8]) {}
    fn websocket_error(&self, _stage: &str, _err: &str) {}
}
