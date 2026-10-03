//! OAuth tab (Go: internal/tui/oauth_tab.go): starts a provider login through the management API,
//! shows the authorization URL (opening the browser best-effort), and either polls the session
//! (device-code flow) or accepts a pasted callback URL (web flow).

use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::json;

use crate::browser::open_browser;
use crate::i18n::{t, tf};
use crate::jsonutil::{get_float, get_string};
use crate::keys::Key;
use crate::msg::{Ctx, Msg, OAuthPoll, OAuthStart};
use crate::styles::{self, blank, push_title, styled};
use crate::text::wrap_text;
use crate::widgets::{TextInput, Viewport};

/// An OAuth provider entry.
pub struct Provider {
    pub name: &'static str,
    /// Management API path that returns the authorization URL.
    pub api_path: &'static str,
    pub emoji: &'static str,
    /// RFC 8628 device-code provider.
    pub device_flow: bool,
}

pub static PROVIDERS: [Provider; 7] = [
    Provider { name: "Claude (Anthropic)", api_path: "anthropic-auth-url", emoji: "🟧", device_flow: false },
    Provider { name: "Codex (OpenAI)", api_path: "codex-auth-url", emoji: "🟩", device_flow: false },
    Provider { name: "Antigravity", api_path: "antigravity-auth-url", emoji: "🟪", device_flow: false },
    Provider { name: "Kimi (kimi.com)", api_path: "kimi-auth-url", emoji: "🟫", device_flow: true },
    Provider { name: "Kimi (kimi.ai)", api_path: "kimi-ai-auth-url", emoji: "🟫", device_flow: true },
    Provider { name: "xAI", api_path: "xai-auth-url", emoji: "⬛", device_flow: true },
    Provider { name: "Meta", api_path: "meta-auth-url", emoji: "🔵", device_flow: true },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthState {
    Idle,
    Pending,
    /// Remote browser mode: waiting for a manual callback or device authorization.
    Remote,
    Success,
    Error,
}

const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEVICE_POLL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_STATUS_POLL_ERRORS: u32 = 5;
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct OAuthTab {
    /// `None` only in unit tests, where no background work may start.
    ctx: Option<Ctx>,
    pub viewport: Viewport,
    pub cursor: usize,
    pub state: OAuthState,
    pub message: Option<Line<'static>>,
    pub err: Option<String>,
    width: usize,

    pub auth_url: String,
    pub auth_state: String,
    pub provider_name: String,
    pub user_code: String,
    pub device_flow: bool,
    pub expires_in: i64,
    pub callback_input: TextInput,
    pub input_active: bool,

    /// Bumped on cancel or restart so in-flight start/poll results are ignored.
    pub poll_generation: u64,
}

impl OAuthTab {
    pub fn new(ctx: Option<Ctx>) -> Self {
        let mut callback_input = TextInput::new();
        callback_input.placeholder = "http://localhost:.../auth/callback?code=...&state=...".into();
        callback_input.char_limit = 2048;
        // The Go source hard-codes this Chinese prompt regardless of locale.
        callback_input.prompt = "  回调 URL: ".into();
        OAuthTab {
            ctx,
            viewport: Viewport::default(),
            cursor: 0,
            state: OAuthState::Idle,
            message: None,
            err: None,
            width: 0,
            auth_url: String::new(),
            auth_state: String::new(),
            provider_name: String::new(),
            user_code: String::new(),
            device_flow: false,
            expires_in: 0,
            callback_input,
            input_active: false,
            poll_generation: 0,
        }
    }

    fn refresh_view(&mut self) {
        let lines = self.render_content();
        self.viewport.set_content(lines);
    }

    /// The callback URL input is focused (global `q` / `L` must not fire).
    pub fn captures_text(&self) -> bool {
        self.input_active && !self.device_flow
    }

    pub fn update(&mut self, msg: &Msg) {
        match msg {
            Msg::LocaleChanged => self.refresh_view(),
            Msg::OAuthStart(start) => self.on_start(start),
            Msg::OAuthPoll(poll) => self.on_poll(poll),
            Msg::OAuthCallbackSubmit { err } => {
                self.message = Some(match err {
                    Some(e) => styled(format!("{}: {e}", t("oauth_submit_fail")), styles::error()),
                    None => styled(t("oauth_submit_ok"), styles::success()),
                });
                self.refresh_view();
            }
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) if self.captures_text() => {
                self.callback_input.insert_text(text);
                self.refresh_view();
            }
            _ => {}
        }
    }

    fn on_start(&mut self, msg: &OAuthStart) {
        if !should_accept_oauth_start(msg, self.poll_generation) {
            // Stale start after Esc/restart: cancel the server session so credentials are not saved.
            if msg.err.is_none() && !msg.state.trim().is_empty() {
                self.cancel_oauth_session(&msg.state);
            }
            return;
        }
        if let Some(e) = &msg.err {
            self.state = OAuthState::Error;
            self.err = Some(e.clone());
            self.message = Some(styled(format!("✗ {e}"), styles::error()));
            self.refresh_view();
            return;
        }
        self.auth_url = msg.url.clone();
        self.auth_state = msg.state.clone();
        self.provider_name = msg.provider_name.clone();
        self.user_code = msg.user_code.clone();
        self.device_flow = msg.device_flow;
        self.expires_in = msg.expires_in;
        self.state = OAuthState::Remote;
        self.callback_input.set_value("");
        self.message = None;
        if self.device_flow {
            self.input_active = false;
            self.callback_input.blur();
        } else {
            self.callback_input.focus();
            self.input_active = true;
        }
        self.refresh_view();
        self.poll_oauth_status(&msg.state, msg.expires_in, self.device_flow, msg.generation);
    }

    fn on_poll(&mut self, msg: &OAuthPoll) {
        if !should_accept_oauth_poll(msg, &self.auth_state, self.poll_generation, self.state) {
            return;
        }
        if let Some(e) = &msg.err {
            self.state = OAuthState::Error;
            self.err = Some(e.clone());
            self.message = Some(styled(format!("✗ {e}"), styles::error()));
            self.input_active = false;
            self.callback_input.blur();
        } else if msg.done {
            self.state = OAuthState::Success;
            self.message = Some(styled(format!("✓ {}", msg.message), styles::success()));
            self.input_active = false;
            self.callback_input.blur();
        } else {
            self.message = Some(styled(format!("⏳ {}", msg.message), styles::warning()));
        }
        self.refresh_view();
    }

    fn handle_key(&mut self, key: &Key) {
        // Typing a callback URL (web flow only).
        if self.input_active && !self.device_flow {
            match key.as_str() {
                "enter" => {
                    let callback_url = self.callback_input.value();
                    if callback_url.is_empty() {
                        return;
                    }
                    self.input_active = false;
                    self.callback_input.blur();
                    self.message = Some(styled(t("oauth_submitting"), styles::warning()));
                    self.refresh_view();
                    self.submit_callback(callback_url);
                }
                "esc" => self.cancel_remote_oauth(),
                _ => {
                    self.callback_input.handle_key(key);
                    self.refresh_view();
                }
            }
            return;
        }

        match self.state {
            OAuthState::Remote => match key.as_str() {
                "c" | "C" => {
                    if self.device_flow {
                        return;
                    }
                    self.input_active = true;
                    self.callback_input.focus();
                    self.refresh_view();
                }
                "esc" => self.cancel_remote_oauth(),
                other => {
                    self.viewport.handle_key(other);
                }
            },
            OAuthState::Pending => {
                if key.as_str() == "esc" {
                    self.poll_generation += 1;
                    self.state = OAuthState::Idle;
                    self.message = None;
                    self.refresh_view();
                }
            }
            _ => match key.as_str() {
                "up" | "k" => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.refresh_view();
                    }
                }
                "down" | "j" => {
                    if self.cursor + 1 < PROVIDERS.len() {
                        self.cursor += 1;
                        self.refresh_view();
                    }
                }
                "enter" => {
                    if let Some(provider) = PROVIDERS.get(self.cursor) {
                        self.poll_generation += 1;
                        self.state = OAuthState::Pending;
                        self.message = Some(styled(tf("oauth_initiating", &[&provider.name]), styles::warning()));
                        self.refresh_view();
                        self.start_oauth(provider, self.poll_generation);
                    }
                }
                "esc" => {
                    self.state = OAuthState::Idle;
                    self.message = None;
                    self.err = None;
                    self.refresh_view();
                }
                other => {
                    self.viewport.handle_key(other);
                }
            },
        }
    }

    /// `startOAuth`: fetch the auth URL, open the browser, report the session.
    fn start_oauth(&self, provider: &'static Provider, generation: u64) {
        let Some(ctx) = &self.ctx else { return };
        let client = ctx.client.clone();
        ctx.spawn(async move {
            let failed = |err: String| {
                Some(Msg::OAuthStart(OAuthStart { generation, err: Some(err), ..Default::default() }))
            };
            let data = match client.start_oauth(provider.api_path).await {
                Ok(d) => d,
                Err(e) => return failed(format!("failed to start {} login: {e}", provider.name)),
            };
            let auth_url = get_string(&data, "url");
            let state = get_string(&data, "state");
            if auth_url.is_empty() {
                return failed(format!("no auth URL returned for {}", provider.name));
            }
            let user_code = get_string(&data, "user_code");
            let flow = get_string(&data, "flow").trim().to_lowercase();
            let expires_in = get_float(&data, "expires_in") as i64;
            let device_flow = provider.device_flow || flow == "device" || !user_code.is_empty();
            // Best effort.
            let _ = open_browser(&auth_url);
            Some(Msg::OAuthStart(OAuthStart {
                url: auth_url,
                state,
                provider_name: provider.name.to_string(),
                user_code,
                device_flow,
                expires_in,
                generation,
                err: None,
            }))
        });
    }

    /// `cancelRemoteOAuth`: clears the remote/device UI state and cancels the server session.
    pub fn cancel_remote_oauth(&mut self) {
        let state = std::mem::take(&mut self.auth_state);
        self.poll_generation += 1;
        self.state = OAuthState::Idle;
        self.message = None;
        self.auth_url.clear();
        self.user_code.clear();
        self.device_flow = false;
        self.expires_in = 0;
        self.input_active = false;
        self.callback_input.blur();
        self.callback_input.set_value("");
        self.refresh_view();
        self.cancel_oauth_session(&state);
    }

    fn cancel_oauth_session(&self, state: &str) {
        let state = state.trim().to_string();
        let Some(ctx) = &self.ctx else { return };
        if state.is_empty() {
            return;
        }
        let client = ctx.client.clone();
        ctx.spawn(async move {
            let _ = client.cancel_auth_session(&state).await;
            None
        });
    }

    fn submit_callback(&self, callback_url: String) {
        let Some(ctx) = &self.ctx else { return };
        let client = ctx.client.clone();
        let provider_key = provider_key(&self.provider_name);
        let state = self.auth_state.clone();
        ctx.spawn(async move {
            let body = json!({"provider": provider_key, "redirect_url": callback_url, "state": state});
            let err = client.post_json("/v0/management/oauth-callback", &body).await.err();
            Some(Msg::OAuthCallbackSubmit { err })
        });
    }

    /// `pollOAuthStatus`: checks the session every 2s until it finishes, fails or times out.
    fn poll_oauth_status(&self, state: &str, expires_in: i64, device_flow: bool, generation: u64) {
        let Some(ctx) = &self.ctx else { return };
        let client = ctx.client.clone();
        let state = state.to_string();
        ctx.spawn(async move {
            let timeout = if expires_in > 0 {
                Duration::from_secs(expires_in as u64)
            } else if device_flow {
                DEVICE_POLL_TIMEOUT
            } else {
                DEFAULT_POLL_TIMEOUT
            };
            let deadline = tokio::time::Instant::now() + timeout;
            let mut consecutive_errors = 0;
            let result = |done: bool, message: String, err: Option<String>| {
                Some(Msg::OAuthPoll(OAuthPoll { state: state.clone(), generation, done, message, err }))
            };
            loop {
                if tokio::time::Instant::now() > deadline {
                    return result(false, String::new(), Some(t("oauth_timeout").to_string()));
                }
                tokio::time::sleep(STATUS_POLL_INTERVAL).await;
                match client.get_auth_status(&state).await {
                    Err(e) => {
                        consecutive_errors += 1;
                        if should_fail_oauth_status_poll(consecutive_errors, MAX_STATUS_POLL_ERRORS) {
                            return result(false, String::new(), Some(format!("{}: {e}", t("oauth_status_error"))));
                        }
                    }
                    Ok((status, err_msg)) => {
                        consecutive_errors = 0;
                        match status.as_str() {
                            "ok" => return result(true, t("oauth_success").to_string(), None),
                            "error" => {
                                return result(false, String::new(), Some(format!("{}: {err_msg}", t("oauth_failed"))));
                            }
                            "wait" => {}
                            _ => return result(true, t("oauth_completed").to_string(), None),
                        }
                    }
                }
            }
        });
    }

    pub fn set_size(&mut self, w: usize, _h: usize) {
        self.width = w;
        self.viewport.width = w;
        self.viewport.height = _h;
        self.callback_input.width = w.saturating_sub(16);
        self.refresh_view();
    }

    fn render_content(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        push_title(&mut out, t("oauth_title"));

        if let Some(msg) = &self.message {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(msg.spans.iter().cloned());
            out.push(Line::from(spans));
            out.push(blank());
        }

        if self.state == OAuthState::Remote {
            if self.device_flow {
                self.render_device_mode(&mut out);
            } else {
                self.render_remote_mode(&mut out);
            }
            return out;
        }
        if self.state == OAuthState::Pending {
            out.push(styled(t("oauth_press_esc"), styles::help()));
            return out;
        }

        out.push(styled(t("oauth_select"), styles::help()));
        out.push(blank());
        for (i, p) in PROVIDERS.iter().enumerate() {
            let selected = i == self.cursor;
            let prefix = if selected { "▸ " } else { "  " };
            let label = format!(" {} {} ", p.emoji, p.name);
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD).bg(styles::SELECTED_BG)
            } else {
                Style::default()
            };
            out.push(Line::from(vec![Span::raw(prefix), Span::styled(label, style)]));
        }
        out.push(blank());
        out.push(styled(t("oauth_help"), styles::help()));
        out
    }

    fn push_auth_url(&self, out: &mut Vec<Line<'static>>) {
        out.push(styled(t("oauth_auth_url"), styles::bold().fg(styles::SUBTEXT)));
        let max_url_width = self.width.saturating_sub(6).max(40);
        for line in wrap_text(&self.auth_url, max_url_width) {
            out.push(Line::from(format!("  {line}")));
        }
    }

    fn render_remote_mode(&self, out: &mut Vec<Line<'static>>) {
        out.push(styled(format!("  ✦ {} OAuth", self.provider_name), styles::title()));
        out.push(blank());
        self.push_auth_url(out);
        out.push(blank());
        out.push(styled(t("oauth_remote_hint"), styles::help()));
        out.push(blank());
        out.push(styled(t("oauth_callback_url"), styles::bold().fg(styles::SUBTEXT)));
        if self.input_active {
            out.push(Line::from(self.callback_input.view()));
            out.push(styled(format!("  {} • {}", t("enter_submit"), t("esc_cancel")), styles::help()));
        } else {
            out.push(styled(t("oauth_press_c"), styles::help()));
        }
        out.push(blank());
        out.push(styled(t("oauth_waiting"), styles::warning()));
    }

    fn render_device_mode(&self, out: &mut Vec<Line<'static>>) {
        out.push(styled(format!("  ✦ {} OAuth", self.provider_name), styles::title()));
        out.push(blank());
        self.push_auth_url(out);
        out.push(blank());
        if !self.user_code.trim().is_empty() {
            out.push(styled(t("oauth_user_code"), styles::bold().fg(styles::SUBTEXT)));
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!(" {} ", self.user_code),
                    Style::default().add_modifier(Modifier::BOLD).bg(styles::SELECTED_BG),
                ),
            ]));
            out.push(blank());
        }
        out.push(styled(t("oauth_device_hint"), styles::help()));
        if self.expires_in > 0 {
            out.push(styled(tf("oauth_device_expires", &[&self.expires_in]), styles::help()));
        }
        out.push(blank());
        out.push(styled(t("oauth_waiting"), styles::warning()));
        out.push(styled(t("oauth_press_esc"), styles::help()));
    }
}

