//! Codex live / realtime routes (Go: server_routes.go registrations and the realtime auth
//! middlewares in server_middleware.go). The handlers themselves live in `cpa-live`.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Extension, Path, Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::{get, post};
use cpa_live::endpoints;
use cpa_live::{Caller, ClientSecretCaller, Handler, RequestParts, UpstreamLog};
use serde_json::json;

use crate::reply::Reply;
use crate::middleware;
use crate::req::{AuthenticatedKey, ReqInfo};
use crate::reqlog::ApiLog;
use crate::state::AppState;

/// Identity established by the realtime auth middlewares (`userApiKey` / `accessProvider`),
/// with the client-secret session when a local `ek_` secret was used.
#[derive(Clone)]
pub struct SecretIdentity {
    principal: String,
    provider: String,
    secret: Option<ClientSecretCaller>,
}

/// Request log capture for upstream calls made on behalf of one inbound request.
struct ReqLog(Arc<ApiLog>);

impl UpstreamLog for ReqLog {
    fn response_chunk(&self, data: &[u8]) {
        self.0.append_api_response(data);
    }

    fn response_error(&self, err: &str) {
        self.0.record_error(502, err);
    }

    fn websocket_error(&self, _stage: &str, err: &str) {
        self.0.record_error(502, err);
    }
}

fn caller(info: &ReqInfo, identity: Option<SecretIdentity>) -> Caller {
    let trace = info.trace.clone();
    let request_id = info.request_id.clone();
    let mut caller = Caller {
        trace: Some(Arc::new(move |index| trace.record(index, &request_id))),
        log: Some(Arc::new(ReqLog(info.api_log.clone()))),
        ..Caller::default()
    };
    if let Some(id) = identity {
        caller.principal = id.principal;
        caller.provider = id.provider;
        caller.client_secret = id.secret;
    }
    caller
}

fn parts(info: &ReqInfo, call_id: Option<String>) -> RequestParts {
    RequestParts::new(&info.path, &info.raw_query, &info.headers, call_id)
}

fn realtime_auth_error(status: u16, message: &str, kind: &str, code: &str) -> Response {
    Reply::json_value(status, &json!({"error": {"message": message, "type": kind, "param": null, "code": code}})).into_response()
}

/// `realtimeStandardAuthMiddleware`: the shared access chain (config keys and plugin frontend
/// auth providers) with realtime-shaped errors.
async fn standard_auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    match middleware::authenticate_request(&st, &mut req).await {
        Ok(Some(principal)) => {
            req.extensions_mut().insert(SecretIdentity {
                principal: principal.principal.clone(),
                provider: principal.provider.clone(),
                secret: None,
            });
            req.extensions_mut().insert(AuthenticatedKey(principal));
            next.run(req).await
        }
        Ok(None) => next.run(req).await,
        Err(failure) => {
            let status = failure.status();
            if status >= 500 {
                realtime_auth_error(status, failure.message(), "server_error", "authentication_service_error")
            } else {
                realtime_auth_error(status, failure.message(), "authentication_error", "invalid_api_key")
            }
        }
    }
}

/// `realtimeAuthMiddleware`: a local `ek_` client secret first, then standard auth.
async fn secret_or_standard_auth(State((st, live)): State<(AppState, Handler)>, mut req: Request, next: Next) -> Response {
    let (authorization, matched, error) = live.authenticate_client_secret(req.headers());
    if !matched {
        return standard_auth(State(st), req, next).await;
    }
    if let Some(message) = error {
        return realtime_auth_error(401, &message, "invalid_request_error", "invalid_realtime_client_secret");
    }
    let Some(authorization) = authorization else {
        return realtime_auth_error(401, "Realtime client secret is invalid or expired", "invalid_request_error", "invalid_realtime_client_secret");
    };
    let principal = if authorization.issuer_principal.is_empty() { authorization.principal.clone() } else { authorization.issuer_principal.clone() };
    let provider = if authorization.issuer_provider.is_empty() { "realtime-client-secret".to_string() } else { authorization.issuer_provider.clone() };
    req.extensions_mut().insert(SecretIdentity {
        principal,
        provider,
        secret: Some(ClientSecretCaller { principal: authorization.principal, session: authorization.session }),
    });
    next.run(req).await
}

