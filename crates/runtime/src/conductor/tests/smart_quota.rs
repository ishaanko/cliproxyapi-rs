//! Smart-quota selection through the manager. The failure list this file answers is at the end of
//! `tests.rs` (written before the implementation); signal parsing and score math are covered by
//! the unit tests of `quota_windows.rs`.

use std::collections::BTreeMap;

use cpa_auth::types::{ModelState, QuotaState};

use super::*;

fn sigs(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

fn quota(signals: BTreeMap<String, String>) -> QuotaState {
    QuotaState {
        signals,
        observed_at: Some(t0()),
        ..Default::default()
    }
}

/// Claude signals with `used` percent of the 5h window spent.
fn five_hour(used: f64) -> QuotaState {
    quota(sigs(&[(
        "Anthropic-Ratelimit-Unified-5h-Utilization",
        format!("{}", used / 100.0),
    )]))
}

/// Codex-style integer percent (exact in floating point) in a 5h window.
fn codex_five_hour(used: u32) -> QuotaState {
    quota(sigs(&[
        ("X-Codex-Primary-Used-Percent", used.to_string()),
        ("X-Codex-Primary-Window-Minutes", "300".into()),
    ]))
}

/// Claude 5h and weekly windows; the weekly one resets `weekly_reset_in` after t0.
fn with_weekly(used_5h: f64, used_week: f64, weekly_reset_in: Option<Duration>) -> QuotaState {
    let mut s = sigs(&[
        (
            "Anthropic-Ratelimit-Unified-5h-Utilization",
            format!("{}", used_5h / 100.0),
        ),
        (
            "Anthropic-Ratelimit-Unified-7d-Utilization",
            format!("{}", used_week / 100.0),
        ),
    ]);
    if let Some(d) = weekly_reset_in {
        s.insert(
            "Anthropic-Ratelimit-Unified-7d-Reset".into(),
            (t0().timestamp() + d.as_secs() as i64).to_string(),
        );
    }
    quota(s)
}

/// Quota signals are only retained for providers that support observation (a successful
/// response without them keeps the old snapshot), so the tests run as a Claude provider.
const PROVIDER: &str = "claude";

async fn pay(h: &Harness, model: &str) -> String {
    payload_of(
        h.mgr
            .execute(
                &[PROVIDER.to_string()],
                request(model),
                Options::new(Format::OpenAI),
            )
            .await,
    )
}

async fn run_session(h: &Harness, body: &'static str) -> Result<Response, ExecError> {
    h.mgr
        .execute(&[PROVIDER.to_string()], request("m"), opts_with_body(body))
        .await
}

async fn open_stream(h: &Harness) -> StreamResult {
    h.mgr
        .execute_stream(
            &[PROVIDER.to_string()],
            request("m"),
            Options::new(Format::OpenAI),
        )
        .await
        .unwrap()
}

fn harness(reserve: Option<i64>) -> Harness {
    let h = Harness::with_executor(PROVIDER);
    h.config(|c| {
        c.routing.strategy = "smart-quota".into();
        c.routing.smart_quota_reserve_percent = reserve;
    });
    h
}

async fn add_with(h: &Harness, id: &str, q: Option<QuotaState>) {
    h.add(id, &["m"], |a| {
        if let Some(q) = q {
            a.quota = q;
        }
    })
    .await;
}

async fn picks(h: &Harness, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(pay(h, "m").await);
    }
    out
}

fn payload_of(res: Result<Response, ExecError>) -> String {
    String::from_utf8(res.unwrap().payload.to_vec()).unwrap()
}

const DAY: Duration = Duration::from_secs(86_400);

#[tokio::test]
async fn reserve_boundary_is_inclusive() {
    // a has 29% headroom but a weekly bonus (weight ~54), b exactly 30% (weight 30). With the
    // default 30% reserve only b is eligible; if "exactly at reserve" were excluded nobody would
    // be and a would win.
    let h = harness(None);
    add_with(&h, "a", Some(with_weekly(71.0, 0.0, Some(DAY)))).await;
    add_with(&h, "b", Some(codex_five_hour(70))).await;
    assert_eq!(picks(&h, 3).await, ["b", "b", "b"]);
}

