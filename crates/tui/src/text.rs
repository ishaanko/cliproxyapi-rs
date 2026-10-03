//! Display-width helpers (lipgloss.Width / fitStringWidth equivalents).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Terminal cell width of `s`.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Pads `s` with spaces on the right up to `w` cells (never truncates).
pub fn pad_to(s: &str, w: usize) -> String {
    let have = width(s);
    if have >= w {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(w - have))
}

/// `fitStringWidth`: the longest prefix of `s` that fits in `max` cells.
pub fn fit_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// Byte-oriented truncation used by the Go tables (`s[:n-3] + "..."`), kept on char boundaries.
pub fn truncate_bytes(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        let keep = max_len.saturating_sub(3);
        let mut end = keep.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    } else {
        s.to_string()
    }
}

/// `wrapText`: hard-wraps at `max_width` bytes (the Go code slices bytes, URLs are ASCII).
pub fn wrap_text(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut rest = s;
    while rest.len() > max_width {
        let mut cut = max_width;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        lines.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        lines.push(rest.to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_fits() {
        assert_eq!(wrap_text("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(wrap_text("", 3), Vec::<String>::new());
        assert_eq!(fit_width("日本語", 5), "日本");
        assert_eq!(pad_to("日本", 6), "日本  ");
        assert_eq!(truncate_bytes("abcdefghij", 8), "abcde...");
    }
}
