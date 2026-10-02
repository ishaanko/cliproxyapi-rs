//! OpenAI chat completions and legacy completions (Go: openai/openai_handlers.go).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::format::Format;
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use cpa_json::J;
use serde_json::{Map, Value};

use super::{ok_reply, read_request_body};
use crate::error::{ErrorMessage, build_error_response_body, status_text};
use crate::exec::{ExecArgs, Pipeline};
use crate::forward::{First, StreamHooks, empty_stream_reply, openai_error_reply, peek_first, start_sse_stream, with_nonstream_keepalive};
use crate::models;
use crate::req::ReqInfo;
use crate::state::AppState;

/// `GET /v1/models`.
pub async fn unified_models(State(st): State<AppState>, info: ReqInfo) -> Response {
    let cfg = st.cfg();
    models::unified_models(&cfg, &st.manager, &info.headers, &info.query).into_response()
}

/// `shouldTreatAsResponsesFormat`: Responses-style payloads sent to the chat endpoint.
fn should_treat_as_responses_format(root: &Value) -> bool {
    if root.g("messages").exists() {
        return false;
    }
    root.g("input").exists() || root.g("instructions").exists()
}

/// `POST /v1/chat/completions`.
pub async fn chat_completions(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let mut raw = match read_request_body(&info, body) {
        Ok(b) => b,
        Err(reply) => return reply.into_response(),
    };
    let root = cpa_json::parse(&raw);
    let mut stream = matches!(root.g("stream").v(), Some(Value::Bool(true)));
    if should_treat_as_responses_format(&root) {
        let model = root.g("model").str();
        raw = Bytes::from(cpa_translator::translate_request(
            Format::OpenAIResponse,
            Format::OpenAI,
            &model,
            &raw,
            stream,
        ));
        stream = cpa_json::parse(&raw).g("stream").bool();
    }
    let model = cpa_json::parse(&raw).g("model").str();
    let alt = info.alt();
    if stream {
        stream_chat(&st, &info, Format::OpenAI, &model, raw, &alt, ChatHooks).await
    } else {
        nonstream_chat(&st, &info, &model, raw, &alt, |b| b).await
    }
}

/// Non-stream execution shared by chat and legacy completions; `convert` post-processes the body.
async fn nonstream_chat(
    st: &AppState,
    info: &ReqInfo,
    model: &str,
    raw: Bytes,
    alt: &str,
    convert: fn(Bytes) -> Bytes,
) -> Response {
    let pipeline = Pipeline::new(st, info);
    let interval = pipeline.settings.nonstream_keepalive;
    let passthrough = pipeline.settings.passthrough_headers;
    let model = model.to_string();
    let alt = alt.to_string();
    let info = info.clone();
    with_nonstream_keepalive(interval, async move {
        let args = ExecArgs::new(Format::OpenAI, &model, raw, &alt);
        match pipeline.execute(args).await {
            Err(err) => openai_error_reply(&err, passthrough),
            Ok(ok) => {
                let body = convert(ok.body.clone());
                ok_reply(&info, ok, body)
            }
        }
    })
    .await
}

/// Terminal error as an SSE `data:` frame (`handleStreamResult`'s `WriteTerminalError`).
fn openai_stream_error_frame(out: &mut Vec<u8>, err: &ErrorMessage) {
    let status = err.status_or_500();
    let text = if err.text.is_empty() {
        status_text(status).to_string()
    } else {
        err.text.clone()
    };
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&build_error_response_body(status, &text));
    out.extend_from_slice(b"\n\n");
}

/// `data: <chunk>\n\n` framing, `[DONE]` on clean close and no `[DONE]` after an error.
struct ChatHooks;

impl StreamHooks for ChatHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\n\n");
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        openai_stream_error_frame(out, err);
    }

    fn write_done(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

/// Completions variant: every chunk is converted, empty conversions are dropped.
struct CompletionsHooks;

impl StreamHooks for CompletionsHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        if let Some(converted) = convert_chat_stream_chunk_to_completions(chunk) {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(&converted);
            out.extend_from_slice(b"\n\n");
        }
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        openai_stream_error_frame(out, err);
    }

    fn write_done(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

/// Streaming handler: peeks the first item, then commits SSE headers and forwards.
async fn stream_chat<H: StreamHooks + 'static>(
    st: &AppState,
    info: &ReqInfo,
    entry: Format,
    model: &str,
    raw: Bytes,
    alt: &str,
    hooks: H,
) -> Response {
    let pipeline = Pipeline::new(st, info);
    let passthrough = pipeline.settings.passthrough_headers;
    let keepalive = pipeline.settings.stream_keepalive;
    let mut es = pipeline.execute_stream(ExecArgs::new(entry, model, raw, alt)).await;
    match peek_first(&mut es).await {
        First::Error(err) => openai_error_reply(&err, passthrough).into_response(),
        First::Closed => empty_stream_reply(&es.headers, "data: [DONE]\n\n", true).into_response(),
        First::Chunk(chunk) => {
            // The first chunk goes through the same writer as the rest, then the loop continues.
            let mut hooks = hooks;
            let mut initial = Vec::new();
            hooks.write_chunk(&mut initial, &chunk);
            start_sse_stream(HeaderMap::new(), &es.headers, initial, es.rx, hooks, keepalive, true)
        }
    }
}

