//! Small helpers every translator needs: clocks, string/lookup pickers and a few JSON part
//! builders that were previously copied into each package.

use chrono::DateTime;
use cpa_json::{json, Res, Value};

/// Current Unix time in seconds.
pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current Unix time in nanoseconds (Go `time.Now().UnixNano()`), used in generated ids.
pub fn unix_nano_now() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
}

/// Current UTC time as `2006-01-02T15:04:05Z` (Go `time.Now().UTC().Format(time.RFC3339)`).
pub fn utc_now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Unix seconds of an RFC 3339 timestamp (Go: `time.Parse(RFC3339Nano, s).Unix()`).
pub fn parse_create_time(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp())
}

/// Go `bytes.TrimSpace` (Unicode whitespace when the bytes are UTF-8).
pub fn trim_space(b: &[u8]) -> &[u8] {
    match std::str::from_utf8(b) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => b.trim_ascii(),
    }
}

/// First value that is not the empty string (Go `if v != ""`), returned as-is.
pub fn first_non_empty(values: &[&str]) -> String {
    values.iter().find(|v| !v.is_empty()).map(|v| (*v).to_string()).unwrap_or_default()
}

/// First value that is not blank after trimming (Go `strings.TrimSpace(v) != ""`); the original
/// untrimmed string is returned.
pub fn first_non_blank(values: &[&str]) -> String {
    values.iter().find(|v| !v.trim().is_empty()).map(|v| (*v).to_string()).unwrap_or_default()
}

/// First value that is not blank after trimming, returned trimmed.
pub fn first_trimmed<S: AsRef<str>>(values: &[S]) -> String {
    values.iter().map(|v| v.as_ref().trim()).find(|v| !v.is_empty()).unwrap_or_default().to_string()
}

/// First lookup result that exists, or a missing result.
pub fn first_existing<'a>(values: impl IntoIterator<Item = Res<'a>>) -> Res<'a> {
    values.into_iter().find(Res::exists).unwrap_or(Res::NONE)
}

/// Audio MIME type to the `input_audio.format` value of OpenAI-style requests.
pub fn input_audio_format_from_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

/// A Gemini text part `{"text": ...}`.
pub fn text_part(text: &str) -> Value {
    json!({ "text": text })
}

/// Source text of a part's `functionCall.args` (Go's `Raw`), falling back to the compact form.
/// `part_raw` is the part's own source text, as listed by `cpa_json::raw_children`.
pub fn args_raw(part_raw: Option<&&str>, args: &Res<'_>) -> String {
    super::raw_in(part_raw, "functionCall.args").map_or_else(|| args.raw(), str::to_string)
}
