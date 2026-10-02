//! Sanitizing replayed reasoning items in OpenAI Responses requests (Go:
//! internal/runtime/executor/openai_responses_signature.go, shared by the Responses-style
//! upstreams: Codex, xAI, OpenAI-compatible).

use cpa_core::signature::inspect_gpt_reasoning_signature;
use cpa_json::{J, Kind, Value};
use serde_json::json;

/// [`sanitize_openai_responses_reasoning_encrypted_content_with_compat`] for official upstreams.
pub fn sanitize_openai_responses_reasoning_encrypted_content(provider: &str, body: &[u8]) -> Vec<u8> {
    sanitize_openai_responses_reasoning_encrypted_content_with_compat(provider, body, false)
}

fn summary_is_empty(summary: Option<&Value>) -> bool {
    match summary {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// Copies cleartext `reasoning_text` parts of `content` into `summary` as `summary_text` parts;
/// leaves the item untouched when there are none.
fn promote_reasoning_text_to_summary(item: &mut Value, content: &[Value]) {
    let parts: Vec<Value> = content
        .iter()
        .filter(|part| part.g("type").str().trim() == "reasoning_text")
        .map(|part| part.g("text").str())
        .filter(|text| !text.is_empty())
        .map(|text| json!({"type": "summary_text", "text": text}))
        .collect();
    if !parts.is_empty() {
        cpa_json::set(item, "summary", Value::Array(parts));
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Null => "Null",
        Kind::False => "False",
        Kind::Number => "Number",
        Kind::String => "String",
        Kind::True => "True",
        Kind::Json => "JSON",
    }
}

/// Fixes reasoning items in `input` so the upstream accepts the replay:
/// - cleartext `reasoning.content` is promoted into an empty `summary` and cleared (official
///   schema allows no content; skipped for `is_compat` third-party models such as DeepSeek that
///   need `reasoning_text` replayed);
/// - an invalid `encrypted_content` (whitespace, null, non-string, or failing the GPT signature
///   shape check) is dropped;
/// - orphan reasoning ids (no usable encrypted content) are dropped unless the request opts into
///   `store=true`, because the Codex backend would treat them as a store lookup and fail with
///   `Item with id '...' not found`.
///
/// Returns the same bytes when nothing needed editing.
pub fn sanitize_openai_responses_reasoning_encrypted_content_with_compat(
    provider: &str,
    body: &[u8],
    is_compat: bool,
) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    let Some(Value::Array(items)) = root.g("input").v().cloned() else {
        return body.to_vec();
    };
    let provider = match provider.trim() {
        "" => "openai responses upstream",
        p => p,
    };
    let strip_orphan_ids = !root.g("store").bool();
    let mut items = items;
    let mut changed_any = false;

    for (index, slot) in items.iter_mut().enumerate() {
        let item = slot.clone();
        if item.g("type").str().trim() != "reasoning" {
            continue;
        }
        let encrypted = item.g("encrypted_content");
        let item_id = match item.g("id").str().trim() {
            "" => format!("input[{index}]"),
            id => id.to_string(),
        };
        let has_id = item.g("id").exists();
        let mut next = item.clone();
        let mut changed = false;

        if !is_compat && let Some(Value::Array(content)) = item.g("content").v() && !content.is_empty() {
            if summary_is_empty(item.g("summary").v()) {
                promote_reasoning_text_to_summary(&mut next, content);
            }
            cpa_json::set(&mut next, "content", Value::Array(Vec::new()));
            changed = true;
            tracing::debug!("{provider}: cleared reasoning content at input[{index}] item_id={item_id:?}");
        }

        if !encrypted.exists() {
            if !is_compat && strip_orphan_ids && has_id {
                cpa_json::delete(&mut next, "id");
                changed = true;
                tracing::debug!(
                    "{provider}: dropped orphan reasoning id at input[{index}] item_id={item_id:?} reason=missing encrypted_content with store disabled"
                );
            }
            if changed {
                *slot = next;
                changed_any = true;
            }
            continue;
        }

        let reason = match encrypted.v() {
            Some(Value::String(raw)) if raw != raw.trim() => "encrypted_content has leading or trailing whitespace".to_string(),
            Some(Value::String(raw)) => match inspect_gpt_reasoning_signature(raw) {
                Ok(_) => String::new(),
                Err(err) => err.to_string(),
            },
            Some(Value::Null) => "encrypted_content is null".to_string(),
            _ => format!("encrypted_content must be a string, got {}", kind_name(encrypted.kind())),
        };
        if reason.is_empty() {
            if changed {
                *slot = next;
                changed_any = true;
            }
            continue;
        }

        cpa_json::delete(&mut next, "encrypted_content");
        if !is_compat && strip_orphan_ids && has_id {
            cpa_json::delete(&mut next, "id");
        }
        *slot = next;
        changed_any = true;
        tracing::debug!("{provider}: dropped invalid reasoning encrypted_content at input[{index}] item_id={item_id:?} reason={reason}");
    }

    if !changed_any {
        return body.to_vec();
    }
    cpa_json::set(&mut root, "input", Value::Array(items));
    cpa_json::to_vec(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape-valid GPT signature: `gAAAA` prefix, base64url, version byte 0x80, block-aligned body.
    fn valid_signature() -> String {
        use base64_url_shape::encode;
        encode()
    }

    mod base64_url_shape {
        /// Builds a 1 + 8 + 16 + 16 + 32 byte token (version, timestamp, iv, ciphertext, hmac).
        pub fn encode() -> String {
            let mut raw = vec![0x80u8];
            raw.extend_from_slice(&[0u8; 8]);
            raw.extend_from_slice(&[7u8; 16]);
            raw.extend_from_slice(&[9u8; 16]);
            raw.extend_from_slice(&[1u8; 32]);
            let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in raw.chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
                let chars = chunk.len() + 1;
                for i in 0..chars {
                    out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        }
    }

    fn run(input: serde_json::Value) -> Value {
        let out = sanitize_openai_responses_reasoning_encrypted_content("codex", &serde_json::to_vec(&input).unwrap());
        cpa_json::parse(&out)
    }

    #[test]
    fn drops_invalid_signature_and_orphan_ids() {
        let out = run(json!({"input":[
            {"type":"message","role":"user"},
            {"type":"reasoning","id":"rs_1","encrypted_content":"not-a-signature","summary":[]},
            {"type":"reasoning","id":"rs_2","summary":[]},
            {"type":"reasoning","id":"rs_3","encrypted_content":null},
            {"type":"reasoning","id":"rs_4","encrypted_content":7}
        ]}));
        assert!(out.g("input.0.role").exists());
        for i in 1..=4 {
            assert!(!out.g(&format!("input.{i}.encrypted_content")).exists(), "item {i}");
            assert!(!out.g(&format!("input.{i}.id")).exists(), "item {i}");
        }
        // store=true keeps ids (the backend can look the items up).
        let kept = run(json!({"store":true,"input":[{"type":"reasoning","id":"rs_2","summary":[]}]}));
        assert_eq!(kept.g("input.0.id").str(), "rs_2");
    }

    #[test]
    fn valid_signature_survives_and_content_is_promoted() {
        let sig = valid_signature();
        let out = run(json!({"input":[{"type":"reasoning","id":"rs_1","encrypted_content":sig,
            "summary":[],"content":[{"type":"reasoning_text","text":"thinking"},{"type":"x","text":"no"}]}]}));
        assert_eq!(out.g("input.0.encrypted_content").str(), sig);
        assert_eq!(out.g("input.0.id").str(), "rs_1");
        assert_eq!(out.g("input.0.summary.0.text").str(), "thinking");
        assert_eq!(out.g("input.0.summary.0.type").str(), "summary_text");
        assert_eq!(out.g("input.0.content.#").int(), 0);
        // Compat models keep the cleartext content for replay.
        let body = serde_json::to_vec(&json!({"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"t"}]}]})).unwrap();
        assert_eq!(sanitize_openai_responses_reasoning_encrypted_content_with_compat("x", &body, true), body);
    }

    #[test]
    fn untouched_bodies_keep_their_bytes() {
        let body = br#"{"input": [ {"type":"message"} ] }"#;
        assert_eq!(sanitize_openai_responses_reasoning_encrypted_content("p", body), body.to_vec());
        assert_eq!(sanitize_openai_responses_reasoning_encrypted_content("p", b"{}"), b"{}".to_vec());
    }
}
