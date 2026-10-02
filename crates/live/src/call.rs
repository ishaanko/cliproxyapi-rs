//! `POST /v1/live`, `/v1/realtime`, `/v1/realtime/calls`: WebRTC call bootstrap (Go: live.go).

use std::sync::Arc;

use bytes::Bytes;
use cpa_executors::helps::logging::UpstreamRequestLog;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde_json::Value;

use crate::media::{MediaRelaySession, MediaRoute};
use crate::multipart::{PartError, Reader};
use crate::reply::{Reply, live_error};
use crate::session::LiveSession;
use crate::upstream::{
    BodyRead, MAX_BODY_SIZE, call_response_headers, headers_for_logging, prepare_request_headers, protocol_headers, read_limited,
    send, set_account_header,
};
use crate::util::{
    DEFAULT_LIVE_MODEL, call_id_from_location, codex_realtime_model, compact_escaped, json_string, marshal_raw_map, media_type,
    model_from_json, parse_media_type, raw_value, unmarshal_raw_map,
};
use crate::{Caller, Handler, RequestParts, live_selection_headers, proxy_url_for_auth, selection_error};

pub(crate) const ERR_BODY_TOO_LARGE: &str = "Codex live request body too large";

fn is_sdp_media(media: &Option<String>) -> bool {
    matches!(media.as_deref(), Some("application/sdp") | Some("text/plain"))
}

fn is_json_media(media: &Option<String>) -> bool {
    media.as_deref() == Some("application/json")
}

/// `prepareCallRequest`: `(body, content type, model)`.
fn prepare_call_request(body: &[u8], content_type: &str) -> Result<(Vec<u8>, String, String), String> {
    if let Some((media, params)) = parse_media_type(content_type)
        && media == "multipart/form-data"
    {
        let boundary = params.iter().find(|(k, _)| k == "boundary").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
        return multipart_call_request(body, &boundary);
    }
    let mut model = model_from_json(body);
    if model.is_empty() {
        model = DEFAULT_LIVE_MODEL.to_string();
    }
    let content_type = if content_type.trim().is_empty() { "application/json" } else { content_type };
    Ok((body.to_vec(), content_type.to_string(), model))
}

/// `applyClientSecretCallSession`: binds the key's session to the call request.
fn apply_client_secret_call_session(
    body: Vec<u8>,
    content_type: String,
    model: String,
    session: &str,
) -> Result<(Vec<u8>, String, String), String> {
    if session.is_empty() {
        return Ok((body, content_type, model));
    }
    let media = media_type(&content_type);
    if is_sdp_media(&media) {
        let encoded = encode_call_request(&String::from_utf8_lossy(&body), Some(session));
        return Ok((encoded.into_bytes(), "application/json".into(), model_from_json(session.as_bytes())));
    }
    if !is_json_media(&media) {
        return Err("Realtime client secrets require an SDP or JSON call request".into());
    }
    let mut payload = unmarshal_raw_map(&body).map_err(|e| format!("failed to decode Realtime call request: {e}"))?;
    payload.insert("session".into(), raw_value(session));
    Ok((marshal_raw_map(&payload).into_bytes(), "application/json".into(), model_from_json(session.as_bytes())))
}

/// `rewriteCallRequestModel`: realtime model aliases become the live model in the JSON body.
fn rewrite_call_request_model(body: Vec<u8>, content_type: &str, model: &str) -> Result<(Vec<u8>, String), String> {
    let upstream_model = codex_realtime_model(model);
    let media = media_type(content_type);
    if !is_json_media(&media) || body.trim_ascii().is_empty() {
        return Ok((body, upstream_model));
    }
    let mut payload = unmarshal_raw_map(&body).map_err(|e| format!("failed to decode Realtime call request: {e}"))?;
    let encoded_model = raw_value(&json_string(&upstream_model));
    let mut changed = false;
    if let Some(session_json) = payload.get("session").filter(|s| !s.get().is_empty()) {
        let mut session = unmarshal_raw_map(session_json.get().as_bytes()).map_err(|e| format!("failed to decode Realtime session: {e}"))?;
        session.insert("model".into(), encoded_model);
        payload.insert("session".into(), raw_value(&marshal_raw_map(&session)));
        changed = true;
    } else if payload.contains_key("model") {
        payload.insert("model".into(), encoded_model);
        changed = true;
    }
    if !changed {
        return Ok((body, upstream_model));
    }
    Ok((marshal_raw_map(&payload).into_bytes(), upstream_model))
}

