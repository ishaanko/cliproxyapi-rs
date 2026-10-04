//! Usage windows and the `smart-quota` score (Rust-only extension; Go has no such strategy).
//!
//! The windows come from the passive quota observations the conductor already stores per
//! credential ([`cpa_auth::types::QuotaState::signals`], Go-canonical header names compared
//! case-insensitively): Claude's `anthropic-ratelimit-unified-{5h,7d}-*` and Codex's
//! `x-codex-{primary,secondary}-*`. Providers without such signals simply have no windows.
//!
//! [`windows_for_auth`] reads them, [`score`] turns them plus the in-flight load into the weight
//! the selector maximises (see `Selector::pick_smart`).

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use cpa_auth::Auth;

use super::util::canonical_model_key;

/// Headroom assumed for a credential without any usage data.
const UNKNOWN_HEADROOM: f64 = 50.0;
/// Seconds in the weekly allowance cycle used for the renewal bonus.
const WEEK_SECS: f64 = 7.0 * 86_400.0;
/// Remaining weekly percent below which a credential is progressively avoided.
const WEEKLY_FADE_PERCENT: f64 = 25.0;
/// Codex windows of at most this many minutes count as the 5h window.
const FIVE_HOUR_MAX_MINUTES: f64 = 360.0;
/// Codex windows of at least this many minutes (6 days) count as the weekly window.
const WEEKLY_MIN_MINUTES: f64 = 8640.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    FiveHour,
    Weekly,
    Other,
}

/// One rate-limit window as last observed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub class: WindowClass,
    /// 0..=100.
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// The selector-facing result for one candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub weight: f64,
    /// Percent left in the 5h window when one is known (drives the reserve rule).
    pub five_hour_headroom: Option<f64>,
}

impl Score {
    /// No data, no load: what a credential the selector knows nothing about scores.
    pub const UNKNOWN: Score = Score {
        weight: UNKNOWN_HEADROOM,
        five_hour_headroom: None,
    };
}

/// Case-insensitive view of the signals.
struct Signals<'a>(BTreeMap<String, &'a str>);

impl<'a> Signals<'a> {
    fn new(signals: &'a BTreeMap<String, String>) -> Self {
        Signals(
            signals
                .iter()
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim()))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&'a str> {
        self.0.get(key).copied()
    }

    fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse::<f64>().ok().filter(|v| v.is_finite())
    }

    fn unix_time(&self, key: &str) -> Option<DateTime<Utc>> {
        let secs = self.number(key)?;
        DateTime::from_timestamp(secs as i64, 0)
    }
}

