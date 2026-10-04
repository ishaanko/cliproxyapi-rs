//! Local calendar date in an IANA timezone, for the Claude Code current-date reminder
//! (Go: claudeCodeCurrentTime / claudeCodeTimezone with `time.LoadLocation`).
//!
//! The workspace has no timezone database crate, so named zones are read from the system TZif
//! files (`/usr/share/zoneinfo`), including the POSIX footer rule that slim files use for dates
//! after the last explicit transition. Unknown zones fall back like Go does.

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use cpa_auth::Auth;
use cpa_config::Config;

/// Timezone name on the credential (attribute first, then credential JSON; Go: claudeCredentialTimezone).
fn claude_credential_timezone(auth: &Auth) -> String {
    if let Some(tz) = auth.attributes.get("timezone") {
        let tz = tz.trim();
        if !tz.is_empty() {
            return tz.to_string();
        }
    }
    auth.meta_str("timezone").trim().to_string()
}

/// `YYYY-MM-DD` of `now` in the credential timezone, else the configured one, else local time
/// (Go: claudeCodeLocalDate(claudeCodeCurrentTime(cfg, auth))).
pub fn claude_code_current_date(cfg: &Config, auth: &Auth) -> String {
    #[cfg(test)]
    let now = test_clock::now_for(&auth.id).unwrap_or_else(|| Utc::now().timestamp());
    #[cfg(not(test))]
    let now = Utc::now().timestamp();
    date_at(now, &claude_credential_timezone(auth), cfg.claude_header_defaults.timezone.trim())
}

/// Per-credential clock override for tests (Go: `claudeCodeCurrentTimeFunc`), keyed by auth id so
/// concurrently running tests never see each other's time.
#[cfg(test)]
pub(crate) mod test_clock {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    static CLOCKS: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(Default::default);

    /// Pins "now" (unix seconds) for requests made with credential `auth_id`.
    pub(crate) fn set(auth_id: &str, unix: i64) {
        if let Ok(mut clocks) = CLOCKS.lock() {
            clocks.insert(auth_id.to_string(), unix);
        }
    }

    pub(super) fn now_for(auth_id: &str) -> Option<i64> {
        CLOCKS.lock().ok()?.get(auth_id).copied()
    }
}

fn date_at(unix: i64, credential_tz: &str, config_tz: &str) -> String {
    for name in [credential_tz, config_tz] {
        if name.is_empty() {
            continue;
        }
        if let Some(offset) = utc_offset(name, unix) {
            return format_date(unix + offset);
        }
    }
    let local = DateTime::<Utc>::from_timestamp(unix, 0).unwrap_or_default().with_timezone(&Local);
    format!("{:04}-{:02}-{:02}", local.year(), local.month(), local.day())
}

fn format_date(local_unix: i64) -> String {
    let date = DateTime::<Utc>::from_timestamp(local_unix, 0).unwrap_or_default().date_naive();
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// UTC offset in seconds of the named zone at `unix`; `None` when the zone cannot be loaded.
/// Go semantics: "" and "UTC" are UTC, "Local" is the process zone.
fn utc_offset(name: &str, unix: i64) -> Option<i64> {
    match name {
        "" | "UTC" => return Some(0),
        "Local" => {
            let local = DateTime::<Utc>::from_timestamp(unix, 0)?.with_timezone(&Local);
            return Some(i64::from(local.offset().local_minus_utc()));
        }
        _ => {}
    }
    if name.contains("..") || name.starts_with('/') || name.contains('\\') {
        return None;
    }
    let data = std::fs::read(format!("/usr/share/zoneinfo/{name}")).ok()?;
    tzif_offset(&data, unix)
}

struct Tzif {
    times: Vec<i64>,
    idx: Vec<u8>,
    /// (utc offset, is_dst)
    types: Vec<(i32, bool)>,
    footer: String,
}

fn be32(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)))
}

