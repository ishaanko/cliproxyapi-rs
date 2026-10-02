//! Request-level helpers (Go: common/request.go).

use cpa_json::J;
use rand::RngCore;

const TOOLU_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// A random tool use ID: `toolu_` plus 24 alphanumeric characters, drawn by rejection sampling so
/// every one of the 62 characters is equally likely.
pub fn generate_claude_tool_call_id() -> String {
    // 248: the largest exact multiple of 62 below 256.
    const MAX_VALID_BYTE: usize = 256 - (256 % TOOLU_LETTERS.len());
    let mut id = String::with_capacity("toolu_".len() + 24);
    id.push_str("toolu_");
    let mut rng = rand::rng();
    let mut buf = [0u8; 32];
    let mut n = 0;
    while n < 24 {
        rng.fill_bytes(&mut buf);
        for &byte in &buf {
            if usize::from(byte) < MAX_VALID_BYTE {
                id.push(char::from(TOOLU_LETTERS[usize::from(byte) % TOOLU_LETTERS.len()]));
                n += 1;
                if n == 24 {
                    break;
                }
            }
        }
    }
    id
}

/// The model name from the original request, falling back to the translated request when the
/// original is unavailable. The first non-blank string at `model`, then `request.model`.
pub fn request_model_name(original_request_raw_json: &[u8], request_raw_json: &[u8]) -> String {
    [original_request_raw_json, request_raw_json]
        .into_iter()
        .map(model_name_of)
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}

fn model_name_of(raw_json: &[u8]) -> String {
    if raw_json.is_empty() || !cpa_json::valid(raw_json) {
        return String::new();
    }
    let root = cpa_json::parse(raw_json);
    for path in ["model", "request.model"] {
        let model = root.g(path);
        if let Some(name) = model.as_str()
            && !name.trim().is_empty()
        {
            return name.to_string();
        }
    }
    String::new()
}
