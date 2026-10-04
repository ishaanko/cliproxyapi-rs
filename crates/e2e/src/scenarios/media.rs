//! Image generation/edit, video and Codex Alpha Search scenarios. Models: `gpt-image-*` route to
//! the Codex key, `grok-imagine-*` to the xAI keys, `compat-image` to the openai-compatibility
//! provider's image model.

use serde_json::{Value, json};

use crate::client::{Auth, HttpReq, Step as Req};
use crate::config::{ConfigSpec, KeyEntry, ModelCfg};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

// ---------------------------------------------------------------- profiles

/// xAI keys, an image model on the compat provider and one alpha-search capable codex key.
fn media(s: &mut ConfigSpec) {
    let base = s.codex[0].base_url.replace("/codex", "/xai");
    s.xai = (1..=2).map(|i| KeyEntry { api_key: format!("sk-xai-{i}"), base_url: base.clone(), ..Default::default() }).collect();
    s.compat[0].models.push(ModelCfg { name: "mock-image".into(), alias: "compat-image".into(), image: true });
    s.codex[0].alpha_search = Some(true);
}

fn media_disable_all(s: &mut ConfigSpec) {
    media(s);
    s.multimedia.push(("disable-image-generation", json!(true)));
}

fn media_disable_chat(s: &mut ConfigSpec) {
    media(s);
    s.multimedia.push(("disable-image-generation", json!("chat")));
}

/// Bindings expire after 300ms.
fn media_short_ttl(s: &mut ConfigSpec) {
    media(s);
    s.multimedia.push(("video-result-auth-cache-ttl", json!("300ms")));
}

fn media_bad_ttl(s: &mut ConfigSpec) {
    media(s);
    s.multimedia.push(("video-result-auth-cache-ttl", json!("not-a-duration")));
}

fn media_no_alpha(s: &mut ConfigSpec) {
    media(s);
    s.codex[0].alpha_search = None;
}

/// The alpha-search key exposes `gpt-5.5` under an alias.
fn media_codex_alias(s: &mut ConfigSpec) {
    media(s);
    s.codex[0].models = vec![ModelCfg { name: "gpt-5.5".into(), alias: "search-alias".into(), ..Default::default() }];
}

/// The first codex key also serves the xAI image model `grok-imagine-image` (as `gpt-5.5`), so a
/// codex credential receives an images request for a model that is not a direct image model and
/// the executor takes the Responses image tool path.
fn media_image_tool(s: &mut ConfigSpec) {
    media(s);
    s.codex[0].models = vec![ModelCfg { name: "gpt-5.5".into(), alias: "grok-imagine-image".into(), ..Default::default() }];
    // The higher priority keeps every request on that codex key instead of rotating over the
    // xAI keys that serve the model too.
    s.codex[0].priority = Some(10);
}

fn media_image_tool_base_model(s: &mut ConfigSpec) {
    media_image_tool(s);
    s.multimedia.push(("gpt-image-2-base-model", json!("gpt-5.5")));
}

fn media_image_tool_bad_base_model(s: &mut ConfigSpec) {
    media_image_tool(s);
    s.multimedia.push(("gpt-image-2-base-model", json!("claude-sonnet-4")));
}

fn media_image_tool_usage(s: &mut ConfigSpec) {
    media_image_tool(s);
    s.usage_statistics = true;
}

/// Only the codex key serves the model, so a failing call has no failover target and leaves one
/// failed record.
fn media_image_tool_usage_failed(s: &mut ConfigSpec) {
    media_image_tool_usage(s);
    s.xai.clear();
}

fn media_usage(s: &mut ConfigSpec) {
    media(s);
    s.usage_statistics = true;
}

/// A payload rule that targets the base model of the image tool request.
fn media_image_tool_payload_rules(s: &mut ConfigSpec) {
    media_image_tool(s);
    s.multimedia.push(("payload", json!({"override": [{"models": [{"name": "gpt-5.4-mini", "protocol": "codex"}], "params": {"metadata.rule": "applied"}}]})));
}

fn media_image_tool_disable_chat(s: &mut ConfigSpec) {
    media_image_tool(s);
    s.multimedia.push(("disable-image-generation", json!("chat")));
}

fn media_keepalive(s: &mut ConfigSpec) {
    media(s);
    s.keepalive_seconds = 1;
    s.nonstream_keepalive = 1;
}

fn media_passthrough(s: &mut ConfigSpec) {
    media(s);
    s.passthrough_headers = true;
}

fn media_no_retry(s: &mut ConfigSpec) {
    media(s);
    s.request_retry = 0;
}

// ---------------------------------------------------------------- helpers

fn ok() -> Script {
    Script::ok(Content::Text)
}

fn fail_always(status: u16) -> Script {
    Script::steps(vec![Step::always(Reply::error(status))])
}

fn fail_once(status: u16) -> Script {
    Script::steps(vec![Step::once(Reply::error(status))])
}

fn raw_ok(content_type: &str, body: &str) -> Script {
    Script::steps(vec![Step::always(Reply::Raw { status: 200, content_type: content_type.into(), body: body.into() })])
}

fn cut_stream(after: usize, abort: bool) -> Script {
    Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after, abort })])
}

fn stream_error(after: usize) -> Script {
    Script::steps(vec![Step::always(Reply::StreamError { content: Content::Text, after })])
}

fn one(req: HttpReq) -> Vec<Req> {
    vec![Req::Http(req)]
}

fn many(reqs: Vec<HttpReq>) -> Vec<Req> {
    reqs.into_iter().map(Req::Http).collect()
}

fn sc(out: &mut Vec<Scenario>, id: &str, desc: &str, script: Script, steps: Vec<Req>) {
    out.push(Scenario::new(format!("media.{id}"), desc, script, steps).profile(media));
}