/// `multipartCallRequest`: `sdp` and `session` fields become a JSON call request.
fn multipart_call_request(body: &[u8], boundary: &str) -> Result<(Vec<u8>, String, String), String> {
    if boundary.is_empty() {
        return Err("Codex live multipart boundary is missing".into());
    }
    let mut reader = Reader::new(body, boundary);
    let mut sdp: Option<String> = None;
    let mut session: Option<String> = None;
    let mut model = String::new();
    loop {
        let part = match reader.next_part() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(PartError::Next(e)) => return Err(format!("failed to parse Codex live multipart body: {e}")),
            Err(PartError::Read(e)) => return Err(format!("failed to read Codex live multipart field: {e}")),
        };
        match part.form_name.as_str() {
            "sdp" => sdp = Some(String::from_utf8_lossy(&part.body).into_owned()),
            "session" => {
                if !cpa_json::valid(&part.body) {
                    return Err("Codex live session field must contain valid JSON".into());
                }
                model = model_from_json(&part.body);
                session = Some(String::from_utf8_lossy(&part.body).into_owned());
            }
            _ => {}
        }
    }
    let Some(sdp) = sdp else {
        return Err("Codex live multipart body requires an sdp field".into());
    };
    if model.is_empty() {
        model = DEFAULT_LIVE_MODEL.to_string();
    }
    Ok((encode_call_request(&sdp, session.as_deref()).into_bytes(), "application/json".into(), model))
}

