//! Root model (Go: internal/tui/app.go): password gate, tab bar, per-tab routing and the status bar.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::auth_tab::AuthTab;
use crate::client::Client;
use crate::config_tab::ConfigTab;
use crate::dashboard::Dashboard;
use crate::i18n::{t, tab_names, tf, toggle_locale};
use crate::keys::Key;
use crate::keys_tab::KeysTab;
use crate::loghook::LogHook;
use crate::logs_tab::LogsTab;
use crate::msg::{Ctx, Msg};
use crate::oauth_tab::OAuthTab;
use crate::styles::{self, blank, styled};
use crate::text::{fit_width, width};
use crate::widgets::TextInput;

pub const TAB_DASHBOARD: usize = 0;
pub const TAB_CONFIG: usize = 1;
pub const TAB_AUTH_FILES: usize = 2;
pub const TAB_API_KEYS: usize = 3;
pub const TAB_OAUTH: usize = 4;
pub const TAB_LOGS: usize = 5;

/// Rows taken by the tab bar and the status bar.
const CHROME_ROWS: u16 = 2;

pub struct App {
    pub active_tab: usize,
    pub tabs: Vec<&'static str>,

    pub standalone: bool,
    pub logs_enabled: bool,

    pub authenticated: bool,
    pub auth_input: TextInput,
    pub auth_error: String,
    pub auth_connecting: bool,

    pub dashboard: Dashboard,
    pub config: ConfigTab,
    pub auth: AuthTab,
    pub keys: KeysTab,
    pub oauth: OAuthTab,
    pub logs: LogsTab,

    ctx: Ctx,
    pub width: u16,
    pub height: u16,
    pub ready: bool,

    /// Tabs whose data has been fetched.
    initialized: [bool; 6],
}

impl App {
    /// `NewAppWithBaseURL`: standalone mode is implied by a log hook, and skips the password gate.
    pub fn new(base_url: &str, secret_key: &str, hook: Option<LogHook>, tx: UnboundedSender<Msg>) -> App {
        let standalone = hook.is_some();
        let auth_required = !standalone;
        let mut auth_input = TextInput::new();
        auth_input.char_limit = 512;
        auth_input.password = true;
        auth_input.set_value(secret_key.trim());
        auth_input.focus();

        let client = Arc::new(Client::with_base_url(base_url, secret_key));
        let ctx = Ctx::new(tx, client);
        let mut app = App {
            active_tab: TAB_DASHBOARD,
            tabs: Vec::new(),
            standalone,
            logs_enabled: true,
            authenticated: !auth_required,
            auth_input,
            auth_error: String::new(),
            auth_connecting: false,
            dashboard: Dashboard::new(ctx.clone()),
            config: ConfigTab::new(ctx.clone()),
            auth: AuthTab::new(ctx.clone()),
            keys: KeysTab::new(ctx.clone()),
            oauth: OAuthTab::new(Some(ctx.clone())),
            logs: LogsTab::new(ctx.clone(), hook),
            ctx,
            width: 0,
            height: 0,
            ready: false,
            initialized: [true, false, false, false, false, true],
        };
        app.refresh_tabs();
        if auth_required {
            app.initialized = [false; 6];
        }
        app.set_auth_input_prompt();
        app
    }

    /// `Init`: the first fetches once authenticated.
    pub fn init(&self) {
        if !self.authenticated {
            return;
        }
        self.dashboard.init();
        if self.logs_enabled {
            self.logs.init();
        }
    }

    /// Handles one message; returns true when the app should quit.
    pub fn update(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Resize(w, h) => {
                self.resize(w, h);
                false
            }
            Msg::AuthConnect(result) => {
                self.on_auth_connect(result);
                false
            }
            Msg::ConfigUpdate { .. } => {
                self.on_config_update(&msg);
                false
            }
            Msg::Key(key) => self.on_key(key),
            Msg::Paste(text) => {
                if !self.authenticated {
                    self.auth_input.insert_text(&text);
                } else {
                    self.route_to_active(&Msg::Paste(text));
                }
                false
            }
            other => {
                self.route(&other);
                false
            }
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
        self.ready = true;
        if w > 0 {
            self.auth_input.width = (w as usize).saturating_sub(6);
        }
        let content_h = h.saturating_sub(CHROME_ROWS).max(1) as usize;
        let content_w = w as usize;
        self.dashboard.set_size(content_w, content_h);
        self.config.set_size(content_w, content_h);
        self.auth.set_size(content_w, content_h);
        self.keys.set_size(content_w, content_h);
        self.oauth.set_size(content_w, content_h);
        self.logs.set_size(content_w, content_h);
    }

