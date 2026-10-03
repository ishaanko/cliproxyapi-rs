//! Request and frame shapes exchanged with Home (Go: `internal/home/requests.go`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The JSON key of an auth dispatch `RPOP` (`omitempty` fields are skipped when empty).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AuthDispatchRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub model: String,
    pub count: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub concurrency_protocol: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub node_kind: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub credential_policy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_round: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_auth_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pinned_auth_id: String,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelsRequest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub auth_index: String,
    #[serde(rename = "access_token_sha256", skip_serializing_if = "String::is_empty")]
    pub observed_access_token_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InFlightFrameKind {
    Part,
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InFlightAccountedStatus {
    Accounted,
    Unaccounted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlightAggregate {
    pub credential_id: String,
    pub model: String,
    pub status: InFlightAccountedStatus,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlightRequestDetail {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlightSnapshotFrame {
    pub kind: InFlightFrameKind,
    pub revision: i64,
    pub observed_at: DateTime<Utc>,
    pub barrier_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_count: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub details_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregates: Vec<InFlightAggregate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<InFlightRequestDetail>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub aggregate_group_count: usize,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// One pending plugin task handed out by Home.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PluginTask {
    pub id: u64,
    pub operation: String,
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_node_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_node_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Cumulative concurrency release accepted by Home for one credential and model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConcurrencyReleaseFrame {
    pub credential_id: String,
    pub model: String,
    pub release_seq: i64,
}
