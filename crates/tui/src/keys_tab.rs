//! API Keys tab (Go: internal/tui/keys_tab.go): the access API keys (add / edit / delete / copy)
//! plus read-only listings of every provider key section.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::clipboard;
use crate::i18n::{t, tf};
use crate::jsonutil::get_string;
use crate::keys::Key;
use crate::msg::{Ctx, KeysData, Msg};
use crate::styles::{self, blank, push_title, styled};
use crate::text::width;
use crate::widgets::{TextInput, Viewport};

pub struct KeysTab {
    ctx: Ctx,
    pub viewport: Viewport,
    pub keys: Vec<String>,
    gemini: Vec<Value>,
    interactions: Vec<Value>,
    claude: Vec<Value>,
    codex: Vec<Value>,
    xai: Vec<Value>,
    vertex: Vec<Value>,
    openai: Vec<Value>,
    pub err: Option<String>,
    width: usize,
    pub cursor: usize,
    /// Index awaiting a delete confirmation.
    pub confirm: Option<usize>,
    status: Option<Line<'static>>,
    pub editing: bool,
    pub adding: bool,
    edit_idx: usize,
    pub edit_input: TextInput,
    cursor_line: usize,
}

impl KeysTab {
    pub fn new(ctx: Ctx) -> Self {
        let mut edit_input = TextInput::new();
        edit_input.char_limit = 512;
        edit_input.prompt = "  Key: ".into();
        KeysTab {
            ctx,
            viewport: Viewport::default(),
            keys: Vec::new(),
            gemini: Vec::new(),
            interactions: Vec::new(),
            claude: Vec::new(),
            codex: Vec::new(),
            xai: Vec::new(),
            vertex: Vec::new(),
            openai: Vec::new(),
            err: None,
            width: 0,
            cursor: 0,
            confirm: None,
            status: None,
            editing: false,
            adding: false,
            edit_idx: 0,
            edit_input,
            cursor_line: 0,
        }
    }

    pub fn init(&self) {
        self.fetch_keys();
    }

