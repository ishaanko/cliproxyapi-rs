//! Screen snapshots and interaction flows against a mock management API.

mod common;

use common::{Harness, Mock, assert_snapshot};
use cpa_tui::LogHook;
use cpa_tui::i18n;
use ratatui::style::Color;
use serde_json::{Value, json};

/// Standalone-mode harness (no gate) with the log hook pre-filled.
async fn standalone(mock: &Mock) -> Harness {
    let mut h = Harness::standalone(&mock.url(), "mgmt-secret", LogHook::new(100));
    h.start().await;
    h
}

async fn goto_tab(h: &mut Harness, n: usize) {
    for _ in 0..n {
        h.press("tab").await;
    }
}

#[tokio::test]
async fn gate_rejects_wrong_password_then_connects() {
    let mock = Mock::start().await;
    let mut h = Harness::client_mode(&mock.url(), "");
    h.start().await;
    assert_snapshot("gate_empty", &h.screen());

    h.press("enter").await;
    assert!(h.screen().contains("password is required"));

    h.type_text("wrong");
    h.press("enter").await;
    let screen = h.screen();
    assert!(screen.contains("Connection failed: HTTP 401"), "{screen}");
    assert!(screen.contains("Password: *****"));
    assert_snapshot("gate_wrong_password", &screen);

    h.key("ctrl+u");
    h.type_text("mgmt-secret");
    h.press("enter").await;
    assert!(h.app.authenticated);
    assert!(h.screen().contains("● Connected"));
    // The probe and the dashboard fetches all carry the entered secret.
    let calls = mock.calls_matching("GET", "/v0/management/config");
    assert_eq!(calls.last().map(|c| c.auth.as_str()), Some("Bearer mgmt-secret"));
}

#[tokio::test]
async fn gate_accepts_q_and_l_as_password_characters() {
    let mock = Mock::start().await;
    let mut h = Harness::client_mode(&mock.url(), "");
    h.start().await;
    h.type_text("qL");
    assert!(!h.quit);
    assert!(h.screen().contains("Password: **"));
    assert!(h.screen().contains("Ctrl+C: quit"));
    h.key("ctrl+c");
    assert!(h.quit);
}

#[tokio::test]
async fn dashboard_snapshot_and_refresh() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    assert_snapshot("dashboard", &h.screen());
    let before = mock.calls_matching("GET", "/v0/management/auth-files").len();
    h.press("r").await;
    assert_eq!(mock.calls_matching("GET", "/v0/management/auth-files").len(), before + 1);
}

#[tokio::test]
async fn config_tab_toggle_edit_and_validation() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    goto_tab(&mut h, 1).await;
    assert_snapshot("config", &h.screen());

    // Port and Host are read-only; Debug (third row) toggles with Enter.
    h.press("enter").await;
    assert!(mock.calls_matching("PUT", "/v0/management/").is_empty());
    h.key("j");
    h.key("j");
    h.press("enter").await;
    let puts = mock.calls_matching("PUT", "/v0/management/debug");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].body, r#"{"value":true}"#);
    assert!(h.screen().contains("✓ Updated successfully"));

    // Proxy URL: edit with q and L in the text (they must not quit or switch language).
    h.key("j");
    h.press("enter").await;
    h.type_text("http://quux:8080/L");
    assert!(!h.quit);
    assert_snapshot("config_editing", &h.screen());
    h.press("enter").await;
    let puts = mock.calls_matching("PUT", "/v0/management/proxy-url");
    assert_eq!(puts[0].body, r#"{"value":"http://quux:8080/L"}"#);
    assert_eq!(i18n::current_locale(), "en");

    // Request Retry: non-numeric input is rejected locally.
    h.key("j");
    h.press("enter").await;
    h.key("ctrl+u");
    h.type_text("abc");
    h.press("enter").await;
    assert!(h.screen().contains("✗ invalid integer: abc"));
    assert!(mock.calls_matching("PUT", "/v0/management/request-retry").is_empty());
}

