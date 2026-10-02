//! Persisted cooldown state (Go: auth/cooldown_state.go).
//!
//! With `routing.cooldown.save-cooldown-status` the unexpired cooldown records of every
//! credential are saved independently of the auth files (one `<authfile>.cds` per credential) and
//! restored on startup. The manager decides *when* to save; this module owns the format.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use cpa_auth::types::{Auth, AuthError, ModelState, QuotaState, go_time};
use serde::{Deserialize, Serialize};

use super::cooldown::{cooldown_fields_of, cooldown_reason};
use super::util::after;

/// One auth/model cooldown snapshot (`model` empty = credential level).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CooldownStateRecord {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    pub auth_id: String,
    #[serde(skip)]
    pub auth_file: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, with = "go_time")]
    pub next_retry_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default)]
    pub quota: QuotaState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<AuthError>,
    #[serde(default, with = "go_time")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Persists runtime cooldown state independently from auth tokens.
pub trait CooldownStateStore: Send + Sync {
    fn load(&self) -> Result<Vec<CooldownStateRecord>, String>;
    fn save(&self, records: &[CooldownStateRecord]) -> Result<(), String>;
}

#[derive(Serialize, Deserialize)]
struct CooldownStateFile {
    version: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    auth_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    provider: String,
    #[serde(with = "go_time")]
    updated_at: Option<DateTime<Utc>>,
    records: Vec<CooldownStateRecord>,
}

/// One `.cds` file per credential under `dir`.
pub struct FileCooldownStateStore {
    dir: PathBuf,
    auth_dir: PathBuf,
    lock: parking_lot::Mutex<()>,
}

impl FileCooldownStateStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_auth_dir(dir, PathBuf::new())
    }

    /// `.cds` paths mirror auth file paths relative to `auth_dir` when possible.
    pub fn with_auth_dir(dir: impl Into<PathBuf>, auth_dir: impl Into<PathBuf>) -> Self {
        FileCooldownStateStore {
            dir: dir.into(),
            auth_dir: auth_dir.into(),
            lock: parking_lot::Mutex::new(()),
        }
    }

    fn state_relative_path(&self, record: &CooldownStateRecord) -> String {
        let auth_file = record.auth_file.trim();
        if !auth_file.is_empty() {
            let p = Path::new(auth_file);
            if p.is_absolute() && !self.auth_dir.as_os_str().is_empty() {
                if let Ok(rel) = p.strip_prefix(&self.auth_dir)
                    && rel.components().next().is_some()
                {
                    return cds_path_for_rel(rel);
                }
            }
            if !p.is_absolute() {
                return cds_path_for_rel(p);
            }
            return sanitize_file_name(
                &p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
        }
        sanitize_file_name(record.auth_id.trim())
    }

    fn state_path(&self, record: &CooldownStateRecord) -> Result<PathBuf, String> {
        let rel = self.state_relative_path(record);
        if rel.is_empty() {
            return Err("cooldown state path: missing auth identity".into());
        }
        Ok(self.dir.join(rel))
    }

    fn walk_cds(&self, visit: &mut dyn FnMut(&Path) -> Result<(), String>) -> Result<(), String> {
        fn rec(
            dir: &Path,
            visit: &mut dyn FnMut(&Path) -> Result<(), String>,
        ) -> Result<(), String> {
            let entries = match fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.to_string()),
            };
            for entry in entries {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                let ft = entry.file_type().map_err(|e| e.to_string())?;
                if ft.is_dir() {
                    rec(&path, visit)?;
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("cds"))
                {
                    visit(&path)?;
                }
            }
            Ok(())
        }
        rec(&self.dir, visit)
    }

    fn remove_stale(&self, desired: &HashMap<PathBuf, ()>) -> Result<(), String> {
        let mut doomed = Vec::new();
        self.walk_cds(&mut |p| {
            if !desired.contains_key(&clean(p)) {
                doomed.push(p.to_path_buf());
            }
            Ok(())
        })?;
        for p in doomed {
            if let Err(e) = fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("remove stale cooldown state {}: {e}", p.display()));
            }
        }
        Ok(())
    }
}

fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn cds_path_for_rel(rel: &Path) -> String {
    let clean = clean(rel);
    if clean.as_os_str().is_empty() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return String::new();
    }
    let base = sanitize_file_name(
        &clean
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    if base.is_empty() {
        return String::new();
    }
    match clean.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(base).to_string_lossy().into_owned(),
        _ => base,
    }
}

/// `[^A-Za-z0-9._-]+` -> `_`, trimmed of `._-`, extension replaced by `.cds`.
fn sanitize_file_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    let stem = match Path::new(name).extension() {
        Some(ext) => name[..name.len() - ext.len() - 1].to_string(),
        None => name.to_string(),
    };
    let mut out = String::with_capacity(stem.len());
    let mut last_underscore = false;
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let out = out.trim_matches(|c| matches!(c, '.' | '_' | '-'));
    if out.is_empty() {
        String::new()
    } else {
        format!("{out}.cds")
    }
}

impl CooldownStateStore for FileCooldownStateStore {
    fn load(&self) -> Result<Vec<CooldownStateRecord>, String> {
        if self.dir.as_os_str().is_empty() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        self.walk_cds(&mut |path| {
            let data = match fs::read(path) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("read cooldown state {}: {e}", path.display())),
            };
            if data.iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            let envelope: CooldownStateFile = serde_json::from_slice(&data)
                .map_err(|e| format!("parse cooldown state {}: {e}", path.display()))?;
            records.extend(envelope.records);
            Ok(())
        })
        .map_err(|e| format!("read cooldown state directory: {e}"))?;
        Ok(records)
    }

    fn save(&self, records: &[CooldownStateRecord]) -> Result<(), String> {
        if self.dir.as_os_str().is_empty() {
            return Ok(());
        }
        let _guard = self.lock.lock();
        let mut groups: BTreeMap<PathBuf, Vec<CooldownStateRecord>> = BTreeMap::new();
        for record in records {
            if record.auth_id.trim().is_empty() {
                continue;
            }
            let path = self.state_path(record)?;
            groups.entry(path).or_default().push(record.clone());
        }
        if groups.is_empty() {
            return self.remove_stale(&HashMap::new());
        }
        fs::create_dir_all(&self.dir)
            .map_err(|e| format!("create cooldown state directory: {e}"))?;
        let mut desired = HashMap::new();
        for (path, mut group) in groups {
            group.sort_by(|a, b| a.model.cmp(&b.model));
            let envelope = CooldownStateFile {
                version: 1,
                auth_id: group[0].auth_id.clone(),
                provider: group[0].provider.clone(),
                updated_at: Some(Utc::now()),
                records: group,
            };
            let mut data = serde_json::to_vec_pretty(&envelope)
                .map_err(|e| format!("marshal cooldown state: {e}"))?;
            data.push(b'\n');
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create cooldown state directory: {e}"))?;
            }
            let tmp = path.with_extension(format!("cds.{}.tmp", std::process::id()));
            {
                let mut f = fs::File::create(&tmp)
                    .map_err(|e| format!("create cooldown state temp file: {e}"))?;
                f.write_all(&data)
                    .map_err(|e| format!("write cooldown state temp file: {e}"))?;
            }
            fs::rename(&tmp, &path).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                format!("replace cooldown state file: {e}")
            })?;
            desired.insert(clean(&path), ());
        }
        self.remove_stale(&desired)
    }
}

pub fn cooldown_auth_file(auth: &Auth) -> String {
    let path = auth.attr("path");
    if !path.is_empty() {
        return path;
    }
    auth.file_name.trim().to_string()
}

fn auth_record(auth: &Auth, now: DateTime<Utc>) -> Option<CooldownStateRecord> {
    if !auth.unavailable || auth.next_retry_after.is_none() || !after(auth.next_retry_after, now) {
        return None;
    }
    Some(CooldownStateRecord {
        provider: auth.provider.trim().to_string(),
        auth_id: auth.id.clone(),
        auth_file: cooldown_auth_file(auth),
        model: String::new(),
        status: "cooling".into(),
        next_retry_after: auth.next_retry_after,
        reason: cooldown_reason(&auth.status_message, &auth.quota, auth.last_error.as_ref()),
        quota: cooldown_fields_of(&auth.quota),
        last_error: auth.last_error.clone(),
        updated_at: auth.updated_at,
    })
}