#[tokio::test]
async fn all_below_reserve_falls_back_to_everyone() {
    // Both under 30% headroom: nothing is refused and the weights alone decide (b's weekly
    // bonus lifts 15 above a's 20).
    let h = harness(None);
    add_with(&h, "a", Some(five_hour(80.0))).await;
    add_with(&h, "b", Some(with_weekly(85.0, 0.0, Some(DAY)))).await;
    assert_eq!(picks(&h, 2).await, ["b", "b"]);
}

#[tokio::test]
async fn reserve_filter_overrides_a_higher_weight_and_zero_disables_it() {
    // a: 50% headroom (weight 50). b: 28% headroom plus a weekly bonus (weight ~56), below the
    // default 30% reserve. The reserve keeps b out; with reserve 0 the weights decide.
    let build = |reserve| async move {
        let h = harness(reserve);
        add_with(&h, "a", Some(five_hour(50.0))).await;
        add_with(&h, "b", Some(with_weekly(72.0, 0.0, Some(Duration::from_secs(3600))))).await;
        h
    };
    assert_eq!(picks(&build(None).await, 3).await, ["a", "a", "a"]);
    assert_eq!(picks(&build(Some(0)).await, 3).await, ["b", "b", "b"]);
}

#[tokio::test]
async fn reserve_value_is_clamped_and_defaults_to_30() {
    let h = Harness::new();
    for (raw, want) in [(Some(500), 100), (Some(-3), 0), (None, 30), (Some(45), 45)] {
        h.config(|c| {
            c.routing.strategy = "sq".into();
            c.routing.smart_quota_reserve_percent = raw;
        });
        assert_eq!(h.mgr.selector().config.smart_quota_reserve, want);
    }
}

#[tokio::test]
async fn unknown_data_scores_50_and_ties_rotate() {
    let h = harness(Some(0));
    for id in ["a", "b", "c"] {
        add_with(&h, id, None).await;
    }
    assert_eq!(picks(&h, 6).await, ["a", "b", "c", "a", "b", "c"]);

    // Known 49% headroom loses to the unknown credentials (50), which rotate.
    let h = harness(Some(0));
    add_with(&h, "a", None).await;
    add_with(&h, "b", None).await;
    add_with(&h, "c", Some(five_hour(51.0))).await;
    assert_eq!(picks(&h, 4).await, ["a", "b", "a", "b"]);
    // Known 51% headroom beats them.
    let h = harness(Some(0));
    add_with(&h, "a", None).await;
    add_with(&h, "b", None).await;
    add_with(&h, "c", Some(five_hour(49.0))).await;
    assert_eq!(picks(&h, 4).await, ["c", "c", "c", "c"]);
}

#[tokio::test]
async fn reserve_excludes_unknown_when_someone_has_known_headroom() {
    let h = harness(None);
    add_with(&h, "a", None).await;
    add_with(&h, "b", Some(five_hour(40.0))).await;
    assert_eq!(picks(&h, 3).await, ["b", "b", "b"]);
}

#[tokio::test]
async fn expired_windows_are_ignored() {
    // a's window reset in the past: the allowance is back, so it scores as unknown (50) rather
    // than full (0).
    let h = harness(Some(0));
    let mut expired = five_hour(100.0);
    expired.signals.insert(
        "Anthropic-Ratelimit-Unified-5h-Reset".into(),
        (t0().timestamp() - 10).to_string(),
    );
    add_with(&h, "a", Some(expired)).await;
    add_with(&h, "b", Some(five_hour(60.0))).await;
    assert_eq!(picks(&h, 2).await, ["a", "a"]);
}

