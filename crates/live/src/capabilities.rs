//! Capability stubs and call hangup (Go: capabilities.go).

use axum::body::Body;
use cpa_executors::helps::logging::UpstreamRequestLog;
use http::{HeaderValue, Method};

use crate::call::auth_account_type;
use crate::endpoints::{BodyReadError, read_body};
use crate::reply::{Reply, capability_not_supported, realtime_error};
use crate::upstream::{
    BodyRead, MAX_BODY_SIZE, call_response_headers, copy_handshake_headers, headers_for_logging, prepare_request_headers,
    protocol_headers, read_limited, send, set_account_header,
};
use crate::util::is_call_id;
use crate::{Caller, Handler, RequestParts, live_selection_headers, selection_error};

impl Handler {
    /// `HandleTranslation`.
    pub fn handle_translation(&self) -> Reply {
        capability_not_supported("Realtime translation sessions")
    }

    /// `HandleTranscriptionSession`.
    pub fn handle_transcription_session(&self) -> Reply {
        capability_not_supported("Realtime transcription-only sessions")
    }

    /// `HandleSIPControl`: the action is the last path segment.
    pub fn handle_sip_control(&self, path: &str) -> Reply {
        let action = path.trim_matches('/').rsplit('/').next().map(str::trim).filter(|a| !a.is_empty()).unwrap_or("control");
        capability_not_supported(&format!("Realtime SIP {action}"))
    }

    /// `HandleHangup`: forwards the hangup of a local call using its pinned OAuth credential. The
    /// request body is only read once the call id, owner and credential checks passed.
    pub async fn handle_hangup(&self, caller: &Caller, parts: &RequestParts, body: Body) -> Reply {
        let call_id = parts.call_id.as_deref().unwrap_or("").trim().to_string();
        if !is_call_id(&call_id) {
            return realtime_error(400, "Invalid Realtime call ID", "invalid_request_error", "invalid_call_id");
        }
        let Some(session) = self.inner.sessions.peek(&call_id) else {
            return realtime_error(404, "Realtime call not found", "invalid_request_error", "realtime_call_not_found");
        };
        let (owner_principal, owner_provider) = caller.owner();
        if !session.owner_principal.is_empty() && (owner_principal != session.owner_principal || owner_provider != session.owner_provider) {
            return realtime_error(
                403,
                "Realtime call belongs to another API principal",
                "invalid_request_error",
                "realtime_call_scope_mismatch",
            );
        }
        let mut selected = match self.select_oauth(&live_selection_headers(parts, caller), &[], Some((&session.auth_id, &call_id))) {
            Ok(a) => a,
            Err(e) => return selection_error(&parts.path, &e),
        };
        caller.record_trace(&mut selected);

        let body = match read_body(body, MAX_BODY_SIZE).await {
            Ok(b) => b,
            Err(BodyReadError::TooLarge) => {
                return realtime_error(400, crate::call::ERR_BODY_TOO_LARGE, "invalid_request_error", "invalid_request");
            }
            Err(BodyReadError::Failed(e)) => {
                return realtime_error(400, &format!("failed to read Codex live request: {e}"), "invalid_request_error", "invalid_request");
            }
        };
        let upstream_url = format!("{}/realtime/calls/{}/hangup", self.realtime_http_base_url(), call_id);
        let mut headers = protocol_headers(&parts.headers);
        let content_type = parts.headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string();
        if !content_type.is_empty()
            && let Ok(v) = HeaderValue::from_str(&content_type)
        {
            headers.insert(http::header::CONTENT_TYPE, v);
        }
        set_account_header(&mut headers, &selected);
        prepare_request_headers(&selected, &mut headers);
        let cfg = self.cfg();
        if let Some(log) = caller.log() {
            let (auth_type, auth_value) = auth_account_type(&selected);
            log.request(UpstreamRequestLog {
                url: upstream_url.clone(),
                method: "POST".into(),
                headers: headers_for_logging(&headers),
                body: body.to_vec(),
                provider: "codex".into(),
                auth_id: selected.id.clone(),
                auth_label: selected.label.clone(),
                auth_type,
                auth_value,
            });
        }
        let response = match send(&cfg, &selected, Method::POST, &upstream_url, headers, body).await {
            Ok(r) => r,
            Err(e) => {
                if let Some(log) = caller.log() {
                    log.response_error(&e.message);
                }
                return realtime_error(e.status_or(502), &e.message, "api_error", "realtime_upstream_unavailable");
            }
        };
        if let Some(log) = caller.log() {
            log.response_metadata(response.status, &call_response_headers(&response.headers));
        }
        let status = response.status;
        let response_headers = response.headers.clone();
        let response_body = match read_limited(response.response).await {
            BodyRead::Ok(b) => b,
            BodyRead::TooLarge(b) | BodyRead::Failed(b, _) => {
                if let Some(log) = caller.log() {
                    log.response_chunk(&b);
                }
                return realtime_error(502, "Failed to read Realtime hangup response", "api_error", "realtime_upstream_unavailable");
            }
        };
        if let Some(log) = caller.log() {
            log.response_chunk(&response_body);
        }
        if (200..300).contains(&status) {
            self.inner.sessions.complete(&session, "client_hangup");
        }
        let mut reply = Reply::new(status);
        if let Some(ct) = response_headers.get(http::header::CONTENT_TYPE).filter(|v| !v.is_empty()) {
            reply.headers.insert(http::header::CONTENT_TYPE, ct.clone());
        }
        copy_handshake_headers(&mut reply.headers, &response_headers);
        reply.body = response_body;
        reply
    }
}
