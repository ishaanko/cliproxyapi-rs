//! Claude Code style MCP tool aliases (Go: helps/claude_mcp_alias.go, claude_mcp_alias_wordlist.go).
//!
//! Every client tool declared on an OAuth/CLI-profile request is renamed to
//! `mcp__<word>_<word>__<word>_<semantic>`; the words come from the embedded BIP-39 English list
//! and are keyed by an HMAC of the downstream caller's secret.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};

/// Go: `claudeMCPAliasEnglishWords` (the 2048-word BIP-39 English dictionary, embedded verbatim).
static WORDS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    include_str!("claude_bip39_words.txt")
        .split_whitespace()
        .collect()
});

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Go: `IsClaudeMCPToolName`. True when `name` follows Claude Code's `mcp__server__tool`
/// convention, is at most 64 bytes and only contains `[A-Za-z0-9_-]`.
pub fn is_claude_mcp_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let Some(rest) = name.strip_prefix("mcp__") else {
        return false;
    };
    match rest.find("__") {
        Some(sep) if sep > 0 && sep + 2 < rest.len() => {}
        _ => return false,
    }
    name.chars().all(is_name_char)
}

/// Go: `ClaudeMCPAliasWordCount`.
pub fn claude_mcp_alias_word_count() -> usize {
    WORDS.len()
}

/// Go: `ClaudeMCPToolAlias`. Derives the alias for `original` under `secret`; a higher `attempt`
/// linearly probes the next tool word.
pub fn claude_mcp_tool_alias(secret: &str, original: &str, attempt: u32) -> String {
    let tool_digest = alias_digest(secret, "tool", original);
    alias_for(
        &server_component(secret),
        &alias_word(&tool_digest, 0, attempt),
        original,
    )
}

/// Go: `AllocateClaudeMCPToolAlias`. Picks the first probe attempt whose alias is not in
/// `reserved`; `None` when the wordlist is empty or every tool word is already reserved.
pub fn allocate_claude_mcp_tool_alias(
    secret: &str,
    original: &str,
    reserved: Option<&HashSet<String>>,
) -> Option<String> {
    let words = &*WORDS;
    let total = words.len();
    if total == 0 {
        tracing::error!(
            "claude oauth mcp alias: embedded BIP-39 wordlist is empty, tool aliasing is disabled"
        );
        return None;
    }
    let server = server_component(secret);
    let tool_digest = alias_digest(secret, "tool", original);
    let base = u16::from_be_bytes([tool_digest[0], tool_digest[1]]) as usize % total;
    for attempt in 0..total {
        let alias = alias_for(&server, words[(base + attempt) % total], original);
        if reserved.is_some_and(|r| r.contains(&alias)) {
            continue;
        }
        return Some(alias);
    }
    None
}

/// Go: `claudeMCPAliasFor`. Both entry points build names here so they cannot drift apart.
fn alias_for(server: &str, tool_id: &str, original: &str) -> String {
    let prefix = format!("mcp__{server}__{tool_id}_");
    let max_semantic = 64usize.saturating_sub(prefix.len()).max(1);
    format!("{prefix}{}", semantic_suffix(original, max_semantic))
}

/// Go: `claudeMCPAliasServerComponent`. Two-word virtual server shared by one caller's aliases.
fn server_component(secret: &str) -> String {
    let digest = alias_digest(secret, "server", "");
    format!(
        "{}_{}",
        alias_word(&digest, 0, 0),
        alias_word(&digest, 2, 0)
    )
}

/// Go: `claudeMCPAliasWord`.
fn alias_word(digest: &[u8], offset: usize, attempt: u32) -> String {
    let words = &*WORDS;
    if words.is_empty() || offset + 2 > digest.len() {
        return "tool".to_string();
    }
    let base = u16::from_be_bytes([digest[offset], digest[offset + 1]]) as usize;
    words[(base + attempt as usize) % words.len()].to_string()
}

