//! Palette and text styles (Go: internal/tui/styles.go). The Go theme is violet chrome on a dark
//! surface; here the screen is true black with white text and only semantic colours (success,
//! warning, error, info) so dense tables stay readable without decorative boxes.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub const BG: Color = Color::Rgb(0, 0, 0);
pub const TEXT: Color = Color::Rgb(255, 255, 255);
/// Labels and secondary text.
pub const SUBTEXT: Color = Color::Rgb(166, 173, 188);
/// Help lines, rules, inactive tabs.
pub const MUTED: Color = Color::Rgb(107, 114, 128);
pub const SUCCESS: Color = Color::Rgb(34, 197, 94);
pub const WARNING: Color = Color::Rgb(234, 179, 8);
pub const ERROR: Color = Color::Rgb(239, 68, 68);
pub const INFO: Color = Color::Rgb(59, 130, 246);
/// Background of the selected row.
pub const SELECTED_BG: Color = Color::Rgb(38, 38, 38);

pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BG)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Page titles and section headers.
pub fn title() -> Style {
    bold().fg(TEXT)
}

pub fn subtitle() -> Style {
    Style::default().fg(SUBTEXT).add_modifier(Modifier::ITALIC)
}

pub fn label() -> Style {
    Style::default().fg(SUBTEXT)
}

pub fn value() -> Style {
    Style::default().fg(TEXT)
}

pub fn error() -> Style {
    bold().fg(ERROR)
}

pub fn success() -> Style {
    Style::default().fg(SUCCESS)
}

pub fn warning() -> Style {
    Style::default().fg(WARNING)
}

pub fn help() -> Style {
    Style::default().fg(MUTED)
}

pub fn selected() -> Style {
    Style::default().bg(SELECTED_BG)
}

pub fn log_debug() -> Style {
    Style::default().fg(MUTED)
}

pub fn log_info() -> Style {
    Style::default().fg(INFO)
}

pub fn log_warn() -> Style {
    Style::default().fg(WARNING)
}

pub fn log_error() -> Style {
    Style::default().fg(ERROR)
}

/// `Line` helpers used by every tab to build viewport content.
pub fn styled(text: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(Span::styled(text.into(), style))
}

pub fn blank() -> Line<'static> {
    Line::default()
}

/// A title line followed by the blank line lipgloss' `MarginBottom(1)` produced.
pub fn push_title(out: &mut Vec<Line<'static>>, text: &str) {
    out.push(styled(text, title()));
    out.push(blank());
}

/// `strings.Repeat("─", n)` in the muted colour.
pub fn rule(n: usize) -> Line<'static> {
    styled("─".repeat(n), help())
}