/// Canonical provider key the callback endpoint expects.
fn provider_key(provider_name: &str) -> &'static str {
    match PROVIDERS.iter().find(|p| p.name == provider_name).map(|p| p.api_path) {
        Some("anthropic-auth-url") => "anthropic",
        Some("codex-auth-url") => "codex",
        Some("antigravity-auth-url") => "antigravity",
        Some("kimi-auth-url") => "kimi",
        Some("kimi-ai-auth-url") => "kimi-ai",
        Some("xai-auth-url") => "xai",
        Some("meta-auth-url") => "meta",
        _ => "",
    }
}

/// `shouldAcceptOAuthStart`: a start result belongs to the current flow.
pub fn should_accept_oauth_start(msg: &OAuthStart, generation: u64) -> bool {
    msg.generation == generation
}

/// `shouldAcceptOAuthPoll`: a poll result belongs to the active remote flow.
pub fn should_accept_oauth_poll(msg: &OAuthPoll, auth_state: &str, generation: u64, state: OAuthState) -> bool {
    if msg.generation != generation {
        return false;
    }
    if msg.state.is_empty() || msg.state != auth_state {
        return false;
    }
    state == OAuthState::Remote
}

/// `shouldFailOAuthStatusPoll`: consecutive status errors that end the flow.
pub fn should_fail_oauth_status_poll(consecutive_errors: u32, max_errors: u32) -> bool {
    if max_errors == 0 {
        return consecutive_errors > 0;
    }
    consecutive_errors >= max_errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> OAuthTab {
        let mut m = OAuthTab::new(None);
        m.set_size(80, 24);
        m
    }

    fn poll(state: &str, generation: u64) -> OAuthPoll {
        OAuthPoll { state: state.into(), generation, done: true, message: "ok".into(), err: None }
    }

    #[test]
    fn poll_filters_stale_messages() {
        let msg = poll("state-a", 1);
        assert!(!should_accept_oauth_poll(&msg, "state-a", 2, OAuthState::Remote), "stale generation");
        assert!(!should_accept_oauth_poll(&msg, "state-b", 1, OAuthState::Remote), "mismatched state");
        assert!(!should_accept_oauth_poll(&msg, "state-a", 1, OAuthState::Idle), "not remote");
        assert!(should_accept_oauth_poll(&msg, "state-a", 1, OAuthState::Remote));
    }

    #[test]
    fn start_filters_stale_messages() {
        let msg = OAuthStart { state: "state-a".into(), generation: 1, url: "https://example.com".into(), ..Default::default() };
        assert!(!should_accept_oauth_start(&msg, 2));
        assert!(should_accept_oauth_start(&msg, 1));
    }

    #[test]
    fn status_poll_failure_threshold() {
        assert!(!should_fail_oauth_status_poll(4, 5));
        assert!(should_fail_oauth_status_poll(5, 5));
        assert!(should_fail_oauth_status_poll(1, 0));
    }

    #[test]
    fn update_ignores_stale_poll_and_accepts_current() {
        let mut m = tab();
        m.state = OAuthState::Remote;
        m.auth_state = "state-current".into();
        m.poll_generation = 2;
        m.update(&Msg::OAuthPoll(poll("state-old", 1)));
        assert_eq!(m.state, OAuthState::Remote);
        assert!(m.message.is_none(), "stale poll changed the message");

        m.poll_generation = 3;
        m.update(&Msg::OAuthPoll(poll("state-current", 3)));
        assert_eq!(m.state, OAuthState::Success);
    }

    #[test]
    fn esc_in_remote_mode_cancels_and_clears_state() {
        let mut m = tab();
        m.state = OAuthState::Remote;
        m.auth_state = "state-to-cancel".into();
        m.auth_url = "https://example.com".into();
        m.device_flow = true;
        m.poll_generation = 4;
        m.update(&Msg::Key(Key::named("esc")));
        assert_eq!(m.state, OAuthState::Idle);
        assert_eq!(m.poll_generation, 5);
        assert!(m.auth_state.is_empty() && m.auth_url.is_empty() && !m.device_flow);
    }

    #[test]
    fn esc_with_active_callback_input_cancels_remote_session() {
        let mut m = tab();
        m.state = OAuthState::Remote;
        m.auth_state = "state-to-cancel".into();
        m.auth_url = "https://example.com".into();
        m.input_active = true;
        m.callback_input.focus();
        m.callback_input.set_value("https://callback.example/?code=abc&state=state-to-cancel");
        m.poll_generation = 7;
        m.update(&Msg::Key(Key::named("esc")));
        assert_eq!(m.state, OAuthState::Idle);
        assert_eq!(m.poll_generation, 8);
        assert!(!m.input_active);
        assert!(m.callback_input.value().is_empty());
        assert!(m.auth_state.is_empty() && m.auth_url.is_empty());
    }

    #[test]
    fn stale_start_is_ignored() {
        let mut m = tab();
        m.poll_generation = 2;
        m.update(&Msg::OAuthStart(OAuthStart {
            url: "https://example.com".into(),
            state: "stale-state".into(),
            generation: 1,
            ..Default::default()
        }));
        assert_eq!(m.state, OAuthState::Idle);
        assert!(m.auth_state.is_empty());
    }
}
