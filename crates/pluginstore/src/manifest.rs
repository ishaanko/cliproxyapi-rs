//! Pinned store manifest stored under `plugins.configs.<id>.store` (Go `manifest.go`).

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::errf;
use crate::goturl::GoUrl;
use crate::github::{Release, release_version};
use crate::registry::{
    Artifact, INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Plugin, SCHEMA_VERSION_V2, Source,
    has_sensitive_query_parameter, is_zero_i64, normalize_install_plan, normalize_version, null_default,
    plugin_install_type, valid_plugin_id, validate_install_plan, validate_plugin,
};

/// Manifest persisted in config (YAML, kebab-case keys) and exchanged with Home (JSON,
/// snake_case keys); deserialization accepts both spellings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    #[serde(rename = "schema_version", alias = "schema-version", skip_serializing_if = "is_zero_i64", deserialize_with = "null_default")]
    pub schema_version: i64,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub version: String,
    #[serde(rename = "release_tag", alias = "release-tag", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub release_tag: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub repository: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub logo: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub homepage: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub license: String,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub tags: Vec<String>,
    #[serde(rename = "source_id", alias = "source-id", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub source_id: String,
    #[serde(rename = "source_name", alias = "source-name", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub source_name: String,
    #[serde(rename = "source_url", alias = "source-url", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub source_url: String,
    #[serde(deserialize_with = "null_default")]
    pub install: InstallPlan,
}

/// Scalar keys that Go's YAML decoder coerces to strings (`version: 1.0` reads as "1.0").
const STRING_KEYS: [&str; 13] = [
    "id", "name", "description", "author", "version", "release-tag", "repository", "logo", "homepage",
    "license", "source-id", "source-name", "source-url",
];

impl Manifest {
    /// Decodes a `store` YAML subtree like `yaml.Node.Decode`: unknown keys are ignored
    /// and numeric/boolean scalars are accepted for string fields.
    pub fn from_yaml(value: &serde_yaml_ng::Value) -> std::result::Result<Manifest, String> {
        let mut value = value.clone();
        if let serde_yaml_ng::Value::Mapping(map) = &mut value {
            for key in STRING_KEYS {
                let slot = map.get_mut(key);
                if let Some(slot) = slot {
                    match slot {
                        serde_yaml_ng::Value::Number(n) => *slot = serde_yaml_ng::Value::String(n.to_string()),
                        serde_yaml_ng::Value::Bool(b) => *slot = serde_yaml_ng::Value::String(b.to_string()),
                        _ => {}
                    }
                }
            }
        }
        serde_yaml_ng::from_value(value).map_err(|err| err.to_string())
    }

    /// Manifest for a resolved GitHub release (pinned to its tag).
    pub fn from_release(source: &Source, plugin: &Plugin, release: &Release) -> Result<Manifest> {
        let version = release_version(release)?;
        Ok(manifest_from_plugin(
            source,
            plugin,
            Manifest {
                version,
                release_tag: release.tag_name.trim().to_string(),
                repository: plugin.repository.trim().to_string(),
                install: InstallPlan { kind: INSTALL_TYPE_GITHUB_RELEASE.to_string(), artifacts: Vec::new() },
                ..Default::default()
            },
        ))
    }

    /// Manifest for a direct-install plugin (pins all artifacts).
    pub fn from_plugin(source: &Source, plugin: &Plugin) -> Result<Manifest> {
        validate_plugin(plugin)?;
        match plugin_install_type(plugin).as_str() {
            INSTALL_TYPE_DIRECT => {
                let manifest = manifest_from_plugin(
                    source,
                    plugin,
                    Manifest {
                        schema_version: SCHEMA_VERSION_V2,
                        version: plugin.version.trim().to_string(),
                        install: normalize_install_plan(&plugin.install),
                        ..Default::default()
                    },
                );
                manifest.validate()?;
                Ok(manifest)
            }
            INSTALL_TYPE_GITHUB_RELEASE => Err(errf!("github-release manifest requires a resolved release")),
            _ => Err(errf!("unsupported install type {:?}", plugin.install.kind)),
        }
    }