// ---- handlers (the live handler arrives as an extension)

pub async fn live_call(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    req: Request,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::call(&live, &caller, parts(&info, None), req.into_body()).await
}

pub async fn live_sideband(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    Path(call_id): Path<String>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::sideband(&live, &caller, parts(&info, Some(call_id)), ws.ok()).await
}

async fn realtime_websocket(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::realtime_websocket(&live, &caller, parts(&info, None), ws.ok()).await
}

async fn client_secret(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    req: Request,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::client_secret(&live, &caller, req.into_body()).await
}

async fn legacy_session(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    req: Request,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::legacy_session(&live, &caller, req.into_body()).await
}

async fn hangup(
    Extension(live): Extension<Handler>,
    identity: Option<Extension<SecretIdentity>>,
    info: ReqInfo,
    Path(call_id): Path<String>,
    req: Request,
) -> Response {
    let caller = caller(&info, identity.map(|e| e.0));
    endpoints::hangup(&live, &caller, parts(&info, Some(call_id)), req.into_body()).await
}

async fn transcription_session(Extension(live): Extension<Handler>) -> Response {
    endpoints::transcription_session(&live)
}

async fn translation(Extension(live): Extension<Handler>) -> Response {
    endpoints::translation(&live)
}

async fn sip_control(Extension(live): Extension<Handler>, info: ReqInfo) -> Response {
    endpoints::sip_control(&live, &info.path)
}

/// `/v1/realtime*` routes with their auth choices (`realtimeAuth` vs `standardAuth`).
pub fn routes(state: &AppState, live: &Handler) -> Router<AppState> {
    let rt_auth = || from_fn_with_state((state.clone(), live.clone()), secret_or_standard_auth);
    let std_auth = || from_fn_with_state(state.clone(), standard_auth);

    let realtime = Router::new()
        .route("/v1/realtime", get(realtime_websocket).post(live_call))
        .route("/v1/realtime/calls", post(live_call))
        .route("/v1/realtime/calls/{call_id}", get(live_sideband))
        .route("/v1/realtime/translations", get(translation).post(translation))
        .route_layer(rt_auth());
    let standard = Router::new()
        .route("/v1/realtime/client_secrets", post(client_secret))
        .route("/v1/realtime/sessions", post(legacy_session))
        .route("/v1/realtime/transcription_sessions", post(transcription_session))
        .route("/v1/realtime/translations/client_secrets", post(translation))
        .route("/v1/realtime/calls/{call_id}/hangup", post(hangup))
        .route("/v1/realtime/calls/{call_id}/accept", post(sip_control))
        .route("/v1/realtime/calls/{call_id}/reject", post(sip_control))
        .route("/v1/realtime/calls/{call_id}/refer", post(sip_control))
        .route_layer(std_auth());
    realtime.merge(standard).layer(Extension(live.clone()))
}

/// Registered `(method, pattern)` pairs for the trailing-slash redirect table.
pub const ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/live"),
    ("GET", "/v1/live/:call_id"),
    ("GET", "/v1/realtime"),
    ("POST", "/v1/realtime"),
    ("POST", "/v1/realtime/calls"),
    ("GET", "/v1/realtime/calls/:call_id"),
    ("POST", "/v1/realtime/client_secrets"),
    ("POST", "/v1/realtime/sessions"),
    ("POST", "/v1/realtime/transcription_sessions"),
    ("GET", "/v1/realtime/translations"),
    ("POST", "/v1/realtime/translations"),
    ("POST", "/v1/realtime/translations/client_secrets"),
    ("POST", "/v1/realtime/calls/:call_id/hangup"),
    ("POST", "/v1/realtime/calls/:call_id/accept"),
    ("POST", "/v1/realtime/calls/:call_id/reject"),
    ("POST", "/v1/realtime/calls/:call_id/refer"),
];
