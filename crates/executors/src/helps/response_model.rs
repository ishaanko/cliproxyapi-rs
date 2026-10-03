//! Upstream-reported (served) model extraction and substitution detection (Go:
//! helps/response_model.go).
//!
//! Executors feed raw upstream frames to [`extract_response_model_event`]; the served model is
//! compared with the requested one to warn about silent model substitution (throttled per
//! credential and model pair).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cpa_core::thinking::parse_suffix;
use cpa_json::Value;
use cpa_runtime::conductor::session::lazy::Doc;
use parking_lot::Mutex;

use super::text::json_payload;

/// Defensive bound on an upstream-controlled string reaching logs and usage records.
pub const MAX_RESPONSE_MODEL_LENGTH: usize = 128;

/// How often one credential and model pair may warn.
pub const MODEL_SUBSTITUTION_WARN_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Caps the throttle state; memory safety wins over perfect throttling.
pub const MODEL_SUBSTITUTION_WARN_MAX_ENTRIES: usize = 1024;

/// The model an upstream reports serving, read from a raw JSON frame or an SSE line, and whether
/// the event terminates the response. `("", false)` when the frame carries no model.
pub fn extract_response_model_event(payload: &[u8], provider: &str) -> (String, bool) {
    let Some(data) = json_payload(payload) else {
        return (String::new(), false);
    };
    match provider.trim().to_lowercase().as_str() {
        "codex" => extract_codex_response_model_event(payload),
        "claude" => extract_claude_response_model_event(data),
        "gemini" | "gemini-interactions" | "vertex" | "aistudio" | "antigravity" => {
            extract_gemini_response_model_event(data)
        }
        _ => extract_generic_response_model_event(data),
    }
}

/// Trimmed string at `path` when it is a JSON string within the length bound.
fn bounded_model(v: &Doc<'_>, path: &str) -> Option<String> {
    match v.g(path).v() {
        Some(Value::String(s)) => {
            let s = s.trim();
            (s.len() <= MAX_RESPONSE_MODEL_LENGTH).then(|| s.to_string())
        }
        _ => None,
    }
}

fn is_string_at(v: &Doc<'_>, path: &str) -> bool {
    matches!(v.g(path).v(), Some(Value::String(_)))
}

/// Response model of an Anthropic message stream or non-stream message.
pub fn extract_claude_response_model_event(data: &[u8]) -> (String, bool) {
    let v = Doc::new(data);
    match v.g("type").str().as_str() {
        "message_start" => (bounded_model(&v, "message.model").unwrap_or_default(), false),
        "message_stop" => (String::new(), true),
        "message" => (bounded_model(&v, "model").unwrap_or_default(), true),
        _ => {
            if !v.exists() {
                return (String::new(), false);
            }
            // The first string-typed location wins even when it is too long.
            let model = if is_string_at(&v, "message.model") {
                bounded_model(&v, "message.model")
            } else if is_string_at(&v, "model") {
                bounded_model(&v, "model")
            } else {
                None
            };
            (model.unwrap_or_default(), false)
        }
    }
}

/// Response model of a Gemini / Vertex / AI Studio / interactions frame.
pub fn extract_gemini_response_model_event(data: &[u8]) -> (String, bool) {
    let v = Doc::new(data);
    if !v.exists() {
        return (String::new(), false);
    }
    let path = ["response.modelVersion", "modelVersion", "interaction.model", "model"]
        .into_iter()
        .find(|p| is_string_at(&v, p));
    let served = path.and_then(|p| bounded_model(&v, p)).unwrap_or_default();
    let mut finish = v.g("candidates.0.finishReason");
    if !finish.exists() {
        finish = v.g("response.candidates.0.finishReason");
    }
    let mut terminal = finish.exists() && !finish.str().is_empty();
    if !terminal {
        let mut event_type = v.g("event_type").str();
        if event_type.is_empty() {
            event_type = v.g("type").str();
        }
        terminal = is_interactions_terminal(&event_type, &v.g("interaction.status").str());
    }
    (served, terminal)
}

