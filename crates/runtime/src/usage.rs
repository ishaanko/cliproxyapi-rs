//! Usage accounting (Go: sdk/cliproxy/usage + the record built in redisqueue/plugin.go).
//!
//! The conductor builds one [`UsageRecord`] per completed upstream execution and calls
//! [`UsageTracker::record`]. The management API reads aggregates and the recent-request
//! ring buffer (shapes in ui/API_EXTENSIONS.md). Memory only; resets on restart.

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cached_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageFailure {
    pub status_code: u16,
    /// Truncated to 2 KiB.
    pub body: String,
}

/// One usage event (field names match the Go usage-queue record).
#[derive(Debug, Clone, Serialize)]
pub struct UsageRecord {
    pub timestamp: DateTime<Utc>,
    pub latency_ms: i64,
    /// 0 for non-streaming requests.
    pub ttft_ms: i64,
    /// Credential label (email or key alias).
    pub source: String,
    pub auth_index: String,
    pub auth_type: String,
    pub provider: String,
    pub executor_type: String,
    /// Upstream model.
    pub model: String,
    /// Client-visible model alias, empty when same as `model`.
    pub alias: String,
    /// `"<METHOD> <path>"`.
    pub endpoint: String,
    pub api_key: String,
    pub request_id: String,
    pub failed: bool,
    pub stream: bool,
    pub fail: UsageFailure,
    pub tokens: TokenUsage,
}

/// Process-wide usage store. Implemented by the management/usage port.
#[derive(Default)]
pub struct UsageTracker {}

impl UsageTracker {
    pub fn record(&self, _record: UsageRecord) {
        todo!("usage port")
    }
}
