//! Signature sanitizers for Claude `/v1/messages` bodies (Go: signature/claude.go and
//! claude_messages_sanitize.go).
//!
//! All functions take and return JSON bytes. When nothing changes the input bytes are returned
//! untouched; otherwise the body is re-serialized compactly (see crate docs on `cpa_json`).

use cpa_json::{J, Res, Value};

use super::claude::{is_valid_claude_thinking_signature, ClaudeSignatureValidationOptions};
use super::provider::{
    decide_signature_compatibility_for_model, normalize_signature_target_provider,
    signature_provider_from_model_name, SignatureBlockKind, SignatureCompatibilityAction,
    SignatureCompatibilityDecision, SignatureProvider,
};

/// Removes Claude thinking blocks whose signatures are empty or not valid Claude thinking
/// signatures (after stripping an optional cache prefix), unless `opts` allows empty placeholders.
pub fn strip_invalid_claude_thinking_blocks(
    payload: &[u8],
    opts: ClaudeSignatureValidationOptions,
) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let messages = root.g("messages");
    if !messages.is_array() {
        return payload.to_vec();
    }
    let mut kept_messages: Vec<Value> = Vec::new();
    let mut modified = false;
    for msg in messages.array() {
        let content = msg.g("content");
        if !content.is_array() {
            kept_messages.push(msg.value());
            continue;
        }
        let mut kept_parts: Vec<Value> = Vec::new();
        let mut stripped = false;
        for part in content.array() {
            if part.g("type").str() == "thinking" && should_strip_claude_thinking_block(&part, opts) {
                stripped = true;
                continue;
            }
            kept_parts.push(part.value());
        }
        if stripped {
            modified = true;
            let mut updated = msg.value();
            cpa_json::set(&mut updated, "content", Value::Array(kept_parts));
            kept_messages.push(updated);
            continue;
        }
        kept_messages.push(msg.value());
    }
    if !modified {
        return payload.to_vec();
    }
    cpa_json::set(&mut root, "messages", Value::Array(kept_messages));
    cpa_json::to_vec(&root)
}

/// Like [`strip_invalid_claude_thinking_blocks`], and also removes messages whose content array
/// became empty.
pub fn strip_invalid_claude_thinking_blocks_and_empty_messages(
    payload: &[u8],
    opts: ClaudeSignatureValidationOptions,
) -> Vec<u8> {
    let stripped = strip_invalid_claude_thinking_blocks(payload, opts);
    if stripped == payload {
        return payload.to_vec();
    }
    let mut root = cpa_json::parse(&stripped);
    let messages = root.g("messages");
    if !messages.is_array() {
        return stripped;
    }
    let kept: Vec<Value> = messages
        .array()
        .iter()
        .filter(|message| {
            let content = message.g("content");
            !(content.is_array() && content.array().is_empty())
        })
        .map(Res::value)
        .collect();
    cpa_json::set(&mut root, "messages", Value::Array(kept));
    cpa_json::to_vec(&root)
}

fn should_strip_claude_thinking_block(part: &Res<'_>, opt: ClaudeSignatureValidationOptions) -> bool {
    if opt.allow_empty_signature_with_empty_text && is_empty_claude_thinking_placeholder(part) {
        return false;
    }
    !is_valid_claude_thinking_signature(&part.g("signature").str(), opt)
}

fn is_empty_claude_thinking_placeholder(part: &Res<'_>) -> bool {
    if !part.g("signature").str().trim().is_empty() {
        return false;
    }
    claude_thinking_block_text(part).trim().is_empty()
}

