//! Provider token files (`*TokenStorage` structs of internal/auth/*). Each struct is the exact JSON
//! shape the Go app writes, so existing auth dirs keep working in both directions. Saving merges the
//! auth's free-form metadata at the top level (`misc.MergeMetadata`: metadata wins) and encodes with
//! Go's map semantics (sorted keys, trailing newline).

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credmeta::Metadata;
use crate::kimi::{
    KIMI_AI_DOMAIN, KIMI_DEFAULT_DOMAIN, is_kimi_ai_domain, resolve_kimi_api_base_url,
};
use crate::util::{encode_compact, encode_pretty};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{0}")]
    Invalid(String),
    #[error("failed to write token file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to encode token file: {0}")]
    Encode(#[from] serde_json::Error),
}

/// A provider credential struct that can write its own file. Closed set mirroring Go's
/// `TokenStorage` implementers.
#[derive(Debug, Clone)]
pub enum TokenStorage {
    Claude(ClaudeTokenStorage),
    Codex(CodexTokenStorage),
    Xai(XaiTokenStorage),
    Kimi(KimiTokenStorage),
    Vertex(VertexCredentialStorage),
    Meta(MetaTokenStorage),
    /// `pluginTokenStorage`: provider-owned JSON handed over by a plugin auth provider.
    Plugin(PluginTokenStorage),
    /// `EmptyStorage`: persists nothing.
    Empty,
}

impl TokenStorage {
    /// `SetMetadata` + `SaveTokenToFile`: writes the credential file with `metadata` merged at the
    /// top level. Creates the parent directory with mode 0700.
    pub fn save_to_file(&self, path: &Path, metadata: &Metadata) -> Result<(), StorageError> {
        match self {
            TokenStorage::Empty => Ok(()),
            TokenStorage::Claude(s) => write_encoded(path, &s.render(metadata)?, Layout::Compact),
            TokenStorage::Codex(s) => write_encoded(path, &s.render(metadata)?, Layout::Compact),
            TokenStorage::Xai(s) => write_encoded(path, &s.render(metadata)?, Layout::Pretty),
            TokenStorage::Kimi(s) => write_encoded(path, &s.render(metadata)?, Layout::Pretty),
            TokenStorage::Vertex(s) => write_encoded(path, &s.render(metadata)?, Layout::Pretty),
            TokenStorage::Meta(s) => s.save(path, metadata),
            TokenStorage::Plugin(s) => s.save(path, metadata),
        }
    }

    /// Provider `type` value this storage writes.
    pub fn type_name(&self) -> &str {
        match self {
            TokenStorage::Claude(_) => "claude",
            TokenStorage::Codex(_) => "codex",
            TokenStorage::Xai(_) => "xai",
            TokenStorage::Kimi(s) => s.effective_type(),
            TokenStorage::Vertex(_) => "vertex",
            TokenStorage::Meta(_) => "meta",
            TokenStorage::Plugin(s) => s.provider.as_str(),
            TokenStorage::Empty => "empty",
        }
    }
}

enum Layout {
    Compact,
    Pretty,
}

/// `misc.MergeMetadata`: struct JSON as a map, then metadata keys overwrite.
fn merge_metadata<T: Serialize>(source: &T, metadata: &Metadata) -> Result<Value, StorageError> {
    let mut data = match serde_json::to_value(source)? {
        Value::Object(m) => m,
        _ => {
            return Err(StorageError::Invalid(
                "token storage did not serialize to an object".into(),
            ));
        }
    };
    for (k, v) in metadata {
        data.insert(k.clone(), v.clone());
    }
    Ok(Value::Object(data))
}

fn write_encoded(path: &Path, data: &Value, layout: Layout) -> Result<(), StorageError> {
    let text = match layout {
        Layout::Compact => encode_compact(data)?,
        Layout::Pretty => encode_pretty(data)?,
    };
    write_file_in_place(path, text.as_bytes())
}

/// `os.MkdirAll(dir, 0700)` + `os.Create(path)`: truncating in-place write, new files get 0600.
pub(crate) fn write_file_in_place(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let io_err = |source| StorageError::Io {
        path: path.display().to_string(),
        source,
    };
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        mkdir_all_private(dir).map_err(io_err)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(io_err)?;
    f.write_all(bytes).map_err(io_err)
}

pub(crate) fn mkdir_all_private(dir: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

// ---- Claude ----

/// `ClaudeTokenStorage` (internal/auth/claude/token.go).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeTokenStorage {
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub account_uuid: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub organization_uuid: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub organization_name: String,
    #[serde(
        default,
        rename = "claude_device_ids",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub device_ids: Vec<String>,
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default, rename = "expired")]
    pub expire: String,
}

