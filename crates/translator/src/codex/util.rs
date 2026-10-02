//! Helpers shared by the Codex translators. Go keeps a private copy in each package; the
//! copies are identical except where noted.

use std::collections::{HashMap, HashSet};

use chrono::{Local, Offset, TimeZone};
use cpa_json::{Res, Value};

/// Tool names and call ids are limited to 64 bytes.
const NAME_LIMIT: usize = 64;

/// Go's `s[:n]` on a string: slices bytes (possibly mid-character) and, as `json.Marshal`
/// would later do, turns every invalid byte into U+FFFD.
pub fn truncate_bytes(s: &str, n: usize) -> String {
    go_lossy(&s.as_bytes()[..n.min(s.len())])
}

/// Decodes bytes like Go's JSON encoder: each invalid UTF-8 byte becomes one U+FFFD.
pub fn go_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                let bad = e.error_len().unwrap_or(after.len());
                out.extend(std::iter::repeat_n('\u{FFFD}', bad));
                rest = &after[bad..];
            }
        }
    }
}

/// Shortening rule for a single tool name: names over 64 bytes keep `mcp__` plus the last
/// `__` segment when they start with `mcp__`, otherwise are cut to 64 bytes.
pub fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_string();
    }
    if name.starts_with("mcp__")
        && let Some(idx) = name.rfind("__")
        && idx > 0
    {
        let cand = format!("mcp__{}", &name[idx + 2..]);
        if cand.len() > NAME_LIMIT {
            return truncate_bytes(&cand, NAME_LIMIT);
        }
        return cand;
    }
    truncate_bytes(name, NAME_LIMIT)
}

/// Unique shortened names (original -> short) within one request. `base` is the per-name
/// shortening rule; collisions get `_1`, `_2`, ... suffixes inside the 64 byte limit.
pub fn build_short_name_map(
    names: &[String],
    base: impl Fn(&str) -> String,
) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut m = HashMap::new();
    for n in names {
        let cand = base(n);
        let uniq = make_unique(&used, cand);
        used.insert(uniq.clone());
        m.insert(n.clone(), uniq);
    }
    m
}

fn make_unique(used: &HashSet<String>, cand: String) -> String {
    if !used.contains(&cand) {
        return cand;
    }
    for i in 1.. {
        let suffix = format!("_{i}");
        let allowed = NAME_LIMIT.saturating_sub(suffix.len());
        let tmp = if cand.len() > allowed {
            truncate_bytes(&cand, allowed)
        } else {
            cand.clone()
        } + &suffix;
        if !used.contains(&tmp) {
            return tmp;
        }
    }
    unreachable!("unbounded suffix search")
}

/// Inverts an original -> short map.
pub fn reverse_map(m: HashMap<String, String>) -> HashMap<String, String> {
    m.into_iter().map(|(orig, short)| (short, orig)).collect()
}

/// Placeholder filename for an inline file of the given MIME type.
pub fn file_name_from_mime(mime_type: &str) -> &'static str {
    let m = mime_type.trim().to_lowercase();
    match m.as_str() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        _ if m.starts_with("video/") => "video",
        _ => "document",
    }
}

/// MIME type for a Codex image generation `output_format` (bare format name or full MIME).
pub fn mime_type_from_output_format(output_format: &str) -> String {
    if output_format.is_empty() {
        return "image/png".into();
    }
    if output_format.contains('/') {
        return output_format.to_string();
    }
    match output_format.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
    .into()
}

/// Go `time.Unix(secs, 0).Format(time.RFC3339Nano)` in the local zone ("Z" for UTC).
pub fn rfc3339_local(secs: i64) -> String {
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) if t.offset().fix().local_minus_utc() != 0 => {
            t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
        }
        Some(t) => t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => "1970-01-01T00:00:00Z".into(),
    }
}

/// Go `time.Unix(secs, 0).UTC().Format(time.RFC3339)`.
pub fn rfc3339_utc(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// gjson `ForEach` values: array elements, object values, a scalar once, nothing when absent.
pub fn values<'a>(r: &'a Res<'_>) -> Vec<Res<'a>> {
    match r.v() {
        None => vec![],
        Some(Value::Array(a)) => a.iter().map(Res::of).collect(),
        Some(Value::Object(m)) => m.values().map(Res::of).collect(),
        Some(v) => vec![Res::of(v)],
    }
}
