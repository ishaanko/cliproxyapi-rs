//! Raw-byte prefilter for the Claude request classifiers (Go: helps/claude_json_prefilter.go).
//!
//! A Claude Code request body carries the whole conversation (often several MB); a byte search for
//! a needle is far cheaper than parsing or walking it, so the classifiers ask here first.

use memchr::memmem;

/// Whether decoding the JSON document `body` could yield a key or string value containing any of
/// `needles`.
///
/// Every needle must be printable ASCII without `\` or `/` (JSON may spell `/` as `\/`). A `"` may
/// only stand at either end of a needle, where it matches a structural string delimiter (`"1h"`
/// matches the whole string value `1h`). JSON can only spell such a character as itself or as a
/// `\u00XX` escape, so when `body` holds no `\u` escape of a printable ASCII character, a needle
/// absent from the raw bytes is absent from every decoded string too. If such an escape is present
/// the answer is true and the caller falls back to its full walk, so the result never changes,
/// only the cost.
pub fn json_may_contain_ascii(body: &[u8], needles: &[&str]) -> bool {
    has_printable_ascii_unicode_escape(body) || needles.iter().any(|needle| memmem::find(body, needle.as_bytes()).is_some())
}

/// Whether `body` contains a `\u00XX` escape whose first hex digit is 2-7, i.e. a printable ASCII
/// character. It errs only toward "yes": it also matches DEL (7F), malformed escapes such as a
/// 2-7 digit followed by a non-hex character, an escape truncated after that digit, and an escaped
/// backslash followed by literal text such as `\\u0041`. Each of those only makes the caller fall
/// back to a full walk.
fn has_printable_ascii_unicode_escape(body: &[u8]) -> bool {
    let finder = memmem::Finder::new(br"\u00");
    let mut rest = body;
    loop {
        let Some(i) = finder.find(rest) else { return false };
        if i + 4 >= rest.len() {
            return false;
        }
        if matches!(rest[i + 4], b'2'..=b'7') {
            return true;
        }
        rest = &rest[i + 4..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::helps::diagnostics::{claude_payload_has_1h_ttl, is_claude_probe_or_helper_request};

    #[test]
    fn may_contain_ascii() {
        let cases: &[(&str, &str, &[&str], bool)] = &[
            ("absent", r#"{"system":"hello"}"#, &["naming a coding session"], false),
            ("present", r#"{"system":"naming a coding session"}"#, &["naming a coding session"], true),
            ("any needle", r#"{"output_config":{}}"#, &["naming", "output_config"], true),
            ("escaped space falls back", r#"{"system":"naming a coding session"}"#, &["naming a coding session"], true),
            ("escaped letter falls back", r#"{"s":"naming"}"#, &["naming"], true),
            ("upper hex falls back", r#"{"s":"N"}"#, &["N"], true),
            ("escaped backslash is conservative", r#"{"s":"\\u0041"}"#, &["zzz"], true),
            ("control escape keeps shortcut", "{\"s\":\"a\\u001bb\u{e9}\"}", &["naming"], false),
            ("truncated escape", r#"{"s":"\u00"#, &["naming"], false),
            ("quoted value", r#"{"ttl":"1h"}"#, &[r#""1h""#], true),
            ("quoted value absent", r#"{"ttl":"5m","note":"11h"}"#, &[r#""1h""#], false),
        ];
        for (name, body, needles, want) in cases {
            assert_eq!(json_may_contain_ascii(body.as_bytes(), needles), *want, "{name}: {body}");
        }
    }

    /// The prefilters must never change an answer, including when the matched text is spelled
    /// with `\u` escapes.
    #[test]
    fn prefiltered_classifiers_keep_escaped_matches() {
        let escaped_title = r#"{"model":"m","system":[{"type":"text","text":"You are naming a coding session."}],"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(is_claude_probe_or_helper_request(escaped_title.as_bytes()), "escaped system title instruction");
        let plain_title = r#"{"model":"m","system":"Return a short title for this.","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(is_claude_probe_or_helper_request(plain_title.as_bytes()), "plain system title instruction");
        let schema_title = r#"{"model":"m","output_config":{"format":{"schema":{"properties":{"title":{"type":"string"}}}}},"messages":[{"role":"user","content":"<session>x</session>"}]}"#;
        assert!(is_claude_probe_or_helper_request(schema_title.as_bytes()), "title schema request");
        let ordinary = r#"{"model":"m","system":"You are Claude Code.","messages":[{"role":"user","content":"Return a short answer"}]}"#;
        assert!(!is_claude_probe_or_helper_request(ordinary.as_bytes()), "ordinary request");
        let escaped_1h = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert!(claude_payload_has_1h_ttl(escaped_1h.as_bytes()), "escaped 1h ttl");
        let five_minutes = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"took 11h","cache_control":{"type":"ephemeral","ttl":"5m"}}]}]}"#;
        assert!(!claude_payload_has_1h_ttl(five_minutes.as_bytes()), "5m ttl");
    }
}