    /// `fetchKeys`: access keys decide success; provider lists are best effort.
    fn fetch_keys(&self) {
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            let mut data = KeysData::default();
            match client.get_api_keys().await {
                Err(e) => data.err = Some(e),
                Ok(keys) => {
                    data.api_keys = keys;
                    data.gemini = client.get_gemini_keys().await.unwrap_or_default();
                    data.interactions = client.get_interactions_keys().await.unwrap_or_default();
                    data.claude = client.get_claude_keys().await.unwrap_or_default();
                    data.codex = client.get_codex_keys().await.unwrap_or_default();
                    data.xai = client.get_xai_keys().await.unwrap_or_default();
                    data.vertex = client.get_vertex_keys().await.unwrap_or_default();
                    data.openai = client.get_openai_compat().await.unwrap_or_default();
                }
            }
            Some(Msg::KeysData(Box::new(data)))
        });
    }

    fn refresh_view(&mut self) {
        let lines = self.render_content();
        self.viewport.set_content(lines);
    }

    fn follow_cursor(&mut self) {
        let line = self.cursor_line;
        let off = self.viewport.y_offset();
        if line < off {
            self.viewport.set_y_offset(line);
        } else if line >= off + self.viewport.height {
            self.viewport.set_y_offset(line + 1 - self.viewport.height);
        }
    }

    pub fn captures_text(&self) -> bool {
        self.editing || self.adding
    }

    fn stop_editing(&mut self) {
        self.editing = false;
        self.adding = false;
        self.edit_input.blur();
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => self.refresh_view(),
            Msg::KeysData(data) => {
                match &data.err {
                    Some(e) => self.err = Some(e.clone()),
                    None => {
                        self.err = None;
                        self.keys = data.api_keys.clone();
                        self.gemini = data.gemini.clone();
                        self.interactions = data.interactions.clone();
                        self.claude = data.claude.clone();
                        self.codex = data.codex.clone();
                        self.xai = data.xai.clone();
                        self.vertex = data.vertex.clone();
                        self.openai = data.openai.clone();
                        if self.cursor >= self.keys.len() {
                            self.cursor = self.keys.len().saturating_sub(1);
                        }
                    }
                }
                self.refresh_view();
            }
            Msg::KeyAction { action, err } => {
                self.status = Some(match err {
                    Some(e) => styled(format!("✗ {e}"), styles::error()),
                    None => styled(format!("✓ {action}"), styles::success()),
                });
                self.confirm = None;
                self.refresh_view();
                self.fetch_keys();
            }
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) if self.editing || self.adding => {
                self.edit_input.insert_text(text);
                self.refresh_view();
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: &Key) {
        if self.editing || self.adding {
            self.handle_edit_key(key);
            return;
        }
        if self.confirm.is_some() {
            match key.as_str() {
                "y" | "Y" => {
                    if let Some(idx) = self.confirm.take() {
                        let client = self.ctx.client.clone();
                        self.ctx.spawn(async move {
                            Some(match client.delete_api_key(idx).await {
                                Err(e) => Msg::KeyAction { action: String::new(), err: Some(e) },
                                Ok(()) => Msg::KeyAction { action: t("key_deleted").into(), err: None },
                            })
                        });
                    }
                }
                "n" | "N" | "esc" => {
                    self.confirm = None;
                    self.refresh_view();
                }
                _ => {}
            }
            return;
        }

        let n = self.keys.len();
        match key.as_str() {
            "j" | "down" => {
                if n > 0 {
                    self.cursor = (self.cursor + 1) % n;
                    self.refresh_view();
                    self.follow_cursor();
                }
            }
            "k" | "up" => {
                if n > 0 {
                    self.cursor = (self.cursor + n - 1) % n;
                    self.refresh_view();
                    self.follow_cursor();
                }
            }
            "a" => {
                self.adding = true;
                self.editing = false;
                self.edit_input.set_value("");
                self.edit_input.prompt = t("new_key_prompt").into();
                self.edit_input.focus();
                self.refresh_view();
                self.follow_cursor();
            }
            "e" => {
                if let Some(current) = self.keys.get(self.cursor).cloned() {
                    self.editing = true;
                    self.adding = false;
                    self.edit_idx = self.cursor;
                    self.edit_input.set_value(&current);
                    self.edit_input.prompt = t("edit_key_prompt").into();
                    self.edit_input.focus();
                    self.refresh_view();
                }
            }
            "d" => {
                if self.cursor < n {
                    self.confirm = Some(self.cursor);
                    self.refresh_view();
                }
            }
            "c" => {
                if let Some(key) = self.keys.get(self.cursor) {
                    self.status = Some(match clipboard::write_all(key) {
                        Err(e) => styled(format!("{}: {e}", t("copy_failed")), styles::error()),
                        Ok(()) => styled(t("copied"), styles::success()),
                    });
                    self.refresh_view();
                }
            }
            "r" => {
                self.status = None;
                self.fetch_keys();
            }
            other => {
                self.viewport.handle_key(other);
            }
        }
    }

    fn handle_edit_key(&mut self, key: &Key) {
        match key.as_str() {
            "enter" => {
                let value = self.edit_input.value().trim().to_string();
                if value.is_empty() {
                    self.stop_editing();
                    self.refresh_view();
                    return;
                }
                let is_adding = self.adding;
                let edit_idx = self.edit_idx;
                self.stop_editing();
                let client = self.ctx.client.clone();
                self.ctx.spawn(async move {
                    Some(if is_adding {
                        match client.add_api_key(&value).await {
                            Err(e) => Msg::KeyAction { action: String::new(), err: Some(e) },
                            Ok(()) => Msg::KeyAction { action: t("key_added").into(), err: None },
                        }
                    } else {
                        match client.edit_api_key(edit_idx, &value).await {
                            Err(e) => Msg::KeyAction { action: String::new(), err: Some(e) },
                            Ok(()) => Msg::KeyAction { action: t("key_updated").into(), err: None },
                        }
                    })
                });
            }
            "esc" => {
                self.stop_editing();
                self.refresh_view();
            }
            _ => {
                self.edit_input.handle_key(key);
                self.refresh_view();
            }
        }
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = h;
        self.edit_input.width = w.saturating_sub(16);
        self.refresh_view();
    }

    fn render_content(&mut self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        push_title(&mut out, t("keys_title"));
        out.push(styled(t("keys_help"), styles::help()));
        out.push(styles::rule(self.width));

        if let Some(e) = &self.err {
            out.push(styled(format!("{}{e}", t("error_prefix")), styles::error()));
            return out;
        }

        render_section(&mut out, t("access_keys"), self.keys.len());
        if self.keys.is_empty() {
            out.push(styled(t("no_keys"), styles::subtitle()));
        }
        for (i, key) in self.keys.iter().enumerate() {
            let selected = i == self.cursor;
            if selected {
                self.cursor_line = out.len();
            }
            let cursor = if selected { "▸ " } else { "  " };
            let style = if selected { styles::bold() } else { Style::default() };
            out.push(styled(format!("{cursor}{}. {}", i + 1, mask_key(key)), style));

            if self.confirm == Some(i) {
                out.push(styled(format!("    {}", tf("confirm_delete_key", &[&mask_key(key)])), styles::warning()));
            }
            if self.editing && self.edit_idx == i {
                out.push(Line::from(self.edit_input.view()));
                out.push(styled(t("enter_save_esc"), styles::help()));
            }
        }
        if self.adding {
            out.push(blank());
            self.cursor_line = out.len();
            out.push(Line::from(self.edit_input.view()));
            out.push(styled(t("enter_add"), styles::help()));
        }
        out.push(blank());

        render_provider_keys(&mut out, "Gemini API Keys", &self.gemini);
        render_provider_keys(&mut out, "Interactions API Keys", &self.interactions);
        render_provider_keys(&mut out, "Claude API Keys", &self.claude);
        render_provider_keys(&mut out, "Codex API Keys", &self.codex);
        render_provider_keys(&mut out, "xAI API Keys", &self.xai);
        render_provider_keys(&mut out, "Vertex API Keys", &self.vertex);

        if !self.openai.is_empty() {
            render_section(&mut out, "OpenAI Compatibility", self.openai.len());
            for (i, entry) in self.openai.iter().enumerate() {
                let mut info = get_string(entry, "name");
                let prefix = get_string(entry, "prefix");
                let base_url = get_string(entry, "base-url");
                if !prefix.is_empty() {
                    info.push_str(&format!(" (prefix: {prefix})"));
                }
                if !base_url.is_empty() {
                    info.push_str(&format!(" → {base_url}"));
                }
                out.push(Line::from(format!("  {}. {info}", i + 1)));
            }
            out.push(blank());
        }

        if let Some(status) = &self.status {
            out.push(status.clone());
        }
        out
    }
}