    pub fn plugin(&self) -> Plugin {
        Plugin {
            id: self.id.trim().to_string(),
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            author: self.author.trim().to_string(),
            version: self.version.trim().to_string(),
            repository: self.repository.trim().to_string(),
            logo: self.logo.trim().to_string(),
            homepage: self.homepage.trim().to_string(),
            license: self.license.trim().to_string(),
            tags: self.tags.clone(),
            install: normalize_install_plan(&self.install),
            ..Default::default()
        }
    }

    /// Lowercased install type; empty means `github-release`.
    pub fn install_type(&self) -> String {
        let install_type = self.install.kind.trim().to_lowercase();
        if install_type.is_empty() {
            INSTALL_TYPE_GITHUB_RELEASE.to_string()
        } else {
            install_type
        }
    }

    pub fn validate(&self) -> Result<()> {
        let version = self.version.trim();
        if version.is_empty() {
            return Err(errf!("missing required field version"));
        }
        if !crate::registry::valid_plugin_version(&normalize_version(version)) {
            return Err(errf!("invalid plugin version {:?}", self.version));
        }
        match self.install_type().as_str() {
            INSTALL_TYPE_DIRECT => {
                if self.schema_version != 0 && self.schema_version != SCHEMA_VERSION_V2 {
                    return Err(errf!("unsupported schema-version {}", self.schema_version));
                }
                validate_manifest_plugin_id(&self.id)?;
                let mut plan = normalize_install_plan(&self.install);
                plan.kind = INSTALL_TYPE_DIRECT.to_string();
                if !plan.artifacts.is_empty() {
                    validate_install_plan(&plan)?;
                    return validate_pinned_artifact_urls(&plan.artifacts);
                }
                validate_manifest_source_url(&self.source_url)
            }
            INSTALL_TYPE_GITHUB_RELEASE => {
                let release_tag = self.release_tag.trim();
                if release_tag.is_empty() {
                    return Err(errf!("missing required field release-tag"));
                }
                let mut plugin = self.plugin();
                plugin.install = InstallPlan { kind: INSTALL_TYPE_GITHUB_RELEASE.to_string(), artifacts: Vec::new() };
                validate_plugin(&plugin)?;
                let release_version = release_version(&Release { tag_name: release_tag.to_string(), assets: Vec::new() })?;
                let want = normalize_version(version);
                if release_version != want {
                    return Err(errf!("release-tag {release_tag:?} resolves version {release_version:?}, want {want:?}"));
                }
                Ok(())
            }
            _ => Err(errf!("unsupported install type {:?}", self.install.kind)),
        }
    }
}

fn manifest_from_plugin(source: &Source, plugin: &Plugin, mut base: Manifest) -> Manifest {
    base.id = plugin.id.trim().to_string();
    base.name = plugin.name.trim().to_string();
    base.description = plugin.description.trim().to_string();
    base.author = plugin.author.trim().to_string();
    base.logo = plugin.logo.trim().to_string();
    base.homepage = plugin.homepage.trim().to_string();
    base.license = plugin.license.trim().to_string();
    base.tags = plugin.tags.clone();
    base.source_id = source.id.trim().to_string();
    base.source_name = source.name.trim().to_string();
    base.source_url = source.url.trim().to_string();
    base
}

fn validate_pinned_artifact_urls(artifacts: &[Artifact]) -> Result<()> {
    for (index, artifact) in artifacts.iter().enumerate() {
        let Ok(parsed) = GoUrl::parse(artifact.url.trim()) else {
            return Err(errf!("artifacts[{index}]: invalid artifact url"));
        };
        if parsed.has_user {
            return Err(errf!("artifacts[{index}]: pinned artifact url must not contain credentials"));
        }
        if !parsed.raw_query.is_empty() || !parsed.fragment.is_empty() {
            return Err(errf!("artifacts[{index}]: pinned artifact url must not contain query or fragment"));
        }
    }
    Ok(())
}

fn validate_manifest_plugin_id(id: &str) -> Result<()> {
    let id = id.trim();
    if id.is_empty() {
        return Err(errf!("missing required field id"));
    }
    if !valid_plugin_id(id) {
        return Err(errf!("invalid plugin id {id:?}"));
    }
    Ok(())
}