fn be64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn parse_tzif(data: &[u8]) -> Option<Tzif> {
    if data.get(..4)? != b"TZif" {
        return None;
    }
    let version = *data.get(4)?;
    let counts = |at: usize| -> Option<[usize; 6]> {
        let mut out = [0usize; 6];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = usize::try_from(be32(data, at + 20 + i * 4)?).ok()?;
        }
        Some(out)
    };
    let [isut, isstd, leap, time, typ, chars] = counts(0)?;
    let block_len = |time_size: usize, [isut, isstd, leap, time, typ, chars]: [usize; 6]| {
        time * time_size + time + typ * 6 + chars + leap * (time_size + 4) + isstd + isut
    };
    let (header_at, time_size, c) = if version >= b'2' {
        let v1 = 44 + block_len(4, [isut, isstd, leap, time, typ, chars]);
        (v1, 8, counts(v1)?)
    } else {
        (0, 4, [isut, isstd, leap, time, typ, chars])
    };
    let [isut, isstd, leap, time, typ, chars] = c;
    let mut at = header_at + 44;
    let mut times = Vec::with_capacity(time);
    for i in 0..time {
        times.push(if time_size == 8 { be64(data, at + i * 8)? } else { be32(data, at + i * 4)? });
    }
    at += time * time_size;
    let idx = data.get(at..at + time)?.to_vec();
    at += time;
    let mut types = Vec::with_capacity(typ);
    for i in 0..typ {
        let off = be32(data, at + i * 6)? as i32;
        types.push((off, *data.get(at + i * 6 + 4)? != 0));
    }
    at += typ * 6 + chars + leap * (time_size + 4) + isstd + isut;
    let footer = if version >= b'2' {
        let rest = data.get(at..)?;
        let text = std::str::from_utf8(rest).ok()?;
        text.trim_matches('\n').to_string()
    } else {
        String::new()
    };
    Some(Tzif { times, idx, types, footer })
}

fn tzif_offset(data: &[u8], unix: i64) -> Option<i64> {
    let tz = parse_tzif(data)?;
    if tz.types.is_empty() {
        return None;
    }
    if tz.times.is_empty() || unix >= *tz.times.last()? {
        if !tz.footer.is_empty()
            && let Some(offset) = posix_tz_offset(&tz.footer, unix)
        {
            return Some(offset);
        }
        if tz.times.is_empty() {
            return Some(i64::from(tz.types[0].0));
        }
    }
    if unix < tz.times[0] {
        // Before the first transition: the first standard-time type, else type 0.
        let first = tz.types.iter().find(|(_, dst)| !dst).unwrap_or(&tz.types[0]);
        return Some(i64::from(first.0));
    }
    let pos = tz.times.partition_point(|t| *t <= unix) - 1;
    let ty = *tz.idx.get(pos)? as usize;
    Some(i64::from(tz.types.get(ty)?.0))
}

// ---------------------------------------------------------------- POSIX TZ footer

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn name(&mut self) -> Option<()> {
        if self.peek()? == b'<' {
            while self.peek()? != b'>' {
                self.i += 1;
            }
            self.i += 1;
        } else {
            let start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            if self.i - start < 3 {
                return None;
            }
        }
        Some(())
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }

    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn hms(&mut self) -> Option<i64> {
        let sign = match self.peek()? {
            b'-' => {
                self.i += 1;
                -1
            }
            b'+' => {
                self.i += 1;
                1
            }
            _ => 1,
        };
        let mut secs = self.number()? * 3600;
        if self.peek() == Some(b':') {
            self.i += 1;
            secs += self.number()? * 60;
            if self.peek() == Some(b':') {
                self.i += 1;
                secs += self.number()?;
            }
        }
        Some(sign * secs)
    }
}

enum Rule {
    /// Month, week (1-5, 5 = last), weekday (0 = Sunday), seconds after local midnight.
    Month(i64, i64, i64, i64),
    /// 1-based day of year, leap days never counted (`Jn`).
    Julian(i64, i64),
    /// 0-based day of year, leap days counted (`n`).
    Day(i64, i64),
}

