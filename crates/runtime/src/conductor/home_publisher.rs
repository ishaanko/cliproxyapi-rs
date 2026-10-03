//! In-flight observation frames published to Home (Go: `home_in_flight_publisher.go`). Home
//! reconciles credential concurrency with the executions this node really has running; the
//! frames are bounded in size, count and string length and degrade to an overflow marker.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use cpa_config::{
    CredentialInFlightConfig, DEFAULT_IN_FLIGHT_MAX_AGGREGATE_GROUPS, DEFAULT_IN_FLIGHT_MAX_DETAILS,
    DEFAULT_IN_FLIGHT_MAX_PART_COUNT, DEFAULT_IN_FLIGHT_MAX_REVISION_BYTES, DEFAULT_IN_FLIGHT_MAX_STRING_BYTES,
};
use cpa_home::conn::Kill;
use cpa_home::executionregistry::{Freeze, Registry};
use cpa_home::requests::{
    InFlightAccountedStatus, InFlightAggregate, InFlightFrameKind, InFlightRequestDetail, InFlightSnapshotFrame,
};

use super::Manager;
use super::home_concurrency::valid_canonical_concurrency_model_key;

/// Bounds of one in-flight observation revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherConfig {
    pub snapshot_interval: Duration,
    pub max_part_bytes: i64,
    pub max_part_count: i64,
    pub max_revision_bytes: i64,
    pub max_aggregate_groups: i64,
    pub max_details: i64,
    pub max_string_bytes: i64,
}

impl PublisherConfig {
    /// Go: `HomeInFlightPublisherConfigFromConfig`.
    pub fn from_config(cfg: &CredentialInFlightConfig) -> Result<PublisherConfig, cpa_config::ConfigError> {
        let (snapshot, _, _) = cfg.durations()?;
        cfg.validate()?;
        Ok(PublisherConfig {
            snapshot_interval: snapshot.to_std(),
            max_part_bytes: cfg.max_part_bytes,
            max_part_count: cfg.max_part_count,
            max_revision_bytes: cfg.max_revision_bytes,
            max_aggregate_groups: cfg.max_aggregate_groups,
            max_details: cfg.max_details,
            max_string_bytes: cfg.max_string_bytes,
        })
    }

    /// Go: `validHomeInFlightPublisherConfig`.
    fn is_valid(&self) -> bool {
        if self.snapshot_interval.is_zero()
            || self.max_part_bytes < 1024
            || self.max_part_count <= 0
            || self.max_part_count > DEFAULT_IN_FLIGHT_MAX_PART_COUNT
            || self.max_revision_bytes < self.max_part_bytes
            || self.max_revision_bytes > DEFAULT_IN_FLIGHT_MAX_REVISION_BYTES
            || self.max_aggregate_groups <= 0
            || self.max_aggregate_groups > DEFAULT_IN_FLIGHT_MAX_AGGREGATE_GROUPS
            || self.max_details < 0
            || self.max_details > DEFAULT_IN_FLIGHT_MAX_DETAILS
            || self.max_string_bytes <= 0
            || self.max_string_bytes > DEFAULT_IN_FLIGHT_MAX_STRING_BYTES
        {
            return false;
        }
        (self.max_revision_bytes + self.max_part_bytes - 1) / self.max_part_bytes <= self.max_part_count
    }

    /// Go: `validHomeInFlightPublisherBounds`.
    fn bounds_valid(&self) -> bool {
        self.max_part_bytes > 0
            && self.max_part_count > 0
            && self.max_revision_bytes >= self.max_part_bytes
            && self.max_aggregate_groups > 0
            && self.max_details >= 0
            && self.max_string_bytes > 0
    }
}

/// The destination of snapshot frames (the Home client).
#[async_trait::async_trait]
pub trait InFlightTransport: Send + Sync {
    fn heartbeat_ok(&self) -> bool;
    async fn lpush_in_flight_snapshot(&self, payload: &[u8]) -> Result<(), cpa_home::HomeError>;
}

