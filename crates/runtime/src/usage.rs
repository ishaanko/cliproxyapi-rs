//! Usage accounting (Go: sdk/cliproxy/usage + the record built in redisqueue/plugin.go).
//!
//! Recording happens in the conductor (`conductor::Manager`): once per completed upstream
//! execution, whether it succeeded or failed, it builds a [`UsageRecord`] and calls
//! [`UsageTracker::record`], independently of any queue or consumer. Nothing is recorded until
//! the conductor is wired to a tracker. The service should mirror `usage-statistics-enabled`
//! into [`UsageTracker::set_enabled`] at start and on every config reload. The management API
//! reads aggregates ([`UsageTracker::summary`]) and
//! the recent-request ring buffer ([`UsageTracker::requests`]); shapes are specified in
//! ui/API_EXTENSIONS.md. Memory only; resets on restart.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Serialize, Serializer};

/// Events kept in the request ring buffer.
pub const REQUEST_RING_CAPACITY: usize = 1000;
/// Upstream failure bodies are truncated to this many bytes.
const FAIL_BODY_MAX_BYTES: usize = 2048;
const HOUR_SECS: i64 = 3600;
/// Buckets reported by [`UsageSummary::hourly`].
const HOURLY_BUCKETS: i64 = 24;
/// Older hourly buckets are dropped from memory.
const HOURLY_RETAIN: i64 = 48;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cached_tokens: i64,
    pub total_tokens: i64,
}

impl TokenUsage {
    fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cached_tokens += other.cached_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageFailure {
    pub status_code: u16,
    /// Truncated to 2 KiB.
    pub body: String,
}

/// Request context a usage record carries for the usage queue (Go: `ClientRequestMetadata`,
/// the trace id and the upstream response headers). Not part of the aggregate/ring-buffer JSON.
#[derive(Debug, Clone, Default)]
pub struct UsageExtra {
    pub client_ip: String,
    pub resolved_client_ip: String,
    pub x_forwarded_for: String,
    pub user_agent: String,
    /// Canonical UUID.
    pub session_id: String,
    pub parent_session_id: String,
    pub trace_id: String,
    pub response_headers: http::HeaderMap,
    /// Account the usage queue reports as the record's `source` (API key or e-mail); empty
    /// falls back to `UsageRecord::source`.
    pub queue_source: String,
}

/// One usage event (field names match the Go usage-queue record).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageRecord {
    #[serde(serialize_with = "serialize_ts")]
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
    #[serde(skip)]
    pub extra: UsageExtra,
}

/// A [`UsageRecord`] in the ring buffer: the record plus its sequence number. Serializes to the
/// event shape of `GET /observability/requests`.
#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub record: UsageRecord,
}

/// UTC RFC 3339 without a fraction when it is zero, else milliseconds.
fn format_ts(t: DateTime<Utc>) -> String {
    let fmt = if t.timestamp_subsec_nanos() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Millis
    };
    t.to_rfc3339_opts(fmt, true)
}

fn serialize_ts<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_ts(*t))
}

/// Requests, failures and tokens of one aggregation key.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UsageAgg {
    pub requests: u64,
    pub failed: u64,
    pub tokens: TokenUsage,
}

