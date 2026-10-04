//! v8 <-> legacy YAML layout translation (port of `config_v8.go`).
//!
//! The runtime [`Config`](crate::Config) always uses the legacy field names. Documents may use the
//! v8 layout, the legacy layout, or a mix; a v8 value that is present (even `false`, `0`, `[]`)
//! wins over its legacy twin. [`flatten_v8`] turns any of them into the legacy layout, which is
//! then decoded with serde.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, RwLock};

use serde_yaml_ng::{Mapping, Value};

use crate::comments::{CPath, Comments, Seg, dotted};
use crate::error::{ConfigError, Result};
use crate::yamlpath::{
    delete_yaml_path, empty_map, legacy_path, parse_yaml, set_yaml_path, str_key, strip_nulls,
    yaml_path,
};

/// Legacy path -> v8 path prefixes. Order matters: the first match wins.
const PREFIXES: &[(&str, &str)] = &[
    ("host", "server.host"),
    ("port", "server.port"),
    ("trusted-proxies", "server.trusted-proxies"),
    ("tls", "server.tls"),
    ("commercial-mode", "server.commercial-mode"),
    ("discovery", "server.discovery"),
    ("remote-management", "management"),
    ("api-keys", "access.api-keys"),
    ("credential-concurrency", "credentials.concurrency"),
    ("credential-in-flight", "credentials.in-flight"),
    ("force-model-prefix", "routing.force-model-prefix"),
    ("request-retry", "routing.retry.request-retry"),
    (
        "max-retry-credentials",
        "routing.retry.max-retry-credentials",
    ),
    ("max-retry-interval", "routing.retry.max-retry-interval"),
    ("disable-cooling", "routing.cooldown.disable-cooling"),
    (
        "save-cooldown-status",
        "routing.cooldown.save-cooldown-status",
    ),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
    ),
    ("proxy-url", "requests.proxy-url"),
    ("passthrough-headers", "requests.passthrough-headers"),
    (
        "nonstream-keepalive-interval",
        "requests.nonstream-keepalive-interval",
    ),
    ("streaming", "requests.streaming"),
    ("payload", "requests.payload"),
    ("auth-dir", "oauth.auth-dir"),
    (
        "auth-auto-refresh-workers",
        "oauth.auth-auto-refresh-workers",
    ),
    ("oauth-model-alias", "oauth.model-alias"),
    ("oauth-excluded-models", "oauth.excluded-models"),
    ("oauth-request-scoped-errors", "oauth.request-scoped-errors"),
    ("oauth-settings", "oauth.settings"),
    ("ws-auth", "oauth.providers.aistudio.ws-auth"),
    // Shared Codex fields precede the `codex` container: the first match wins.
    (
        "codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    ("codex.response-steering", "upstream.codex.response-steering"),
    ("codex", "oauth.providers.codex"),
    (
        "codex-header-defaults",
        "oauth.providers.codex.header-defaults",
    ),
    ("claude", "upstream.claude"),
    ("claude-code", "upstream.claude"),
    (
        "disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    (
        "claude-header-defaults",
        "upstream.claude.header-defaults",
    ),
    ("antigravity", "oauth.providers.antigravity"),
    (
        "antigravity-signature-cache-enabled",
        "oauth.providers.antigravity.signature-cache-enabled",
    ),
    (
        "antigravity-signature-bypass-strict",
        "oauth.providers.antigravity.signature-bypass-strict",
    ),
    (
        "quota-exceeded.antigravity-credits",
        "oauth.providers.antigravity.antigravity-credits",
    ),
    ("xai", "upstream.xai"),
    ("devin", "oauth.providers.devin"),
    (
        "disable-image-generation",
        "multimedia.disable-image-generation",
    ),
    (
        "gpt-image-2-base-model",
        "multimedia.gpt-image-2-base-model",
    ),
    (
        "video-result-auth-cache-ttl",
        "multimedia.video-result-auth-cache-ttl",
    ),
    ("debug", "observability.logs.debug"),
    ("logging-to-file", "observability.logs.logging-to-file"),
    (
        "logs-max-total-size-mb",
        "observability.logs.logs-max-total-size-mb",
    ),
    ("request-log", "observability.logs.request-log"),
    (
        "error-logs-max-files",
        "observability.logs.error-logs-max-files",
    ),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
    ),
    ("pprof", "observability.pprof"),
];