#[async_trait::async_trait]
impl InFlightTransport for cpa_home::Client {
    fn heartbeat_ok(&self) -> bool {
        cpa_home::Client::heartbeat_ok(self)
    }

    async fn lpush_in_flight_snapshot(&self, payload: &[u8]) -> Result<(), cpa_home::HomeError> {
        cpa_home::Client::lpush_in_flight_snapshot(self, payload).await
    }
}

impl Manager {
    /// Stores a validated publisher config snapshot; invalid configs are ignored.
    pub fn apply_home_in_flight_publisher_config(&self, cfg: PublisherConfig) {
        if cfg.is_valid() {
            *self.home.publisher_config.write() = Some(cfg);
        }
    }

    pub fn home_in_flight_publisher_config(&self) -> Option<PublisherConfig> {
        *self.home.publisher_config.read()
    }

    /// Publishes periodic snapshots of `registry` until `cancel` fires (Go:
    /// `StartHomeInFlightPublisher`).
    pub async fn start_home_in_flight_publisher(
        &self,
        cancel: &Arc<Kill>,
        transport: Arc<dyn InFlightTransport>,
        registry: Registry,
    ) {
        let mut next = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = cancel.wait() => return,
                _ = tokio::time::sleep_until(next) => {}
            }
            let cfg = self.home_in_flight_publisher_config().unwrap_or(PublisherConfig {
                snapshot_interval: Duration::from_secs(2),
                max_part_bytes: 0,
                max_part_count: 0,
                max_revision_bytes: 0,
                max_aggregate_groups: 0,
                max_details: 0,
                max_string_bytes: 0,
            });
            let interval = if cfg.snapshot_interval.is_zero() { Duration::from_secs(2) } else { cfg.snapshot_interval };
            next = tokio::time::Instant::now() + interval;
            if !transport.heartbeat_ok() {
                continue;
            }
            let observed_at = Utc::now();
            let freeze = registry.freeze_in_flight();
            let frames = encode_freeze(&freeze, observed_at, &cfg);
            for frame in &frames {
                let Ok(raw) = serde_json::to_vec(frame) else {
                    tracing::warn!("failed to encode in-flight snapshot frame");
                    break;
                };
                if transport.lpush_in_flight_snapshot(&raw).await.is_err() {
                    tracing::warn!("failed to publish in-flight snapshot frame");
                    break;
                }
            }
        }
    }
}

type AggregateKey = (String, String, bool);

fn status(accounted: bool) -> InFlightAccountedStatus {
    if accounted { InFlightAccountedStatus::Accounted } else { InFlightAccountedStatus::Unaccounted }
}

fn observation_model(model: &str, accounted: bool) -> String {
    if accounted {
        return model.to_string();
    }
    let (key, valid) = valid_canonical_concurrency_model_key(model);
    if valid { key } else { "unknown".into() }
}