impl ClaudeTokenStorage {
    fn render(&self, metadata: &Metadata) -> Result<Value, StorageError> {
        let mut s = self.clone();
        s.type_ = "claude".into();
        merge_metadata(&s, metadata)
    }
}

// ---- Codex ----

/// `CodexTokenStorage` (internal/auth/codex/token.go).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTokenStorage {
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default, rename = "expired")]
    pub expire: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub plan_type: String,
}

impl CodexTokenStorage {
    fn render(&self, metadata: &Metadata) -> Result<Value, StorageError> {
        let mut s = self.clone();
        s.type_ = "codex".into();
        merge_metadata(&s, metadata)
    }
}

// ---- xAI ----

/// xAI `TokenStorage` (internal/auth/xai/token.go).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XaiTokenStorage {
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub id_token: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub expires_in: i64,
    #[serde(default, rename = "expired", skip_serializing_if = "is_empty_str")]
    pub expire: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub last_refresh: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub email: String,
    #[serde(default, rename = "sub", skip_serializing_if = "is_empty_str")]
    pub subject: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub redirect_uri: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub auth_kind: String,
}

impl XaiTokenStorage {
    fn render(&self, metadata: &Metadata) -> Result<Value, StorageError> {
        let mut s = self.clone();
        s.type_ = "xai".into();
        s.auth_kind = "oauth".into();
        merge_metadata(&s, metadata)
    }
}

// ---- Kimi ----

/// `KimiTokenStorage` (internal/auth/kimi/token.go).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KimiTokenStorage {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub scope: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub device_id: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub expired: String,
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub domain: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub base_url: String,
}

impl KimiTokenStorage {
    /// `type` as it will be written: explicit, else derived from the domain.
    fn effective_type(&self) -> &str {
        if !self.type_.is_empty() {
            &self.type_
        } else if is_kimi_ai_domain(&self.domain) {
            "kimi-ai"
        } else {
            "kimi"
        }
    }

    fn render(&self, metadata: &Metadata) -> Result<Value, StorageError> {
        let mut s = self.clone();
        s.type_ = s.effective_type().to_string();
        if s.domain.is_empty() {
            s.domain = if is_kimi_ai_domain(&s.type_) {
                KIMI_AI_DOMAIN
            } else {
                KIMI_DEFAULT_DOMAIN
            }
            .to_string();
        }
        if s.base_url.is_empty() {
            s.base_url = resolve_kimi_api_base_url(&s.domain).to_string();
        }
        merge_metadata(&s, metadata)
    }

    /// `IsExpired()`: unparseable expiry counts as expired; 300 s refresh threshold.
    pub fn is_expired(&self) -> bool {
        if self.expired.is_empty() {
            return false;
        }
        match chrono::DateTime::parse_from_rfc3339(&self.expired) {
            Ok(t) => crate::util::now_plus_secs(300) > t,
            Err(_) => true,
        }
    }

    pub fn needs_refresh(&self) -> bool {
        !self.refresh_token.is_empty() && self.is_expired()
    }
}

// ---- Vertex ----

/// `VertexCredentialStorage` (internal/auth/vertex/vertex_credentials.go).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VertexCredentialStorage {
    #[serde(default)]
    pub service_account: Metadata,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub location: String,
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub prefix: String,
}

impl VertexCredentialStorage {
    fn render(&self, metadata: &Metadata) -> Result<Value, StorageError> {
        if self.service_account.is_empty() {
            return Err(StorageError::Invalid(
                "vertex credential: service account content is empty".into(),
            ));
        }
        let mut s = self.clone();
        s.type_ = "vertex".into();
        merge_metadata(&s, metadata)
    }
}

// ---- Meta ----

/// `MetaTokenStorage` (internal/auth/meta/meta.go). Unlike the others it builds the file by hand and
/// writes through a temp file + rename.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MetaTokenStorage {
    pub access_token: String,
    pub dca_token: String,
    pub api_key: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expired: String,
    pub dca_expired: String,
    pub dca_expires_at: i64,
    pub last_refresh: String,
    pub base_url: String,
    pub email: String,
    pub name: String,
}

/// Keys the Meta storage owns; free-form metadata never overrides them.
const META_CREDENTIAL_FIELDS: [&str; 11] = [
    "type",
    "auth_kind",
    "access_token",
    "token_type",
    "dca_token",
    "api_key",
    "expires_in",
    "expired",
    "dca_expired",
    "dca_expires_at",
    "last_refresh",
];