/// `renderSection`: bold header with an underline as wide as the header text.
fn render_section(out: &mut Vec<Line<'static>>, title: &str, count: usize) {
    let header = format!("  {title} ({count})");
    out.push(styled(header.clone(), styles::title()));
    out.push(styles::rule(width(&header)));
}

fn render_provider_keys(out: &mut Vec<Line<'static>>, title: &str, keys: &[Value]) {
    if keys.is_empty() {
        return;
    }
    render_section(out, title, keys.len());
    for (i, key) in keys.iter().enumerate() {
        let mut info = mask_key(&get_string(key, "api-key"));
        let prefix = get_string(key, "prefix");
        let base_url = get_string(key, "base-url");
        if !prefix.is_empty() {
            info.push_str(&format!(" (prefix: {prefix})"));
        }
        if !base_url.is_empty() {
            info.push_str(&format!(" → {base_url}"));
        }
        out.push(Line::from(vec![Span::raw(format!("  {}. {info}", i + 1))]));
    }
    out.push(blank());
}

/// `maskKey`: all stars when 8 chars or fewer, else first 4 and last 4 around stars.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(chars.len() - 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_like_go() {
        assert_eq!(mask_key("short"), "*****");
        assert_eq!(mask_key("sk-test-key-0001-abcdefghij"), "sk-t*******************ghij");
    }
}
