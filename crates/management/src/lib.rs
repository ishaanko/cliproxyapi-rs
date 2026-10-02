//! The v8 management API as an axum router (Go: `internal/api/handlers/management` and
//! `internal/api/server_management*.go`).
//!
//! ```ignore
//! let state = ManagementState::new(config_path, config_rx, manager, store, sessions, login, usage, log_dir);
//! let app = Router::new().merge(proxy_routes).merge(cpa_management::router(state.clone()))
//!     .merge(cpa_management::oauth_redirect_router(state));
//! // Client IPs (ban list, localhost rule) need connection info:
//! axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());
//! ```
//!
//! Everything below `/v8/management` sits behind the availability gate (bare 404 until a
//! management secret exists, or in Home mode) and the management key check (Bearer or
//! `X-Management-Key`, bcrypt-hashed `secret-key`, `MANAGEMENT_PASSWORD`, local password, remote
//! rules, IP ban after 5 failures). `/oauth/callback` skips the key check like in Go.
//!
//! Not provided: plugin management (`GET /plugins` lists none, the rest answers 501), the Redis
//! usage queue (`/observability/usage/queue` answers 501; use `/observability/requests`), and
//! the deprecated `/v0/management` tree except `/v0/management/oauth-callback`.

mod config_v8;
mod cooldown;
mod credential_edit;
mod credentials;
mod gate;
mod gin_routes;
mod go_json;
mod http;
mod key_lists;
mod logs;
mod oauth;
mod observability;
mod plugin_store;
mod plugins_v0;
mod routing;
mod settings_v0;
mod state;
mod tools;
mod v0_routes;
mod v0_util;
mod yaml_comments;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::Method;
use axum::middleware;
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use bytes::Bytes;

use config_v8::{ConfigRequest, split_path};

pub use gin_routes::GIN_ROUTES;
pub use oauth::oauth_redirect_router;
pub use state::{AuthRegistry, BuildInfo, ManagementState, ReloadHook};

/// Largest accepted request body (credential uploads, config documents).
pub(crate) const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

async fn config_root(State(st): State<ManagementState>, method: Method, body: Bytes) -> Response {
    config_v8::handle(
        &st,
        ConfigRequest {
            method,
            yaml: false,
            parts: Vec::new(),
            body,
        },
    )
    .await
}

async fn config_yaml(State(st): State<ManagementState>, method: Method, body: Bytes) -> Response {
    config_v8::handle(
        &st,
        ConfigRequest {
            method,
            yaml: true,
            parts: Vec::new(),
            body,
        },
    )
    .await
}

async fn config_node(
    State(st): State<ManagementState>,
    method: Method,
    Path(path): Path<String>,
    body: Bytes,
) -> Response {
    config_v8::handle(
        &st,
        ConfigRequest {
            method,
            yaml: false,
            parts: split_path(&path),
            body,
        },
    )
    .await
}

/// The management router. Mount it on the main server; see the crate docs for the serve call.
pub fn router(state: ManagementState) -> Router {
    let v8 = Router::new()
        .route(
            "/config",
            get(config_root).put(config_root).patch(config_root),
        )
        .route("/config.yaml", get(config_yaml).put(config_yaml))
        .route(
            "/config/",
            get(config_root)
                .put(config_root)
                .patch(config_root)
                .delete(config_root),
        )
        .route(
            "/config/{*path}",
            get(config_node)
                .put(config_node)
                .patch(config_node)
                .delete(config_node),
        )
        .route("/server/latest-version", get(tools::latest_version))
        .route("/requests/api-call", post(tools::api_call))
        .route("/routing/cooldown/reset", post(routing::reset_cooldown))
        .route(
            "/routing/model-definitions/{channel}",
            get(routing::model_definitions),
        )
        .route(
            "/observability/logs",
            get(logs::get_logs).delete(logs::delete_logs),
        )
        .route("/observability/logs/errors", get(logs::error_logs))
        .route(
            "/observability/logs/errors/{name}",
            get(logs::download_error_log),
        )
        .route(
            "/observability/logs/requests/{id}",
            get(logs::request_log_by_id),
        )
        .route(
            "/observability/usage/api-keys",
            get(observability::api_key_usage),
        )
        .route(
            "/observability/usage/queue",
            get(observability::usage_queue),
        )
        .route(
            "/observability/usage/summary",
            get(observability::usage_summary),
        )
        .route(
            "/observability/requests",
            get(observability::usage_requests),
        )
        .route(
            "/credentials",
            get(credentials::list)
                .post(credentials::upload)
                .delete(credentials::delete),
        )
        .route("/credentials/models", get(credentials::models))
        .route("/credentials/download", get(credentials::download))
        .route("/credentials/status", patch(credential_edit::patch_status))
        .route("/credentials/fields", patch(credential_edit::patch_fields))
        .route("/credentials/refresh", post(routing::refresh))
        .route("/oauth/import", post(oauth::import))
        .route("/oauth/auth-url", get(oauth::auth_url))
        .route("/oauth/status", get(oauth::status))
        .route("/oauth/session", delete(oauth::cancel_session))
        .route("/plugins", get(tools::list_plugins))
        .route("/plugins/{id}", delete(tools::plugins_unavailable))
        .route("/plugins/store", get(tools::plugins_unavailable))
        .route(
            "/plugins/store/{id}/install",
            post(tools::plugins_unavailable),
        )
        .route(
            "/plugins/{id}/quota",
            get(tools::plugins_unavailable)
                .post(tools::plugins_unavailable)
                .delete(tools::plugins_unavailable),
        )
        // Key check first, availability gate outside it.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate::authenticate,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate::availability,
        ));

    // The callback endpoints validate a pending `state` instead of a key.
    let callbacks = Router::new()
        .route(
            "/v8/management/oauth/callback",
            get(oauth::callback).post(oauth::callback),
        )
        .route(
            "/v0/management/oauth-callback",
            get(oauth::callback).post(oauth::callback),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate::availability,
        ));

    Router::new()
        .nest("/v8/management", v8)
        .nest("/v0/management", v0_routes::router(state.clone()))
        .merge(callbacks)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(gate::cors))
        .with_state(state)
}
