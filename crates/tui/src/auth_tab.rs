//! Auth Files tab (Go: internal/tui/auth_tab.go): credential files with expand, enable/disable,
//! delete (with confirmation), refresh and inline editing of prefix / proxy_url / priority.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use crate::i18n::{t, tf};
use crate::jsonutil::{get_any_string, get_bool, get_string};
use crate::keys::Key;
use crate::msg::{Ctx, Msg};
use crate::styles::{self, push_title, styled};
use crate::text::{pad_to, truncate_bytes};
use crate::widgets::{TextInput, Viewport};

/// Fields editable with keys 1-3: (label, API field key).
pub const EDITABLE_FIELDS: [(&str, &str); 3] = [("Prefix", "prefix"), ("Proxy URL", "proxy_url"), ("Priority", "priority")];

pub struct AuthTab {
    ctx: Ctx,
    pub viewport: Viewport,
    pub files: Vec<Value>,
    pub err: Option<String>,
    width: usize,
    pub cursor: usize,
    /// Index of the expanded detail row.
    pub expanded: Option<usize>,
    /// Index awaiting a delete confirmation.
    pub confirm: Option<usize>,
    status: Option<Line<'static>>,
    pub editing: bool,
    edit_field: usize,
    pub edit_input: TextInput,
    edit_file_name: String,
    cursor_line: usize,
}

impl AuthTab {
    pub fn new(ctx: Ctx) -> Self {
        let mut edit_input = TextInput::new();
        edit_input.char_limit = 256;
        AuthTab {
            ctx,
            viewport: Viewport::default(),
            files: Vec::new(),
            err: None,
            width: 0,
            cursor: 0,
            expanded: None,
            confirm: None,
            status: None,
            editing: false,
            edit_field: 0,
            edit_input,
            edit_file_name: String::new(),
            cursor_line: 0,
        }
    }

    pub fn init(&self) {
        self.fetch_files();
    }