fn put_nonempty(data: &mut serde_json::Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        data.insert(key.into(), value.into());
    }
}

impl MetaTokenStorage {
    /// File body. `metadata` extras are merged after the credential fields (credential keys skipped).
    fn render(&self, metadata: &Metadata) -> Value {
        let mut data = serde_json::Map::new();
        data.insert("type".into(), "meta".into());
        data.insert("auth_kind".into(), "oauth".into());
        data.insert("access_token".into(), self.access_token.clone().into());
        put_nonempty(&mut data, "dca_token", &self.dca_token);
        put_nonempty(&mut data, "api_key", &self.api_key);
        put_nonempty(&mut data, "token_type", &self.token_type);
        if self.expires_in > 0 {
            data.insert("expires_in".into(), self.expires_in.into());
        }
        put_nonempty(&mut data, "expired", &self.expired);
        put_nonempty(&mut data, "dca_expired", &self.dca_expired);
        if self.dca_expires_at > 0 {
            data.insert("dca_expires_at".into(), self.dca_expires_at.into());
        }
        put_nonempty(&mut data, "last_refresh", &self.last_refresh);
        put_nonempty(&mut data, "base_url", &self.base_url);
        put_nonempty(&mut data, "email", &self.email);
        put_nonempty(&mut data, "name", &self.name);
        for (k, v) in metadata {
            if META_CREDENTIAL_FIELDS.contains(&k.as_str()) {
                continue;
            }
            data.insert(k.clone(), v.clone());
        }
        Value::Object(data)
    }

    fn save(&self, path: &Path, metadata: &Metadata) -> Result<(), StorageError> {
        let io_err = |source| StorageError::Io {
            path: path.display().to_string(),
            source,
        };
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        mkdir_all_private(dir).map_err(io_err)?;
        let text = encode_pretty(&self.render(metadata))?;
        let tmp = dir.join(format!(".meta-token-{}", crate::util::random_hex(8)));
        let result = (|| {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(io_err)
    }
}

// ---- Plugin ----

/// Storage of a plugin-provided auth (Go: `pluginTokenStorage`): the plugin's raw JSON merged
/// with the auth metadata and stamped with the provider `type`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginTokenStorage {
    pub provider: String,
    pub raw_json: Vec<u8>,
    /// Metadata the storage was created with (used to notice a dropped `priority`).
    pub meta: Metadata,
}

impl PluginTokenStorage {
    /// `RawJSON()`: the storage payload merged with its own metadata.
    pub fn raw_json_payload(&self) -> Option<Vec<u8>> {
        self.merged(&self.meta, &self.raw_json).ok()
    }

    /// `mergedStorageJSON`: raw JSON, then metadata keys, then `type`, normalized and compact.
    fn merged(&self, metadata: &Metadata, raw: &[u8]) -> Result<Vec<u8>, StorageError> {
        let mut out = Metadata::new();
        if !raw.iter().all(u8::is_ascii_whitespace) {
            match serde_json::from_slice::<Value>(raw) {
                Ok(Value::Object(m)) => out = m,
                Ok(Value::Null) => {}
                Ok(_) | Err(_) => {
                    return Err(StorageError::Invalid(
                        "decode plugin token storage: invalid JSON object".into(),
                    ));
                }
            }
        }
        for (k, v) in metadata {
            out.insert(k.clone(), v.clone());
        }
        let provider = self.provider.trim().to_lowercase();
        if !provider.is_empty() {
            out.insert("type".into(), Value::String(provider));
        }
        crate::credmeta::normalize_credential_metadata(&mut out);
        if out.is_empty() {
            return Err(StorageError::Invalid(
                "plugin token storage payload is empty".into(),
            ));
        }
        Ok(crate::util::marshal_compact(&Value::Object(out))?.into_bytes())
    }

