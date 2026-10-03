//! `POST /v1/alpha/search` and `/backend-api/codex/alpha/search` (Go: internal/api
//! server_routes.go `codexAlphaSearch`): the Codex standalone search endpoint is forwarded to the
//! upstream with a Codex credential picked under the `codex_alpha_search_v1` policy. The payload
//! is already in Codex format, so no translator runs and the upstream answer (status, body,
//! `Content-Type`) is relayed as is.
//!
//! Model-router plugins and Home dispatch of the Go handler are not ported.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_core::format::Format;
use cpa_core::util::go_json_string;
use cpa_executors::helps::logging::UpstreamRequestLog;
use cpa_runtime::conductor::CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1;
use cpa_runtime::executor::Options;
use http_body_util::BodyExt;
use serde_json::Value;
use serde_json::value::RawValue;

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

/// `json.Marshal` of a `json.RawMessage`: `compact` with HTML escaping. Whitespace outside
/// strings is dropped and `<`, `>`, `&`, U+2028 and U+2029 inside strings are `\u`-escaped;
/// everything else (number spelling, string escapes) is kept as written.
fn compact_raw(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if !in_string {
            match c {
                ' ' | '\t' | '\r' | '\n' => {}
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                _ => out.push(c),
            }
            continue;
        }
        if escaped {
            escaped = false;
            out.push(c);
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                out.push(c);
            }
            '"' => {
                in_string = false;
                out.push(c);
            }
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out
}

/// Re-encodes a top-level JSON object the way Go does for `map[string]json.RawMessage`: keys
/// sorted, values kept as they were written (compacted).
fn marshal_raw_map(map: &BTreeMap<String, &str>) -> String {
    let fields: Vec<String> = map.iter().map(|(k, v)| format!("{}:{}", go_json_string(k), compact_raw(v))).collect();
    format!("{{{}}}", fields.join(","))
}

/// `json.Unmarshal(body, &map[string]json.RawMessage)`; `None` when it fails or yields a nil map
/// (`null`).
fn raw_object(body: &[u8]) -> Option<BTreeMap<String, &str>> {
    let map = serde_json::from_slice::<BTreeMap<String, &RawValue>>(body).ok()?;
    Some(map.into_iter().map(|(k, v)| (k, v.get())).collect())
}

/// `json.Unmarshal` into `struct{ID, Model string}`: keys match case-insensitively, a value that
/// is not a string (or `null`) leaves the field alone, and invalid JSON leaves both empty.
fn routing_fields(body: &[u8]) -> (String, String) {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(body) else {
        return (String::new(), String::new());
    };
    let (mut id, mut model) = (String::new(), String::new());
    for (key, value) in &obj {
        let Some(text) = value.as_str() else { continue };
        if key.eq_ignore_ascii_case("id") {
            id = text.to_string();
        } else if key.eq_ignore_ascii_case("model") {
            model = text.to_string();
        }
    }
    (id, model)
}

/// `io.ReadAll(io.LimitReader(body, limit))`: stops pulling frames once `limit` bytes are in.
async fn read_limited_body(mut body: Body, limit: usize) -> Result<Vec<u8>, axum::Error> {
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(frame) = body.frame().await else { break };
        if let Ok(chunk) = frame?.into_data() {
            let take = chunk.len().min(limit - out.len());
            out.extend_from_slice(&chunk[..take]);
        }
    }
    Ok(out)
}

/// `sanitizeCodexAlphaSearchBody`: drops `prompt_cache_key` and `prompt_cache_retention`; the
/// body is untouched when neither is present or it is not a JSON object.
fn sanitize_body(body: &[u8]) -> Vec<u8> {
    let Some(mut payload) = raw_object(body) else {
        return body.to_vec();
    };
    let mut removed = false;
    for field in ["prompt_cache_key", "prompt_cache_retention"] {
        removed |= payload.remove(field).is_some();
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
    let Some(mut payload) = raw_object(body) else {
        return body.to_vec();
    };
    let Some(current) = payload.get("model") else {
        return body.to_vec();
    };
    let model_json = go_json_string(upstream_model);
    if *current == model_json {
        return body.to_vec();
    }
    payload.insert("model".to_string(), &model_json);
    marshal_raw_map(&payload).into_bytes()
}

/// Reads a header value like `c.GetHeader` followed by `TrimSpace`.
fn trimmed_header(headers: &HeaderMap, name: &str) -> String {
    crate::headers::header_trimmed(headers, name)
}

/// `codexAlphaSearch`.
pub async fn alpha_search(State(st): State<AppState>, info: ReqInfo, body: Body) -> Response {
    let body = match read_limited_body(body, MAX_REQUEST_BYTES).await {
        Ok(body) => Bytes::from(body),
        Err(_) => return error_reply(400, "Failed to read search request").into_response(),
    };
    let (routing_id, routing_model) = routing_fields(&body);
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
    // `NewHttpRequest` injects the Codex executor's credential and custom headers, `HttpRequest`
    // sends through its (Chrome-fingerprinted) client; both are logged like the executor calls.
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
    let mut resp = match st.manager.http_request(&selected, request).await {
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
    // `io.ReadAll(io.LimitReader(resp.Body, 32<<20))`.
    let mut upstream = Vec::new();
    while upstream.len() < MAX_RESPONSE_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(MAX_RESPONSE_BYTES - upstream.len());
                upstream.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(err) => {
                upstream_log.append_api_response_chunk(&cfg, &upstream);
                upstream_log.record_api_response_error(&cfg, &err.to_string());
                info.api_log.record_error(502, &err.to_string());
                return error_reply(502, "Failed to read Codex search response").into_response();
            }
        }
    }
    upstream_log.append_api_response_chunk(&cfg, &upstream);
    let mut reply = Reply::new(status).with_body(Bytes::from(upstream));
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

    #[test]
    fn raw_values_keep_their_spelling() {
        let out = sanitize_body(br#"{"prompt_cache_key":"k","n":1.50,"s":"a\/b\u00e9 &","arr": [1, 2e3]}"#);
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"arr":[1,2e3],"n":1.50,"s":"a\/b\u00e9 \u0026"}"#);
        assert_eq!(sanitize_body(b"null"), b"null");
    }

    #[test]
    fn routing_fields_follow_json_unmarshal() {
        assert_eq!(routing_fields(br#"{"ID":"s1","model":"m"}"#), ("s1".to_string(), "m".to_string()));
        assert_eq!(routing_fields(br#"{"id":5,"model":"m"}"#), (String::new(), "m".to_string()));
        assert_eq!(routing_fields(b"{bad"), (String::new(), String::new()));
    }
}
