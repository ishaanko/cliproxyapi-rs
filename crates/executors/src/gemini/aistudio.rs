//! AI Studio executor (Go: aistudio_executor.go). Requests are not sent to Google by the proxy:
//! they travel over the websocket relay to a browser page that performs the fetch with its own
//! logged-in session. The auth id is the relay channel (`aistudio-<id>`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_core::util::go_json_canonicalize;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::{HeaderMap, HeaderValue};

use super::common::{
    GL_API_VERSION, GL_ENDPOINT, PumpSetup, StreamPump, apply_custom_headers,
    compact_unsupported, fix_gemini_image_aspect_ratio, is_count_tokens_action, original_payload, thinking_error,
    translate_request, upstream_error, usage_metadata,
};
use crate::helps::gemini_content_turns::{ensure_leading_user_content_value, ensure_trailing_user_content_value};
use super::wsrelay::{
    self, HttpRequest, MESSAGE_TYPE_HTTP_RESP, MESSAGE_TYPE_STREAM_CHUNK, MESSAGE_TYPE_STREAM_END,
    MESSAGE_TYPE_STREAM_START, Manager, RelayError, StreamEvent, canonical_header_key,
};
use crate::ConfigRx;
use crate::helps::apply_patch::{apply_patch_original_request, apply_patch_translation_error, gateway_error};
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model};
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::text::trim_space;
use crate::helps::thinking::apply_thinking_with_source_payload;
use crate::helps::usage::{UsageReporter, filter_sse_usage_metadata, parse_gemini_stream_usage, parse_gemini_usage};

/// Executor for the `aistudio` provider.
pub struct AiStudioExecutor {
    cfg: ConfigRx,
    relay: Arc<Manager>,
}

impl AiStudioExecutor {
    /// Executor bound to `relay` (the process-wide relay in production).
    pub fn new(cfg: ConfigRx, relay: Arc<Manager>) -> Self {
        Self { cfg, relay }
    }

    /// Executor on the process-wide relay the `/v1/ws` route feeds.
    pub fn with_global_relay(cfg: ConfigRx) -> Self {
        Self::new(cfg, wsrelay::global())
    }

    fn reporter(&self, model: &str, auth: &Auth, opts: &Options) -> UsageReporter {
        UsageReporter::new("aistudio", "AIStudioExecutor", model, Some(auth), Some(opts))
    }
}

/// A request translated for the relay.
struct Translated {
    payload: Vec<u8>,
    action: &'static str,
}

fn relay_error(err: RelayError) -> ExecError {
    let mut out = ExecError::new(0, err.message);
    out.upstream_attempted = err.attempted;
    out
}

/// Uppercases `generationConfig.thinkingConfig.thinkingLevel` when it is one of
/// minimal/low/medium/high: AI Studio validates the enum case-sensitively and rejects lowercase
/// with a 400.
pub(super) fn normalize_thinking_level(payload: &[u8]) -> Vec<u8> {
    const PATH: &str = "generationConfig.thinkingConfig.thinkingLevel";
    let mut v = cpa_json::parse(payload);
    let level = v.g(PATH);
    let Some(current) = level.as_str() else {
        return payload.to_vec();
    };
    let normalized = current.to_uppercase();
    if !matches!(normalized.as_str(), "MINIMAL" | "LOW" | "MEDIUM" | "HIGH") || normalized == current {
        return payload.to_vec();
    }
    cpa_json::set(&mut v, PATH, normalized);
    cpa_json::to_vec(&v)
}

/// `{GL_ENDPOINT}/v1beta/models/{model}:{action}`; streams get `?alt=sse` (or an escaped
/// `?$alt=`), non-count calls an escaped `?$alt=` when the client sent one.
pub(super) fn build_endpoint(model: &str, action: &str, alt: &str) -> String {
    let base = format!("{GL_ENDPOINT}/{GL_API_VERSION}/models/{model}:{action}");
    if action == "streamGenerateContent" {
        if alt.is_empty() {
            return format!("{base}?alt=sse");
        }
        return format!("{base}?$alt={}", url::form_urlencoded::byte_serialize(alt.as_bytes()).collect::<String>());
    }
    if !alt.is_empty() && action != "countTokens" {
        return format!("{base}?$alt={}", url::form_urlencoded::byte_serialize(alt.as_bytes()).collect::<String>());
    }
    base
}

