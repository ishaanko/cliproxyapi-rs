//! Helpers shared by the v0 handlers: Go-style JSON body binding and the config
//! mutate-save-reload cycle (Go: `Handler.persist`).

use axum::http::Uri;
use cpa_config::Config;
use serde_json::{Map, Value, json};

use crate::http::{ApiError, ApiResult, blocking, detached, ok_json};
use crate::state::ManagementState;

/// First JSON value of `body`; trailing bytes are ignored like Go's `json.Decoder.Decode`
/// (gin's `ShouldBindJSON`). `None` on empty or malformed input.
pub(crate) fn first_json(body: &[u8]) -> Option<Value> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// Case-insensitive field lookup, exact spelling first (Go's `encoding/json` field matching).
pub(crate) fn field<'a>(obj: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    obj.get(name).or_else(|| {
        obj.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

/// `{"value": ...}` body of the simple setters: the object's `value` unless it is absent or null.
/// Any other shape is the `invalid body` error.
pub(crate) fn value_body(body: &[u8]) -> ApiResult<Value> {
    let invalid = || ApiError::bad_request("invalid body");
    match first_json(body) {
        Some(Value::Object(obj)) => match field(&obj, "value") {
            Some(Value::Null) | None => Err(invalid()),
            Some(v) => Ok(v.clone()),
        },
        _ => Err(invalid()),
    }
}

pub(crate) fn bool_value_body(body: &[u8]) -> ApiResult<bool> {
    value_body(body)?
        .as_bool()
        .ok_or_else(|| ApiError::bad_request("invalid body"))
}

pub(crate) fn int_value_body(body: &[u8]) -> ApiResult<i64> {
    value_body(body)?
        .as_i64()
        .ok_or_else(|| ApiError::bad_request("invalid body"))
}

pub(crate) fn string_value_body(body: &[u8]) -> ApiResult<String> {
    match value_body(body)? {
        Value::String(s) => Ok(s),
        _ => Err(ApiError::bad_request("invalid body")),
    }
}

/// `fmt.Sscanf(s, "%d", &n)`: optional sign and digits at the start, the rest is ignored.
pub(crate) fn sscanf_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits_start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    t[..end].parse().ok()
}

/// `strings.TrimSpace(c.Query(key))`.
pub(crate) fn q_trim(uri: &Uri, key: &str) -> String {
    crate::http::query_trim(uri, key)
}

/// Go marshals nil slices as `null` and empty non-nil slices as `[]`. A config list is nil when
/// its key is missing (or null) in the YAML the config was loaded from, and empty-but-present
/// when the file holds `[]`; the Rust config cannot tell the two apart, so each empty array in
/// config-derived JSON looks up the same path in `yaml` (the legacy-layout file) and stays `[]`
/// only when the file has a sequence there.
pub(crate) fn nil_empty(v: &mut Value, yaml: Option<&serde_yaml_ng::Value>) {
    match v {
        Value::Array(items) if items.is_empty() => {
            if !yaml.is_some_and(serde_yaml_ng::Value::is_sequence) {
                *v = Value::Null;
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                nil_empty(item, yaml.and_then(|y| y.get(i)));
            }
        }
        Value::Object(map) => {
            for (k, item) in map.iter_mut() {
                nil_empty(item, yaml.and_then(|y| y.get(k.as_str())));
            }
        }
        _ => {}
    }
}

/// The config file parsed as plain YAML, for [`nil_empty`]. `None` when unreadable.
pub(crate) async fn yaml_snapshot(st: &ManagementState) -> Option<serde_yaml_ng::Value> {
    let text = tokio::fs::read_to_string(&st.config_path).await.ok()?;
    serde_yaml_ng::from_str(&text).ok()
}

/// Go: mutate `h.cfg`, then `h.persist(c)`. Runs `edit` on a copy of the live config under the
/// config lock; on success the file is saved in the legacy layout (comments preserved), the
/// reload hook runs and the answer is `{"status":"ok"}`. An `Err` from `edit` is answered as is
/// and nothing is written. The work is detached so a client disconnect cannot interrupt it.
pub(crate) async fn persist<F>(st: &ManagementState, edit: F) -> ApiResult
where
    F: FnOnce(&mut Config) -> ApiResult<()> + Send + 'static,
{
    let st = st.clone();
    detached(async move {
        let _guard = st.shared.config_lock.clone().lock_owned().await;
        let mut cfg = (*st.cfg()).clone();
        edit(&mut cfg)?;
        let path = st.config_path.clone();
        blocking(move || {
            cpa_config::save_config_preserve_comments(&path, &mut cfg, false)
                .map_err(|e| ApiError::new(500, format!("failed to save config: {e}")))
        })
        .await?;
        st.reload_config().await;
        Ok(ok_json(&json!({"status": "ok"})))
    })
    .await
}