#[tokio::test]
async fn auth_files_tab_expand_confirm_toggle_and_edit() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    goto_tab(&mut h, 2).await;
    assert_snapshot("auth_files", &h.screen());

    h.press("enter").await;
    assert_snapshot("auth_files_expanded", &h.screen());

    h.key("d");
    assert!(h.screen().contains("⚠ Delete claude-test@example.com.json? [y/n]"));
    h.press("n").await;
    assert!(mock.calls_matching("DELETE", "/v0/management/auth-files").is_empty());
    h.key("d");
    h.press("y").await;
    assert_eq!(
        mock.calls_matching("DELETE", "/v0/management/auth-files")[0].path,
        "/v0/management/auth-files?name=claude-test%40example.com.json"
    );

    h.press("e").await;
    let patches = mock.calls_matching("PATCH", "/v0/management/auth-files/status");
    assert_eq!(patches[0].body, r#"{"disabled":true,"name":"claude-test@example.com.json"}"#);

    h.key("3");
    assert!(h.screen().contains("Priority: 5"));
    h.key("backspace");
    h.type_text("9");
    h.press("enter").await;
    let fields = mock.calls_matching("PATCH", "/v0/management/auth-files/fields");
    assert_eq!(fields[0].body, r#"{"name":"claude-test@example.com.json","priority":9}"#);

    h.key("3");
    h.key("backspace");
    h.type_text("x");
    h.key("enter");
    // The status shows until the follow-up refresh replaces the list (as in Go).
    h.drain();
    assert!(h.screen().contains("✗ invalid integer: x"));
}

#[tokio::test]
async fn api_keys_tab_lists_adds_edits_and_deletes() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    goto_tab(&mut h, 3).await;
    assert_snapshot("api_keys", &h.screen());

    h.key("a");
    h.type_text("sk-new-quux");
    assert_snapshot("api_keys_adding", &h.screen());
    h.press("enter").await;
    let patches = mock.calls_matching("PATCH", "/v0/management/api-keys");
    let body: Value = serde_json::from_str(&patches[0].body).unwrap();
    assert_eq!(body["new"], json!("sk-new-quux"));
    assert!(h.screen().contains("✓ API Key added"));
    assert!(h.screen().contains("3. sk-n***quux") || h.screen().contains("3. sk-n"));

    h.key("e");
    h.key("ctrl+u");
    h.type_text("replaced-key-value");
    h.press("enter").await;
    let patches = mock.calls_matching("PATCH", "/v0/management/api-keys");
    assert_eq!(patches[1].body, r#"{"index":0,"value":"replaced-key-value"}"#);

    h.key("d");
    assert!(h.screen().contains("⚠ Delete "));
    h.press("y").await;
    assert_eq!(mock.calls_matching("DELETE", "/v0/management/api-keys")[0].path, "/v0/management/api-keys?index=0");
}

#[tokio::test]
async fn oauth_tab_idle_web_flow_and_cancel() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    goto_tab(&mut h, 4).await;
    assert_snapshot("oauth_idle", &h.screen());

    h.press("enter").await;
    let starts = mock.calls_matching("GET", "/v0/management/anthropic-auth-url");
    assert_eq!(starts[0].path, "/v0/management/anthropic-auth-url?is_webui=true");
    assert_snapshot("oauth_remote", &h.screen());

    // Type a callback URL containing q/L, then submit it.
    h.type_text("http://localhost/cb?code=q&state=L");
    h.press("enter").await;
    let posts = mock.calls.lock().iter().filter(|c| c.method == "POST").cloned().collect::<Vec<_>>();
    assert_eq!(posts[0].path, "/v0/management/oauth-callback");
    assert_eq!(
        posts[0].body,
        r#"{"provider":"anthropic","redirect_url":"http://localhost/cb?code=q&state=L","state":"st-1"}"#
    );

    h.press("esc").await;
    assert_eq!(mock.calls_matching("DELETE", "/v0/management/oauth-session")[0].path, "/v0/management/oauth-session?state=st-1");
    assert!(h.screen().contains("Select a provider"));
}

