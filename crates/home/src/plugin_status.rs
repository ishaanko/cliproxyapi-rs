//! Plugin status reports pushed to Home (Go: `internal/home/plugin_status.go` and the report
//! shape of `internal/homeplugins`). This build has no plugin host, so reports only ever say
//! that no plugins were installed.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::error::HomeError;

const PLUGIN_STATUS_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Platform {
    pub goos: String,
    pub goarch: String,
}

impl Platform {
    /// The running platform in Go's `GOOS`/`GOARCH` spelling.
    pub fn current() -> Platform {
        let goos = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let goarch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            other => other,
        };
        Platform { goos: goos.into(), goarch: goarch.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInstallStatus {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub install_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub load_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    pub schema_version: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub task_id: u64,
    pub task: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_id: String,
    pub status: String,
    pub phase: String,
    pub ok: bool,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub platform: Platform,
    pub plugins: Vec<PluginInstallStatus>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Go: `homeplugins.CompletedSyncReport`: a finished plugin-sync report for outcomes before any
/// installation starts (`err` marks it failed).
pub fn completed_sync_report(platform: Platform, err: Option<&str>) -> SyncReport {
    let now = Utc::now();
    SyncReport {
        schema_version: 1,
        task_id: 0,
        task: "plugin-sync".into(),
        node_id: String::new(),
        status: if err.is_some() { "failed" } else { "success" }.into(),
        phase: "install".into(),
        ok: err.is_none(),
        started_at: now,
        finished_at: Some(now),
        updated_at: now,
        platform,
        plugins: Vec::new(),
        error: err.unwrap_or_default().to_string(),
    }
}

/// Go: `ReportPluginStatus`: stamps `node_id` and the update time, then `RPUSH plugin-status`
/// within ten seconds.
pub async fn report_plugin_status(client: &Client, node_id: &str, report: SyncReport) -> Result<(), HomeError> {
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err(HomeError::other("home plugin status node id is empty"));
    }
    let report = SyncReport { node_id: node_id.to_string(), updated_at: Utc::now(), ..report };
    let raw = serde_json::to_vec(&report).map_err(HomeError::other)?;
    tokio::time::timeout(PLUGIN_STATUS_REPORT_TIMEOUT, client.rpush_plugin_status(&raw))
        .await
        .map_err(|_| HomeError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self as t, MockHome};
    use cpa_config::HomeConfig;

    fn client(mock: &MockHome) -> Client {
        Client::new(HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()), ..Default::default() })
    }

    #[tokio::test]
    async fn report_pushes_the_node_report_to_plugin_status() {
        let mock = MockHome::start(|_| t::int(1)).await;
        let report = SyncReport { plugins: vec![PluginInstallStatus { id: "sample".into(), version: String::new(), install_status: "installed".into(), load_status: String::new(), error: String::new() }], ..completed_sync_report(Platform::current(), None) };
        report_plugin_status(&client(&mock), " node-1 ", report).await.unwrap();
        let cmd = &mock.commands()[0];
        assert_eq!((cmd[0].to_lowercase().as_str(), cmd[1].as_str()), ("rpush", "plugin-status"));
        let payload: SyncReport = serde_json::from_str(&cmd[2]).unwrap();
        assert!(payload.node_id == "node-1" && payload.ok && payload.plugins.len() == 1);
    }

    #[tokio::test]
    async fn empty_reports_push_and_blank_node_ids_fail() {
        let mock = MockHome::start(|_| t::int(1)).await;
        let c = client(&mock);
        report_plugin_status(&c, "node-1", completed_sync_report(Platform::current(), None)).await.unwrap();
        let payload: serde_json::Value = serde_json::from_str(&mock.commands()[0][2]).unwrap();
        assert_eq!(payload["plugins"], serde_json::json!([]));
        assert_eq!(report_plugin_status(&c, "  ", completed_sync_report(Platform::current(), None)).await.unwrap_err().to_string(), "home plugin status node id is empty");
    }

    #[test]
    fn failed_reports_carry_the_error() {
        let r = completed_sync_report(Platform { goos: "linux".into(), goarch: "amd64".into() }, Some("boom"));
        assert_eq!((r.status.as_str(), r.ok, r.error.as_str()), ("failed", false, "boom"));
    }
}