fn sc_with(out: &mut Vec<Scenario>, id: &str, desc: &str, script: Script, steps: Vec<Req>, profile: fn(&mut ConfigSpec)) {
    out.push(Scenario::new(format!("media.{id}"), desc, script, steps).profile(profile));
}

fn gen_req(body: Value) -> HttpReq {
    HttpReq::post("/v1/images/generations", body)
}

fn edit_json(body: Value) -> HttpReq {
    HttpReq::post("/v1/images/edits", body)
}

const PNG: &[u8] = b"PNGDATA-1";
const PNG2: &[u8] = b"PNGDATA-2";
const MASK: &[u8] = b"MASKDATA";
const DATA_URL: &str = "data:image/png;base64,UE5HREFUQS0x";

fn edit_multipart(fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]) -> HttpReq {
    HttpReq::multipart("/v1/images/edits", fields, files)
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    codex_images(&mut out);
    codex_image_tool(&mut out);
    xai_images(&mut out);
    compat_images(&mut out);
    image_validation(&mut out);
    native_videos(&mut out);
    openai_videos(&mut out);
    alpha_search(&mut out);
    out
}

// ---------------------------------------------------------------- codex images

fn codex_images(out: &mut Vec<Scenario>) {
    let imgen = |extra: Value| {
        let mut body = json!({"model": "gpt-image-2", "prompt": "a cute baby sea otter"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        gen_req(body)
    };
    sc(out, "images.codex.gen_json", "codex image generation, options pass through", ok(), one(imgen(json!({"size": "1024x1024", "quality": "high", "n": 2, "output_format": "jpeg", "extra": {"keep": true}}))));
    sc(out, "images.codex.gen_default_model", "no model defaults to gpt-image-2", ok(), one(gen_req(json!({"prompt": "p"}))));
    sc(out, "images.codex.gen_model_variants", "gpt-image-1.5 and 2.5 models", ok(), many(vec![imgen(json!({"model": "gpt-image-1.5"})), imgen(json!({"model": "gpt-image-2.5"})), imgen(json!({"model": "gpt-image-2.5-flare"}))]));
    sc(out, "images.codex.gen_stream_false", "stream false drops the flag", ok(), one(imgen(json!({"stream": false}))));
    sc(out, "images.codex.gen_stream", "codex image generation stream relayed", ok(), one(imgen(json!({"stream": true, "partial_images": 1}))));
    sc(out, "images.codex.gen_stream_string_flag", "stream given as the string true", ok(), one(imgen(json!({"stream": "true"}))));
    sc(out, "images.codex.gen_stream_cut_clean", "stream ends without the completed event", cut_stream(1, false), one(imgen(json!({"stream": true}))));
    sc(out, "images.codex.gen_stream_cut_abort", "upstream drops the stream", cut_stream(1, true), one(imgen(json!({"stream": true}))));
    sc(out, "images.codex.gen_stream_error_event", "in-band error event mid stream", stream_error(1), one(imgen(json!({"stream": true}))));
    sc(out, "images.codex.gen_stream_http_error", "stream request fails before data", fail_always(500), one(imgen(json!({"stream": true}))));
    sc(out, "images.codex.gen_401", "upstream 401", fail_always(401), one(imgen(json!({}))));
    sc(out, "images.codex.gen_400", "upstream 400", fail_always(400), one(imgen(json!({}))));
    sc(out, "images.codex.gen_500_failover", "500 then success on the second key", fail_once(500), one(imgen(json!({}))));
    sc(out, "images.codex.gen_429_all", "every key rate limited", Script::steps(vec![Step::always(Reply::error_with(429, &[("retry-after", "30")], None))]), many(vec![imgen(json!({})), imgen(json!({}))]));
    sc(out, "images.codex.gen_rr", "round robin across codex keys", ok(), many(vec![imgen(json!({})), imgen(json!({})), imgen(json!({}))]));
    sc(out, "images.codex.gen_non_json_answer", "upstream answers plain text", raw_ok("text/plain", "plain answer"), one(imgen(json!({}))));
    sc_with(out, "images.codex.gen_passthrough_headers", "upstream headers are relayed with passthrough", Script::steps(vec![Step::always(Reply::ok(Content::Text)).with_headers(&[("x-upstream-note", "kept"), ("x-request-id", "up-1")])]), one(imgen(json!({}))), media_passthrough);
    sc(out, "images.codex.gen_client_headers", "client Codex headers reach the upstream, user agent does not", ok(),
        one(imgen(json!({})).header("User-Agent", "downstream-client/9.9").header("Version", "0.135.0").header("X-Codex-Turn-Metadata", "{\"turn_id\":\"t1\"}").header("X-Client-Request-Id", "client-req-1").header("Originator", "Codex Desktop")));

    sc(out, "images.codex.edits_json", "edit with JSON body", ok(), one(edit_json(json!({"model": "gpt-image-2", "prompt": "make it blue", "images": [{"image_url": DATA_URL}], "mask": {"image_url": DATA_URL}, "input_fidelity": "high"}))));
    sc(out, "images.codex.edits_json_stream", "edit stream with JSON body", ok(), one(edit_json(json!({"model": "gpt-image-2", "prompt": "p", "images": [{"image_url": DATA_URL}], "stream": true}))));
    sc(out, "images.codex.edits_multipart", "edit with image, mask and options as multipart", ok(),
        one(edit_multipart(&[("model", "gpt-image-2"), ("prompt", "make it blue"), ("size", "1024x1024"), ("n", "2"), ("quality", "high")], &[("image", "a.png", "image/png", PNG), ("mask", "m.png", "image/png", MASK)])));
    sc(out, "images.codex.edits_multipart_multi", "edit with image[] files and no content type", ok(),
        one(edit_multipart(&[("prompt", "merge")], &[("image[]", "a.png", "image/png", PNG), ("image[]", "b.png", "", PNG2)])));
    sc(out, "images.codex.edits_multipart_stream", "edit stream with multipart body", ok(),
        one(edit_multipart(&[("model", "gpt-image-1.5"), ("prompt", "p"), ("stream", "true")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.codex.edits_multipart_stream_yes", "stream flag spelled yes", ok(),
        one(edit_multipart(&[("prompt", "p"), ("stream", "yes")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.codex.edits_error", "edit upstream 500 on every key", fail_always(500), one(edit_multipart(&[("prompt", "p")], &[("image", "a.png", "image/png", PNG)])));
    sc_with(out, "images.codex.no_retry_500", "single attempt, upstream 500", fail_always(500), one(imgen(json!({}))), media_no_retry);
}

// ---------------------------------------------------------------- codex image tool (Responses)

fn codex_image_tool(out: &mut Vec<Scenario>) {
    // `grok-imagine-image` is an xAI image model by name, so the handler accepts it without
    // consulting the model registry; the codex key serves it as an alias, which sends the call to
    // the Codex executor with a model that is not a direct image model.
    let imgen = |extra: Value| {
        let mut body = json!({"model": "grok-imagine-image", "prompt": "a lighthouse"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        gen_req(body)
    };
    let edit = |extra: Value| {
        let mut body = json!({"model": "grok-imagine-image", "prompt": "make it blue", "image": DATA_URL});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        edit_json(body)
    };
    let img = Script::ok(Content::Image);
    let tool = |out: &mut Vec<Scenario>, id: &str, desc: &str, script: Script, steps: Vec<Req>| sc_with(out, &format!("images.codex_tool.{id}"), desc, script, steps, media_image_tool);
    let tool_with = |out: &mut Vec<Scenario>, id: &str, desc: &str, script: Script, steps: Vec<Req>, profile: fn(&mut ConfigSpec)| {
        sc_with(out, &format!("images.codex_tool.{id}"), desc, script, steps, profile)
    };
    let usage_queue = || Req::Http(HttpReq::get("/v0/management/usage-queue?count=5").auth(Auth::Mgmt));

    tool(out, "gen_json", "xAI options become image tool fields", img.clone(), one(imgen(json!({"size": "1024x1024", "quality": "high", "n": 2, "extra": 1}))));
    tool(out, "gen_url", "response_format url builds data urls", img.clone(), one(imgen(json!({"response_format": "URL"}))));
    tool(out, "gen_stream", "the stream is synthesized from one call", img.clone(), many(vec![imgen(json!({"stream": true})), imgen(json!({"stream": true, "response_format": "url"}))]));
    tool(out, "gen_cut_clean", "answer ends before completion", Script::steps(vec![Step::always(Reply::Cut { content: Content::Image, after: 6, abort: false })]), one(imgen(json!({}))));
    tool(out, "gen_cut_abort", "upstream drops the connection", Script::steps(vec![Step::always(Reply::Cut { content: Content::Image, after: 6, abort: true })]), one(imgen(json!({}))));
    tool(out, "gen_error_event", "failed event mid answer", Script::steps(vec![Step::always(Reply::StreamError { content: Content::Image, after: 6 })]), one(imgen(json!({}))));
    tool(out, "gen_no_image", "completed without an image call", Script::ok(Content::Text), many(vec![imgen(json!({})), imgen(json!({"stream": true}))]));
    tool(out, "gen_500", "upstream 500 on every key", fail_always(500), one(imgen(json!({}))));
    tool(out, "gen_401", "upstream 401", fail_always(401), one(imgen(json!({}))));
    tool(out, "gen_400", "upstream 400", fail_always(400), one(imgen(json!({}))));
    tool(out, "gen_client_headers", "client Codex headers and user agent reach the upstream", img.clone(),
        one(imgen(json!({})).header("User-Agent", "downstream-client/9.9").header("Version", "0.135.0").header("X-Codex-Turn-Metadata", "{\"turn_id\":\"t1\"}").header("X-Client-Request-Id", "client-req-1").header("Originator", "Codex Desktop")));

    tool(out, "edits_json", "edit with the image as a string", img.clone(), one(edit(json!({"quality": "hd", "n": 2}))));
    tool(out, "edits_json_images", "edit with several images", img.clone(), one(edit(json!({"image": null, "images": ["https://img/a.png", {"url": "https://img/b.png"}]}))));
    tool(out, "edits_json_stream", "edit stream", img.clone(), one(edit(json!({"stream": true}))));
    tool(out, "edits_multipart", "edit with uploaded images", img.clone(),
        one(edit_multipart(&[("model", "grok-imagine-image"), ("prompt", "p"), ("size", "1024x1024"), ("response_format", "url")], &[("image[]", "a.png", "image/png", PNG), ("image[]", "b.png", "image/png", PNG2)])));

    tool_with(out, "base_model_config", "gpt-image-2-base-model picks the base model", img.clone(), many(vec![imgen(json!({})), edit(json!({}))]), media_image_tool_base_model);
    tool_with(out, "base_model_invalid", "a base model that is not a gpt model falls back", img.clone(), one(imgen(json!({}))), media_image_tool_bad_base_model);
    tool_with(out, "usage", "usage records of the base model and of the image tool", img.clone(), vec![Req::Http(imgen(json!({}))), Req::Http(imgen(json!({"stream": true}))), Req::Pause(500), usage_queue()], media_image_tool_usage);
    tool_with(out, "usage_failed", "usage record of a failed image call", fail_always(500), vec![Req::Http(imgen(json!({}))), Req::Pause(500), usage_queue()], media_image_tool_usage_failed);
    tool_with(out, "payload_rules", "payload rules apply to the base model request", img.clone(), one(imgen(json!({}))), media_image_tool_payload_rules);
    tool_with(out, "disable_chat", "chat mode keeps the image tool on the images endpoint", img.clone(), one(imgen(json!({}))), media_image_tool_disable_chat);

    // Image generation inside a normal Responses request: the tool's usage is its own record and
    // comes after the main model's (upstream a3b7756).
    let responses_req = |stream: bool| HttpReq::post("/v1/responses", json!({"model": "gpt-5.5", "input": "draw a cat", "stream": stream, "tools": [{"type": "image_generation"}]}));
    sc_with(out, "responses.image_tool_usage", "image tool usage is published after the main usage", img.clone(), vec![Req::Http(responses_req(false)), Req::Http(responses_req(true)), Req::Pause(500), usage_queue()], media_usage);
}

// ---------------------------------------------------------------- xai images

fn xai_images(out: &mut Vec<Scenario>) {
    let imgen = |extra: Value| {
        let mut body = json!({"model": "grok-imagine-image", "prompt": "a red fox"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        gen_req(body)
    };
    sc(out, "images.xai.gen_b64", "xAI generation returns b64_json", ok(), one(imgen(json!({}))));
    sc(out, "images.xai.gen_url", "response_format url", ok(), one(imgen(json!({"response_format": "URL"}))));
    sc(out, "images.xai.gen_options", "size, quality and n map to xAI options", ok(), one(imgen(json!({"size": "2048x2048", "quality": "high", "n": 3}))));
    sc(out, "images.xai.gen_aspect", "aspect ratios and prefixed models", ok(), many(vec![imgen(json!({"aspect_ratio": "landscape"})), imgen(json!({"model": "xai/grok-imagine-image-quality", "size": "1024x1792"})), imgen(json!({"model": "x-ai/grok-imagine-image-2.0", "aspect_ratio": "20:9", "resolution": "2K"}))]));
    sc(out, "images.xai.gen_stream", "stream is synthesized from a normal call", ok(), one(imgen(json!({"stream": true}))));
    sc(out, "images.xai.gen_stream_url", "stream with url format", ok(), one(imgen(json!({"stream": true, "response_format": "url", "n": 2}))));
    sc(out, "images.xai.gen_empty_data", "upstream returns no images", raw_ok("application/json", r#"{"data":[]}"#), one(imgen(json!({}))));
    sc(out, "images.xai.gen_invalid_answer", "upstream returns invalid JSON", raw_ok("application/json", "nope"), one(imgen(json!({}))));
    sc(out, "images.xai.gen_stream_empty_data", "stream with an empty upstream answer", raw_ok("application/json", r#"{"data":[]}"#), one(imgen(json!({"stream": true}))));
    sc(out, "images.xai.gen_data_only_urls", "answer with only urls and a data url conversion", raw_ok("application/json", r#"{"created":5,"data":[{"url":"https://img/x.png","mime_type":"image/jpeg"},{"b64_json":"QUJD","mime_type":"image/webp"}]}"#), many(vec![imgen(json!({})), imgen(json!({"response_format": "url"}))]));
    sc(out, "images.xai.gen_401", "upstream 401", fail_always(401), one(imgen(json!({}))));
    sc(out, "images.xai.gen_stream_401", "stream, upstream 401", fail_always(401), one(imgen(json!({"stream": true}))));
    sc(out, "images.xai.gen_500_failover", "500 then success on the second xAI key", fail_once(500), one(imgen(json!({}))));
    sc(out, "images.xai.gen_rr", "round robin across xAI keys", ok(), many(vec![imgen(json!({})), imgen(json!({}))]));

    let edit = |extra: Value| {
        let mut body = json!({"model": "grok-imagine-image", "prompt": "make it night", "image": DATA_URL});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        edit_json(body)
    };
    sc(out, "images.xai.edits_json_string", "edit with an image string", ok(), one(edit(json!({}))));
    sc(out, "images.xai.edits_json_object", "edit with image_url object", ok(), one(edit(json!({"image": {"image_url": {"url": "https://img/a.png"}}, "size": "1536x1024", "n": 2}))));
    sc(out, "images.xai.edits_json_images", "edit with several images", ok(), one(edit(json!({"image": null, "images": ["https://img/a.png", {"url": "https://img/b.png"}, {"image_url": "https://img/c.png"}], "quality": "hd", "resolution": "2k"}))));
    sc(out, "images.xai.edits_json_no_image", "edit without any image", ok(), one(edit_json(json!({"model": "grok-imagine-image", "prompt": "p"}))));
    sc(out, "images.xai.edits_json_stream", "edit stream", ok(), one(edit(json!({"stream": true}))));
    sc(out, "images.xai.edits_multipart", "edit with uploaded images", ok(),
        one(edit_multipart(&[("model", "grok-imagine-image"), ("prompt", "p"), ("size", "1024x1024"), ("n", "2"), ("response_format", "url")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.xai.edits_multipart_multi", "edit with two uploaded images", ok(),
        one(edit_multipart(&[("model", "grok-imagine-image-quality"), ("prompt", "p"), ("quality", "hd")], &[("image[]", "a.png", "image/png", PNG), ("image[]", "b.bin", "", b"\x89PNG\r\n\x1a\nxx")])));
    sc(out, "images.xai.edits_multipart_stream", "edit stream from multipart", ok(),
        one(edit_multipart(&[("model", "grok-imagine-image"), ("prompt", "p"), ("stream", "1")], &[("image", "a.png", "image/png", PNG)])));
}

// ---------------------------------------------------------------- compat images

fn compat_images(out: &mut Vec<Scenario>) {
    let imgen = |extra: Value| {
        let mut body = json!({"model": "compat-image", "prompt": "a lighthouse"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        gen_req(body)
    };
    sc(out, "images.compat.gen_json", "compat image generation re-shaped", ok(), one(imgen(json!({"size": "512x512", "extra": 1}))));
    sc(out, "images.compat.gen_url", "compat generation, url format", ok(), one(imgen(json!({"response_format": "url"}))));
    sc(out, "images.compat.gen_stream", "compat stream relayed", ok(), one(imgen(json!({"stream": true}))));
    sc(out, "images.compat.gen_stream_error", "compat stream with an in-band error", stream_error(1), one(imgen(json!({"stream": true}))));
    sc(out, "images.compat.gen_stream_cut_clean", "compat stream ends early", cut_stream(1, false), one(imgen(json!({"stream": true}))));
    sc(out, "images.compat.gen_stream_http_error", "compat stream fails before data", fail_always(500), one(imgen(json!({"stream": true}))));
    sc(out, "images.compat.gen_500", "compat upstream 500", fail_always(500), one(imgen(json!({}))));
    sc(out, "images.compat.gen_empty_data", "compat answer without images", raw_ok("application/json", r#"{"created":1,"data":[]}"#), one(imgen(json!({}))));
    sc(out, "images.compat.edits_json", "compat edit with JSON body", ok(), one(edit_json(json!({"model": "compat-image", "prompt": "p", "images": [{"image_url": DATA_URL}]}))));
    sc(out, "images.compat.edits_multipart", "compat edit with multipart body", ok(),
        one(edit_multipart(&[("model", "compat-image"), ("prompt", "p"), ("size", "512x512")], &[("image", "a.png", "image/png", PNG), ("mask", "m.png", "image/png", MASK)])));
    sc(out, "images.compat.edits_multipart_stream", "compat edit stream", ok(),
        one(edit_multipart(&[("model", "compat-image"), ("prompt", "p"), ("stream", "true")], &[("image", "a.png", "image/png", PNG)])));
}

// ---------------------------------------------------------------- validation and modes

fn image_validation(out: &mut Vec<Scenario>) {
    sc(out, "images.invalid.gen_not_json", "generations body is not JSON", ok(), one(gen_req(json!(null)).raw("{nope")));
    sc(out, "images.invalid.gen_unsupported_model", "chat model on the images endpoint", ok(), one(gen_req(json!({"model": "gpt-5.5", "prompt": "p"}))));
    sc(out, "images.invalid.gen_unknown_prefix", "unknown provider prefix on an image model", ok(), one(gen_req(json!({"model": "other/gpt-image-2", "prompt": "p"}))));
    sc(out, "images.invalid.gen_prefixed_model", "provider prefix on a codex image model", ok(), one(gen_req(json!({"model": "openai/gpt-image-2", "prompt": "p"}))));
    sc(out, "images.invalid.gen_no_prompt", "prompt missing", ok(), one(gen_req(json!({"model": "gpt-image-2"}))));
    sc(out, "images.invalid.gen_blank_prompt", "prompt blank", ok(), one(gen_req(json!({"model": "gpt-image-2", "prompt": "  "}))));
    sc(out, "images.invalid.chat_model_image_only", "image model on chat completions", ok(), one(HttpReq::post("/v1/chat/completions", json!({"model": "gpt-image-2", "messages": [{"role": "user", "content": "hi"}]}))));
    sc(out, "images.invalid.edits_not_json", "edit JSON body invalid", ok(), one(edit_json(json!(null)).raw("{nope")));
    sc(out, "images.invalid.edits_unsupported_model", "edit with a chat model", ok(), one(edit_json(json!({"model": "gpt-5.5", "prompt": "p"}))));
    sc(out, "images.invalid.edits_no_prompt_json", "edit JSON without prompt", ok(), one(edit_json(json!({"model": "gpt-image-2"}))));
    sc(out, "images.invalid.edits_mp_unsupported_model", "multipart edit with a chat model", ok(), one(edit_multipart(&[("model", "gpt-5.5"), ("prompt", "p")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.invalid.edits_mp_no_prompt", "multipart edit without prompt", ok(), one(edit_multipart(&[("model", "gpt-image-2")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.invalid.edits_mp_no_image", "multipart edit without image", ok(), one(edit_multipart(&[("model", "gpt-image-2"), ("prompt", "p")], &[])));
    sc(out, "images.invalid.edits_mp_bad_stream_flag", "unparseable stream flag counts as false", ok(), one(edit_multipart(&[("prompt", "p"), ("stream", "maybe")], &[("image", "a.png", "image/png", PNG)])));
    sc(out, "images.invalid.edits_text_plain", "edit with an unsupported content type", ok(), one(HttpReq::form("/v1/images/edits", &[("prompt", "p")])));
    sc(out, "images.invalid.edits_multipart_no_boundary", "multipart edit without a boundary", ok(), one(edit_json(json!({})).raw_typed("multipart/form-data", b"--x")));
    sc(out, "images.invalid.edits_no_content_type", "edit without a content type", ok(), one(edit_json(json!({})).raw_typed("", b"{}")));
    sc(out, "images.invalid.gen_missing_auth", "images endpoint needs a client key", ok(), one(gen_req(json!({"prompt": "p"})).auth(Auth::None)));
    sc(out, "images.invalid.edits_missing_auth", "edits endpoint needs a client key", ok(), one(edit_json(json!({"prompt": "p"})).auth(Auth::None)));

    sc_with(out, "images.mode.all_gen", "disable-image-generation true: generations is a 404", ok(), one(gen_req(json!({"model": "gpt-image-2", "prompt": "p"}))), media_disable_all);
    sc_with(out, "images.mode.all_edits", "disable-image-generation true: edits is a 404", ok(), one(edit_json(json!({"model": "gpt-image-2", "prompt": "p"}))), media_disable_all);
    sc_with(out, "images.mode.all_invalid_first", "disabled endpoint answers 404 before validation", ok(), one(gen_req(json!(null)).raw("{nope")), media_disable_all);
    sc_with(out, "images.mode.chat_gen", "chat mode keeps the images endpoints", ok(), one(gen_req(json!({"model": "gpt-image-2", "prompt": "p"}))), media_disable_chat);
    sc_with(out, "images.mode.chat_edits", "chat mode keeps image edits", ok(), one(edit_json(json!({"model": "gpt-image-2", "prompt": "p", "images": [{"image_url": DATA_URL}]}))), media_disable_chat);
    sc_with(out, "images.mode.chat_responses_tool", "chat mode strips the image tool from responses", ok(),
        one(HttpReq::post("/v1/responses", json!({"model": "gpt-5.5", "input": "draw", "tools": [{"type": "image_generation"}]}))), media_disable_chat);
    sc_with(out, "images.keepalive.gen", "non-stream keep-alive newlines before a slow image answer", Script::steps(vec![Step::always(Reply::ok(Content::Text)).delayed(2300)]),
        one(gen_req(json!({"model": "gpt-image-2", "prompt": "p"}))), media_keepalive);
    sc_with(out, "images.keepalive.stream", "stream keep-alive before the first event", Script::steps(vec![Step::always(Reply::ok(Content::Text)).delayed(2300)]),
        one(gen_req(json!({"model": "gpt-image-2", "prompt": "p", "stream": true}))), media_keepalive);
    sc_with(out, "images.keepalive.xai_stream", "xAI stream keep-alive while the call runs", Script::steps(vec![Step::always(Reply::ok(Content::Text)).delayed(2300)]),
        one(gen_req(json!({"model": "grok-imagine-image", "prompt": "p", "stream": true}))), media_keepalive);
    sc_with(out, "images.keepalive.stream_error", "stream keep-alive then upstream error", Script::steps(vec![Step::always(Reply::error(500)).delayed(2300)]),
        one(gen_req(json!({"model": "gpt-image-2", "prompt": "p", "stream": true}))), media_keepalive);
    sc_with(out, "images.no_retry.xai_500", "xAI 500 without retries", fail_always(500), one(gen_req(json!({"model": "grok-imagine-image", "prompt": "p"}))), media_no_retry);
}

// ---------------------------------------------------------------- native xai videos

fn native_videos(out: &mut Vec<Scenario>) {
    let create = |path: &str, body: Value| HttpReq::post(path, body);
    sc(out, "videos.native.create_generations", "native video generation", ok(), one(create("/v1/videos/generations", json!({"model": "grok-imagine-video", "prompt": "a drone shot"}))));
    sc(out, "videos.native.create_root", "POST /v1/videos is a generation", ok(), one(create("/v1/videos", json!({"prompt": "a drone shot", "duration": 5}))));
    sc(out, "videos.native.create_edits", "native video edit", ok(), one(create("/v1/videos/edits", json!({"model": "xai/grok-imagine-video", "prompt": "darker", "video": {"url": "https://v/x.mp4"}}))));
    sc(out, "videos.native.create_extensions", "native video extension", ok(), one(create("/v1/videos/extensions", json!({"model": "x-ai/grok-imagine-video-1.5", "prompt": "continue", "video": {"url": "https://v/x.mp4"}}))));
    sc(out, "videos.native.create_preview_alias", "preview alias is normalized in the payload", ok(), one(create("/v1/videos/generations", json!({"model": "grok-imagine-video-1.5-preview", "prompt": "p"}))));
    sc(out, "videos.native.retrieve", "native retrieve", ok(), one(HttpReq::get("/v1/videos/vid_abc")));
    sc(out, "videos.native.retrieve_states", "native retrieve of failed and pending videos", ok(), many(vec![HttpReq::get("/v1/videos/vid_failed"), HttpReq::get("/v1/videos/vid_pending")]));
    sc(out, "videos.native.create_then_retrieve", "retrieve reuses the credential that created the video", ok(),
        many(vec![create("/v1/videos/generations", json!({"prompt": "p"})), HttpReq::get("/v1/videos/vid_mock_generations"), HttpReq::get("/v1/videos/vid_mock_generations"), HttpReq::get("/v1/videos/vid_other")]));
    sc(out, "videos.native.unsupported_model", "sora is not a native xAI model", ok(), one(create("/v1/videos/generations", json!({"model": "sora-2", "prompt": "p"}))));
    sc(out, "videos.native.foreign_prefix", "foreign provider prefix rejected", ok(), one(create("/v1/videos/edits", json!({"model": "codex/grok-imagine-video", "prompt": "p"}))));
    sc(out, "videos.native.not_json", "native body is not JSON", ok(), one(create("/v1/videos/generations", json!(null)).raw("{nope")));
    sc(out, "videos.native.upstream_500", "native create upstream 500 on every key", fail_always(500), one(create("/v1/videos/generations", json!({"prompt": "p"}))));
    sc(out, "videos.native.upstream_401", "native create upstream 401", fail_always(401), one(create("/v1/videos/generations", json!({"prompt": "p"}))));
    sc(out, "videos.native.retrieve_404", "native retrieve upstream 404", fail_always(404), one(HttpReq::get("/v1/videos/vid_gone")));
    sc(out, "videos.native.missing_auth", "videos need a client key", ok(), one(HttpReq::get("/v1/videos/vid_abc").auth(Auth::None)));
    sc_with(out, "videos.native.passthrough_headers", "upstream headers relayed with passthrough", Script::steps(vec![Step::always(Reply::ok(Content::Text)).with_headers(&[("x-upstream-note", "kept")])]), one(create("/v1/videos/generations", json!({"prompt": "p"}))), media_passthrough);
    sc_with(out, "videos.native.ttl_expired", "binding expires with a tiny ttl", ok(),
        vec![
            Req::Http(create("/v1/videos/generations", json!({"prompt": "p"}))),
            Req::Http(HttpReq::get("/v1/videos/vid_mock_generations")),
            Req::Pause(600),
            Req::Http(HttpReq::get("/v1/videos/vid_mock_generations")),
            Req::Http(HttpReq::get("/v1/videos/vid_mock_generations")),
        ],
        media_short_ttl,
    );
    sc_with(out, "videos.native.ttl_invalid", "invalid ttl falls back to the default", ok(),
        many(vec![create("/v1/videos/generations", json!({"prompt": "p"})), HttpReq::get("/v1/videos/vid_mock_generations")]), media_bad_ttl);
}

// ---------------------------------------------------------------- OpenAI-shaped videos

fn openai_videos(out: &mut Vec<Scenario>) {
    let create = |body: Value| HttpReq::post("/openai/v1/videos", body);
    sc(out, "videos.openai.create_json", "OpenAI-shaped create", ok(), one(create(json!({"model": "sora-2", "prompt": "a cat playing piano", "seconds": "8"}))));
    sc(out, "videos.openai.create_defaults", "create with defaults", ok(), one(create(json!({"prompt": "p"}))));
    sc(out, "videos.openai.create_options", "size, aspect ratio, resolution and clamped seconds", ok(), one(create(json!({"model": "grok-imagine-video-1.5-preview", "prompt": "p", "seconds": 99, "size": "1792x1024", "aspect_ratio": "square", "resolution": "480p"}))));
    sc(out, "videos.openai.create_input_reference", "image reference", ok(), many(vec![
        create(json!({"prompt": "p", "input_reference": {"image_url": "https://img/a.png"}})),
        create(json!({"prompt": "p", "image": {"image_url": {"url": "https://img/b.png"}}})),
        create(json!({"prompt": "p", "image_url": "https://img/c.png"})),
    ]));
    sc(out, "videos.openai.create_references", "reference images", ok(), one(create(json!({"prompt": "p", "reference_images": ["https://img/a.png", {"url": "https://img/b.png"}], "reference_image_urls": ["https://img/c.png"]}))));
    sc(out, "videos.openai.create_form_multipart", "multipart form create", ok(),
        one(HttpReq::multipart("/openai/v1/videos", &[("model", "sora-2"), ("prompt", "form prompt"), ("seconds", "6"), ("size", "1280x720"), ("input_reference[image_url]", "https://img/a.png")], &[])));
    sc(out, "videos.openai.create_form_urlencoded", "urlencoded form create", ok(),
        one(HttpReq::form("/openai/v1/videos", &[("prompt", "urlencoded"), ("reference_image_urls", "https://a/1.png,https://a/2.png"), ("seconds", "5")])));
    sc(out, "videos.openai.create_errors", "request validation errors become failed video resources", ok(), many(vec![
        create(json!({"prompt": " "})),
        create(json!({"prompt": "p", "seconds": "abc"})),
        create(json!({"prompt": "p", "size": "100x100"})),
        create(json!({"prompt": "p", "input_reference": {"file_id": "file_1"}})),
        create(json!({"prompt": "p", "input_reference": {"file_id": "f", "image_url": "u"}})),
        create(json!({"prompt": "p", "image": "u", "reference_images": ["a"]})),
        create(json!({"prompt": "p", "reference_images": ["1", "2", "3", "4", "5", "6", "7", "8"]})),
    ]));
    sc(out, "videos.openai.create_unsupported_model", "model not supported on the OpenAI route", ok(), one(create(json!({"model": "gpt-5.5", "prompt": "p"}))));
    sc(out, "videos.openai.create_not_json", "create body is not JSON", ok(), one(create(json!(null)).raw("{nope")));
    sc(out, "videos.openai.create_upstream_500", "create upstream 500", fail_always(500), one(create(json!({"prompt": "p"}))));
    sc(out, "videos.openai.create_upstream_429", "create upstream 429 on every key", fail_always(429), one(create(json!({"prompt": "p"}))));
    sc(out, "videos.openai.create_no_request_id", "upstream answers without a request id", raw_ok("application/json", "{}"), one(create(json!({"prompt": "p"}))));
    sc(out, "videos.openai.create_status_mapping", "upstream status and progress are mapped", raw_ok("application/json", r#"{"request_id":"vid_s","status":"processing","progress":25}"#), one(create(json!({"prompt": "p"}))));
    sc(out, "videos.openai.retrieve_done", "retrieve of a finished video", ok(), one(HttpReq::get("/openai/v1/videos/vid_abc")));
    sc(out, "videos.openai.retrieve_states", "retrieve of failed, pending and error shapes", ok(), many(vec![
        HttpReq::get("/openai/v1/videos/vid_failed"),
        HttpReq::get("/openai/v1/videos/vid_pending"),
        HttpReq::get("/openai/v1/videos/vid_string_error"),
        HttpReq::get("/openai/v1/videos/vid_code_only"),
        HttpReq::get("/openai/v1/videos/vid_nourl"),
    ]));
    sc(out, "videos.openai.create_then_retrieve", "retrieve is bound to the creating credential", ok(), many(vec![
        create(json!({"prompt": "p"})),
        HttpReq::get("/openai/v1/videos/vid_mock_generations"),
        HttpReq::get("/openai/v1/videos/vid_mock_generations/content"),
        HttpReq::get("/openai/v1/videos/vid_mock_generations"),
    ]));
    sc(out, "videos.openai.retrieve_404", "retrieve upstream 404", fail_always(404), one(HttpReq::get("/openai/v1/videos/vid_gone")));
    sc(out, "videos.openai.retrieve_encoded_id", "blank id is rejected", ok(), one(HttpReq::get("/openai/v1/videos/%20")));
    sc(out, "videos.openai.content", "video download", ok(), one(HttpReq::get("/openai/v1/videos/vid_abc/content")));
    sc(out, "videos.openai.content_variants", "variant query", ok(), many(vec![
        HttpReq::get("/openai/v1/videos/vid_abc/content?variant=video"),
        HttpReq::get("/openai/v1/videos/vid_abc/content?variant=thumbnail"),
    ]));
    sc(out, "videos.openai.content_problems", "content with a missing url, bad url or missing file", ok(), many(vec![
        HttpReq::get("/openai/v1/videos/vid_nourl/content"),
        HttpReq::get("/openai/v1/videos/vid_badurl/content"),
        HttpReq::get("/openai/v1/videos/vid_missing_file/content"),
        HttpReq::get("/openai/v1/videos/vid_failed/content"),
    ]));
    sc(out, "videos.openai.content_upstream_500", "content, poll fails upstream", fail_always(500), one(HttpReq::get("/openai/v1/videos/vid_abc/content")));
    sc_with(out, "videos.openai.ttl_expired", "binding expires, retrieve rotates keys", ok(),
        vec![
            Req::Http(create(json!({"prompt": "p"}))),
            Req::Http(HttpReq::get("/openai/v1/videos/vid_mock_generations")),
            Req::Pause(600),
            Req::Http(HttpReq::get("/openai/v1/videos/vid_mock_generations")),
            Req::Http(HttpReq::get("/openai/v1/videos/vid_mock_generations")),
        ],
        media_short_ttl,
    );
    sc_with(out, "videos.openai.passthrough_headers", "retrieve relays upstream headers with passthrough", Script::steps(vec![Step::always(Reply::ok(Content::Text)).with_headers(&[("x-upstream-note", "kept")])]), one(HttpReq::get("/openai/v1/videos/vid_abc")), media_passthrough);
    sc_with(out, "videos.openai.keepalive", "non-stream keep-alive on a slow create", Script::steps(vec![Step::always(Reply::ok(Content::Text)).delayed(2300)]), one(create(json!({"prompt": "p"}))), media_keepalive);
}

// ---------------------------------------------------------------- alpha search

fn alpha_search(out: &mut Vec<Scenario>) {
    let search = |path: &str, body: Value| HttpReq::post(path, body);
    let body = json!({"id": "sess-1", "model": "gpt-5.5", "query": "rust async", "prompt_cache_key": "k1", "prompt_cache_retention": "24h", "z": {"b": 1, "a": "<tag> & more"}});
    sc(out, "search.v1_ok", "alpha search forwarded with cache fields removed", ok(), one(search("/v1/alpha/search", body.clone())));
    sc(out, "search.codex_alias_path", "same endpoint under /backend-api/codex", ok(), one(search("/backend-api/codex/alpha/search", body.clone())));
    sc(out, "search.untouched_body", "body without cache fields is forwarded as sent", ok(), one(search("/v1/alpha/search", json!({"model": "gpt-5.5", "query": "q"}))));
    sc(out, "search.client_headers", "selected client headers are forwarded", ok(),
        one(search("/v1/alpha/search", json!({"model": "gpt-5.5"})).header("Version", "0.135.0").header("User-Agent", "codex-test/1").header("Session_id", "s-42").header("X-Client-Request-Id", "req-9").header("X-Other", "dropped")));
    sc(out, "search.not_json", "non-JSON body is forwarded verbatim", ok(), one(search("/v1/alpha/search", json!(null)).raw("not json at all")));
    sc(out, "search.upstream_errors", "upstream status and body are relayed", Script::steps(vec![Step::once(Reply::error(401)), Step::once(Reply::error(429)), Step::once(Reply::error(500))]),
        many(vec![search("/v1/alpha/search", json!({"model": "gpt-5.5"})), search("/v1/alpha/search", json!({"model": "gpt-5.5"})), search("/v1/alpha/search", json!({"model": "gpt-5.5"}))]));
    sc(out, "search.upstream_text", "upstream non-JSON answer", raw_ok("text/plain", "search backend down"), one(search("/v1/alpha/search", json!({"model": "gpt-5.5"}))));
    sc(out, "search.rr_same_key", "only the alpha-search key is used", ok(), many(vec![search("/v1/alpha/search", json!({"model": "gpt-5.5"})), search("/v1/alpha/search", json!({"model": "gpt-5.5"}))]));
    sc_with(out, "search.no_eligible_key", "no key allows alpha search", ok(), one(search("/v1/alpha/search", json!({"model": "gpt-5.5"}))), media_no_alpha);
    sc_with(out, "search.model_alias", "alias resolved to the upstream model", ok(), one(search("/v1/alpha/search", json!({"model": "search-alias", "query": "q"}))), media_codex_alias);
    sc(out, "search.unknown_model", "model without a matching credential", ok(), one(search("/v1/alpha/search", json!({"model": "no-such-model", "query": "q"}))));
    sc(out, "search.missing_auth", "alpha search needs a client key", ok(), one(search("/v1/alpha/search", json!({"model": "gpt-5.5"})).auth(Auth::None)));
    sc(out, "search.wrong_method", "GET is not routed", ok(), one(HttpReq::get("/v1/alpha/search")));
}
