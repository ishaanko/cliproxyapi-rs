//! Dashboard tab (Go: internal/tui/dashboard.go): connection line, key and auth file counts and the
//! current config overview.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::i18n::t;
use crate::jsonutil::{fmt_f0, get_bool, get_float, get_string};
use crate::msg::{Ctx, Msg};
use crate::styles::{self, blank, push_title, styled};
use crate::text::pad_to;
use crate::widgets::Viewport;

pub struct Dashboard {
    ctx: Ctx,
    pub viewport: Viewport,
    content: Vec<Line<'static>>,
    pub err: Option<String>,
    width: usize,
    ready: bool,
    // Cached so a locale switch can re-render before fresh data arrives.
    last_config: Value,
    last_auth_files: Vec<Value>,
    last_api_keys: Vec<String>,
}

impl Dashboard {
    pub fn new(ctx: Ctx) -> Self {
        Dashboard {
            ctx,
            viewport: Viewport::default(),
            content: Vec::new(),
            err: None,
            width: 0,
            ready: false,
            last_config: Value::Null,
            last_auth_files: Vec::new(),
            last_api_keys: Vec::new(),
        }
    }

    /// `Init`: fetch config, auth files and keys.
    pub fn init(&self) {
        self.fetch();
    }

    fn fetch(&self) {
        let client = self.ctx.client.clone();
        self.ctx.spawn(async move {
            let config = client.get_config().await;
            let auth_files = client.get_auth_files().await;
            let api_keys = client.get_api_keys().await;
            let err = [
                config.as_ref().err(),
                auth_files.as_ref().err(),
                api_keys.as_ref().err(),
            ]
            .into_iter()
            .flatten()
            .next()
            .cloned();
            Some(Msg::DashboardData {
                config: config.unwrap_or(Value::Null),
                auth_files: auth_files.unwrap_or_default(),
                api_keys: api_keys.unwrap_or_default(),
                err,
            })
        });
    }

    fn rerender(&mut self) {
        self.content = self.render_dashboard(&self.last_config, &self.last_auth_files, &self.last_api_keys);
        self.viewport.set_content(self.content.clone());
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => {
                self.rerender();
                self.fetch();
            }
            Msg::DashboardData { config, auth_files, api_keys, err } => {
                match err {
                    Some(e) => {
                        self.err = Some(e.clone());
                        self.content = vec![styled(format!("⚠ Error: {e}"), styles::error())];
                    }
                    None => {
                        self.err = None;
                        self.last_config = config.clone();
                        self.last_auth_files = auth_files.clone();
                        self.last_api_keys = api_keys.clone();
                        self.content = self.render_dashboard(config, auth_files, api_keys);
                    }
                }
                self.viewport.set_content(self.content.clone());
            }
            Msg::Key(key) => {
                if key.as_str() == "r" {
                    self.fetch();
                } else {
                    self.viewport.handle_key(key.as_str());
                }
            }
            _ => {}
        }
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = h;
        if !self.ready {
            self.viewport.set_content(self.content.clone());
            self.ready = true;
        } else if self.err.is_none() && self.last_config != Value::Null {
            self.rerender();
        }
    }

    fn render_dashboard(&self, cfg: &Value, auth_files: &[Value], api_keys: &[String]) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        push_title(&mut out, t("dashboard_title"));
        out.push(styled(t("dashboard_help"), styles::help()));
        out.push(blank());

        out.push(Line::from(vec![
            Span::styled(t("connected"), styles::bold().fg(styles::SUCCESS)),
            Span::raw(format!("  {}", self.ctx.client.base_url())),
        ]));
        out.push(blank());

        // Two compact stat columns instead of bordered cards.
        let col = if self.width > 0 { ((self.width.saturating_sub(2)) / 2).max(18) + 3 } else { 28 };
        let active = auth_files.iter().filter(|f| !get_bool(f, "disabled")).count();
        let big = |text: String, color: Color| Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD));
        out.push(Line::from(vec![
            big(pad_to(&format!("🔑 {}", api_keys.len()), col), Color::Rgb(135, 175, 255)),
            big(format!("📄 {}", auth_files.len()), Color::Rgb(95, 215, 0)),
        ]));
        out.push(Line::from(vec![
            Span::styled(pad_to(t("mgmt_keys"), col), styles::help()),
            Span::styled(
                format!("{} ({} {})", t("auth_files_label"), active, t("active_suffix")),
                styles::help(),
            ),
        ]));
        out.push(blank());

        out.push(styled(t("current_config"), styles::title()));
        out.push(styles::rule(self.width.min(60)));

        if !cfg.is_null() {
            let debug = get_bool(cfg, "debug");
            let retry = get_float(cfg, "request-retry");
            let proxy_url = get_string(cfg, "proxy-url");
            let logging_to_file = get_bool(cfg, "logging-to-file");
            let usage_enabled = match cfg.get("usage-statistics-enabled") {
                Some(Value::Bool(b)) => *b,
                _ => true,
            };
            let mut items = vec![
                (t("debug_mode"), bool_emoji(debug)),
                (t("usage_stats"), bool_emoji(usage_enabled)),
                (t("log_to_file"), bool_emoji(logging_to_file)),
                (t("retry_count"), fmt_f0(retry)),
            ];
            if !proxy_url.is_empty() {
                items.push((t("proxy_url"), proxy_url));
            }
            for (label, value) in items {
                out.push(kv_line(label, &value));
            }
            let strategy = cfg
                .get("routing")
                .map(|r| get_string(r, "strategy"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "round-robin".into());
            out.push(kv_line(t("routing_strategy"), &strategy));
        }
        out.push(blank());
        out
    }
}

/// `  <label>:` padded to 24 cells, then the value.
pub fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(pad_to(&format!("{label}:"), 24), styles::label()),
        Span::raw(" "),
        Span::styled(value.to_string(), styles::value()),
    ])
}

/// `boolEmoji`: "Yes ✓" / "No".
pub fn bool_emoji(b: bool) -> String {
    if b { t("bool_yes") } else { t("bool_no") }.to_string()
}
