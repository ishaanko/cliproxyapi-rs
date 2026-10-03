//! Plugin-owned auth files (Go: sdk/auth/filestore.go `PluginAuthParser` and
//! `synthesizer.PluginAuthParser`). The plugin host registers one process-wide parser; the file
//! store and the file synthesizer ask it before applying the built-in parsing.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::credmeta::{ATTRIBUTE_FILE_PRIORITY, Metadata, validate_auth_weight};
use crate::storage::TokenStorage;
use crate::types::Auth;

/// What the parser is asked to parse (Go: `pluginapi.AuthParseRequest`).
pub struct PluginParseRequest<'a> {
    pub provider: &'a str,
    pub path: &'a str,
    pub file_name: &'a str,
    pub raw_json: &'a [u8],
}

pub trait PluginAuthParser: Send + Sync {
    /// `Ok(Some(auths))` when a plugin handled the file (possibly with no auths), `Ok(None)` when
    /// no plugin claimed it. Blocking.
    fn parse_auths(&self, req: &PluginParseRequest<'_>) -> Result<Option<Vec<Auth>>, String>;
}

static PARSER: RwLock<Option<Arc<dyn PluginAuthParser>>> = RwLock::new(None);

/// Go `RegisterPluginAuthParser`.
pub fn set_plugin_auth_parser(parser: Option<Arc<dyn PluginAuthParser>>) {
    *PARSER.write() = parser;
}

pub fn current_plugin_auth_parser() -> Option<Arc<dyn PluginAuthParser>> {
    PARSER.read().clone()
}

/// Go `compactPluginAuths`: drops auths with an invalid weight.
pub fn compact_plugin_auths(auths: Vec<Auth>) -> Vec<Auth> {
    auths.into_iter().filter(|a| validate_auth_weight(a).is_ok()).collect()
}

/// Shared tail of the plugin branch: the file-priority metadata sync into a plugin storage.
pub fn sync_plugin_storage_metadata(auth: &mut Auth) {
    if auth.attributes.contains_key(ATTRIBUTE_FILE_PRIORITY)
        && let Some(TokenStorage::Plugin(storage)) = auth.storage.as_mut()
    {
        let meta: Metadata = auth.metadata.clone();
        storage.meta = meta;
    }
}
