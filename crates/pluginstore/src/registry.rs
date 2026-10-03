//! Plugin registry schema, parsing and validation (Go `internal/pluginstore/registry.go`).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::errf;
use crate::goturl::{GoUrl, path_unescape};

pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/router-for-me/CLIProxyAPI-Plugins-Store/main/registry.json";
pub const DEFAULT_SOURCE_ID: &str = "official";
pub const DEFAULT_SOURCE_NAME: &str = "Official";
pub const SCHEMA_VERSION: i64 = 1;
pub const SCHEMA_VERSION_V2: i64 = 2;

pub const INSTALL_TYPE_GITHUB_RELEASE: &str = "github-release";
pub const INSTALL_TYPE_DIRECT: &str = "direct";

static PLUGIN_VERSION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9][0-9A-Za-z.+-]*$").expect("static regex"));
static PLUGIN_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("static regex"));

/// Go's JSON decoding leaves a field at its zero value for `null`.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    #[serde(deserialize_with = "null_default")]
    pub schema_version: i64,
    #[serde(deserialize_with = "null_default")]
    pub plugins: Vec<Plugin>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugin {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub description: String,
    #[serde(deserialize_with = "null_default")]
    pub author: String,
    #[serde(deserialize_with = "null_default")]
    pub version: String,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub versions: Vec<Version>,
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
    #[serde(deserialize_with = "null_default")]
    pub install: InstallPlan,
    #[serde(skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub auth_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    #[serde(deserialize_with = "null_default")]
    pub version: String,
    #[serde(deserialize_with = "null_default")]
    pub install: InstallPlan,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallPlan {
    #[serde(rename = "type", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Artifact {
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub goos: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub goarch: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub sha256: String,
    #[serde(skip_serializing_if = "is_zero_i64", deserialize_with = "null_default")]
    pub size: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Platform {
    #[serde(deserialize_with = "null_default")]
    pub goos: String,
    #[serde(deserialize_with = "null_default")]
    pub goarch: String,
}

pub fn default_source() -> Source {
    Source {
        id: DEFAULT_SOURCE_ID.to_string(),
        name: DEFAULT_SOURCE_NAME.to_string(),
        url: DEFAULT_REGISTRY_URL.to_string(),
    }
}

/// Builds the source list: the official source first, then each distinct configured URL.
pub fn normalize_sources(registry_urls: &[String]) -> Result<Vec<Source>> {
    let mut out = vec![default_source()];
    let mut seen_ids: HashMap<String, String> = HashMap::new();
    seen_ids.insert(DEFAULT_SOURCE_ID.to_string(), DEFAULT_REGISTRY_URL.to_string());
    let mut seen_urls: HashSet<String> = HashSet::new();
    seen_urls.insert(DEFAULT_REGISTRY_URL.to_string());
    for registry_url in registry_urls {
        let registry_url = registry_url.trim();
        if registry_url.is_empty() || seen_urls.contains(registry_url) {
            continue;
        }
        let source = Source {
            id: source_id(registry_url),
            name: source_name(registry_url),
            url: registry_url.to_string(),
        };
        if let Some(existing) = seen_ids.get(&source.id) {
            return Err(errf!("plugin store source id collision for {existing:?} and {registry_url:?}"));
        }
        seen_ids.insert(source.id.clone(), registry_url.to_string());
        seen_urls.insert(registry_url.to_string());
        out.push(source);
    }
    Ok(out)
}

/// `source-` plus the first 12 hex chars of the SHA-256 of the trimmed URL.
pub fn source_id(registry_url: &str) -> String {
    let sum = Sha256::digest(registry_url.trim().as_bytes());
    format!("source-{}", &hex::encode(sum)[..12])
}

/// Host of the registry URL, or the trimmed URL when it has no host.
pub fn source_name(registry_url: &str) -> String {
    let trimmed = registry_url.trim();
    match GoUrl::parse(trimmed) {
        Ok(parsed) if !parsed.host.trim().is_empty() => parsed.host,
        _ => trimmed.to_string(),
    }
}

/// Decodes (first JSON value only, like `json.Decoder.Decode`), normalizes and validates.
pub fn parse_registry(data: &[u8]) -> Result<Registry> {
    let mut stream = serde_json::Deserializer::from_slice(data).into_iter::<Registry>();
    let mut registry = match stream.next() {
        Some(Ok(registry)) => registry,
        Some(Err(err)) => return Err(errf!("decode registry: {err}")),
        None => return Err(errf!("decode registry: EOF")),
    };
    normalize_registry(&mut registry);
    validate_registry(&registry)?;
    Ok(registry)
}

fn normalize_registry(registry: &mut Registry) {
    for plugin in &mut registry.plugins {
        plugin.id = plugin.id.trim().to_string();
        plugin.name = plugin.name.trim().to_string();
        plugin.description = plugin.description.trim().to_string();
        plugin.author = plugin.author.trim().to_string();
        plugin.version = plugin.version.trim().to_string();
        plugin.repository = plugin.repository.trim().to_string();
        plugin.logo = plugin.logo.trim().to_string();
        plugin.homepage = plugin.homepage.trim().to_string();
        plugin.license = plugin.license.trim().to_string();
        plugin.install = normalize_install_plan(&plugin.install);
        for version in &mut plugin.versions {
            version.version = normalize_version(&version.version);
            version.install = normalize_install_plan(&version.install);
        }
        for tag in &mut plugin.tags {
            *tag = tag.trim().to_string();
        }
    }
}

pub fn validate_registry(registry: &Registry) -> Result<()> {
    if registry.schema_version != SCHEMA_VERSION && registry.schema_version != SCHEMA_VERSION_V2 {
        return Err(errf!("unsupported schema_version {}", registry.schema_version));
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(registry.plugins.len());
    for (index, plugin) in registry.plugins.iter().enumerate() {
        if registry.schema_version == SCHEMA_VERSION && plugin_install_type(plugin) == INSTALL_TYPE_DIRECT {
            return Err(errf!(
                "plugins[{index}]: direct install requires schema_version {SCHEMA_VERSION_V2}"
            ));
        }
        validate_plugin(plugin).map_err(|err| err.wrap(format!("plugins[{index}]")))?;
        let id = plugin.id.trim();
        if !seen.insert(id) {
            return Err(errf!("plugins[{index}]: duplicate plugin id {id:?}"));
        }
    }
    Ok(())
}

pub fn validate_plugin(plugin: &Plugin) -> Result<()> {
    let install_type = plugin_install_type(plugin);
    let mut required = vec![
        ("id", plugin.id.as_str()),
        ("name", plugin.name.as_str()),
        ("description", plugin.description.as_str()),
        ("author", plugin.author.as_str()),
    ];
    if install_type == INSTALL_TYPE_GITHUB_RELEASE {
        required.push(("repository", plugin.repository.as_str()));
    }
    for (field, value) in required {
        if value.trim().is_empty() {
            return Err(errf!("missing required field {field}"));
        }
    }
    if !valid_plugin_id(plugin.id.trim()) {
        return Err(errf!("invalid plugin id {:?}", plugin.id));
    }
    // The version is optional since the latest release is the source of truth;
    // when present it is only used as a display fallback and must be valid.
    let version = plugin.version.trim();
    if !version.is_empty() && !valid_plugin_version(version) {
        return Err(errf!("invalid plugin version {:?}", plugin.version));
    }
    match install_type.as_str() {
        INSTALL_TYPE_GITHUB_RELEASE => {
            github_repository_parts(&plugin.repository)?;
        }
        INSTALL_TYPE_DIRECT => {
            if plugin.version.trim().is_empty() {
                return Err(errf!("missing required field version"));
            }
            validate_install_plan(&plugin.install)?;
            validate_plugin_versions(plugin)?;
        }
        _ => return Err(errf!("unsupported install type {:?}", plugin.install.kind)),
    }
    Ok(())
}

pub fn validate_plugin_versions(plugin: &Plugin) -> Result<()> {
    if plugin.versions.is_empty() {
        return Ok(());
    }
    let mut seen: HashSet<String> = HashSet::with_capacity(plugin.versions.len());
    for (index, version) in plugin.versions.iter().enumerate() {
        let mut version = version.clone();
        version.version = normalize_version(&version.version);
        if !valid_plugin_version(&version.version) {
            return Err(errf!("versions[{index}]: invalid plugin version {:?}", version.version));
        }
        if !seen.insert(version.version.clone()) {
            return Err(errf!("versions[{index}]: duplicate plugin version {:?}", version.version));
        }
        let mut install_type = version.install.kind.trim().to_lowercase();
        if install_type.is_empty() {
            install_type = plugin_install_type(plugin);
            version.install.kind = install_type.clone();
        }
        if install_type != plugin_install_type(plugin) {
            return Err(errf!(
                "versions[{index}]: install type {:?} does not match plugin install type {:?}",
                install_type,
                plugin_install_type(plugin)
            ));
        }
        validate_install_plan(&version.install).map_err(|err| err.wrap(format!("versions[{index}]")))?;
    }
    Ok(())
}

/// Lowercased install type; empty means `github-release`.
pub fn plugin_install_type(plugin: &Plugin) -> String {
    let install_type = plugin.install.kind.trim().to_lowercase();
    if install_type.is_empty() {
        INSTALL_TYPE_GITHUB_RELEASE.to_string()
    } else {
        install_type
    }
}

pub fn normalize_install_plan(plan: &InstallPlan) -> InstallPlan {
    let mut plan = plan.clone();
    plan.kind = plan.kind.trim().to_lowercase();
    for artifact in &mut plan.artifacts {
        artifact.goos = normalize_goos(&artifact.goos);
        artifact.goarch = normalize_goarch(&artifact.goarch);
        artifact.url = artifact.url.trim().to_string();
        artifact.sha256 = artifact.sha256.trim().to_lowercase();
    }
    plan
}

pub fn validate_install_plan(plan: &InstallPlan) -> Result<()> {
    let plan = normalize_install_plan(plan);
    if plan.kind.is_empty() {
        return Err(errf!("missing install type"));
    }
    if plan.kind != INSTALL_TYPE_DIRECT && plan.kind != INSTALL_TYPE_GITHUB_RELEASE {
        return Err(errf!("unsupported install type {:?}", plan.kind));
    }
    if plan.kind != INSTALL_TYPE_DIRECT {
        return Ok(());
    }
    if plan.artifacts.is_empty() {
        return Err(errf!("direct install requires at least one artifact"));
    }
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        validate_artifact(artifact).map_err(|err| err.wrap(format!("artifacts[{index}]")))?;
    }
    Ok(())
}

pub fn validate_artifact(artifact: &Artifact) -> Result<()> {
    let goos = normalize_goos(&artifact.goos);
    let goarch = normalize_goarch(&artifact.goarch);
    let url = artifact.url.trim();
    let sha256 = artifact.sha256.trim().to_lowercase();
    if goos.is_empty() {
        return Err(errf!("missing goos"));
    }
    if goarch.is_empty() {
        return Err(errf!("missing goarch"));
    }
    if url.is_empty() {
        return Err(errf!("missing url"));
    }
    let parsed = match GoUrl::parse(url) {
        Ok(parsed) if !parsed.scheme.is_empty() && !parsed.host.is_empty() => parsed,
        _ => return Err(errf!("invalid artifact url")),
    };
    if parsed.scheme != "https" && parsed.scheme != "http" {
        return Err(errf!("artifact url must use http or https"));
    }
    if has_sensitive_query_parameter(&parsed) {
        return Err(errf!("artifact url contains sensitive query parameter"));
    }
    if sha256.is_empty() {
        return Err(errf!("missing sha256"));
    }
    if sha256.len() != 64 {
        return Err(errf!("invalid sha256 length"));
    }
    if let Err(err) = hex::decode(&sha256) {
        return Err(errf!("invalid sha256: {}", hex_error_text(&err)));
    }
    if artifact.size < 0 {
        return Err(errf!("invalid size"));
    }
    Ok(())
}

/// Go's `hex.DecodeString` error text (`encoding/hex: invalid byte: U+0067 'g'`).
pub(crate) fn hex_error_text(err: &hex::FromHexError) -> String {
    match err {
        hex::FromHexError::InvalidHexCharacter { c, .. } => {
            format!("encoding/hex: invalid byte: U+{:04X} {:?}", *c as u32, c)
        }
        hex::FromHexError::OddLength => "encoding/hex: odd length hex string".to_string(),
        other => other.to_string(),
    }
}

pub fn plugin_platforms(plugin: &Plugin) -> Vec<Platform> {
    if plugin_install_type(plugin) != INSTALL_TYPE_DIRECT {
        return Vec::new();
    }
    let artifacts = plugin_artifacts(plugin);
    let mut seen: HashSet<Platform> = HashSet::with_capacity(artifacts.len());
    let mut platforms = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let platform = Platform { goos: artifact.goos, goarch: artifact.goarch };
        if platform.goos.is_empty() || platform.goarch.is_empty() {
            continue;
        }
        if seen.insert(platform.clone()) {
            platforms.push(platform);
        }
    }
    platforms
}

/// Normalized artifacts of the top-level install plan followed by each versioned plan.
pub fn plugin_artifacts(plugin: &Plugin) -> Vec<Artifact> {
    if plugin_install_type(plugin) != INSTALL_TYPE_DIRECT {
        return Vec::new();
    }
    let mut artifacts = normalize_install_plan(&plugin.install).artifacts;
    for version in &plugin.versions {
        artifacts.extend(normalize_install_plan(&version.install).artifacts);
    }
    artifacts
}

pub(crate) fn normalize_goos(goos: &str) -> String {
    let lowered = goos.trim().to_lowercase();
    match lowered.as_str() {
        "mac" | "macos" | "osx" => "darwin".to_string(),
        _ => lowered,
    }
}

pub(crate) fn normalize_goarch(goarch: &str) -> String {
    let lowered = goarch.trim().to_lowercase();
    match lowered.as_str() {
        "x64" | "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        _ => lowered,
    }
}

pub(crate) fn has_sensitive_query_parameter(parsed: &GoUrl) -> bool {
    if parsed.raw_query.is_empty() {
        return false;
    }
    parsed.query_keys().iter().any(|key| {
        matches!(
            key.trim().to_lowercase().as_str(),
            "token" | "access_token" | "access_key" | "secret" | "secret_key" | "api_key"
        )
    })
}

pub(crate) fn normalize_version(version: &str) -> String {
    let version = version.trim();
    let bytes = version.as_bytes();
    if bytes.len() > 1 && (bytes[0] == b'v' || bytes[0] == b'V') {
        return version[1..].to_string();
    }
    version.to_string()
}

pub(crate) fn valid_plugin_version(version: &str) -> bool {
    !version.is_empty() && !version.starts_with('v') && PLUGIN_VERSION_PATTERN.is_match(version)
}

pub(crate) fn valid_plugin_id(id: &str) -> bool {
    PLUGIN_ID_PATTERN.is_match(id)
}

/// Splits `https://github.com/{owner}/{repo}` into owner and repo.
pub fn github_repository_parts(repository: &str) -> Result<(String, String)> {
    let repository = repository.trim();
    let parsed = GoUrl::parse(repository).map_err(|err| errf!("invalid repository URL: {err}"))?;
    if parsed.scheme != "https" || parsed.host != "github.com" || !parsed.raw_query.is_empty() || !parsed.fragment.is_empty() {
        return Err(errf!("repository must be https://github.com/{{owner}}/{{repo}}"));
    }
    let escaped = parsed.escaped_path();
    let segments: Vec<&str> = escaped.trim_matches('/').split('/').collect();
    if segments.len() != 2 || segments[0].is_empty() || segments[1].is_empty() {
        return Err(errf!("repository must be https://github.com/{{owner}}/{{repo}}"));
    }
    let owner = path_unescape(segments[0]).map_err(|err| errf!("invalid repository owner: {err}"))?;
    let repo = path_unescape(segments[1]).map_err(|err| errf!("invalid repository name: {err}"))?;
    if repo.ends_with(".git") {
        return Err(errf!("repository must be https://github.com/{{owner}}/{{repo}}"));
    }
    Ok((owner, repo))
}

impl Registry {
    pub fn plugin_by_id(&self, id: &str) -> Option<&Plugin> {
        let id = id.trim();
        self.plugins.iter().find(|plugin| plugin.id.trim() == id)
    }
}