#[tokio::test]
async fn claude_rejected_status_counts_as_full() {
    let h = harness(Some(0));
    let mut rejected = five_hour(10.0);
    rejected.signals.insert(
        "Anthropic-Ratelimit-Unified-5h-Status".into(),
        "rejected".into(),
    );
    add_with(&h, "a", Some(rejected)).await;
    add_with(&h, "b", Some(five_hour(90.0))).await;
    assert_eq!(picks(&h, 2).await, ["b", "b"]);
}

#[tokio::test]
async fn codex_reset_after_is_measured_from_observed_at() {
    // Observed 1000s ago with a 600s reset-after: the window is already over, so a scores as
    // unknown (50) and beats b (40). Measured from now it would still look full.
    let h = harness(Some(0));
    let stale = QuotaState {
        observed_at: Some(t0() - chrono::Duration::seconds(1000)),
        signals: sigs(&[
            ("X-Codex-Primary-Used-Percent", "100".into()),
            ("X-Codex-Primary-Window-Minutes", "300".into()),
            ("X-Codex-Primary-Reset-After-Seconds", "600".into()),
        ]),
        ..Default::default()
    };
    add_with(&h, "a", Some(stale)).await;
    add_with(&h, "b", Some(codex_five_hour(60))).await;
    assert_eq!(picks(&h, 2).await, ["a", "a"]);
}

#[tokio::test]
async fn earlier_weekly_renewal_wins_at_equal_headroom() {
    let h = harness(None);
    add_with(&h, "a", Some(with_weekly(50.0, 10.0, Some(6 * DAY)))).await;
    add_with(&h, "b", Some(with_weekly(50.0, 10.0, Some(DAY)))).await;
    add_with(&h, "c", Some(with_weekly(50.0, 10.0, Some(3 * DAY)))).await;
    assert_eq!(picks(&h, 3).await, ["b", "b", "b"]);
}

#[tokio::test]
async fn nearly_spent_week_is_avoided() {
    // a: 60% headroom but 95% of the week gone (fade 0.2 -> 12); b: 40%.
    let h = harness(None);
    add_with(&h, "a", Some(with_weekly(40.0, 95.0, None))).await;
    add_with(&h, "b", Some(five_hour(60.0))).await;
    assert_eq!(picks(&h, 3).await, ["b", "b", "b"]);
}

#[tokio::test]
async fn in_flight_load_spreads_simultaneous_picks_and_is_released() {
    let h = harness(None);
    add_with(&h, "a", Some(five_hour(0.0))).await; // 100
    add_with(&h, "b", Some(five_hour(40.0))).await; // 60
    // Without load a always wins.
    assert_eq!(picks(&h, 2).await, ["a", "a"]);
    // A held stream on a halves its weight (50 < 60): the next requests go to b.
    h.exec.script("a", vec![Step::Idle("x")]);
    let held = open_stream(&h).await;
    assert_eq!(h.mgr.in_flight.load("a"), 1);
    assert_eq!(picks(&h, 2).await, ["b", "b"]);
    // Dropping the stream releases the slot and a is preferred again.
    drop(held);
    assert_eq!(h.mgr.in_flight.load("a"), 0);
    assert_eq!(picks(&h, 2).await, ["a", "a"]);
    // Failover after a failed attempt leaves no slot behind on either credential.
    h.exec.script("a", vec![Step::Err(status_err(500, "down"))]);
    assert_eq!(pay(&h, "m").await, "b");
    assert_eq!(
        (h.mgr.in_flight.load("a"), h.mgr.in_flight.load("b")),
        (0, 0)
    );
}

#[tokio::test]
async fn per_model_signals_override_auth_level() {
    let h = harness(Some(0));
    // a: auth-level says 10% headroom, but model m's own state says 90%.
    h.add("a", &["m", "n"], |a| {
        a.quota = five_hour(90.0);
        a.model_states.insert(
            "m".into(),
            ModelState {
                quota: five_hour(10.0),
                ..Default::default()
            },
        );
        // An empty per-model state must not shadow the auth-level signals.
        a.model_states.insert("n".into(), ModelState::default());
    })
    .await;
    h.add("b", &["m", "n"], |a| a.quota = five_hour(50.0)).await;
    assert_eq!(pay(&h, "m").await, "a");
    assert_eq!(pay(&h, "n").await, "b");
}

