//! Sensitive-word obfuscation for cloaked requests (Go: helps/cloak_obfuscate.go).
//!
//! Matched words get a zero-width space after their first character. Edits are made on the raw
//! body bytes (only the matched string values change) because the body is later CCH-signed.

use cpa_json::{J, Res};
use regex::Regex;

use crate::claude::signing::set_strings_at;

/// U+200B inserted into matched words.
const ZERO_WIDTH_SPACE: &str = "\u{200B}";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// Compiled case-insensitive alternation of the configured words (Go: `SensitiveWordMatcher`).
#[derive(Debug, Clone)]
pub struct SensitiveWordMatcher {
    regex: Regex,
}

impl SensitiveWordMatcher {
    /// [`build_sensitive_word_matcher`] for a word list.
    pub fn new<S: AsRef<str>>(words: &[S]) -> Option<Self> {
        build_sensitive_word_matcher(words)
    }
}

/// Compiles the word list (Go: `BuildSensitiveWordMatcher`). Words are trimmed; words shorter than
/// two characters or already containing a zero-width space are dropped; longest first so the
/// longest word wins at a position. `None` when nothing usable remains.
pub fn build_sensitive_word_matcher<S: AsRef<str>>(words: &[S]) -> Option<SensitiveWordMatcher> {
    let mut valid: Vec<&str> = words
        .iter()
        .map(|w| w.as_ref().trim())
        .filter(|w| w.chars().count() >= 2 && !w.contains(ZERO_WIDTH_SPACE))
        .collect();
    if valid.is_empty() {
        return None;
    }
    valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
    let escaped: Vec<String> = valid.iter().map(|w| regex::escape(w)).collect();
    let regex = Regex::new(&format!("(?i){}", escaped.join("|"))).ok()?;
    Some(SensitiveWordMatcher { regex })
}

/// Inserts a zero-width space after the first character (Go: `obfuscateWord`).
fn obfuscate_word(word: &str) -> String {
    if word.contains(ZERO_WIDTH_SPACE) {
        return word.to_string();
    }
    let mut chars = word.chars();
    match chars.next() {
        // Go also leaves a word alone when its first rune decodes as RuneError.
        Some(first) if first != '\u{FFFD}' && first.len_utf8() < word.len() => {
            format!("{first}{ZERO_WIDTH_SPACE}{}", chars.as_str())
        }
        _ => word.to_string(),
    }
}

