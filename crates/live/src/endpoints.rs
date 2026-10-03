//! axum-facing entry points: body reading and `Response` conversion around the [`Handler`]
//! methods. Route registration and authentication stay with the server.

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use http::HeaderMap;

use crate::client_secret::CLIENT_SECRET_MAX_BODY;
use crate::reply::{live_error, realtime_error};
use crate::upstream::MAX_BODY_SIZE;
use crate::{Caller, Handler, RequestParts};

/// Why a request body could not be read.
pub enum BodyReadError {
    TooLarge,
    Failed(String),
}

/// `readLimitedBody` for a request: more than `limit` bytes is [`BodyReadError::TooLarge`].
pub async fn read_body(body: Body, limit: usize) -> Result<Bytes, BodyReadError> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                buf.extend_from_slice(&chunk);
                if buf.len() > limit {
                    return Err(BodyReadError::TooLarge);
                }
            }
            Err(e) => return Err(BodyReadError::Failed(crate::upstream::error_chain(&e))),
        }
    }
    Ok(Bytes::from(buf))
}

impl RequestParts {
    /// Parts for a request: `raw_query` is the undecoded query string.
    pub fn new(path: &str, raw_query: &str, headers: &HeaderMap, call_id: Option<String>) -> Self {
        let query = url::form_urlencoded::parse(raw_query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        RequestParts { path: path.to_string(), query, headers: headers.clone(), call_id }
    }
}

/// `POST /v1/live`, `/v1/realtime`, `/v1/realtime/calls`.
pub async fn call(handler: &Handler, caller: &Caller, parts: RequestParts, body: Body) -> Response {
    let body = match read_body(body, MAX_BODY_SIZE).await {
        Ok(b) => b,
        Err(BodyReadError::TooLarge) => return live_error(&parts.path, 413, crate::call::ERR_BODY_TOO_LARGE).into_response(),
        Err(BodyReadError::Failed(e)) => {
            return live_error(&parts.path, 400, &format!("failed to read Codex live request: {e}")).into_response();
        }
    };
    handler.handle_call(caller, &parts, body).await.into_response()
}

/// `POST /v1/realtime/calls/:call_id/hangup`.
pub async fn hangup(handler: &Handler, caller: &Caller, parts: RequestParts, body: Body) -> Response {
    let body = match read_body(body, MAX_BODY_SIZE).await {
        Ok(b) => b,
        Err(BodyReadError::TooLarge) => {
            return realtime_error(400, crate::call::ERR_BODY_TOO_LARGE, "invalid_request_error", "invalid_request").into_response();
        }
        Err(BodyReadError::Failed(e)) => {
            return realtime_error(400, &format!("failed to read Codex live request: {e}"), "invalid_request_error", "invalid_request")
                .into_response();
        }
    };
    handler.handle_hangup(caller, &parts, body).await.into_response()
}

async fn read_secret_body(body: Body) -> Result<Bytes, Response> {
    read_body(body, CLIENT_SECRET_MAX_BODY).await.map_err(|e| {
        match e {
            BodyReadError::TooLarge => realtime_error(413, crate::call::ERR_BODY_TOO_LARGE, "invalid_request_error", "invalid_request"),
            BodyReadError::Failed(e) => realtime_error(
                400,
                &format!("failed to read Realtime client secret request: {e}"),
                "invalid_request_error",
                "invalid_request",
            ),
        }
        .into_response()
    })
}

/// `POST /v1/realtime/client_secrets`.
pub async fn client_secret(handler: &Handler, caller: &Caller, body: Body) -> Response {
    match read_secret_body(body).await {
        Ok(b) => handler.create_client_secret(caller, &b).into_response(),
        Err(r) => r,
    }
}

/// `POST /v1/realtime/sessions` (deprecated).
pub async fn legacy_session(handler: &Handler, caller: &Caller, body: Body) -> Response {
    match read_secret_body(body).await {
        Ok(b) => handler.create_legacy_session(caller, &b).into_response(),
        Err(r) => r,
    }
}

/// `GET /v1/live/:call_id` and `/v1/realtime/calls/:call_id`.
pub async fn sideband(handler: &Handler, caller: &Caller, parts: RequestParts, ws: Option<WebSocketUpgrade>) -> Response {
    handler.handle_sideband(caller, &parts, ws).await
}

/// `GET /v1/realtime`.
pub async fn realtime_websocket(handler: &Handler, caller: &Caller, parts: RequestParts, ws: Option<WebSocketUpgrade>) -> Response {
    handler.handle_realtime_websocket(caller, &parts, ws).await
}

pub fn transcription_session(handler: &Handler) -> Response {
    handler.handle_transcription_session().into_response()
}

pub fn translation(handler: &Handler) -> Response {
    handler.handle_translation().into_response()
}

pub fn sip_control(handler: &Handler, path: &str) -> Response {
    handler.handle_sip_control(path).into_response()
}
