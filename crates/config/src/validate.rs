//! Validation rules (ports of `trusted_proxies.go`, `weight.go`, `credential_concurrency.go`,
//! `credential_in_flight.go`, `codex_live.go`).

use std::net::IpAddr;

use crate::duration::GoDuration;
use crate::error::{ConfigError, Result};
use crate::layout::MAX_CREDENTIAL_WEIGHT;
use crate::types::*;

fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(ConfigError::invalid(msg))
}

/// Each entry must be an exact IP or a CIDR, with no surrounding whitespace.
pub fn validate_trusted_proxies(trusted_proxies: &[String]) -> Result<()> {
    for entry in trusted_proxies {
        if entry.is_empty() || entry.trim() != entry {
            return invalid(format!(
                "invalid trusted-proxies entry {entry:?}: expected an IP address or CIDR"
            ));
        }
        if entry.parse::<IpAddr>().is_ok() {
            continue;
        }
        if let Err(reason) = parse_cidr(entry) {
            return invalid(format!("invalid trusted-proxies entry {entry:?}: {reason}"));
        }
    }
    Ok(())
}

fn parse_cidr(s: &str) -> std::result::Result<(), String> {
    let invalid = || format!("invalid CIDR address: {s}");
    let (ip, prefix) = s.split_once('/').ok_or_else(invalid)?;
    let ip: IpAddr = ip.parse().map_err(|_| invalid())?;
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let prefix: u32 = prefix.parse().map_err(|_| invalid())?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some(()).ok_or_else(invalid)
}

/// Weights above 1,000,000 are rejected; non-positive values are valid (they exclude the
/// credential from weighted routing).
pub fn validate_credential_weight(weight: Option<i64>) -> Result<()> {
    match weight {
        Some(w) if w > MAX_CREDENTIAL_WEIGHT => {
            invalid(format!("weight must not exceed {MAX_CREDENTIAL_WEIGHT}"))
        }
        _ => Ok(()),
    }
}

