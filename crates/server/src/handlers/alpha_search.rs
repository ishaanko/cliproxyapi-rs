//! `POST /v1/alpha/search` and `/backend-api/codex/alpha/search` (Go: internal/api
//! server_routes.go `codexAlphaSearch`): the Codex standalone search endpoint is forwarded to the
//! upstream with a Codex credential picked under the `codex_alpha_search_v1` policy. The payload
//! is already in Codex format, so no translator runs and the upstream answer (status, body,
//! `Content-Type`) is relayed as is.
//!
//! Model-router plugins and Home dispatch of the Go handler are not ported.

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_core::format::Format;
use cpa_core::util::go_json_string;
use cpa_executors::helps::logging::UpstreamRequestLog;
use cpa_json::J;
use cpa_runtime::conductor::CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1;
use cpa_runtime::executor::Options;
use serde_json::{Map, Value};

use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;

const MAX_REQUEST_BYTES: usize = 16 << 20;
const MAX_RESPONSE_BYTES: usize = 32 << 20;
const DEFAULT_UPSTREAM_URL: &str = "https://chatgpt.com/backend-api/codex/alpha/search";
const MISSING_BASE_URL: &str = "Codex Alpha Search API key base URL unavailable";

/// `c.JSON(status, gin.H{"error": message})`.
fn error_reply(status: u16, message: &str) -> Reply {
    Reply::json(status, format!(r#"{{"error":{}}}"#, go_json_string(message)).into_bytes())
}

/// `json.Marshal` of raw JSON text: Go's compaction escapes `<`, `>` and `&` (and U+2028/9).
fn escape_html(raw: &str) -> String {
    raw.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

/// Re-encodes a top-level JSON object the way Go does for `map[string]json.RawMessage`: keys
/// sorted, values kept as they were.
fn marshal_raw_map(map: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let fields: Vec<String> = keys.iter().map(|k| format!("{}:{}", go_json_string(k), escape_html(&map[*k].to_string()))).collect();
    format!("{{{}}}", fields.join(","))
}

/// `sanitizeCodexAlphaSearchBody`: drops `prompt_cache_key` and `prompt_cache_retention`; the
/// body is untouched when neither is present or it is not a JSON object.
fn sanitize_body(body: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let mut removed = false;
    for field in ["prompt_cache_key", "prompt_cache_retention"] {
        removed |= payload.shift_remove(field).is_some();
    }
    if !removed {
        return body.to_vec();
    }
    marshal_raw_map(&payload).into_bytes()
}

/// `rewriteCodexAlphaSearchModel`: replaces a present top-level `model` with `upstream_model`.
fn rewrite_model(body: &[u8], upstream_model: &str) -> Vec<u8> {
    let upstream_model = upstream_model.trim();
    if upstream_model.is_empty() {
        return body.to_vec();
    }
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(current) = payload.get("model") else {
        return body.to_vec();
    };
    if current.as_str() == Some(upstream_model) {
        return body.to_vec();
    }
    payload.insert("model".into(), Value::String(upstream_model.to_string()));
    marshal_raw_map(&payload).into_bytes()
}

/// Reads a header value like `c.GetHeader` followed by `TrimSpace`.
fn trimmed_header(headers: &HeaderMap, name: &str) -> String {
    crate::headers::header_trimmed(headers, name)
}

/// `codexAlphaSearch`.
pub async fn alpha_search(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let body = if body.len() > MAX_REQUEST_BYTES { body.slice(..MAX_REQUEST_BYTES) } else { body };
    let routing = cpa_json::parse(&body);
    let routing_id = routing.g("id").str();
    let routing_model = routing.g("model").str();
    let upstream_body = sanitize_body(&body);

    let mut selection_headers = info.headers.clone();
    let session_id = routing_id.trim();
    if !session_id.is_empty()
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        selection_headers.insert(HeaderName::from_static("x-session-id"), value);
    }
    let selection_model = routing_model.trim().to_string();
    let mut opts = Options::new(Format::OpenAI);
    opts.headers = selection_headers;
    opts.original_request = body.clone();

    let mut selected = match st.manager.select_auth_with_credential_policy("codex", &selection_model, CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1, &opts) {
        Ok(auth) => auth,
        Err(err) => {
            let status = if err.status > 0 { err.status } else { 503 };
            let mut reply = error_reply(status, &err.message);
            for value in cpa_runtime::conductor::errors::safe_response_headers(&err).get_all(header::RETRY_AFTER) {
                reply.headers.append(header::RETRY_AFTER, value.clone());
            }
            return reply.into_response();
        }
    };
    info.trace.record(&selected.ensure_index(), &info.request_id);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(HeaderName::from_static("originator"), HeaderValue::from_static("codex_cli_rs"));
    for name in ["Version", "User-Agent", "Session_id", "X-Client-Request-Id"] {
        let value = trimmed_header(&info.headers, name);
        if !value.is_empty()
            && let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value))
        {
            headers.insert(name, value);
        }
    }

    let route_model = if selection_model.is_empty() { routing_model.trim().to_string() } else { selection_model };
    if let Some(account_id) = selected.metadata.get("account_id").and_then(Value::as_str).filter(|a| !a.trim().is_empty())
        && let Ok(value) = HeaderValue::from_str(account_id)
    {
        headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
    }
    let mut url = DEFAULT_UPSTREAM_URL.to_string();
    let mut request_body = upstream_body;
    // API-key credentials resolve the upstream model and use their own base URL.
    if selected.auth_kind() == AUTH_KIND_API_KEY {
        let base_url = selected.attributes.get("base_url").map(|b| b.trim().to_string()).unwrap_or_default();
        if base_url.is_empty() {
            return error_reply(503, MISSING_BASE_URL).into_response();
        }
        url = format!("{}/alpha/search", base_url.trim_end_matches('/'));
        let upstream_model = st.manager.resolve_execution_model(&selected, &route_model);
        if !upstream_model.is_empty() {
            request_body = rewrite_model(&request_body, &upstream_model);
        }
    }
    // The Codex executor injects its credential and custom headers (`NewHttpRequest`), the
    // upstream call goes through its HTTP client (`HttpRequest`).
    let cfg = st.cfg();
    let upstream_log = info.api_log.exec_handle();
    let body_for_log = request_body.clone();
    let request = match st.manager.new_http_request(&selected, "POST", &url, Some(Bytes::from(request_body)), Some(&headers)).await {
        Ok(request) => request,
        Err(err) => {
            upstream_log.record_api_response_error(&cfg, &err.message);
            return error_reply(if err.status > 0 { err.status } else { 502 }, &err.message).into_response();
        }
    };
    let (auth_type, auth_value) = selected.account_info();
    upstream_log.record_api_request(
        &cfg,
        UpstreamRequestLog {
            url: url.clone(),
            method: "POST".to_string(),
            headers: request.headers().clone(),
            body: body_for_log,
            provider: "codex".to_string(),
            auth_id: selected.id.clone(),
            auth_label: selected.label.clone(),
            auth_type: auth_type.to_string(),
            auth_value,
        },
    );
    let resp = match st.manager.http_request(&selected, request).await {
        Ok(resp) => resp,
        Err(err) => {
            upstream_log.record_api_response_error(&cfg, &err.message);
            info.api_log.record_error(502, &err.message);
            return error_reply(if err.status > 0 { err.status } else { 502 }, &err.message).into_response();
        }
    };
    let status = resp.status().as_u16();
    upstream_log.record_api_response_metadata(&cfg, status, resp.headers());
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let upstream = match resp.bytes().await {
        Ok(b) => b,
        Err(err) => {
            upstream_log.record_api_response_error(&cfg, &err.to_string());
            info.api_log.record_error(502, &err.to_string());
            return error_reply(502, "Failed to read Codex search response").into_response();
        }
    };
    let upstream = if upstream.len() > MAX_RESPONSE_BYTES { upstream.slice(..MAX_RESPONSE_BYTES) } else { upstream };
    upstream_log.append_api_response_chunk(&cfg, &upstream);
    let mut reply = Reply::new(status).with_body(upstream);
    if let Some(ct) = content_type.filter(|v| !v.is_empty()) {
        reply.headers.insert(header::CONTENT_TYPE, ct);
    }
    reply.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_cache_fields_and_sorts_keys() {
        let out = sanitize_body(br#"{"model":"m","prompt_cache_key":"k","a":{"z":1,"b":"<x>"},"prompt_cache_retention":"24h"}"#);
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"a":{"z":1,"b":"\u003cx\u003e"},"model":"m"}"#);
        let untouched = br#"{"model": "m"}"#;
        assert_eq!(sanitize_body(untouched), untouched);
        assert_eq!(sanitize_body(b"not json"), b"not json");
    }

    #[test]
    fn model_rewrite_only_when_present_and_different() {
        assert_eq!(String::from_utf8(rewrite_model(br#"{"model":"alias","q":1}"#, " real ")).unwrap(), r#"{"model":"real","q":1}"#);
        let same = br#"{"model":"real"}"#;
        assert_eq!(rewrite_model(same, "real"), same);
        let missing = br#"{"q":1}"#;
        assert_eq!(rewrite_model(missing, "real"), missing);
        assert_eq!(rewrite_model(same, " "), same);
    }
}
