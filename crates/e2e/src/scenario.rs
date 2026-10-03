//! Scenario definition and the captured (golden) shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{Observed, Step};
use crate::config::ConfigSpec;
use crate::mock::script::Script;

/// Tweaks the baseline server config for a scenario (retry policy, strategy, ...).
pub type Profile = fn(&mut ConfigSpec);

pub fn default_profile(_: &mut ConfigSpec) {}

/// One scenario: a server config, a mock-upstream script and the client requests to send.
pub struct Scenario {
    /// Dotted id, also the golden file name: `<dialect>.<family>.<case>`.
    pub id: String,
    pub desc: String,
    pub profile: Profile,
    pub script: Script,
    pub steps: Vec<Step>,
    /// Also capture the request-log files the server wrote (normalized) into the golden.
    pub capture_logs: bool,
}

impl Scenario {
    pub fn new(id: impl Into<String>, desc: impl Into<String>, script: Script, steps: Vec<Step>) -> Self {
        Scenario { id: id.into(), desc: desc.into(), profile: default_profile, script, steps, capture_logs: false }
    }

    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Compare the request-log files too (pair with `profiles::request_log` for full logs).
    pub fn with_logs(mut self) -> Self {
        self.capture_logs = true;
        self
    }
}

/// What the client sent and received for one step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepCapture {
    pub request: Value,
    pub response: Observed,
}

/// What the mock upstream received (normalized).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpstreamCapture {
    pub family: String,
    pub method: String,
    pub path: String,
    pub query: String,
    pub credential: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ws_frames: Vec<Value>,
}

/// The golden file content for one scenario.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub id: String,
    pub desc: String,
    pub steps: Vec<StepCapture>,
    pub upstream: Vec<UpstreamCapture>,
    /// Normalized request-log files the server wrote, oldest first (only for `with_logs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_logs: Vec<String>,
    /// JSON pointers of leaves that differed between the two recording runs; `check` ignores
    /// their values (they are stored as `<volatile>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volatile: Vec<String>,
}
