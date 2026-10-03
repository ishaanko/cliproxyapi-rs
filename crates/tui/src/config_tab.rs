//! Config tab (Go: internal/tui/config_tab.go): the parsed config as an editable field list.
//! Booleans toggle on Enter, ints and strings open an inline editor, everything is written back
//! through the per-field management endpoints.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use crate::i18n::t;
use crate::jsonutil::{fmt_f0, get_bool, get_bool_nested, get_float, get_string};
use crate::msg::{Ctx, Msg};
use crate::styles::{self, blank, push_title, styled};
use crate::text::pad_to;
use crate::widgets::{TextInput, Viewport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    Int,
    Str,
    ReadOnly,
}

#[derive(Debug, Clone)]
pub struct ConfigField {
    pub label: &'static str,
    /// Management API path, e.g. "debug" or "quota-exceeded/switch-project".
    pub api_path: &'static str,
    pub kind: FieldKind,
    /// Display value; booleans are "true" / "false".
    pub value: String,
}

pub struct ConfigTab {
    ctx: Ctx,
    pub viewport: Viewport,
    pub fields: Vec<ConfigField>,
    pub cursor: usize,
    pub editing: bool,
    pub text_input: TextInput,
    pub err: Option<String>,
    /// Styled status line (success or error) from the last write.
    message: Option<Line<'static>>,
    width: usize,
    ready: bool,
    /// Row of the selected field in the last render, used to keep it on screen.
    cursor_line: usize,
}

impl ConfigTab {
    pub fn new(ctx: Ctx) -> Self {
        let mut text_input = TextInput::new();
        text_input.char_limit = 256;
        ConfigTab {
            ctx,
            viewport: Viewport::default(),
            fields: Vec::new(),
            cursor: 0,
            editing: false,
            text_input,
            err: None,
            message: None,
            width: 0,
            ready: false,
            cursor_line: 0,
        }
    }

    pub fn init(&self) {
        self.fetch_config();
    }