    /// `SetMetadata` + `SaveTokenToFile`: skips the write when the file already holds the same
    /// JSON, otherwise writes through a temp file and rename.
    fn save(&self, path: &Path, metadata: &Metadata) -> Result<(), StorageError> {
        let mut raw = self.raw_json.clone();
        if self.meta.contains_key("priority")
            && !metadata.contains_key("priority")
            && let Ok(Value::Object(mut m)) = serde_json::from_slice::<Value>(&raw)
        {
            m.shift_remove("priority");
            if let Ok(cleaned) = serde_json::to_vec(&Value::Object(m)) {
                raw = cleaned;
            }
        }
        let payload = self.merged(metadata, &raw)?;
        if let Ok(current) = fs::read(path)
            && let (Ok(a), Ok(b)) = (
                serde_json::from_slice::<Value>(&current),
                serde_json::from_slice::<Value>(&payload),
            )
            && a == b
        {
            return Ok(());
        }
        let io_err = |source| StorageError::Io {
            path: path.display().to_string(),
            source,
        };
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        mkdir_all_private(dir).map_err(io_err)?;
        let tmp = dir.join(format!(".plugin-auth-{}.tmp", crate::util::random_hex(8)));
        let result = (|| {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&payload)?;
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(io_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: Value) -> Metadata {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn claude_file_matches_go_layout() {
        let s = ClaudeTokenStorage {
            access_token: "sk-ant-oat01-a".into(),
            refresh_token: "sk-ant-ort01-b".into(),
            last_refresh: "2026-10-01T12:00:00Z".into(),
            email: "u@x.com".into(),
            device_ids: vec!["ab".repeat(32)],
            expire: "2026-10-01T20:00:00Z".into(),
            ..Default::default()
        };
        let extra =
            meta(json!({"email": "u@x.com", "proxy_url": "http://p", "organization_uuid": "org"}));
        let body = encode_compact(&s.render(&extra).unwrap()).unwrap();
        // Sorted keys, empty id_token present, empty account_uuid omitted, metadata merged, newline.
        let expected = format!(
            "{{\"access_token\":\"sk-ant-oat01-a\",\"claude_device_ids\":[\"{}\"],\"email\":\"u@x.com\",\"expired\":\"2026-10-01T20:00:00Z\",\"id_token\":\"\",\"last_refresh\":\"2026-10-01T12:00:00Z\",\"organization_uuid\":\"org\",\"proxy_url\":\"http://p\",\"refresh_token\":\"sk-ant-ort01-b\",\"type\":\"claude\"}}\n",
            "ab".repeat(32)
        );
        assert_eq!(body, expected);
    }

    #[test]
    fn go_written_claude_file_deserializes() {
        let go = r#"{"access_token":"a","claude_device_ids":["x"],"email":"e","expired":"t","id_token":"","last_refresh":"l","refresh_token":"r","type":"claude","cloak_mode":"always"}"#;
        let s: ClaudeTokenStorage = serde_json::from_str(go).unwrap();
        assert_eq!(s.access_token, "a");
        assert_eq!(s.device_ids, vec!["x".to_string()]);
        assert_eq!(s.expire, "t");
    }

    #[test]
    fn codex_plan_type_omitted_when_empty_and_metadata_wins() {
        let s = CodexTokenStorage {
            email: "e".into(),
            plan_type: "".into(),
            ..Default::default()
        };
        let v = s
            .render(&meta(json!({"email": "override", "websockets": true})))
            .unwrap();
        assert!(v.get("plan_type").is_none());
        assert_eq!(v["email"], "override");
        assert_eq!(v["type"], "codex");
        assert_eq!(v["websockets"], true);
    }

    #[test]
    fn kimi_fills_type_domain_and_base_url() {
        let s = KimiTokenStorage {
            access_token: "t".into(),
            type_: "kimi-ai".into(),
            ..Default::default()
        };
        let v = s.render(&Metadata::new()).unwrap();
        assert_eq!(v["domain"], "kimi.ai");
        assert_eq!(v["base_url"], "https://api.kimi.ai/coding");
        let s = KimiTokenStorage::default();
        let v = s.render(&Metadata::new()).unwrap();
        assert_eq!(
            (v["type"].as_str(), v["domain"].as_str()),
            (Some("kimi"), Some("kimi.com"))
        );
    }

    #[test]
    fn meta_storage_protects_credential_fields() {
        let s = MetaTokenStorage {
            access_token: "key".into(),
            email: "e@x".into(),
            ..Default::default()
        };
        let v = s.render(&meta(
            json!({"access_token": "evil", "subs_tier_name": "pro"}),
        ));
        assert_eq!(v["access_token"], "key");
        assert_eq!(v["subs_tier_name"], "pro");
        assert_eq!(v["auth_kind"], "oauth");
        assert!(v.get("dca_token").is_none());
    }

    #[test]
    fn save_writes_file_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/claude-x.json");
        let st = TokenStorage::Claude(ClaudeTokenStorage {
            email: "e".into(),
            ..Default::default()
        });
        st.save_to_file(&path, &Metadata::new()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n") && text.contains("\"type\":\"claude\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