impl Config {
    /// Validates weights for every API-key family.
    pub fn validate_credential_weights(&self) -> Result<()> {
        fn check(name: &str, weights: impl Iterator<Item = Option<i64>>) -> Result<()> {
            for (index, weight) in weights.enumerate() {
                validate_credential_weight(weight)
                    .map_err(|e| ConfigError::invalid(format!("{name}[{index}].weight: {e}")))?;
            }
            Ok(())
        }
        check("gemini-api-key", self.gemini_key.iter().map(|k| k.weight))?;
        check(
            "interactions-api-key",
            self.interactions_key.iter().map(|k| k.weight),
        )?;
        check("claude-api-key", self.claude_key.iter().map(|k| k.weight))?;
        check(
            "vertex-api-key",
            self.vertex_compat_api_key.iter().map(|k| k.weight),
        )?;
        check("codex-api-key", self.codex_key.iter().map(|k| k.weight))?;
        check("xai-api-key", self.xai_key.iter().map(|k| k.weight))?;
        check("meta-api-key", self.meta_key.iter().map(|k| k.weight))?;
        for (provider_index, provider) in self.openai_compatibility.iter().enumerate() {
            check(
                &format!("openai-compatibility[{provider_index}].api-key-entries"),
                provider.api_key_entries.iter().map(|k| k.weight),
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Credential concurrency (Home lifecycle)
// ---------------------------------------------------------------------------------------------

const DEFAULT_CPA_HEARTBEAT_TIMEOUT: GoDuration = GoDuration::from_secs(3);
const DEFAULT_CPA_CANCEL_BOUND: GoDuration = GoDuration::from_secs(5);
const DEFAULT_RECLAIM_GRACE: GoDuration = GoDuration::from_secs(5);
const DEFAULT_CLEANUP_INTERVAL: GoDuration = GoDuration::from_secs(5);
const DEFAULT_RELEASE_FLUSH_INTERVAL: GoDuration = GoDuration::from_millis(250);
const DEFAULT_RELEASE_MAX_BACKOFF: GoDuration = GoDuration::from_secs(2);
const DEFAULT_BUSY_RETRY_MIN: GoDuration = GoDuration::from_millis(250);
const DEFAULT_BUSY_RETRY_MAX: GoDuration = GoDuration::from_secs(1);
const MAX_CREDENTIAL_CONCURRENCY_LIMIT: i64 = 1_000_000;

impl CredentialConcurrencyConfig {
    /// Applies the lifecycle defaults required for older Home versions: only fields that were
    /// absent from the YAML (and are zero) get a default.
    pub fn with_defaults(mut self) -> Self {
        let p = self.present;
        let fill = |present: bool, value: &mut GoDuration, default: GoDuration| {
            if !present && value.0 == 0 {
                *value = default;
            }
        };
        fill(
            p.cpa_heartbeat_timeout,
            &mut self.cpa_heartbeat_timeout,
            DEFAULT_CPA_HEARTBEAT_TIMEOUT,
        );
        fill(
            p.cpa_cancel_bound,
            &mut self.cpa_cancel_bound,
            DEFAULT_CPA_CANCEL_BOUND,
        );
        fill(
            p.reclaim_grace,
            &mut self.reclaim_grace,
            DEFAULT_RECLAIM_GRACE,
        );
        fill(
            p.cleanup_interval,
            &mut self.cleanup_interval,
            DEFAULT_CLEANUP_INTERVAL,
        );
        fill(
            p.release_flush_interval,
            &mut self.release_flush_interval,
            DEFAULT_RELEASE_FLUSH_INTERVAL,
        );
        fill(
            p.release_max_backoff,
            &mut self.release_max_backoff,
            DEFAULT_RELEASE_MAX_BACKOFF,
        );
        fill(
            p.busy_retry_min,
            &mut self.busy_retry_min,
            DEFAULT_BUSY_RETRY_MIN,
        );
        fill(
            p.busy_retry_max,
            &mut self.busy_retry_max,
            DEFAULT_BUSY_RETRY_MAX,
        );
        if !p.max_limit && self.max_limit == 0 {
            self.max_limit = MAX_CREDENTIAL_CONCURRENCY_LIMIT;
        }
        self
    }

    /// Validates values intrinsic to a credential concurrency configuration.
    pub fn validate(&self) -> Result<()> {
        if self.lifecycle_config_revision < 0
            || (self.present.lifecycle_config_revision && self.lifecycle_config_revision == 0)
        {
            return invalid("lifecycle configuration revision must be positive when present");
        }
        if self.observation_barrier_revision < 0 {
            return invalid("observation barrier revision must not be negative");
        }
        let lifecycle = [
            self.cpa_heartbeat_timeout,
            self.cpa_cancel_bound,
            self.reclaim_grace,
            self.cleanup_interval,
        ];
        if lifecycle.iter().any(|d| d.0 <= 0) {
            return invalid("credential concurrency lifecycle durations must be positive");
        }
        let limiter = [
            self.release_flush_interval,
            self.release_max_backoff,
            self.busy_retry_min,
            self.busy_retry_max,
        ];
        if limiter.iter().any(|d| d.0 <= 0) {
            return invalid("credential concurrency limiter durations must be positive");
        }
        if self.release_max_backoff < self.release_flush_interval {
            return invalid(
                "credential concurrency release max backoff must not be less than release flush interval",
            );
        }
        if self.busy_retry_min.0 % GoDuration::MILLISECOND != 0
            || self.busy_retry_max.0 % GoDuration::MILLISECOND != 0
        {
            return invalid(
                "credential concurrency busy retry durations must be whole milliseconds",
            );
        }
        if self.busy_retry_max < self.busy_retry_min {
            return invalid(
                "credential concurrency busy retry max must not be less than busy retry min",
            );
        }
        if self.max_limit < 1 || self.max_limit > MAX_CREDENTIAL_CONCURRENCY_LIMIT {
            return invalid(format!(
                "credential concurrency max limit must be between 1 and {MAX_CREDENTIAL_CONCURRENCY_LIMIT}"
            ));
        }
        Ok(())
    }

    /// Verifies the Home lifecycle timing safety invariant: node heartbeat timeout plus reclaim
    /// grace must exceed the CPA heartbeat timeout plus cancel bound.
    pub fn validate_lifecycle(&self, node_heartbeat_timeout: GoDuration) -> Result<()> {
        if node_heartbeat_timeout.0 <= 0 {
            return invalid("credential concurrency lifecycle durations must be positive");
        }
        self.validate()?;
        let left = node_heartbeat_timeout.0.checked_add(self.reclaim_grace.0);
        let right = self
            .cpa_heartbeat_timeout
            .0
            .checked_add(self.cpa_cancel_bound.0);
        let (Some(left), Some(right)) = (left, right) else {
            return invalid("credential concurrency lifecycle timing safety invariant overflows");
        };
        if left <= right {
            return invalid(
                "node heartbeat timeout plus reclaim grace must exceed CPA heartbeat timeout plus cancel bound",
            );
        }
        Ok(())
    }
}

impl CredentialInFlightConfig {
    /// Parses and validates the observation durations: (snapshot interval, stale-after, staging
    /// retention).
    pub fn durations(&self) -> Result<(GoDuration, GoDuration, GoDuration)> {
        let snapshot = GoDuration::parse(&self.snapshot_interval)
            .ok()
            .filter(|d| d.0 > 0);
        let Some(snapshot) = snapshot else {
            return invalid("credential-in-flight.snapshot-interval must be positive");
        };
        let stale = GoDuration::parse(&self.stale_after)
            .ok()
            .filter(|d| d.0 > 0 && snapshot.0 <= d.0 / 3);
        let Some(stale) = stale else {
            return invalid(
                "credential-in-flight.stale-after must be at least three snapshot intervals",
            );
        };
        let retention = GoDuration::parse(&self.staging_retention)
            .ok()
            .filter(|d| d.0 > 0);
        let Some(retention) = retention else {
            return invalid("credential-in-flight.staging-retention must be positive");
        };
        Ok((snapshot, stale, retention))
    }

    /// Verifies the observation bounds.
    pub fn validate(&self) -> Result<()> {
        self.durations()?;
        if self.max_part_bytes < 1024
            || self.max_part_count <= 0
            || self.max_part_count > DEFAULT_IN_FLIGHT_MAX_PART_COUNT
        {
            return invalid("credential-in-flight part bounds are invalid");
        }
        if self.max_revision_bytes < self.max_part_bytes
            || self.max_revision_bytes > DEFAULT_IN_FLIGHT_MAX_REVISION_BYTES
        {
            return invalid("credential-in-flight.max-revision-bytes is outside hard bounds");
        }
        let required_parts =
            (self.max_revision_bytes + self.max_part_bytes - 1) / self.max_part_bytes;
        if required_parts > self.max_part_count {
            return invalid("credential-in-flight.max-revision-bytes exceeds part capacity");
        }
        if self.max_aggregate_groups <= 0
            || self.max_aggregate_groups > DEFAULT_IN_FLIGHT_MAX_AGGREGATE_GROUPS
        {
            return invalid("credential-in-flight.max-aggregate-groups is invalid");
        }
        if self.max_details < 0 || self.max_details > DEFAULT_IN_FLIGHT_MAX_DETAILS {
            return invalid("credential-in-flight.max-details is invalid");
        }
        if self.max_string_bytes <= 0 || self.max_string_bytes > DEFAULT_IN_FLIGHT_MAX_STRING_BYTES
        {
            return invalid("credential-in-flight.max-string-bytes is invalid");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Codex Live media relay
// ---------------------------------------------------------------------------------------------

/// Default in-process media session limit.
pub const DEFAULT_CODEX_LIVE_MEDIA_MAX_SESSIONS: i64 = 32;

impl CodexLiveMediaRelayConfig {
    /// The configured media session limit.
    pub fn effective_max_sessions(&self) -> i64 {
        if self.max_sessions > 0 {
            self.max_sessions
        } else {
            DEFAULT_CODEX_LIVE_MEDIA_MAX_SESSIONS
        }
    }

    /// Verifies the relay configuration (only when enabled).
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.max_sessions < 0 {
            return invalid("codex.live-media-relay.max-sessions must not be negative");
        }
        let public_ip = self.public_ip.trim();
        if !public_ip.is_empty() && public_ip.parse::<IpAddr>().is_err() {
            return invalid(format!(
                "codex.live-media-relay.public-ip is invalid: {public_ip:?}"
            ));
        }
        if (self.udp_port_min == 0) != (self.udp_port_max == 0) {
            return invalid("codex.live-media-relay UDP port minimum and maximum must both be set");
        }
        if self.udp_port_min > self.udp_port_max {
            return invalid("codex.live-media-relay.udp-port-min must not exceed udp-port-max");
        }
        if self.udp_port_min != 0 {
            let available = i64::from(self.udp_port_max) - i64::from(self.udp_port_min) + 1;
            let required = self.effective_max_sessions().saturating_mul(2);
            if available < required {
                return invalid(format!(
                    "codex.live-media-relay UDP range requires at least {required} ports for {} sessions",
                    self.effective_max_sessions()
                ));
            }
        }
        for (index, server) in self.ice_servers.iter().enumerate() {
            if server.urls.is_empty() {
                return invalid(format!(
                    "codex.live-media-relay.ice-servers[{index}].urls is required"
                ));
            }
            for raw in &server.urls {
                let Some(scheme) = url_scheme(raw.trim()) else {
                    return invalid(format!(
                        "codex.live-media-relay.ice-servers[{index}] contains an invalid URL"
                    ));
                };
                if !matches!(scheme.as_str(), "stun" | "stuns" | "turn" | "turns") {
                    return invalid(format!(
                        "codex.live-media-relay.ice-servers[{index}] uses unsupported scheme {scheme:?}"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Lowercased RFC 3986 scheme of `url`, or `None` when there is no valid scheme.
fn url_scheme(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let valid = first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}
