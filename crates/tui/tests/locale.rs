//! Locale switching mutates process-wide state, so it lives in its own test binary.

mod common;

use common::{Harness, Mock, assert_snapshot};
use cpa_tui::{LogHook, i18n};

#[tokio::test]
async fn locale_toggle_switches_every_screen() {
    let mock = Mock::start().await;
    let mut h = Harness::standalone(&mock.url(), "mgmt-secret", LogHook::new(10));
    h.start().await;
    h.press("L").await;
    assert_snapshot("dashboard_zh", &h.screen());
    h.press("L").await;
    assert!(h.screen().contains("Dashboard"));
    assert_eq!(i18n::current_locale(), "en");
}