/// Every legacy leaf field that has a v8 twin, in the order Go discovers them by reflection.
/// Slices, maps and pointers are leaves; nested structs are expanded.
const LEAF_PATHS: &[&str] = &[
    "proxy-url",
    "disable-image-generation",
    "gpt-image-2-base-model",
    "video-result-auth-cache-ttl",
    "force-model-prefix",
    "request-log",
    "claude-code.disable-cloaking-model-list",
    "api-keys",
    "passthrough-headers",
    "streaming.keepalive-seconds",
    "streaming.bootstrap-retries",
    "nonstream-keepalive-interval",
    "host",
    "port",
    "trusted-proxies",
    "tls.enable",
    "tls.cert",
    "tls.key",
    "credential-concurrency.lifecycle-config-revision",
    "credential-concurrency.observation-barrier-revision",
    "credential-concurrency.cpa-heartbeat-timeout",
    "credential-concurrency.cpa-cancel-bound",
    "credential-concurrency.reclaim-grace",
    "credential-concurrency.cleanup-interval",
    "credential-concurrency.release-flush-interval",
    "credential-concurrency.release-max-backoff",
    "credential-concurrency.busy-retry-min",
    "credential-concurrency.busy-retry-max",
    "credential-concurrency.max-limit",
    "credential-in-flight.snapshot-interval",
    "credential-in-flight.stale-after",
    "credential-in-flight.max-part-bytes",
    "credential-in-flight.max-part-count",
    "credential-in-flight.max-revision-bytes",
    "credential-in-flight.max-aggregate-groups",
    "credential-in-flight.max-details",
    "credential-in-flight.max-string-bytes",
    "credential-in-flight.staging-retention",
    "remote-management.allow-remote",
    "remote-management.secret-key",
    "remote-management.disable-control-panel",
    "remote-management.disable-auto-update-panel",
    "remote-management.panel-github-repository",
    "remote-management.base-url",
    "auth-dir",
    "debug",
    "pprof.enable",
    "pprof.addr",
    "discovery.enabled",
    "discovery.service-name",
    "discovery.service-type",
    "discovery.subtypes",
    "discovery.interfaces.include",
    "discovery.interfaces.exclude",
    "discovery.auth-required",
    "discovery.advertise-management",
    "commercial-mode",
    "logging-to-file",
    "logs-max-total-size-mb",
    "error-logs-max-files",
    "usage-statistics-enabled",
    "redis-usage-queue-retention-seconds",
    "disable-cooling",
    "save-cooldown-status",
    "transient-error-cooldown-seconds",
    "auth-auto-refresh-workers",
    "request-retry",
    "max-retry-credentials",
    "max-retry-interval",
    "quota-exceeded.antigravity-credits",
    "ws-auth",
    "antigravity-signature-cache-enabled",
    "antigravity-signature-bypass-strict",
    "antigravity.sensitive-words",
    "antigravity.connection-pool.enabled",
    "antigravity.connection-pool.idle-conn-timeout",
    "antigravity.connection-pool.max-idle-conns-per-host",
    "devin.sensitive-words",
    "xai.inject-x-search",
    "codex.disable-codex-cloaking",
    "codex.stream-bootstrap-buffering",
    "codex.stream-bootstrap-timeout",
    "codex.orphan-delegation-compatibility",
    "codex.model-level-cooling",
    "codex.live-media-relay.enabled",
    "codex.live-media-relay.max-sessions",
    "codex.live-media-relay.disable-private-remote-ips",
    "codex.live-media-relay.public-ip",
    "codex.live-media-relay.udp-port-min",
    "codex.live-media-relay.udp-port-max",
    "codex.live-media-relay.ice-servers",
    "codex.response-steering",
    "codex-header-defaults.user-agent",
    "codex-header-defaults.beta-features",
    "claude.model-level-cooling",
    "claude-header-defaults.user-agent",
    "claude-header-defaults.package-version",
    "claude-header-defaults.runtime-version",
    "claude-header-defaults.os",
    "claude-header-defaults.arch",
    "claude-header-defaults.timeout",
    "claude-header-defaults.timezone",
    "claude-header-defaults.stabilize-device-profile",
    "disable-claude-cloak-mode",
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
    "payload.default",
    "payload.default-raw",
    "payload.override",
    "payload.override-raw",
    "payload.filter",
];

/// Legacy struct containers that have a v8 twin (an empty or null one is moved as an empty map).
const STRUCT_PATHS: &[&str] = &[
    "claude-code",
    "streaming",
    "tls",
    "credential-concurrency",
    "credential-in-flight",
    "remote-management",
    "pprof",
    "discovery",
    "discovery.interfaces",
    "antigravity",
    "antigravity.connection-pool",
    "devin",
    "xai",
    "codex",
    "codex.live-media-relay",
    "codex-header-defaults",
    "claude",
    "claude-header-defaults",
    "payload",
];

/// Historical spellings of the client options. The canonical `client.codex.*` wins by presence;
/// otherwise the first of these that is present is used.
pub(crate) const CLIENT_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
];

/// Historical OAuth spellings of the shared upstream fields (`upstream.*`). The canonical path
/// wins by presence; otherwise the historical OAuth field applies, and both precede the global
/// legacy field.
const SHARED_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "oauth.providers.codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "oauth.providers.codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "oauth.providers.codex.response-steering",
        "upstream.codex.response-steering",
    ),
    (
        "oauth.providers.claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
    ),
    (
        "oauth.providers.claude.claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    (
        "oauth.providers.claude.disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    (
        "oauth.providers.claude.header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
    ),
    (
        "oauth.providers.claude.header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
    ),
    (
        "oauth.providers.claude.header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
    ),
    (
        "oauth.providers.claude.header-defaults.os",
        "upstream.claude.header-defaults.os",
    ),
    (
        "oauth.providers.claude.header-defaults.arch",
        "upstream.claude.header-defaults.arch",
    ),
    (
        "oauth.providers.claude.header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
    ),
    (
        "oauth.providers.claude.header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
    ),
    (
        "oauth.providers.claude.header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
    ),
    (
        "oauth.providers.xai.inject-x-search",
        "upstream.xai.inject-x-search",
    ),
];

/// Historical empty containers of the shared upstream settings (an empty or null one is moved as
/// an empty mapping). Longest paths first.
pub(crate) const SHARED_STRUCT_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.claude.header-defaults",
        "upstream.claude.header-defaults",
    ),
    ("oauth.providers.claude.claude-code", "upstream.claude"),
    ("oauth.providers.claude", "upstream.claude"),
    ("oauth.providers.xai", "upstream.xai"),
];

/// The historical alias paths as (historical, canonical) pairs.
pub fn v8_alias_paths() -> Vec<(&'static str, &'static str)> {
    aliases().copied().collect()
}

