//! OpenAI-compatible provider payload normalization (Go: helps/openai_compat_max_tokens.go and
//! openai_compat_tool_results.go).

use cpa_config::{OpenAiCompatibility, OpenAiCompatibilityModel};
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Res, Value};

fn normalize_model_name(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    parse_suffix(model).model_name.trim().to_string()
}

// ---------------------------------------------------------------- max tokens

/// Whether the resolved model under an OpenAI-compatibility config has
/// `use-max-completion-tokens`. The upstream model is looked up first (name, then alias), then
/// the requested model.
pub fn should_use_max_completion_tokens_for_model(
    compat: Option<&OpenAiCompatibility>,
    upstream_model: &str,
    requested_model: &str,
) -> bool {
    let Some(compat) = compat else {
        return false;
    };
    if let Some(use_mct) = uses_max_completion_tokens(&compat.models, upstream_model) {
        return use_mct;
    }
    uses_max_completion_tokens(&compat.models, requested_model).unwrap_or(false)
}

/// `Some(flag)` of the model matched by name, else by alias; `None` when nothing matches.
fn uses_max_completion_tokens(models: &[OpenAiCompatibilityModel], model: &str) -> Option<bool> {
    let model = normalize_model_name(model);
    if model.is_empty() {
        return None;
    }
    let by_name = models.iter().find(|m| model.eq_ignore_ascii_case(&normalize_model_name(&m.name)));
    let found = by_name.or_else(|| models.iter().find(|m| model.eq_ignore_ascii_case(&normalize_model_name(&m.alias))));
    found.map(|m| m.use_max_completion_tokens)
}

/// Normalizes `max_tokens` / `max_completion_tokens`: with `use_max_completion_tokens` the value
/// moves to `max_completion_tokens` (and `max_tokens` is removed), otherwise to `max_tokens`.
/// An existing value of the preferred field wins over the other one.
pub fn normalize_openai_max_tokens(payload: &[u8], use_max_completion_tokens: bool) -> Vec<u8> {
    if payload.is_empty() {
        return payload.to_vec();
    }
    let mut v = cpa_json::parse(payload);
    let has_max_tokens = v.g("max_tokens").exists();
    let has_max_completion = v.g("max_completion_tokens").exists();
    if !has_max_tokens && !has_max_completion {
        return payload.to_vec();
    }
    let (preferred, other, has_preferred, has_other) = if use_max_completion_tokens {
        ("max_completion_tokens", "max_tokens", has_max_completion, has_max_tokens)
    } else {
        ("max_tokens", "max_completion_tokens", has_max_tokens, has_max_completion)
    };
    if has_other && !has_preferred {
        let value = v.g(other).value();
        cpa_json::set(&mut v, preferred, value);
    }
    if has_other {
        cpa_json::delete(&mut v, other);
    }
    cpa_json::to_vec(&v)
}

// ---------------------------------------------------------------- tool results

const IMAGE_OMITTED_TEXT: &str = "[image omitted: unsupported by upstream]";
const CLAUDE_IMAGE_RELAY_NOTICE: &str = "Images returned by the preceding tool call(s):";
const CLAUDE_IMAGE_PLACEHOLDER: &str = "[Tool returned image content; the images follow in the next user message.]";

/// Whether the selected model's `input-modalities` explicitly exclude images (upstream model
/// first, then the requested one; an alias shared by several models must exclude on all of them).
pub fn should_normalize_openai_tool_results_for_model(
    compat: Option<&OpenAiCompatibility>,
    upstream_model: &str,
    requested_model: &str,
) -> bool {
    let Some(compat) = compat else {
        return false;
    };
    if let Some(normalize) = model_excludes_images(&compat.models, upstream_model) {
        return normalize;
    }
    model_excludes_images(&compat.models, requested_model).unwrap_or(false)
}

fn model_excludes_images(models: &[OpenAiCompatibilityModel], model: &str) -> Option<bool> {
    let model = normalize_model_name(model);
    if model.is_empty() {
        return None;
    }
    if let Some(m) = models.iter().find(|m| model.eq_ignore_ascii_case(&normalize_model_name(&m.name))) {
        return Some(input_modalities_exclude_images(&m.input_modalities));
    }
    let mut matched = false;
    let mut excludes = true;
    for m in models.iter().filter(|m| model.eq_ignore_ascii_case(&normalize_model_name(&m.alias))) {
        matched = true;
        if !input_modalities_exclude_images(&m.input_modalities) {
            excludes = false;
        }
    }
    matched.then_some(excludes)
}

/// Text modality listed and no image modality.
fn input_modalities_exclude_images(modalities: &[String]) -> bool {
    let mut has_text = false;
    for raw in modalities {
        match raw.trim().to_lowercase().as_str() {
            "image" => return false,
            "text" => has_text = true,
            _ => {}
        }
    }
    has_text
}

