//! PostgreSQL-backed runtime cooldown state (Go: internal/store/postgres_cooldown_store.go).
//!
//! Rows are keyed by (auth_id, model). Saves merge by `updated_at`: an upsert only overwrites an
//! older-or-equal row, and records that disappeared since the last load/save are soft-deleted
//! (`deleted = TRUE`) only if nobody has updated them since, so concurrent instances do not
//! resurrect or clobber each other's state.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use cpa_runtime::conductor::{CooldownStateRecord, CooldownStateStore};
use parking_lot::Mutex;

use crate::pgconn::ErrText;
use crate::postgres::Shared;
use crate::rt;

type Key = (String, String);

pub(crate) struct PostgresCooldownStore {
    shared: Arc<Shared>,
    /// Versions seen by the last `load`/`save`; saving diffs against them.
    previous: Mutex<HashMap<Key, DateTime<Utc>>>,
    /// Serializes load/save like the Go store mutex.
    mu: Mutex<()>,
}

impl PostgresCooldownStore {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared, previous: Mutex::new(HashMap::new()), mu: Mutex::new(()) }
    }
}

fn record_key(record: &CooldownStateRecord) -> Key {
    (record.auth_id.trim().to_string(), record.model.trim().to_string())
}

/// `normalizePostgresCooldownTime`: zero falls back, then UTC truncated to microseconds.
fn normalize_time(value: Option<DateTime<Utc>>, fallback: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let v = value.or(fallback).unwrap_or_default();
    v.duration_trunc(TimeDelta::microseconds(1)).unwrap_or(v)
}

impl CooldownStateStore for PostgresCooldownStore {
    fn load(&self) -> Result<Vec<CooldownStateRecord>, String> {
        let _guard = self.mu.lock();
        let table = self.shared.full_table_name(&self.shared.cfg.cooldown_table);
        let sql = format!("SELECT content::text, updated_at FROM {table} WHERE deleted = FALSE");
        let shared = self.shared.clone();
        let rows: Vec<(String, DateTime<Utc>)> = rt::block_on(async move {
            let mut db = shared.db.lock().await;
            let client = db.client().await?;
            let rows = client.query(sql.as_str(), &[]).await?;
            Ok::<_, tokio_postgres::Error>(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
        })
        .map_err(|e| format!("postgres cooldown store: load state: {}", e.err_text()))?;

        let mut records = Vec::with_capacity(rows.len());
        let mut previous = HashMap::with_capacity(rows.len());
        for (content, updated_at) in rows {
            let record: CooldownStateRecord = serde_json::from_str(&content)
                .map_err(|e| format!("postgres cooldown store: decode state: {e}"))?;
            let key = record_key(&record);
            if key.0.is_empty() {
                return Err("postgres cooldown store: decoded state has empty auth ID".into());
            }
            previous.insert(key, updated_at);
            records.push(record);
        }
        *self.previous.lock() = previous;
        Ok(records)
    }

    fn save(&self, records: &[CooldownStateRecord]) -> Result<(), String> {
        let now = normalize_time(Some(Utc::now()), None);
        let mut current: HashMap<Key, DateTime<Utc>> = HashMap::with_capacity(records.len());
        let mut encoded: Vec<(Key, serde_json::Value, DateTime<Utc>)> = Vec::with_capacity(records.len());
        for record in records {
            let key = record_key(record);
            if key.0.is_empty() {
                return Err("postgres cooldown store: state has empty auth ID".into());
            }
            let mut record = record.clone();
            let updated_at = normalize_time(record.updated_at, Some(now));
            record.updated_at = Some(updated_at);
            let content = serde_json::to_value(&record)
                .map_err(|e| format!("postgres cooldown store: encode state for {:?}: {e}", key.0))?;
            current.insert(key.clone(), updated_at);
            encoded.push((key, content, updated_at));
        }

        let _guard = self.mu.lock();
        let previous = self.previous.lock().clone();
        let table = self.shared.full_table_name(&self.shared.cfg.cooldown_table);
        let upsert = format!(
            "INSERT INTO {table} AS target (auth_id, model, content, deleted, created_at, updated_at) \
             VALUES ($1, $2, $3, FALSE, NOW(), $4) \
             ON CONFLICT (auth_id, model) DO UPDATE SET \
             content = EXCLUDED.content, deleted = FALSE, updated_at = EXCLUDED.updated_at \
             WHERE target.updated_at <= EXCLUDED.updated_at"
        );
        let delete = format!(
            "INSERT INTO {table} AS target (auth_id, model, content, deleted, created_at, updated_at) \
             VALUES ($1, $2, $3, TRUE, NOW(), $4) \
             ON CONFLICT (auth_id, model) DO UPDATE SET \
             content = EXCLUDED.content, deleted = TRUE, updated_at = EXCLUDED.updated_at \
             WHERE NOT target.deleted AND target.updated_at <= $5"
        );
        let mut clears: Vec<(Key, DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        for (key, prev) in &previous {
            if current.contains_key(key) {
                continue;
            }
            let mut deleted_at = now;
            if deleted_at <= *prev {
                deleted_at = *prev + TimeDelta::microseconds(1);
            }
            clears.push((key.clone(), deleted_at, *prev));
        }

        let shared = self.shared.clone();
        rt::block_on(async move {
            let mut db = shared.db.lock().await;
            let client = db.client().await.map_err(|e| format!("postgres cooldown store: begin save: {}", e.err_text()))?;
            let tx = client
                .transaction()
                .await
                .map_err(|e| format!("postgres cooldown store: begin save: {}", e.err_text()))?;
            for ((auth_id, model), content, updated_at) in &encoded {
                tx.execute(upsert.as_str(), &[auth_id, model, content, updated_at])
                    .await
                    .map_err(|e| format!("postgres cooldown store: save state for {auth_id:?}: {}", e.err_text()))?;
            }
            let empty = serde_json::json!({});
            for ((auth_id, model), deleted_at, observed) in &clears {
                tx.execute(delete.as_str(), &[auth_id, model, &empty, deleted_at, observed])
                    .await
                    .map_err(|e| format!("postgres cooldown store: clear state for {auth_id:?}: {}", e.err_text()))?;
            }
            tx.commit().await.map_err(|e| format!("postgres cooldown store: commit save: {}", e.err_text()))
        })?;
        *self.previous.lock() = current;
        Ok(())
    }
}
