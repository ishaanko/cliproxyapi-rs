//! Best-effort comment preservation for rewritten YAML.
//!
//! serde YAML values carry no comments, so when a config file is rewritten (hashing the
//! management secret, v8 migration, saves from the management API) comments would be lost. The Go
//! implementation edits a `yaml.Node` tree to avoid that. Here the original text is scanned for
//! comments keyed by the path of the mapping key / sequence item they belong to, the value is
//! re-serialised, and the comments are re-inserted at the same paths. Paths can be moved when the
//! layout migration relocates a field.
//!
//! The scanner understands block-style YAML (what config files use) plus single-line flow values.
//! Comments inside multi-line flow collections or scalars are not tracked.

use std::collections::BTreeMap;

/// One step of a document path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Seg {
    Key(String),
    Index(usize),
}

pub(crate) type CPath = Vec<Seg>;

/// Converts a dotted key path ("server.tls.enable") to a [`CPath`].
pub(crate) fn dotted(path: &str) -> CPath {
    path.split('.').map(|k| Seg::Key(k.to_string())).collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Comments {
    /// Standalone comment lines above a node. An empty string is a blank line.
    head: BTreeMap<CPath, Vec<String>>,
    /// Trailing `# ...` comment on the node's own line.
    line: BTreeMap<CPath, String>,
    /// Comment lines after the last node.
    pub foot: Vec<String>,
    /// How the source quoted its string scalars (kept when the document is written back).
    pub(crate) styles: crate::emit::Styles,
    /// Indentation step used when rendering; 0 means the default of 2.
    pub(crate) indent: usize,
}

/// What the scanner learned about one content line.
struct ScannedLine {
    /// Shallowest path introduced on this line (the sequence item for `- key: v`).
    shallow: CPath,
    /// Deepest path introduced on this line (the key itself).
    deepest: CPath,
    /// Byte offset where an inline comment starts, if any.
    inline_comment: Option<usize>,
}

enum Frame {
    Key { col: usize, key: String },
    Item { col: usize, index: usize },
}

impl Frame {
    fn col(&self) -> usize {
        match self {
            Frame::Key { col, .. } | Frame::Item { col, .. } => *col,
        }
    }
}

fn frame_path(stack: &[Frame]) -> CPath {
    stack
        .iter()
        .map(|f| match f {
            Frame::Key { key, .. } => Seg::Key(key.clone()),
            Frame::Item { index, .. } => Seg::Index(*index),
        })
        .collect()
}

/// Line-by-line scanner shared by extraction and re-insertion.
struct Scanner {
    stack: Vec<Frame>,
    /// Indent of the key that opened a block scalar; deeper lines are scalar content.
    block_scalar: Option<usize>,
    /// Open flow-collection depth continued from a previous line.
    flow_depth: i32,
}

enum LineKind {
    Blank,
    Comment,
    /// Doc markers, scalar content, anything without a path.
    Other,
    Node(ScannedLine),
}

impl Scanner {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            block_scalar: None,
            flow_depth: 0,
        }
    }

    fn classify(&mut self, raw: &str) -> LineKind {
        let trimmed = raw.trim_start_matches(' ');
        let indent = raw.len() - trimmed.len();
        if trimmed.trim().is_empty() {
            return LineKind::Blank;
        }
        if let Some(block_indent) = self.block_scalar {
            if indent > block_indent {
                return LineKind::Other;
            }
            self.block_scalar = None;
        }
        if self.flow_depth > 0 {
            self.flow_depth += flow_delta(trimmed);
            return LineKind::Other;
        }
        if trimmed.starts_with('#') {
            return LineKind::Comment;
        }
        if trimmed.starts_with("---") || trimmed.starts_with("...") || trimmed.starts_with('%') {
            return LineKind::Other;
        }

        let mut col = indent;
        let mut rest = trimmed;
        let mut shallow: Option<CPath> = None;
        // Sequence item markers, possibly nested ("- - x").
        while rest == "-" || rest.starts_with("- ") {
            self.pop_for_dash(col);
            let index = match self.stack.last_mut() {
                Some(Frame::Item { col: c, index }) if *c == col => {
                    *index += 1;
                    *index
                }
                _ => {
                    self.stack.push(Frame::Item { col, index: 0 });
                    0
                }
            };
            if shallow.is_none() {
                let mut path = frame_path(&self.stack[..self.stack.len() - 1]);
                path.push(Seg::Index(index));
                shallow = Some(path);
            }
            let after = rest[1..].trim_start_matches(' ');
            col += rest.len() - after.len();
            rest = after;
        }
        let mut deepest = None;
        if let Some((key, value)) = split_key(rest) {
            while self.stack.last().is_some_and(|f| f.col() >= col) {
                self.stack.pop();
            }
            self.stack.push(Frame::Key { col, key });
            deepest = Some(frame_path(&self.stack));
            let value = value.trim_start();
            if value.starts_with('|') || value.starts_with('>') {
                self.block_scalar = Some(col);
            } else {
                let delta = flow_delta(value);
                if delta > 0 {
                    self.flow_depth = delta;
                }
            }
        } else {
            let delta = flow_delta(rest);
            if delta > 0 {
                self.flow_depth = delta;
            }
        }
        let Some(deepest) = deepest.or_else(|| shallow.clone()) else {
            return LineKind::Other;
        };
        let shallow = shallow.unwrap_or_else(|| deepest.clone());
        let inline_comment = if self.flow_depth > 0 {
            None
        } else {
            inline_comment_start(raw)
        };
        LineKind::Node(ScannedLine {
            shallow,
            deepest,
            inline_comment,
        })
    }

    /// Frames deeper than a dash at `col` are finished; keys/items at `col` stay (a sequence may
    /// sit at the same indent as its parent key).
    fn pop_for_dash(&mut self, col: usize) {
        while self.stack.last().is_some_and(|f| f.col() > col) {
            self.stack.pop();
        }
    }
}