    fn on_auth_connect(&mut self, result: Result<Value, String>) {
        self.auth_connecting = false;
        let cfg = match result {
            Err(e) => {
                self.auth_error = tf("auth_gate_connect_fail", &[&e]);
                return;
            }
            Ok(cfg) => cfg,
        };
        self.auth_error.clear();
        self.authenticated = true;
        self.logs_enabled = self.standalone || is_logs_enabled_from_config(&cfg);
        self.refresh_tabs();
        self.initialized = [false; 6];
        self.initialized[TAB_DASHBOARD] = true;
        self.dashboard.init();
        if self.logs_enabled {
            self.initialized[TAB_LOGS] = true;
            self.logs.init();
        }
    }

    /// Toggling `logging-to-file` shows or hides the Logs tab in client mode.
    fn on_config_update(&mut self, msg: &Msg) {
        if let Msg::ConfigUpdate { path, value, err: None } = msg
            && !self.standalone
            && path == "logging-to-file"
            && let Some(Value::Bool(enabled)) = value
        {
            let before = self.logs_enabled;
            self.logs_enabled = *enabled;
            if before != self.logs_enabled {
                self.refresh_tabs();
            }
            if !self.logs_enabled {
                self.initialized[TAB_LOGS] = false;
            }
            if !before && self.logs_enabled {
                self.initialized[TAB_LOGS] = true;
                self.logs.init();
            }
        }
        self.config.update(msg);
    }

    fn on_key(&mut self, key: Key) -> bool {
        if !self.authenticated {
            return self.on_gate_key(key);
        }
        // Text editors own every key except ctrl+c and tab switching, so `q` and `L` can be typed.
        let capturing = self.active_captures_text();
        match key.as_str() {
            "ctrl+c" => return true,
            "q" if !capturing => {
                // The Logs tab keeps `q` for itself.
                if !self.logs_enabled || self.active_tab != TAB_LOGS {
                    return true;
                }
            }
            "L" if !capturing => {
                toggle_locale();
                self.refresh_tabs();
                self.broadcast(&Msg::LocaleChanged);
                return false;
            }
            "tab" => {
                if self.tabs.is_empty() {
                    return false;
                }
                self.active_tab = (self.active_tab + 1) % self.tabs.len();
                self.init_tab_if_needed();
                return false;
            }
            "shift+tab" => {
                if self.tabs.is_empty() {
                    return false;
                }
                self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
                self.init_tab_if_needed();
                return false;
            }
            _ => {}
        }
        self.route_to_active(&Msg::Key(key));
        false
    }

    /// Password gate keys: `q`/ctrl+c quit, `L` language, Enter connects, the rest edit the input.
    fn on_gate_key(&mut self, key: Key) -> bool {
        match key.as_str() {
            "ctrl+c" | "q" => return true,
            "L" => {
                toggle_locale();
                self.refresh_tabs();
                self.set_auth_input_prompt();
            }
            "enter" => {
                if self.auth_connecting {
                    return false;
                }
                let password = self.auth_input.value().trim().to_string();
                if password.is_empty() {
                    self.auth_error = t("auth_gate_password_required").to_string();
                    return false;
                }
                self.auth_error.clear();
                self.auth_connecting = true;
                let client = self.ctx.client.clone();
                self.ctx.spawn(async move {
                    client.set_secret_key(&password);
                    Some(Msg::AuthConnect(client.get_config().await))
                });
            }
            _ => self.auth_input.handle_key(&key),
        }
        false
    }

    fn active_captures_text(&self) -> bool {
        match self.active_tab {
            TAB_CONFIG => self.config.captures_text(),
            TAB_AUTH_FILES => self.auth.captures_text(),
            TAB_API_KEYS => self.keys.captures_text(),
            TAB_OAUTH => self.oauth.captures_text(),
            _ => false,
        }
    }

    fn route_to_active(&mut self, msg: &Msg) {
        match self.active_tab {
            TAB_DASHBOARD => self.dashboard.update(msg),
            TAB_CONFIG => self.config.update(msg),
            TAB_AUTH_FILES => self.auth.update(msg),
            TAB_API_KEYS => self.keys.update(msg),
            TAB_OAUTH => self.oauth.update(msg),
            TAB_LOGS => self.logs.update(msg),
            _ => {}
        }
    }

    /// Data messages go to their owning tab even when another tab is showing, so a fetch that
    /// finishes after a tab switch is not lost.
    fn route(&mut self, msg: &Msg) {
        match msg {
            Msg::DashboardData { .. } => self.dashboard.update(msg),
            Msg::ConfigData { .. } => self.config.update(msg),
            Msg::AuthFiles { .. } | Msg::AuthAction { .. } => self.auth.update(msg),
            Msg::KeysData(_) | Msg::KeyAction { .. } => self.keys.update(msg),
            Msg::OAuthStart(_) | Msg::OAuthPoll(_) | Msg::OAuthCallbackSubmit { .. } => self.oauth.update(msg),
            Msg::LogsPoll { .. } | Msg::LogsTick | Msg::LogLine(_) => {
                if self.logs_enabled {
                    self.logs.update(msg);
                }
            }
            Msg::LocaleChanged => self.broadcast(msg),
            _ => {}
        }
    }