#[tokio::test]
async fn oauth_device_flow_shows_user_code() {
    let mock = Mock::start().await;
    mock.state.lock().oauth_start = json!({
        "url": "https://device.example.com/verify", "state": "dev-1", "user_code": "ABCD-1234", "expires_in": 600
    });
    let mut h = standalone(&mock).await;
    goto_tab(&mut h, 4).await;
    h.key("j");
    h.key("j");
    h.key("j");
    h.press("enter").await;
    assert_snapshot("oauth_device", &h.screen());
}

#[tokio::test]
async fn logs_tab_filters_and_clear() {
    let mock = Mock::start().await;
    let hook = LogHook::new(100);
    let mut h = Harness::standalone(&mock.url(), "mgmt-secret", hook.clone());
    h.start().await;
    for line in [
        "[2026-10-02 10:00:00] [--------] [info ] [a.rs:1] started",
        "[2026-10-02 10:00:01] [--------] [debug] [a.rs:2] detail",
        "[2026-10-02 10:00:02] [--------] [warn ] [a.rs:3] slow",
        "[2026-10-02 10:00:03] [--------] [error] [a.rs:4] failed",
    ] {
        hook.push(line);
        h.settle().await;
    }
    goto_tab(&mut h, 5).await;
    assert_snapshot("logs_all", &h.screen());

    // `q` stays on the Logs tab instead of quitting.
    h.press("q").await;
    assert!(!h.quit);

    h.press("3").await;
    assert_snapshot("logs_warn", &h.screen());
    h.press("c").await;
    assert!(h.screen().contains("Waiting for log output..."));
}

#[tokio::test]
async fn client_mode_polls_server_logs_when_enabled() {
    let mock = Mock::start().await;
    let mut h = Harness::client_mode(&mock.url(), "mgmt-secret");
    h.start().await;
    h.press("enter").await;
    goto_tab(&mut h, 5).await;
    assert_snapshot("logs_polled", &h.screen());
    let polls = mock.calls_matching("GET", "/v0/management/logs");
    assert_eq!(polls[0].path, "/v0/management/logs?limit=200");
}

#[tokio::test]
async fn logs_tab_hidden_when_logging_to_file_disabled() {
    let mock = Mock::start().await;
    mock.state.lock().config["logging-to-file"] = json!(false);
    let mut h = Harness::client_mode(&mock.url(), "mgmt-secret");
    h.start().await;
    h.press("enter").await;
    assert_eq!(h.app.tabs, vec!["Dashboard", "Config", "Auth Files", "API Keys", "OAuth"]);
    assert!(!h.screen().lines().next().unwrap_or("").contains("Logs"));
}

#[tokio::test]
async fn quit_keys_and_tab_cycling() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    h.key("shift+tab");
    assert_eq!(h.app.active_tab, 5, "shift+tab wraps to Logs");
    h.key("tab");
    assert_eq!(h.app.active_tab, 0);
    h.key("ctrl+c");
    assert!(h.quit);

    let mut h = standalone(&mock).await;
    h.key("q");
    assert!(h.quit);
}

#[tokio::test]
async fn screen_is_true_black_with_white_text() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    let buf = h.buffer();
    for y in 0..buf.area.height {
        let mut skip = 0;
        for x in 0..buf.area.width {
            if skip > 0 {
                // Continuation cell of a wide glyph; never painted on its own.
                skip -= 1;
                continue;
            }
            let cell = &buf[(x, y)];
            skip = unicode_width::UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
            assert_eq!(cell.bg, Color::Rgb(0, 0, 0), "background must be true black at ({x},{y})");
        }
    }
    let title = &buf[(0, 1)];
    assert_eq!(title.symbol(), "📊");
    assert_eq!(buf[(3, 1)].fg, Color::Rgb(255, 255, 255));
}

#[tokio::test]
async fn small_terminal_does_not_panic() {
    let mock = Mock::start().await;
    let mut h = standalone(&mock).await;
    for (w, hgt) in [(1, 1), (10, 3), (20, 2), (40, 5)] {
        h.w = w;
        h.h = hgt;
        h.feed(cpa_tui::Msg::Resize(w, hgt));
        for tab in 0..6 {
            h.app.active_tab = tab;
            let _ = h.screen();
        }
    }
}
