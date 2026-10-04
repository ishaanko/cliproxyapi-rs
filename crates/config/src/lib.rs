//! Configuration for the CLI proxy: schema, defaults, validation, v8 <-> legacy layout handling,
//! YAML persistence, `.env` loading and hot reload.
//!
//! Ported from Go `internal/config` (and `sdk/config`), plus the config parts of
//! `internal/watcher` (`watcher`, `diff`).
//!
//! A [`Config`] always carries the legacy field names; documents may use the v8 layout, the
//! legacy layout or a mix (v8 wins by presence). See `docs/survey/conductor-config.md` section 15.

mod comments;
pub mod diff;
mod duration;
mod emit;
mod env;
mod error;
mod goerr;
mod json_view;
mod layout;
mod lenient;
mod load;
mod normalize;
mod paths;
mod rawparse;
mod save;
mod scope;
mod types;
mod v8_api;
mod validate;
pub mod watcher;
mod yamlpath;

pub use comments::normalize_comment_indentation;
pub use duration::{DurationParseError, GoDuration};
pub use env::load_dotenv;
pub use error::{ConfigError, Result};
pub use layout::marshal_document;
pub use layout::{
    MAX_CREDENTIAL_WEIGHT, WarnFn, is_v8_config_layout, normalize_config_layout, normalize_for_write,
    set_v8_migration_warn_func, v8_alias_paths, validate_v8_config,
};
pub use load::{
    load_config, load_config_optional, looks_like_bcrypt, normalize_home_port, parse_config_bytes,
};
pub use normalize::{
    CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI, CLAUDE_FINGERPRINT_PROFILE_DEFAULT,
    format_sorted_headers, normalize_claude_fingerprint_profile, normalize_cloak_config,
    normalize_excluded_models, normalize_headers, normalize_model_prefix,
    normalize_oauth_excluded_models, normalize_plugin_store_auth,
    validate_claude_fingerprint_profile,
};
pub use paths::{clean_path, resolve_auth_dir, resolve_plugins_dir};
pub use save::{save_config_preserve_comments, save_config_update_nested_scalar};
pub use types::*;
pub use v8_api::{
    DocComments, marshal_document_with_comments, normalize_v8_config_aliases,
    normalize_v8_config_aliases_with_comments, project_v8_config_aliases,
};
pub use validate::{
    DEFAULT_CODEX_LIVE_MEDIA_MAX_SESSIONS, validate_credential_weight, validate_trusted_proxies,
};