    fn broadcast(&mut self, msg: &Msg) {
        self.dashboard.update(msg);
        self.config.update(msg);
        self.auth.update(msg);
        self.keys.update(msg);
        self.oauth.update(msg);
        self.logs.update(msg);
    }

    /// `refreshTabs`: the tab list without Logs when it is disabled; clamps the active tab.
    fn refresh_tabs(&mut self) {
        let names = tab_names();
        self.tabs = if self.logs_enabled {
            names.to_vec()
        } else {
            names.iter().enumerate().filter(|(i, _)| *i != TAB_LOGS).map(|(_, n)| *n).collect()
        };
        if self.tabs.is_empty() {
            self.active_tab = TAB_DASHBOARD;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// `initTabIfNeeded`: first visit triggers the tab's initial fetch.
    fn init_tab_if_needed(&mut self) {
        if self.initialized[self.active_tab] {
            return;
        }
        self.initialized[self.active_tab] = true;
        match self.active_tab {
            TAB_DASHBOARD => self.dashboard.init(),
            TAB_CONFIG => self.config.init(),
            TAB_AUTH_FILES => self.auth.init(),
            TAB_API_KEYS => self.keys.init(),
            TAB_OAUTH => {}
            TAB_LOGS if self.logs_enabled => self.logs.init(),
            _ => {}
        }
    }

    fn set_auth_input_prompt(&mut self) {
        self.auth_input.prompt = format!("  {}: ", t("auth_gate_password"));
    }

    /// Renders the whole screen.
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Block::default().style(styles::base()), area);
        if !self.authenticated {
            f.render_widget(Paragraph::new(self.render_auth_view()), area);
            return;
        }
        if !self.ready {
            f.render_widget(Paragraph::new(t("initializing_tui")), area);
            return;
        }
        if area.height < CHROME_ROWS + 1 {
            return;
        }
        let tab_bar = Rect { height: 1, ..area };
        let content = Rect { y: area.y + 1, height: area.height - CHROME_ROWS, ..area };
        let status = Rect { y: area.y + area.height - 1, height: 1, ..area };

        f.render_widget(Paragraph::new(self.render_tab_bar()), tab_bar);
        self.logs.flush();
        match self.active_tab {
            TAB_DASHBOARD => f.render_widget(&self.dashboard.viewport, content),
            TAB_CONFIG => f.render_widget(&self.config.viewport, content),
            TAB_AUTH_FILES => f.render_widget(&self.auth.viewport, content),
            TAB_API_KEYS => f.render_widget(&self.keys.viewport, content),
            TAB_OAUTH => f.render_widget(&self.oauth.viewport, content),
            TAB_LOGS if self.logs_enabled => f.render_widget(&self.logs.viewport, content),
            _ => {}
        }
        f.render_widget(Paragraph::new(self.render_status_bar()), status);
    }

    fn render_auth_view(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        styles::push_title(&mut out, t("auth_gate_title"));
        out.push(styled(t("auth_gate_help"), styles::help()));
        out.push(blank());
        if self.auth_connecting {
            out.push(styled(t("auth_gate_connecting"), styles::warning()));
            out.push(blank());
        }
        if !self.auth_error.trim().is_empty() {
            out.push(styled(self.auth_error.clone(), styles::error()));
            out.push(blank());
        }
        out.push(Line::from(self.auth_input.view()));
        out.push(styled(t("auth_gate_enter"), styles::help()));
        out
    }

    fn render_tab_bar(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for (i, name) in self.tabs.iter().enumerate() {
            let style = if i == self.active_tab {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(styles::MUTED)
            };
            spans.push(Span::raw("  "));
            spans.push(Span::styled(*name, style));
            spans.push(Span::raw("  "));
        }
        Line::from(spans)
    }

    /// Left title and right key hints on one row, shrinking the hints (then the title) to fit.
    pub fn render_status_bar(&self) -> Line<'static> {
        let mut left = t("status_left").trim_end().to_string();
        let mut right = t("status_right").trim_end().to_string();
        let total = (self.width as usize).max(1);
        // One cell of padding on each side.
        let content_width = total.saturating_sub(2);
        if width(&left) > content_width {
            left = fit_width(&left, content_width);
            right.clear();
        }
        let remaining = content_width.saturating_sub(width(&left));
        if width(&right) > remaining {
            right = fit_width(&right, remaining);
        }
        let gap = content_width.saturating_sub(width(&left) + width(&right));
        Line::styled(format!(" {left}{}{right} ", " ".repeat(gap)), Style::default().fg(styles::SUBTEXT))
    }
}

/// `isLogsEnabledFromConfig`: logs show unless `logging-to-file` is explicitly false.
pub fn is_logs_enabled_from_config(cfg: &Value) -> bool {
    match cfg.get("logging-to-file") {
        Some(Value::Bool(b)) => *b,
        _ => true,
    }
}