/// Re-encodes JSON like Go's `json.Marshal` of a decoded `any` (sorted keys, float64 numbers) and
/// puts a space after every key colon; non-JSON input is returned unchanged (Go:
/// ensureColonSpacedJSON, a fingerprinting nicety of the AI Studio path).
pub(super) fn ensure_colon_spaced_json(payload: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(payload);
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    let Ok(text) = std::str::from_utf8(trimmed) else {
        return payload.to_vec();
    };
    let Some(canonical) = go_json_canonicalize(text) else {
        return payload.to_vec();
    };
    let mut out = String::with_capacity(canonical.len() + canonical.len() / 8);
    let mut in_string = false;
    let mut escaped = false;
    for ch in canonical.chars() {
        out.push(ch);
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        } else if ch == ':' {
            out.push(' ');
        }
    }
    out.into_bytes()
}

/// Request headers as the relay envelope carries them: `Content-Type` plus custom headers.
fn envelope_headers(
    auth: &Auth,
    opts: &Options,
    session_id: Option<&str>,
    with_custom: bool,
) -> BTreeMap<String, Vec<String>> {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if with_custom {
        apply_custom_headers(&mut headers, auth, opts, session_id);
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in &headers {
        out.entry(canonical_header_key(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

impl AiStudioExecutor {
    /// Translation, thinking, payload rules and the AI Studio specific body edits.
    fn translate(&self, cfg: &Config, req: &Request, opts: &Options, stream: bool) -> Result<Translated, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let to = Format::Gemini;
        let original_source = original_payload(req, opts);
        let original_translated =
            translate_request(cfg, &opts.headers, from, to, &base_model, original_source, stream, false);
        let payload = translate_request(cfg, &opts.headers, from, to, &base_model, &req.payload, stream, false);
        let payload = apply_thinking_with_source_payload(
            &payload,
            &req.payload,
            original_source,
            &req.model,
            from.as_str(),
            to.as_str(),
            "aistudio",
        )
        .map_err(thinking_error)?;
        let payload = fix_gemini_image_aspect_ratio(&base_model, payload);
        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        let payload_req = PayloadRequest {
            cfg: Some(cfg),
            target_executor: "",
            model: &base_model,
            protocol: to.as_str(),
            from_protocol: from.as_str(),
            root: "",
            requested_model: &requested_model,
            request_path: &request_path,
            headers: Some(&opts.headers),
        };
        let payload = apply_payload_config(&payload_req, &payload, &original_translated);
        let mut v = cpa_json::parse(&payload);
        for key in [
            "generationConfig.maxOutputTokens",
            "generationConfig.responseMimeType",
            "generationConfig.responseJsonSchema",
        ] {
            cpa_json::delete(&mut v, key);
        }
        let count_tokens = is_count_tokens_action(req);
        let action = if count_tokens {
            "countTokens"
        } else if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        cpa_json::delete(&mut v, "session_id");
        ensure_leading_user_content_value(&mut v, "contents");
        if action != "countTokens" {
            ensure_trailing_user_content_value(&mut v, "contents");
        }
        let payload = normalize_thinking_level(&cpa_json::to_vec(&v));
        Ok(Translated { payload, action })
    }
}

#[async_trait]
impl Executor for AiStudioExecutor {
    fn identifier(&self) -> &str {
        "aistudio"
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        let cfg = self.cfg.borrow().clone();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result = self.execute_inner(&cfg, auth, &req, &opts, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        let cfg = self.cfg.borrow().clone();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result = self.stream_inner(&cfg, auth, req, opts, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, auth: &Auth, mut req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.cfg.borrow().clone();
        let base_model = parse_suffix(&req.model).model_name;
        req.metadata.insert("action".into(), Value::String("countTokens".into()));
        let translated = self.translate(&cfg, &req, &opts, false)?;
        let mut v = cpa_json::parse(&translated.payload);
        for key in ["generationConfig", "tools", "safetySettings"] {
            cpa_json::delete(&mut v, key);
        }
        let payload = cpa_json::to_vec(&v);

        let ws_req = HttpRequest {
            method: "POST".into(),
            url: build_endpoint(&base_model, "countTokens", ""),
            headers: envelope_headers(auth, &opts, None, false),
            body: payload,
        };
        let resp = self.relay.non_stream(&auth.id, &ws_req).await.map_err(relay_error)?;
        if !(200..300).contains(&resp.status) {
            return Err(upstream_error(resp.status, &resp.body));
        }
        let total_tokens = cpa_json::parse(&resp.body).g("totalTokens").int();
        if total_tokens <= 0 {
            return Err(ExecError::new(0, "wsrelay: totalTokens missing in response"));
        }
        let response_format = opts.response_format_or_source();
        let out = cpa_translator::translate_token_count(
            &Ctx::default(),
            Format::Gemini,
            response_format,
            total_tokens,
            &resp.body,
        );
        Ok(Response { payload: Bytes::from(out), ..Default::default() })
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }
}

impl AiStudioExecutor {
    #[allow(clippy::too_many_arguments)]
    async fn execute_inner(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let translated = self.translate(cfg, req, opts, false)?;
        reporter.set_translated_reasoning_effort(&translated.payload, Format::Gemini.as_str());
        let ws_req = HttpRequest {
            method: "POST".into(),
            url: build_endpoint(base_model, translated.action, &opts.alt),
            headers: envelope_headers(auth, opts, session_id, true),
            body: translated.payload.clone(),
        };
        reporter.start_response_ttft();
        let resp = self.relay.non_stream(&auth.id, &ws_req).await.map_err(relay_error)?;
        reporter.start_response_ttft();
        if !resp.body.is_empty() {
            reporter.mark_first_response_byte();
        }
        if !(200..300).contains(&resp.status) {
            return Err(upstream_error(resp.status, &resp.body));
        }
        reporter.observe_response_model(&resp.body);
        let response_format = opts.response_format_or_source();
        let mut param = Param::default();
        let original = apply_patch_original_request(req, opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            Format::Gemini,
            response_format,
            &req.model,
            &original,
            &translated.payload,
            &resp.body,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(gateway_error()),
        };
        let detail = parse_gemini_usage(&resp.body);
        reporter.publish(detail.clone());
        let out = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response {
            payload: Bytes::from(ensure_colon_spaced_json(&out)),
            metadata: usage_metadata(&detail),
            headers: resp.headers,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_inner(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        opts: Options,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let translated = self.translate(cfg, &req, &opts, true)?;
        reporter.set_translated_reasoning_effort(&translated.payload, Format::Gemini.as_str());
        let ws_req = HttpRequest {
            method: "POST".into(),
            url: build_endpoint(base_model, translated.action, &opts.alt),
            headers: envelope_headers(auth, &opts, session_id, true),
            body: translated.payload.clone(),
        };
        reporter.start_response_ttft();
        let mut events = self.relay.stream(&auth.id, &ws_req).await.map_err(relay_error)?;
        let Some(first) = events.recv().await else {
            return Err(ExecError::new(0, "wsrelay: stream closed before start"));
        };
        if first.status > 0 && first.status != 200 {
            // The upstream refused: drain the remaining frames into the error body.
            reporter.start_response_ttft();
            let mut body: Vec<u8> = Vec::new();
            if !first.payload.is_empty() {
                reporter.mark_first_response_byte();
                body.extend_from_slice(&first.payload);
            }
            if first.kind == MESSAGE_TYPE_STREAM_END {
                return Err(upstream_error(first.status, &body));
            }
            while let Some(event) = events.recv().await {
                if let Some(err) = &event.err {
                    if body.is_empty() {
                        body.extend_from_slice(err.as_bytes());
                    }
                    break;
                }
                if !event.payload.is_empty() {
                    reporter.mark_first_response_byte();
                    body.extend_from_slice(&event.payload);
                }
                if event.kind == MESSAGE_TYPE_STREAM_END {
                    break;
                }
            }
            return Err(upstream_error(first.status, &body));
        }

        let response_format = opts.response_format_or_source();
        let resp_headers = first.headers.clone();
        let (mut pump, rx, usage_rx) = StreamPump::new(PumpSetup {
            reporter: reporter.clone(),
            from: opts.source_format,
            upstream: Format::Gemini,
            response: response_format,
            req: &req,
            opts: &opts,
            body: translated.payload,
            ctx: Ctx::default(),
        });
        let reporter = reporter.clone();
        tokio::spawn(async move {
            let mut next = Some(first);
            loop {
                let event = match next.take() {
                    Some(event) => event,
                    None => tokio::select! {
                        _ = pump.tx.closed() => return,
                        event = events.recv() => match event {
                            Some(event) => event,
                            None => break,
                        },
                    },
                };
                match process_event(&mut pump, &reporter, event).await {
                    Flow::Continue => {}
                    Flow::Stop => return,
                    Flow::Finish => {
                        pump.finish();
                        return;
                    }
                }
            }
            pump.end_apply_patch().await;
            pump.finish();
        });
        let mut result = StreamResult::new(resp_headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

enum Flow {
    Continue,
    /// Stop without finishing (client gone or a failure was delivered).
    Stop,
    /// The upstream completed this response.
    Finish,
}

/// Translates and forwards one relay event (Go: processEvent).
async fn process_event(pump: &mut StreamPump, reporter: &UsageReporter, event: StreamEvent) -> Flow {
    if let Some(err) = event.err {
        pump.fail(ExecError::new(0, format!("wsrelay: {err}"))).await;
        return Flow::Stop;
    }
    match event.kind.as_str() {
        MESSAGE_TYPE_STREAM_START => Flow::Continue,
        MESSAGE_TYPE_STREAM_CHUNK => {
            if event.payload.is_empty() {
                return Flow::Continue;
            }
            reporter.mark_first_response_byte();
            reporter.observe_response_model(&event.payload);
            let filtered = filter_sse_usage_metadata(&event.payload);
            if let Some(detail) = parse_gemini_stream_usage(&filtered) {
                pump.usage.observe(detail, true);
            }
            if feed_spaced(pump, &filtered).await { Flow::Continue } else { Flow::Stop }
        }
        MESSAGE_TYPE_STREAM_END => {
            if pump.end_apply_patch().await {
                return Flow::Stop;
            }
            Flow::Finish
        }
        MESSAGE_TYPE_HTTP_RESP => {
            if !event.payload.is_empty() {
                reporter.mark_first_response_byte();
            }
            if !feed_spaced(pump, &event.payload).await {
                return Flow::Stop;
            }
            if pump.end_apply_patch().await {
                return Flow::Stop;
            }
            reporter.observe_response_model(&event.payload);
            pump.usage.observe(parse_gemini_usage(&event.payload), true);
            Flow::Finish
        }
        _ => Flow::Continue,
    }
}

/// Translates a payload and sends each frame colon-spaced.
async fn feed_spaced(pump: &mut StreamPump, payload: &[u8]) -> bool {
    pump.feed_with(payload, ensure_colon_spaced_json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_building_matches_go() {
        let base = "https://generativelanguage.googleapis.com/v1beta/models/m";
        assert_eq!(build_endpoint("m", "generateContent", ""), format!("{base}:generateContent"));
        assert_eq!(build_endpoint("m", "generateContent", "json"), format!("{base}:generateContent?$alt=json"));
        assert_eq!(build_endpoint("m", "streamGenerateContent", ""), format!("{base}:streamGenerateContent?alt=sse"));
        assert_eq!(
            build_endpoint("m", "streamGenerateContent", "a b"),
            format!("{base}:streamGenerateContent?$alt=a+b")
        );
        assert_eq!(build_endpoint("m", "countTokens", "json"), format!("{base}:countTokens"));
    }

    #[test]
    fn thinking_level_is_uppercased_only_for_known_levels() {
        let lower = br#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"}}}"#;
        let out = cpa_json::parse(&normalize_thinking_level(lower));
        assert_eq!(out.g("generationConfig.thinkingConfig.thinkingLevel").str(), "LOW");
        for same in [
            br#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"HIGH"}}}"#.as_slice(),
            br#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"extreme"}}}"#.as_slice(),
            br#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":3}}}"#.as_slice(),
            b"{}".as_slice(),
        ] {
            assert_eq!(normalize_thinking_level(same), same.to_vec());
        }
    }

    #[test]
    fn colon_spacing_sorts_keys_and_skips_strings() {
        let out = ensure_colon_spaced_json(br#" {"b":"x:y\":z","a":[1,{"c":null}],"n":1.50} "#);
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"a": [1,{"c": null}],"b": "x:y\":z","n": 1.5}"#);
        assert_eq!(ensure_colon_spaced_json(b"data: {\"a\":1}"), b"data: {\"a\":1}".to_vec());
        assert_eq!(ensure_colon_spaced_json(b""), b"".to_vec());
        // Go's encoder escapes HTML characters as \u003c, \u0026 and \u003e.
        let esc = |hex: &str| format!("{}u{hex}", '\\');
        let expected = format!(r#"{{"a": "{}{}{}"}}"#, esc("003c"), esc("0026"), esc("003e"));
        assert_eq!(ensure_colon_spaced_json(b"{\"a\":\"<&>\"}"), expected.into_bytes());
    }

    #[test]
    fn envelope_carries_content_type_and_custom_headers() {
        let mut auth = Auth::new("aistudio-x", "aistudio");
        auth.attributes.insert("header:X-Team".into(), "blue".into());
        let opts = Options::new(Format::Gemini);
        let headers = envelope_headers(&auth, &opts, None, true);
        assert_eq!(headers["Content-Type"], vec!["application/json".to_string()]);
        assert_eq!(headers["X-Team"], vec!["blue".to_string()]);
        assert_eq!(envelope_headers(&auth, &opts, None, false).len(), 1);
    }
}
