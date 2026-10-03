//! Upstream request-log entries for the Codex executors (the `UpstreamRequestLog` Go builds
//! inline before each `helps.RecordAPIRequest` / `RecordAPIWebsocketRequest`).

use cpa_auth::Auth;
use cpa_runtime::executor::ExecError;
use http::HeaderMap;

use crate::helps::logging::UpstreamRequestLog;

/// Log entry for one outbound Codex request: auth identity from `auth.AccountInfo()`, provider
/// `codex`, headers and body exactly as sent.
pub(super) fn upstream_log(auth: &Auth, url: &str, method: &str, headers: &HeaderMap, body: &[u8]) -> UpstreamRequestLog {
    let (auth_type, auth_value) = auth.account_info();
    UpstreamRequestLog {
        url: url.to_string(),
        method: method.to_string(),
        headers: headers.clone(),
        body: body.to_vec(),
        provider: "codex".to_string(),
        auth_id: auth.id.clone(),
        auth_label: auth.label.clone(),
        auth_type: auth_type.to_string(),
        auth_value,
    }
}

/// Go `error.Error()` of an executor error: the message, or `status N` when it is empty.
pub(super) fn error_text(err: &ExecError) -> String {
    if err.message.is_empty() { format!("status {}", err.status) } else { err.message.clone() }
}
