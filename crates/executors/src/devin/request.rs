//! Interactions payload to Devin request material (Go: devin_executor.go `parseInteractionsPayload`
//! and its helpers).
//!
//! The executor consumes the internal interactions format; this module extracts the system
//! prompt, history prompts, tools, sampling settings, session ids and thinking settings from it,
//! recovering images and thinking signatures from the original client request when a translator
//! dropped them.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};
use cpa_core::signature::{SignatureProvider, detect_signature_provider};
use cpa_json::{J, Res, Value};
use cpa_translator::common::{
    is_devin_codex_app_automation_update, sanitize_devin_tool_description,
};
use uuid::Uuid;

use super::wire::{DEFAULT_MAX_TOKENS, Image, Prompt, Tool, ToolCall};

const EMPTY_TOOL_RESULT_PLACEHOLDER: &str = "{}";

/// Everything `parse_interactions_payload` extracts for one request.
#[derive(Debug, Clone, Default)]
pub struct ParsedPayload {
    pub system_prompt: String,
    pub prompts: Vec<Prompt>,
    pub tools: Vec<Tool>,
    pub temperature: Option<f64>,
    pub max_tokens: i64,
    pub session_id: String,
    pub cascade_id: String,
    pub thinking_level: String,
    pub budget_tokens: i64,
}

fn new_message_id() -> String {
    Uuid::new_v4().to_string()
}

/// First value that is not blank after trimming (returned untrimmed).
fn first_non_empty<I: IntoIterator<Item = String>>(values: I) -> String {
    values
        .into_iter()
        .find(|v| !v.trim().is_empty())
        .unwrap_or_default()
}

/// Parses an interactions payload (or an OpenAI-style `messages` payload as a fallback).
pub fn parse_interactions_payload(payload: &[u8], original_request: &[u8]) -> ParsedPayload {
    let root = cpa_json::parse(payload);
    let original = (!original_request.is_empty()).then(|| cpa_json::parse(original_request));

    // 1. System prompt
    let mut system_prompt = root.g("system_instruction").str().trim().to_string();
    if system_prompt.is_empty() {
        system_prompt = root.g("systemInstruction").str().trim().to_string();
    }
    let mut out = ParsedPayload {
        system_prompt,
        ..Default::default()
    };

    // 2. Generation config
    let mut gen_cfg = root.g("generation_config");
    if !gen_cfg.exists() {
        gen_cfg = root.g("generationConfig");
    }
    if gen_cfg.exists() {
        let t = gen_cfg.g("temperature");
        if t.exists() {
            out.temperature = Some(t.float());
        }
        out.max_tokens = gen_cfg.g("max_output_tokens").int();
        out.thinking_level = gen_cfg.g("thinking_level").str();
        out.budget_tokens = gen_cfg.g("thinking_config.thinking_budget").int();
    }
    if out.temperature.is_none() {
        // The original client temperature wins over the translated one.
        let orig_t = original
            .as_ref()
            .map(|o| o.g("temperature"))
            .filter(|t| t.exists());
        if let Some(t) = orig_t {
            out.temperature = Some(t.float());
        } else if root.g("temperature").exists() {
            out.temperature = Some(root.g("temperature").float());
        }
    }
    if out.max_tokens <= 0 {
        out.max_tokens = DEFAULT_MAX_TOKENS;
    }

    // 3. Session and cascade id: stable identifiers first (keeps upstream prompt caching),
    //    previous_interaction_id only as the last resort.
    let session_keys = [
        "session_id",
        "sessionId",
        "conversation_id",
        "previous_interaction_id",
    ];
    out.session_id = first_non_empty(session_keys.iter().map(|k| root.g(k).str()))
        .trim()
        .to_string();
    if out.session_id.is_empty()
        && let Some(orig) = &original
    {
        out.session_id = first_non_empty(session_keys.iter().map(|k| orig.g(k).str()))
            .trim()
            .to_string();
    }
    out.cascade_id = out.session_id.clone();

    // 4. History prompts
    let mut pending_tool_calls: Vec<String> = Vec::new();
    let input = root.g("input");
    if input.is_array() {
        for step in input.array() {
            parse_input_step(&step, &mut out.prompts, &mut pending_tool_calls);
        }
    } else if root.g("messages").is_array() {
        parse_messages_fallback(&root, &mut out, &mut pending_tool_calls);
    }

    // 5. Recover signatures, reasoning and images the translators may have lost.
    if let Some(orig) = &original {
        supplement_signatures_from_original(orig, &mut out.prompts);
        supplement_images_from_original(orig, &mut out.prompts);
    }

    // 6. Tools
    out.tools = parse_tools(&root);
    out
}