impl UsageAgg {
    fn bump(&mut self, r: &UsageRecord) {
        self.requests += 1;
        if r.failed {
            self.failed += 1;
        }
        self.tokens.add(&r.tokens);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub agg: UsageAgg,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialUsage {
    pub auth_index: String,
    /// Hints from the latest record of this credential.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(flatten)]
    pub agg: UsageAgg,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyUsage {
    pub api_key: String,
    #[serde(flatten)]
    pub agg: UsageAgg,
}

#[derive(Debug, Clone, Serialize)]
pub struct HourlyUsage {
    /// Top of the UTC hour.
    pub hour: String,
    #[serde(flatten)]
    pub agg: UsageAgg,
}

/// `GET /observability/usage/summary`.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub since: String,
    pub totals: UsageAgg,
    pub models: Vec<ModelUsage>,
    pub credentials: Vec<CredentialUsage>,
    pub api_keys: Vec<ApiKeyUsage>,
    /// Exactly 24 buckets, oldest first, zero filled, ending with the current hour.
    pub hourly: Vec<HourlyUsage>,
}

/// `GET /observability/requests`.
#[derive(Debug, Clone, Serialize)]
pub struct RequestsPage {
    /// Highest assigned sequence number, 0 when nothing was recorded.
    pub seq: u64,
    pub started_at: String,
    pub capacity: usize,
    pub has_more: bool,
    pub events: Vec<UsageEvent>,
}

#[derive(Default)]
struct CredentialAgg {
    agg: UsageAgg,
    provider: String,
    source: String,
}

#[derive(Default)]
struct State {
    /// Highest assigned sequence number.
    seq: u64,
    ring: VecDeque<UsageEvent>,
    totals: UsageAgg,
    models: HashMap<String, UsageAgg>,
    credentials: HashMap<String, CredentialAgg>,
    api_keys: HashMap<String, UsageAgg>,
    /// Hour start (unix seconds) to aggregate.
    hourly: BTreeMap<i64, UsageAgg>,
}

/// Observer of every recorded event (the usage queue).
pub type UsageSink = std::sync::Arc<dyn Fn(&UsageRecord) + Send + Sync>;

/// Process-wide usage store: counters plus a ring buffer of recent events.
pub struct UsageTracker {
    enabled: AtomicBool,
    sink: Mutex<Option<UsageSink>>,
    started_at: DateTime<Utc>,
    state: Mutex<State>,
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "unknown" } else { s }
}

/// Aggregates sorted by request count, busiest first (ties by key for stable output).
fn sorted_by_requests<T>(
    mut rows: Vec<(String, T)>,
    requests: impl Fn(&T) -> u64,
) -> Vec<(String, T)> {
    rows.sort_by(|a, b| {
        requests(&b.1)
            .cmp(&requests(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    rows
}

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

impl UsageTracker {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            sink: Mutex::new(None),
            started_at: Utc::now(),
            state: Mutex::new(State::default()),
        }
    }

    /// Process start time; doubles as the instance id of the request feed.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn capacity(&self) -> usize {
        REQUEST_RING_CAPACITY
    }

    /// Mirrors `usage-statistics-enabled`: while false, `record` drops events. Defaults to true.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Installs (or clears) the sink that sees every recorded event.
    pub fn set_sink(&self, sink: Option<UsageSink>) {
        *self.sink.lock() = sink;
    }

    /// Records one usage event: aggregates it and appends it to the ring buffer.
    pub fn record(&self, mut record: UsageRecord) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let sink = self.sink.lock().clone();
        if let Some(sink) = sink {
            sink(&record);
        }
        truncate_utf8(&mut record.fail.body, FAIL_BODY_MAX_BYTES);
        let hour = record.timestamp.timestamp().div_euclid(HOUR_SECS) * HOUR_SECS;
        let mut st = self.state.lock();

        st.totals.bump(&record);
        let model = if !record.alias.is_empty() {
            &record.alias
        } else {
            &record.model
        };
        st.models
            .entry(or_unknown(model).to_string())
            .or_default()
            .bump(&record);
        let cred = st
            .credentials
            .entry(or_unknown(&record.auth_index).to_string())
            .or_default();
        cred.agg.bump(&record);
        if !record.provider.is_empty() {
            cred.provider.clone_from(&record.provider);
        }
        if !record.source.is_empty() {
            cred.source.clone_from(&record.source);
        }
        st.api_keys
            .entry(or_unknown(&record.api_key).to_string())
            .or_default()
            .bump(&record);
        st.hourly.entry(hour).or_default().bump(&record);
        let cutoff =
            Utc::now().timestamp().div_euclid(HOUR_SECS) * HOUR_SECS - HOURLY_RETAIN * HOUR_SECS;
        while st
            .hourly
            .first_key_value()
            .is_some_and(|(h, _)| *h < cutoff)
        {
            st.hourly.pop_first();
        }

        st.seq += 1;
        let seq = st.seq;
        st.ring.push_back(UsageEvent { seq, record });
        while st.ring.len() > REQUEST_RING_CAPACITY {
            st.ring.pop_front();
        }
    }

    /// Aggregates since process start. `hourly` is aligned to the UTC hour of `now`.
    pub fn summary(&self) -> UsageSummary {
        self.summary_at(Utc::now())
    }

    fn summary_at(&self, now: DateTime<Utc>) -> UsageSummary {
        let st = self.state.lock();
        let models = sorted_by_requests(
            st.models
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            |a| a.requests,
        )
        .into_iter()
        .map(|(model, agg)| ModelUsage { model, agg })
        .collect();
        let credentials = sorted_by_requests(
            st.credentials
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        (v.agg.clone(), v.provider.clone(), v.source.clone()),
                    )
                })
                .collect(),
            |v| v.0.requests,
        )
        .into_iter()
        .map(|(auth_index, (agg, provider, source))| CredentialUsage {
            auth_index,
            provider,
            source,
            agg,
        })
        .collect();
        let api_keys = sorted_by_requests(
            st.api_keys
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            |a| a.requests,
        )
        .into_iter()
        .map(|(api_key, agg)| ApiKeyUsage { api_key, agg })
        .collect();

        let now_hour = now.timestamp().div_euclid(HOUR_SECS) * HOUR_SECS;
        let hourly = (0..HOURLY_BUCKETS)
            .map(|i| {
                let start = now_hour - (HOURLY_BUCKETS - 1 - i) * HOUR_SECS;
                let hour = DateTime::from_timestamp(start, 0)
                    .map(format_ts)
                    .unwrap_or_default();
                HourlyUsage {
                    hour,
                    agg: st.hourly.get(&start).cloned().unwrap_or_default(),
                }
            })
            .collect();

        UsageSummary {
            since: format_ts(self.started_at),
            totals: st.totals.clone(),
            models,
            credentials,
            api_keys,
            hourly,
        }
    }

    /// Ring buffer view. `limit` is clamped to `1..=capacity`.
    ///
    /// Without `after`: the newest `limit` events, oldest first, `has_more` false. With `after`:
    /// the oldest `limit` events with `seq > after`, oldest first, `has_more` when more follow,
    /// so a client can page forward without gaps. An `after` older than the oldest retained event
    /// returns what is retained.
    pub fn requests(&self, limit: usize, after: Option<u64>) -> RequestsPage {
        let limit = limit.clamp(1, REQUEST_RING_CAPACITY);
        let st = self.state.lock();
        let (events, has_more) = match after {
            None => {
                let skip = st.ring.len().saturating_sub(limit);
                (st.ring.iter().skip(skip).cloned().collect(), false)
            }
            Some(after) => {
                // The ring is ordered by seq.
                let start = st.ring.partition_point(|e| e.seq <= after);
                let newer = st.ring.len() - start;
                (
                    st.ring.iter().skip(start).take(limit).cloned().collect(),
                    newer > limit,
                )
            }
        };
        RequestsPage {
            seq: st.seq,
            started_at: format_ts(self.started_at),
            capacity: REQUEST_RING_CAPACITY,
            has_more,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(model: &str, failed: bool) -> UsageRecord {
        UsageRecord {
            timestamp: Utc::now(),
            latency_ms: 10,
            ttft_ms: 0,
            source: "a@b.c".into(),
            auth_index: "idx1".into(),
            auth_type: "oauth".into(),
            provider: "codex".into(),
            executor_type: "codex".into(),
            model: model.into(),
            alias: String::new(),
            endpoint: "POST /v1/responses".into(),
            api_key: "sk-1".into(),
            request_id: "req".into(),
            failed,
            stream: false,
            fail: UsageFailure::default(),
            extra: Default::default(),
            tokens: TokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                total_tokens: 5,
                ..Default::default()
            },
        }
    }

    fn seqs(p: &RequestsPage) -> Vec<u64> {
        p.events.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn newest_without_after_and_forward_paging_with_after() {
        let t = UsageTracker::new();
        for _ in 0..10 {
            t.record(rec("m", false));
        }
        let p = t.requests(4, None);
        assert_eq!(
            (seqs(&p), p.has_more, p.seq),
            (vec![7, 8, 9, 10], false, 10)
        );

        // Oldest `limit` after the cursor, then follow the last returned seq.
        let p = t.requests(4, Some(0));
        assert_eq!((seqs(&p), p.has_more), (vec![1, 2, 3, 4], true));
        let p = t.requests(4, Some(4));
        assert_eq!((seqs(&p), p.has_more), (vec![5, 6, 7, 8], true));
        let p = t.requests(4, Some(8));
        assert_eq!((seqs(&p), p.has_more), (vec![9, 10], false));
        let p = t.requests(4, Some(10));
        assert!(p.events.is_empty() && !p.has_more);
    }

    #[test]
    fn ring_keeps_newest_capacity_and_after_older_than_oldest_returns_retained() {
        let t = UsageTracker::new();
        for _ in 0..(REQUEST_RING_CAPACITY + 5) {
            t.record(rec("m", false));
        }
        let p = t.requests(1000, None);
        assert_eq!(p.events.len(), REQUEST_RING_CAPACITY);
        assert_eq!(p.events[0].seq, 6);
        let p = t.requests(3, Some(2));
        assert_eq!((seqs(&p), p.has_more), (vec![6, 7, 8], true));
        // limit is clamped to 1..=capacity
        assert_eq!(t.requests(0, None).events.len(), 1);
    }

    #[test]
    fn summary_aggregates_alias_unknown_keys_and_zero_filled_hours() {
        let t = UsageTracker::new();
        let mut a = rec("gpt-5", false);
        a.alias = "my-alias".into();
        t.record(a);
        t.record(rec("gpt-5", true));
        let mut b = rec("", false);
        b.auth_index.clear();
        b.api_key.clear();
        t.record(b);

        let s = t.summary();
        assert_eq!(
            (
                s.totals.requests,
                s.totals.failed,
                s.totals.tokens.total_tokens
            ),
            (3, 1, 15)
        );
        let models: Vec<_> = s
            .models
            .iter()
            .map(|m| (m.model.as_str(), m.agg.requests))
            .collect();
        assert_eq!(models.len(), 3);
        assert!(
            models.contains(&("my-alias", 1))
                && models.contains(&("gpt-5", 1))
                && models.contains(&("unknown", 1))
        );
        assert_eq!(s.credentials[0].auth_index, "idx1");
        assert_eq!(s.credentials[0].provider, "codex");
        assert!(s.api_keys.iter().any(|k| k.api_key == "unknown"));
        assert_eq!(s.hourly.len(), 24);
        assert_eq!(s.hourly[23].agg.requests, 3);
        assert!(s.hourly[..23].iter().all(|h| h.agg.requests == 0));
        assert!(s.hourly[23].hour.ends_with(":00:00Z"));
    }

    #[test]
    fn failure_body_truncates_on_char_boundary() {
        let t = UsageTracker::new();
        let mut r = rec("m", true);
        r.fail = UsageFailure {
            status_code: 500,
            body: "é".repeat(2000),
        };
        t.record(r);
        let body = &t.requests(1, None).events[0].record.fail.body;
        assert!(body.len() <= FAIL_BODY_MAX_BYTES && body.chars().all(|c| c == 'é'));
    }

    #[test]
    fn disabled_tracker_drops_events() {
        let t = UsageTracker::new();
        t.set_enabled(false);
        t.record(rec("m", false));
        assert_eq!(t.requests(10, None).seq, 0);
    }
}