/// Response model of standard chat / responses / interactions / Gemini-shaped JSON.
pub fn extract_generic_response_model_event(data: &[u8]) -> (String, bool) {
    let v = Doc::new(data);
    if !v.exists() {
        return (String::new(), false);
    }
    let event_type_of = |key: &str| v.g(key).str();
    if is_string_at(&v, "response.model")
        && let Some(served) = bounded_model(&v, "response.model")
    {
        let t = event_type_of("type");
        return (served, matches!(t.as_str(), "response.completed" | "response.done" | "response.incomplete"));
    }
    if is_string_at(&v, "interaction.model")
        && let Some(served) = bounded_model(&v, "interaction.model")
    {
        let mut t = event_type_of("event_type");
        if t.is_empty() {
            t = event_type_of("type");
        }
        return (served, is_interactions_terminal(&t, &v.g("interaction.status").str()));
    }
    if is_string_at(&v, "modelVersion")
        && let Some(served) = bounded_model(&v, "modelVersion")
    {
        let cand = v.g("candidates.0.finishReason");
        return (served, cand.exists() && !cand.str().is_empty());
    }
    if is_string_at(&v, "response.modelVersion")
        && let Some(served) = bounded_model(&v, "response.modelVersion")
    {
        let cand = v.g("response.candidates.0.finishReason");
        return (served, cand.exists() && !cand.str().is_empty());
    }
    if is_string_at(&v, "message.model")
        && let Some(served) = bounded_model(&v, "message.model")
    {
        return (served, false);
    }
    if is_string_at(&v, "model")
        && let Some(served) = bounded_model(&v, "model")
    {
        let object_type = v.g("object").str();
        let finish_reason = v.g("choices.0.finish_reason").str();
        let status = v.g("status").str();
        let terminal = object_type == "chat.completion"
            || !finish_reason.is_empty()
            || status == "completed"
            || status == "incomplete";
        return (served, terminal);
    }
    let mut event_type = event_type_of("event_type");
    if event_type.is_empty() {
        event_type = event_type_of("type");
    }
    if is_interactions_terminal(&event_type, &v.g("interaction.status").str()) || event_type == "message_stop" {
        return (String::new(), true);
    }
    (String::new(), false)
}

fn is_interactions_terminal(event_type: &str, status: &str) -> bool {
    matches!(
        event_type,
        "interaction.completed" | "interaction.done" | "interaction.failed" | "interaction.cancelled"
    ) || matches!(status, "completed" | "incomplete" | "cancelled" | "failed")
}

/// Codex frames: only `response.created|in_progress|completed|incomplete|done` embed the
/// authoritative response object; the event type is checked before parsing the payload.
pub fn extract_codex_response_model_event(payload: &[u8]) -> (String, bool) {
    let Some(data) = json_payload(payload) else {
        return (String::new(), false);
    };
    let v = Doc::new(data);
    let (carries_model, terminal) = match v.g("type").str().trim() {
        "response.created" | "response.in_progress" => (true, false),
        "response.completed" | "response.incomplete" | "response.done" => (true, true),
        _ => (false, false),
    };
    if !carries_model || !v.exists() {
        return (String::new(), false);
    }
    // Upstream-controlled: reject non-string and oversized names.
    (bounded_model(&v, "response.model").unwrap_or_default(), terminal)
}

/// Lower-cases a model id and drops its thinking suffix (which never reaches the upstream).
pub fn normalize_model_name(model: &str) -> String {
    parse_suffix(&model.trim().to_lowercase()).model_name.trim().to_string()
}

fn strip_model_provider_prefix(model: &str) -> &str {
    match model.rfind('/') {
        Some(idx) if idx < model.len() - 1 => &model[idx + 1..],
        _ => model,
    }
}

/// Whether the upstream served a model other than the requested one; dated aliases, snapshot
/// pins, provider prefixes and `-latest` are accepted as the same model.
pub fn is_model_substituted(requested: &str, served: &str) -> bool {
    let served_model = normalize_model_name(served);
    if served_model.is_empty() {
        return false;
    }
    let requested_model = normalize_model_name(requested);
    if requested_model.is_empty() || requested_model == served_model {
        return false;
    }
    if is_dated_model_alias(&requested_model, &served_model) || is_dated_model_alias(&served_model, &requested_model) {
        return false;
    }
    let clean_req = strip_model_provider_prefix(&requested_model);
    let clean_srv = strip_model_provider_prefix(&served_model);
    if clean_req == clean_srv {
        return false;
    }
    if is_dated_model_alias(clean_req, clean_srv) || is_dated_model_alias(clean_srv, clean_req) {
        return false;
    }
    let req_no_latest = clean_req.strip_suffix("-latest").unwrap_or(clean_req);
    let srv_no_latest = clean_srv.strip_suffix("-latest").unwrap_or(clean_srv);
    if req_no_latest == srv_no_latest {
        return false;
    }
    !(is_dated_model_alias(req_no_latest, srv_no_latest) || is_dated_model_alias(srv_no_latest, req_no_latest))
}