/// Every historical alias: the client option spellings, then the shared upstream ones.
pub(crate) fn aliases() -> impl Iterator<Item = &'static (&'static str, &'static str)> + Clone {
    CLIENT_PATHS.iter().chain(SHARED_PATHS.iter())
}

/// Legacy API-key family field -> v8 `api-keys.<name>`.
pub(crate) const KEY_FAMILIES: &[(&str, &str)] = &[
    ("gemini-api-key", "gemini"),
    ("interactions-api-key", "interactions"),
    ("vertex-api-key", "vertex"),
    ("codex-api-key", "codex"),
    ("claude-api-key", "claude"),
    ("xai-api-key", "xai"),
    ("meta-api-key", "meta"),
    ("openai-compatibility", "openai-compatibility"),
];

/// Fields a v8 key group may carry besides `name`, `base-url` and `keys`; each can be overridden
/// at key level.
const SHARED_KEY_FIELDS: &[&str] = &[
    "priority",
    "prefix",
    "proxy-url",
    "headers",
    "models",
    "excluded-models",
    "disable-cooling",
    "request-retry",
    "request-scoped-errors",
];

fn v8_for(old: &str) -> Option<String> {
    PREFIXES.iter().find_map(|(prefix, current)| {
        (old == *prefix || old.strip_prefix(prefix).is_some_and(|r| r.starts_with('.')))
            .then(|| format!("{current}{}", &old[prefix.len()..]))
    })
}

/// (legacy leaf path, v8 path) pairs in discovery order.
static V8_PATHS: LazyLock<Vec<(&'static str, String)>> = LazyLock::new(|| {
    LEAF_PATHS
        .iter()
        .filter_map(|old| v8_for(old).map(|cur| (*old, cur)))
        .collect()
});

/// (legacy struct path, v8 path) pairs.
static V8_STRUCT_PATHS: LazyLock<Vec<(&'static str, String)>> = LazyLock::new(|| {
    STRUCT_PATHS
        .iter()
        .filter_map(|old| v8_for(old).map(|cur| (*old, cur)))
        .collect()
});

/// (legacy leaf path, v8 path) pairs in discovery order.
pub(crate) fn v8_paths() -> impl Iterator<Item = &'static (&'static str, String)> {
    V8_PATHS.iter()
}

/// Whether a v8 path lives under `oauth.providers.*` (such fields are OAuth-only).
pub(crate) fn is_oauth_only_path(current: &str) -> bool {
    current.starts_with("oauth.providers.")
}

/// Legacy leaf fields set via `oauth.providers.*` in `source`.
pub(crate) fn oauth_only_fields(source: &Value) -> Vec<String> {
    V8_PATHS
        .iter()
        .filter(|(_, cur)| is_oauth_only_path(cur) && yaml_path(source, cur).is_some())
        .map(|(old, _)| (*old).to_string())
        .collect()
}

/// Whether a parsed document is in the v8 layout (declared version or any v8-only path). Roots
/// shared with the legacy schema do not alone indicate v8 (port of `IsV8ConfigLayout`).
pub fn is_v8_config_layout(root: &Value) -> bool {
    let Value::Mapping(map) = root else {
        return false;
    };
    let mut sections = v8_allowed_roots();
    for shared in ["api-keys", "plugins", "quota-exceeded", "routing", "client"] {
        sections.remove(shared);
    }
    if map
        .keys()
        .any(|key| key.as_str().is_some_and(|k| sections.contains(k)))
    {
        return true;
    }
    if yaml_path(root, "api-keys").is_some_and(Value::is_mapping) {
        return true;
    }
    let pairs = V8_PATHS
        .iter()
        .chain(V8_STRUCT_PATHS.iter())
        .map(|(o, c)| (*o, c.as_str()))
        .chain(aliases().map(|(o, c)| (*o, *c)));
    for (old, current) in pairs {
        if old != current && yaml_path(root, current).is_some() {
            return true;
        }
        if old.starts_with("providers.") && yaml_path(root, old).is_some() {
            return true;
        }
        if old != current
            && let Some(rest) = current.strip_prefix("routing.")
        {
            let child = rest.split('.').next().unwrap_or_default();
            if yaml_path(root, &format!("routing.{child}")).is_some() {
                return true;
            }
        }
    }
    false
}