/// One element of the interactions `input` array.
fn parse_input_step(step: &Res<'_>, prompts: &mut Vec<Prompt>, pending: &mut Vec<String>) {
    let step_type = step.g("type").str().trim().to_lowercase();
    match step_type.as_str() {
        "user_input" => {
            let (text, images) = extract_step_content(step);
            prompts.push(Prompt {
                message_id: new_message_id(),
                source: 1,
                content: text,
                images,
                ..Default::default()
            });
        }
        "model_output" => {
            let text = extract_step_text(step);
            let (sig, sig_type) = parse_signature_bytes(&first_non_empty([
                step.g("signature").str(),
                step.g("thought_signature").str(),
            ]));
            match prompts.last_mut().filter(|p| p.source == 2) {
                Some(last) => {
                    if last.content.is_empty() {
                        last.content = text;
                    } else {
                        last.content.push('\n');
                        last.content.push_str(&text);
                    }
                    if !sig.is_empty() && last.signature.is_empty() {
                        last.signature = sig;
                        last.signature_type = sig_type;
                    }
                }
                None => prompts.push(Prompt {
                    message_id: new_message_id(),
                    source: 2,
                    content: text,
                    signature: sig,
                    signature_type: sig_type,
                    ..Default::default()
                }),
            }
        }
        "thought" => {
            let text = extract_step_text(step);
            let (sig, sig_type) = parse_signature_bytes(&first_non_empty([
                step.g("signature").str(),
                step.g("thought_signature").str(),
            ]));
            match prompts.last_mut().filter(|p| p.source == 2) {
                Some(last) => {
                    if last.thinking.is_empty() {
                        last.thinking = text;
                    } else {
                        last.thinking.push_str("\n\n");
                        last.thinking.push_str(&text);
                    }
                    if !sig.is_empty() && last.signature.is_empty() {
                        last.signature = sig;
                        last.signature_type = sig_type;
                    }
                }
                None => prompts.push(Prompt {
                    message_id: new_message_id(),
                    source: 2,
                    thinking: text,
                    signature: sig,
                    signature_type: sig_type,
                    ..Default::default()
                }),
            }
        }
        "function_call" => {
            let id = first_non_empty([step.g("id").str(), step.g("call_id").str()]);
            let args = step.g("arguments");
            let arguments = if args.is_string() {
                args.str()
            } else if args.exists() {
                args.raw()
            } else {
                String::new()
            };
            let tc = ToolCall {
                id: id.clone(),
                name: step.g("name").str(),
                arguments,
            };
            match prompts.last_mut().filter(|p| p.source == 2) {
                Some(last) => last.tool_calls.push(tc),
                None => prompts.push(Prompt {
                    message_id: new_message_id(),
                    source: 2,
                    tool_calls: vec![tc],
                    ..Default::default()
                }),
            }
            pending.push(id);
        }
        "function_result" => {
            let id = first_non_empty([step.g("call_id").str(), step.g("id").str()]);
            push_tool_result(prompts, pending, &id, extract_function_result_content(step));
        }
        _ => {}
    }
}

/// A tool result: matched to a pending call it becomes a source-4 turn, otherwise it is
/// downgraded to an orphaned user turn.
fn push_tool_result(
    prompts: &mut Vec<Prompt>,
    pending: &mut Vec<String>,
    id: &str,
    (text, images): (String, Vec<Image>),
) {
    match match_pending_tool_call(pending, id) {
        Some(matched_id) => prompts.push(Prompt {
            message_id: new_message_id(),
            source: 4,
            tool_call_id: matched_id,
            content: text,
            images,
            ..Default::default()
        }),
        None => prompts.push(Prompt {
            message_id: new_message_id(),
            source: 1,
            original_tool_call_id: id.to_string(),
            is_orphaned_tool: true,
            content: text,
            images,
            ..Default::default()
        }),
    }
}

