//! Wire types for CLIProxyAPIHome plugin sync (Go `home_sync.go`).

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::{ResolvedAuthConfig, clear_resolved_auth_configs, validate_resolved_auth_config};
use crate::error::Result;
use crate::errf;
use crate::goturl::GoUrl;
use crate::manifest::Manifest;
use crate::registry::{INSTALL_TYPE_DIRECT, normalize_install_plan, null_default};

pub const PLUGIN_SYNC_SCHEMA_VERSION: i64 = 1;

/// Request sent to Home: platform plus the versions currently installed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSyncRequest {
    #[serde(deserialize_with = "null_default")]
    pub schema_version: i64,
    #[serde(deserialize_with = "null_default")]
    pub goos: String,
    #[serde(deserialize_with = "null_default")]
    pub goarch: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "null_default")]
    pub installed_versions: BTreeMap<String, String>,
}

impl PluginSyncRequest {
    pub fn clear(&mut self) {
        self.installed_versions = BTreeMap::new();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSyncItem {
    #[serde(deserialize_with = "null_default")]
    pub manifest: Manifest,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub auth: Vec<ResolvedAuthConfig>,
}

impl PluginSyncItem {
    /// Overwrites resolved credentials and resets the manifest.
    pub fn clear(&mut self) {
        clear_resolved_auth_configs(&mut self.auth);
        self.auth = Vec::new();
        self.manifest = Manifest::default();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSyncResponse {
    #[serde(deserialize_with = "null_default")]
    pub schema_version: i64,
    /// `None` is the Go zero time.
    #[serde(with = "crate::gotime")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "null_default")]
    pub items: Vec<PluginSyncItem>,
}

impl PluginSyncResponse {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<()> {
        if self.schema_version != PLUGIN_SYNC_SCHEMA_VERSION {
            return Err(errf!("unsupported plugin sync schema_version {}", self.schema_version));
        }
        let Some(expires_at) = self.expires_at.filter(|_| !crate::gotime::is_zero(&self.expires_at)) else {
            return Err(errf!("plugin sync response missing expires_at"));
        };
        if now >= expires_at {
            return Err(errf!("plugin sync response expired"));
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            item.manifest
                .validate()
                .map_err(|err| err.wrap(format!("plugin sync item {index}")))?;
            validate_plugin_sync_manifest_urls(&item.manifest)
                .map_err(|err| err.wrap(format!("plugin sync item {index}")))?;
            let id = item.manifest.id.trim();
            if !seen.insert(id) {
                return Err(errf!("plugin sync response contains duplicate plugin {id:?}"));
            }
            for (auth_index, auth) in item.auth.iter().enumerate() {
                validate_resolved_auth_config(auth)
                    .map_err(|err| err.wrap(format!("plugin sync item {index} auth {auth_index}")))?;
            }
        }
        Ok(())
    }

    /// Overwrites all credentials and resets the response.
    pub fn clear(&mut self) {
        for item in &mut self.items {
            item.clear();
        }
        self.items = Vec::new();
        self.expires_at = None;
        self.schema_version = 0;
    }
}

fn validate_plugin_sync_manifest_urls(manifest: &Manifest) -> Result<()> {
    if manifest.install_type() != INSTALL_TYPE_DIRECT {
        return Ok(());
    }
    let plan = normalize_install_plan(&manifest.install);
    if plan.artifacts.is_empty() {
        return Err(errf!("direct plugin sync manifest requires pinned artifacts"));
    }
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        match GoUrl::parse(artifact.url.trim()) {
            Ok(parsed) if parsed.scheme.eq_ignore_ascii_case("https") => {}
            _ => return Err(errf!("direct plugin sync artifact {index} must use https")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AUTH_TYPE_BEARER, REQUEST_KIND_ARTIFACT, Secret};
    use crate::registry::{Artifact, INSTALL_TYPE_DIRECT, InstallPlan, SCHEMA_VERSION_V2};
    use chrono::Duration;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn direct_manifest(url: &str) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION_V2,
            id: "sample".into(),
            version: "1.0.0".into(),
            install: InstallPlan {
                kind: INSTALL_TYPE_DIRECT.into(),
                artifacts: vec![Artifact {
                    goos: "linux".into(),
                    goarch: "amd64".into(),
                    url: url.into(),
                    sha256: SHA.into(),
                    size: 0,
                }],
            },
            ..Default::default()
        }
    }

    fn response(manifest: Manifest, auth: Vec<ResolvedAuthConfig>) -> PluginSyncResponse {
        PluginSyncResponse {
            schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
            expires_at: Some(Utc::now() + Duration::minutes(1)),
            items: vec![PluginSyncItem { manifest, auth }],
        }
    }

    fn bearer(match_url: &str, apply_to: &[&str]) -> ResolvedAuthConfig {
        ResolvedAuthConfig {
            match_url: match_url.into(),
            apply_to: apply_to.iter().map(|s| s.to_string()).collect(),
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }
    }

    #[test]
    fn validates_and_clears_resolved_auth() {
        let mut response = response(
            direct_manifest("https://downloads.example/sample.zip"),
            vec![bearer("https://downloads.example/", &[])],
        );
        response.validate(Utc::now()).expect("valid");
        response.clear();
        assert!(response.items.is_empty() && response.expires_at.is_none() && response.schema_version == 0);
    }

    #[test]
    fn json_keeps_secrets_out_of_plain_text() {
        let response = PluginSyncResponse {
            schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
            expires_at: Some(Utc::now() + Duration::minutes(1)),
            items: vec![PluginSyncItem {
                auth: vec![ResolvedAuthConfig { token: Secret::from("temporary-token"), ..Default::default() }],
                ..Default::default()
            }],
        };
        let raw = serde_json::to_string(&response).expect("marshal");
        assert!(!raw.contains("temporary-token"), "exposed token as plain text: {raw}");
        let decoded: PluginSyncResponse = serde_json::from_str(&raw).expect("unmarshal");
        assert_eq!(decoded.items[0].auth[0].token.to_string_lossy(), "temporary-token");
    }

    #[test]
    fn rejects_expired_plan() {
        let response = PluginSyncResponse {
            schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
            expires_at: Some(Utc::now() - Duration::seconds(1)),
            items: Vec::new(),
        };
        assert!(response.validate(Utc::now()).is_err());
    }

    #[test]
    fn rejects_insecure_resolved_auth_match() {
        let response = response(
            direct_manifest("https://downloads.example/sample.zip"),
            vec![bearer("http://downloads.example/", &[])],
        );
        assert!(response.validate(Utc::now()).is_err());
    }

    #[test]
    fn rejects_http_artifact() {
        let response = response(direct_manifest("http://downloads.example/sample.zip"), Vec::new());
        assert!(response.validate(Utc::now()).is_err());
    }

    #[test]
    fn rejects_http_artifact_with_resolved_auth() {
        let response = response(
            direct_manifest("http://downloads.example/sample.zip"),
            vec![bearer("https://downloads.example/", &[REQUEST_KIND_ARTIFACT])],
        );
        assert!(response.validate(Utc::now()).is_err());
    }

    #[test]
    fn rejects_artifact_url_credentials_without_leaking_them() {
        let response = response(direct_manifest("https://user:password@downloads.example/sample.zip"), Vec::new());
        let err = response.validate(Utc::now()).expect_err("credentials rejected");
        assert!(!err.to_string().contains("password"), "{err}");
    }
}
