//! Helpers shared by the interactions <-> Gemini converters (Go: interactions_gemini_common.go).

use cpa_json::{json, Value, J};

use crate::common::normalize_openai_file_data;

/// First non-blank value, trimmed (Go: firstNonEmptyString).
pub(super) fn first_non_empty_string(values: &[&str]) -> String {
    values.iter().map(|v| v.trim()).find(|v| !v.is_empty()).unwrap_or_default().to_string()
}

/// First value that is not blank, returned as-is (Go: firstNonEmptyInteractionString).
pub(super) fn first_non_empty_interaction_string(values: &[&str]) -> String {
    values.iter().find(|v| !v.trim().is_empty()).map(|v| v.to_string()).unwrap_or_default()
}

/// A Gemini content turn.
pub(super) fn gemini_content(role: &str, parts: Vec<Value>) -> Value {
    json!({ "role": role, "parts": parts })
}

pub(super) fn gemini_text_part_json(text: &str, thought: bool) -> Value {
    let mut part = json!({ "text": text });
    if thought {
        cpa_json::set(&mut part, "thought", true);
    }
    part
}

/// `inlineData` part from an inline object (`mimeType` or `mime_type`, `data`); `None` unless
/// both are present.
pub(super) fn gemini_inline_data_part_json(inline: &Value) -> Option<Value> {
    let mut mime_type = inline.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline.g("mime_type").str();
    }
    let data = inline.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({ "inlineData": { "mimeType": mime_type, "data": data } }))
}

fn inline_data_part(mime_type: &str, data: &str) -> Option<Value> {
    gemini_inline_data_part_json(&json!({ "mime_type": mime_type, "data": data }))
}

/// `fileData` part from a file data object; `None` unless mime type and uri are present.
pub(super) fn gemini_file_data_part_json(file_data: &Value) -> Option<Value> {
    let mut mime_type = file_data.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = file_data.g("mime_type").str();
    }
    let mut file_uri = file_data.g("fileUri").str();
    if file_uri.is_empty() {
        file_uri = file_data.g("file_uri").str();
    }
    if mime_type.is_empty() || file_uri.is_empty() {
        return None;
    }
    Some(json!({ "fileData": { "mimeType": mime_type, "fileUri": file_uri } }))
}

/// `inlineData` part from a `data:<mime>;base64,<data>` URL.
fn gemini_inline_data_part_from_data_url(data_url: &str) -> Option<Value> {
    let payload = data_url.strip_prefix("data:")?;
    let (mime_type, rest) = payload.split_once(';')?;
    let data = rest.strip_prefix("base64,")?;
    inline_data_part(mime_type, data)
}

fn interactions_input_audio_mime_type(format: &str) -> &'static str {
    match format.trim().to_lowercase().as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "pcm16" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// Converts one Interactions content block (text, inline data, media by data or uri, image_url,
/// input_audio, file) into a Gemini part; `None` for unsupported or incomplete blocks.
pub(super) fn interactions_content_part_to_gemini_part(part: &Value, thought: bool) -> Option<Value> {
    let text = part.g("text");
    if text.exists() {
        return Some(gemini_text_part_json(&text.str(), thought));
    }
    for key in ["inline_data", "inlineData"] {
        let inline = part.g(key);
        if inline.exists() {
            return gemini_inline_data_part_json(&inline.value());
        }
    }
    match part.g("type").str().trim().to_lowercase().as_str() {
        "image" | "audio" | "video" | "document" => {
            let mime = part.g("mime_type");
            if mime.exists() || part.g("mimeType").exists() {
                let mut mime_type = mime.str();
                if mime_type.is_empty() {
                    mime_type = part.g("mimeType").str();
                }
                let data = part.g("data").str();
                if !data.is_empty() {
                    return inline_data_part(&mime_type, &data);
                }
            }
            let uri = part.g("file_uri");
            if uri.exists() || part.g("fileUri").exists() {
                let mut file_uri = uri.str();
                if file_uri.is_empty() {
                    file_uri = part.g("fileUri").str();
                }
                let mut mime_type = part.g("mime_type").str();
                if mime_type.is_empty() {
                    mime_type = part.g("mimeType").str();
                }
                return gemini_file_data_part_json(&json!({ "mimeType": mime_type, "fileUri": file_uri }));
            }
            let url = part.g("url");
            if url.exists() {
                return gemini_inline_data_part_from_data_url(&url.str());
            }
        }
        "image_url" => return gemini_inline_data_part_from_data_url(&part.g("image_url.url").str()),
        "input_audio" => {
            let mime_type = interactions_input_audio_mime_type(&part.g("input_audio.format").str());
            return inline_data_part(mime_type, &part.g("input_audio.data").str());
        }
        "file" => {
            let filename = part.g("file.filename").str();
            let file_data = part.g("file.file_data").str();
            if let Some((mime_type, data)) = normalize_openai_file_data(&filename, "", &file_data) {
                return inline_data_part(&mime_type, &data);
            }
        }
        _ => {}
    }
    None
}