/// `dated` is `base` plus a release date (`-YYYY-MM-DD`, `-YYYYMMDD`) or 3-digit version suffix.
pub fn is_dated_model_alias(base: &str, dated: &str) -> bool {
    let Some(suffix) = dated.strip_prefix(base).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    is_model_date_suffix(suffix) || (suffix.len() == 3 && is_digits(suffix))
}

fn is_model_date_suffix(suffix: &str) -> bool {
    let b = suffix.as_bytes();
    match b.len() {
        10 => b[4] == b'-' && b[7] == b'-' && is_digits(&suffix[..4]) && is_digits(&suffix[5..7]) && is_digits(&suffix[8..]),
        8 => is_digits(suffix),
        _ => false,
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Throttle key: provider, credential id and the normalized requested/served pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelSubstitutionKey {
    pub provider: String,
    pub auth_id: String,
    pub requested: String,
    pub served: String,
}

/// Records the last warning per key so repeats inside the window stay silent.
#[derive(Default)]
pub struct ModelSubstitutionThrottle {
    last_warn: Mutex<HashMap<ModelSubstitutionKey, Instant>>,
}

impl ModelSubstitutionThrottle {
    /// Whether `key` may warn now (recording the decision).
    pub fn allow(&self, key: ModelSubstitutionKey) -> bool {
        self.allow_at(key, Instant::now())
    }

    /// [`allow`](Self::allow) with an explicit clock for tests.
    pub fn allow_at(&self, key: ModelSubstitutionKey, now: Instant) -> bool {
        let mut last = self.last_warn.lock();
        if let Some(prev) = last.get(&key)
            && now.saturating_duration_since(*prev) < MODEL_SUBSTITUTION_WARN_WINDOW
        {
            return false;
        }
        if last.len() >= MODEL_SUBSTITUTION_WARN_MAX_ENTRIES {
            last.retain(|_, at| now.saturating_duration_since(*at) < MODEL_SUBSTITUTION_WARN_WINDOW);
            if last.len() >= MODEL_SUBSTITUTION_WARN_MAX_ENTRIES {
                last.clear();
            }
        }
        last.insert(key, now);
        true
    }
}

/// Process-wide substitution warning throttle.
pub static MODEL_SUBSTITUTION_WARNS: LazyLock<ModelSubstitutionThrottle> =
    LazyLock::new(ModelSubstitutionThrottle::default);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_accepts_snapshots_and_prefixes() {
        assert!(!is_model_substituted("gpt-5", "gpt-5-2025-08-07"));
        assert!(!is_model_substituted("gpt-5(high)", "GPT-5"));
        assert!(!is_model_substituted("openai/gpt-5", "gpt-5"));
        assert!(!is_model_substituted("claude-sonnet-4-latest", "claude-sonnet-4-20250514"));
        assert!(is_model_substituted("gpt-5", "gpt-4o"));
        assert!(is_model_substituted("gpt-5", "gpt-5-mini"));
        assert!(!is_model_substituted("gpt-5", ""));
    }

    #[test]
    fn extracts_models_per_provider() {
        let claude = br#"data: {"type":"message_start","message":{"model":"claude-x"}}"#;
        assert_eq!(extract_response_model_event(claude, "claude"), ("claude-x".into(), false));
        assert_eq!(extract_response_model_event(br#"{"type":"message_stop"}"#, "claude"), (String::new(), true));
        let codex = br#"data: {"type":"response.completed","response":{"model":"gpt-5"}}"#;
        assert_eq!(extract_response_model_event(codex, "codex"), ("gpt-5".into(), true));
        let gemini = br#"{"modelVersion":"gemini-2.5-pro","candidates":[{"finishReason":"STOP"}]}"#;
        assert_eq!(extract_response_model_event(gemini, "vertex"), ("gemini-2.5-pro".into(), true));
        let chat = br#"{"object":"chat.completion","model":"m1","choices":[]}"#;
        assert_eq!(extract_response_model_event(chat, "kimi"), ("m1".into(), true));
        let long = format!(r#"{{"model":"{}"}}"#, "x".repeat(200));
        assert_eq!(extract_response_model_event(long.as_bytes(), "kimi").0, "");
    }

    #[test]
    fn throttle_windows() {
        let t = ModelSubstitutionThrottle::default();
        let key = ModelSubstitutionKey { provider: "codex".into(), auth_id: "a".into(), requested: "r".into(), served: "s".into() };
        let now = Instant::now();
        assert!(t.allow_at(key.clone(), now));
        assert!(!t.allow_at(key.clone(), now + Duration::from_secs(599)));
        assert!(t.allow_at(key, now + Duration::from_secs(601)));
    }
}
