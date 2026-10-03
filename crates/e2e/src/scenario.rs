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
}

impl Scenario {
    pub fn new(id: impl Into<String>, desc: impl Into<String>, script: Script, steps: Vec<Step>) -> Self {
        Scenario { id: id.into(), desc: desc.into(), profile: default_profile, script, steps }
    }

    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Plugin libraries this scenario loads that have not been built.
    pub fn missing_plugins(&self, mock_port: u16) -> Vec<&'static str> {
        let mut spec = ConfigSpec::baseline(mock_port);
        (self.profile)(&mut spec);
        crate::server::missing_plugins(&spec)
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
    /// JSON pointers of leaves that differed between the two recording runs; `check` ignores
    /// their values (they are stored as `<volatile>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volatile: Vec<String>,
}