    fn fetch_config(&self) {
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            Some(match client.get_config().await {
                Ok(config) => Msg::ConfigData { config, err: None },
                Err(e) => Msg::ConfigData { config: Value::Null, err: Some(e) },
            })
        });
    }

    fn refresh_view(&mut self) {
        let lines = self.render_content();
        self.viewport.set_content(lines);
    }

    /// Whether keystrokes belong to the inline editor (global `q` / `L` must not fire).
    pub fn captures_text(&self) -> bool {
        self.editing
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => self.refresh_view(),
            Msg::ConfigData { config, err } => {
                match err {
                    Some(e) => {
                        self.err = Some(e.clone());
                        self.fields.clear();
                    }
                    None => {
                        self.err = None;
                        self.fields = parse_config(config);
                    }
                }
                self.refresh_view();
            }
            Msg::ConfigUpdate { err, .. } => {
                self.message = Some(match err {
                    Some(e) => styled(format!("✗ {e}"), styles::error()),
                    None => styled(t("updated_ok"), styles::success()),
                });
                self.refresh_view();
                self.fetch_config();
            }
            Msg::Key(key) => {
                if self.editing {
                    self.handle_editing_key(key);
                } else {
                    self.handle_normal_key(key.as_str());
                }
            }
            Msg::Paste(text) if self.editing => {
                self.text_input.insert_text(text);
                self.refresh_view();
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: &str) {
        match key {
            "r" => {
                self.message = None;
                self.fetch_config();
            }
            "up" | "k" => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.refresh_view();
                    self.ensure_cursor_visible();
                }
            }
            "down" | "j" => {
                if self.cursor + 1 < self.fields.len() {
                    self.cursor += 1;
                    self.refresh_view();
                    self.ensure_cursor_visible();
                }
            }
            "enter" | " " => {
                if let Some(f) = self.fields.get(self.cursor) {
                    match f.kind {
                        FieldKind::ReadOnly => {}
                        FieldKind::Bool => self.toggle_bool(self.cursor),
                        FieldKind::Int | FieldKind::Str => {
                            self.editing = true;
                            let value = f.value.clone();
                            self.text_input.set_value(&value);
                            self.text_input.focus();
                            self.refresh_view();
                        }
                    }
                }
            }
            other => {
                self.viewport.handle_key(other);
            }
        }
    }

    fn handle_editing_key(&mut self, key: &crate::keys::Key) {
        match key.as_str() {
            "enter" => {
                self.editing = false;
                self.text_input.blur();
                self.submit_edit(self.cursor, self.text_input.value());
            }
            "esc" => {
                self.editing = false;
                self.text_input.blur();
                self.refresh_view();
            }
            _ => {
                self.text_input.handle_key(key);
                self.refresh_view();
            }
        }
    }

    fn toggle_bool(&self, idx: usize) {
        let Some(f) = self.fields.get(idx).cloned() else { return };
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            let new_value = f.value != "true";
            let result = client.put_bool_field(f.api_path, new_value).await;
            Some(Msg::ConfigUpdate {
                path: f.api_path.to_string(),
                value: Some(json!(new_value)),
                err: result.err(),
            })
        });
    }

    fn submit_edit(&self, idx: usize, new_value: String) {
        let Some(f) = self.fields.get(idx).cloned() else { return };
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            let path = f.api_path.to_string();
            match f.kind {
                FieldKind::Int => match new_value.parse::<i64>() {
                    Err(_) => Some(Msg::ConfigUpdate {
                        path,
                        value: None,
                        err: Some(format!("{}: {}", t("invalid_int"), new_value)),
                    }),
                    Ok(n) => {
                        let err = client.put_int_field(f.api_path, n).await.err();
                        Some(Msg::ConfigUpdate { path, value: Some(json!(n)), err })
                    }
                },
                FieldKind::Str => {
                    let err = client.put_string_field(f.api_path, &new_value).await.err();
                    Some(Msg::ConfigUpdate { path, value: Some(json!(new_value)), err })
                }
                _ => Some(Msg::ConfigUpdate { path, value: None, err: None }),
            }
        });
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = h;
        self.ready = true;
        self.refresh_view();
    }

    /// Scrolls so the selected field's row is inside the window.
    fn ensure_cursor_visible(&mut self) {
        let line = self.cursor_line;
        let off = self.viewport.y_offset();
        if line < off {
            self.viewport.set_y_offset(line);
        } else if line >= off + self.viewport.height {
            self.viewport.set_y_offset(line + 1 - self.viewport.height);
        }
    }

    fn render_content(&mut self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        push_title(&mut out, t("config_title"));
        if let Some(msg) = &self.message {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(msg.spans.iter().cloned());
            out.push(Line::from(spans));
        }
        out.push(styled(t("config_help1"), styles::help()));
        out.push(styled(t("config_help2"), styles::help()));
        out.push(blank());

        if let Some(e) = &self.err {
            out.push(styled(format!("  ⚠ Error: {e}"), styles::error()));
            return out;
        }
        if self.fields.is_empty() {
            out.push(styled(t("no_config"), styles::subtitle()));
            return out;
        }

        let mut current_section = "";
        let mut cursor_line = 0;
        for (i, f) in self.fields.iter().enumerate() {
            let section = field_section(f.api_path);
            if section != current_section {
                current_section = section;
                out.push(blank());
                out.push(styled(format!("  ── {section} "), styles::title()));
            }
            let selected = i == self.cursor;
            if selected {
                cursor_line = out.len();
            }
            let prefix = if selected { "▸ " } else { "  " };
            let mut label_style = Style::default().fg(styles::SUBTEXT);
            if selected {
                label_style = label_style.fg(styles::TEXT).add_modifier(ratatui::style::Modifier::BOLD);
            }
            let mut spans = vec![
                Span::raw(prefix),
                Span::styled(pad_to(f.label, 32), label_style),
                Span::raw("  "),
            ];
            if self.editing && selected {
                spans.extend(self.text_input.view());
            } else {
                spans.push(match f.kind {
                    FieldKind::Bool if f.value == "true" => Span::styled("● ON", styles::success()),
                    FieldKind::Bool => Span::styled("○ OFF", styles::help()),
                    FieldKind::ReadOnly => Span::styled(f.value.clone(), styles::label()),
                    _ => Span::styled(f.value.clone(), styles::value()),
                });
            }
            let mut line = Line::from(spans);
            if selected && !self.editing {
                line = line.style(styles::selected());
            }
            out.push(line);
        }
        self.cursor_line = cursor_line;
        out
    }
}