fn rule(c: &mut Cursor<'_>) -> Option<Rule> {
    let kind = c.peek()?;
    let parsed = match kind {
        b'M' => {
            c.i += 1;
            let m = c.number()?;
            c.i += 1;
            let w = c.number()?;
            c.i += 1;
            let d = c.number()?;
            (Some((m, w, d)), 0, 0)
        }
        b'J' => {
            c.i += 1;
            (None, 1, c.number()?)
        }
        _ => (None, 2, c.number()?),
    };
    let time = if c.peek() == Some(b'/') {
        c.i += 1;
        c.hms()?
    } else {
        7200
    };
    Some(match parsed {
        (Some((m, w, d)), _, _) => Rule::Month(m, w, d, time),
        (None, 1, n) => Rule::Julian(n, time),
        (None, _, n) => Rule::Day(n, time),
    })
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Seconds since the Unix epoch of the rule's transition in `year`, given the local offset in
/// effect just before it.
fn rule_instant(rule: &Rule, year: i32, offset_before: i64) -> Option<i64> {
    let jan1 = NaiveDate::from_ymd_opt(year, 1, 1)?;
    let (day_index, time) = match *rule {
        Rule::Month(m, w, d, time) => {
            let first = NaiveDate::from_ymd_opt(year, u32::try_from(m).ok()?, 1)?;
            let first_wd = i64::from(first.weekday().num_days_from_sunday());
            let mut day = 1 + (d - first_wd).rem_euclid(7) + (w - 1) * 7;
            let month_len = {
                let next = if m == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1)?
                } else {
                    NaiveDate::from_ymd_opt(year, u32::try_from(m + 1).ok()?, 1)?
                };
                (next - first).num_days()
            };
            while day > month_len {
                day -= 7;
            }
            ((first - jan1).num_days() + day - 1, time)
        }
        Rule::Julian(n, time) => (n - 1 + i64::from(is_leap(year) && n >= 60), time),
        Rule::Day(n, time) => (n, time),
    };
    let midnight = jan1.and_hms_opt(0, 0, 0)?.and_utc().timestamp() + day_index * 86400;
    Some(midnight + time - offset_before)
}

/// Offset of a POSIX TZ string such as `PST8PDT,M3.2.0,M11.1.0` at `unix`.
fn posix_tz_offset(tz: &str, unix: i64) -> Option<i64> {
    let mut c = Cursor { s: tz.as_bytes(), i: 0 };
    c.name()?;
    let std_off = -c.hms()?;
    if c.peek().is_none() {
        return Some(std_off);
    }
    c.name()?;
    let dst_off = if matches!(c.peek(), Some(b',') | None) { std_off + 3600 } else { -c.hms()? };
    if c.peek() != Some(b',') {
        return Some(std_off);
    }
    c.i += 1;
    let start = rule(&mut c)?;
    if c.peek() != Some(b',') {
        return None;
    }
    c.i += 1;
    let end = rule(&mut c)?;
    let year = DateTime::<Utc>::from_timestamp(unix + std_off, 0)?.year();
    let dst_start = rule_instant(&start, year, std_off)?;
    let dst_end = rule_instant(&end, year, dst_off)?;
    let in_dst = if dst_start < dst_end { unix >= dst_start && unix < dst_end } else { !(unix >= dst_end && unix < dst_start) };
    Some(if in_dst { dst_off } else { std_off })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_rules() {
        let la = "PST8PDT,M3.2.0,M11.1.0";
        // 2026-07-01T08:00Z is 01:00 PDT; 2026-01-15T08:00Z is 00:00 PST.
        assert_eq!(posix_tz_offset(la, 1_782_892_800), Some(-7 * 3600));
        assert_eq!(posix_tz_offset(la, 1_768_464_000), Some(-8 * 3600));
        assert_eq!(posix_tz_offset("<+03>-3", 0), Some(3 * 3600));
        assert_eq!(posix_tz_offset("CET-1CEST,M3.5.0,M10.5.0/3", 1_782_892_800), Some(2 * 3600));
    }

    #[test]
    fn named_zone_date_rolls_at_local_midnight() {
        if !std::path::Path::new("/usr/share/zoneinfo/America/Los_Angeles").exists() {
            return;
        }
        // 2026-07-01T06:00Z is 23:00 PDT the previous day.
        assert_eq!(date_at(1_782_885_600, "America/Los_Angeles", ""), "2026-06-30");
        assert_eq!(date_at(1_782_892_800, "America/Los_Angeles", ""), "2026-07-01");
        // Invalid credential zone falls through to the configured one.
        assert_eq!(date_at(1_782_885_600, "Not/AZone", "Asia/Tokyo"), "2026-07-01");
    }
}
