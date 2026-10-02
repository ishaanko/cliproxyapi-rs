//! Small request helpers the Antigravity executor shares with the Gemini family in Go's `helps`
//! package (gemini_content_turns.go, cloak_obfuscate.go, claude_code_session.go,
//! claude_input_tokens.go). Kept local to this module.

use cpa_json::J;
use http::HeaderMap;
use regex::Regex;
use serde_json::{Value, json};

// ---------------------------------------------------------------- gemini content turns

fn empty_user_turn() -> Value {
    json!({"role": "user", "parts": [{"text": ""}]})
}

/// Prepends an empty user turn when `contents` at `path` starts with a model turn.
pub fn ensure_gemini_leading_user_content(payload: Vec<u8>, path: &str) -> Vec<u8> {
    let mut v = cpa_json::parse(&payload);
    if v.g(&format!("{path}.0.role")).str() != "model" {
        return payload;
    }
    let contents = v.g(path);
    if !contents.is_array() || contents.array().is_empty() {
        return payload;
    }
    let mut items = vec![empty_user_turn()];
    items.extend(contents.array().iter().map(|c| c.value()));
    drop(contents);
    cpa_json::set(&mut v, path, Value::Array(items));
    cpa_json::to_vec(&v)
}

fn content_has_function_response(content: &cpa_json::Res<'_>) -> bool {
    let parts = content.g("parts");
    parts.is_array() && parts.array().iter().any(|p| p.g("functionResponse").exists())
}

/// Appends an empty user turn when the last turn is a model turn without a functionResponse.
pub fn ensure_gemini_trailing_user_content(payload: Vec<u8>, path: &str) -> Vec<u8> {
    let mut v = cpa_json::parse(&payload);
    let contents = v.g(path);
    if !contents.is_array() {
        return payload;
    }
    let list = contents.array();
    let Some(last) = list.last() else {
        return payload;
    };
    let role = last.g("role").str();
    if (role != "model" && role != "assistant") || content_has_function_response(last) {
        return payload;
    }
    let mut items: Vec<Value> = list.iter().map(|c| c.value()).collect();
    items.push(empty_user_turn());
    drop(list);
    drop(contents);
    cpa_json::set(&mut v, path, Value::Array(items));
    cpa_json::to_vec(&v)
}

/// Both of the above.
pub fn ensure_gemini_boundary_user_content(payload: Vec<u8>, path: &str) -> Vec<u8> {
    ensure_gemini_trailing_user_content(ensure_gemini_leading_user_content(payload, path), path)
}

// ---------------------------------------------------------------- sensitive words

const ZERO_WIDTH_SPACE: &str = "\u{200B}";

/// Case-insensitive matcher over the configured sensitive words (Go: SensitiveWordMatcher).
pub struct SensitiveWordMatcher {
    regex: Regex,
}

impl SensitiveWordMatcher {
    /// Words shorter than two characters or already containing a zero-width space are ignored;
    /// longer words match first. `None` when nothing usable remains.
    pub fn new(words: &[String]) -> Option<Self> {
        let mut valid: Vec<String> = words
            .iter()
            .map(|w| w.trim().to_string())
            .filter(|w| w.chars().count() >= 2 && !w.contains(ZERO_WIDTH_SPACE))
            .collect();
        if valid.is_empty() {
            return None;
        }
        valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let escaped: Vec<String> = valid.iter().map(|w| regex::escape(w)).collect();
        Regex::new(&format!("(?i){}", escaped.join("|"))).ok().map(|regex| Self { regex })
    }

    /// Inserts a zero-width space after the first character of every match.
    pub fn obfuscate_text(&self, text: &str) -> String {
        self.regex
            .replace_all(text, |caps: &regex::Captures<'_>| obfuscate_word(&caps[0]))
            .into_owned()
    }
}

fn obfuscate_word(word: &str) -> String {
    if word.contains(ZERO_WIDTH_SPACE) {
        return word.to_string();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) if !chars.as_str().is_empty() => format!("{first}{ZERO_WIDTH_SPACE}{}", chars.as_str()),
        _ => word.to_string(),
    }
}

/// Obfuscates sensitive words in `request.systemInstruction` / `request.system_instruction`.
pub fn obfuscate_sensitive_words_in_system_instruction(payload: Vec<u8>, matcher: &SensitiveWordMatcher) -> Vec<u8> {
    let mut v = cpa_json::parse(&payload);
    let mut changed = false;
    for path in ["request.systemInstruction", "request.system_instruction"] {
        let instruction = v.g(path);
        if !instruction.exists() {
            continue;
        }
        if instruction.is_string() {
            let text = instruction.str();
            let obfuscated = matcher.obfuscate_text(&text);
            drop(instruction);
            if obfuscated != text {
                cpa_json::set(&mut v, path, obfuscated);
                changed = true;
            }
            continue;
        }
        let parts = instruction.g("parts");
        if !parts.is_array() {
            continue;
        }
        let edits: Vec<(usize, String)> = parts
            .array()
            .iter()
            .enumerate()
            .filter_map(|(i, part)| {
                let t = part.g("text");
                if !t.is_string() {
                    return None;
                }
                let text = t.str();
                let obfuscated = matcher.obfuscate_text(&text);
                (obfuscated != text).then_some((i, obfuscated))
            })
            .collect();
        drop(parts);
        drop(instruction);
        for (i, text) in edits {
            cpa_json::set(&mut v, &format!("{path}.parts.{i}.text"), text);
            changed = true;
        }
    }
    if changed { cpa_json::to_vec(&v) } else { payload }
}

// ---------------------------------------------------------------- claude code session

const CLAUDE_CODE_SESSION_HEADER: &str = "X-Claude-Code-Session-Id";
const CLAUDE_CODE_AGENT_HEADER: &str = "X-Claude-Code-Agent-Id";
const CLAUDE_CODE_MAIN_AGENT_ID: &str = "main";

/// First non-empty (trimmed) header value for `name`.
pub fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

fn claude_code_session_id_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let user_id = cpa_json::parse(payload).g("metadata.user_id").str();
    if user_id.is_empty() {
        return String::new();
    }
    // `_session_<hex-and-dashes>` suffix.
    if let Some(idx) = user_id.rfind("_session_") {
        let tail = &user_id[idx + "_session_".len()..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-') {
            return tail.to_string();
        }
    }
    if user_id.starts_with('{') {
        return cpa_json::parse(user_id.as_bytes()).g("session_id").str().trim().to_string();
    }
    String::new()
}

/// `claude:<session>:agent:<agent>` for Claude Code requests (Go: ClaudeCodeExecutionScope).
pub fn claude_code_execution_scope(payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let mut session = header_value(headers, CLAUDE_CODE_SESSION_HEADER);
    if session.is_empty() {
        session = claude_code_session_id_from_payload(payload);
    }
    if session.is_empty() {
        return None;
    }
    let mut agent = header_value(headers, CLAUDE_CODE_AGENT_HEADER);
    if agent.is_empty() {
        agent = CLAUDE_CODE_MAIN_AGENT_ID.to_string();
    }
    Some(format!("claude:{session}:agent:{agent}"))
}