/// `parseConfig`: the fixed field list read out of the config JSON.
pub fn parse_config(cfg: &Value) -> Vec<ConfigField> {
    let f = |label, api_path, kind, value: String| ConfigField { label, api_path, kind, value };
    let int = |key: &str| fmt_f0(get_float(cfg, key));
    let boolean = |key: &str| get_bool(cfg, key).to_string();
    let strategy = cfg.get("routing").filter(|r| r.is_object()).map(|r| get_string(r, "strategy")).unwrap_or_default();
    vec![
        // Server
        f("Port", "port", FieldKind::ReadOnly, int("port")),
        f("Host", "host", FieldKind::ReadOnly, get_string(cfg, "host")),
        f("Debug", "debug", FieldKind::Bool, boolean("debug")),
        f("Proxy URL", "proxy-url", FieldKind::Str, get_string(cfg, "proxy-url")),
        f("Request Retry", "request-retry", FieldKind::Int, int("request-retry")),
        f("Max Retry Interval (s)", "max-retry-interval", FieldKind::Int, int("max-retry-interval")),
        f("Force Model Prefix", "force-model-prefix", FieldKind::Str, get_string(cfg, "force-model-prefix")),
        // Logging
        f("Logging to File", "logging-to-file", FieldKind::Bool, boolean("logging-to-file")),
        f("Logs Max Total Size (MB)", "logs-max-total-size-mb", FieldKind::Int, int("logs-max-total-size-mb")),
        f("Error Logs Max Files", "error-logs-max-files", FieldKind::Int, int("error-logs-max-files")),
        f("Usage Stats Enabled", "usage-statistics-enabled", FieldKind::Bool, boolean("usage-statistics-enabled")),
        f("Request Log", "request-log", FieldKind::Bool, boolean("request-log")),
        // Quota exceeded
        f(
            "Switch Project on Quota",
            "quota-exceeded/switch-project",
            FieldKind::Bool,
            get_bool_nested(cfg, &["quota-exceeded", "switch-project"]).to_string(),
        ),
        f(
            "Switch Preview Model",
            "quota-exceeded/switch-preview-model",
            FieldKind::Bool,
            get_bool_nested(cfg, &["quota-exceeded", "switch-preview-model"]).to_string(),
        ),
        // Routing
        f("Routing Strategy", "routing/strategy", FieldKind::Str, strategy),
        // WebSocket
        f("WebSocket Auth", "ws-auth", FieldKind::Bool, boolean("ws-auth")),
    ]
}

/// `fieldSection`: heading each field is grouped under.
pub fn field_section(api_path: &str) -> &'static str {
    if api_path.starts_with("quota-exceeded/") {
        return t("section_quota");
    }
    if api_path.starts_with("routing/") {
        return t("section_routing");
    }
    match api_path {
        "port" | "host" | "debug" | "proxy-url" | "request-retry" | "max-retry-interval" | "force-model-prefix" => {
            t("section_server")
        }
        "logging-to-file" | "logs-max-total-size-mb" | "error-logs-max-files" | "usage-statistics-enabled" | "request-log" => {
            t("section_logging")
        }
        "ws-auth" => t("section_websocket"),
        _ => t("section_other"),
    }
}