/// Root keys a v8 document may contain.
fn v8_allowed_roots() -> HashSet<String> {
    let mut allowed: HashSet<String> = [
        "config-version",
        "api-keys",
        "plugins",
        "quota-exceeded",
        "client",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for (_, cur) in V8_PATHS.iter() {
        if let Some((section, _)) = cur.split_once('.') {
            allowed.insert(section.to_string());
        } else {
            allowed.insert(cur.clone());
        }
    }
    allowed
}

/// Known child keys per v8 container path, used to comment out unknown fields on migration.
static V8_CHILDREN: LazyLock<HashMap<String, HashSet<String>>> = LazyLock::new(|| {
    let mut children: HashMap<String, HashSet<String>> = HashMap::new();
    let mut add = |parent: &str, child: &str| {
        children
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
    };
    for (_, cur) in V8_PATHS.iter().chain(V8_STRUCT_PATHS.iter()) {
        let parts: Vec<&str> = cur.split('.').collect();
        for i in 1..parts.len() {
            add(&parts[..i].join("."), parts[i]);
        }
    }
    // Containers that keep the same path in both layouts.
    for child in [
        "strategy",
        "session-affinity",
        "session-affinity-ttl",
        "session-affinity-subagents",
    ] {
        add("routing", child);
    }
    for child in [
        "enabled",
        "dir",
        "store-sources",
        "store-auth",
        "auth-revision",
        "configs",
    ] {
        add("plugins", child);
    }
    for child in [
        "switch-project",
        "switch-preview-model",
        "antigravity-credits",
    ] {
        add("quota-exceeded", child);
    }
    add("client", "codex");
    add("client.codex", "optimize-multi-agent-v2");
    add("client.codex", "enable-apply-patch");
    children
});

/// Receives (section, message) for each unrecognised section commented out during migration.
pub type WarnFn = Arc<dyn Fn(&str, &str) + Send + Sync>;
static WARN_FN: RwLock<Option<WarnFn>> = RwLock::new(None);

/// Installs a handler for "unrecognized section commented out" warnings (`None` restores the
/// default, which logs through `tracing`).
pub fn set_v8_migration_warn_func(f: Option<WarnFn>) {
    if let Ok(mut guard) = WARN_FN.write() {
        *guard = f;
    }
}

fn warn_unrecognized_v8_section(section: &str) {
    let msg =
        format!("unrecognized configuration section {section:?} commented out during v8 migration");
    let handler = WARN_FN.read().ok().and_then(|g| g.clone());
    match handler {
        Some(f) => f(section, &msg),
        None => tracing::warn!("{msg}"),
    }
}

// ---------------------------------------------------------------------------------------------
// flatten (v8 / mixed / legacy -> legacy)
// ---------------------------------------------------------------------------------------------

/// Converts a parsed document (any layout) into the legacy layout with presence-based precedence.
/// The input must already have anchors and merge keys expanded (see `parse_yaml`).
pub(crate) fn flatten_v8(node: &Value) -> Result<Value> {
    if !node.is_mapping() {
        return Err(ConfigError::invalid("config must be a mapping"));
    }
    let mut node = node.clone();
    normalize_v8_private_ip_alias(&mut node, true)?;
    let all_paths = V8_PATHS
        .iter()
        .map(|(_, cur)| cur.as_str())
        .chain(aliases().map(|(_, cur)| *cur));
    for path in all_paths {
        let parts: Vec<&str> = path.split('.').collect();
        for i in 1..parts.len() {
            let prefix = parts[..i].join(".");
            let Some(parent) = yaml_path(&node, &prefix) else {
                break;
            };
            // Routing is shared with the legacy layout, where null means defaults.
            if i == 1 && parts[0] == "routing" && parent.is_null() {
                break;
            }
            if !parent.is_mapping() {
                return Err(ConfigError::invalid(format!("{prefix} must be a mapping")));
            }
        }
    }
    let mut root = node.clone();
    for (old, current) in aliases() {
        if let Some(value) = yaml_path(&root, old).cloned() {
            if yaml_path(&root, current).is_none() {
                set_yaml_path(&mut root, current, value);
            }
            delete_yaml_path(&mut root, old);
        }
    }
    for (old, current) in SHARED_STRUCT_PATHS {
        let Some(value) = yaml_path(&root, old) else {
            continue;
        };
        if !value.is_null() && !value.is_mapping() {
            return Err(ConfigError::invalid(format!("{old} must be a mapping")));
        }
        if matches!(value, Value::Mapping(m) if !m.is_empty()) {
            continue;
        }
        if yaml_path(&root, current).is_none() {
            set_yaml_path(&mut root, current, empty_map());
        }
        delete_yaml_path(&mut root, old);
    }
    if let Some(version) = yaml_path(&root, "config-version")
        && version.as_i64() != Some(8)
    {
        return Err(ConfigError::invalid(
            "unsupported config-version (expected 8)",
        ));
    }
    // The v8 upstream map reuses the legacy client-key field name.
    if yaml_path(&root, "api-keys").is_some_and(Value::is_mapping) {
        delete_yaml_path(&mut root, "api-keys");
    }
    for (old, current) in V8_PATHS.iter() {
        if let Some(value) = yaml_path(&root, current).cloned() {
            delete_yaml_path(&mut root, current);
            set_yaml_path(&mut root, old, value);
        }
    }
    for (old, family) in KEY_FAMILIES {
        if let Some(groups) = yaml_path(&node, &format!("api-keys.{family}")) {
            let keys = expand_v8_groups(groups, family)?;
            set_yaml_path(&mut root, old, keys);
        }
    }
    Ok(root)
}

/// Transient management-only fields carrying a credential's live index.
const AUTH_INDEX_FIELDS: [&str; 2] = ["auth_index", "auth-index"];

/// Flattens one family's v8 groups (`{name, base-url, keys: [..]}`) into one legacy entry per key.
fn expand_v8_groups(groups: &Value, provider: &str) -> Result<Value> {
    let Value::Sequence(groups) = groups else {
        return Err(ConfigError::invalid(format!(
            "api-keys.{provider} must be a list"
        )));
    };
    let mut out = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let Value::Mapping(group_map) = group else {
            return Err(ConfigError::invalid(format!(
                "api-keys.{provider}[{index}] must be a mapping"
            )));
        };
        let keys = match group_map.get("keys") {
            Some(keys @ Value::Sequence(_)) => keys,
            _ => {
                return Err(ConfigError::invalid(format!(
                    "api-keys.{provider}[{index}].keys must be a list"
                )));
            }
        };
        validate_weight_sequence_node(keys, &format!("api-keys.{provider}.keys"))?;
        if provider == "openai-compatibility" {
            // `auth_index` is a transient management field, never part of the stored config.
            let mut item = group.clone();
            delete_yaml_path(&mut item, "keys");
            for name in AUTH_INDEX_FIELDS {
                delete_yaml_path(&mut item, name);
            }
            let mut clean_keys = keys.clone();
            if let Value::Sequence(entries) = &mut clean_keys {
                for entry in entries {
                    for name in AUTH_INDEX_FIELDS {
                        delete_yaml_path(entry, name);
                    }
                }
            }
            set_yaml_path(&mut item, "api-key-entries", clean_keys);
            out.push(item);
            continue;
        }
        for field in group_map.keys().filter_map(Value::as_str) {
            if !matches!(field, "name" | "base-url" | "keys") && !SHARED_KEY_FIELDS.contains(&field)
            {
                return Err(ConfigError::invalid(format!(
                    "api-keys.{provider}: unsupported group field {field}"
                )));
            }
        }
        let Value::Sequence(keys) = keys else {
            continue;
        };
        for key in keys {
            let Value::Mapping(key_map) = key else {
                return Err(ConfigError::invalid(format!(
                    "api-keys.{provider} key must be a mapping"
                )));
            };
            if key_map.contains_key("base-url") {
                return Err(ConfigError::invalid(format!(
                    "api-keys.{provider}: base-url belongs to the group"
                )));
            }
            let mut item = empty_map();
            for (field, value) in group_map {
                let Some(field) = field.as_str() else {
                    continue;
                };
                if field == "base-url" || SHARED_KEY_FIELDS.contains(&field) {
                    set_yaml_path(&mut item, field, value.clone());
                }
            }
            for (field, value) in key_map {
                // Key-level null means "inherit the group value".
                if let (Some(field), false) = (field.as_str(), value.is_null())
                    && !AUTH_INDEX_FIELDS.contains(&field)
                {
                    set_yaml_path(&mut item, field, value.clone());
                }
            }
            out.push(item);
        }
    }
    Ok(Value::Sequence(out))
}

