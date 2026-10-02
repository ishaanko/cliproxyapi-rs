//! Sensitive word matcher (Go: helps/cloak_obfuscate.go, the part Devin uses).
//!
//! Configured words get a zero-width space after their first character so upstream filters no
//! longer match them. Only the system prompt is rewritten (see `wire::sanitize_system_prompt`).

use regex::Regex;

/// U+200B, inserted after the first character of a matched word.
pub const ZERO_WIDTH_SPACE: &str = "\u{200B}";

/// Case-insensitive alternation of the configured words, longest first.
#[derive(Debug, Clone)]
pub struct SensitiveWordMatcher {
    regex: Regex,
}

impl SensitiveWordMatcher {
    /// Builds a matcher from trimmed words of at least two characters that do not already carry a
    /// zero-width space. `None` when no word qualifies.
    pub fn new(words: &[String]) -> Option<Self> {
        let mut valid: Vec<&str> = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| w.chars().count() >= 2 && !w.contains(ZERO_WIDTH_SPACE))
            .collect();
        if valid.is_empty() {
            return None;
        }
        valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let pattern = format!(
            "(?i){}",
            valid
                .iter()
                .map(|w| regex::escape(w))
                .collect::<Vec<_>>()
                .join("|")
        );
        Regex::new(&pattern).ok().map(|regex| Self { regex })
    }

    /// Whether `text` contains any configured word.
    pub fn matches(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }

    /// Obfuscates every occurrence of a configured word in `text`.
    pub fn obfuscate_text(&self, text: &str) -> String {
        self.regex
            .replace_all(text, |caps: &regex::Captures<'_>| obfuscate_word(&caps[0]))
            .into_owned()
    }
}

/// Inserts a zero-width space after the first character (single-character words are unchanged).
fn obfuscate_word(word: &str) -> String {
    if word.contains(ZERO_WIDTH_SPACE) {
        return word.to_string();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) if !chars.as_str().is_empty() => {
            format!("{first}{ZERO_WIDTH_SPACE}{}", chars.as_str())
        }
        _ => word.to_string(),
    }
}