/// Thinking text of a block: `text`, then `thinking` (string, or object with `text`/`thinking`).
fn claude_thinking_block_text(part: &Res<'_>) -> String {
    let text = part.g("text");
    if text.is_string() {
        return text.str();
    }
    let thinking = part.g("thinking");
    if !thinking.exists() {
        return String::new();
    }
    if thinking.is_string() {
        return thinking.str();
    }
    if thinking.is_object() {
        let inner = thinking.g("text");
        if inner.is_string() {
            return inner.str();
        }
        let inner = thinking.g("thinking");
        if inner.is_string() {
            return inner.str();
        }
    }
    String::new()
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeMessagesSignatureSanitizeOptions {
    pub target_provider: SignatureProvider,
    pub target_model: String,
    pub drop_empty_messages: bool,
    pub drop_tool_signatures: bool,
    pub drop_empty_thinking_placeholders: bool,
    /// Preserve compatibility-mode thinking blocks together with their original signatures,
    /// including opaque signatures.
    pub preserve_empty_thinking_blocks: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SignatureSanitizeReport {
    pub target_provider: SignatureProvider,
    pub preserved: usize,
    pub dropped_blocks: usize,
    pub dropped_signatures: usize,
    pub replaced_signatures: usize,
    pub decisions: Vec<SignatureCompatibilityDecision>,
}

/// Removes or preserves Claude `/v1/messages` signed history according to the provider family
/// implied by `target_model`.
pub fn sanitize_claude_messages_signatures_for_model(
    payload: &[u8],
    target_model: &str,
) -> (Vec<u8>, SignatureSanitizeReport) {
    sanitize_claude_messages_signatures_for_target(
        payload,
        &ClaudeMessagesSignatureSanitizeOptions {
            target_provider: signature_provider_from_model_name(target_model),
            target_model: target_model.to_string(),
            drop_empty_messages: true,
            ..Default::default()
        },
    )
}

/// Prepares a Claude `/v1/messages` body for Claude-compatible upstreams. Valid Claude signatures
/// are normalized to provider-native E-form, CAIS signatures are kept, incompatible thinking
/// blocks are dropped, and tool_use blocks keep only their tool-call payload.
pub fn sanitize_claude_messages_for_claude_upstream(
    payload: &[u8],
    target_model: &str,
    preserve_empty_thinking_blocks: bool,
) -> (Vec<u8>, SignatureSanitizeReport) {
    sanitize_claude_messages_signatures_for_target(
        payload,
        &ClaudeMessagesSignatureSanitizeOptions {
            target_provider: SignatureProvider::Claude,
            target_model: target_model.to_string(),
            drop_empty_messages: true,
            drop_tool_signatures: true,
            drop_empty_thinking_placeholders: !preserve_empty_thinking_blocks,
            preserve_empty_thinking_blocks,
        },
    )
}

/// Applies provider-aware signature compatibility rules to Claude `/v1/messages` history.
/// Compatible thinking signatures are preserved; incompatible thinking blocks are removed so a
/// conversation can continue after switching between Claude, GPT/Codex and Gemini models.
pub fn sanitize_claude_messages_signatures_for_target(
    payload: &[u8],
    opts: &ClaudeMessagesSignatureSanitizeOptions,
) -> (Vec<u8>, SignatureSanitizeReport) {
    let mut target_provider = normalize_signature_target_provider(opts.target_provider);
    if target_provider == SignatureProvider::Unknown && !opts.target_model.is_empty() {
        target_provider = signature_provider_from_model_name(&opts.target_model);
    }
    let mut report = SignatureSanitizeReport {
        target_provider,
        ..Default::default()
    };

    let mut root = cpa_json::parse(payload);
    let messages = root.g("messages");
    if !messages.is_array() {
        return (payload.to_vec(), report);
    }

    let mut kept_messages: Vec<Value> = Vec::new();
    let mut modified = false;

    for (i, message) in messages.array().iter().enumerate() {
        let content = message.g("content");
        if !content.is_array() {
            kept_messages.push(message.value());
            continue;
        }

        let mut kept_parts: Vec<Value> = Vec::new();
        let mut message_modified = false;

        for (j, part) in content.array().iter().enumerate() {
            let part_type = part.g("type").str();
            if part_type == "tool_use" {
                if opts.drop_tool_signatures {
                    let (updated_part, changed) = strip_claude_tool_use_signature_fields(part);
                    if changed {
                        message_modified = true;
                        report.dropped_signatures += 1;
                    }
                    kept_parts.push(updated_part);
                    continue;
                }
                let (updated_part, changed, decisions) =
                    sanitize_claude_tool_use_signature(part, target_provider, &opts.target_model, i, j);
                if changed {
                    message_modified = true;
                }
                for decision in &decisions {
                    match decision.action {
                        SignatureCompatibilityAction::Preserve => report.preserved += 1,
                        SignatureCompatibilityAction::ReplaceWithGeminiBypass => {
                            report.replaced_signatures += 1
                        }
                        _ => report.dropped_signatures += 1,
                    }
                }
                report.decisions.extend(decisions);
                kept_parts.push(updated_part);
                continue;
            }

            if part_type != "thinking" {
                kept_parts.push(part.value());
                continue;
            }

            let raw_signature = part.g("signature").str();
            if opts.preserve_empty_thinking_blocks {
                report.preserved += 1;
                kept_parts.push(part.value());
                continue;
            }
            if target_provider == SignatureProvider::Claude
                && is_empty_claude_thinking_placeholder(part)
                && !opts.drop_empty_thinking_placeholders
            {
                kept_parts.push(part.value());
                continue;
            }

            let mut decision = decide_signature_compatibility_for_model(
                target_provider,
                &opts.target_model,
                &raw_signature,
                SignatureBlockKind::ClaudeThinking,
            );
            decision.reason = format!("messages[{i}].content[{j}]: {}", decision.reason);

            match decision.action {
                SignatureCompatibilityAction::Preserve => {
                    report.preserved += 1;
                    if !decision.normalized_signature.is_empty()
                        && decision.normalized_signature != raw_signature
                    {
                        let mut updated = part.value();
                        cpa_json::set(&mut updated, "signature", decision.normalized_signature.as_str());
                        kept_parts.push(updated);
                        message_modified = true;
                    } else {
                        kept_parts.push(part.value());
                    }
                }
                SignatureCompatibilityAction::ReplaceWithGeminiBypass => {
                    report.replaced_signatures += 1;
                    let mut updated = part.value();
                    cpa_json::set(&mut updated, "signature", decision.replacement_signature.as_str());
                    kept_parts.push(updated);
                    message_modified = true;
                }
                SignatureCompatibilityAction::DropSignature => {
                    report.dropped_signatures += 1;
                    let mut updated = part.value();
                    cpa_json::delete(&mut updated, "signature");
                    kept_parts.push(updated);
                    message_modified = true;
                }
                _ => {
                    report.dropped_blocks += 1;
                    message_modified = true;
                }
            }
            report.decisions.push(decision);
        }

        if message_modified {
            modified = true;
            if kept_parts.is_empty() && opts.drop_empty_messages {
                continue;
            }
            let mut updated = message.value();
            cpa_json::set(&mut updated, "content", Value::Array(kept_parts));
            kept_messages.push(updated);
            continue;
        }

        kept_messages.push(message.value());
    }

    if !modified {
        return (payload.to_vec(), report);
    }
    cpa_json::set(&mut root, "messages", Value::Array(kept_messages));
    (cpa_json::to_vec(&root), report)
}

fn strip_claude_tool_use_signature_fields(part: &Res<'_>) -> (Value, bool) {
    let mut updated = part.value();
    let mut changed = false;
    for sig_path in claude_tool_use_provenance_paths() {
        if !updated.g(sig_path).exists() {
            continue;
        }
        cpa_json::delete(&mut updated, sig_path);
        changed = true;
    }
    changed |= delete_empty_json_object_path(&mut updated, "extra_content.google");
    changed |= delete_empty_json_object_path(&mut updated, "extra_content");
    (updated, changed)
}

fn sanitize_claude_tool_use_signature(
    part: &Res<'_>,
    target_provider: SignatureProvider,
    target_model: &str,
    message_idx: usize,
    part_idx: usize,
) -> (Value, bool, Vec<SignatureCompatibilityDecision>) {
    let mut updated = part.value();
    let mut changed = false;
    let mut decisions = Vec::new();

    for sig_path in CLAUDE_TOOL_USE_SIGNATURE_PATHS {
        let sig_result = part.g(sig_path);
        if !sig_result.exists() {
            continue;
        }

        let block_kind = match target_provider {
            SignatureProvider::Claude => SignatureBlockKind::ClaudeThinking,
            SignatureProvider::Gpt => SignatureBlockKind::GptReasoning,
            _ => SignatureBlockKind::GeminiFunctionCall,
        };
        let sig_string = sig_result.str();
        let mut decision =
            decide_signature_compatibility_for_model(target_provider, target_model, &sig_string, block_kind);
        decision.reason = format!(
            "messages[{message_idx}].content[{part_idx}].{sig_path}: {}",
            decision.reason
        );

        match decision.action {
            SignatureCompatibilityAction::Preserve => {
                if !decision.normalized_signature.is_empty() && decision.normalized_signature != sig_string {
                    cpa_json::set(&mut updated, sig_path, decision.normalized_signature.as_str());
                    changed = true;
                }
            }
            SignatureCompatibilityAction::ReplaceWithGeminiBypass => {
                cpa_json::set(&mut updated, sig_path, decision.replacement_signature.as_str());
                changed = true;
            }
            _ => {
                cpa_json::delete(&mut updated, sig_path);
                changed = true;
            }
        }
        decisions.push(decision);
    }

    changed |= delete_empty_json_object_path(&mut updated, "extra_content.google");
    changed |= delete_empty_json_object_path(&mut updated, "extra_content");

    (updated, changed, decisions)
}

const CLAUDE_TOOL_USE_SIGNATURE_PATHS: [&str; 4] = [
    "signature",
    "thoughtSignature",
    "thought_signature",
    "extra_content.google.thought_signature",
];

fn claude_tool_use_provenance_paths() -> impl Iterator<Item = &'static str> {
    CLAUDE_TOOL_USE_SIGNATURE_PATHS.into_iter().chain(["model"])
}

/// Deletes `path` when it is an empty object; reports whether it did.
fn delete_empty_json_object_path(raw: &mut Value, path: &str) -> bool {
    let is_empty_object = matches!(raw.g(path).v(), Some(Value::Object(m)) if m.is_empty());
    if is_empty_object {
        cpa_json::delete(raw, path);
    }
    is_empty_object
}
