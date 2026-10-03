//! Single-line text input (Go: bubbles/textinput with its default key map). The cursor is drawn as
//! a reversed cell and does not blink, so an idle screen never repaints.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use crate::keys::Key;
use crate::styles;

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub prompt: String,
    pub placeholder: String,
    /// Maximum characters; 0 means unlimited.
    pub char_limit: usize,
    /// Visible width of the value area in cells; 0 means unbounded.
    pub width: usize,
    /// Render every character as `*` (password mode).
    pub password: bool,
    value: Vec<char>,
    pos: usize,
    offset: usize,
    focused: bool,
}

fn is_space(c: char) -> bool {
    c.is_whitespace()
}

impl TextInput {
    pub fn new() -> Self {
        TextInput { prompt: "> ".into(), ..Default::default() }
    }

    pub fn value(&self) -> String {
        self.value.iter().collect()
    }

    /// `SetValue`: replaces the text (truncated to the char limit) and moves the cursor to the end.
    pub fn set_value(&mut self, s: &str) {
        self.value = s.chars().collect();
        if self.char_limit > 0 {
            self.value.truncate(self.char_limit);
        }
        self.pos = self.value.len();
        self.offset = 0;
        self.fix_offset();
    }

    pub fn focus(&mut self) {
        self.focused = true;
    }

