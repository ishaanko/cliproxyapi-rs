//! Claude-facing helpers: model checks, tool ids and names, attribution blocks, tool results
//! (Go: util/claude_model.go, claude_tool_id.go, claude_attribution.go, claude_tool_result.go).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cpa_json::{J, Value};
use regex::Regex;
use sha2::{Digest, Sha256};

use super::gojson::go_json_canonicalize;

// ---------------------------------------------------------------- model checks

/// Whether the model name identifies a Claude model (case-insensitive substring `claude`).
pub fn is_claude_model(model: &str) -> bool {
    model.to_lowercase().contains("claude")
}

/// Whether the model is a Claude thinking model (needs the interleaved-thinking beta header).
pub fn is_claude_thinking_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("claude") && lower.contains("thinking")
}

// ---------------------------------------------------------------- tool ids and names

const GEMINI_CLAUDE_TOOL_USE_ID_PREFIX: &str = "cpa_gemini_";

static CLAUDE_NAME_SANITIZER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_-]").expect("static regex"));
static CLAUDE_TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Makes an id conform to Claude's `tool_use.id` regex `^[a-zA-Z0-9_-]+$`: other characters become
/// `_`; an empty result gets a generated `toolu_<unix nanos>_<counter>` fallback.
pub fn sanitize_claude_tool_id(id: &str) -> String {
    let s = CLAUDE_NAME_SANITIZER.replace_all(id, "_").into_owned();
    if !s.is_empty() {
        return s;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = CLAUDE_TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("toolu_{nanos}_{counter}")
}

/// Makes a tool name conform to Claude's `^[a-zA-Z0-9_-]{1,64}$`: other characters (dots and
/// colons from MCP tools included) become `_`, capped at 64 bytes. Empty stays empty.
pub fn sanitize_claude_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut s = CLAUDE_NAME_SANITIZER.replace_all(name, "_").into_owned();
    s.truncate(64); // ASCII only after replacement
    if s.is_empty() {
        s = "_".into();
    }
    s
}

/// Stable Claude-facing id for a provider-native Gemini function call:
/// `cpa_gemini_` + hex(sha256(callID \0 name \0 canonical args)[..16]). Empty when callID or name
/// is blank. `args_raw` is canonicalized like Go (sorted keys, float64 numbers, HTML escaping)
/// when it is valid JSON, else trimmed as-is.
pub fn gemini_claude_tool_use_id(call_id: &str, name: &str, args_raw: &str) -> String {
    let call_id = call_id.trim();
    let name = name.trim();
    if call_id.is_empty() || name.is_empty() {
        return String::new();
    }
    let mut args = args_raw.to_string();
    if !args_raw.trim().is_empty() {
        args = go_json_canonicalize(args_raw).unwrap_or_else(|| args_raw.trim().to_string());
    }
    let digest = Sha256::digest([call_id, name, &args].join("\0").as_bytes());
    format!(
        "{GEMINI_CLAUDE_TOOL_USE_ID_PREFIX}{}",
        hex::encode(&digest[..16])
    )
}

/// Whether `id` belongs to the reserved Claude-facing Gemini provenance namespace.
pub fn is_gemini_claude_tool_use_id(id: &str) -> bool {
    let Some(digest) = id.trim().strip_prefix(GEMINI_CLAUDE_TOOL_USE_ID_PREFIX) else {
        return false;
    };
    digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------- attribution

const CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

/// Whether `text` is the Claude Code attribution block carrying per-request billing and prompt
/// fingerprint data (leading whitespace ignored).
pub fn is_claude_code_attribution_system_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_SYSTEM_PREFIX)
}

