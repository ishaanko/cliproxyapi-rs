//! Best-effort comment carry-over for config edits.
//!
//! The v8 config tree handler edits a parsed YAML value, which drops comments when it is
//! serialized again. [`carry_comments`] re-attaches the comments of the previous document text
//! to the same mapping-key paths of the new text: the comment and blank lines above a key, an
//! inline comment on the key line, and trailing comments at the end of the file. Keys inside
//! sequences are not tracked (their comments are lost).

use std::collections::HashMap;

#[derive(Default)]
struct KeyComments {
    /// Comment and blank lines directly above the key line.
    leading: Vec<String>,
    /// Inline ` # ...` comment on the key line.
    trailing: Option<String>,
}

struct Scan {
    comments: HashMap<Vec<String>, KeyComments>,
    /// Comment lines after the last content line.
    footer: Vec<String>,
}

/// Key of a `key: value` / `key:` line, unquoted. `None` for anything else.
fn parse_key(trimmed: &str) -> Option<String> {
    let bytes = trimmed.as_bytes();
    let first = *bytes.first()?;
    if first == b'"' || first == b'\'' {
        let end = trimmed[1..].find(first as char)? + 1;
        let rest = trimmed[end + 1..].trim_start();
        return (rest == ":" || rest.starts_with(": ") || rest.starts_with(":\t")).then(|| trimmed[1..end].to_string());
    }
    let idx = trimmed.find(':')?;
    let after = &trimmed[idx + 1..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let key = trimmed[..idx].trim_end();
    (!key.is_empty() && !key.starts_with('#') && !key.starts_with('[') && !key.starts_with('{')).then(|| key.to_string())
}

/// Position of an inline comment (`#` preceded by whitespace, outside quotes).
fn inline_comment(line: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_space = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' if prev_space || i == 0 || line[..i].ends_with(": ") => quote = Some(c),
                '#' if prev_space => return Some(i),
                _ => {}
            },
        }
        prev_space = c == ' ' || c == '\t';
    }
    None
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#')
}

/// Walks the document and yields `(line index, key path)` for every mapping key outside
/// sequences.
fn key_lines(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    // Indent of the key owning the sequence we are inside of.
    let mut seq_owner: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        if is_comment_or_blank(line) {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        let is_item = trimmed == "-" || trimmed.starts_with("- ");
        if let Some(owner) = seq_owner {
            if is_item && indent >= owner || indent > owner {
                continue;
            }
            seq_owner = None;
        }
        if is_item {
            seq_owner = stack.last().map(|(ind, _)| *ind).or(Some(0));
            continue;
        }
        let Some(key) = parse_key(trimmed) else { continue };
        while stack.last().is_some_and(|(ind, _)| *ind >= indent) {
            stack.pop();
        }
        stack.push((indent, key));
        out.push((i, stack.iter().map(|(_, k)| k.clone()).collect()));
    }
    out
}

fn scan(text: &str) -> Scan {
    let lines: Vec<&str> = text.lines().collect();
    let mut comments: HashMap<Vec<String>, KeyComments> = HashMap::new();
    for (idx, path) in key_lines(text) {
        let mut start = idx;
        while start > 0 && is_comment_or_blank(lines[start - 1]) {
            start -= 1;
        }
        // Drop blank lines at the start of the block; keep interior separators.
        while start < idx && lines[start].trim().is_empty() {
            start += 1;
        }
        let leading: Vec<String> = lines[start..idx].iter().map(|l| (*l).to_string()).collect();
        let trailing = inline_comment(lines[idx]).map(|p| lines[idx][p..].to_string());
        comments.entry(path).or_insert(KeyComments { leading, trailing });
    }
    let last_content = lines.iter().rposition(|l| !is_comment_or_blank(l));
    let footer_start = last_content.map_or(0, |i| i + 1);
    let footer: Vec<String> = lines[footer_start..].iter().map(|l| (*l).to_string()).collect();
    Scan { comments, footer }
}

/// Re-attaches the comments of `old` to the matching keys of `new` (which has none).
pub(crate) fn carry_comments(old: &str, new: &str) -> String {
    let scanned = scan(old);
    if scanned.comments.values().all(|c| c.leading.is_empty() && c.trailing.is_none()) && scanned.footer.is_empty() {
        return new.to_string();
    }
    let lines: Vec<&str> = new.lines().collect();
    let by_line: HashMap<usize, Vec<String>> = key_lines(new).into_iter().collect();
    let mut out = String::with_capacity(new.len() + old.len() / 4);
    for (i, line) in lines.iter().enumerate() {
        let carried = by_line.get(&i).and_then(|path| scanned.comments.get(path));
        if let Some(c) = carried {
            for l in &c.leading {
                out.push_str(l);
                out.push('\n');
            }
        }
        out.push_str(line);
        if let Some(t) = carried.and_then(|c| c.trailing.as_ref())
            && inline_comment(line).is_none()
        {
            out.push(' ');
            out.push_str(t);
        }
        out.push('\n');
    }
    // Footer comments stay at the end of the file.
    let footer_start = scanned.footer.iter().position(|l| !l.trim().is_empty()).unwrap_or(scanned.footer.len());
    for l in &scanned.footer[footer_start..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_follow_their_keys_across_reformatting() {
        let old = "# header\nport: 8317 # listen\n\n# who may call us\naccess:\n    # keys\n    api-keys:\n        - a\n        - b\n\n# tail\n";
        let new = "port: 8317\naccess:\n  api-keys:\n  - a\n  - c\n";
        let out = carry_comments(old, new);
        assert_eq!(
            out,
            "# header\nport: 8317 # listen\n# who may call us\naccess:\n    # keys\n  api-keys:\n  - a\n  - c\n# tail\n"
        );
    }

    #[test]
    fn removed_keys_lose_their_comments_and_plain_text_is_untouched() {
        let old = "# gone\nold: 1\nkeep: 2\n";
        assert_eq!(carry_comments(old, "keep: 2\n"), "keep: 2\n");
        assert_eq!(carry_comments("a: 1\n", "a: 2\n"), "a: 2\n");
    }
}
