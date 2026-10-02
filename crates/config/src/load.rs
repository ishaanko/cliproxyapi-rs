//! Loading pipeline (port of `config_load.go` and `parse.go`): read, flatten the v8 layout,
//! decode with the pre-set defaults, validate, clamp, hash the management secret and sanitise.

use std::path::Path;

use serde_yaml_ng::Value;

use crate::error::{ConfigError, Result};
use crate::layout::{
    flatten_v8, normalize_config_layout, oauth_only_fields, validate_credential_weight_yaml,
};
use crate::save::{save_config_update_nested_scalar, write_private};
use crate::types::*;
use crate::validate::validate_trusted_proxies;
use crate::yamlpath::{parse_yaml, strip_nulls, yaml_path};

/// bcrypt cost used for the management secret (Go's `bcrypt.DefaultCost`).
const BCRYPT_COST: u32 = 10;

/// Reads and validates the YAML config at `path` (the file must exist).
pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    load_config_optional(path, false)
}

/// Like [`load_config`]. With `optional`, a missing, empty or invalid file yields an empty config
/// (cloud deploy standby) instead of an error.
///
/// Side effects, as in the Go loader: a plaintext management secret is replaced by its bcrypt hash
/// in the file, and legacy fields that conflict with a present v8 field are removed from it.
pub fn load_config_optional(path: impl AsRef<Path>, optional: bool) -> Result<Config> {
    let path = path.as_ref();
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            let standby = matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            );
            if optional && standby {
                return Ok(Config::empty_optional());
            }
            return Err(ConfigError::io("failed to read config file", err));
        }
    };
    // In cloud deploy mode, an empty or whitespace-only file means an empty config.
    if optional && data.iter().all(u8::is_ascii_whitespace) {
        return Ok(Config::empty_optional());
    }
    let text = match String::from_utf8(data) {
        Ok(text) => text,
        Err(_) if optional => return Ok(Config::empty_optional()),
        Err(_) => {
            return Err(ConfigError::invalid(
                "failed to parse config file: config is not valid UTF-8",
            ));
        }
    };
    if let Err(err) = validate_credential_weight_yaml(&text) {
        return if optional {
            Ok(Config::empty_optional())
        } else {
            Err(err)
        };
    }
    let root = match parse_yaml(&text) {
        Ok(root) => root,
        Err(_) if optional => return Ok(Config::empty_optional()),
        Err(err) => {
            return Err(ConfigError::invalid(format!(
                "failed to parse config file: {err}"
            )));
        }
    };
    let mut cfg = match &root {
        Some(root) => match decode_config(root) {
            Ok(cfg) => cfg,
            Err(_) if optional => return Ok(Config::empty_optional()),
            Err(err) => {
                return Err(ConfigError::invalid(format!(
                    "failed to parse config file: {err}"
                )));
            }
        },
        None => Config::parse_defaults(),
    };
    finalize(&mut cfg, true, |hashed| {
        // Persist the hash so the secret is not re-hashed on every start. Comments and ordering
        // are preserved; only the nested key changes.
        let prefix = match &root {
            Some(root) if yaml_path(root, "management.secret-key").is_some() => "management",
            _ => "remote-management",
        };
        let _ = save_config_update_nested_scalar(path, &[prefix, "secret-key"], hashed);
    })?;

    // Only conflicting legacy fields are removed on load. A legacy-only document stays legacy
    // until a v8 configuration write explicitly migrates it.
    let current = std::fs::read(path).map_err(|e| ConfigError::io("read config file", e))?;
    let (cleaned, changed) = normalize_config_layout(&current, false)?;
    if changed {
        write_private(path, &cleaned)
            .map_err(|e| ConfigError::io("clean conflicting config fields", e))?;
    }
    Ok(cfg)
}

/// Parses a YAML configuration payload and applies the same in-memory normalisation as
/// [`load_config`], without touching the filesystem.
pub fn parse_config_bytes(data: &[u8]) -> Result<Config> {
    if data.is_empty() {
        return Err(ConfigError::invalid("config payload is empty"));
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| ConfigError::invalid("parse config payload: config is not valid UTF-8"))?;
    validate_credential_weight_yaml(text)?;
    let root =
        parse_yaml(text).map_err(|e| ConfigError::invalid(format!("parse config payload: {e}")))?;
    let mut cfg = match &root {
        Some(root) => decode_config(root)
            .map_err(|e| ConfigError::invalid(format!("parse config payload: {e}")))?,
        None => Config::parse_defaults(),
    };
    finalize(&mut cfg, false, |_| {})?;
    Ok(cfg)
}

/// Decodes a parsed document of any layout into a [`Config`] (the port of `Config.UnmarshalYAML`):
/// the document is flattened to legacy names with v8-wins-by-presence, then decoded over the
/// pre-set defaults. Also records which fields were set under `oauth.providers.*`.
pub(crate) fn decode_config(root: &Value) -> Result<Config> {
    let mut flat = flatten_v8(root)?;
    strip_nulls(&mut flat);
    let raw_plugin_configs = yaml_path(&flat, "plugins.configs").cloned();
    let mut cfg: Config = crate::lenient::from_value(flat)?;
    // Plugin options are opaque and written back as is, including the written form of scalars
    // (`1.10`, `True`) that the typed decode resolves.
    if let Some(Value::Mapping(raw)) = raw_plugin_configs {
        for (id, instance) in &mut cfg.plugins.configs {
            if let Some(tree) = raw.get(id.as_str()).filter(|v| !v.is_null()) {
                instance.raw = tree.clone();
            }
        }
    }
    cfg.oauth_only_fields = oauth_only_fields(root).into_iter().collect();
    Ok(cfg)
}