// ------------------------------------------------------------------ legacy completions

/// `POST /v1/completions`.
pub async fn completions(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let raw = match read_request_body(&info, body) {
        Ok(b) => b,
        Err(reply) => return reply.into_response(),
    };
    let root = cpa_json::parse(&raw);
    let stream = matches!(root.g("stream").v(), Some(Value::Bool(true)));
    let chat = Bytes::from(convert_completions_request_to_chat(&root));
    let model = cpa_json::parse(&chat).g("model").str();
    if stream {
        stream_chat(&st, &info, Format::OpenAI, &model, chat, "", CompletionsHooks).await
    } else {
        nonstream_chat(&st, &info, &model, chat, "", |b| Bytes::from(convert_chat_response_to_completions(&b))).await
    }
}

fn float_value(v: f64) -> Value {
    cpa_json::num_f64(v)
}

/// `convertCompletionsRequestToChatCompletions`.
pub fn convert_completions_request_to_chat(root: &Value) -> Vec<u8> {
    let mut prompt = root.g("prompt").str();
    if prompt.is_empty() {
        prompt = "Complete this:".to_string();
    }
    let mut out = cpa_json::parse_str(r#"{"model":"","messages":[{"role":"user","content":""}]}"#);
    let model = root.g("model");
    if model.exists() {
        cpa_json::set(&mut out, "model", model.str());
    }
    cpa_json::set(&mut out, "messages.0.content", prompt);
    for (key, kind) in [
        ("max_tokens", 'i'),
        ("temperature", 'f'),
        ("top_p", 'f'),
        ("frequency_penalty", 'f'),
        ("presence_penalty", 'f'),
        ("stop", 'r'),
        ("stream", 'b'),
        ("logprobs", 'b'),
        ("top_logprobs", 'i'),
        ("echo", 'b'),
    ] {
        let node = root.g(key);
        if !node.exists() {
            continue;
        }
        match kind {
            'i' => {
                cpa_json::set(&mut out, key, node.int());
            }
            'f' => {
                cpa_json::set(&mut out, key, float_value(node.float()));
            }
            'b' => {
                cpa_json::set(&mut out, key, node.bool());
            }
            _ => {
                cpa_json::set(&mut out, key, node.value());
            }
        }
    }
    cpa_json::to_vec(&out)
}

/// `json.Marshal` of a generic value: sorted keys, float64 numbers, HTML escaped.
fn marshal_any(v: &Value) -> String {
    go_json_sorted(v, GoJsonStyle::MARSHAL_ANY).unwrap_or_else(|| "null".to_string())
}

fn copy_base_fields(root: &Value, out: &mut Value) {
    let id = root.g("id");
    if id.exists() {
        cpa_json::set(out, "id", id.str());
    }
    let created = root.g("created");
    if created.exists() {
        cpa_json::set(out, "created", created.int());
    }
    let model = root.g("model");
    if model.exists() {
        cpa_json::set(out, "model", model.str());
    }
}

fn base_completions_value() -> Value {
    cpa_json::parse_str(r#"{"id":"","object":"text_completion","created":0,"model":"","choices":[]}"#)
}

fn set_choices(out: &mut Value, choices: Vec<Value>) {
    if choices.is_empty() {
        return;
    }
    let text = marshal_any(&Value::Array(choices));
    let _ = cpa_json::set_raw(out, "choices", &text);
}

/// `convertChatCompletionsResponseToCompletions`.
pub fn convert_chat_response_to_completions(raw: &[u8]) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let mut out = base_completions_value();
    copy_base_fields(&root, &mut out);
    let usage = root.g("usage");
    if usage.exists() {
        cpa_json::set(&mut out, "usage", usage.value());
    }
    let mut choices = Vec::new();
    let chat_choices = root.g("choices");
    if chat_choices.exists() && chat_choices.is_array() {
        for choice in chat_choices.array() {
            let mut c = Map::new();
            c.insert("index".into(), Value::from(choice.g("index").int()));
            let message = choice.g("message");
            if message.exists() {
                let content = message.g("content");
                if content.exists() {
                    c.insert("text".into(), Value::String(content.str()));
                }
            } else {
                let delta = choice.g("delta");
                if delta.exists() {
                    let content = delta.g("content");
                    if content.exists() {
                        c.insert("text".into(), Value::String(content.str()));
                    }
                }
            }
            let finish = choice.g("finish_reason");
            if finish.exists() {
                c.insert("finish_reason".into(), Value::String(finish.str()));
            }
            let logprobs = choice.g("logprobs");
            if logprobs.exists() {
                c.insert("logprobs".into(), logprobs.value());
            }
            choices.push(Value::Object(c));
        }
    }
    set_choices(&mut out, choices);
    cpa_json::to_vec(&out)
}

/// `convertChatCompletionsStreamChunkToCompletions`: `None` for chunks without content, finish
/// reason or usage.
pub fn convert_chat_stream_chunk_to_completions(chunk: &[u8]) -> Option<Vec<u8>> {
    let root = cpa_json::parse(chunk);
    let has_usage = root.g("usage").exists();
    let mut has_content = false;
    let chat_choices = root.g("choices");
    if chat_choices.exists() && chat_choices.is_array() {
        for choice in chat_choices.array() {
            let delta = choice.g("delta");
            if delta.exists() {
                let content = delta.g("content");
                if content.exists() && !content.str().is_empty() {
                    has_content = true;
                    break;
                }
            }
            let finish = choice.g("finish_reason");
            if finish.exists() && !finish.str().is_empty() && finish.str() != "null" {
                has_content = true;
                break;
            }
        }
    }
    if !has_content && !has_usage {
        return None;
    }

    let mut out = base_completions_value();
    copy_base_fields(&root, &mut out);
    let mut choices = Vec::new();
    if chat_choices.exists() && chat_choices.is_array() {
        for choice in chat_choices.array() {
            let mut c = Map::new();
            c.insert("index".into(), Value::from(choice.g("index").int()));
            let delta = choice.g("delta");
            let text = if delta.exists() {
                let content = delta.g("content");
                if content.exists() && !content.str().is_empty() {
                    content.str()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            c.insert("text".into(), Value::String(text));
            let finish = choice.g("finish_reason");
            if finish.exists() && finish.str() != "null" {
                c.insert("finish_reason".into(), Value::String(finish.str()));
            }
            let logprobs = choice.g("logprobs");
            if logprobs.exists() {
                c.insert("logprobs".into(), logprobs.value());
            }
            choices.push(Value::Object(c));
        }
    }
    set_choices(&mut out, choices);
    let usage = root.g("usage");
    if usage.exists() {
        cpa_json::set(&mut out, "usage", usage.value());
    }
    Some(cpa_json::to_vec(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn completions_request_becomes_a_chat_request() {
        let root = cpa_json::parse(br#"{"model":"m","prompt":"hi","max_tokens":7,"temperature":0.5,"stop":["a"],"stream":true}"#);
        assert_eq!(
            s(convert_completions_request_to_chat(&root)),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":7,"temperature":0.5,"stop":["a"],"stream":true}"#
        );
        let empty = cpa_json::parse(b"{}");
        assert_eq!(
            s(convert_completions_request_to_chat(&empty)),
            r#"{"model":"","messages":[{"role":"user","content":"Complete this:"}]}"#
        );
    }

    #[test]
    fn chat_response_becomes_text_completion() {
        let resp = br#"{"id":"c1","created":5,"model":"m","usage":{"total_tokens":3},"choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}]}"#;
        assert_eq!(
            s(convert_chat_response_to_completions(resp)),
            r#"{"id":"c1","object":"text_completion","created":5,"model":"m","choices":[{"finish_reason":"stop","index":0,"text":"Hello"}],"usage":{"total_tokens":3}}"#
        );
    }

    #[test]
    fn stream_chunks_without_payload_are_dropped() {
        let empty = br#"{"id":"c","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        assert!(convert_chat_stream_chunk_to_completions(empty).is_none());
        let content = br#"{"id":"c","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}"#;
        // a JSON null finish_reason becomes "" like Go's gjson String()
        assert_eq!(
            s(convert_chat_stream_chunk_to_completions(content).unwrap()),
            r#"{"id":"c","object":"text_completion","created":1,"model":"m","choices":[{"finish_reason":"","index":0,"text":"x"}]}"#
        );
        let usage_only = br#"{"id":"c","choices":[],"usage":{"total_tokens":1}}"#;
        assert!(convert_chat_stream_chunk_to_completions(usage_only).is_some());
    }

    #[test]
    fn responses_payload_detection() {
        assert!(should_treat_as_responses_format(&cpa_json::parse(br#"{"input":"x"}"#)));
        assert!(should_treat_as_responses_format(&cpa_json::parse(br#"{"instructions":"x"}"#)));
        assert!(!should_treat_as_responses_format(&cpa_json::parse(br#"{"messages":[],"input":"x"}"#)));
        assert!(!should_treat_as_responses_format(&cpa_json::parse(b"{}")));
    }
}