fn to_utc(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Longest prefix of `value` within `max_bytes` on a char boundary.
fn truncate_string(value: &str, max_bytes: i64) -> String {
    if max_bytes <= 0 || value.len() as i64 <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes as usize;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn valid_detail(d: &InFlightRequestDetail, max: i64) -> bool {
    let ok = |s: &str| !s.trim().is_empty() && s.len() as i64 <= max;
    ok(&d.request_id) && ok(&d.credential_id) && ok(&d.model) && ok(&d.request_kind) && d.started_at.timestamp() != 0
}

fn bound_detail(mut d: InFlightRequestDetail, max: i64) -> (InFlightRequestDetail, bool) {
    let mut truncated = false;
    let mut bound = |v: &mut String| {
        let b = truncate_string(v, max);
        truncated |= b != *v;
        *v = b;
    };
    bound(&mut d.request_id);
    bound(&mut d.credential_id);
    bound(&mut d.model);
    bound(&mut d.request_kind);
    (d, truncated)
}

fn frame_len(frame: &InFlightSnapshotFrame) -> Option<usize> {
    serde_json::to_vec(frame).ok().map(|raw| raw.len())
}

fn part_frame(freeze: &Freeze, observed_at: DateTime<Utc>, part_count: i64, details_truncated: bool) -> InFlightSnapshotFrame {
    InFlightSnapshotFrame {
        kind: InFlightFrameKind::Part,
        revision: freeze.revision,
        observed_at,
        barrier_revision: freeze.barrier_revision,
        part_index: Some(0),
        part_count: Some(part_count),
        details_truncated,
        aggregates: Vec::new(),
        details: Vec::new(),
        aggregate_group_count: 0,
    }
}

fn overflow(freeze: &Freeze, observed_at: DateTime<Utc>, aggregate_groups: usize) -> Vec<InFlightSnapshotFrame> {
    vec![InFlightSnapshotFrame {
        kind: InFlightFrameKind::Overflow,
        revision: freeze.revision,
        observed_at,
        barrier_revision: freeze.barrier_revision,
        part_index: None,
        part_count: None,
        details_truncated: false,
        aggregates: Vec::new(),
        details: Vec::new(),
        aggregate_group_count: aggregate_groups,
    }]
}

fn within_part_limit(frame: &InFlightSnapshotFrame, max_part_bytes: i64) -> bool {
    frame_len(frame).is_some_and(|n| n as i64 <= max_part_bytes)
}

fn frames_within_bounds(frames: &[InFlightSnapshotFrame], cfg: &PublisherConfig) -> bool {
    if frames.is_empty() || frames.len() as i64 > cfg.max_part_count {
        return false;
    }
    let mut total: i64 = 0;
    for frame in frames {
        let Some(n) = frame_len(frame) else { return false };
        if n as i64 > cfg.max_part_bytes {
            return false;
        }
        total += n as i64;
        if total > cfg.max_revision_bytes {
            return false;
        }
    }
    true
}

/// Splits aggregates then details into parts of at most `max_part_bytes`. Returns the frames,
/// whether every aggregate fit, and how many details were included (Go: `packHomeInFlightFrames`).
fn pack_frames(
    freeze: &Freeze,
    observed_at: DateTime<Utc>,
    cfg: &PublisherConfig,
    aggregates: &[InFlightAggregate],
    details: &[InFlightRequestDetail],
    details_truncated: bool,
) -> (Vec<InFlightSnapshotFrame>, bool, usize) {
    let mut frames: Vec<InFlightSnapshotFrame> = Vec::new();
    let mut current = part_frame(freeze, observed_at, cfg.max_part_count, details_truncated);
    macro_rules! append_current {
        () => {{
            if frames.len() as i64 >= cfg.max_part_count {
                false
            } else {
                frames.push(std::mem::replace(
                    &mut current,
                    part_frame(freeze, observed_at, cfg.max_part_count, details_truncated),
                ));
                true
            }
        }};
    }
    for aggregate in aggregates {
        let mut candidate = current.clone();
        candidate.aggregates.push(aggregate.clone());
        if within_part_limit(&candidate, cfg.max_part_bytes) {
            current = candidate;
            continue;
        }
        if current.aggregates.is_empty() && current.details.is_empty() {
            return (Vec::new(), false, 0);
        }
        if !append_current!() {
            return (Vec::new(), false, 0);
        }
        let mut candidate = current.clone();
        candidate.aggregates.push(aggregate.clone());
        if !within_part_limit(&candidate, cfg.max_part_bytes) {
            return (Vec::new(), false, 0);
        }
        current = candidate;
    }

    let mut included = 0usize;
    for detail in details {
        let mut candidate = current.clone();
        candidate.details.push(detail.clone());
        if within_part_limit(&candidate, cfg.max_part_bytes) {
            current = candidate;
            included += 1;
            continue;
        }
        if current.aggregates.is_empty() && current.details.is_empty() {
            return (frames, true, included);
        }
        let in_current = current.details.len();
        if !append_current!() {
            return (frames, true, included - in_current);
        }
        let mut candidate = current.clone();
        candidate.details.push(detail.clone());
        if !within_part_limit(&candidate, cfg.max_part_bytes) {
            return (frames, true, included);
        }
        current = candidate;
        included += 1;
    }
    if !current.aggregates.is_empty() || !current.details.is_empty() || frames.is_empty() {
        let in_current = current.details.len();
        if !append_current!() {
            if !current.aggregates.is_empty() {
                return (Vec::new(), false, 0);
            }
            return (frames, true, included - in_current);
        }
    }
    let count = frames.len() as i64;
    for (index, frame) in frames.iter_mut().enumerate() {
        frame.part_index = Some(index as i64);
        frame.part_count = Some(count);
    }
    (frames, true, included)
}

/// Go: `encodeHomeInFlightFreeze`.
pub(crate) fn encode_freeze(freeze: &Freeze, observed_at: DateTime<Utc>, cfg: &PublisherConfig) -> Vec<InFlightSnapshotFrame> {
    let mut counts: BTreeMap<AggregateKey, i64> = BTreeMap::new();
    let mut keys_valid = true;
    for obs in &freeze.executions {
        let key = (obs.credential_id.clone(), observation_model(&obs.model, obs.accounted), obs.accounted);
        if key.0.len() as i64 > cfg.max_string_bytes || key.1.len() as i64 > cfg.max_string_bytes {
            keys_valid = false;
        }
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut aggregates: Vec<InFlightAggregate> = counts
        .into_iter()
        .map(|((credential_id, model, accounted), count)| InFlightAggregate {
            credential_id,
            model,
            status: status(accounted),
            count,
        })
        .collect();
    aggregates.sort_by(|a, b| {
        (a.credential_id.as_str(), a.model.as_str(), a.status).cmp(&(b.credential_id.as_str(), b.model.as_str(), b.status))
    });
    if !cfg.bounds_valid() || !keys_valid || aggregates.len() as i64 > cfg.max_aggregate_groups {
        return overflow(freeze, observed_at, aggregates.len());
    }

    let mut details: Vec<InFlightRequestDetail> = Vec::with_capacity(freeze.executions.len());
    let mut details_truncated = false;
    for obs in &freeze.executions {
        let (detail, bounded) = bound_detail(
            InFlightRequestDetail {
                request_id: obs.request_id.clone(),
                credential_id: obs.credential_id.clone(),
                model: observation_model(&obs.model, obs.accounted),
                request_kind: obs.request_kind.clone(),
                started_at: obs.started_at.map(to_utc).unwrap_or_default(),
            },
            cfg.max_string_bytes,
        );
        if !valid_detail(&detail, cfg.max_string_bytes) {
            details_truncated = true;
            continue;
        }
        details_truncated |= bounded;
        details.push(detail);
    }
    details.sort_by(|a, b| {
        (a.started_at, &a.request_id, &a.credential_id, &a.model, &a.request_kind).cmp(&(
            b.started_at,
            &b.request_id,
            &b.credential_id,
            &b.model,
            &b.request_kind,
        ))
    });
    if details.len() as i64 > cfg.max_details {
        details.truncate(cfg.max_details as usize);
        details_truncated = true;
    }

    loop {
        let (frames, aggregates_packed, included) =
            pack_frames(freeze, observed_at, cfg, &aggregates, &details, details_truncated);
        if !aggregates_packed {
            return overflow(freeze, observed_at, aggregates.len());
        }
        if included < details.len() {
            details.truncate(included);
            details_truncated = true;
            continue;
        }
        if frames_within_bounds(&frames, cfg) {
            return frames;
        }
        if details.is_empty() {
            return overflow(freeze, observed_at, aggregates.len());
        }
        details.pop();
        details_truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_home::executionregistry::Observation;

    fn cfg() -> PublisherConfig {
        PublisherConfig::from_config(&CredentialInFlightConfig::parse_defaults()).unwrap()
    }

    fn obs(id: &str, cred: &str, model: &str, accounted: bool) -> Observation {
        Observation {
            request_id: id.into(),
            credential_id: cred.into(),
            model: model.into(),
            request_kind: "http".into(),
            started_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            accounted,
        }
    }

    fn freeze(executions: Vec<Observation>) -> Freeze {
        Freeze { revision: 3, barrier_revision: 2, executions }
    }

    #[test]
    fn default_config_is_valid_and_bad_ones_are_rejected() {
        assert!(cfg().is_valid());
        let mut c = cfg();
        c.max_part_bytes = 10;
        assert!(!c.is_valid());
        let mut c = cfg();
        c.max_part_count = 1;
        assert!(!c.is_valid(), "revision cannot fit in one part");
    }

    #[test]
    fn aggregates_count_per_group_with_sorted_output_and_unaccounted_models_canonicalized() {
        let f = freeze(vec![
            obs("r2", "b", "gpt-5", true),
            obs("r1", "a", "gpt-5", true),
            obs("r3", "a", "gpt-5", true),
            obs("r4", "a", "GPT-5(high)", false),
        ]);
        let frames = encode_freeze(&f, Utc::now(), &cfg());
        assert_eq!(frames.len(), 1);
        let fr = &frames[0];
        assert_eq!((fr.kind, fr.revision, fr.barrier_revision, fr.part_index, fr.part_count), (InFlightFrameKind::Part, 3, 2, Some(0), Some(1)));
        let groups: Vec<_> = fr.aggregates.iter().map(|a| (a.credential_id.as_str(), a.model.as_str(), a.status, a.count)).collect();
        assert_eq!(
            groups,
            vec![
                ("a", "gpt-5", InFlightAccountedStatus::Accounted, 2),
                ("a", "gpt-5", InFlightAccountedStatus::Unaccounted, 1),
                ("b", "gpt-5", InFlightAccountedStatus::Accounted, 1),
            ]
        );
        let ids: Vec<_> = fr.details.iter().map(|d| d.request_id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2", "r3", "r4"]);
    }

    #[test]
    fn too_many_groups_or_oversized_keys_overflow() {
        let mut c = cfg();
        c.max_aggregate_groups = 1;
        let f = freeze(vec![obs("r1", "a", "m", true), obs("r2", "b", "m", true)]);
        let frames = encode_freeze(&f, Utc::now(), &c);
        assert_eq!((frames[0].kind, frames[0].aggregate_group_count), (InFlightFrameKind::Overflow, 2));
        let f = freeze(vec![obs("r1", &"x".repeat(300), "m", true)]);
        assert_eq!(encode_freeze(&f, Utc::now(), &cfg())[0].kind, InFlightFrameKind::Overflow);
    }

    #[test]
    fn details_are_bounded_and_flagged_truncated() {
        let mut c = cfg();
        c.max_details = 2;
        let f = freeze((0..5).map(|i| obs(&format!("r{i}"), "a", "m", true)).collect());
        let frames = encode_freeze(&f, Utc::now(), &c);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].details_truncated);
        assert_eq!(frames[0].details.len(), 2);
        assert_eq!(frames[0].aggregates[0].count, 5);
    }

    #[test]
    fn large_snapshots_split_into_parts_within_the_part_limit() {
        let mut c = cfg();
        c.max_part_bytes = 1024;
        c.max_part_count = 64;
        let f = freeze((0..40).map(|i| obs(&format!("request-{i:04}"), "cred", "model", true)).collect());
        let frames = encode_freeze(&f, Utc::now(), &c);
        assert!(frames.len() > 1);
        let count = frames.len() as i64;
        for (i, fr) in frames.iter().enumerate() {
            assert_eq!((fr.part_index, fr.part_count), (Some(i as i64), Some(count)));
            assert!(serde_json::to_vec(fr).unwrap().len() <= 1024);
        }
        let included: usize = frames.iter().map(|f| f.details.len()).sum();
        assert!(included > 0 && included <= 40);
    }

    #[test]
    fn empty_freeze_publishes_one_empty_part() {
        let frames = encode_freeze(&freeze(vec![]), Utc::now(), &cfg());
        assert_eq!(frames.len(), 1);
        assert!(frames[0].aggregates.is_empty() && frames[0].details.is_empty());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_string("héllo", 2), "h");
        assert_eq!(truncate_string("abc", 10), "abc");
    }
}
