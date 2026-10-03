//! The usage record an executor's reporter produces for one upstream attempt (Go:
//! `usage.Record`), and the collector the conductor hands executors through
//! [`Options`](crate::executor::Options) to receive it.
//!
//! Go executors publish records straight to a process-wide manager. Here the conductor attaches a
//! [`UsageCollector`] to each attempt's options, the executor's `UsageReporter` publishes into it,
//! and the conductor turns the collected [`Record`]s into [`UsageRecord`]s (adding the facts only
//! it knows: endpoint, client metadata, response headers). The report is therefore the single
//! source of tokens, response model and tier, reasoning effort, latency and failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::usage::{TokenUsage, UsageExtra, UsageFailure, UsageRecord};
use crate::usage_accounting::Detail;

/// HTTP failure facts of a failed attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failure {
    pub status_code: u16,
    pub body: String,
}

/// Usage statistics of one upstream attempt (Go: usage.Record).
#[derive(Debug, Clone)]
pub struct Record {
    pub request_id: String,
    pub trace_id: String,
    pub provider: String,
    pub base_url: String,
    pub executor_type: String,
    pub model: String,
    pub alias: String,
    pub api_key: String,
    pub session_id: String,
    pub parent_session_id: String,
    pub auth_id: String,
    pub auth_index: String,
    /// SHA-256 of the access token actually used, never the token.
    pub access_token_sha256: String,
    pub auth_type: String,
    pub source: String,
    /// Translated upstream thinking level; empty when the request carried none.
    pub reasoning_effort: String,
    pub service_tier: String,
    /// Final tier the upstream response reported; empty when unknown.
    pub response_service_tier: String,
    /// Model the upstream response reported; empty when unknown.
    pub response_model: String,
    pub generate: bool,
    pub stream: bool,
    pub requested_at: DateTime<Utc>,
    pub latency: Duration,
    pub ttft: Duration,
    pub failed: bool,
    pub fail: Failure,
    pub detail: Detail,
}

/// Receives the finished record of each reporter.
pub trait UsageSink: Send + Sync {
    fn publish(&self, record: Record);
}

/// Per-attempt record store: shared between the conductor (which drains it) and every reporter
/// the executor creates from the attempt's options.
#[derive(Clone, Default)]
pub struct UsageCollector(Arc<CollectorInner>);

#[derive(Default)]
struct CollectorInner {
    records: Mutex<Vec<Record>>,
    /// Set once an executor created a reporter on this collector: the executor reports its own
    /// usage, so the conductor need not scan the response for token counts.
    reporter_attached: AtomicBool,
}

impl UsageCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes and returns the records published so far, oldest first.
    pub fn take(&self) -> Vec<Record> {
        std::mem::take(&mut *self.0.records.lock())
    }

    /// Marks that an executor reporter publishes into this collector (called by the reporter).
    pub fn mark_reporter_attached(&self) {
        self.0.reporter_attached.store(true, Ordering::Release);
    }

    /// Whether an executor reporter publishes into this collector.
    pub fn has_reporter(&self) -> bool {
        self.0.reporter_attached.load(Ordering::Acquire)
    }
}

impl UsageSink for UsageCollector {
    fn publish(&self, record: Record) {
        self.0.records.lock().push(record);
    }
}

impl std::fmt::Debug for UsageCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UsageCollector")
    }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// `s` without surrounding whitespace, reusing the allocation when already trimmed.
fn trimmed(s: String) -> String {
    if s.trim().len() == s.len() { s } else { s.trim().to_string() }
}

impl Record {
    /// The tracker record this report describes, without conductor-only facts (endpoint, client
    /// metadata, response headers); callers add those (see `conductor::usage` for the full
    /// overlay). The usage queue's `execution_id` is the report's request id.
    pub fn to_usage_record(&self) -> UsageRecord {
        self.clone().into_usage_record()
    }

    /// [`to_usage_record`](Self::to_usage_record) consuming the report, so its strings move
    /// instead of being cloned.
    pub fn into_usage_record(self) -> UsageRecord {
        let tokens = TokenUsage::from_detail(&self.detail);
        let model = trimmed(self.model);
        let alias = trimmed(self.alias);
        let non_empty = |s: String| Some(trimmed(s)).filter(|s| !s.is_empty());
        UsageRecord {
            timestamp: self.requested_at,
            latency_ms: millis(self.latency),
            ttft_ms: millis(self.ttft),
            source: self.source.clone(),
            auth_index: self.auth_index,
            auth_type: self.auth_type,
            provider: self.provider,
            executor_type: self.executor_type,
            alias: if alias != model { alias } else { String::new() },
            model,
            endpoint: String::new(),
            api_key: self.api_key,
            request_id: String::new(),
            failed: self.failed,
            stream: self.stream,
            fail: UsageFailure { status_code: self.fail.status_code, body: self.fail.body },
            tokens,
            extra: UsageExtra {
                session_id: self.session_id,
                parent_session_id: self.parent_session_id,
                trace_id: self.trace_id,
                base_url: self.base_url,
                auth_id: self.auth_id,
                execution_id: self.request_id,
                response_service_tier: trimmed(self.response_service_tier),
                response_model: trimmed(self.response_model),
                detail: self.detail,
                queue_source: self.source,
                access_token_sha256: self.access_token_sha256,
                reasoning_effort: non_empty(self.reasoning_effort),
                service_tier: non_empty(self.service_tier),
                generate: Some(self.generate),
                ..Default::default()
            },
        }
    }
}