/// Splits "key: value" (plain or quoted key). Returns `None` for non-mapping lines.
fn split_key(s: &str) -> Option<(String, &str)> {
    let (key, after) = if let Some(rest) = s.strip_prefix('"') {
        let mut escaped = false;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i);
                break;
            }
        }
        let end = end?;
        (rest[..end].replace("\\\"", "\""), &rest[end + 1..])
    } else if let Some(rest) = s.strip_prefix('\'') {
        let mut end = None;
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                end = Some(i);
                break;
            }
            i += 1;
        }
        let end = end?;
        (rest[..end].replace("''", "'"), &rest[end + 1..])
    } else {
        if s.starts_with(['[', '{', '#', '&', '*', '!', '|', '>']) {
            return None;
        }
        // The key ends at the first ": " (or a trailing ':').
        let idx = s
            .match_indices(':')
            .find(|(i, _)| s[i + 1..].is_empty() || s[i + 1..].starts_with(' '))
            .map(|(i, _)| i)?;
        (s[..idx].trim_end().to_string(), &s[idx..])
    };
    let after = after.strip_prefix(':')?;
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    Some((key, after))
}

/// Net change in flow-collection nesting on this text (ignores quoted text and comments).
fn flow_delta(s: &str) -> i32 {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some('"') => {
                if c == '\\' {
                    chars.next();
                } else if c == '"' {
                    quote = None;
                }
            }
            Some(_) => {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '"' | '\'' if prev_space || depth > 0 => quote = Some(c),
                '#' if prev_space => break,
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
        prev_space = c == ' ';
    }
    depth
}

