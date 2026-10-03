//! Codex live / realtime routes (`/v1/live`, `/v1/realtime*`). The baseline config has no Codex
//! OAuth credential and the upstream endpoints are fixed to the real services, so the scenarios
//! cover everything observable without reaching them: client secrets and legacy sessions, the
//! capability stubs, authentication shapes, request validation, credential selection failures and
//! the sideband/hangup rejections. Flows that need a live upstream call are covered by the
//! `cpa-live` integration tests.

use serde_json::json;

use crate::client::{Auth, HttpReq, Step as Req, WsReq};
use crate::mock::script::{Content, Script};
use crate::scenario::Scenario;

fn ok() -> Script {
    Script::ok(Content::Text)
}

fn post(path: &str) -> HttpReq {
    HttpReq::post(path, json!({}))
}

fn multipart(boundary: &str, parts: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in parts {
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    let mut add = |id: &str, desc: &str, steps: Vec<Req>| out.push(Scenario::new(format!("realtime.{id}"), desc, ok(), steps));

    // ---- client secrets and legacy sessions (values are random and masked as volatile)
    add(
        "secrets.create",
        "client secret for a realtime session with an explicit lifetime",
        vec![HttpReq::post(
            "/v1/realtime/client_secrets",
            json!({"session": {"type": "realtime", "model": "gpt-realtime", "instructions": "help"}, "expires_after": {"anchor": "created_at", "seconds": 60}}),
        )
        .into()],
    );
    add("secrets.create_empty", "empty request object gets the default session", vec![post("/v1/realtime/client_secrets").into()]);
    add(
        "secrets.create_no_body",
        "no body at all",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).raw("").into()],
    );
    add(
        "secrets.model_alias",
        "realtime-preview models keep the client name in the response",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": {"model": "gpt-4o-realtime-preview-2024-12-17", "voice": "alloy"}})).into()],
    );
    add(
        "secrets.session_type_defaults",
        "a session without type or model gets both defaults",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": {"modalities": ["audio", "text"]}})).into()],
    );
    for (id, desc, body) in [
        ("unsupported_type", "transcription sessions are not supported", json!({"session": {"type": "transcription", "model": "gpt-4o-transcribe"}})),
        ("invalid_session", "session must be an object", json!({"session": []})),
        ("lifetime_too_short", "expires_after below the minimum", json!({"expires_after": {"seconds": 5}})),
        ("lifetime_too_long", "expires_after above the maximum", json!({"expires_after": {"seconds": 7201}})),
        ("lifetime_anchor", "unknown anchor", json!({"expires_after": {"anchor": "now", "seconds": 60}})),
        ("lifetime_type", "seconds must be an integer", json!({"expires_after": {"seconds": "60"}})),
        ("array_body", "request must be an object", json!([1, 2])),
    ] {
        add(&format!("secrets.{id}"), desc, vec![HttpReq::post("/v1/realtime/client_secrets", body).into()]);
    }
    add(
        "secrets.html_and_unicode",
        "response escaping of <, >, & and non-ASCII text",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": {"instructions": "a<b&c>d \u{e9}\u{2028}", "voice": "alloy"}})).into()],
    );
    add(
        "secrets.number_formats",
        "session numbers and nesting survive the canonical re-encoding",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!(null))
            .raw(r#"{"session":{"temperature":0.8,"max_response_output_tokens":4096,"n":1e3,"big":12345678901234567890,"tools":[{"type":"function","name":"x","parameters":{"b":1,"a":2}}],"flag":true,"nothing":null}}"#)
            .into()],
    );
    add(
        "secrets.field_types",
        "non-string type and blank model fall back to the defaults",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": {"type": 5, "model": "  "}})).into()],
    );
    add(
        "secrets.duplicate_keys",
        "the last duplicate key wins",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).raw(r#"{"session":{"model":"a","model":"gpt-realtime-mini"}}"#).into()],
    );
    add(
        "secrets.case_insensitive_keys",
        "request members match case-insensitively",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).raw(r#"{"SESSION":{"model":"x"},"Expires_After":{"SECONDS":30}}"#).into()],
    );
    add(
        "secrets.lifetime_float",
        "fractional seconds do not decode",
        vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).raw(r#"{"expires_after":{"seconds":60.5}}"#).into()],
    );
    add("secrets.session_null", "explicit null session", vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": null})).into()]);
    add("secrets.session_string", "string session", vec![HttpReq::post("/v1/realtime/client_secrets", json!({"session": "x"})).into()]);
    add("secrets.null_body", "JSON null body", vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).into()]);
    add("secrets.malformed","malformed JSON", vec![HttpReq::post("/v1/realtime/client_secrets", json!(null)).raw("{broken").into()]);
    add("secrets.missing_key", "no credentials (realtime-shaped error)", vec![post("/v1/realtime/client_secrets").auth(Auth::None).into()]);
    add("secrets.invalid_key", "wrong credentials", vec![post("/v1/realtime/client_secrets").auth(Auth::Bearer("wrong-key")).into()]);
    add("secrets.second_key", "the second configured client key", vec![post("/v1/realtime/client_secrets").auth(Auth::Bearer(crate::config::CLIENT_KEY_2)).into()]);
    add("secrets.get_not_routed", "only POST is registered", vec![HttpReq::get("/v1/realtime/client_secrets").into()]);
    add(
        "sessions.legacy",
        "deprecated sessions endpoint embeds the client secret",
        vec![HttpReq::post("/v1/realtime/sessions", json!({"model": "gpt-realtime", "voice": "alloy", "instructions": "hi"})).into()],
    );
    add("sessions.legacy_empty", "legacy endpoint without a body", vec![HttpReq::post("/v1/realtime/sessions", json!(null)).raw("").into()]);
    add("sessions.legacy_array", "legacy session must be an object", vec![HttpReq::post("/v1/realtime/sessions", json!([])).into()]);
    add("sessions.legacy_unsupported", "legacy transcription session", vec![HttpReq::post("/v1/realtime/sessions", json!({"type": "transcription"})).into()]);

    // ---- capability stubs
    for (id, req) in [
        ("transcription_sessions", post("/v1/realtime/transcription_sessions")),
        ("translations_post", post("/v1/realtime/translations")),
        ("translations_get", HttpReq::get("/v1/realtime/translations")),
        ("translations_client_secrets", post("/v1/realtime/translations/client_secrets")),
        ("sip_accept", post("/v1/realtime/calls/call-123/accept")),
        ("sip_reject", post("/v1/realtime/calls/call-123/reject")),
        ("sip_refer", post("/v1/realtime/calls/call-123/refer")),
    ] {
        add(&format!("stubs.{id}"), "capability is not supported by the Codex OAuth upstream", vec![req.into()]);
    }
    add("stubs.no_key", "stub behind standard auth", vec![post("/v1/realtime/transcription_sessions").auth(Auth::None).into()]);
    add("stubs.translations_no_key", "stub behind realtime auth", vec![HttpReq::get("/v1/realtime/translations").auth(Auth::None).into()]);

    // ---- realtime auth (client secrets or API keys)
    add("auth.call_missing_key", "call bootstrap without credentials", vec![post("/v1/realtime/calls").auth(Auth::None).into()]);
    add("auth.call_invalid_key", "call bootstrap with a wrong key", vec![post("/v1/realtime/calls").auth(Auth::Bearer("wrong-key")).into()]);
    add("auth.unknown_client_secret", "an ek_ bearer that was never issued", vec![post("/v1/realtime/calls").auth(Auth::Bearer("ek_not-issued")).into()]);
    add("auth.live_missing_key", "plain API-key error shape on /v1/live", vec![post("/v1/live").auth(Auth::None).into()]);
    add("auth.live_invalid_key", "wrong key on /v1/live", vec![post("/v1/live").auth(Auth::Bearer("wrong-key")).into()]);
    add("auth.preflight", "CORS preflight on a realtime route", vec![HttpReq::options("/v1/realtime/calls").auth(Auth::None).into()]);
    add("auth.sideband_missing_key", "sideband without credentials", vec![HttpReq::get("/v1/realtime/calls/call-123").auth(Auth::None).into()]);

    // ---- call bootstrap validation and credential selection
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
    for path in ["/v1/live", "/v1/realtime", "/v1/realtime/calls"] {
        let id = path.trim_start_matches("/v1/").replace('/', "_");
        add(
            &format!("call.{id}.no_credential_sdp"),
            "raw SDP offer without a Codex OAuth credential",
            vec![post(path).typed("application/sdp", sdp).into()],
        );
        add(
            &format!("call.{id}.no_credential_json"),
            "JSON call request without a Codex OAuth credential",
            vec![HttpReq::post(path, json!({"sdp": sdp, "session": {"model": "gpt-realtime"}})).into()],
        );
    }
    add(
        "call.multipart_no_credential",
        "multipart call request without a Codex OAuth credential",
        vec![post("/v1/realtime/calls")
            .typed("multipart/form-data; boundary=e2e-boundary", &multipart("e2e-boundary", &[("sdp", sdp), ("session", r#"{"type":"realtime","model":"gpt-realtime"}"#)]))
            .into()],
    );
    add(
        "call.multipart_missing_sdp",
        "multipart body without the sdp field",
        vec![post("/v1/realtime/calls")
            .typed("multipart/form-data; boundary=e2e-boundary", &multipart("e2e-boundary", &[("session", r#"{"model":"gpt-realtime"}"#)]))
            .into()],
    );
    add(
        "call.multipart_bad_session",
        "multipart session field is not JSON",
        vec![post("/v1/live")
            .typed("multipart/form-data; boundary=e2e-boundary", &multipart("e2e-boundary", &[("sdp", sdp), ("session", "not json")]))
            .into()],
    );
    add(
        "call.multipart_no_boundary",
        "multipart without a boundary parameter",
        vec![post("/v1/live").typed("multipart/form-data", &multipart("e2e-boundary", &[("sdp", sdp)])).into()],
    );
    add("call.malformed_json", "JSON call request that does not parse", vec![post("/v1/realtime/calls").raw("{broken").into()]);
    add("call.malformed_json_live", "same on the live path", vec![post("/v1/live").raw("{broken").into()]);
    add("call.json_array", "JSON call request that is an array", vec![HttpReq::post("/v1/realtime/calls", json!([1])).into()]);
    add(
        "call.session_not_object",
        "session field that is not an object",
        vec![HttpReq::post("/v1/realtime/calls", json!({"sdp": sdp, "session": "x"})).into()],
    );
    add("call.json_null", "JSON null call request", vec![HttpReq::post("/v1/realtime/calls", json!(null)).into()]);
    add("call.unknown_content_type", "an unrelated content type", vec![post("/v1/live").typed("image/png", "xx").into()]);
    add(
        "call.multipart_lf_only",
        "multipart with LF-only line endings",
        vec![post("/v1/live")
            .typed("multipart/form-data; boundary=lf", "--lf\nContent-Disposition: form-data; name=\"sdp\"\n\nv=0\n--lf--\n")
            .into()],
    );
    add(
        "call.multipart_truncated",
        "multipart body cut inside a part",
        vec![post("/v1/live").typed("multipart/form-data; boundary=cut", "--cut\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0").into()],
    );
    add("call.multipart_empty", "multipart content type with an empty body", vec![post("/v1/live").typed("multipart/form-data; boundary=x", "").into()]);
    add("call.empty_body","no body and no content type", vec![HttpReq::post("/v1/live", json!(null)).raw("").into()]);

    // ---- sideband, hangup and direct websocket (none of them has a session or credential)
    add("sideband.not_upgrade", "sideband GET without an upgrade", vec![HttpReq::get("/v1/live/call-123").into()]);
    add("sideband.not_upgrade_calls", "same on the realtime calls path", vec![HttpReq::get("/v1/realtime/calls/call-123").into()]);
    add("sideband.unknown_call", "upgrade for a call that does not exist", vec![WsReq::new("/v1/live/call-123", vec![]).into()]);
    add("sideband.unknown_call_realtime", "realtime-shaped not found", vec![WsReq::new("/v1/realtime/calls/call-123", vec![]).into()]);
    add("sideband.unknown_call_query", "call id in the query", vec![WsReq::new("/v1/realtime?call_id=call-123", vec![]).into()]);
    add("sideband.invalid_call_id", "call id with characters outside the allowed set", vec![WsReq::new("/v1/live/call.123", vec![]).into()]);
    add("sideband.invalid_call_id_query", "invalid call id in the query", vec![WsReq::new("/v1/realtime?call_id=a.b", vec![]).into()]);
    add("direct.not_upgrade", "standard realtime GET without an upgrade", vec![HttpReq::get("/v1/realtime").into()]);
    add("direct.not_upgrade_model", "same with a model query", vec![HttpReq::get("/v1/realtime?model=gpt-realtime").into()]);
    add("direct.no_credential", "upgrade without a Codex OAuth credential", vec![WsReq::new("/v1/realtime?model=gpt-realtime", vec![]).into()]);
    add(
        "hangup.invalid_call_id",
        "hangup with an invalid call id",
        vec![post("/v1/realtime/calls/call.123/hangup").into()],
    );
    add("hangup.unknown_call", "hangup for a call that does not exist", vec![post("/v1/realtime/calls/call-123/hangup").into()]);
    add("hangup.missing_key", "hangup without credentials", vec![post("/v1/realtime/calls/call-123/hangup").auth(Auth::None).into()]);

    // ---- routing details
    add("routing.trailing_slash_post", "POST with a trailing slash redirects", vec![post("/v1/realtime/").into()]);
    add("routing.trailing_slash_get", "GET with a trailing slash redirects", vec![HttpReq::get("/v1/live/").into()]);
    add("routing.wrong_method_calls", "GET on the POST-only calls route", vec![HttpReq::get("/v1/realtime/calls").into()]);
    add("routing.wrong_method_hangup", "GET on the hangup route", vec![HttpReq::get("/v1/realtime/calls/call-123/hangup").into()]);
    add("routing.put_live", "PUT is not registered on /v1/live", vec![HttpReq::put("/v1/live", json!({})).into()]);
    out
}