/// Removes Claude Code billing/CCH attribution blocks from a Messages body, keeping other system
/// content. Takes and returns body bytes; the input is returned byte-for-byte when nothing is
/// removed (otherwise the body is re-serialized compactly).
pub fn strip_claude_code_attribution_system(payload: &[u8]) -> Vec<u8> {
    let root = cpa_json::parse(payload);
    let system = root.g("system");
    let Some(system_value) = system.v() else {
        return payload.to_vec();
    };
    let mut updated = root.clone();
    match system_value {
        Value::String(s) => {
            if !is_claude_code_attribution_system_text(s) {
                return payload.to_vec();
            }
            cpa_json::delete(&mut updated, "system");
        }
        Value::Array(blocks) => {
            let kept: Vec<Value> = blocks
                .iter()
                .filter(|block| {
                    !(block.g("type").str() == "text"
                        && is_claude_code_attribution_system_text(&block.g("text").str()))
                })
                .cloned()
                .collect();
            if kept.len() == blocks.len() {
                return payload.to_vec();
            }
            if kept.is_empty() {
                cpa_json::delete(&mut updated, "system");
            } else {
                cpa_json::set(&mut updated, "system", Value::Array(kept));
            }
        }
        _ => return payload.to_vec(),
    }
    cpa_json::to_vec(&updated)
}

// ---------------------------------------------------------------- tool result

/// A base64 image extracted from a Claude `tool_result` content block, to be emitted as a
/// provider-specific inline data part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeToolResultImage {
    pub mime_type: String,
    pub data: String,
}

/// Normalized Claude `tool_result` `content`, ready for a Gemini-style functionResponse.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaudeToolResult {
    /// Value for `functionResponse.response.result`.
    pub result: String,
    /// True when `result` holds raw JSON (write with a raw set), false for a plain string. Writing
    /// raw JSON text as a string value would double-encode it, so callers must honor this.
    pub result_is_raw: bool,
    /// Base64 image blocks separated out of the content.
    pub images: Vec<ClaudeToolResultImage>,
}

/// Normalizes a Claude `tool_result` `content` field (`None` when absent):
/// string -> plain string result; one non-image block -> raw JSON result; several -> raw JSON
/// array; base64 image blocks -> `images` (blocks without data are dropped); an object -> raw JSON
/// or an image; absent -> empty result. Raw results are compact JSON.
pub fn convert_claude_tool_result_content(content: Option<&Value>) -> ClaudeToolResult {
    let Some(content) = content else {
        return ClaudeToolResult::default();
    };
    match content {
        Value::String(s) => ClaudeToolResult {
            result: s.clone(),
            ..Default::default()
        },
        Value::Array(blocks) => {
            let mut images = Vec::new();
            let mut non_image: Vec<&Value> = Vec::new();
            for block in blocks {
                if is_claude_base64_image(block) {
                    images.extend(claude_image_from_block(block));
                } else {
                    non_image.push(block);
                }
            }
            match non_image.len() {
                0 => ClaudeToolResult {
                    images,
                    ..Default::default()
                },
                1 => ClaudeToolResult {
                    result: non_image[0].to_string(),
                    result_is_raw: true,
                    images,
                },
                _ => ClaudeToolResult {
                    result: Value::Array(non_image.into_iter().cloned().collect()).to_string(),
                    result_is_raw: true,
                    images,
                },
            }
        }
        Value::Object(_) => {
            if is_claude_base64_image(content) {
                return ClaudeToolResult {
                    images: claude_image_from_block(content).into_iter().collect(),
                    ..Default::default()
                };
            }
            ClaudeToolResult {
                result: content.to_string(),
                result_is_raw: true,
                images: vec![],
            }
        }
        other => ClaudeToolResult {
            result: other.to_string(),
            result_is_raw: true,
            images: vec![],
        },
    }
}

fn is_claude_base64_image(block: &Value) -> bool {
    block.g("type").str() == "image" && block.g("source.type").str() == "base64"
}

/// Extracts image data from a base64 image block; `None` when it carries no data.
fn claude_image_from_block(block: &Value) -> Option<ClaudeToolResultImage> {
    let data = block.g("source.data").str();
    if data.is_empty() {
        return None;
    }
    Some(ClaudeToolResultImage {
        mime_type: block.g("source.media_type").str(),
        data,
    })
}