/// Removes and returns the pending call id matching `id` (the oldest one when `id` is empty).
fn match_pending_tool_call(pending: &mut Vec<String>, id: &str) -> Option<String> {
    let idx = if id.is_empty() {
        (!pending.is_empty()).then_some(0)
    } else {
        pending.iter().position(|p| p == id)
    }?;
    let matched = pending.remove(idx);
    Some(if id.is_empty() {
        matched
    } else {
        id.to_string()
    })
}

/// Direct OpenAI chat `messages` payloads that were not translated.
fn parse_messages_fallback(root: &Value, out: &mut ParsedPayload, pending: &mut Vec<String>) {
    for m in root.g("messages").array() {
        let role = m.g("role").str().trim().to_lowercase();
        match role.as_str() {
            "system" | "developer" => {
                if out.system_prompt.is_empty() {
                    out.system_prompt = m.g("content").str();
                }
            }
            "user" => {
                let (text, images) = extract_step_content(&m);
                out.prompts.push(Prompt {
                    message_id: new_message_id(),
                    source: 1,
                    content: text,
                    images,
                    ..Default::default()
                });
            }
            "assistant" => {
                let text = extract_step_text(&m);
                let mut tool_calls = Vec::new();
                let tcs = m.g("tool_calls");
                if tcs.is_array() {
                    for item in tcs.array() {
                        let id = first_non_empty([item.g("id").str(), item.g("call_id").str()]);
                        let mut name = item.g("function.name").str();
                        if name.is_empty() {
                            name = item.g("name").str();
                        }
                        let mut fn_args = item.g("function.arguments");
                        if !fn_args.exists() {
                            fn_args = item.g("arguments");
                        }
                        let arguments = if fn_args.is_string() {
                            fn_args.str()
                        } else if fn_args.exists() {
                            fn_args.raw()
                        } else {
                            String::new()
                        };
                        pending.push(id.clone());
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                }
                out.prompts.push(Prompt {
                    message_id: new_message_id(),
                    source: 2,
                    content: text,
                    tool_calls,
                    ..Default::default()
                });
            }
            "tool" => {
                let id = first_non_empty([
                    m.g("tool_call_id").str(),
                    m.g("id").str(),
                    m.g("call_id").str(),
                ]);
                push_tool_result(
                    &mut out.prompts,
                    pending,
                    &id,
                    extract_function_result_content(&m),
                );
            }
            _ => {}
        }
    }
}

/// Tool declarations: plain, Gemini `function_declarations`, and the `mcp__codex_app` namespace
/// (minus `automation_update`).
fn parse_tools(root: &Value) -> Vec<Tool> {
    let tools_res = root.g("tools");
    let mut tools = Vec::new();
    if !tools_res.is_array() {
        return tools;
    }
    let mut append = |t: &Res<'_>| {
        let name = t.g("name").str();
        if name.is_empty() || is_devin_codex_app_automation_update("", &name) {
            return;
        }
        let desc = sanitize_devin_tool_description(&name, &t.g("description").str());
        let mut params = t.g("parameters").raw();
        if params.is_empty() {
            params = t.g("parametersJsonSchema").raw();
        }
        tools.push(Tool {
            name,
            description: desc,
            parameters: params.into_bytes(),
        });
    };
    for t in tools_res.array() {
        if t.g("type").str() == "namespace"
            && t.g("name")
                .str()
                .trim()
                .eq_ignore_ascii_case("mcp__codex_app")
        {
            let mut children = t.g("tools");
            if !children.is_array() {
                children = t.g("children");
            }
            if children.is_array() {
                for c in children.array() {
                    if c.g("name")
                        .str()
                        .trim()
                        .eq_ignore_ascii_case("automation_update")
                    {
                        continue;
                    }
                    append(&c);
                }
            }
            continue;
        }
        let decls = t.g("function_declarations");
        if decls.is_array() {
            decls.array().iter().for_each(&mut append);
            continue;
        }
        let decls = t.g("functionDeclarations");
        if decls.is_array() {
            decls.array().iter().for_each(&mut append);
            continue;
        }
        append(&t);
    }
    tools
}

// ------------------------------------------------------------------ content extraction