    fn fetch_files(&self) {
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            Some(match client.get_auth_files().await {
                Ok(files) => Msg::AuthFiles { files, err: None },
                Err(e) => Msg::AuthFiles { files: Vec::new(), err: Some(e) },
            })
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
        self.editing
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => self.refresh_view(),
            Msg::AuthFiles { files, err } => {
                match err {
                    Some(e) => self.err = Some(e.clone()),
                    None => {
                        self.err = None;
                        self.files = files.clone();
                        if self.cursor >= self.files.len() {
                            self.cursor = self.files.len().saturating_sub(1);
                        }
                        self.status = None;
                    }
                }
                self.refresh_view();
            }
            Msg::AuthAction { action, err } => {
                self.status = Some(match err {
                    Some(e) => styled(format!("✗ {e}"), styles::error()),
                    None => styled(format!("✓ {action}"), styles::success()),
                });
                self.confirm = None;
                self.refresh_view();
                self.fetch_files();
            }
            Msg::Key(key) => {
                if self.editing {
                    self.handle_edit_input(key);
                } else if self.confirm.is_some() {
                    self.handle_confirm_input(key.as_str());
                } else {
                    self.handle_normal_input(key.as_str());
                }
            }
            Msg::Paste(text) if self.editing => {
                self.edit_input.insert_text(text);
                self.refresh_view();
            }
            _ => {}
        }
    }

    /// `startEdit`: inline editor for one of the editable fields on the selected file.
    fn start_edit(&mut self, field_idx: usize) {
        let Some(f) = self.files.get(self.cursor) else { return };
        self.edit_file_name = get_string(f, "name");
        self.edit_field = field_idx;
        self.editing = true;
        let (label, key) = EDITABLE_FIELDS[field_idx];
        let current = get_any_string(f, key);
        self.edit_input.set_value(&current);
        self.edit_input.focus();
        self.edit_input.prompt = format!("  {label}: ");
        self.refresh_view();
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = h;
        self.edit_input.width = w.saturating_sub(20);
        self.refresh_view();
    }

    fn handle_edit_input(&mut self, key: &Key) {
        match key.as_str() {
            "enter" => {
                let value = self.edit_input.value();
                let field_key = EDITABLE_FIELDS[self.edit_field].1;
                let file_name = self.edit_file_name.clone();
                self.editing = false;
                self.edit_input.blur();
                let field_value = if field_key == "priority" {
                    match value.parse::<i64>() {
                        Ok(p) => json!(p),
                        Err(_) => {
                            self.ctx.send(Msg::AuthAction {
                                action: String::new(),
                                err: Some(format!("{}: {}", t("invalid_int"), value)),
                            });
                            return;
                        }
                    }
                } else {
                    json!(value)
                };
                let client = self.ctx.client.clone();
                self.ctx.spawn(async move {
                    let fields = json!({ field_key: field_value });
                    Some(match client.patch_auth_file_fields(&file_name, fields).await {
                        Err(e) => Msg::AuthAction { action: String::new(), err: Some(e) },
                        Ok(()) => Msg::AuthAction { action: tf("updated_field", &[&field_key, &file_name]), err: None },
                    })
                });
            }
            "esc" => {
                self.editing = false;
                self.edit_input.blur();
                self.refresh_view();
            }
            _ => {
                self.edit_input.handle_key(key);
                self.refresh_view();
            }
        }
    }

    fn handle_confirm_input(&mut self, key: &str) {
        match key {
            "y" | "Y" => {
                let idx = self.confirm.take();
                match idx.and_then(|i| self.files.get(i)) {
                    Some(f) => {
                        let name = get_string(f, "name");
                        let client = self.ctx.client.clone();
                        self.ctx.spawn(async move {
                            Some(match client.delete_auth_file(&name).await {
                                Err(e) => Msg::AuthAction { action: String::new(), err: Some(e) },
                                Ok(()) => Msg::AuthAction { action: tf("deleted", &[&name]), err: None },
                            })
                        });
                    }
                    None => self.refresh_view(),
                }
            }
            "n" | "N" | "esc" => {
                self.confirm = None;
                self.refresh_view();
            }
            _ => {}
        }
    }

    fn handle_normal_input(&mut self, key: &str) {
        let n = self.files.len();
        match key {
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
            "enter" | " " => {
                self.expanded = if self.expanded == Some(self.cursor) { None } else { Some(self.cursor) };
                self.refresh_view();
            }
            "d" | "D" => {
                if self.cursor < n {
                    self.confirm = Some(self.cursor);
                    self.refresh_view();
                }
            }
            "e" | "E" => {
                if let Some(f) = self.files.get(self.cursor) {
                    let name = get_string(f, "name");
                    let new_disabled = !get_bool(f, "disabled");
                    let client = self.ctx.client.clone();
                    self.ctx.spawn(async move {
                        Some(match client.toggle_auth_file(&name, new_disabled).await {
                            Err(e) => Msg::AuthAction { action: String::new(), err: Some(e) },
                            Ok(()) => {
                                let action = if new_disabled { t("disabled") } else { t("enabled") };
                                Msg::AuthAction { action: format!("{action} {name}"), err: None }
                            }
                        })
                    });
                }
            }
            "1" => self.start_edit(0),
            "2" => self.start_edit(1),
            "3" => self.start_edit(2),
            "r" => {
                self.status = None;
                self.fetch_files();
            }
            "R" => {
                if let Some(f) = self.files.get(self.cursor) {
                    let name = get_string(f, "name");
                    let client = self.ctx.client.clone();
                    self.ctx.spawn(async move {
                        Some(match client.refresh_auth_file(&name).await {
                            Err(e) => Msg::AuthAction { action: String::new(), err: Some(e) },
                            Ok(()) => Msg::AuthAction { action: tf("refreshed_auth", &[&name]), err: None },
                        })
                    });
                }
            }
            other => {
                self.viewport.handle_key(other);
            }
        }
    }

    fn render_content(&mut self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        push_title(&mut out, t("auth_title"));
        out.push(styled(t("auth_help1"), styles::help()));
        out.push(styled(t("auth_help2"), styles::help()));
        out.push(styles::rule(self.width));

        if let Some(e) = &self.err {
            out.push(styled(format!("⚠ Error: {e}"), styles::error()));
            return out;
        }
        if self.files.is_empty() {
            out.push(styled(t("no_auth_files"), styles::subtitle()));
            return out;
        }

        for (i, f) in self.files.iter().enumerate() {
            let name = get_string(f, "name");
            let channel = get_string(f, "channel");
            let email = get_string(f, "email");
            let disabled = get_bool(f, "disabled");

            let (icon, status_text) = if disabled {
                (Span::styled("○", styles::help()), t("status_disabled"))
            } else {
                (Span::styled("●", styles::success()), t("status_active"))
            };
            let selected = i == self.cursor;
            if selected {
                self.cursor_line = out.len();
            }
            let cursor = if selected { "▸ " } else { "  " };
            let row_style = if selected { styles::bold() } else { Style::default() };
            let line = Line::from(vec![
                Span::raw(cursor),
                icon,
                Span::raw(format!(
                    " {} {} {} {}",
                    pad_to(&truncate_bytes(&name, 24), 24),
                    pad_to(&channel, 12),
                    pad_to(&truncate_bytes(&email, 28), 28),
                    status_text
                )),
            ])
            .style(row_style);
            out.push(line);

            if self.confirm == Some(i) {
                out.push(styled(format!("    {}", tf("confirm_delete", &[&name])), styles::warning()));
            }
            if self.editing && selected {
                out.push(Line::from(self.edit_input.view()));
                out.push(styled(
                    format!("    {} • {}", t("enter_save"), t("esc_cancel")),
                    styles::help(),
                ));
            }
            if self.expanded == Some(i) {
                render_detail(&mut out, f);
            }
        }

        if let Some(status) = &self.status {
            out.push(Line::default());
            out.push(status.clone());
        }
        out
    }
}

/// `renderDetail`: key/value rows for the expanded file, behind a left gutter.
fn render_detail(out: &mut Vec<Line<'static>>, f: &Value) {
    let label_style = Style::default().fg(Color::Rgb(135, 175, 255)).add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(Color::Rgb(255, 255, 255));
    let fields: [(&str, &str, bool); 14] = [
        ("Name", "name", false),
        ("Channel", "channel", false),
        ("Email", "email", false),
        ("Status", "status", false),
        ("Status Msg", "status_message", false),
        ("File Name", "file_name", false),
        ("Auth Type", "auth_type", false),
        ("Prefix", "prefix", true),
        ("Proxy URL", "proxy_url", true),
        ("Priority", "priority", true),
        ("Project ID", "project_id", false),
        ("Disabled", "disabled", false),
        ("Created", "created_at", false),
        ("Updated", "updated_at", false),
    ];
    for (label, key, editable) in fields {
        let mut val = get_any_string(f, key);
        if val.is_empty() || val == "<nil>" {
            if editable {
                val = t("not_set").to_string();
            } else {
                continue;
            }
        }
        let mut spans = vec![
            Span::styled("    │ ", styles::help()),
            Span::styled(format!("{label:<12}:"), label_style),
            Span::raw(" "),
            Span::styled(val, value_style),
        ];
        if editable {
            spans.push(Span::styled(" ✎", Style::default().fg(Color::Rgb(255, 175, 0))));
        }
        out.push(Line::from(spans));
    }
}
