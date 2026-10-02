//! Three-way merges of refreshed / prepared credentials into the live auth (Go: metadata_merge.go).
//!
//! A refresh or request-preparation runs on a clone (`base`) while requests keep mutating the
//! live record (`current`: cooldowns, operator edits). The executor's result (`updated`) is merged
//! so concurrent edits survive: a field changed only by the executor is taken, a field changed by
//! the user wins, and token payload keys always follow the executor.

use chrono::{DateTime, Utc};
use cpa_auth::credmeta::is_auth_token_payload_key;
use cpa_auth::types::{Auth, Status};
use serde_json::Value;

use super::cooldown::is_disabled;
use super::util::after;

fn trimmed_meta_str(auth: &Auth, key: &str) -> String {
    auth.metadata
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Merges request-preparation results without touching refresh lifecycle fields.
pub fn merge_prepared_auth(base: Option<&Auth>, current: &Auth, updated: &Auth) -> Auth {
    merge_auth_content(base, current, updated)
}

/// Merges refresh results from `updated` (derived from `base`) into the latest `current`,
/// preserving concurrent user edits and active cooldowns.
pub fn merge_refreshed_auth(
    base: Option<&Auth>,
    current: &Auth,
    updated: &Auth,
    now: DateTime<Utc>,
) -> Auth {
    let mut merged = merge_auth_content(base, current, updated);
    if base.is_some_and(|b| current.registration_epoch != b.registration_epoch) {
        return merged;
    }

    // 1. Refresh lifecycle timestamps.
    if updated.last_refreshed_at.is_some() {
        merged.last_refreshed_at = updated.last_refreshed_at;
    }
    if updated.next_refresh_after.is_some() || base.is_some_and(|b| b.next_refresh_after.is_some())
    {
        merged.next_refresh_after = updated.next_refresh_after;
    }

    // 2. Error and status recovery.
    let base_err = base
        .and_then(|b| b.last_error.as_ref())
        .map(|e| e.message.as_str())
        .unwrap_or("");
    let current_err = current
        .last_error
        .as_ref()
        .map(|e| e.message.as_str())
        .unwrap_or("");
    let has_new_concurrent_error = !current_err.is_empty() && current_err != base_err;

    // Disabled status three-way merge.
    let base_disabled = base.is_some_and(is_disabled);
    let current_disabled = is_disabled(current);
    let updated_disabled = is_disabled(updated);
    let changed_by_executor = updated_disabled != base_disabled;
    let changed_by_user = current_disabled != base_disabled;
    let mut final_disabled = current_disabled;
    if changed_by_executor && !changed_by_user {
        final_disabled = updated_disabled;
    }
    if final_disabled {
        merged.disabled = true;
        merged.status = Status::Disabled;
        merged.metadata.insert("disabled".into(), Value::Bool(true));
    } else {
        merged.disabled = false;
        if merged.status == Status::Disabled {
            merged.status = Status::Active;
        }
        merged
            .metadata
            .insert("disabled".into(), Value::Bool(false));

        if has_new_concurrent_error {
            // A new error landed concurrently (503, 429, timeout): preserve it.
            merged.last_error = current.last_error.clone();
            merged.status = current.status;
            merged.unavailable = current.unavailable;
            merged.status_message = current.status_message.clone();
        } else if (current.quota.exceeded
            && current.quota.reason == "credential_quota"
            && after(current.quota.next_recover_at, now))
            || (current.unavailable && after(current.next_retry_after, now))
        {
            // Preserve an active credential quota or cooldown.
            merged.unavailable = current.unavailable;
            merged.status = current.status;
            merged.status_message = current.status_message.clone();
        } else if updated.status == Status::Active || updated.status == Status::Unknown {
            // Successful refresh clears the previous auth error.
            merged.status = Status::Active;
            merged.unavailable = false;
            merged.status_message.clear();
            merged.last_error = None;
        }
    }

    // 3. Model states: three-way merge to preserve concurrent cooldown/quota.
    let empty = Default::default();
    let base_models = base.map(|b| &b.model_states).unwrap_or(&empty);
    if !updated.model_states.is_empty() {
        for (model, upd_state) in &updated.model_states {
            let base_state = base_models.get(model);
            let current_state = current.model_states.get(model);
            let changed_by_exec = base_state != Some(upd_state);
            let changed_by_usr = base_state != current_state;
            if changed_by_exec && !changed_by_usr {
                merged.model_states.insert(model.clone(), upd_state.clone());
            }
        }
        for (model, base_state) in base_models {
            if !updated.model_states.contains_key(model)
                && current
                    .model_states
                    .get(model)
                    .is_some_and(|cur| cur == base_state)
            {
                merged.model_states.remove(model);
            }
        }
    }
    merged
}

fn merge_auth_content(base: Option<&Auth>, current: &Auth, updated: &Auth) -> Auth {
    if base.is_some_and(|b| current.registration_epoch != b.registration_epoch) {
        // Stale update from a previous registration cycle; keep current state.
        return current.clone();
    }
    let mut merged = current.clone();
    let empty_meta = Default::default();
    let base_meta = base.map(|b| &b.metadata).unwrap_or(&empty_meta);

    // 1. Metadata three-way merge (proxy_url has a dedicated merge below).
    for (k, v) in &updated.metadata {
        if k.trim().eq_ignore_ascii_case("proxy_url") {
            continue;
        }
        let base_val = base_meta.get(k);
        let current_val = current.metadata.get(k);
        let changed_by_executor = base_val.is_none() || base_val != Some(v);
        let changed_by_user = base_val.is_some() != current_val.is_some()
            || (base_val.is_some() && base_val != current_val);
        if changed_by_executor && (!changed_by_user || is_auth_token_payload_key(k)) {
            merged.metadata.insert(k.clone(), v.clone());
        }
    }
    // Deletions by the executor apply only if the user did not edit the field.
    for (k, base_val) in base_meta {
        if k.trim().eq_ignore_ascii_case("proxy_url") {
            continue;
        }
        if !updated.metadata.contains_key(k)
            && current.metadata.get(k).is_some_and(|c| c == base_val)
        {
            merged.metadata.shift_remove(k);
        }
    }

    // 2. Storage and runtime follow the executor.
    if updated.storage.is_some() {
        merged.storage = updated.storage.clone();
    }
    if updated.runtime.is_some() {
        merged.runtime = updated.runtime.clone();
    }

    // 3. Proxy URL three-way merge across the struct field and metadata copy.
    let base_struct = base
        .map(|b| b.proxy_url.trim().to_string())
        .unwrap_or_default();
    let current_struct = current.proxy_url.trim().to_string();
    let updated_struct = updated.proxy_url.trim().to_string();
    let base_meta_proxy = base
        .map(|b| trimmed_meta_str(b, "proxy_url"))
        .unwrap_or_default();
    let current_meta_proxy = trimmed_meta_str(current, "proxy_url");
    let updated_meta_proxy = trimmed_meta_str(updated, "proxy_url");

    let user_changed_struct = current_struct != base_struct;
    let user_changed_meta = current_meta_proxy != base_meta_proxy;
    let exec_changed_struct = updated_struct != base_struct;
    let exec_changed_meta = updated_meta_proxy != base_meta_proxy;

    let mut final_proxy = current_struct.clone();
    if !current_meta_proxy.is_empty() && current_struct.is_empty() && !user_changed_struct {
        final_proxy = current_meta_proxy.clone();
    }
    if user_changed_struct || user_changed_meta {
        final_proxy = if user_changed_struct && !user_changed_meta {
            current_struct.clone()
        } else if user_changed_meta && !user_changed_struct {
            current_meta_proxy.clone()
        } else if !current_struct.is_empty() {
            current_struct.clone()
        } else {
            current_meta_proxy.clone()
        };
    } else if exec_changed_struct || exec_changed_meta {
        final_proxy = if exec_changed_struct && !exec_changed_meta {
            updated_struct.clone()
        } else if exec_changed_meta && !exec_changed_struct {
            updated_meta_proxy.clone()
        } else if !updated_struct.is_empty() {
            updated_struct.clone()
        } else {
            updated_meta_proxy.clone()
        };
    }
    if final_proxy.is_empty() {
        merged.proxy_url.clear();
        merged.metadata.shift_remove("proxy_url");
    } else {
        merged.proxy_url = final_proxy.clone();
        merged
            .metadata
            .insert("proxy_url".into(), Value::String(final_proxy));
    }

    // 4. Prefix: user edits win.
    let base_prefix = base
        .map(|b| b.prefix.trim().to_string())
        .unwrap_or_default();
    let current_prefix = current.prefix.trim().to_string();
    let updated_prefix = updated.prefix.trim().to_string();
    merged.prefix = if updated_prefix != base_prefix && current_prefix == base_prefix {
        updated_prefix
    } else {
        current_prefix
    };

    // 5. Attributes three-way merge.
    if !updated.attributes.is_empty() || base.is_some_and(|b| !b.attributes.is_empty()) {
        let empty_attrs = Default::default();
        let base_attrs = base.map(|b| &b.attributes).unwrap_or(&empty_attrs);
        for (k, v) in &updated.attributes {
            let base_val = base_attrs.get(k);
            let current_val = current.attributes.get(k);
            let changed_by_executor = base_val.is_none() || base_val != Some(v);
            let changed_by_user = base_val.is_some() != current_val.is_some()
                || (base_val.is_some() && base_val != current_val);
            if changed_by_executor && !changed_by_user {
                merged.attributes.insert(k.clone(), v.clone());
            }
        }
        for (k, base_val) in base_attrs {
            if !updated.attributes.contains_key(k)
                && current.attributes.get(k).is_some_and(|c| c == base_val)
            {
                merged.attributes.remove(k);
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth_with(access: &str, proxy: &str) -> Auth {
        let mut a = Auth::new("a", "claude");
        a.registration_epoch = 1;
        a.metadata.insert("access_token".into(), json!(access));
        a.metadata.insert("note".into(), json!("orig"));
        a.proxy_url = proxy.into();
        a
    }

    #[test]
    fn refresh_keeps_concurrent_user_edits_and_takes_tokens() {
        let base = auth_with("old", "");
        let mut current = base.clone();
        current
            .metadata
            .insert("note".into(), json!("edited by operator"));
        current.proxy_url = "http://proxy".into();
        let mut updated = base.clone();
        updated.metadata.insert("access_token".into(), json!("new"));
        updated.metadata.insert("note".into(), json!("exec note"));
        updated.last_refreshed_at = Some(DateTime::from_timestamp(1_800_000_000, 0).unwrap());
        let merged = merge_refreshed_auth(
            Some(&base),
            &current,
            &updated,
            DateTime::from_timestamp(1_800_000_010, 0).unwrap(),
        );
        assert_eq!(merged.metadata["access_token"], "new");
        assert_eq!(merged.metadata["note"], "edited by operator");
        assert_eq!(merged.proxy_url, "http://proxy");
        assert_eq!(merged.last_refreshed_at, updated.last_refreshed_at);
    }

    #[test]
    fn refresh_preserves_concurrent_cooldown_error() {
        let base = auth_with("old", "");
        let mut current = base.clone();
        current.last_error = Some(cpa_auth::types::AuthError {
            message: "429".into(),
            http_status: 429,
            ..Default::default()
        });
        current.unavailable = true;
        current.status = Status::Error;
        let mut updated = base.clone();
        updated.status = Status::Active;
        let merged = merge_refreshed_auth(
            Some(&base),
            &current,
            &updated,
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        );
        assert!(merged.unavailable && merged.status == Status::Error);
        assert_eq!(merged.last_error.unwrap().http_status, 429);
    }

    #[test]
    fn stale_registration_epoch_keeps_current() {
        let base = auth_with("old", "");
        let mut current = base.clone();
        current.registration_epoch = 2;
        current
            .metadata
            .insert("access_token".into(), json!("current"));
        let mut updated = base.clone();
        updated
            .metadata
            .insert("access_token".into(), json!("stale-exec"));
        let merged = merge_prepared_auth(Some(&base), &current, &updated);
        assert_eq!(merged.metadata["access_token"], "current");
    }
}