/// `(mime type, data)` of a `data:` URL; the mime type defaults to image/png.
pub fn parse_data_url(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let rest = raw.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let header = &rest[..comma];
    let data = &rest[comma + 1..];
    let mime = header.split(';').next().unwrap_or("").trim();
    Some((
        if mime.is_empty() { "image/png" } else { mime }.to_string(),
        data.to_string(),
    ))
}

fn mime_extension(mime: &str) -> &'static str {
    match mime.trim().to_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// Image from an `image`, `input_image` or `image_url` part in any of the supported shapes.
fn extract_image(part: &Res<'_>) -> Option<Image> {
    let part_type = part.g("type").str().trim().to_lowercase();
    if !matches!(part_type.as_str(), "image" | "input_image" | "image_url") {
        return None;
    }
    let mut data = part.g("data").str().trim().to_string();
    let mut mime = part.g("mime_type").str().trim().to_string();

    if data.is_empty() {
        data = part.g("source.data").str().trim().to_string();
        if mime.is_empty() {
            mime = part.g("source.media_type").str().trim().to_string();
        }
    }
    if data.is_empty() {
        let url = first_non_empty([
            part.g("image_url.url").str(),
            part.g("image_url").str(),
            part.g("url").str(),
        ]);
        if let Some((m, d)) = parse_data_url(&url) {
            data = d;
            if mime.is_empty() {
                mime = m;
            }
        }
    }
    if data.is_empty() {
        data = part.g("inline_data.data").str().trim().to_string();
        if mime.is_empty() {
            mime = part.g("inline_data.mime_type").str().trim().to_string();
        }
    }
    if data.is_empty() {
        return None;
    }
    if mime.is_empty() {
        mime = "image/png".into();
    }
    Some(Image {
        base64_data: data,
        mime_type: mime,
    })
}

/// Whether `obj` is a protocol envelope around `wrapper_key` (a Claude `tool_result` block, or an
/// object holding only that key plus optional `cache_control`).
fn is_protocol_wrapper_object(obj: &Res<'_>, wrapper_key: &str) -> bool {
    if !obj.is_object() || !obj.g(wrapper_key).exists() {
        return false;
    }
    let entries = obj.entries();
    if obj
        .g("type")
        .str()
        .trim()
        .eq_ignore_ascii_case("tool_result")
    {
        return entries.iter().all(|(k, _)| {
            matches!(
                *k,
                "type" | "tool_use_id" | "id" | "is_error" | "cache_control"
            ) || *k == wrapper_key
        });
    }
    entries
        .iter()
        .all(|(k, _)| *k == wrapper_key || *k == "cache_control")
}

/// A `{"type":"text","text":...}` part carrying nothing else but `cache_control`.
fn is_pure_text_part(obj: &Res<'_>) -> bool {
    obj.entries()
        .iter()
        .all(|(k, _)| matches!(*k, "type" | "text" | "cache_control"))
}

fn trimmed_raw(item: &Res<'_>) -> String {
    item.raw().trim().to_string()
}

/// Text and images of a tool result target (`result`, `output` or `content`).
fn extract_function_result_target(target: &Res<'_>) -> (String, Vec<Image>) {
    if !target.exists() {
        return (String::new(), Vec::new());
    }
    if target.is_string() {
        return (target.str(), Vec::new());
    }
    if let Some(img) = extract_image(target) {
        return (String::new(), vec![img]);
    }
    if target.is_object() {
        for key in ["content", "output", "result"] {
            if is_protocol_wrapper_object(target, key) {
                return extract_function_result_target(&target.g(key));
            }
        }
        if target.g("type").str().trim().eq_ignore_ascii_case("text") && is_pure_text_part(target) {
            return (target.g("text").str(), Vec::new());
        }
        return (target.raw(), Vec::new());
    }
    if target.is_array() {
        let mut text_parts: Vec<String> = Vec::new();
        let mut images: Vec<Image> = Vec::new();
        let mut has_structured = false;
        for item in target.array() {
            if let Some(img) = extract_image(&item) {
                images.push(img);
                has_structured = true;
                continue;
            }
            if item.is_object() {
                let wrapper = ["content", "output", "result"]
                    .into_iter()
                    .find(|k| is_protocol_wrapper_object(&item, k));
                if let Some(key) = wrapper {
                    has_structured = true;
                    let (txt, imgs) = extract_function_result_target(&item.g(key));
                    if !txt.is_empty() {
                        text_parts.push(txt);
                    }
                    images.extend(imgs);
                    continue;
                }
                if item.g("type").str().trim().eq_ignore_ascii_case("text") {
                    if is_pure_text_part(&item) {
                        has_structured = true;
                        let t = item.g("text").str();
                        if !t.is_empty() {
                            text_parts.push(t);
                        }
                    } else {
                        let raw = trimmed_raw(&item);
                        if !raw.is_empty() {
                            text_parts.push(raw);
                        }
                    }
                    continue;
                }
            }
            // Keep unconsumed items (business JSON, string parts) verbatim.
            let raw = trimmed_raw(&item);
            if !raw.is_empty() {
                text_parts.push(raw);
            }
        }
        if has_structured || !images.is_empty() {
            return (text_parts.join("\n"), images);
        }
        return (target.raw(), Vec::new());
    }
    (target.raw(), Vec::new())
}