fn model_record(
    auth: &Auth,
    model: &str,
    state: &ModelState,
    now: DateTime<Utc>,
) -> Option<CooldownStateRecord> {
    let model = model.trim();
    if model.is_empty()
        || !state.unavailable
        || state.next_retry_after.is_none()
        || !after(state.next_retry_after, now)
    {
        return None;
    }
    Some(CooldownStateRecord {
        provider: auth.provider.trim().to_string(),
        auth_id: auth.id.clone(),
        auth_file: cooldown_auth_file(auth),
        model: model.to_string(),
        status: "cooling".into(),
        next_retry_after: state.next_retry_after,
        reason: cooldown_reason(
            &state.status_message,
            &state.quota,
            state.last_error.as_ref(),
        ),
        quota: cooldown_fields_of(&state.quota),
        last_error: state.last_error.clone(),
        updated_at: state.updated_at,
    })
}

/// Unexpired cooldown records of one credential, sorted by model (credential record first).
pub fn records_for_auth(auth: &Auth, now: DateTime<Utc>) -> Vec<CooldownStateRecord> {
    let mut records = Vec::new();
    if let Some(r) = auth_record(auth, now) {
        records.push(r);
    }
    for (model, state) in &auth.model_states {
        if let Some(r) = model_record(auth, model, state, now) {
            records.push(r);
        }
    }
    records.sort_by(|a, b| a.model.cmp(&b.model));
    records
}

/// Compares records ignoring observation data (Go: cooldownStateRecordsEqual).
pub fn records_equal(a: &[CooldownStateRecord], b: &[CooldownStateRecord]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.provider == y.provider
                && x.auth_id == y.auth_id
                && x.auth_file == y.auth_file
                && x.model == y.model
                && x.status == y.status
                && x.reason == y.reason
                && x.next_retry_after == y.next_retry_after
                && x.updated_at == y.updated_at
                && x.quota.exceeded == y.quota.exceeded
                && x.quota.reason == y.quota.reason
                && x.quota.backoff_level == y.quota.backoff_level
                && x.quota.next_recover_at == y.quota.next_recover_at
                && x.last_error == y.last_error
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap()
    }

    fn cooling_auth() -> Auth {
        let mut a = Auth::new("sub/claude-a@b.json", "claude");
        a.attributes
            .insert("path".into(), "sub/claude-a@b.json".into());
        a.model_states.insert(
            "m".into(),
            ModelState {
                unavailable: true,
                next_retry_after: Some(t(100)),
                quota: QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(t(100)),
                    backoff_level: 2,
                    ..Default::default()
                },
                last_error: Some(AuthError {
                    code: String::new(),
                    message: "slow".into(),
                    retryable: false,
                    http_status: 429,
                }),
                updated_at: Some(t(0)),
                ..Default::default()
            },
        );
        a
    }

    #[test]
    fn only_unexpired_records_are_emitted() {
        let a = cooling_auth();
        let recs = records_for_auth(&a, t(10));
        assert_eq!(recs.len(), 1);
        assert_eq!(
            (
                recs[0].model.as_str(),
                recs[0].reason.as_str(),
                recs[0].status.as_str()
            ),
            ("m", "quota", "cooling")
        );
        assert!(records_for_auth(&a, t(100) + Duration::seconds(1)).is_empty());
    }

    #[test]
    fn file_store_round_trips_one_cds_per_auth_and_removes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCooldownStateStore::with_auth_dir(dir.path().join("cds"), dir.path());
        let recs = records_for_auth(&cooling_auth(), t(10));
        store.save(&recs).unwrap();
        assert!(dir.path().join("cds/sub/claude-a_b.cds").exists());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].next_retry_after, Some(t(100)));
        assert_eq!(loaded[0].quota.backoff_level, 2);
        assert_eq!(loaded[0].last_error.as_ref().unwrap().http_status, 429);
        store.save(&[]).unwrap();
        assert!(store.load().unwrap().is_empty());
        assert!(!dir.path().join("cds/sub/claude-a_b.cds").exists());
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize_file_name("claude-a@b.c.json"), "claude-a_b.c.cds");
        assert_eq!(sanitize_file_name("__.json"), "");
    }
}
