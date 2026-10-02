//! Deterministic Claude `metadata.user_id` (Go: common/claude_user_id.go).

use cpa_json::{J, Res, Value};
use sha2::{Digest, Sha256};

use super::is_gemini_thought_part;

/// A stable value for the Claude request field `metadata.user_id`. A caller-supplied
/// `metadata.user_id` or OpenAI `user` field is preserved; otherwise the value is derived from
/// stable client signals (`prompt_cache_key`, `session_id`, `conversation_id`, first user message
/// content, then model/system instructions) so the same conversation gets the same user_id on
/// every worker and turn. `"unknown"` when there is nothing to hash.
pub fn derive_claude_user_id(raw_json: &[u8]) -> String {
    let root = cpa_json::parse(raw_json);

    for path in ["metadata.user_id", "user"] {
        if let Some(raw) = root.g(path).as_str()
            && !raw.trim().is_empty()
        {
            return raw.to_string();
        }
    }

    let mut seed = String::new();

    if let Some(value) = trimmed_if_exists(&root, "prompt_cache_key") {
        add_seed(&mut seed, "prompt_cache_key:", &value);
    }

    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            if let Some(value) = trimmed_if_exists(&root, path) {
                add_seed(&mut seed, "session_id:", &value);
                break;
            }
        }
    }

    if seed.is_empty() {
        let conversation = root.g("conversation");
        let sid = conversation.g("id").str().trim().to_string();
        if !sid.is_empty() {
            add_seed(&mut seed, "conversation_id:", &sid);
        } else if let Some(text) = conversation.as_str() {
            let sid = text.trim();
            if !sid.is_empty() {
                add_seed(&mut seed, "conversation_id:", sid);
            }
        } else if let Some(value) = trimmed_if_exists(&root, "conversation_id") {
            add_seed(&mut seed, "conversation_id:", &value);
        }
    }

    if seed.is_empty() {
        let content = first_stable_request_content(&root);
        if !content.is_empty() {
            add_seed(&mut seed, "content:", &content);
        }
    }

    if seed.is_empty() {
        if let Some(value) = trimmed_if_exists(&root, "model") {
            add_seed(&mut seed, "model:", &value);
        }
        for (label, path) in [
            (";instructions:", "instructions"),
            (";system:", "system"),
            (";systemInstruction:", "systemInstruction"),
            (";system_instruction:", "system_instruction"),
        ] {
            let v = root.g(path);
            if v.exists() {
                add_seed(&mut seed, label, &v.str());
            }
        }
    }

    if seed.is_empty() {
        return "unknown".to_string();
    }
    hex::encode(Sha256::digest(seed.as_bytes()))
}

fn add_seed(seed: &mut String, label: &str, value: &str) {
    seed.push_str(label);
    seed.push_str(value);
}

/// The trimmed string value at `path` when the path exists and is non-blank after trimming.
fn trimmed_if_exists(root: &Value, path: &str) -> Option<String> {
    let v = root.g(path);
    if !v.exists() {
        return None;
    }
    let value = v.str().trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// First stable user text of the request: Chat Completions/Claude `messages`, then Responses
/// `input`, then Gemini `contents`.
fn first_stable_request_content(root: &Value) -> String {
    let messages = root.g("messages");
    if messages.is_array() {
        for message in messages.array() {
            let role = message.g("role").str().trim().to_lowercase();
            if role == "user" {
                let content = extract_text_content(&message.g("content"));
                if !content.is_empty() {
                    return content;
                }
            }
        }
    }

    let input = root.g("input");
    if input.exists() {
        if let Some(text) = input.as_str() {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        } else if input.is_array() {
            for item in input.array() {
                if is_responses_user_item(&item) {
                    let content = extract_responses_item_text(&item.g("content"));
                    if !content.is_empty() {
                        return content;
                    }
                }
            }
        }
    }

    let contents = root.g("contents");
    if contents.is_array() {
        for content_item in contents.array() {
            let role = content_item.g("role").str().trim().to_lowercase();
            // In Gemini API format a missing role defaults to "user".
            if role.is_empty() || role == "user" {
                let parts = content_item.g("parts");
                if parts.is_array() {
                    let texts: Vec<String> = parts
                        .array()
                        .iter()
                        .filter(|part| !is_gemini_thought_part(part))
                        .filter_map(|part| {
                            let text = part.g("text");
                            let value = text.exists().then(|| text.str().trim().to_string())?;
                            (!value.is_empty()).then_some(value)
                        })
                        .collect();
                    if !texts.is_empty() {
                        return texts.join("\n");
                    }
                }
            }
        }
    }

    String::new()
}

fn extract_text_content(content: &Res<'_>) -> String {
    if let Some(text) = content.as_str() {
        return text.trim().to_string();
    }
    if !content.is_array() {
        return String::new();
    }
    let texts: Vec<String> = content
        .array()
        .iter()
        .filter(|part| part.g("type").str() == "text")
        .filter_map(|part| trimmed_text(part))
        .collect();
    texts.join("\n").trim().to_string()
}

fn is_responses_user_item(item: &Res<'_>) -> bool {
    let role = item.g("role").str().trim().to_lowercase();
    if role == "user" {
        return true;
    }
    if matches!(role.as_str(), "system" | "developer" | "assistant") {
        return false;
    }
    // A non-assistant, non-system message defaults to user.
    item.g("type").str().trim().to_lowercase() == "message"
}

fn extract_responses_item_text(content: &Res<'_>) -> String {
    if let Some(text) = content.as_str() {
        return text.trim().to_string();
    }
    if !content.is_array() {
        return String::new();
    }
    let texts: Vec<String> = content
        .array()
        .iter()
        .filter(|part| matches!(part.g("type").str().as_str(), "input_text" | "output_text" | "text"))
        .filter_map(|part| trimmed_text(part))
        .collect();
    texts.join("\n").trim().to_string()
}

/// The trimmed `text` field of a part when it exists and is non-blank.
fn trimmed_text(part: &Res<'_>) -> Option<String> {
    let text = part.g("text");
    if !text.exists() {
        return None;
    }
    let value = text.str().trim().to_string();
    (!value.is_empty()).then_some(value)
}