/// `[Image N: pasted_image_N.ext]` lines announcing attached images.
fn image_headers(images: &[Image]) -> String {
    images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            format!(
                "[Image {}: pasted_image_{}.{}]",
                i + 1,
                i + 1,
                mime_extension(&img.mime_type)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prepends image headers to `text` unless it already mentions `[Image `.
fn with_image_headers(text: String, images: &[Image]) -> String {
    if images.is_empty() || text.contains("[Image ") {
        return text;
    }
    let header = image_headers(images);
    if text.is_empty() {
        header
    } else {
        format!("{header}\n\n{text}")
    }
}

/// Text and images of a tool result step/message; `{}` when empty.
fn extract_function_result_content(step: &Res<'_>) -> (String, Vec<Image>) {
    let mut target = step.g("result");
    if !target.exists() {
        target = step.g("output");
    }
    if !target.exists() {
        target = step.g("content");
    }
    if !target.exists() {
        return (EMPTY_TOOL_RESULT_PLACEHOLDER.into(), Vec::new());
    }
    let (text, images) = extract_function_result_target(&target);
    let mut text = with_image_headers(text, &images);
    if text.trim().is_empty() && images.is_empty() {
        text = EMPTY_TOOL_RESULT_PLACEHOLDER.into();
    }
    (text, images)
}

/// Text (newline-joined) and images of a user step or message.
fn extract_step_content(step: &Res<'_>) -> (String, Vec<Image>) {
    let content = step.g("content");
    let mut text_parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    let mut from_part = |p: &Res<'_>| {
        if let Some(img) = extract_image(p) {
            images.push(img);
            return;
        }
        let t = p.g("text").str();
        if !t.is_empty() {
            text_parts.push(t);
        }
    };
    if content.is_string() {
        text_parts.push(content.str());
    } else if content.is_array() {
        content.array().iter().for_each(&mut from_part);
    } else if step.g("text").exists() {
        text_parts.push(step.g("text").str());
    }
    let text = with_image_headers(text_parts.join("\n"), &images);
    (text, images)
}

/// Plain text of a model output, thought or assistant message.
fn extract_step_text(step: &Res<'_>) -> String {
    let content = step.g("content");
    if content.is_string() {
        return content.str();
    }
    if content.is_array() {
        return content
            .array()
            .iter()
            .map(|p| p.g("text").str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    step.g("text").str()
}

// ------------------------------------------------------------------ original request recovery

#[derive(Default)]
struct OriginalAssistant {
    signature: Vec<u8>,
    signature_type: String,
    thinking: String,
}

/// Fills missing signatures and thinking of assistant prompts from the original Claude request,
/// matching assistant messages to assistant prompts by position.
fn supplement_signatures_from_original(original: &Value, prompts: &mut [Prompt]) {
    let messages = original.g("messages");
    if !messages.is_array() {
        return;
    }
    let mut originals: Vec<OriginalAssistant> = Vec::new();
    for m in messages.array() {
        if !m.g("role").str().eq_ignore_ascii_case("assistant") {
            continue;
        }
        let mut meta = OriginalAssistant::default();
        let content = m.g("content");
        if content.is_array() {
            for part in content.array() {
                if part.g("type").str() != "thinking" {
                    continue;
                }
                let sig = part.g("signature").str();
                if !sig.is_empty() {
                    let (bytes, sig_type) = parse_signature_bytes(&sig);
                    if !bytes.is_empty() {
                        meta.signature = bytes;
                        meta.signature_type = sig_type;
                    }
                }
                let t = part.g("thinking").str();
                if !t.is_empty() {
                    meta.thinking = t;
                }
            }
        }
        originals.push(meta);
    }
    let mut originals = originals.into_iter();
    for p in prompts.iter_mut().filter(|p| p.source == 2) {
        let Some(orig) = originals.next() else { break };
        if p.signature.is_empty() && !orig.signature.is_empty() {
            p.signature = orig.signature;
            p.signature_type = orig.signature_type;
        }
        if p.thinking.is_empty() && !orig.thinking.is_empty() {
            p.thinking = orig.thinking;
        }
    }
}

/// Images of an original message's `content` array, or of the message itself when it is an
/// image part.
fn images_of_content(content: &Res<'_>, owner: &Res<'_>) -> Vec<Image> {
    if content.is_array() {
        content.array().iter().filter_map(extract_image).collect()
    } else {
        extract_image(owner).into_iter().collect()
    }
}

/// Re-attaches images from the original messages: user images by user-turn order, tool result
/// images strictly by tool call id.
fn supplement_images_from_original(original: &Value, prompts: &mut [Prompt]) {
    let messages = original.g("messages");
    if !messages.is_array() {
        return;
    }
    let mut user_images: Vec<Vec<Image>> = Vec::new();
    let mut tool_images: std::collections::HashMap<String, Vec<Image>> =
        std::collections::HashMap::new();

    for m in messages.array() {
        match m.g("role").str().trim().to_lowercase().as_str() {
            "user" => {
                let mut imgs = Vec::new();
                let content = m.g("content");
                if content.is_array() {
                    for part in content.array() {
                        if part
                            .g("type")
                            .str()
                            .trim()
                            .eq_ignore_ascii_case("tool_result")
                        {
                            let id =
                                first_non_empty([part.g("tool_use_id").str(), part.g("id").str()]);
                            let tool_content = part.g("content");
                            let found = images_of_content(&tool_content, &part);
                            if !found.is_empty() && !id.is_empty() {
                                tool_images.entry(id).or_default().extend(found);
                            }
                        } else if let Some(img) = extract_image(&part) {
                            imgs.push(img);
                        }
                    }
                }
                user_images.push(imgs);
            }
            "tool" => {
                let id = first_non_empty([m.g("tool_call_id").str(), m.g("id").str()]);
                let found = images_of_content(&m.g("content"), &m);
                if !found.is_empty() && !id.is_empty() {
                    tool_images.entry(id).or_default().extend(found);
                }
            }
            _ => {}
        }
    }

    let mut user_idx = 0;
    for p in prompts.iter_mut() {
        if p.source == 1 {
            if p.is_orphaned_tool {
                // Downgraded tool results never consume images of user messages.
                if p.images.is_empty()
                    && !p.original_tool_call_id.is_empty()
                    && let Some(imgs) = tool_images
                        .get(&p.original_tool_call_id)
                        .filter(|i| !i.is_empty())
                {
                    p.images = imgs.clone();
                    p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
                }
                continue;
            }
            if p.images.is_empty()
                && let Some(imgs) = user_images.get(user_idx).filter(|i| !i.is_empty())
            {
                p.images = imgs.clone();
                p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
            }
            user_idx += 1;
        } else if p.source == 4 && p.images.is_empty() && !p.tool_call_id.is_empty() {
            // Strictly by tool call id; never cross-associate images between tools.
            if let Some(imgs) = tool_images.get(&p.tool_call_id).filter(|i| !i.is_empty()) {
                p.images = imgs.clone();
                p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
            }
        }
    }
}

// ------------------------------------------------------------------ signatures

fn base64_std() -> GeneralPurpose {
    GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    )
}

/// Signature bytes plus the upstream signature type (`sealed`, `anthropic`, `openai`, `gemini`).
pub fn parse_signature_bytes(sig: &str) -> (Vec<u8>, String) {
    let s = sig.trim();
    if s.is_empty() {
        return (Vec::new(), String::new());
    }
    if s.starts_with("sealed.v1.") {
        return (s.as_bytes().to_vec(), "sealed".into());
    }
    if let Some(rest) = s.strip_prefix("claude#") {
        return (rest.as_bytes().to_vec(), "anthropic".into());
    }
    if let Some(rest) = s.strip_prefix("gpt#") {
        return (rest.as_bytes().to_vec(), "openai".into());
    }
    if let Some(rest) = s.strip_prefix("gemini#") {
        return (rest.as_bytes().to_vec(), "gemini".into());
    }
    match detect_signature_provider(s) {
        SignatureProvider::Claude => return (s.as_bytes().to_vec(), "anthropic".into()),
        SignatureProvider::Gpt => return (s.as_bytes().to_vec(), "openai".into()),
        SignatureProvider::Gemini => return (s.as_bytes().to_vec(), "gemini".into()),
        _ => {}
    }
    if s.starts_with("AY") {
        return (s.as_bytes().to_vec(), "gemini".into());
    }
    // Go's decoder skips CR and LF.
    let compact: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if let Ok(decoded) = base64_std().decode(compact.as_bytes())
        && !decoded.is_empty()
    {
        let dec_str = String::from_utf8_lossy(&decoded);
        if dec_str.starts_with("sealed.v1.") {
            return (decoded, "sealed".into());
        }
        match detect_signature_provider(&dec_str) {
            SignatureProvider::Claude => return (decoded, "anthropic".into()),
            SignatureProvider::Gpt => return (decoded, "openai".into()),
            SignatureProvider::Gemini => return (s.as_bytes().to_vec(), "gemini".into()),
            _ => {}
        }
        if dec_str.starts_with("CAQS") || dec_str.starts_with("CAIS") {
            return (decoded, "anthropic".into());
        }
        if dec_str.starts_with("gAAAA") {
            return (decoded, "openai".into());
        }
        if decoded[0] == 0x01 {
            return (s.as_bytes().to_vec(), "gemini".into());
        }
    }
    (s.as_bytes().to_vec(), detect_signature_type(s))
}

/// Signature type guessed from the signature text alone.
pub fn detect_signature_type(sig: &str) -> String {
    let s = sig.trim();
    if s.starts_with("sealed.v1.") {
        return "sealed".into();
    }
    if s.starts_with("claude#") {
        return "anthropic".into();
    }
    if s.starts_with("gpt#") {
        return "openai".into();
    }
    if s.starts_with("gemini#") {
        return "gemini".into();
    }
    match detect_signature_provider(s) {
        SignatureProvider::Claude => return "anthropic".into(),
        SignatureProvider::Gpt => return "openai".into(),
        SignatureProvider::Gemini => return "gemini".into(),
        _ => {}
    }
    if s.starts_with("CAQS") || s.starts_with("CAIS") {
        return "anthropic".into();
    }
    if s.starts_with("gAAAA") {
        return "openai".into();
    }
    if s.starts_with("AY") {
        return "gemini".into();
    }
    "sealed".into()
}

// ------------------------------------------------------------------ session ids

/// A valid UUID is kept; any other session string maps deterministically to a v5 UUID in the OID
/// namespace; blank gets a random UUID.
pub fn normalize_uuid(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return Uuid::new_v4().to_string();
    }
    if Uuid::parse_str(raw).is_ok() {
        return raw.to_string();
    }
    Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes()).to_string()
}

/// `(session id, cascade id)` ready for the wire. `fallback_session` is consulted when the payload
/// carried none (the request-scoped session, then the canonical id of the client request).
pub fn resolve_session_and_cascade_ids(
    session_id: &str,
    cascade_id: &str,
    ctx_session: &str,
    canonical_session: impl FnOnce() -> String,
) -> (String, String) {
    let mut session = session_id.to_string();
    if session.is_empty() {
        let ctx = ctx_session.trim();
        if !ctx.is_empty() {
            session = ctx.to_string();
        } else {
            session = canonical_session();
        }
    }
    let session = normalize_uuid(&session);
    let cascade = if cascade_id.is_empty() {
        session.clone()
    } else {
        normalize_uuid(cascade_id)
    };
    (session, cascade)
}

#[cfg(test)]
mod tests;