/// Converts tool message content to strings and strips relayed tool result images for text-only
/// models: text parts are preserved and image parts are replaced with a short marker. The
/// synthetic user message the Claude translator emits to relay images (notice text plus images)
/// is removed and its marker appended to the preceding tool message.
pub fn normalize_openai_tool_results_text_only(payload: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let Some(messages) = root.g("messages").value().as_array().cloned() else {
        return payload.to_vec();
    };
    if messages.is_empty() {
        return payload.to_vec();
    }
    let mut new_messages: Vec<Value> = Vec::with_capacity(messages.len());
    let mut replaced_placeholder_in_turn = false;

    for mut msg in messages {
        let role = msg.g("role").str();
        match role.as_str() {
            "tool" => {
                let content = msg.g("content");
                if content.exists() && !content.is_string() {
                    let flat = flatten_tool_result_content(&content);
                    cpa_json::set(&mut msg, "content", flat);
                } else if content.as_str() == Some(CLAUDE_IMAGE_PLACEHOLDER) {
                    cpa_json::set(&mut msg, "content", IMAGE_OMITTED_TEXT);
                    replaced_placeholder_in_turn = true;
                }
                new_messages.push(msg);
            }
            "user" => {
                let content = msg.g("content").value();
                if let Value::Array(parts) = content {
                    let (mut remaining, mut has_notice, mut has_images) = (Vec::new(), false, false);
                    for part in parts {
                        if part.is_object() {
                            if part.g("type").str() == "text" && part.g("text").str() == CLAUDE_IMAGE_RELAY_NOTICE {
                                has_notice = true;
                                continue;
                            }
                            if is_image_tool_result_part(&Res::of(&part)) {
                                has_images = true;
                                continue;
                            }
                        }
                        remaining.push(part);
                    }
                    if has_notice && has_images {
                        if !replaced_placeholder_in_turn {
                            append_marker_to_last_tool_message(&mut new_messages);
                        }
                        replaced_placeholder_in_turn = false;
                        if remaining.is_empty() {
                            // The synthetic relay message held only images: omit it.
                            continue;
                        }
                        cpa_json::set(&mut msg, "content", Value::Array(remaining));
                    }
                }
                new_messages.push(msg);
            }
            _ => {
                replaced_placeholder_in_turn = false;
                new_messages.push(msg);
            }
        }
    }
    cpa_json::set(&mut root, "messages", Value::Array(new_messages));
    cpa_json::to_vec(&root)
}

/// Appends the omitted-image marker to the nearest tool message right before the relay message.
fn append_marker_to_last_tool_message(messages: &mut [Value]) {
    let Some(prev) = messages.last_mut() else {
        return;
    };
    if prev.g("role").str() != "tool" {
        return;
    }
    let prev_content = prev.g("content").str();
    if prev_content.contains(IMAGE_OMITTED_TEXT) {
        return;
    }
    let content = if prev_content.is_empty() {
        IMAGE_OMITTED_TEXT.to_string()
    } else {
        format!("{prev_content}\n\n{IMAGE_OMITTED_TEXT}")
    };
    cpa_json::set(prev, "content", content);
}

fn flatten_tool_result_content(content: &Res<'_>) -> String {
    match content.v() {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => {
            let parts: Vec<String> = content.array().iter().filter_map(tool_result_part_text).collect();
            parts.join("\n\n")
        }
        Some(Value::Object(_)) => {
            if is_image_tool_result_part(content) {
                return IMAGE_OMITTED_TEXT.to_string();
            }
            if let Some(Value::String(text)) = content.g("text").v() {
                return text.clone();
            }
            content.raw()
        }
        _ => content.raw(),
    }
}

fn tool_result_part_text(item: &Res<'_>) -> Option<String> {
    match item.v() {
        Some(Value::String(s)) => return Some(s.clone()),
        Some(Value::Object(_)) => {
            if is_image_tool_result_part(item) {
                return Some(IMAGE_OMITTED_TEXT.to_string());
            }
            if let Some(Value::String(text)) = item.g("text").v() {
                return Some(text.clone());
            }
        }
        _ => {}
    }
    let raw = item.raw();
    (!raw.is_empty()).then_some(raw)
}

