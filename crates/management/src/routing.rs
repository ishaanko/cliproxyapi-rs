//! `POST /credentials/refresh`, `POST /routing/cooldown/reset` and
//! `GET /routing/model-definitions/{channel}` (Go: `auth_files_refresh.go`, `quota.go`,
//! `model_definitions.go`).

use axum::extract::{Path, Request, State};
use bytes::Bytes;
use chrono::Utc;
use cpa_auth::Auth;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cooldown::reset_cooldowns;
use crate::credentials::{ERR_NOT_FOUND, auth_by_index, lookup_auth_file};
use crate::http::{ApiError, ApiResult, ok_json, query_get, query_trim};
use crate::state::ManagementState;

#[derive(Deserialize, Default)]
struct RefreshRequest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    auth_index: String,
    #[serde(default)]
    all: bool,
}

fn has_refresh_credential(auth: &Auth) -> bool {
    !auth.refresh_token().is_empty() && cpa_auth::refresh::supports_refresh(auth)
}

/// `POST /credentials/refresh`: `?all=true`, `?name=`, or a JSON body `{"name","auth_index","all"}`.
pub(crate) async fn refresh(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let uri = req.uri().clone();
    let body = crate::http::read_body(req.into_body())
        .await
        .map_err(|_| ApiError::bad_request("failed to read body"))?;
    // Token exchanges may rotate refresh tokens: finish them even if the client goes away.
    crate::http::detached(refresh_inner(st, uri, body)).await
}

async fn refresh_inner(st: ManagementState, uri: axum::http::Uri, body: Bytes) -> ApiResult {
    let mut parsed = RefreshRequest::default();
    if !body.trim_ascii().is_empty() {
        parsed = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
    }
    if query_get(&uri, "all").as_deref() == Some("true") {
        parsed.all = true;
    }
    let name_q = query_trim(&uri, "name");
    if !name_q.is_empty() && parsed.name.is_empty() {
        parsed.name = name_q;
    }
    let index_q = query_trim(&uri, "auth_index");
    if !index_q.is_empty() && parsed.auth_index.is_empty() {
        parsed.auth_index = index_q;
    }

    if parsed.all {
        let ids: Vec<String> = st
            .registry
            .list()
            .into_iter()
            .filter(|a| !a.disabled && has_refresh_credential(a))
            .map(|a| a.id)
            .collect();
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            match st.registry.force_refresh_auth(&id).await {
                Ok(_) => results.push(json!({"id": id, "success": true})),
                Err(e) => results.push(json!({"id": id, "success": false, "error": e})),
            }
        }
        return Ok(ok_json(&json!({"ok": true, "results": results})));
    }

    let name = parsed.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name or all=true is required"));
    }
    let Some(target) = lookup_auth_file(&st, name, &parsed.auth_index) else {
        return Err(ApiError::new(404, ERR_NOT_FOUND));
    };
    let refreshed = st
        .registry
        .force_refresh_auth(&target.id)
        .await
        .map_err(|e| ApiError::new(500, e))?;
    Ok(ok_json(
        &json!({"ok": true, "auth": serde_json::to_value(&refreshed).unwrap_or(Value::Null)}),
    ))
}

/// `POST /routing/cooldown/reset` with `{"auth_index": "..."}`.
pub(crate) async fn reset_cooldown(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    crate::http::detached(reset_cooldown_inner(st, body)).await
}

async fn reset_cooldown_inner(st: ManagementState, body: Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        auth_index: String,
    }
    let req: Body =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid request body"))?;
    let index = req.auth_index.trim();
    if index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    let Some(mut auth) = auth_by_index(&st, index) else {
        return Err(ApiError::new(404, "auth not found"));
    };
    let mut models = reset_cooldowns(&mut auth, Utc::now());
    if models.is_empty() {
        models = registered_models(&auth.id);
    }
    let mut stored = st
        .registry
        .update(auth)
        .await
        .map_err(|e| ApiError::new(500, format!("failed to reset quota: {e}")))?;
    Ok(ok_json(
        &json!({"status": "ok", "auth_index": stored.ensure_index(), "models": models}),
    ))
}

/// Models the registry currently serves for a credential (used when it has no per-model state).
fn registered_models(auth_id: &str) -> Vec<String> {
    let mut models: Vec<String> = cpa_core::registry::global_registry()
        .get_models_for_client(auth_id)
        .into_iter()
        .map(|m| m.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    models.sort();
    models.dedup();
    models
}

/// `GET /routing/model-definitions/{channel}`: static model metadata of a channel.
pub(crate) async fn model_definitions(Path(channel): Path<String>) -> ApiResult {
    let channel = channel.trim().to_string();
    if channel.is_empty() {
        return Err(ApiError::bad_request("channel is required"));
    }
    let Some(models) = cpa_core::registry::get_static_model_definitions_by_channel(&channel) else {
        return Err(ApiError::from_body(
            400,
            json!({"error": "unknown channel", "channel": channel}),
        ));
    };
    Ok(ok_json(
        &json!({"channel": channel.to_lowercase(), "models": models}),
    ))
}