/// Interactions content block for inline data: `image`, `audio`, `video`, or `document` by MIME.
pub(super) fn gemini_inline_data_to_interactions_content(mime_type: &str, data: &str) -> Value {
    let lower = mime_type.to_lowercase();
    let content_type = if lower.starts_with("image/") {
        "image"
    } else if lower.starts_with("audio/") {
        "audio"
    } else if lower.starts_with("video/") {
        "video"
    } else {
        "document"
    };
    json!({ "type": content_type, "mime_type": mime_type, "data": data })
}

/// A Gemini part's thought signature (first non-blank of the known paths).
pub(super) fn interactions_thought_signature(part: &Value) -> String {
    for path in ["thoughtSignature", "thought_signature", "extra_content.google.thought_signature"] {
        let signature = part.g(path).str().trim().to_string();
        if !signature.is_empty() {
            return signature;
        }
    }
    String::new()
}

/// An Interactions `thought` step carrying a signature and/or thought text.
fn gemini_thought_step_json(sig: &str, text: &str) -> Value {
    let mut step = json!({ "type": "thought" });
    if !sig.is_empty() {
        cpa_json::set(&mut step, "signature", sig);
    }
    if !text.is_empty() {
        cpa_json::set(&mut step, "content", json!([{ "text": text }]));
    }
    step
}

fn model_output_step(item: Value, sig: &str) -> Vec<Value> {
    let mut steps = vec![json!({ "type": "model_output", "content": [item] })];
    if !sig.is_empty() {
        steps.push(gemini_thought_step_json(sig, ""));
    }
    steps
}

/// Converts a Gemini part into Interactions steps (a signature becomes an extra `thought` step).
pub(super) fn gemini_part_to_interactions_steps(part: &Value) -> Vec<Value> {
    let sig = interactions_thought_signature(part);
    let fc = part.g("functionCall");
    if fc.exists() {
        let mut steps = Vec::new();
        if !sig.is_empty() {
            steps.push(gemini_thought_step_json(&sig, ""));
        }
        let mut step = json!({ "type": "function_call", "name": "", "arguments": {} });
        cpa_json::set(&mut step, "name", fc.g("name").str());
        let id = fc.g("id");
        let call_id = fc.g("call_id");
        if id.exists() {
            cpa_json::set(&mut step, "call_id", id.str());
        } else if call_id.exists() {
            cpa_json::set(&mut step, "call_id", call_id.str());
        }
        let args = fc.g("args");
        if args.exists() {
            cpa_json::set(&mut step, "arguments", args.value());
        }
        steps.push(step);
        return steps;
    }
    let fr = part.g("functionResponse");
    if fr.exists() {
        let mut step = json!({ "type": "function_result", "name": "", "result": {} });
        cpa_json::set(&mut step, "name", fr.g("name").str());
        let id = fr.g("id");
        let call_id = fr.g("call_id");
        if id.exists() {
            cpa_json::set(&mut step, "call_id", id.str());
        } else if call_id.exists() {
            cpa_json::set(&mut step, "call_id", call_id.str());
        }
        let response = fr.g("response");
        if response.exists() {
            cpa_json::set(&mut step, "result", response.value());
        }
        return vec![step];
    }
    let text = part.g("text");
    if text.exists() {
        if part.g("thought").bool() {
            return vec![gemini_thought_step_json(&sig, &text.str())];
        }
        if text.str().is_empty() {
            return if sig.is_empty() { vec![] } else { vec![gemini_thought_step_json(&sig, "")] };
        }
        return model_output_step(json!({ "text": text.str() }), &sig);
    }
    let inline = part.g("inlineData");
    if inline.exists() {
        let mut mime_type = inline.g("mimeType").str();
        if mime_type.is_empty() {
            mime_type = inline.g("mime_type").str();
        }
        let item = gemini_inline_data_to_interactions_content(&mime_type, &inline.g("data").str());
        return model_output_step(item, &sig);
    }
    let inline = part.g("inline_data");
    if inline.exists() {
        let item = gemini_inline_data_to_interactions_content(&inline.g("mime_type").str(), &inline.g("data").str());
        return model_output_step(item, &sig);
    }
    if !sig.is_empty() {
        return vec![gemini_thought_step_json(&sig, "")];
    }
    vec![]
}
