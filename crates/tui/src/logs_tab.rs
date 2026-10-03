//! Logs tab (Go: internal/tui/logs_tab.go): live log lines, from the in-process hook in standalone
//! mode or by polling `/v0/management/logs` otherwise, with level filters and auto-scroll.

use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::i18n::t;
use crate::loghook::LogHook;
use crate::msg::{Ctx, Msg};
use crate::styles::{self, styled};
use crate::widgets::Viewport;

const MAX_LINES: usize = 5000;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct LogsTab {
    ctx: Ctx,
    hook: Option<LogHook>,
    pub viewport: Viewport,
    pub lines: Vec<String>,
    pub auto_scroll: bool,
    width: usize,
    /// "", "info", "warn" or "error".
    pub filter: &'static str,
    after: i64,
    last_err: Option<String>,
    /// Content is rebuilt lazily by `flush` so bursts of lines cost one render per frame.
    dirty: bool,
}

impl LogsTab {
    pub fn new(ctx: Ctx, hook: Option<LogHook>) -> Self {
        LogsTab {
            ctx,
            hook,
            viewport: Viewport::default(),
            lines: Vec::new(),
            auto_scroll: true,
            width: 0,
            filter: "",
            after: 0,
            last_err: None,
            dirty: true,
        }
    }

    pub fn init(&self) {
        if self.hook.is_some() {
            self.wait_for_log();
        } else {
            self.fetch_logs();
        }
    }

    fn fetch_logs(&self) {
        let client = self.ctx.client.clone();
        let after = self.after;
        self.ctx.spawn(async move {
            Some(match client.get_logs(after, 200).await {
                Ok((lines, latest)) => Msg::LogsPoll { lines, latest, err: None },
                Err(e) => Msg::LogsPoll { lines: Vec::new(), latest: after, err: Some(e) },
            })
        });
    }

    fn wait_for_next_poll(&self) {
        self.ctx.spawn(async {
            tokio::time::sleep(POLL_INTERVAL).await;
            Some(Msg::LogsTick)
        });
    }

    fn wait_for_log(&self) {
        let Some(hook) = self.hook.clone() else { return };
        self.ctx.spawn(async move { Some(Msg::LogLine(hook.recv().await)) });
    }

    fn push_lines(&mut self, new: impl IntoIterator<Item = String>) {
        self.lines.extend(new);
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
    }

    /// Rebuilds the viewport content when something changed since the last frame.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let lines = self.render_logs();
        self.viewport.set_content(lines);
        if self.auto_scroll {
            self.viewport.goto_bottom();
        }
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => self.dirty = true,
            Msg::LogsTick => {
                if self.hook.is_none() {
                    self.fetch_logs();
                }
            }
            Msg::LogsPoll { lines, latest, err } => {
                if self.hook.is_some() {
                    return;
                }
                match err {
                    Some(e) => self.last_err = Some(e.clone()),
                    None => {
                        self.last_err = None;
                        self.after = *latest;
                        self.push_lines(lines.iter().cloned());
                    }
                }
                self.dirty = true;
                self.wait_for_next_poll();
            }
            Msg::LogLine(line) => {
                self.push_lines([line.clone()]);
                self.dirty = true;
                self.wait_for_log();
            }
            Msg::Key(key) => self.handle_key(key.as_str()),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: &str) {
        self.flush();
        match key {
            "a" => {
                self.auto_scroll = !self.auto_scroll;
                if self.auto_scroll {
                    self.viewport.goto_bottom();
                }
                self.dirty = true;
            }
            "c" => {
                self.lines.clear();
                self.last_err = None;
                self.dirty = true;
            }
            "1" | "2" | "3" | "4" => {
                self.filter = match key {
                    "2" => "info",
                    "3" => "warn",
                    "4" => "error",
                    _ => "",
                };
                self.dirty = true;
            }
            other => {
                let was_at_bottom = self.viewport.at_bottom();
                self.viewport.handle_key(other);
                // Scrolling up pauses auto-scroll; reaching the bottom resumes it.
                if !self.viewport.at_bottom() && was_at_bottom {
                    self.auto_scroll = false;
                    self.dirty = true;
                }
                if self.viewport.at_bottom() && !self.auto_scroll {
                    self.auto_scroll = true;
                    self.dirty = true;
                }
            }
        }
        self.flush();
    }

    pub fn set_size(&mut self, w: usize, h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = h;
        self.dirty = true;
    }

    fn render_logs(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let scroll_status = if self.auto_scroll {
            Span::styled(t("logs_auto_scroll"), styles::success())
        } else {
            Span::styled(t("logs_paused"), styles::warning())
        };
        let filter_label = if self.filter.is_empty() { "ALL".to_string() } else { format!("{}+", self.filter.to_uppercase()) };
        out.push(Line::from(vec![
            Span::styled(format!(" {}  ", t("logs_title")), styles::title()),
            scroll_status,
            Span::styled(
                format!(
                    "  {}: {}  {}: {}",
                    t("logs_filter"),
                    filter_label,
                    t("logs_lines"),
                    self.lines.len()
                ),
                styles::title(),
            ),
        ]));
        out.push(Line::default());
        out.push(styled(t("logs_help"), styles::help()));
        out.push(styles::rule(self.width));

        if let Some(e) = &self.last_err {
            out.push(styled(format!("⚠ Error: {e}"), styles::error()));
        }
        if self.lines.is_empty() {
            out.push(styled(t("logs_waiting"), styles::subtitle()));
            return out;
        }
        for line in &self.lines {
            if !self.filter.is_empty() && !self.match_level(line) {
                continue;
            }
            out.push(style_line(line));
        }
        out
    }

    fn match_level(&self, line: &str) -> bool {
        match self.filter {
            "error" => line.contains("[error]") || line.contains("[fatal]") || line.contains("[panic]"),
            "warn" => line.contains("[warn") || line.contains("[error]") || line.contains("[fatal]"),
            "info" => !line.contains("[debug]"),
            _ => true,
        }
    }
}

/// `styleLine`: colour by the bracketed level token.
fn style_line(line: &str) -> Line<'static> {
    let style = if line.contains("[error]") || line.contains("[fatal]") {
        Some(styles::log_error())
    } else if line.contains("[warn") {
        Some(styles::log_warn())
    } else if line.contains("[info") {
        Some(styles::log_info())
    } else if line.contains("[debug]") {
        Some(styles::log_debug())
    } else {
        None
    };
    Line::from(Span::styled(line.to_string(), style.unwrap_or_else(Style::default)))
}