fn is_image_tool_result_part(item: &Res<'_>) -> bool {
    if !item.is_object() {
        return false;
    }
    let is_image_type = matches!(
        item.g("type").str().trim().to_lowercase().as_str(),
        "image" | "image_url" | "input_image"
    );
    is_image_type || item.g("image_url").exists() || item.g("input_image").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str, alias: &str, mct: bool, modalities: &[&str]) -> OpenAiCompatibilityModel {
        OpenAiCompatibilityModel {
            name: name.into(),
            alias: alias.into(),
            use_max_completion_tokens: mct,
            input_modalities: modalities.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn max_tokens_follow_model_preference() {
        let compat = OpenAiCompatibility {
            models: vec![model("o-model", "alias-a", true, &[]), model("plain", "alias-b", false, &[])],
            ..Default::default()
        };
        assert!(should_use_max_completion_tokens_for_model(Some(&compat), "O-Model(high)", "x"));
        // Name wins over alias; the requested model is the fallback.
        assert!(should_use_max_completion_tokens_for_model(Some(&compat), "unknown", "alias-a"));
        assert!(!should_use_max_completion_tokens_for_model(Some(&compat), "plain", "alias-a"));
        assert!(!should_use_max_completion_tokens_for_model(None, "o-model", "o-model"));

        let to_mct = normalize_openai_max_tokens(br#"{"max_tokens":5,"x":1}"#, true);
        assert_eq!(String::from_utf8(to_mct).unwrap(), r#"{"x":1,"max_completion_tokens":5}"#);
        let to_mt = normalize_openai_max_tokens(br#"{"max_completion_tokens":7}"#, false);
        assert_eq!(String::from_utf8(to_mt).unwrap(), r#"{"max_tokens":7}"#);
        // The preferred field wins when both are present.
        let both = normalize_openai_max_tokens(br#"{"max_tokens":1,"max_completion_tokens":2}"#, true);
        assert_eq!(String::from_utf8(both).unwrap(), r#"{"max_completion_tokens":2}"#);
        assert_eq!(normalize_openai_max_tokens(br#"{"a":1}"#, true), br#"{"a":1}"#.to_vec());
    }

    #[test]
    fn modalities_gate_tool_result_normalization() {
        let compat = OpenAiCompatibility {
            models: vec![
                model("text-only", "shared", false, &["text"]),
                model("vision", "shared", false, &["text", "image"]),
                model("alias-only", "just-alias", false, &["Text"]),
            ],
            ..Default::default()
        };
        assert!(should_normalize_openai_tool_results_for_model(Some(&compat), "text-only", ""));
        assert!(!should_normalize_openai_tool_results_for_model(Some(&compat), "vision", ""));
        // An alias matching models with different capabilities does not exclude images.
        assert!(!should_normalize_openai_tool_results_for_model(Some(&compat), "shared", ""));
        assert!(should_normalize_openai_tool_results_for_model(Some(&compat), "none", "just-alias"));
        assert!(!should_normalize_openai_tool_results_for_model(Some(&compat), "none", "other"));
    }

    #[test]
    fn tool_results_become_text_and_relay_images_are_dropped() {
        let payload = serde_json::json!({"messages":[
            {"role":"user","content":"go"},
            {"role":"tool","tool_call_id":"1","content":[{"type":"text","text":"a"},{"type":"image_url","image_url":{"url":"x"}},"b"]},
            {"role":"tool","tool_call_id":"2","content":"fine"},
            {"role":"user","content":[
                {"type":"text","text":CLAUDE_IMAGE_RELAY_NOTICE},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]},
            {"role":"assistant","content":"done"}
        ]});
        let out = cpa_json::parse(&normalize_openai_tool_results_text_only(&serde_json::to_vec(&payload).unwrap()));
        assert_eq!(out.g("messages.#").int(), 4);
        assert_eq!(out.g("messages.1.content").str(), format!("a\n\n{IMAGE_OMITTED_TEXT}\n\nb"));
        // The relay message is gone and its marker lands on the preceding tool message.
        assert_eq!(out.g("messages.2.content").str(), format!("fine\n\n{IMAGE_OMITTED_TEXT}"));
        assert_eq!(out.g("messages.3.role").str(), "assistant");
    }

    #[test]
    fn placeholder_content_is_replaced_without_double_marker() {
        let payload = serde_json::json!({"messages":[
            {"role":"tool","tool_call_id":"1","content":CLAUDE_IMAGE_PLACEHOLDER},
            {"role":"user","content":[
                {"type":"text","text":CLAUDE_IMAGE_RELAY_NOTICE},
                {"type":"image_url","image_url":{"url":"x"}},
                {"type":"text","text":"keep"}]}
        ]});
        let out = cpa_json::parse(&normalize_openai_tool_results_text_only(&serde_json::to_vec(&payload).unwrap()));
        assert_eq!(out.g("messages.0.content").str(), IMAGE_OMITTED_TEXT);
        assert_eq!(out.g("messages.1.content.#").int(), 1);
        assert_eq!(out.g("messages.1.content.0.text").str(), "keep");
    }
}
