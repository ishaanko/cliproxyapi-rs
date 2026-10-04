//! v0 config document and scalar settings (Go: `config_basic.go`, `quota.go`).

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::http::{ApiError, ApiResult, blocking, detached, ok_json, ok_struct};
use crate::state::ManagementState;
use crate::v0_util::{
    bool_value_body, int_value_body, nil_empty, persist, string_value_body, yaml_snapshot,
};

/// `GET /config`: the runtime config as Go's `encoding/json` writes it.
pub(crate) async fn get_config(State(st): State<ManagementState>) -> ApiResult {
    let mut value = st
        .cfg()
        .to_json_value()
        .map_err(|e| ApiError::new(500, e.to_string()))?;
    nil_empty(&mut value, yaml_snapshot(&st).await.as_ref());
    Ok(ok_struct(&value))
}

/// `GET /config.yaml`: the file bytes as they are on disk.
pub(crate) async fn get_config_yaml(State(st): State<ManagementState>) -> ApiResult {
    let path = st.config_path.clone();
    let data = tokio::fs::read(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::with_message(404, "not_found", "config file not found")
        } else {
            ApiError::with_message(500, "read_failed", io_message(&e))
        }
    })?;
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/yaml; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}

/// Go's `os.PathError` text for a failed read: `read <path>: <reason>`.
fn io_message(e: &std::io::Error) -> String {
    e.to_string()
}

/// `PUT /config.yaml`: validates the document, replaces the file and reloads.
pub(crate) async fn put_config_yaml(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    detached(async move {
        // Go decodes into `config.Config` first (syntax and type errors), then runs the full
        // loader (validation errors).
        if let Err(e) = cpa_config::parse_config_bytes(&body) {
            let msg = e.to_string();
            return Err(match msg.strip_prefix("parse config payload: ") {
                Some(rest) => ApiError::with_message(400, "invalid_yaml", rest),
                None => ApiError::with_message(422, "invalid_config", msg),
            });
        }
        let _guard = st.shared.config_lock.clone().lock_owned().await;
        let path = st.config_path.clone();
        let data = body.clone();
        blocking(move || {
            crate::config_v8::write_config(&path, &data, None).map_err(|e| {
                tracing::error!("failed to write config: {e}");
                ApiError::with_message(500, "write_failed", "failed to write config")
            })
        })
        .await?;
        st.reload_config().await;
        Ok(ok_json(&json!({"ok": true, "changed": ["config"]})))
    })
    .await
}

macro_rules! getter {
    ($name:ident, $key:literal, |$c:ident| $expr:expr) => {
        pub(crate) async fn $name(State(st): State<ManagementState>) -> ApiResult {
            let $c = st.cfg();
            Ok(ok_json(&json!({ $key: $expr })))
        }
    };
}

macro_rules! bool_setter {
    ($name:ident, |$c:ident, $v:ident| $set:expr) => {
        pub(crate) async fn $name(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
            let $v = bool_value_body(&body)?;
            persist(&st, move |$c| {
                $set;
                Ok(())
            })
            .await
        }
    };
}

macro_rules! int_setter {
    ($name:ident, |$c:ident, $v:ident| $set:expr) => {
        pub(crate) async fn $name(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
            let $v = int_value_body(&body)?;
            persist(&st, move |$c| {
                $set;
                Ok(())
            })
            .await
        }
    };
}

getter!(get_debug, "debug", |c| c.debug);
bool_setter!(put_debug, |c, v| c.debug = v);

getter!(
    get_usage_statistics_enabled,
    "usage-statistics-enabled",
    |c| c.usage_statistics_enabled
);
bool_setter!(put_usage_statistics_enabled, |c, v| c
    .usage_statistics_enabled =
    v);

getter!(get_logging_to_file, "logging-to-file", |c| c
    .logging_to_file);
bool_setter!(put_logging_to_file, |c, v| c.logging_to_file = v);

getter!(get_logs_max_total_size_mb, "logs-max-total-size-mb", |c| c
    .logs_max_total_size_mb);
int_setter!(put_logs_max_total_size_mb, |c, v| c
    .logs_max_total_size_mb =
    v.max(0));

getter!(get_error_logs_max_files, "error-logs-max-files", |c| c
    .error_logs_max_files);
int_setter!(put_error_logs_max_files, |c, v| c.error_logs_max_files =
    if v < 0 { 10 } else { v });

getter!(get_request_log, "request-log", |c| c.request_log);
bool_setter!(put_request_log, |c, v| c.request_log = v);

getter!(get_ws_auth, "ws-auth", |c| c.websocket_auth);
bool_setter!(put_ws_auth, |c, v| c.websocket_auth = v);

getter!(get_request_retry, "request-retry", |c| c.request_retry);
int_setter!(put_request_retry, |c, v| c.request_retry = v);

getter!(get_max_retry_credentials, "max-retry-credentials", |c| c
    .max_retry_credentials);
int_setter!(put_max_retry_credentials, |c, v| c.max_retry_credentials =
    v);

getter!(get_max_retry_interval, "max-retry-interval", |c| c
    .max_retry_interval);
int_setter!(put_max_retry_interval, |c, v| c.max_retry_interval = v);

getter!(get_force_model_prefix, "force-model-prefix", |c| c
    .force_model_prefix);
bool_setter!(put_force_model_prefix, |c, v| c.force_model_prefix = v);

getter!(get_switch_project, "switch-project", |c| c
    .quota_exceeded
    .switch_project);
bool_setter!(put_switch_project, |c, v| c.quota_exceeded.switch_project =
    v);

getter!(get_switch_preview_model, "switch-preview-model", |c| c
    .quota_exceeded
    .switch_preview_model);
bool_setter!(put_switch_preview_model, |c, v| c
    .quota_exceeded
    .switch_preview_model =
    v);

getter!(get_proxy_url, "proxy-url", |c| c.proxy_url);

pub(crate) async fn put_proxy_url(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let v = string_value_body(&body)?;
    persist(&st, move |c| {
        c.proxy_url = v;
        Ok(())
    })
    .await
}

pub(crate) async fn delete_proxy_url(State(st): State<ManagementState>) -> ApiResult {
    persist(&st, |c| {
        c.proxy_url.clear();
        Ok(())
    })
    .await
}

/// Go: `normalizeRoutingStrategy`.
fn normalize_routing_strategy(strategy: &str) -> Option<&'static str> {
    match strategy.trim().to_lowercase().as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some("round-robin"),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => Some("weighted-round-robin"),
        "fill-first" | "fillfirst" | "ff" => Some("fill-first"),
        _ => None,
    }
}

pub(crate) async fn get_routing_strategy(State(st): State<ManagementState>) -> ApiResult {
    let cfg = st.cfg();
    let strategy: Value = match normalize_routing_strategy(&cfg.routing.strategy) {
        Some(s) => s.into(),
        None => cfg.routing.strategy.trim().into(),
    };
    Ok(ok_json(&json!({"strategy": strategy})))
}

pub(crate) async fn put_routing_strategy(
    State(st): State<ManagementState>,
    body: Bytes,
) -> ApiResult {
    let raw = string_value_body(&body)?;
    let Some(normalized) = normalize_routing_strategy(&raw) else {
        return Err(ApiError::bad_request("invalid strategy"));
    };
    persist(&st, move |c| {
        c.routing.strategy = normalized.to_string();
        Ok(())
    })
    .await
}
