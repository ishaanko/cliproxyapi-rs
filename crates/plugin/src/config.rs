//! Plugin runtime configuration derived from the host config (Go: `config.go`).

use std::collections::BTreeMap;

use cpa_config::Config;
use serde_yaml_ng::Value as Yaml;

use crate::platform::normalize_desired_version;

const DEFAULT_CONFIG_YAML: &str = "enabled: false\npriority: 0\n";

#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    pub enabled: bool,
    pub dir: String,
    pub items: BTreeMap<String, RuntimeItem>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeItem {
    pub id: String,
    pub enabled: bool,
    pub priority: i64,
    pub version: String,
    pub config_yaml: Vec<u8>,
}

pub fn default_runtime_item(id: &str) -> RuntimeItem {
    RuntimeItem { id: id.to_string(), config_yaml: DEFAULT_CONFIG_YAML.as_bytes().to_vec(), ..Default::default() }
}

/// Go: `runtimeConfigFromConfig`. A disabled `plugins` section yields an empty item table.
pub fn runtime_config_from_config(cfg: Option<&Config>) -> Result<RuntimeConfig, String> {
    let mut out = RuntimeConfig { dir: "plugins".into(), ..Default::default() };
    let Some(cfg) = cfg else { return Ok(out) };
    out.enabled = cfg.plugins.enabled;
    if !out.enabled {
        return Ok(out);
    }
    out.dir = cpa_config::resolve_plugins_dir(&cfg.plugins.dir).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    for (id, item) in &cfg.plugins.configs {
        let enabled = item.enabled.unwrap_or(false);
        out.items.insert(
            id.clone(),
            RuntimeItem {
                id: id.clone(),
                enabled,
                priority: item.priority,
                version: desired_version(&item.raw),
                config_yaml: runtime_config_yaml(&item.raw, enabled, item.priority),
            },
        );
    }
    Ok(out)
}

fn yaml_scalar_string(v: Option<&Yaml>) -> String {
    match v {
        Some(Yaml::String(s)) => s.trim().to_string(),
        Some(Yaml::Number(n)) => n.to_string(),
        Some(Yaml::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// `store.version`, else `store.release-tag` (Go: `pluginConfigDesiredVersion`).
fn desired_version(raw: &Yaml) -> String {
    let Some(store) = raw.get("store") else { return String::new() };
    let v = normalize_desired_version(&yaml_scalar_string(store.get("version")));
    if !v.is_empty() {
        return v;
    }
    normalize_desired_version(&yaml_scalar_string(store.get("release-tag")))
}

/// The YAML handed to the plugin: the raw subtree with `enabled` and `priority` ensured.
fn runtime_config_yaml(raw: &Yaml, enabled: bool, priority: i64) -> Vec<u8> {
    let node = match raw {
        Yaml::Null => {
            let mut m = serde_yaml_ng::Mapping::new();
            m.insert(Yaml::String("enabled".into()), Yaml::Bool(enabled));
            m.insert(Yaml::String("priority".into()), Yaml::Number(priority.into()));
            Yaml::Mapping(m)
        }
        Yaml::Mapping(m) => {
            let mut m = m.clone();
            if !m.contains_key("enabled") {
                m.insert(Yaml::String("enabled".into()), Yaml::Bool(enabled));
            }
            if !m.contains_key("priority") {
                m.insert(Yaml::String("priority".into()), Yaml::Number(priority.into()));
            }
            Yaml::Mapping(m)
        }
        other => other.clone(),
    };
    let text = serde_yaml_ng::to_string(&node).unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return DEFAULT_CONFIG_YAML.as_bytes().to_vec();
    }
    let mut out = text.as_bytes().to_vec();
    out.push(b'\n');
    out
}

/// Desired versions of the items that pin one (Go: `desiredPluginVersions`).
pub fn desired_versions(items: &BTreeMap<String, RuntimeItem>) -> std::collections::HashMap<String, String> {
    items
        .iter()
        .filter_map(|(id, item)| {
            let (id, v) = (id.trim(), item.version.trim());
            (!id.is_empty() && !v.is_empty()).then(|| (id.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_config::PluginInstanceConfig;

    fn raw(text: &str) -> Yaml {
        serde_yaml_ng::from_str(text).expect("valid yaml")
    }

    fn configured(id: &str, raw_yaml: &str) -> Config {
        let mut cfg = Config::default();
        cfg.plugins.enabled = true;
        cfg.plugins.configs.insert(id.into(), PluginInstanceConfig { enabled: Some(true), priority: 0, raw: raw(raw_yaml) });
        cfg
    }

    #[test]
    fn runtime_yaml_adds_host_defaults_to_the_raw_plugin_config() {
        let got = String::from_utf8(runtime_config_yaml(&raw("config1: true\nconfig2: value\n"), true, 3)).unwrap();
        for want in ["config1: true", "config2: value", "enabled: true", "priority: 3"] {
            assert!(got.contains(want), "missing {want:?} in:\n{got}");
        }
    }

    #[test]
    fn runtime_yaml_defaults_enabled_false() {
        let got = String::from_utf8(runtime_config_yaml(&Yaml::Null, false, 3)).unwrap();
        for want in ["enabled: false", "priority: 3"] {
            assert!(got.contains(want), "missing {want:?} in:\n{got}");
        }
    }

    #[test]
    fn store_version_is_extracted_from_the_store_section() {
        let cfg = configured("alpha", "store:\n  version: 1.0.3\n  release-tag: v1.0.3\n");
        let got = runtime_config_from_config(Some(&cfg)).unwrap();
        assert_eq!(got.items["alpha"].version, "1.0.3");
    }

    #[test]
    fn store_version_derives_from_the_release_tag() {
        let cfg = configured("alpha", "store:\n  release-tag: v1.0.3\n");
        let got = runtime_config_from_config(Some(&cfg)).unwrap();
        assert_eq!(got.items["alpha"].version, "1.0.3");
    }

    #[test]
    fn disabled_plugins_section_yields_no_items() {
        let mut cfg = configured("alpha", "enabled: true\n");
        cfg.plugins.enabled = false;
        assert!(runtime_config_from_config(Some(&cfg)).unwrap().items.is_empty());
    }
}