/// Post-decode steps shared by `load_config` and `parse_config_bytes`: defaults, validation,
/// secret hashing, clamps and sanitisers, in the same order as Go (`file_load` selects the file
/// loader's order). `persist_secret` receives the
/// bcrypt hash when a plaintext management secret was hashed.
fn finalize(cfg: &mut Config, file_load: bool, persist_secret: impl FnOnce(&str)) -> Result<()> {
    validate_trusted_proxies(&cfg.trusted_proxies)?;

    cfg.credential_concurrency = std::mem::take(&mut cfg.credential_concurrency).with_defaults();
    cfg.credential_in_flight.validate()?;
    if cfg.discovery.service_type.is_empty() {
        cfg.discovery.service_type = DEFAULT_DISCOVERY_SERVICE_TYPE.to_string();
    }
    if cfg.discovery.subtypes.is_empty() {
        cfg.discovery.subtypes = DEFAULT_DISCOVERY_SUBTYPES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    // Go validates in a different order on the two paths (only visible when both are invalid).
    if file_load {
        cfg.codex.live_media_relay.validate()?;
        cfg.validate_credential_weights()?;
    } else {
        cfg.validate_credential_weights()?;
        cfg.codex.live_media_relay.validate()?;
    }

    // Hash the management key if plaintext is detected (a bcrypt hash has a $2a$/$2b$/$2y$ prefix).
    if !cfg.remote_management.secret_key.is_empty()
        && !looks_like_bcrypt(&cfg.remote_management.secret_key)
    {
        let hashed = bcrypt::non_truncating_hash(&cfg.remote_management.secret_key, BCRYPT_COST)
            .map_err(|e| {
                ConfigError::invalid(format!("failed to hash remote management key: {e}"))
            })?;
        cfg.remote_management.secret_key = hashed;
        persist_secret(&cfg.remote_management.secret_key);
    }

    let rm = &mut cfg.remote_management;
    rm.panel_github_repository = rm.panel_github_repository.trim().to_string();
    if rm.panel_github_repository.is_empty() {
        rm.panel_github_repository = DEFAULT_PANEL_GITHUB_REPOSITORY.to_string();
    }
    cfg.pprof.addr = cfg.pprof.addr.trim().to_string();
    if cfg.pprof.addr.is_empty() {
        cfg.pprof.addr = DEFAULT_PPROF_ADDR.to_string();
    }
    if cfg.logs_max_total_size_mb < 0 {
        cfg.logs_max_total_size_mb = 0;
    }
    if cfg.error_logs_max_files < 0 {
        cfg.error_logs_max_files = 10;
    }
    if cfg.redis_usage_queue_retention_seconds <= 0 {
        cfg.redis_usage_queue_retention_seconds = 60;
    } else if cfg.redis_usage_queue_retention_seconds > 3600 {
        tracing::warn!(
            value = cfg.redis_usage_queue_retention_seconds,
            "redis-usage-queue-retention-seconds too large; clamping to 3600"
        );
        cfg.redis_usage_queue_retention_seconds = 3600;
    }
    if cfg.max_retry_credentials < 0 {
        cfg.max_retry_credentials = 0;
    }

    cfg.normalize_plugins_config();
    if let Err(err) = cfg.resolve_plugins_dir()
        && cfg.plugins.enabled
    {
        return Err(err);
    }

    cfg.sanitize_gemini_keys();
    cfg.sanitize_interactions_keys();
    cfg.sanitize_vertex_compat_keys();
    cfg.sanitize_codex_keys();
    cfg.sanitize_xai_keys();
    cfg.sanitize_meta_keys();
    cfg.sanitize_codex_header_defaults();
    cfg.sanitize_claude_header_defaults();
    cfg.sanitize_claude_keys();
    cfg.sanitize_openai_compatibility();
    cfg.oauth_excluded_models =
        crate::normalize::normalize_oauth_excluded_models(&cfg.oauth_excluded_models);
    cfg.sanitize_oauth_model_alias();
    cfg.sanitize_oauth_settings();
    cfg.sanitize_oauth_request_scoped_errors();
    cfg.sanitize_payload_rules();
    Ok(())
}

/// A bcrypt hash starts with `$2a$`, `$2b$` or `$2y$`.
pub fn looks_like_bcrypt(s: &str) -> bool {
    s.len() > 4 && matches!(s.as_bytes().get(..4), Some(b"$2a$" | b"$2b$" | b"$2y$"))
}

/// Port range the server falls back to when Home sends an unset or invalid port.
pub fn normalize_home_port(port: i64) -> i64 {
    if port <= 0 { DEFAULT_PORT } else { port }
}
