//! Plugin store catalog (Go: `plugin_store.go`, `internal/pluginstore/registry.go`). Listing
//! fetches every configured registry and annotates entries with their (never installed) local
//! status; installing is unsupported because this build cannot load plugins.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::Uri;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::http::{ApiError, ApiResult, ok_struct};
use crate::state::ManagementState;

const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/router-for-me/CLIProxyAPI-Plugins-Store/main/registry.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Source {
    id: String,
    name: String,
    url: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Registry {
    schema_version: i64,
    plugins: Vec<Plugin>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Plugin {
    id: String,
    name: String,
    description: String,
    author: String,
    version: String,
    repository: String,
    logo: String,
    homepage: String,
    license: String,
    tags: Vec<String>,
    install: InstallPlan,
    auth_required: bool,
    versions: Vec<VersionEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct InstallPlan {
    r#type: String,
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VersionEntry {
    install: InstallPlan,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Artifact {
    goos: String,
    goarch: String,
}

/// Go: `html.EscapeString`, applied to every string the store endpoints return.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// Go: `pluginstore.NormalizeSources`: the official source first, then the configured URLs.
fn sources(configured: &[String]) -> Result<Vec<Source>, String> {
    let mut out = vec![Source {
        id: "official".into(),
        name: "Official".into(),
        url: DEFAULT_REGISTRY_URL.into(),
    }];
    let mut seen_urls: BTreeSet<String> = BTreeSet::from([DEFAULT_REGISTRY_URL.to_string()]);
    let mut seen_ids: Vec<(String, String)> = vec![("official".into(), DEFAULT_REGISTRY_URL.into())];
    for raw in configured {
        let url = raw.trim().to_string();
        if url.is_empty() || !seen_urls.insert(url.clone()) {
            continue;
        }
        let digest = hex::encode(Sha256::digest(url.as_bytes()));
        let id = format!("source-{}", &digest[..12]);
        let name = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| url.clone());
        if let Some((_, existing)) = seen_ids.iter().find(|(i, _)| *i == id) {
            return Err(format!(
                "plugin store source id collision for {existing:?} and {url:?}"
            ));
        }
        seen_ids.push((id.clone(), url.clone()));
        out.push(Source { id, name, url });
    }
    Ok(out)
}

fn valid_plugin_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn valid_version(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

/// A trimmed copy of Go's `ParseRegistry` + `ValidateRegistry` (schema, required fields, ids).
fn parse_registry(data: &[u8]) -> Result<Registry, String> {
    let mut reg: Registry =
        serde_json::from_slice(data).map_err(|e| format!("decode registry: {e}"))?;
    if reg.schema_version != 1 && reg.schema_version != 2 {
        return Err(format!("unsupported schema_version {}", reg.schema_version));
    }
    let mut seen = BTreeSet::new();
    for (i, p) in reg.plugins.iter_mut().enumerate() {
        for f in [
            &mut p.id,
            &mut p.name,
            &mut p.description,
            &mut p.author,
            &mut p.version,
            &mut p.repository,
            &mut p.logo,
            &mut p.homepage,
            &mut p.license,
        ] {
            *f = f.trim().to_string();
        }
        for t in &mut p.tags {
            *t = t.trim().to_string();
        }
        p.install.r#type = p.install.r#type.trim().to_lowercase();
        let kind = install_type(p);
        let github = kind == "github-release";
        for (field, value) in [
            ("id", &p.id),
            ("name", &p.name),
            ("description", &p.description),
            ("author", &p.author),
        ] {
            if value.is_empty() {
                return Err(format!("plugins[{i}]: missing required field {field}"));
            }
        }
        if github && p.repository.is_empty() {
            return Err(format!("plugins[{i}]: missing required field repository"));
        }
        if !valid_plugin_id(&p.id) {
            return Err(format!("plugins[{i}]: invalid plugin id {:?}", p.id));
        }
        if !p.version.is_empty() && !valid_version(&p.version) {
            return Err(format!("plugins[{i}]: invalid plugin version {:?}", p.version));
        }
        if !seen.insert(p.id.clone()) {
            return Err(format!("plugins[{i}]: duplicate plugin id {:?}", p.id));
        }
    }
    Ok(reg)
}

fn install_type(p: &Plugin) -> String {
    if p.install.r#type.is_empty() {
        "github-release".into()
    } else {
        p.install.r#type.clone()
    }
}

fn platforms(p: &Plugin) -> Vec<Value> {
    if install_type(p) != "direct" {
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let artifacts = p
        .install
        .artifacts
        .iter()
        .chain(p.versions.iter().flat_map(|v| v.install.artifacts.iter()));
    for a in artifacts {
        let (os, arch) = (a.goos.trim().to_lowercase(), a.goarch.trim().to_lowercase());
        if os.is_empty() || arch.is_empty() || !seen.insert((os.clone(), arch.clone())) {
            continue;
        }
        out.push(json!({"goos": os, "goarch": arch}));
    }
    out
}

async fn fetch_registry(proxy_url: &str, url: &str) -> Result<Registry, String> {
    let client = cpa_auth::http::build_client(proxy_url, Some(REQUEST_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", "CLIProxyAPI")
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("unexpected status {}", status.as_u16()));
    }
    let data = resp.bytes().await.map_err(|e| e.without_url().to_string())?;
    parse_registry(&data)
}

/// `GET /plugin-store`.
pub(crate) async fn list(st: &ManagementState) -> ApiResult {
    let cfg = st.cfg();
    let dir = if cfg.plugins.dir.trim().is_empty() {
        "plugins".to_string()
    } else {
        cfg.plugins.dir.trim().to_string()
    };
    let sources = sources(&cfg.plugins.store_sources)
        .map_err(|m| ApiError::with_message(500, "plugin_store_source_invalid", m))?;
    let proxy = cfg.proxy_url.trim().to_string();

    let mut entries = Vec::new();
    let mut errors: Vec<(&Source, String)> = Vec::new();
    for source in &sources {
        match fetch_registry(&proxy, &source.url).await {
            Ok(reg) => {
                for p in reg.plugins {
                    entries.push((source, p));
                }
            }
            Err(message) => errors.push((source, message)),
        }
    }
    if entries.is_empty()
        && let Some((_, message)) = errors.first()
    {
        return Err(ApiError::with_message(
            502,
            "plugin_store_registry_failed",
            message.clone(),
        ));
    }

    let mut plugins = Vec::with_capacity(entries.len());
    for (source, p) in &entries {
        let configured = cfg.plugins.configs.get(&p.id);
        let enabled = configured.and_then(|c| c.enabled).unwrap_or(false);
        let mut m = Map::new();
        m.insert("store_id".into(), esc(&format!("{}/{}", source.id, p.id)).into());
        m.insert("source_id".into(), esc(&source.id).into());
        m.insert("source_name".into(), esc(&source.name).into());
        m.insert("source_url".into(), esc(&source.url).into());
        m.insert("id".into(), esc(&p.id).into());
        m.insert("name".into(), esc(&p.name).into());
        m.insert("description".into(), esc(&p.description).into());
        m.insert("author".into(), esc(&p.author).into());
        m.insert("version".into(), esc(&p.version).into());
        m.insert("repository".into(), esc(&p.repository).into());
        m.insert("install_type".into(), esc(&install_type(p)).into());
        m.insert("auth_required".into(), p.auth_required.into());
        m.insert("auth_configured".into(), false.into());
        let plats = platforms(p);
        if !plats.is_empty() {
            m.insert("platforms".into(), Value::Array(plats));
        }
        for (key, value) in [
            ("logo", &p.logo),
            ("homepage", &p.homepage),
            ("license", &p.license),
        ] {
            if !value.is_empty() {
                m.insert(key.into(), esc(value).into());
            }
        }
        if !p.tags.is_empty() {
            m.insert(
                "tags".into(),
                Value::Array(p.tags.iter().map(|t| esc(t).into()).collect()),
            );
        }
        m.insert("installed".into(), false.into());
        m.insert("installed_version".into(), "".into());
        m.insert("path".into(), "".into());
        m.insert("configured".into(), configured.is_some().into());
        m.insert("registered".into(), false.into());
        m.insert("enabled".into(), enabled.into());
        m.insert("effective_enabled".into(), false.into());
        m.insert("update_available".into(), false.into());
        plugins.push(Value::Object(m));
    }

    let mut body = Map::new();
    body.insert("plugins_enabled".into(), cfg.plugins.enabled.into());
    body.insert("plugins_dir".into(), esc(&dir).into());
    body.insert(
        "sources".into(),
        Value::Array(
            sources
                .iter()
                .map(|s| json!({"id": esc(&s.id), "name": esc(&s.name), "url": esc(&s.url)}))
                .collect(),
        ),
    );
    if !errors.is_empty() {
        body.insert(
            "source_errors".into(),
            Value::Array(
                errors
                    .iter()
                    .map(|(s, m)| {
                        json!({"source_id": esc(&s.id), "source_name": esc(&s.name),
                               "source_url": esc(&s.url), "message": esc(m)})
                    })
                    .collect(),
            ),
        );
    }
    body.insert("plugins".into(), Value::Array(plugins));
    Ok(ok_struct(&Value::Object(body)))
}

/// `POST /plugin-store/:id/install`: installing needs a plugin host.
pub(crate) async fn install(
    _st: &ManagementState,
    _id: &str,
    _uri: &Uri,
    _body: &[u8],
) -> ApiResult {
    Err(ApiError::with_message(
        501,
        "plugins_not_supported",
        "plugins are not supported by this server",
    ))
}