#[tokio::test]
async fn affinity_bound_sessions_do_not_move_and_rebind_by_score() {
    let h = Harness::with_executor(PROVIDER);
    h.config(|c| {
        c.routing.strategy = "smart-quota".into();
        c.routing.session_affinity = true;
    });
    add_with(&h, "a", Some(five_hour(0.0))).await; // 100
    add_with(&h, "b", Some(five_hour(70.0))).await; // 30
    add_with(&h, "c", Some(five_hour(10.0))).await; // 90
    let s1 = r#"{"prompt_cache_key":"s1"}"#;
    assert_eq!(payload_of(run_session(&h, s1).await), "a");
    // Load on a makes it the worst choice for new traffic (50 < 90)...
    h.exec.script("a", vec![Step::Idle("x")]);
    let held = open_stream(&h).await;
    assert_eq!(pay(&h, "m").await, "c");
    // ...but the bound session stays on a.
    for _ in 0..3 {
        assert_eq!(payload_of(run_session(&h, s1).await), "a");
    }
    drop(held);
    // A new session is placed by score.
    let s2 = r#"{"prompt_cache_key":"s2"}"#;
    assert_eq!(payload_of(run_session(&h, s2).await), "a");
    // When a is unavailable the session rebinds to the best remaining (c, not the round-robin
    // successor b) and sticks there.
    mark_failure(&h, "a", "m", 429, Duration::from_secs(600));
    assert_eq!(payload_of(run_session(&h, s1).await), "c");
    assert_eq!(payload_of(run_session(&h, s1).await), "c");
}

#[tokio::test]
async fn mixed_providers_compete_on_score() {
    let h = harness(None);
    let other = Mock::new("codex");
    h.mgr.register_executor(other.clone());
    add_with(&h, "a", Some(five_hour(80.0))).await;
    let mut c = Auth::new("c", "codex");
    c.quota = five_hour(10.0);
    h.registry.register_client(
        "c",
        "codex",
        &[ModelInfo {
            id: "m".into(),
            ..Default::default()
        }],
    );
    h.mgr.register(c).await.unwrap();
    for _ in 0..3 {
        let res = h
            .mgr
            .execute(
                &[PROVIDER.to_string(), "codex".to_string()],
                request("m"),
                Options::new(Format::OpenAI),
            )
            .await;
        assert_eq!(payload_of(res), "c");
    }
    assert_eq!(other.count("c"), 3);
    assert_eq!(h.exec.count("a"), 0);
}

#[test]
fn strategy_aliases_parse() {
    for s in ["smart-quota", "SmartQuota", " SQ "] {
        assert_eq!(Strategy::parse(s), Strategy::SmartQuota, "{s}");
    }
    assert_eq!(Strategy::parse("smart"), Strategy::RoundRobin);
}

#[tokio::test]
async fn reserve_change_rebuilds_selector_only_for_smart_quota() {
    let h = Harness::new();
    h.config(|c| c.routing.strategy = "fill-first".into());
    let before = h.mgr.selector();
    h.config(|c| {
        c.routing.strategy = "fill-first".into();
        c.routing.smart_quota_reserve_percent = Some(80);
    });
    assert!(Arc::ptr_eq(&before, &h.mgr.selector()));
    h.config(|c| {
        c.routing.strategy = "smart-quota".into();
        c.routing.smart_quota_reserve_percent = Some(80);
    });
    let sq = h.mgr.selector();
    assert_eq!(sq.config.smart_quota_reserve, 80);
    h.config(|c| {
        c.routing.strategy = "smart-quota".into();
        c.routing.smart_quota_reserve_percent = Some(20);
    });
    assert!(!Arc::ptr_eq(&sq, &h.mgr.selector()));
    assert_eq!(h.mgr.selector().config.smart_quota_reserve, 20);
}