fn clamp_percent(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

/// Windows described by `signals`, minus those whose reset already passed at `now` (the allowance
/// is back, the old reading is stale). `observed_at` anchors Codex's relative reset.
pub fn windows_from_signals(
    signals: &BTreeMap<String, String>,
    observed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<Window> {
    let sig = Signals::new(signals);
    let mut out = Vec::new();

    for (tag, class) in [("5h", WindowClass::FiveHour), ("7d", WindowClass::Weekly)] {
        let base = format!("anthropic-ratelimit-unified-{tag}");
        let rejected = sig
            .get(&format!("{base}-status"))
            .is_some_and(|s| s.eq_ignore_ascii_case("rejected"));
        let used = if rejected {
            Some(100.0)
        } else {
            sig.number(&format!("{base}-utilization")).map(|u| u * 100.0)
        };
        if let Some(used) = used {
            out.push(Window {
                class,
                used_percent: clamp_percent(used),
                resets_at: sig.unix_time(&format!("{base}-reset")),
            });
        }
    }

    let limit_reached = sig
        .get("x-codex-limit-reached")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    for tag in ["primary", "secondary"] {
        let base = format!("x-codex-{tag}");
        let used = if tag == "primary" && limit_reached {
            Some(100.0)
        } else {
            sig.number(&format!("{base}-used-percent"))
        };
        let Some(used) = used else { continue };
        let class = match sig.number(&format!("{base}-window-minutes")) {
            Some(m) if m <= FIVE_HOUR_MAX_MINUTES => WindowClass::FiveHour,
            Some(m) if m >= WEEKLY_MIN_MINUTES => WindowClass::Weekly,
            _ => WindowClass::Other,
        };
        let resets_at = sig.unix_time(&format!("{base}-reset-at")).or_else(|| {
            let after = sig.number(&format!("{base}-reset-after-seconds"))?;
            Some(observed_at? + Duration::milliseconds((after * 1000.0) as i64))
        });
        out.push(Window {
            class,
            used_percent: clamp_percent(used),
            resets_at,
        });
    }

    out.retain(|w| w.resets_at.is_none_or(|r| r > now));
    out
}

/// Windows of `auth` for the model with canonical key `model_key`: the per-model quota state when
/// it carries signals (newest observation wins among aliases), else the credential-level one.
pub fn windows_for_auth(auth: &Auth, model_key: &str, now: DateTime<Utc>) -> Vec<Window> {
    let per_model = auth
        .model_states
        .iter()
        .filter(|(m, s)| !s.quota.signals.is_empty() && canonical_model_key(m) == model_key)
        .map(|(_, s)| &s.quota)
        .max_by_key(|q| q.observed_at);
    let q = per_model.unwrap_or(&auth.quota);
    windows_from_signals(&q.signals, q.observed_at, now)
}

/// The most used window of a class.
fn fullest(windows: &[Window], class: WindowClass) -> Option<&Window> {
    windows
        .iter()
        .filter(|w| w.class == class)
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
}

/// `headroom * renewal * weekly_fade / (1 + load)`:
/// - headroom: percent left in the 5h window, else in the fullest known window, else 50,
/// - renewal: 2 down to 1 as the weekly reset moves from now to 7+ days away (allowance about to
///   expire unused goes first); 1 when unknown,
/// - weekly_fade: below 25% of the week left the weight shrinks linearly; 1 when unknown.
pub fn score(windows: &[Window], now: DateTime<Utc>, load: usize) -> Score {
    let five_hour = fullest(windows, WindowClass::FiveHour);
    let weekly = fullest(windows, WindowClass::Weekly);
    let five_hour_headroom = five_hour.map(|w| 100.0 - w.used_percent);
    let headroom = five_hour_headroom
        .or_else(|| {
            windows
                .iter()
                .map(|w| w.used_percent)
                .max_by(f64::total_cmp)
                .map(|used| 100.0 - used)
        })
        .unwrap_or(UNKNOWN_HEADROOM);
    let renewal = weekly
        .and_then(|w| w.resets_at)
        .map_or(1.0, |reset| {
            let left = (reset - now).num_milliseconds() as f64 / 1000.0;
            2.0 - (left / WEEK_SECS).clamp(0.0, 1.0)
        });
    let weekly_fade = weekly.map_or(1.0, |w| {
        ((100.0 - w.used_percent) / WEEKLY_FADE_PERCENT).min(1.0)
    });
    Score {
        weight: headroom * renewal * weekly_fade / (1.0 + load as f64),
        five_hour_headroom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn sigs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn at(offset_secs: i64) -> i64 {
        now().timestamp() + offset_secs
    }

    #[test]
    fn claude_utilization_and_reset_parse() {
        let reset = at(3600).to_string();
        let w = windows_from_signals(
            &sigs(&[
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.9"),
                ("Anthropic-Ratelimit-Unified-5h-Reset", &reset),
                ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.25"),
            ]),
            None,
            now(),
        );
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].class, WindowClass::FiveHour);
        assert!((w[0].used_percent - 90.0).abs() < 1e-9);
        assert_eq!(w[0].resets_at.map(|t| t.timestamp()), Some(at(3600)));
        assert_eq!(w[1].class, WindowClass::Weekly);
        assert!((w[1].used_percent - 25.0).abs() < 1e-9);
        assert_eq!(w[1].resets_at, None);
    }

    #[test]
    fn claude_rejected_status_means_full() {
        let w = windows_from_signals(
            &sigs(&[
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.2"),
                ("Anthropic-Ratelimit-Unified-5h-Status", "rejected"),
            ]),
            None,
            now(),
        );
        assert_eq!(score(&w, now(), 0).five_hour_headroom, Some(0.0));
    }

    #[test]
    fn codex_window_classes() {
        let class_of = |minutes: &str| {
            windows_from_signals(
                &sigs(&[
                    ("X-Codex-Primary-Used-Percent", "10"),
                    ("X-Codex-Primary-Window-Minutes", minutes),
                ]),
                None,
                now(),
            )[0]
            .class
        };
        assert_eq!(class_of("300"), WindowClass::FiveHour);
        assert_eq!(class_of("360"), WindowClass::FiveHour);
        assert_eq!(class_of("361"), WindowClass::Other);
        assert_eq!(class_of("8639"), WindowClass::Other);
        assert_eq!(class_of("8640"), WindowClass::Weekly);
        assert_eq!(class_of("10080"), WindowClass::Weekly);
        let unknown = windows_from_signals(
            &sigs(&[("X-Codex-Primary-Used-Percent", "10")]),
            None,
            now(),
        );
        assert_eq!(unknown[0].class, WindowClass::Other);
    }

    #[test]
    fn codex_reset_after_is_relative_to_observed_at() {
        let observed = now() - Duration::seconds(1000);
        let signals = sigs(&[
            ("X-Codex-Primary-Used-Percent", "80"),
            ("X-Codex-Primary-Window-Minutes", "300"),
            ("X-Codex-Primary-Reset-After-Seconds", "1500"),
        ]);
        // Resets 500s from now: still counted.
        let w = windows_from_signals(&signals, Some(observed), now());
        assert_eq!(w[0].resets_at, Some(observed + Duration::seconds(1500)));
        // Measured from `now` it would still be live; from observed_at it has expired by +600s.
        let later = now() + Duration::seconds(600);
        assert!(windows_from_signals(&signals, Some(observed), later).is_empty());
        // An absolute reset-at wins over reset-after.
        let mut both = signals.clone();
        both.insert("X-Codex-Primary-Reset-At".into(), at(10).to_string());
        let w = windows_from_signals(&both, Some(observed), now());
        assert_eq!(w[0].resets_at.map(|t| t.timestamp()), Some(at(10)));
    }

    #[test]
    fn codex_limit_reached_fills_primary() {
        let w = windows_from_signals(
            &sigs(&[
                ("X-Codex-Primary-Used-Percent", "20"),
                ("X-Codex-Primary-Window-Minutes", "300"),
                ("X-Codex-Limit-Reached", "true"),
            ]),
            None,
            now(),
        );
        assert_eq!(score(&w, now(), 0).five_hour_headroom, Some(0.0));
    }

    #[test]
    fn expired_windows_ignored() {
        let past = at(-1).to_string();
        let w = windows_from_signals(
            &sigs(&[
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "1"),
                ("Anthropic-Ratelimit-Unified-5h-Reset", &past),
            ]),
            None,
            now(),
        );
        assert!(w.is_empty());
        assert_eq!(score(&w, now(), 0), Score::UNKNOWN);
    }

    #[test]
    fn garbage_values_are_clamped() {
        let w = windows_from_signals(
            &sigs(&[
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "7"),
                ("Anthropic-Ratelimit-Unified-7d-Utilization", "nan"),
                ("X-Codex-Primary-Used-Percent", "-5"),
                ("X-Codex-Primary-Window-Minutes", "300"),
                ("X-Codex-Secondary-Used-Percent", "abc"),
            ]),
            None,
            now(),
        );
        // 5h clamps to 100, codex primary to 0; the unparsable ones are dropped.
        assert_eq!(w.len(), 2);
        let s = score(&w, now(), 0);
        assert!(s.weight.is_finite());
        // Both are 5h windows: the fullest decides.
        assert_eq!(s.five_hour_headroom, Some(0.0));
    }

    #[test]
    fn headroom_fallback() {
        // No 5h window: headroom is what the fullest known window leaves.
        let w = windows_from_signals(
            &sigs(&[
                ("X-Codex-Primary-Used-Percent", "30"),
                ("X-Codex-Primary-Window-Minutes", "1000"),
                ("X-Codex-Secondary-Used-Percent", "60"),
                ("X-Codex-Secondary-Window-Minutes", "2000"),
            ]),
            None,
            now(),
        );
        let s = score(&w, now(), 0);
        assert_eq!(s.five_hour_headroom, None);
        assert!((s.weight - 40.0).abs() < 1e-9);
    }

    #[test]
    fn renewal_prefers_the_sooner_weekly_reset() {
        let weekly = |reset_in: i64| {
            vec![
                Window {
                    class: WindowClass::FiveHour,
                    used_percent: 50.0,
                    resets_at: None,
                },
                Window {
                    class: WindowClass::Weekly,
                    used_percent: 10.0,
                    resets_at: Some(now() + Duration::seconds(reset_in)),
                },
            ]
        };
        let soon = score(&weekly(86_400), now(), 0).weight;
        let late = score(&weekly(6 * 86_400), now(), 0).weight;
        let beyond = score(&weekly(30 * 86_400), now(), 0).weight;
        assert!(soon > late && late > beyond);
        // Reset now: renewal 2; reset a week or more away: renewal 1 (headroom 50, fade 1).
        assert!((score(&weekly(0), now(), 0).weight - 100.0).abs() < 1e-6);
        assert!((beyond - 50.0).abs() < 1e-9);
    }

    #[test]
    fn weekly_fade_penalizes_a_nearly_spent_week() {
        let with_weekly_used = |used: f64| {
            score(
                &[
                    Window {
                        class: WindowClass::FiveHour,
                        used_percent: 0.0,
                        resets_at: None,
                    },
                    Window {
                        class: WindowClass::Weekly,
                        used_percent: used,
                        resets_at: None,
                    },
                ],
                now(),
                0,
            )
            .weight
        };
        assert!((with_weekly_used(75.0) - 100.0).abs() < 1e-9);
        assert!((with_weekly_used(90.0) - 40.0).abs() < 1e-9);
        assert!((with_weekly_used(100.0)).abs() < 1e-9);
    }

    #[test]
    fn load_divides_the_weight() {
        let w = [Window {
            class: WindowClass::FiveHour,
            used_percent: 0.0,
            resets_at: None,
        }];
        assert!((score(&w, now(), 3).weight - 25.0).abs() < 1e-9);
    }
}
