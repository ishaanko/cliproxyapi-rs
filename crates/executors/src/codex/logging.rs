//! Log helpers of the Codex executors.

use cpa_runtime::executor::ExecError;

/// Go `error.Error()` of an executor error: the message, or `status N` when it is empty.
pub(super) fn error_text(err: &ExecError) -> String {
    if err.message.is_empty() { format!("status {}", err.status) } else { err.message.clone() }
}
