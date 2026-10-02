//! Claude system prompt helpers (Go: common/claude_system.go).

use cpa_core::util::is_claude_code_attribution_system_text;
use cpa_json::Res;

const CLAUDE_SYSTEM_REMINDER_START: &str = "<system-reminder>";
const CLAUDE_SYSTEM_REMINDER_END: &str = "</system-reminder>";

const JSON_OBJECT_INSTRUCTION: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";

/// Wraps text in the `<system-reminder>` envelope so non-Claude upstream formats treat demoted
/// mid-session system or developer instructions as system directives rather than user speech.
pub fn system_reminder_text(text: &str) -> String {
    format!("{CLAUDE_SYSTEM_REMINDER_START}\n{text}\n{CLAUDE_SYSTEM_REMINDER_END}")
}

/// Converts a Claude message-level system value (string or text blocks, Claude Code attribution
/// blocks skipped) into ordinary reminder text for non-Claude upstream formats. `None` when
/// nothing but whitespace remains.
pub fn claude_message_system_reminder_text(content: &Res<'_>) -> Option<String> {
    let parts = claude_system_text_parts(content);
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(system_reminder_text(&text))
}

fn claude_system_text_parts(content: &Res<'_>) -> Vec<String> {
    if !content.exists() {
        return Vec::new();
    }
    if let Some(text) = content.as_str() {
        if text.is_empty() || is_claude_code_attribution_system_text(text) {
            return Vec::new();
        }
        return vec![text.to_string()];
    }
    if !content.is_array() {
        return Vec::new();
    }
    content
        .array()
        .iter()
        .filter(|item| item.g("type").str() == "text")
        .map(|item| item.g("text").str())
        .filter(|text| !text.is_empty() && !is_claude_code_attribution_system_text(text))
        .collect()
}

/// Formats structured output settings (Chat Completions `response_format` or Responses
/// `text.format`) as explicit instructions to inject into Claude's system prompt. Empty for
/// absent or unsupported formats.
pub fn build_claude_structured_output_instruction(format: &Res<'_>) -> String {
    if !format.exists() {
        return String::new();
    }

    let format_type = format.g("type").str().trim().to_lowercase();
    match format_type.as_str() {
        "json_object" => JSON_OBJECT_INSTRUCTION.to_string(),
        "json_schema" => {
            let json_schema = format.g("json_schema");
            let mut schema = json_schema.g("schema");
            if !schema.exists() {
                schema = format.g("schema");
            }
            if !schema.exists() {
                return JSON_OBJECT_INSTRUCTION.to_string();
            }

            let mut out = String::from(
                "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n",
            );
            let mut name = json_schema.g("name").str().trim().to_string();
            if name.is_empty() {
                name = format.g("name").str().trim().to_string();
            }
            if !name.is_empty() {
                out.push_str("Schema Name: ");
                out.push_str(&name);
                out.push('\n');
            }
            let mut desc = json_schema.g("description").str().trim().to_string();
            if desc.is_empty() {
                desc = format.g("description").str().trim().to_string();
            }
            if !desc.is_empty() {
                out.push_str("Schema Description: ");
                out.push_str(&desc);
                out.push('\n');
            }
            out.push_str("JSON Schema:\n");
            out.push_str(&schema.raw());
            out.push_str("\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.");
            out
        }
        _ => String::new(),
    }
}