    pub fn blur(&mut self) {
        self.focused = false;
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Inserts typed or pasted text at the cursor, honouring the char limit. Control characters are
    /// dropped and newlines become spaces like `insertRunesFromUserInput`.
    pub fn insert_text(&mut self, text: &str) {
        let mut incoming: Vec<char> = text
            .chars()
            .filter_map(|c| match c {
                '\n' | '\r' | '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        if self.char_limit > 0 {
            let room = self.char_limit.saturating_sub(self.value.len());
            incoming.truncate(room);
        }
        let n = incoming.len();
        self.value.splice(self.pos..self.pos, incoming);
        self.pos += n;
        self.fix_offset();
    }

    /// Applies a key from the default key map; unknown keys are ignored.
    pub fn handle_key(&mut self, key: &Key) {
        match key.as_str() {
            "left" | "ctrl+b" => self.pos = self.pos.saturating_sub(1),
            "right" | "ctrl+f" => self.pos = (self.pos + 1).min(self.value.len()),
            "ctrl+left" | "alt+left" | "alt+b" => self.pos = self.word_backward_pos(),
            "ctrl+right" | "alt+right" | "alt+f" => self.pos = self.word_forward_pos(),
            "home" | "ctrl+a" => self.pos = 0,
            "end" | "ctrl+e" => self.pos = self.value.len(),
            "backspace" | "ctrl+h" => {
                if self.pos > 0 {
                    self.value.remove(self.pos - 1);
                    self.pos -= 1;
                }
            }
            "delete" | "ctrl+d" => {
                if self.pos < self.value.len() {
                    self.value.remove(self.pos);
                }
            }
            "ctrl+w" | "alt+backspace" => {
                let start = self.word_backward_pos();
                self.value.drain(start..self.pos);
                self.pos = start;
            }
            "alt+d" | "alt+delete" => {
                let end = self.word_forward_pos();
                self.value.drain(self.pos..end);
            }
            "ctrl+k" => self.value.truncate(self.pos),
            "ctrl+u" => {
                self.value.drain(..self.pos);
                self.pos = 0;
            }
            _ => {
                if let Some(c) = key.text {
                    self.insert_text(&c.to_string());
                }
            }
        }
        self.fix_offset();
    }

    fn word_backward_pos(&self) -> usize {
        let mut i = self.pos;
        while i > 0 && is_space(self.value[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_space(self.value[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_forward_pos(&self) -> usize {
        let len = self.value.len();
        let mut i = self.pos;
        while i < len && is_space(self.value[i]) {
            i += 1;
        }
        while i < len && !is_space(self.value[i]) {
            i += 1;
        }
        i
    }

    fn display_char(&self, c: char) -> char {
        if self.password { '*' } else { c }
    }

    fn cell_width(&self, c: char) -> usize {
        self.display_char(c).width().unwrap_or(0)
    }

    /// Keeps the cursor inside the horizontal window when `width` is bounded.
    fn fix_offset(&mut self) {
        if self.width == 0 {
            self.offset = 0;
            return;
        }
        if self.pos < self.offset {
            self.offset = self.pos;
        }
        // The cursor cell itself needs one column.
        loop {
            let used: usize = self.value[self.offset..self.pos].iter().map(|c| self.cell_width(*c)).sum();
            if used + 1 > self.width && self.offset < self.pos {
                self.offset += 1;
            } else {
                break;
            }
        }
    }

    /// `View`: prompt, text and cursor as spans (cursor reversed while focused).
    pub fn view(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(self.prompt.clone())];
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        if self.value.is_empty() {
            let mut chars = self.placeholder.chars();
            let first = chars.next();
            let rest: String = chars.collect();
            match (self.focused, first) {
                (true, Some(c)) => {
                    spans.push(Span::styled(c.to_string(), cursor_style));
                    spans.push(Span::styled(rest, styles::help()));
                }
                (true, None) => spans.push(Span::styled(" ", cursor_style)),
                (false, _) => {
                    let text = self.placeholder.clone();
                    spans.push(Span::styled(text, styles::help()));
                }
            }
            return spans;
        }
        let mut budget = if self.width == 0 { usize::MAX } else { self.width };
        let mut before = String::new();
        let mut at_cursor: Option<char> = None;
        let mut after = String::new();
        for (i, &c) in self.value.iter().enumerate().skip(self.offset) {
            let w = self.cell_width(c);
            let shown = self.display_char(c);
            if i == self.pos {
                if w > budget {
                    break;
                }
                budget -= w;
                at_cursor = Some(shown);
            } else {
                if w > budget {
                    break;
                }
                budget -= w;
                if i < self.pos {
                    before.push(shown);
                } else {
                    after.push(shown);
                }
            }
        }
        spans.push(Span::raw(before));
        if self.focused {
            match at_cursor {
                Some(c) => spans.push(Span::styled(c.to_string(), cursor_style)),
                None => spans.push(Span::styled(" ", cursor_style)),
            }
        } else if let Some(c) = at_cursor {
            spans.push(Span::raw(c.to_string()));
        }
        spans.push(Span::raw(after));
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(ti: &TextInput) -> String {
        ti.view().iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn edits_like_bubbles() {
        let mut ti = TextInput::new();
        ti.focus();
        ti.set_value("hello world");
        ti.handle_key(&Key::named("ctrl+w"));
        assert_eq!(ti.value(), "hello ");
        ti.handle_key(&Key::named("home"));
        ti.handle_key(&Key::named("delete"));
        assert_eq!(ti.value(), "ello ");
        ti.handle_key(&Key::named("x"));
        assert_eq!(ti.value(), "xello ");
        ti.handle_key(&Key::named("ctrl+k"));
        assert_eq!(ti.value(), "x");
    }

    #[test]
    fn password_masks_and_char_limit_applies() {
        let mut ti = TextInput::new();
        ti.password = true;
        ti.char_limit = 4;
        ti.prompt = String::new();
        ti.insert_text("secret");
        assert_eq!(ti.value(), "secr");
        assert_eq!(text_of(&ti), "****");
    }

    #[test]
    fn bounded_width_scrolls_with_cursor() {
        let mut ti = TextInput::new();
        ti.prompt = String::new();
        ti.width = 5;
        ti.focus();
        ti.set_value("abcdefghij");
        assert_eq!(text_of(&ti), "ghij ");
        ti.handle_key(&Key::named("home"));
        assert_eq!(text_of(&ti), "abcde");
    }
}
