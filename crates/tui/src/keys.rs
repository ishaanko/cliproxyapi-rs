//! Key events reduced to bubbletea's `KeyMsg.String()` names ("ctrl+c", "shift+tab", "enter", " ",
//! "L", ...) so the tab handlers can switch on the same strings as the Go source.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A key press: its bubbletea name plus the typed character, if it is plain text input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    pub text: Option<char>,
}

impl Key {
    /// A key built from its bubbletea name; single printable characters count as typed text.
    pub fn named(name: &str) -> Key {
        let mut chars = name.chars();
        let text = match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Some(c),
            _ => None,
        };
        Key { name: name.to_string(), text }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// Maps a crossterm event; releases and unmapped keys yield `None`.
    pub fn from_event(ev: &KeyEvent) -> Option<Key> {
        if ev.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
        let mut prefix = String::new();
        if alt {
            prefix.push_str("alt+");
        }
        let (base, text): (String, Option<char>) = match ev.code {
            KeyCode::Char(c) => {
                if ctrl {
                    (format!("ctrl+{}", c.to_ascii_lowercase()), None)
                } else {
                    (c.to_string(), (!alt).then_some(c))
                }
            }
            KeyCode::Enter => ("enter".into(), None),
            KeyCode::Tab => ("tab".into(), None),
            KeyCode::BackTab => ("shift+tab".into(), None),
            KeyCode::Esc => ("esc".into(), None),
            KeyCode::Backspace => ("backspace".into(), None),
            KeyCode::Delete => ("delete".into(), None),
            KeyCode::Insert => ("insert".into(), None),
            KeyCode::Home => (with_mods("home", ctrl, shift), None),
            KeyCode::End => (with_mods("end", ctrl, shift), None),
            KeyCode::PageUp => ("pgup".into(), None),
            KeyCode::PageDown => ("pgdown".into(), None),
            KeyCode::Up => (with_mods("up", ctrl, shift), None),
            KeyCode::Down => (with_mods("down", ctrl, shift), None),
            KeyCode::Left => (with_mods("left", ctrl, shift), None),
            KeyCode::Right => (with_mods("right", ctrl, shift), None),
            KeyCode::F(n) => (format!("f{n}"), None),
            _ => return None,
        };
        Some(Key { name: format!("{prefix}{base}"), text })
    }
}

fn with_mods(name: &str, ctrl: bool, shift: bool) -> String {
    let mut out = String::new();
    if ctrl {
        out.push_str("ctrl+");
    }
    if shift {
        out.push_str("shift+");
    }
    out.push_str(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn names_follow_bubbletea() {
        let name = |code, mods| Key::from_event(&ev(code, mods)).map(|k| k.name);
        assert_eq!(name(KeyCode::Char('c'), KeyModifiers::CONTROL).as_deref(), Some("ctrl+c"));
        assert_eq!(name(KeyCode::Char('L'), KeyModifiers::SHIFT).as_deref(), Some("L"));
        assert_eq!(name(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(), Some("shift+tab"));
        assert_eq!(name(KeyCode::Char(' '), KeyModifiers::NONE).as_deref(), Some(" "));
        assert_eq!(name(KeyCode::Left, KeyModifiers::CONTROL).as_deref(), Some("ctrl+left"));
        assert_eq!(name(KeyCode::Char('b'), KeyModifiers::ALT).as_deref(), Some("alt+b"));
    }
}