fn validate_manifest_source_url(source_url: &str) -> Result<()> {
    let source_url = source_url.trim();
    if source_url.is_empty() {
        return Err(errf!("missing required field source-url"));
    }
    let parsed = match GoUrl::parse(source_url) {
        Ok(parsed) if !parsed.scheme.is_empty() && !parsed.host.is_empty() => parsed,
        _ => return Err(errf!("invalid source-url")),
    };
    if parsed.scheme != "https" && parsed.scheme != "http" {
        return Err(errf!("source-url must use http or https"));
    }
    if has_sensitive_query_parameter(&parsed) {
        return Err(errf!("source-url contains sensitive query parameter"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::default_source;

    fn valid_manifest() -> Manifest {
        Manifest {
            id: "sample-provider".into(),
            name: "Sample Provider".into(),
            description: "Adds sample provider support.".into(),
            author: "author-name".into(),
            version: "0.2.0".into(),
            release_tag: "v0.2.0".into(),
            repository: "https://github.com/author-name/sample-provider".into(),
            ..Default::default()
        }
    }

    fn direct_plugin(url: &str) -> Plugin {
        Plugin {
            id: "sample-provider".into(),
            name: "Sample Provider".into(),
            description: "Adds sample provider support.".into(),
            author: "author-name".into(),
            version: "0.4.0".into(),
            install: InstallPlan {
                kind: INSTALL_TYPE_DIRECT.into(),
                artifacts: vec![Artifact {
                    goos: "linux".into(),
                    goarch: "amd64".into(),
                    url: url.into(),
                    sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                    size: 0,
                }],
            },
            ..Default::default()
        }
    }

    #[test]
    fn validate_requires_pinned_release_tag() {
        let mut manifest = valid_manifest();
        manifest.release_tag.clear();
        let err = manifest.validate().expect_err("release-tag error");
        assert!(err.to_string().contains("release-tag"), "{err}");
    }

    #[test]
    fn validate_rejects_release_tag_version_mismatch() {
        let mut manifest = valid_manifest();
        manifest.release_tag = "v0.3.0".into();
        let err = manifest.validate().expect_err("version mismatch");
        assert!(err.to_string().contains("resolves version"), "{err}");
    }

    #[test]
    fn from_release_builds_pinned_manifest() {
        let plugin = Plugin {
            id: "sample-provider".into(),
            name: "Sample Provider".into(),
            description: "Adds sample provider support.".into(),
            author: "author-name".into(),
            repository: "https://github.com/author-name/sample-provider".into(),
            ..Default::default()
        };
        let release = Release { tag_name: "v0.2.0".into(), assets: Vec::new() };
        let manifest = Manifest::from_release(&default_source(), &plugin, &release).expect("manifest");
        manifest.validate().expect("valid");
        assert_eq!((manifest.version.as_str(), manifest.release_tag.as_str()), ("0.2.0", "v0.2.0"));
    }

    #[test]
    fn from_plugin_builds_direct_manifest() {
        let plugin = direct_plugin("https://downloads.example/sample-provider.zip");
        let manifest = Manifest::from_plugin(&default_source(), &plugin).expect("manifest");
        manifest.validate().expect("valid");
        assert_eq!(manifest.schema_version, SCHEMA_VERSION_V2);
        assert_eq!(manifest.install_type(), INSTALL_TYPE_DIRECT);
        assert!(manifest.release_tag.is_empty());
        assert_eq!(manifest.source_url, crate::registry::DEFAULT_REGISTRY_URL);
        assert_eq!(manifest.install.artifacts.len(), 1);
        let artifact = &manifest.install.artifacts[0];
        assert_eq!((artifact.goos.as_str(), artifact.goarch.as_str()), ("linux", "amd64"));
        assert_eq!(artifact.url, "https://downloads.example/sample-provider.zip");
    }

    #[test]
    fn from_plugin_rejects_artifact_query_without_leaking_it() {
        let plugin = direct_plugin("https://downloads.example/sample.zip?X-Amz-Signature=secret");
        let err = Manifest::from_plugin(&default_source(), &plugin).expect_err("query rejected");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[test]
    fn yaml_store_node_decodes_kebab_keys() {
        let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            "id: sample\nversion: 1.0\nrelease-tag: v1.0\nsource-url: https://x.example/r.json\nschema-version: 2\n",
        )
        .expect("yaml");
        let manifest = Manifest::from_yaml(&yaml).expect("decode");
        assert_eq!(manifest.version, "1.0");
        assert_eq!(manifest.release_tag, "v1.0");
        assert_eq!(manifest.source_url, "https://x.example/r.json");
        assert_eq!(manifest.schema_version, 2);
    }
}