/// Inverse of [`expand_v8_groups`]. Deliberately one group per legacy entry: equal endpoints do
/// not imply equal routing, headers, models or credential policies.
pub(crate) fn group_legacy_keys(keys: &Value, provider: &str) -> Value {
    let Value::Sequence(entries) = keys else {
        return Value::Sequence(Vec::new());
    };
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if provider == "openai-compatibility" {
            let mut group = entry.clone();
            let entries = yaml_path(&group, "api-key-entries")
                .cloned()
                .unwrap_or(Value::Sequence(Vec::new()));
            set_yaml_path(&mut group, "keys", entries);
            delete_yaml_path(&mut group, "api-key-entries");
            out.push(group);
            continue;
        }
        let mut group = empty_map();
        set_yaml_path(
            &mut group,
            "name",
            Value::String(format!("{provider}-{}", index + 1)),
        );
        let mut key = entry.clone();
        if let Value::Mapping(entry_map) = entry {
            for (field, value) in entry_map {
                let Some(field) = field.as_str() else {
                    continue;
                };
                if field == "base-url" || SHARED_KEY_FIELDS.contains(&field) {
                    set_yaml_path(&mut group, field, value.clone());
                    delete_yaml_path(&mut key, field);
                }
            }
        }
        set_yaml_path(&mut group, "keys", Value::Sequence(vec![key]));
        out.push(group);
    }
    Value::Sequence(out)
}