/// Byte offset of a trailing comment in a full line, if there is one.
fn inline_comment_start(line: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    let mut iter = line.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match quote {
            Some('"') => {
                if c == '\\' {
                    iter.next();
                } else if c == '"' {
                    quote = None;
                }
            }
            Some(_) => {
                if c == '\'' {
                    if iter.peek().is_some_and(|(_, n)| *n == '\'') {
                        iter.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '"' | '\'' if prev_space || line[..i].ends_with(": ") => quote = Some(c),
                '#' if prev_space && i > 0 => return Some(i),
                _ => {}
            },
        }
        prev_space = c == ' ';
    }
    None
}

impl Comments {
    /// Collects the comments of a YAML document.
    pub(crate) fn extract(text: &str) -> Self {
        let mut out = Comments {
            styles: crate::emit::Styles::from_text(text),
            ..Comments::default()
        };
        let mut scanner = Scanner::new();
        let mut pending: Vec<String> = Vec::new();
        let mut blank_before = false;
        for raw in text.lines() {
            match scanner.classify(raw) {
                LineKind::Blank => blank_before = true,
                LineKind::Comment => {
                    if blank_before && pending.is_empty() {
                        pending.push(String::new());
                    }
                    blank_before = false;
                    pending.push(raw.trim().to_string());
                }
                LineKind::Other => {
                    blank_before = false;
                }
                LineKind::Node(info) => {
                    if blank_before && pending.is_empty() {
                        pending.push(String::new());
                    }
                    blank_before = false;
                    if !pending.is_empty() {
                        out.head
                            .entry(info.shallow.clone())
                            .or_default()
                            .append(&mut pending);
                    }
                    if let Some(at) = info.inline_comment {
                        out.line
                            .insert(info.deepest, raw[at..].trim_end().to_string());
                    }
                }
            }
        }
        out.foot = pending;
        // A trailing blank marker is meaningless on its own.
        while out.foot.first().is_some_and(String::is_empty) && out.foot.len() == 1 {
            out.foot.clear();
        }
        out
    }

    /// Re-inserts the collected comments into freshly serialised YAML.
    pub(crate) fn apply(&self, rendered: &str) -> String {
        let mut out = String::with_capacity(rendered.len() + 256);
        let mut scanner = Scanner::new();
        let mut used_head = std::collections::HashSet::new();
        let mut used_line = std::collections::HashSet::new();
        for raw in rendered.lines() {
            match scanner.classify(raw) {
                LineKind::Node(info) => {
                    if let Some(lines) = self.head.get(&info.shallow)
                        && used_head.insert(info.shallow.clone())
                    {
                        for l in lines {
                            // No blank line at the very top, doubled up, or right after a
                            // key that opens a block.
                            if l.is_empty()
                                && (out.is_empty() || out.ends_with("\n\n") || opens_block(&out))
                            {
                                continue;
                            }
                            out.push_str(l);
                            out.push('\n');
                        }
                    }
                    out.push_str(raw);
                    if info.inline_comment.is_none()
                        && let Some(c) = self.line.get(&info.deepest)
                        && used_line.insert(info.deepest.clone())
                    {
                        out.push(' ');
                        out.push_str(c);
                    }
                    out.push('\n');
                }
                _ => {
                    out.push_str(raw);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Removes and returns the comments of `prefix` and everything under it.
    pub(crate) fn take_prefix(&mut self, prefix: &CPath) -> Comments {
        let mut taken = Comments::default();
        let head: Vec<CPath> = self
            .head
            .keys()
            .filter(|p| p.starts_with(prefix))
            .cloned()
            .collect();
        for path in head {
            if let Some(v) = self.head.remove(&path) {
                taken.head.insert(path, v);
            }
        }
        let line: Vec<CPath> = self
            .line
            .keys()
            .filter(|p| p.starts_with(prefix))
            .cloned()
            .collect();
        for path in line {
            if let Some(v) = self.line.remove(&path) {
                taken.line.insert(path, v);
            }
        }
        taken
    }

    /// Re-inserts every comment of `other` (existing ones at the same path are replaced).
    pub(crate) fn merge(&mut self, other: Comments) {
        self.head.extend(other.head);
        self.line.extend(other.line);
    }

    /// Copies the comments of `from_path` (and below, minus entries `skip` rejects given the
    /// path relative to `from_path`) from `source` to `to_path`. Head comments are appended to
    /// any already present, so a group head and a key head can end up on one entry.
    pub(crate) fn transplant(
        &mut self,
        source: &Comments,
        from_path: &CPath,
        to_path: &CPath,
        skip: impl Fn(&[Seg]) -> bool,
    ) {
        let rebase = |path: &CPath| -> Option<CPath> {
            let rest = path.strip_prefix(from_path.as_slice())?;
            if skip(rest) {
                return None;
            }
            let mut new = to_path.clone();
            new.extend_from_slice(rest);
            Some(new)
        };
        for (path, lines) in &source.head {
            if let Some(new) = rebase(path) {
                self.head
                    .entry(new)
                    .or_default()
                    .extend(lines.iter().cloned());
            }
        }
        for (path, text) in &source.line {
            if let Some(new) = rebase(path) {
                self.line.insert(new, text.clone());
            }
        }
    }

    /// Copies only the comments attached to the node at `from_path` itself.
    pub(crate) fn transplant_exact(
        &mut self,
        source: &Comments,
        from_path: &CPath,
        to_path: &CPath,
    ) {
        self.transplant(source, from_path, to_path, |rest| !rest.is_empty());
    }

    /// Moves the comments of `from` (and everything under it) to `to`.
    pub(crate) fn move_prefix(&mut self, from: &CPath, to: &CPath) {
        let rekey = |path: &CPath| -> Option<CPath> {
            path.starts_with(from).then(|| {
                let mut new = to.clone();
                new.extend_from_slice(&path[from.len()..]);
                new
            })
        };
        let moved_head: Vec<_> = self
            .head
            .keys()
            .filter(|p| p.starts_with(from))
            .cloned()
            .collect();
        for old in moved_head {
            if let (Some(new), Some(v)) = (rekey(&old), self.head.remove(&old)) {
                self.head.insert(new, v);
            }
        }
        let moved_line: Vec<_> = self
            .line
            .keys()
            .filter(|p| p.starts_with(from))
            .cloned()
            .collect();
        for old in moved_line {
            if let (Some(new), Some(v)) = (rekey(&old), self.line.remove(&old)) {
                self.line.insert(new, v);
            }
        }
    }

    /// Drops the comments of `path` and everything under it.
    pub(crate) fn remove_prefix(&mut self, path: &CPath) {
        self.head.retain(|p, _| !p.starts_with(path));
        self.line.retain(|p, _| !p.starts_with(path));
    }

    /// Re-numbers comments under sequence `seq` after its items were reordered or truncated.
    /// `new_to_old[new_index]` is the index the item had before (`None` for new items).
    pub(crate) fn permute_sequence(&mut self, seq: &CPath, new_to_old: &[Option<usize>]) {
        let mut old_to_new: BTreeMap<usize, usize> = BTreeMap::new();
        for (new, old) in new_to_old.iter().enumerate() {
            if let Some(old) = old {
                old_to_new.insert(*old, new);
            }
        }
        let rekey = |path: &CPath| -> Option<Option<CPath>> {
            if !path.starts_with(seq) || path.len() <= seq.len() {
                return None;
            }
            let Seg::Index(old) = path[seq.len()] else {
                return None;
            };
            Some(old_to_new.get(&old).map(|new| {
                let mut p = path.clone();
                p[seq.len()] = Seg::Index(*new);
                p
            }))
        };
        let mut head = BTreeMap::new();
        for (path, v) in std::mem::take(&mut self.head) {
            match rekey(&path) {
                None => {
                    head.insert(path, v);
                }
                Some(Some(new)) => {
                    head.insert(new, v);
                }
                Some(None) => {}
            }
        }
        self.head = head;
        let mut line = BTreeMap::new();
        for (path, v) in std::mem::take(&mut self.line) {
            match rekey(&path) {
                None => {
                    line.insert(path, v);
                }
                Some(Some(new)) => {
                    line.insert(new, v);
                }
                Some(None) => {}
            }
        }
        self.line = line;
    }
}

/// Whether the last line of `out` is a "key:" line whose value is the block that follows.
fn opens_block(out: &str) -> bool {
    out.trim_end_matches('\n')
        .lines()
        .last()
        .is_some_and(|l| l.trim_end().ends_with(':'))
}

/// Removes indentation from standalone comment lines so they stay left aligned
/// (port of `NormalizeCommentIndentation`).
pub fn normalize_comment_indentation(data: &str) -> String {
    let mut changed = false;
    let lines: Vec<&str> = data
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_start_matches([' ', '\t']);
            if trimmed.starts_with('#') && trimmed.len() != line.len() {
                changed = true;
                trimmed
            } else {
                line
            }
        })
        .collect();
    if changed {
        lines.join("\n")
    } else {
        data.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_follow_their_keys_through_a_rewrite() {
        let original = "\
# top
a: 1 # one

# about b
b:
  # about c
  c: [x, y]
  list:
    - name: first # f
    - name: second
";
        let comments = Comments::extract(original);
        let rewritten =
            "b:\n  list:\n  - name: first\n  - name: second\n  c:\n  - x\n  - y\na: 1\n";
        let out = comments.apply(rewritten);
        assert!(out.contains("# about b\nb:"), "{out}");
        assert!(out.contains("# about c\n  c:"), "{out}");
        assert!(out.contains("name: first # f"), "{out}");
        assert!(
            out.contains("# top\n") && out.contains("a: 1 # one"),
            "{out}"
        );
    }

    #[test]
    fn moved_paths_take_their_comments() {
        let mut comments = Comments::extract("# keep\nport: 1\n");
        comments.move_prefix(&dotted("port"), &dotted("server.port"));
        let out = comments.apply("server:\n  port: 1\n");
        assert!(out.contains("# keep\n  port: 1"), "{out}");
    }
}
