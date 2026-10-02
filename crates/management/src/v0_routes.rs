//! The deprecated `/v0/management` tree (Go: `registerManagementRoutes`). Handlers shared with
//! v8 (credentials, logs, usage, oauth sessions) are reused; the config, scalar and key-list
//! handlers are v0 only.

use axum::Router;
use axum::extract::{Request, State};
use axum::routing::{delete, get, patch, post};
use cpa_auth::Provider;
use tower::ServiceExt;

use crate::http::empty;
use crate::state::ManagementState;
use crate::{
    credential_edit, credentials, gate, key_lists as k, logs, oauth, observability, plugins_v0,
    routing, settings_v0 as s, tools,
};

macro_rules! auth_url {
    ($provider:expr) => {
        |State(st): State<ManagementState>, req: Request| async move {
            oauth::auth_url_for(st, req, $provider).await
        }
    };
}

/// Every `/v0/management` route behind the availability gate and the key check. Requests that
/// match no route (or the wrong method) get the same gate and then a bare 404, like Go's
/// `pluginManagementNoRoute`.
pub(crate) fn router(state: ManagementState) -> Router<ManagementState> {
    let routes = Router::new()
        .route("/config", get(s::get_config))
        .route("/config.yaml", get(s::get_config_yaml).put(s::put_config_yaml))
        .route("/latest-version", get(tools::latest_version))
        .route("/plugins", get(tools::list_plugins))
        .route("/plugin-store", get(plugins_v0::store))
        .route("/plugin-store/{id}/install", post(plugins_v0::install))
        .route("/plugins/{id}", delete(plugins_v0::delete))
        .route("/plugins/{id}/enabled", patch(plugins_v0::patch_enabled))
        .route(
            "/plugins/{id}/config",
            get(plugins_v0::get_config)
                .put(plugins_v0::put_config)
                .patch(plugins_v0::patch_config),
        )
        .route(
            "/plugins/{id}/quota",
            get(plugins_v0::get_quota)
                .post(plugins_v0::fetch_quota)
                .delete(plugins_v0::reset_quota),
        )
        .route("/plugins/{id}/quota/reset", post(plugins_v0::reset_quota))
        .route("/debug", get(s::get_debug).put(s::put_debug).patch(s::put_debug))
        .route(
            "/logging-to-file",
            get(s::get_logging_to_file)
                .put(s::put_logging_to_file)
                .patch(s::put_logging_to_file),
        )
        .route(
            "/logs-max-total-size-mb",
            get(s::get_logs_max_total_size_mb)
                .put(s::put_logs_max_total_size_mb)
                .patch(s::put_logs_max_total_size_mb),
        )
        .route(
            "/error-logs-max-files",
            get(s::get_error_logs_max_files)
                .put(s::put_error_logs_max_files)
                .patch(s::put_error_logs_max_files),
        )
        .route(
            "/usage-statistics-enabled",
            get(s::get_usage_statistics_enabled)
                .put(s::put_usage_statistics_enabled)
                .patch(s::put_usage_statistics_enabled),
        )
        .route(
            "/proxy-url",
            get(s::get_proxy_url)
                .put(s::put_proxy_url)
                .patch(s::put_proxy_url)
                .delete(s::delete_proxy_url),
        )
        .route("/api-call", post(tools::api_call))
        .route(
            "/quota-exceeded/switch-project",
            get(s::get_switch_project)
                .put(s::put_switch_project)
                .patch(s::put_switch_project),
        )
        .route(
            "/quota-exceeded/switch-preview-model",
            get(s::get_switch_preview_model)
                .put(s::put_switch_preview_model)
                .patch(s::put_switch_preview_model),
        )
        .route("/reset-quota", post(routing::reset_cooldown))
        .route("/quota/providers", get(plugins_v0::quota_providers))
        .route("/quota/fetch", post(plugins_v0::fetch_credential_quota))
        .route("/quota/reset", post(plugins_v0::reset_credential_quota))
        .route(
            "/api-keys",
            get(k::get_api_keys)
                .put(k::put_api_keys)
                .patch(k::patch_api_keys)
                .delete(k::delete_api_keys),
        )
        .route("/api-key-usage", get(observability::api_key_usage))
        .route("/usage-queue", get(observability::usage_queue))
        .route(
            "/gemini-api-key",
            get(k::get_gemini_keys)
                .put(k::put_gemini_keys)
                .patch(k::patch_gemini_key)
                .delete(k::delete_gemini_key),
        )
        .route(
            "/interactions-api-key",
            get(k::get_interactions_keys)
                .put(k::put_interactions_keys)
                .patch(k::patch_interactions_key)
                .delete(k::delete_interactions_key),
        )
        .route("/logs", get(logs::get_logs).delete(logs::delete_logs))
        .route("/request-error-logs", get(logs::error_logs))
        .route("/request-error-logs/{name}", get(logs::download_error_log))
        .route("/request-log-by-id/{id}", get(logs::request_log_by_id))
        .route(
            "/request-log",
            get(s::get_request_log)
                .put(s::put_request_log)
                .patch(s::put_request_log),
        )
        .route(
            "/ws-auth",
            get(s::get_ws_auth).put(s::put_ws_auth).patch(s::put_ws_auth),
        )
        .route(
            "/request-retry",
            get(s::get_request_retry)
                .put(s::put_request_retry)
                .patch(s::put_request_retry),
        )
        .route(
            "/max-retry-credentials",
            get(s::get_max_retry_credentials)
                .put(s::put_max_retry_credentials)
                .patch(s::put_max_retry_credentials),
        )
        .route(
            "/max-retry-interval",
            get(s::get_max_retry_interval)
                .put(s::put_max_retry_interval)
                .patch(s::put_max_retry_interval),
        )
        .route(
            "/force-model-prefix",
            get(s::get_force_model_prefix)
                .put(s::put_force_model_prefix)
                .patch(s::put_force_model_prefix),
        )
        .route(
            "/routing/strategy",
            get(s::get_routing_strategy)
                .put(s::put_routing_strategy)
                .patch(s::put_routing_strategy),
        )
        .route(
            "/claude-api-key",
            get(k::get_claude_keys)
                .put(k::put_claude_keys)
                .patch(k::patch_claude_key)
                .delete(k::delete_claude_key),
        )
        .route(
            "/codex-api-key",
            get(k::get_codex_keys)
                .put(k::put_codex_keys)
                .patch(k::patch_codex_key)
                .delete(k::delete_codex_key),
        )
        .route(
            "/xai-api-key",
            get(k::get_xai_keys)
                .put(k::put_xai_keys)
                .patch(k::patch_xai_key)
                .delete(k::delete_xai_key),
        )
        .route(
            "/meta-api-key",
            get(k::get_meta_keys)
                .put(k::put_meta_keys)
                .patch(k::patch_meta_key)
                .delete(k::delete_meta_key),
        )
        .route(
            "/openai-compatibility",
            get(k::get_openai_compat)
                .put(k::put_openai_compat)
                .patch(k::patch_openai_compat)
                .delete(k::delete_openai_compat),
        )
        .route(
            "/vertex-api-key",
            get(k::get_vertex_keys)
                .put(k::put_vertex_keys)
                .patch(k::patch_vertex_key)
                .delete(k::delete_vertex_key),
        )
        .route(
            "/oauth-excluded-models",
            get(k::get_oauth_excluded_models)
                .put(k::put_oauth_excluded_models)
                .patch(k::patch_oauth_excluded_models)
                .delete(k::delete_oauth_excluded_models),
        )
        .route(
            "/oauth-model-alias",
            get(k::get_oauth_model_alias)
                .put(k::put_oauth_model_alias)
                .patch(k::patch_oauth_model_alias)
                .delete(k::delete_oauth_model_alias),
        )
        .route(
            "/oauth-request-scoped-errors",
            get(k::get_oauth_request_scoped_errors)
                .put(k::put_oauth_request_scoped_errors)
                .patch(k::patch_oauth_request_scoped_errors)
                .delete(k::delete_oauth_request_scoped_errors),
        )
        .route(
            "/auth-files",
            get(credentials::list)
                .post(credentials::upload)
                .delete(credentials::delete),
        )
        .route("/auth-files/models", get(credentials::models))
        .route("/model-definitions/{channel}", get(routing::model_definitions))
        .route("/auth-files/download", get(credentials::download))
        .route("/auth-files/status", patch(credential_edit::patch_status))
        .route("/auth-files/fields", patch(credential_edit::patch_fields))
        .route("/auth-files/refresh", post(routing::refresh))
        .route("/vertex/import", post(oauth::import_vertex_v0))
        .route("/anthropic-auth-url", get(auth_url!(Provider::Claude)))
        .route("/codex-auth-url", get(auth_url!(Provider::Codex)))
        .route("/antigravity-auth-url", get(auth_url!(Provider::Antigravity)))
        .route("/kimi-auth-url", get(auth_url!(Provider::Kimi)))
        .route("/kimi-ai-auth-url", get(auth_url!(Provider::KimiAi)))
        .route("/xai-auth-url", get(auth_url!(Provider::Xai)))
        .route("/devin-auth-url", get(auth_url!(Provider::Devin)))
        .route("/meta-auth-url", get(auth_url!(Provider::Meta)))
        .route("/get-auth-status", get(oauth::status))
        .route("/oauth-session", delete(oauth::cancel_session))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gate::authenticate,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gate::availability,
        ));

    // Unrouted requests still pass the gate: unavailable is a bare 404, a bad key is 401/403,
    // and only an authenticated caller learns the (also bare, but CPA-headed) 404.
    let gate_then_404 = Router::new()
        .fallback(|| async { empty(404) })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gate::authenticate,
        ))
        .layer(axum::middleware::from_fn_with_state(state, gate::availability));
    let no_route = move |req: Request| {
        let svc = gate_then_404.clone();
        async move {
            match svc.oneshot(req).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            }
        }
    };
    routes
        .fallback(no_route.clone())
        .method_not_allowed_fallback(no_route)
}
