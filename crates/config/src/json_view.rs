//! JSON view of the config (what Go's `encoding/json` produces for `Config`).
//!
//! The YAML serialisation is not safe to expose over an API: Go marks the bind address, port,
//! management section (including the secret key), auth dir and TURN credentials `json:"-"`, and
//! a few types use different JSON spellings than YAML (snake_case thinking and plugin-store auth
//! fields, durations as integer nanoseconds, plugin instances reduced to their host fields).

use serde_json::{Map, Value as Json};

use crate::types::Config;

/// Top-level fields Go excludes from JSON.
const HIDDEN_ROOT_FIELDS: [&str; 4] = ["host", "port", "remote-management", "auth-dir"];

/// kebab-case YAML spelling -> Go json tag, for fields whose JSON name differs.
const THINKING_RENAMES: [(&str, &str); 2] = [
    ("zero-allowed", "zero_allowed"),
    ("dynamic-allowed", "dynamic_allowed"),
];
const STORE_AUTH_RENAMES: [(&str, &str); 6] = [
    ("apply-to", "apply_to"),
    ("token-env", "token_env"),
    ("username-env", "username_env"),
    ("password-env", "password_env"),
    ("header-name", "header_name"),
    ("header-value-env", "header_value_env"),
];

impl Config {
    /// The config as Go would marshal it to JSON: secrets and `json:"-"` fields omitted, durations
    /// as integer nanoseconds, snake_case where Go's json tags say so. Empty lists are `[]`
    /// (Go writes `null` for a nil slice, which a Rust `Vec` cannot distinguish).
    pub fn to_json_value(&self) -> serde_json::Result<Json> {
        let mut root = serde_json::to_value(self)?;
        let Some(map) = root.as_object_mut() else {
            return Ok(root);
        };
        for field in HIDDEN_ROOT_FIELDS {
            map.shift_remove(field);
        }
        if let Some(Json::Object(concurrency)) = map.get_mut("credential-concurrency") {
            let c = &self.credential_concurrency;
            for (key, nanos) in [
                ("cpa-heartbeat-timeout", c.cpa_heartbeat_timeout.0),
                ("cpa-cancel-bound", c.cpa_cancel_bound.0),
                ("reclaim-grace", c.reclaim_grace.0),
                ("cleanup-interval", c.cleanup_interval.0),
                ("release-flush-interval", c.release_flush_interval.0),
                ("release-max-backoff", c.release_max_backoff.0),
                ("busy-retry-min", c.busy_retry_min.0),
                ("busy-retry-max", c.busy_retry_max.0),
            ] {
                concurrency.insert(key.to_string(), nanos.into());
            }
        }
        if let Some(Json::Object(plugins)) = map.get_mut("plugins") {
            if let Some(Json::Array(auth)) = plugins.get_mut("store-auth") {
                for item in auth {
                    rename_keys(item, &STORE_AUTH_RENAMES);
                }
            }
            if let Some(Json::Object(configs)) = plugins.get_mut("configs") {
                for (id, instance) in &self.plugins.configs {
                    // Go keeps only the host-owned fields (`enabled` when set, `priority` unless 0).
                    let mut host = Map::new();
                    if let Some(enabled) = instance.enabled {
                        host.insert("enabled".into(), enabled.into());
                    }
                    if instance.priority != 0 {
                        host.insert("priority".into(), instance.priority.into());
                    }
                    configs.insert(id.clone(), Json::Object(host));
                }
            }
        }
        // encoding/json never omits a struct, even one tagged omitempty.
        if let Some(Json::Object(antigravity)) = map.get_mut("antigravity") {
            antigravity
                .entry("connection-pool")
                .or_insert_with(|| Json::Object(Map::new()));
        }
        if let Some(Json::Object(relay)) = map
            .get_mut("codex")
            .and_then(Json::as_object_mut)
            .and_then(|codex| codex.get_mut("live-media-relay"))
            && let Some(Json::Array(servers)) = relay.get_mut("ice-servers")
        {
            for server in servers.iter_mut().filter_map(Json::as_object_mut) {
                server.shift_remove("username");
                server.shift_remove("credential");
            }
        }
        // Free-form subtrees (payload params, plugin options) may use a `thinking` key of their own.
        for (key, child) in map.iter_mut() {
            if !matches!(key.as_str(), "payload" | "plugins") {
                rename_thinking(child);
            }
        }
        Ok(root)
    }
}

fn rename_keys(value: &mut Json, renames: &[(&str, &str)]) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    // Rebuild to keep field order.
    let entries = std::mem::take(map);
    for (key, v) in entries {
        let key = renames
            .iter()
            .find(|(from, _)| *from == key)
            .map_or(key, |(_, to)| (*to).to_string());
        map.insert(key, v);
    }
}

/// Renames the fields of every `thinking` object anywhere in the document.
fn rename_thinking(value: &mut Json) {
    match value {
        Json::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "thinking" {
                    rename_keys(child, &THINKING_RENAMES);
                } else {
                    rename_thinking(child);
                }
            }
        }
        Json::Array(items) => items.iter_mut().for_each(rename_thinking),
        _ => {}
    }
}