impl SensitiveWordMatcher {
    /// Replaces every sensitive word in `text` (Go: `ObfuscateText`).
    pub fn obfuscate_text(&self, text: &str) -> String {
        self.regex.replace_all(text, |caps: &regex::Captures<'_>| obfuscate_word(&caps[0])).into_owned()
    }

    /// Whether `text` contains any configured word (Go: `Matches`).
    pub fn matches(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

/// Obfuscates sensitive words in `system` text blocks and `messages` content (Go:
/// `ObfuscateSensitiveWords`). The billing header block is never touched. A `None` matcher returns
/// the payload unchanged.
pub fn obfuscate_sensitive_words(payload: &[u8], matcher: Option<&SensitiveWordMatcher>) -> Vec<u8> {
    let Some(matcher) = matcher else { return payload.to_vec() };
    let root = cpa_json::parse(payload);
    let mut edits: Vec<(String, String)> = Vec::new();
    collect_system_edits(&root, matcher, &mut edits);
    collect_message_edits(&root, matcher, &mut edits);
    set_strings_at(payload, &edits)
}

/// Queues an edit for `path` when obfuscation changes `text`.
fn push_if_changed(matcher: &SensitiveWordMatcher, text: &str, path: String, edits: &mut Vec<(String, String)>) {
    let obfuscated = matcher.obfuscate_text(text);
    if obfuscated != text {
        edits.push((path, obfuscated));
    }
}

/// Go: `obfuscateSystemBlocks`.
fn collect_system_edits(root: &cpa_json::Value, matcher: &SensitiveWordMatcher, edits: &mut Vec<(String, String)>) {
    let system = root.g("system");
    if !system.exists() {
        return;
    }
    if system.is_array() {
        for (i, block) in system.array().iter().enumerate() {
            if block.g("type").str() == "text" {
                let text = block.g("text").str();
                if text.starts_with(BILLING_PREFIX) {
                    continue;
                }
                push_if_changed(matcher, &text, format!("system.{i}.text"), edits);
            }
        }
    } else if system.is_string() {
        let text = system.str();
        if !text.starts_with(BILLING_PREFIX) {
            push_if_changed(matcher, &text, "system".to_string(), edits);
        }
    }
}

/// Go: `obfuscateMessages`.
fn collect_message_edits(root: &cpa_json::Value, matcher: &SensitiveWordMatcher, edits: &mut Vec<(String, String)>) {
    let messages = root.g("messages");
    if !messages.is_array() {
        return;
    }
    for (m, message) in messages.array().iter().enumerate() {
        let content = message.g("content");
        if !content.exists() {
            continue;
        }
        if content.is_string() {
            push_if_changed(matcher, &content.str(), format!("messages.{m}.content"), edits);
        } else if content.is_array() {
            for (b, block) in content.array().iter().enumerate() {
                if block.g("type").str() == "text" {
                    push_if_changed(matcher, &block.g("text").str(), format!("messages.{m}.content.{b}.text"), edits);
                }
            }
        }
    }
}

/// Obfuscates an Antigravity `request.systemInstruction` / `request.system_instruction` (Go:
/// `ObfuscateSensitiveWordsInSystemInstruction`), either a string or an object with `parts[].text`.
pub fn obfuscate_sensitive_words_in_system_instruction(payload: &[u8], matcher: Option<&SensitiveWordMatcher>) -> Vec<u8> {
    let Some(matcher) = matcher else { return payload.to_vec() };
    let root = cpa_json::parse(payload);
    let mut edits: Vec<(String, String)> = Vec::new();
    for path in ["request.systemInstruction", "request.system_instruction"] {
        let instruction = root.g(path);
        if !instruction.exists() {
            continue;
        }
        if instruction.is_string() {
            push_if_changed(matcher, &instruction.str(), path.to_string(), &mut edits);
            continue;
        }
        let parts = instruction.g("parts");
        if !parts.is_array() {
            continue;
        }
        for (i, part) in parts.array().iter().enumerate() {
            let text: Res<'_> = part.g("text");
            if text.is_string() {
                push_if_changed(matcher, &text.str(), format!("{path}.parts.{i}.text"), &mut edits);
            }
        }
    }
    set_strings_at(payload, &edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(words: &[&str]) -> SensitiveWordMatcher {
        build_sensitive_word_matcher(words).expect("matcher")
    }

    #[test]
    fn matcher_filters_and_prefers_longest() {
        assert!(build_sensitive_word_matcher::<&str>(&[]).is_none());
        assert!(build_sensitive_word_matcher(&["a", "  ", "x\u{200B}y"]).is_none());
        // "secret" and "secrets" both match at the same position: the longest is tried first.
        let m = matcher(&["secret", "secrets"]);
        assert_eq!(m.obfuscate_text("my SECRETS and secret"), "my S\u{200B}ECRETS and s\u{200B}ecret");
        assert!(m.matches("a Secret"));
        assert!(!m.matches("nothing"));
        // Regex metacharacters are literal; multi-byte first characters stay intact.
        let m = matcher(&["a.b", "éclair"]);
        assert_eq!(m.obfuscate_text("a.b axb Éclair"), "a\u{200B}.b axb É\u{200B}clair");
    }

    #[test]
    fn payload_edits_keep_other_bytes_and_skip_billing() {
        let m = matcher(&["cli", "version", "entrypoint", "secret"]);
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli;"},{"type":"text","text":"top secret <ok>"},{"type":"image","text":"secret"}],"messages":[{"role":"user","content":"keep this secret"},{"role":"user","content":[{"type":"text","text":"a secret"},{"type":"tool_result","text":"secret"}]}],"max_tokens":1}"#;
        let got = obfuscate_sensitive_words(body, Some(&m));
        let want = "{\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli;\"},{\"type\":\"text\",\"text\":\"top s\u{200B}ecret <ok>\"},{\"type\":\"image\",\"text\":\"secret\"}],\"messages\":[{\"role\":\"user\",\"content\":\"keep this s\u{200B}ecret\"},{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a s\u{200B}ecret\"},{\"type\":\"tool_result\",\"text\":\"secret\"}]}],\"max_tokens\":1}";
        // Non-ASCII output goes through json.Marshal, which HTML-escapes `<`/`>`.
        let want = want.replace("top s\u{200B}ecret <ok>", "top s\u{200B}ecret \\u003cok\\u003e");
        assert_eq!(String::from_utf8_lossy(&got), want);
        assert_eq!(obfuscate_sensitive_words(body, None), body.to_vec());
    }

    #[test]
    fn string_system_and_system_instruction() {
        let m = matcher(&["secret"]);
        let got = obfuscate_sensitive_words(br#"{"system":"a secret"}"#, Some(&m));
        assert_eq!(String::from_utf8_lossy(&got), "{\"system\":\"a s\u{200B}ecret\"}");

        let body = br#"{"request":{"systemInstruction":{"parts":[{"text":"secret one"},{"inline":1},{"text":"fine"}]},"system_instruction":"secret two"}}"#;
        let got = obfuscate_sensitive_words_in_system_instruction(body, Some(&m));
        assert_eq!(
            String::from_utf8_lossy(&got),
            "{\"request\":{\"systemInstruction\":{\"parts\":[{\"text\":\"s\u{200B}ecret one\"},{\"inline\":1},{\"text\":\"fine\"}]},\"system_instruction\":\"s\u{200B}ecret two\"}}"
        );
    }
}
