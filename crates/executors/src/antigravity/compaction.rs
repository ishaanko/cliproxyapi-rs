//! Responses `/responses/compact` support for Antigravity (Go: helps/antigravity_compaction.go).
//!
//! The upstream has no compaction endpoint, so the executor asks the model for a summary and
//! seals it into an opaque capsule (`cpa-ag-compact-v1:` + base64url of AES-256-GCM(nonce ||
//! ciphertext)). Later requests carrying the capsule get it expanded back into a developer
//! message.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cpa_json::J;
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CAPSULE_PREFIX: &str = "cpa-ag-compact-v1:";
const KEY_SECRET: &str = "CLIProxyAPI";
const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;

const SUMMARY_PROMPT: &str = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Please provide a concise and comprehensive summary of the preceding conversation and task progress so far, including user goals, key findings, actions taken, and current status, so that work can continue smoothly."}]}"#;

fn input_has_type(payload: &[u8], ty: &str) -> bool {
    let v = cpa_json::parse(payload);
    let input = v.g("input");
    input.is_array() && input.array().iter().any(|item| item.g("type").str() == ty)
}

/// Whether `input` holds a `compaction_trigger` item.
pub fn has_responses_compaction_trigger(payload: &[u8]) -> bool {
    input_has_type(payload, "compaction_trigger")
}

/// Whether `input` holds a `compaction` item.
pub fn has_responses_compaction_item(payload: &[u8]) -> bool {
    input_has_type(payload, "compaction")
}

/// Builds the non-stream summary request: the trigger item is dropped, a summary prompt is
/// appended, and tools and stateful fields are removed.
pub fn prepare_summary_payload(payload: &[u8], _model_name: &str) -> Vec<u8> {
    let mut out = cpa_json::parse(payload);
    let prompt: Value = cpa_json::parse_str(SUMMARY_PROMPT);
    let input = out.g("input");
    let new_input = if input.is_array() {
        let mut items: Vec<Value> = input
            .array()
            .iter()
            .filter(|item| item.g("type").str() != "compaction_trigger")
            .map(|item| item.value())
            .collect();
        items.push(prompt);
        Value::Array(items)
    } else if input.is_string() && !input.str().is_empty() {
        let user = json!({"type":"message","role":"user","content":[{"type":"input_text","text":input.str()}]});
        Value::Array(vec![user, prompt])
    } else {
        Value::Array(vec![prompt])
    };
    cpa_json::set(&mut out, "input", new_input);
    for field in [
        "stream",
        "tools",
        "tool_choice",
        "previous_response_id",
        "parallel_tool_calls",
        "additional_tools",
        "truncation",
        "metadata",
    ] {
        cpa_json::delete(&mut out, field);
    }
    cpa_json::set(&mut out, "stream", false);
    cpa_json::to_vec(&out)
}

fn cipher() -> Result<Aes256Gcm, String> {
    let key = Sha256::digest(KEY_SECRET.as_bytes());
    Aes256Gcm::new_from_slice(&key).map_err(|e| format!("create cipher: {e}"))
}

/// Encrypts the summary into an opaque capsule.
pub fn seal_compaction(summary: &str, model_name: &str) -> Result<String, String> {
    let data = json!({
        "summary": summary,
        "model": model_name,
        "created_at": chrono::Utc::now().timestamp(),
    });
    let plaintext = serde_json::to_vec(&data).map_err(|e| format!("marshal compaction capsule: {e}"))?;
    let mut nonce = [0u8; NONCE_SIZE];
    rand::rng().fill_bytes(&mut nonce);
    let ciphertext = cipher()?
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|e| format!("seal compaction capsule: {e}"))?;
    let mut sealed = nonce.to_vec();
    sealed.extend_from_slice(&ciphertext);
    Ok(format!("{CAPSULE_PREFIX}{}", URL_SAFE_NO_PAD.encode(sealed)))
}

/// Decrypts and validates a capsule, returning the summary text.
pub fn unseal_compaction(encrypted_content: &str) -> Result<String, String> {
    let raw = encrypted_content
        .strip_prefix(CAPSULE_PREFIX)
        .ok_or_else(|| "unrecognized compaction capsule format".to_string())?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|e| format!("decode compaction capsule: {e}"))?;
    if ciphertext.len() < NONCE_SIZE + TAG_SIZE {
        return Err("compaction capsule ciphertext too short".into());
    }
    let (nonce, encrypted) = ciphertext.split_at(NONCE_SIZE);
    let plaintext = cipher()?
        .decrypt(Nonce::from_slice(nonce), encrypted)
        .map_err(|e| format!("invalid or corrupted compaction capsule: {e}"))?;
    let data: Value =
        serde_json::from_slice(&plaintext).map_err(|e| format!("unmarshal compaction capsule: {e}"))?;
    Ok(data.get("summary").and_then(Value::as_str).unwrap_or("").to_string())
}