/// `encodeCallRequest`: `{"sdp":..,"session":..}` (session omitted when empty).
fn encode_call_request(sdp: &str, session: Option<&str>) -> String {
    match session.filter(|s| !s.is_empty()) {
        Some(session) => format!(r#"{{"sdp":{},"session":{}}}"#, json_string(sdp), compact_escaped(session)),
        None => format!(r#"{{"sdp":{}}}"#, json_string(sdp)),
    }
}

/// Decodes `struct{SDP string}` from a JSON body with Go's error texts.
fn decode_sdp_field(body: &[u8]) -> Result<String, String> {
    crate::go_json::check_valid(body)?;
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Err("invalid JSON".into());
    };
    let obj = match value {
        Value::Null => return Ok(String::new()),
        Value::Object(o) => o,
        other => {
            let kind = match other {
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                _ => "array",
            };
            return Err(format!("json: cannot unmarshal {kind} into Go value of type struct {{ SDP string \"json:\\\"sdp\\\"\" }}"));
        }
    };
    let mut sdp = String::new();
    for (key, value) in &obj {
        if !key.eq_ignore_ascii_case("sdp") {
            continue;
        }
        match value {
            Value::String(s) => sdp = s.clone(),
            Value::Null => {}
            other => {
                let kind = match other {
                    Value::Bool(_) => "bool",
                    Value::Number(_) => "number",
                    Value::Object(_) => "object",
                    _ => "array",
                };
                return Err(format!("json: cannot unmarshal {kind} into Go struct field .sdp of type string"));
            }
        }
    }
    Ok(sdp)
}

/// `callRequestSDP`: the client's SDP offer from an SDP or JSON request.
fn call_request_sdp(body: &[u8], content_type: &str) -> Result<String, String> {
    let media = media_type(content_type);
    if is_sdp_media(&media) {
        if String::from_utf8_lossy(body).trim().is_empty() {
            return Err("Codex live call request requires an SDP offer".into());
        }
        return Ok(String::from_utf8_lossy(body).into_owned());
    }
    if !is_json_media(&media) {
        return Err("Codex live media relay requires an SDP or JSON call request".into());
    }
    let sdp = decode_sdp_field(body).map_err(|e| format!("failed to decode Codex live call request: {e}"))?;
    if sdp.trim().is_empty() {
        return Err("Codex live call request requires an SDP offer".into());
    }
    Ok(sdp)
}

/// `replaceCallRequestSDP`.
fn replace_call_request_sdp(body: &[u8], content_type: &str, sdp: &str) -> Result<(Vec<u8>, String), String> {
    let media = media_type(content_type);
    if is_sdp_media(&media) {
        return Ok((encode_call_request(sdp, None).into_bytes(), "application/json".into()));
    }
    if !is_json_media(&media) {
        return Err("Codex live media relay requires an SDP or JSON call request".into());
    }
    let mut payload = unmarshal_raw_map(body).map_err(|e| format!("failed to decode Codex live call request: {e}"))?;
    payload.insert("sdp".into(), raw_value(&json_string(sdp)));
    Ok((marshal_raw_map(&payload).into_bytes(), "application/json".into()))
}

/// `callResponseSDP`: the upstream SDP answer.
fn call_response_sdp(body: &[u8], content_type: &str) -> Result<String, String> {
    if is_json_media(&media_type(content_type)) {
        let sdp = decode_sdp_field(body).map_err(|e| format!("failed to decode Codex live response: {e}"))?;
        if sdp.trim().is_empty() {
            return Err("Codex live response requires an SDP answer".into());
        }
        return Ok(sdp);
    }
    if String::from_utf8_lossy(body).trim().is_empty() {
        return Err("Codex live response requires an SDP answer".into());
    }
    Ok(String::from_utf8_lossy(body).into_owned())
}

/// `mediaCredentialName`: label, else file base name, else the auth index.
fn media_credential_name(auth: &cpa_auth::Auth, auth_index: &str) -> String {
    if !auth.label.trim().is_empty() {
        return auth.label.trim().to_string();
    }
    if !auth.file_name.trim().is_empty() {
        let base = std::path::Path::new(auth.file_name.trim()).file_name().map(|n| n.to_string_lossy().trim().to_string()).unwrap_or_default();
        if !base.is_empty() && base != "." {
            return base;
        }
    }
    auth_index.trim().to_string()
}

/// Closes a media session that the request did not hand over to the session store.
struct MediaGuard {
    session: Option<Arc<dyn MediaRelaySession>>,
    retained: bool,
}

impl Drop for MediaGuard {
    fn drop(&mut self) {
        if let (Some(session), false) = (&self.session, self.retained) {
            session.close_with_reason("request_not_retained");
        }
    }
}

impl Handler {
    /// `Handle`: forwards the SDP bootstrap to the Codex realtime calls endpoint.
    pub async fn handle_call(&self, caller: &Caller, parts: &RequestParts, body: Bytes) -> Reply {
        let path = parts.path.as_str();
        if body.len() > MAX_BODY_SIZE {
            return live_error(path, 413, ERR_BODY_TOO_LARGE);
        }
        let content_type = parts.headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let prepared = prepare_call_request(&body, &content_type).and_then(|(b, ct, m)| match &caller.client_secret {
            Some(secret) => apply_client_secret_call_session(b, ct, m, &secret.session),
            None => Ok((b, ct, m)),
        });
        let prepared = prepared.and_then(|(b, ct, m)| rewrite_call_request_model(b, &ct, &m).map(|(b, m)| (b, ct, m)));
        let (upstream_body, upstream_content_type, model) = match prepared {
            Ok(v) => v,
            Err(e) => return live_error(path, 400, &e),
        };
        let (cfg, media_relay, media_relay_err) = self.current_runtime();
        if let Some(e) = media_relay_err {
            return live_error(path, 503, &e);
        }
        let mut selected = match self.select_oauth(&live_selection_headers(parts, caller), &body, None) {
            Ok(a) => a,
            Err(e) => return selection_error(path, &e),
        };
        caller.record_trace(&mut selected);
        let selected_index = selected.ensure_index();

        let mut upstream_body = Bytes::from(upstream_body);
        let mut upstream_content_type = upstream_content_type;
        let mut guard = MediaGuard { session: None, retained: false };
        if let Some(relay) = media_relay {
            let client_offer = match call_request_sdp(&upstream_body, &upstream_content_type) {
                Ok(o) => o,
                Err(e) => return live_error(path, 400, &e),
            };
            let route = MediaRoute {
                proxy_url: proxy_url_for_auth(&cfg, &selected),
                credential: media_credential_name(&selected, &selected_index),
                auth_index: selected_index.clone(),
            };
            let (session, upstream_offer) = match relay.new_session(&client_offer, route).await {
                Ok(v) => v,
                Err(e) => return live_error(path, if e.status > 0 { e.status } else { 502 }, &e.message),
            };
            guard.session = Some(session);
            match replace_call_request_sdp(&upstream_body, &upstream_content_type, &upstream_offer) {
                Ok((b, ct)) => (upstream_body, upstream_content_type) = (Bytes::from(b), ct),
                Err(e) => return live_error(path, 400, &e),
            }
        }

        let call_url = self.inner.call_url.read().clone();
        let mut headers = protocol_headers(&parts.headers);
        if let Ok(v) = HeaderValue::from_str(&upstream_content_type) {
            headers.insert(http::header::CONTENT_TYPE, v);
        }
        set_account_header(&mut headers, &selected);
        prepare_request_headers(&selected, &mut headers);
        if let Some(log) = caller.log() {
            log.request(UpstreamRequestLog {
                url: call_url.clone(),
                method: "POST".into(),
                headers: headers_for_logging(&headers),
                body: upstream_body.to_vec(),
                provider: "codex".into(),
                auth_id: selected.id.clone(),
                auth_label: selected.label.clone(),
                auth_type: auth_account_type(&selected).0,
                auth_value: auth_account_type(&selected).1,
            });
        }
        let response = match send(&cfg, &selected, Method::POST, &call_url, headers, upstream_body).await {
            Ok(r) => r,
            Err(e) => {
                if let Some(log) = caller.log() {
                    log.response_error(&e);
                }
                return live_error(path, 502, &e);
            }
        };
        let mut response_headers = call_response_headers(&response.headers);
        if let Some(log) = caller.log() {
            log.response_metadata(response.status, &response_headers);
        }
        let upstream_ct = response.headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let location = response.headers.get(http::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let status = response.status;
        let response_body = match read_limited(response.response).await {
            BodyRead::Ok(b) => b,
            BodyRead::TooLarge(b) => {
                if let Some(log) = caller.log() {
                    log.response_chunk(&b);
                    log.response_error(ERR_BODY_TOO_LARGE);
                }
                return live_error(path, 502, "Codex live response body too large");
            }
            BodyRead::Failed(b, e) => {
                if let Some(log) = caller.log() {
                    log.response_chunk(&b);
                    log.response_error(&e);
                }
                return live_error(path, 502, "Failed to read Codex live response");
            }
        };
        if let Some(log) = caller.log() {
            log.response_chunk(&response_body);
        }
        let mut body_to_write = response_body.clone();
        let success = (200..300).contains(&status);
        let mut call_id = String::new();
        if success {
            call_id = call_id_from_location(&location);
            if call_id.is_empty() && guard.session.is_some() {
                return live_error(path, 502, "Codex live response is missing a valid call ID");
            }
            if let Some(session) = &guard.session {
                session.set_call_id(&call_id);
            }
            if !call_id.is_empty() && path.starts_with("/v1/realtime") {
                response_headers.insert(http::header::LOCATION, header_value(&format!("/v1/realtime/calls/{call_id}")));
            }
        }
        if success && let Some(session) = guard.session.clone() {
            let upstream_answer = match call_response_sdp(&response_body, &upstream_ct) {
                Ok(a) => a,
                Err(e) => return live_error(path, 502, &e),
            };
            let downstream_answer = match session.accept_upstream_answer(&upstream_answer).await {
                Ok(a) => a,
                Err(e) => return live_error(path, if e.status > 0 { e.status } else { 502 }, &e.message),
            };
            body_to_write = Bytes::from(downstream_answer);
            response_headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
        }
        if success && !call_id.is_empty() {
            let (owner_principal, owner_provider) = caller.owner();
            let session = LiveSession {
                auth_id: selected.id.clone(),
                model: model.clone(),
                media: guard.session.clone(),
                owner_principal,
                owner_provider,
                client_secret_principal: caller.client_secret.as_ref().map(|c| c.principal.clone()).unwrap_or_default(),
                ..Default::default()
            };
            let stored = self.inner.sessions.put(&call_id, session);
            if let Some(media) = &guard.session {
                let store = self.inner.sessions.clone();
                let stored = stored.clone();
                media.set_close_handler(Box::new(move |reason| store.complete(&stored, reason)));
                guard.retained = true;
            }
        }
        let mut reply = Reply::new(status);
        reply.headers = response_headers;
        reply.body = body_to_write;
        reply
    }
}

fn header_value(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// `Auth.AccountInfo()` as `(type, value)` for request logs.
pub(crate) fn auth_account_type(auth: &cpa_auth::Auth) -> (String, String) {
    if let Some(key) = auth.attributes.get("api_key").filter(|k| !k.trim().is_empty()) {
        return ("api_key".into(), key.clone());
    }
    let email = auth.metadata.get("email").and_then(Value::as_str).unwrap_or("").to_string();
    ("oauth".into(), email)
}

#[allow(dead_code)]
fn _assert_types(_: &HeaderMap, _: &HeaderName) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_session_and_top_level_model() {
        let (out, model) = rewrite_call_request_model(br#"{"session":{"model":"gpt-realtime","x":1},"sdp":"v"}"#.to_vec(), "application/json", "gpt-realtime").unwrap();
        assert_eq!(model, DEFAULT_LIVE_MODEL);
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"sdp":"v","session":{"model":"gpt-live-1-codex","x":1}}"#);
        let (out, _) = rewrite_call_request_model(br#"{"model":"gpt-realtime-mini"}"#.to_vec(), "application/json", "gpt-realtime-mini").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"model":"gpt-live-1-codex"}"#);
        let (out, model) = rewrite_call_request_model(b"v=0".to_vec(), "application/sdp", "other").unwrap();
        assert_eq!((out, model.as_str()), (b"v=0".to_vec(), "other"));
        assert!(rewrite_call_request_model(b"{x".to_vec(), "application/json", "m").unwrap_err().starts_with("failed to decode Realtime call request: invalid character"));
    }

    #[test]
    fn multipart_becomes_json() {
        let body = "--b\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n\r\n--b\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{\"model\": \"m\"}\r\n--b--\r\n";
        let (out, ct, model) = prepare_call_request(body.as_bytes(), "multipart/form-data; boundary=b").unwrap();
        assert_eq!((ct.as_str(), model.as_str()), ("application/json", "m"));
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"sdp":"v=0\r\n","session":{"model":"m"}}"#);
        assert_eq!(
            prepare_call_request(b"", "multipart/form-data; boundary=b").unwrap_err(),
            "Codex live multipart body requires an sdp field"
        );
    }
}