/// Go: `claudeMCPToolSemanticSuffix`. Collapses runs of invalid characters into one `_`,
/// truncates to `max_length` bytes and trims `_-` from both ends (empty becomes `tool`).
fn semantic_suffix(original: &str, max_length: usize) -> String {
    let mut semantic = String::with_capacity(original.len().min(max_length));
    let mut pending_separator = false;
    for c in original.chars() {
        if !is_name_char(c) {
            pending_separator = !semantic.is_empty();
            continue;
        }
        if pending_separator && semantic.len() + 1 < max_length {
            semantic.push('_');
        }
        pending_separator = false;
        if semantic.len() >= max_length {
            break;
        }
        semantic.push(c);
    }
    let trimmed = semantic.trim_matches(|c| c == '_' || c == '-');
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Go: `claudeMCPAliasDigest`. HMAC-SHA256 keyed by `secret` over
/// `"cpa-claude-mcp-alias-v2\0" + purpose + "\0" + original`.
fn alias_digest(secret: &str, purpose: &str, original: &str) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret.as_bytes()));
    } else {
        key[..secret.len()].copy_from_slice(secret.as_bytes());
    }
    let mut inner = Sha256::new();
    inner.update(key.map(|b| b ^ 0x36));
    inner.update(b"cpa-claude-mcp-alias-v2\0");
    inner.update(purpose.as_bytes());
    inner.update([0u8]);
    inner.update(original.as_bytes());
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_alias_words(alias: &str) {
        let parts: Vec<&str> = alias.split("__").collect();
        assert_eq!(
            parts.len(),
            3,
            "alias {alias:?} does not have mcp/server/tool parts"
        );
        let server_words: Vec<&str> = parts[1].split('_').collect();
        assert_eq!(
            server_words.len(),
            2,
            "alias {alias:?} server is not two words"
        );
        let (tool_id, _) = parts[2].split_once('_').expect("semantic suffix");
        let allowed: HashSet<&str> = WORDS.iter().copied().collect();
        for word in server_words.iter().copied().chain([tool_id]) {
            assert!(
                allowed.contains(word),
                "alias {alias:?} uses non-BIP39 word {word:?}"
            );
        }
    }

    #[test]
    fn mcp_tool_name_predicate() {
        for name in [
            "mcp__context7__query-docs",
            "mcp__amber_cedar__quiet_harbor",
            "mcp__server__tool__variant",
        ] {
            assert!(is_claude_mcp_tool_name(name), "{name}");
        }
        let too_long = format!("mcp__context7__{}", "x".repeat(64));
        for name in [
            "context7__query-docs",
            "mcp____query-docs",
            "mcp__context7__",
            "mcp__context7__query.docs",
            too_long.as_str(),
        ] {
            assert!(!is_claude_mcp_tool_name(name), "{name}");
        }
    }

    #[test]
    fn alias_is_deterministic_word_based_and_shares_server() {
        let first = claude_mcp_tool_alias("credential-secret", "search_web", 0);
        assert_eq!(
            first,
            claude_mcp_tool_alias("credential-secret", "search_web", 0)
        );
        let case_distinct = claude_mcp_tool_alias("credential-secret", "Search_Web", 0);
        assert_ne!(first, case_distinct);
        let retry = claude_mcp_tool_alias("credential-secret", "search_web", 1);
        assert_ne!(first, retry);
        assert!(is_claude_mcp_tool_name(&first));
        assert!(first.ends_with("_search_web"));
        let re = regex::Regex::new(r"^mcp__[a-z]+_[a-z]+__[a-z]+_search_web$").unwrap();
        assert!(re.is_match(&first), "{first}");
        assert_alias_words(&first);
        let server = first.split("__").nth(1).unwrap();
        assert_eq!(case_distinct.split("__").nth(1).unwrap(), server);
        assert_eq!(retry.split("__").nth(1).unwrap(), server);
        let other = claude_mcp_tool_alias("other-caller", "search_web", 0);
        assert_ne!(other.split("__").nth(1).unwrap(), server);
    }

    // Pinned against the Go implementation: guards the HMAC construction and word selection.
    #[test]
    fn alias_matches_go_reference_values() {
        assert_eq!(
            claude_mcp_tool_alias("credential-secret", "search_web", 0),
            GO_SEARCH_WEB
        );
        let long = "k".repeat(100);
        assert_eq!(claude_mcp_tool_alias(&long, "Bash", 3), GO_LONG_KEY_BASH);
    }
    const GO_SEARCH_WEB: &str = "mcp__useful_begin__cycle_search_web";
    const GO_LONG_KEY_BASH: &str = "mcp__sample_certain__gym_Bash";

    #[test]
    fn semantic_suffix_is_safe_and_bounded() {
        for (original, want) in [
            ("browser.open URL", "_browser_open_URL"),
            ("search.网页/tool with spaces", "_search_tool_with_spaces"),
            ("搜索网页", "_tool"),
        ] {
            let alias = claude_mcp_tool_alias("credential-secret", original, 0);
            assert!(is_claude_mcp_tool_name(&alias), "{alias}");
            assert!(alias.len() <= 64);
            assert!(alias.ends_with(want), "{alias} should end with {want}");
        }

        let original = "a".repeat(68);
        let alias = claude_mcp_tool_alias("credential-secret", &original, 0);
        let prefix_len = alias.rfind('_').expect("semantic separator") + 1;
        let want_len = (64usize.saturating_sub(prefix_len)).max(1);
        assert_eq!(&alias[prefix_len..], "a".repeat(want_len));
        assert_eq!(alias.len(), 64);
    }

    #[test]
    fn strict_64_char_limit_under_all_word_combinations() {
        for i in 0..claude_mcp_alias_word_count() {
            let secret = format!("test-secret-{i}");
            let original = format!("tool_{i}_long_name_").repeat(50);
            let alias = claude_mcp_tool_alias(&secret, &original, i as u32);
            assert!(alias.len() <= 64, "{alias}");
            assert!(is_claude_mcp_tool_name(&alias), "{alias}");
            assert_alias_words(&alias);
        }
    }

    #[test]
    fn probing_covers_every_word_once() {
        let total = claude_mcp_alias_word_count();
        let mut seen = HashSet::new();
        for attempt in 0..total {
            let alias = claude_mcp_tool_alias("test-secret", "tool.name", attempt as u32);
            let tool = alias.split("__").nth(2).unwrap();
            let (tool_id, _) = tool.split_once('_').unwrap();
            assert!(
                seen.insert(tool_id.to_string()),
                "duplicate tool id {tool_id}"
            );
        }
        assert_eq!(seen.len(), total);
    }

    #[test]
    fn allocation_exhausts_and_matches_single_shot() {
        let secret = "exhaust-space";
        let total = claude_mcp_alias_word_count();
        let mut reserved: HashSet<String> = (0..total)
            .map(|a| claude_mcp_tool_alias(secret, "tool.name", a as u32))
            .collect();
        assert!(allocate_claude_mcp_tool_alias(secret, "tool.name", Some(&reserved)).is_none());
        assert!(allocate_claude_mcp_tool_alias(secret, "tool.name", None).is_some());

        // Allocating one by one yields exactly the single-shot probe sequence.
        let secret = "shared-construction";
        let long = "long_tool_name_".repeat(9);
        for original in ["Bash", "read_file", long.as_str()] {
            reserved.clear();
            for attempt in 0..total {
                let allocated = allocate_claude_mcp_tool_alias(secret, original, Some(&reserved))
                    .unwrap_or_else(|| panic!("{original}: exhausted at {attempt}"));
                assert_eq!(
                    allocated,
                    claude_mcp_tool_alias(secret, original, attempt as u32)
                );
                reserved.insert(allocated);
            }
            assert!(allocate_claude_mcp_tool_alias(secret, original, Some(&reserved)).is_none());
        }
    }

    #[test]
    fn wordlist_integrity() {
        assert_eq!(claude_mcp_alias_word_count(), 2048);
        assert_eq!(WORDS[0], "abandon");
        assert_eq!(WORDS[2047], "zoo");
        let mut seen = HashSet::new();
        for word in WORDS.iter() {
            assert!(seen.insert(*word), "duplicate {word}");
            assert!(!word.is_empty() && word.len() <= 8, "{word}");
            assert!(word.bytes().all(|b| b.is_ascii_lowercase()), "{word}");
        }
    }
}