/// Replaces every `compaction` input item with a developer message holding its summary. An
/// invalid capsule is an error (surfaced as 400).
pub fn expand_compaction_capsules(payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut root = cpa_json::parse(payload);
    let input = root.g("input");
    if !input.is_array() {
        return Ok(payload.to_vec());
    }
    let mut items = Vec::new();
    let mut changed = false;
    for item in input.array() {
        if item.g("type").str() == "compaction" {
            let summary = unseal_compaction(&item.g("encrypted_content").str())
                .map_err(|e| format!("invalid compaction capsule: {e}"))?;
            items.push(json!({
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": format!("Context summary from previous turns:\n{summary}")}],
            }));
            changed = true;
            continue;
        }
        items.push(item.value());
    }
    if !changed {
        return Ok(payload.to_vec());
    }
    cpa_json::set(&mut root, "input", Value::Array(items));
    Ok(cpa_json::to_vec(&root))
}

/// Pulls the summary text out of a Responses, Gemini-envelope, Claude or Chat response.
pub fn extract_summary_text(resp: &[u8]) -> Result<String, String> {
    let v = cpa_json::parse(resp);

    let output = v.g("output");
    if output.is_array() {
        let mut parts: Vec<String> = Vec::new();
        for item in output.array() {
            if item.g("type").str() != "message" {
                continue;
            }
            let content = item.g("content");
            if content.is_array() {
                for part in content.array() {
                    if part.g("type").str() == "output_text" {
                        let t = part.g("text").str();
                        if !t.is_empty() {
                            parts.push(t);
                        }
                    }
                }
            } else if content.is_string() && !content.str().is_empty() {
                parts.push(content.str());
            }
        }
        if !parts.is_empty() {
            return Ok(parts.join("\n"));
        }
    }

    let mut candidates = v.g("response.candidates");
    if !candidates.exists() {
        candidates = v.g("candidates");
    }
    if candidates.is_array() {
        let list = candidates.array();
        if let Some(first) = list.first() {
            let parts = first.g("content.parts");
            if parts.is_array() {
                let texts: Vec<String> = parts
                    .array()
                    .iter()
                    .filter(|p| !p.g("thought").bool())
                    .map(|p| p.g("text").str())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !texts.is_empty() {
                    return Ok(texts.join("\n"));
                }
            }
        }
    }

    let content = v.g("content");
    if content.is_array() {
        let texts: Vec<String> = content
            .array()
            .iter()
            .filter(|b| b.g("type").str() == "text")
            .map(|b| b.g("text").str())
            .filter(|t| !t.is_empty())
            .collect();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }

    let text = v.g("choices.0.message.content").str();
    if !text.is_empty() {
        return Ok(text);
    }
    Err("no summary text found in upstream response".into())
}

fn usage_node(input_tokens: i64, output_tokens: i64, total_tokens: i64) -> Value {
    json!({
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
    })
}

fn ids() -> (i64, String, String) {
    let now = chrono::Utc::now();
    let nanos = now.timestamp_nanos_opt().unwrap_or_else(|| now.timestamp() * 1_000_000_000);
    (now.timestamp(), format!("resp_ag_compact_{nanos}"), format!("cmp_ag_compact_{nanos}"))
}

/// JSON body of a non-stream `response.compaction` reply.
pub fn build_compaction_response(
    model_name: &str,
    capsule: &str,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) -> Vec<u8> {
    let (now, response_id, item_id) = ids();
    let item = json!({"type":"compaction","status":"completed","id":item_id,"encrypted_content":capsule});
    let resp = json!({
        "object": "response.compaction",
        "status": "completed",
        "id": response_id,
        "created_at": now,
        "model": model_name,
        "output": [item],
        "usage": usage_node(input_tokens, output_tokens, total_tokens),
    });
    cpa_json::to_vec(&resp)
}

fn sse_frame(event: &str, data: &Value) -> Vec<u8> {
    let mut out = format!("event: {event}\ndata: ").into_bytes();
    out.extend_from_slice(&cpa_json::to_vec(data));
    out.extend_from_slice(b"\n\n");
    out
}

/// Canned SSE frames of a streamed compaction reply.
pub fn build_compaction_stream_chunks(
    model_name: &str,
    capsule: &str,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) -> Vec<Vec<u8>> {
    let (now, response_id, item_id) = ids();
    let item_in_progress =
        json!({"type":"compaction","status":"in_progress","id":item_id,"encrypted_content":capsule});
    let item_completed =
        json!({"type":"compaction","status":"completed","id":item_id,"encrypted_content":capsule});
    let created_response = json!({
        "object": "response", "status": "in_progress", "background": false, "error": null,
        "output": [], "id": response_id, "created_at": now, "model": model_name,
    });
    let completed_response = json!({
        "object": "response", "status": "completed", "background": false, "error": null,
        "id": response_id, "created_at": now, "completed_at": now, "model": model_name,
        "output": [item_completed.clone()],
        "usage": usage_node(input_tokens, output_tokens, total_tokens),
    });
    vec![
        sse_frame(
            "response.created",
            &json!({"type":"response.created","sequence_number":0,"response":created_response.clone()}),
        ),
        sse_frame(
            "response.in_progress",
            &json!({"type":"response.in_progress","sequence_number":1,"response":created_response}),
        ),
        sse_frame(
            "response.output_item.added",
            &json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":item_in_progress}),
        ),
        sse_frame(
            "response.output_item.done",
            &json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":item_completed}),
        ),
        sse_frame(
            "response.completed",
            &json!({"type":"response.completed","sequence_number":4,"response":completed_response}),
        ),
    ]
}
