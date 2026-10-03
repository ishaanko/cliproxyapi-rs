//! Scrollable text pane (Go: bubbles/viewport with its default pager key map; horizontal scrolling
//! is off by default in v1 so wide lines are truncated).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

#[derive(Debug, Default, Clone)]
pub struct Viewport {
    pub width: usize,
    pub height: usize,
    y_offset: usize,
    lines: Vec<Line<'static>>,
}

impl Viewport {
    pub fn new(width: usize, height: usize) -> Self {
        Viewport { width, height, ..Default::default() }
    }

    pub fn y_offset(&self) -> usize {
        self.y_offset
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len().max(1)
    }

    fn max_y_offset(&self) -> usize {
        self.total_lines().saturating_sub(self.height)
    }

    pub fn at_top(&self) -> bool {
        self.y_offset == 0
    }

    pub fn at_bottom(&self) -> bool {
        self.y_offset >= self.max_y_offset()
    }

    /// `SetContent`: replaces the lines; an offset past the end snaps to the bottom.
    pub fn set_content(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines;
        if self.y_offset > self.total_lines() - 1 {
            self.goto_bottom();
        }
    }

    /// `SetYOffset`, clamped to the scrollable range.
    pub fn set_y_offset(&mut self, n: usize) {
        self.y_offset = n.min(self.max_y_offset());
    }

    pub fn goto_bottom(&mut self) {
        self.y_offset = self.max_y_offset();
    }

    pub fn scroll_down(&mut self, n: usize) {
        if self.at_bottom() || n == 0 || self.lines.is_empty() {
            return;
        }
        self.set_y_offset(self.y_offset + n);
    }

    pub fn scroll_up(&mut self, n: usize) {
        if self.at_top() || n == 0 || self.lines.is_empty() {
            return;
        }
        self.set_y_offset(self.y_offset.saturating_sub(n));
    }

    /// Default pager keys: pgdown/space/f, pgup/b, u/ctrl+u and d/ctrl+d (half page), up/k, down/j.
    /// Returns whether the key was one of them.
    pub fn handle_key(&mut self, key: &str) -> bool {
        match key {
            "pgdown" | " " | "f" => self.scroll_down(self.height),
            "pgup" | "b" => self.scroll_up(self.height),
            "u" | "ctrl+u" => self.scroll_up(self.height / 2),
            "d" | "ctrl+d" => self.scroll_down(self.height / 2),
            "down" | "j" => self.scroll_down(1),
            "up" | "k" => self.scroll_up(1),
            _ => return false,
        }
        true
    }

    /// The lines currently inside the window.
    pub fn visible_lines(&self) -> &[Line<'static>] {
        let top = self.y_offset.min(self.lines.len());
        let bottom = (self.y_offset + self.height).min(self.lines.len());
        &self.lines[top..bottom]
    }
}

impl Widget for &Viewport {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows = self.visible_lines().to_vec();
        Paragraph::new(rows).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    #[test]
    fn scrolls_and_clamps() {
        let mut vp = Viewport::new(20, 5);
        vp.set_content(numbered(12));
        assert!(vp.at_top());
        assert!(vp.handle_key("j"));
        assert_eq!(vp.y_offset(), 1);
        vp.handle_key("pgdown");
        assert_eq!(vp.y_offset(), 6);
        vp.handle_key("pgdown");
        assert_eq!(vp.y_offset(), 7, "clamped to max offset");
        assert!(vp.at_bottom());
        vp.handle_key("u");
        assert_eq!(vp.y_offset(), 5);
        assert!(!vp.handle_key("x"));
    }

    #[test]
    fn shrinking_content_snaps_to_bottom() {
        let mut vp = Viewport::new(20, 5);
        vp.set_content(numbered(30));
        vp.goto_bottom();
        vp.set_content(numbered(8));
        assert_eq!(vp.y_offset(), 3);
    }
}