/// The deprecated allow flag is the inverse of `disable-private-remote-ips`. Resolve it before v8
/// precedence so both spellings never reach the decoder together.
fn normalize_v8_private_ip_alias(root: &mut Value, migrate: bool) -> Result<bool> {
    const OLD: &str = "codex.live-media-relay.allow-private-remote-ips";
    const CANONICAL: &str = "codex.live-media-relay.disable-private-remote-ips";
    let Some(value) = yaml_path(root, OLD) else {
        return Ok(false);
    };
    if yaml_path(root, &format!("oauth.providers.{CANONICAL}")).is_some() {
        return Ok(delete_yaml_path(root, OLD));
    }
    if !migrate || yaml_path(root, CANONICAL).is_some() {
        return Ok(false);
    }
    // Null decodes to false, and yes/on/y spellings are accepted, as in Go's typed decode.
    let allow = crate::lenient::from_value::<bool>(value.clone())
        .map_err(|e| ConfigError::invalid(format!("decode {OLD}: {e}")))?;
    set_yaml_path(root, CANONICAL, Value::Bool(!allow));
    delete_yaml_path(root, OLD);
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// credential weights
// ---------------------------------------------------------------------------------------------

pub const MAX_CREDENTIAL_WEIGHT: i64 = 1_000_000;

/// Validates `weight` keys of every mapping in a sequence (`path[i].weight`).
pub(crate) fn validate_weight_sequence_node(seq: &Value, path: &str) -> Result<()> {
    let Value::Sequence(items) = seq else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        let Value::Mapping(map) = item else { continue };
        let Some(weight) = map.get("weight") else {
            continue;
        };
        let item_path = format!("{path}[{index}]");
        let weight = crate::rawparse::resolved(weight);
        let value = if weight.is_number() {
            weight.as_i64()
        } else {
            None
        };
        match value {
            None => {
                return Err(ConfigError::invalid(format!(
                    "{item_path}.weight: weight must be an integer"
                )));
            }
            Some(w) if w > MAX_CREDENTIAL_WEIGHT => {
                return Err(ConfigError::invalid(format!(
                    "{item_path}.weight: weight must not exceed {MAX_CREDENTIAL_WEIGHT}"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Weight validation on the raw document (`validateCredentialWeightYAML`). A document that fails
/// to parse is left for the real decoder to report.
pub(crate) fn validate_credential_weight_yaml(text: &str) -> Result<()> {
    let Ok(Some(document)) = parse_yaml(text) else {
        return Ok(());
    };
    let root = flatten_v8(&document)?;
    let Value::Mapping(map) = &root else {
        return Ok(());
    };
    for (name, value) in map {
        let Some(name) = name.as_str() else { continue };
        match name {
            "gemini-api-key"
            | "interactions-api-key"
            | "claude-api-key"
            | "vertex-api-key"
            | "codex-api-key"
            | "xai-api-key"
            | "meta-api-key" => validate_weight_sequence_node(value, name)?,
            "openai-compatibility" => {
                if let Value::Sequence(providers) = value {
                    for (index, provider) in providers.iter().enumerate() {
                        if let Some(entries) =
                            provider.as_mapping().and_then(|m| m.get("api-key-entries"))
                        {
                            validate_weight_sequence_node(
                                entries,
                                &format!("openai-compatibility[{index}].api-key-entries"),
                            )?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Layout normalisation / migration
// ---------------------------------------------------------------------------------------------

fn utf8(data: &[u8]) -> Result<&str> {
    std::str::from_utf8(data)
        .map_err(|e| ConfigError::invalid(format!("config is not valid UTF-8: {e}")))
}

/// Removes legacy fields that conflict with a present v8 field. With `migrate` it also moves
/// legacy-only fields to their v8 paths, comments out unknown fields and sets `config-version: 8`.
/// Returns the (possibly rewritten) document and whether it changed. Port of
/// `NormalizeConfigLayout`.
pub fn normalize_config_layout(data: &[u8], migrate: bool) -> Result<(Vec<u8>, bool)> {
    normalize_config_layout_inner(data, migrate, None)
}

/// [`normalize_config_layout`] for a document rendered with JSON-origin styling (see
/// [`crate::DocComments`]): a re-render keeps the flow style and quoting of those values.
pub fn normalize_config_layout_keeping_styles(
    data: &[u8],
    migrate: bool,
    marks: &crate::DocComments,
) -> Result<(Vec<u8>, bool)> {
    normalize_config_layout_inner(data, migrate, Some(marks))
}

fn normalize_config_layout_inner(
    data: &[u8],
    migrate: bool,
    marks: Option<&crate::DocComments>,
) -> Result<(Vec<u8>, bool)> {
    let text = utf8(data)?;
    let Some(mut root) = parse_yaml(text)? else {
        return if migrate {
            Err(ConfigError::invalid("empty config"))
        } else {
            Ok((data.to_vec(), false))
        };
    };
    flatten_v8(&root)?;
    let mut comments = Comments::extract(text);
    if let Some(marks) = marks {
        comments.styles.adopt_marks(&marks.comments().styles);
    }
    let mut changed = normalize_v8_private_ip_alias(&mut root, migrate)?;

    // Empty legacy structs have no leaf fields to move. Preserve them as empty v8 mappings; null
    // structs also mean defaults. User-owned maps are not included.
    let mut paths: Vec<(&str, String)> = aliases().map(|(o, c)| (*o, (*c).to_string())).collect();
    paths.extend(V8_PATHS.iter().map(|(o, c)| (*o, c.clone())));
    let structs = V8_STRUCT_PATHS
        .iter()
        .map(|(o, c)| (*o, c.as_str()))
        .chain(SHARED_STRUCT_PATHS.iter().copied());
    for (old, current) in structs {
        let Some(node) = yaml_path(&root, old) else {
            continue;
        };
        if !migrate && yaml_path(&root, current).is_none() {
            continue;
        }
        let current = current.to_string();
        let empty_struct = match node {
            Value::Null => true,
            Value::Mapping(m) => m.is_empty(),
            _ => false,
        };
        if !empty_struct {
            continue;
        }
        set_yaml_path(&mut root, old, empty_map());
        paths.push((old, current));
    }
    for (old, current) in &paths {
        let Some(value) = legacy_path(&root, old).cloned() else {
            continue;
        };
        let target_exists = yaml_path(&root, current).is_some();
        if !target_exists && !migrate {
            continue;
        }
        if target_exists {
            comments.remove_prefix(&dotted(old));
        } else {
            comments.move_field(&root, old, current);
        }
        delete_yaml_path(&mut root, old);
        if !target_exists {
            set_yaml_path(&mut root, current, value);
        }
        changed = true;
    }
    for (old, family) in KEY_FAMILIES {
        let Some(legacy) = yaml_path(&root, old).cloned() else {
            continue;
        };
        if migrate && !matches!(legacy, Value::Sequence(_) | Value::Null) {
            return Err(ConfigError::invalid(format!("{old} must be a list")));
        }
        let path = format!("api-keys.{family}");
        if yaml_path(&root, &path).is_none() {
            if !migrate {
                continue;
            }
            set_yaml_path(&mut root, &path, group_legacy_keys(&legacy, family));
            move_family_comments(&mut comments, old, &path, &legacy, family);
        } else {
            comments.remove_prefix(&dotted(old));
        }
        delete_yaml_path(&mut root, old);
        changed = true;
    }
    let mut footer = Vec::new();
    if migrate {
        // Existing unknown fields are ignored by the runtime. Retain their contents as comments
        // while keeping new v8 writes strictly validated.
        comment_unknown_v8_sections(&mut root, &mut footer)?;
        set_yaml_path(&mut root, "config-version", Value::Number(8.into()));
        changed = true;
    }
    if !changed {
        return Ok((data.to_vec(), false));
    }
    comments.foot.extend(footer);
    // The reference renders the normalized document with `yaml.Marshal` (4-space indent).
    comments.indent = 4;
    Ok((render_yaml(&root, &comments)?.into_bytes(), true))
}

/// Pre-write step of the management `WriteConfig`: a document in any v8 layout (including the
/// historical spellings) is re-normalized to the latest one; other documents pass through. A
/// document that does not parse is an error.
pub fn normalize_for_write(data: &[u8], marks: Option<&crate::DocComments>) -> Result<Vec<u8>> {
    let text = utf8(data)?;
    match parse_yaml(text)? {
        Some(root) if is_v8_config_layout(&root) => {
            Ok(normalize_config_layout_inner(data, true, marks)?.0)
        }
        _ => Ok(data.to_vec()),
    }
}

/// Carries the comments of legacy entries (`gemini-api-key[i]`) to the grouped layout: the
/// entry's own head goes to its group, shared fields to the group, the rest to the group's first
/// key. `openai-compatibility` entries become groups with `api-key-entries` renamed to `keys`.
pub(crate) fn move_family_comments(
    comments: &mut Comments,
    old: &str,
    new_path: &str,
    legacy: &Value,
    family: &str,
) {
    let Value::Sequence(entries) = legacy else {
        return;
    };
    for (index, entry) in entries.iter().enumerate() {
        let mut from_entry = dotted(old);
        from_entry.push(Seg::Index(index));
        let mut to_group = dotted(new_path);
        to_group.push(Seg::Index(index));
        if family == "openai-compatibility" {
            let mut entries_path = from_entry.clone();
            entries_path.push(Seg::Key("api-key-entries".to_string()));
            let mut keys_path = to_group.clone();
            keys_path.push(Seg::Key("keys".to_string()));
            comments.move_prefix(&entries_path, &keys_path);
            comments.move_prefix(&from_entry, &to_group);
            continue;
        }
        if let Value::Mapping(map) = entry {
            for field in map.keys().filter_map(Value::as_str) {
                let mut from = from_entry.clone();
                from.push(Seg::Key(field.to_string()));
                let mut to = to_group.clone();
                if field != "base-url" && !SHARED_KEY_FIELDS.contains(&field) {
                    to.push(Seg::Key("keys".to_string()));
                    to.push(Seg::Index(0));
                }
                to.push(Seg::Key(field.to_string()));
                comments.move_prefix(&from, &to);
            }
        }
        // What is left on the entry itself (the comment above `- api-key:`) belongs to the group.
        comments.move_prefix(&from_entry, &to_group);
    }
}

/// Inverse of [`move_family_comments`] for groups that are kept as written: gives each flattened
/// legacy entry the comments of the group/key it came from (`stash` holds the v8-path comments),
/// so list edits made on the legacy view re-key them correctly.
pub(crate) fn family_comments_to_legacy(
    comments: &mut Comments,
    stash: &Comments,
    old: &str,
    family: &str,
    groups: &Value,
) {
    let Value::Sequence(groups) = groups else {
        return;
    };
    let seg = |s: &str| Seg::Key(s.to_string());
    let mut next = 0usize;
    for (g, group) in groups.iter().enumerate() {
        let Value::Mapping(group_map) = group else {
            continue;
        };
        let group_path: CPath = vec![seg("api-keys"), seg(family), Seg::Index(g)];
        if family == "openai-compatibility" {
            let entry: CPath = vec![seg(old), Seg::Index(g)];
            comments.transplant(stash, &group_path, &entry, |rest| {
                rest.first() == Some(&seg("keys"))
            });
            let from_keys = [group_path.clone(), vec![seg("keys")]].concat();
            comments.transplant(
                stash,
                &from_keys,
                &[entry, vec![seg("api-key-entries")]].concat(),
                |_| false,
            );
            continue;
        }
        let Some(Value::Sequence(keys)) = group_map.get("keys") else {
            continue;
        };
        for (k, key) in keys.iter().enumerate() {
            let entry: CPath = vec![seg(old), Seg::Index(next)];
            next += 1;
            let key_path = [group_path.clone(), vec![seg("keys"), Seg::Index(k)]].concat();
            let key_map = key.as_mapping();
            if k == 0 {
                comments.transplant_exact(stash, &group_path, &entry);
                for field in group_map.keys().filter_map(Value::as_str) {
                    let shared = field == "base-url" || SHARED_KEY_FIELDS.contains(&field);
                    let overridden = key_map
                        .and_then(|m| m.get(field))
                        .is_some_and(|v| !v.is_null());
                    if shared && !overridden {
                        let from = [group_path.clone(), vec![seg(field)]].concat();
                        comments.transplant(
                            stash,
                            &from,
                            &[entry.clone(), vec![seg(field)]].concat(),
                            |_| false,
                        );
                    }
                }
            }
            comments.transplant_exact(stash, &key_path, &entry);
            for (field, value) in key_map.into_iter().flatten() {
                let Some(field) = field.as_str() else {
                    continue;
                };
                if !value.is_null() {
                    let from = [key_path.clone(), vec![seg(field)]].concat();
                    comments.transplant(
                        stash,
                        &from,
                        &[entry.clone(), vec![seg(field)]].concat(),
                        |_| false,
                    );
                }
            }
        }
    }
}

/// Comments out every field that is not part of the v8 schema and records the text for the
/// document footer.
fn comment_unknown_v8_sections(root: &mut Value, footer: &mut Vec<String>) -> Result<()> {
    let allowed_roots = v8_allowed_roots();
    comment_unknown_fields(root, &allowed_roots, "", footer)?;
    // Walk every container whose children we know.
    let Value::Mapping(map) = root else {
        return Ok(());
    };
    for (key, child) in map.iter_mut() {
        let Some(key) = key.as_str() else { continue };
        walk_unknown(child, key, footer)?;
    }
    Ok(())
}

fn walk_unknown(node: &mut Value, path: &str, footer: &mut Vec<String>) -> Result<()> {
    let Some(known) = V8_CHILDREN.get(path) else {
        return Ok(());
    };
    if !node.is_mapping() {
        return Ok(());
    }
    comment_unknown_fields(node, known, path, footer)?;
    let Value::Mapping(map) = node else {
        return Ok(());
    };
    for (key, child) in map.iter_mut() {
        let Some(key) = key.as_str() else { continue };
        walk_unknown(child, &format!("{path}.{key}"), footer)?;
    }
    Ok(())
}

fn comment_unknown_fields(
    node: &mut Value,
    allowed: &HashSet<String>,
    path: &str,
    footer: &mut Vec<String>,
) -> Result<()> {
    let Value::Mapping(map) = node else {
        return Ok(());
    };
    let unknown: Vec<Value> = map
        .keys()
        .filter(|k| k.as_str().is_none_or(|k| !allowed.contains(k)))
        .cloned()
        .collect();
    for key in unknown {
        let Some(value) = map.shift_remove(&key) else {
            continue;
        };
        let key_text = match &key {
            Value::String(s) => s.clone(),
            other => serde_yaml_ng::to_string(other)?.trim().to_string(),
        };
        let section = if path.is_empty() {
            key_text
        } else {
            format!("{path}.{key_text}")
        };
        warn_unrecognized_v8_section(&section);
        let mut entry = Mapping::new();
        entry.insert(str_key(&section), value);
        let text = crate::emit::emit(&Value::Mapping(entry), 4, &Default::default());
        let text = text.trim_end_matches('\n');
        footer.push(format!("# {}", text.replace('\n', "\n# ")));
    }
    Ok(())
}

/// Renders `root` the way `yaml.Marshal` would (4-space indent) while keeping the comments and
/// scalar quoting of `original`, the text it was edited from.
pub fn marshal_document(root: &Value, original: &str) -> Result<String> {
    let mut comments = Comments::extract(original);
    comments.indent = 4;
    render_yaml(root, &comments)
}

/// Serialises a document in yaml.v3 layout (see [`crate::emit`]) and re-attaches comments (see
/// [`Comments`]).
pub(crate) fn render_yaml(root: &Value, comments: &Comments) -> Result<String> {
    let indent = if comments.indent == 0 {
        2
    } else {
        comments.indent
    };
    let mut text = comments.apply(&crate::emit::emit(root, indent, &comments.styles));
    if !comments.foot.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if comments.foot.first().is_some_and(|l| !l.is_empty()) {
            text.push('\n');
        }
        text.push_str(&comments.foot.join("\n"));
        text.push('\n');
    }
    // Comments are re-inserted unindented, so no `normalize_comment_indentation` pass is needed
    // (and running one would corrupt `# ...` lines inside block scalars).
    Ok(text)
}

/// Accepts only the v8 layout (management writes): rejects legacy field names, unknown root
/// sections, unknown API-key providers and unknown fields. Port of `ValidateV8Config`.
pub fn validate_v8_config(data: &[u8]) -> Result<()> {
    let text = utf8(data)?;
    let Some(root) = parse_yaml(text)? else {
        return Err(ConfigError::invalid("empty config"));
    };
    let mut flat = flatten_v8(&root)?;
    let allowed_roots = v8_allowed_roots();
    let legacy_pairs = V8_PATHS
        .iter()
        .map(|(o, c)| (*o, c.as_str()))
        .chain(aliases().map(|(o, c)| (*o, *c)))
        .chain(SHARED_STRUCT_PATHS.iter().copied());
    for (old, current) in legacy_pairs {
        if legacy_path(&root, old).is_some() {
            return Err(ConfigError::invalid(format!(
                "legacy field {old} is not accepted by v8; use {current}"
            )));
        }
    }
    if let Value::Mapping(map) = &root {
        for key in map.keys() {
            let key = key.as_str().unwrap_or_default();
            if !allowed_roots.contains(key) {
                return Err(ConfigError::invalid(format!(
                    "unknown v8 configuration section {key}"
                )));
            }
        }
    }
    if let Some(Value::Mapping(groups)) = yaml_path(&root, "api-keys") {
        for key in groups.keys() {
            let key = key.as_str().unwrap_or_default();
            if !KEY_FAMILIES.iter().any(|(_, family)| *family == key) {
                return Err(ConfigError::invalid(format!(
                    "unknown API-key provider {key}"
                )));
            }
        }
    }
    delete_yaml_path(&mut flat, "config-version");
    // Empty struct containers are valid replacements. Only strip known structural paths; empty
    // user maps (headers, aliases, plugin options) carry real values.
    for (_, current) in V8_PATHS.iter() {
        let parts: Vec<&str> = current.split('.').collect();
        for end in (1..parts.len()).rev() {
            let container = parts[..end].join(".");
            if yaml_path(&flat, &container)
                .is_some_and(|v| v.as_mapping().is_some_and(Mapping::is_empty))
            {
                delete_yaml_path(&mut flat, &container);
            }
        }
    }
    strip_nulls(&mut flat);
    let mut ignored = Vec::new();
    let _: crate::Config = serde_ignored::deserialize(crate::lenient::Lenient(flat), |path| {
        ignored.push(path.to_string())
    })?;
    if let Some(field) = ignored.first() {
        return Err(ConfigError::invalid(format!(
            "field {field} not found in type config.legacyConfig"
        )));
    }
    Ok(())
}
