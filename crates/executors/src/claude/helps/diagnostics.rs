//! Claude continuity and diagnostics state plus request classification (Go:
//! helps/claude_diagnostics.go). The Go context plumbing (`WithIncomingHeaders`,
//! `WithClaudeSessionID`, ...) is replaced by `ClaudeCtx` in `helps/mod.rs`.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cpa_json::J;
use http::HeaderMap;
use parking_lot::Mutex;
use regex::Regex;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::helps::session::{header_value_case_insensitive, header_values_case_insensitive};
use super::credential_identity::sjson_string;

const CLAUDE_DIAGNOSTICS_TTL: Duration = Duration::from_secs(3600);
const CLAUDE_DIAGNOSTICS_CLEANUP_PERIOD: Duration = Duration::from_secs(15 * 60);
const CLAUDE_DIAGNOSTICS_MAX_ENTRIES: usize = 4096;
const CLAUDE_DIAGNOSTICS_EVICT_BATCH_SIZE: usize = 256;

const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

static REQUEST_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^req_[A-Za-z0-9_-]{1,36}$").expect("static regex"));
// Go's `\s` is ASCII only.
static PREV_REQ_BILLING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\t\n\f\r ]*cc_prev_req=[^;]+;").expect("static regex"));
static PROMPT_ID_BILLING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\t\n\f\r ]*cc_prompt_id=[^;]+;").expect("static regex"));

/// Go: `ClaudeContinuityContext`, request-scoped continuity state shared across cloaking and
/// execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeContinuityContext {
    pub key: String,
    pub sequence: u64,
    pub previous_message_id: String,
    pub previous_request_id: String,
    pub prompt_id: String,
    /// Calendar date this session was first seen on. The cloaked currentDate reminder reuses it
    /// so the reminder text stays byte-stable within a session even when the local date flips.
    pub pinned_date: String,
    pub initialized: bool,
}

struct Entry {
    previous_message_id: String,
    previous_request_id: String,
    prompt_id: String,
    pinned_date: String,
    minimum_sequence: u64,
    committed_sequence: u64,
    last_access: u64,
    expires_at: Instant,
}

/// Result of [`begin_claude_continuity`] (Go's five return values). `key` is empty (and the rest
/// zero/empty) when the credential identity or session id is blank.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeContinuityBegin {
    pub key: String,
    pub sequence: u64,
    pub previous_message_id: String,
    pub previous_request_id: String,
    pub prompt_id: String,
}

/// Go: `claudeDiagnosticsState`. Methods take `now` so expiry is testable without sleeping.
pub struct ClaudeDiagnosticsState {
    entries: HashMap<String, Entry>,
    last_cleanup: Option<Instant>,
    next_sequence: u64,
    next_access: u64,
}

static STATE: LazyLock<Mutex<ClaudeDiagnosticsState>> = LazyLock::new(|| Mutex::new(ClaudeDiagnosticsState::new()));

impl ClaudeDiagnosticsState {
    pub fn new() -> Self {
        Self { entries: HashMap::new(), last_cleanup: None, next_sequence: 0, next_access: 0 }
    }

    /// Go: `BeginClaudeContinuity` body.
    pub fn begin(
        &mut self,
        credential_identity: &str,
        session_id: &str,
        is_new_prompt_turn: bool,
        explicit_prompt_id: &str,
        now: Instant,
    ) -> ClaudeContinuityBegin {
        let credential_identity = credential_identity.trim();
        let session_id = session_id.trim();
        if credential_identity.is_empty() || session_id.is_empty() {
            return ClaudeContinuityBegin::default();
        }
        let key = hex::encode(Sha256::digest(format!("{credential_identity}\0{session_id}").as_bytes()));

        self.cleanup(now);
        let existing = self.entries.remove(&key);
        let found = existing.is_some();
        let new_generation = existing.as_ref().is_none_or(|e| now > e.expires_at);
        if new_generation && !found {
            self.evict();
        }

        self.next_sequence += 1;
        let sequence = self.next_sequence;
        let mut entry = match existing {
            Some(e) if !new_generation => e,
            _ => Entry {
                previous_message_id: String::new(),
                previous_request_id: String::new(),
                prompt_id: String::new(),
                pinned_date: String::new(),
                minimum_sequence: sequence,
                committed_sequence: 0,
                last_access: 0,
                expires_at: now,
            },
        };

        let explicit_prompt_id = explicit_prompt_id.trim();
        let active_prompt_id = if !explicit_prompt_id.is_empty() && is_valid_claude_prompt_id(explicit_prompt_id) {
            explicit_prompt_id.to_lowercase()
        } else if is_new_prompt_turn || entry.prompt_id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            entry.prompt_id.clone()
        };

        self.next_access += 1;
        entry.last_access = self.next_access;
        entry.expires_at = now + CLAUDE_DIAGNOSTICS_TTL;
        let result = ClaudeContinuityBegin {
            key: key.clone(),
            sequence,
            previous_message_id: entry.previous_message_id.clone(),
            previous_request_id: entry.previous_request_id.clone(),
            prompt_id: active_prompt_id,
        };
        self.entries.insert(key, entry);
        result
    }

    /// Go: `PinClaudeSessionDate` body.
    pub fn pin_date(&mut self, key: &str, date: &str) -> String {
        let (key, date) = (key.trim(), date.trim());
        if key.is_empty() || date.is_empty() {
            return date.to_string();
        }
        let Some(entry) = self.entries.get_mut(key) else { return date.to_string() };
        if entry.pinned_date.is_empty() {
            entry.pinned_date = date.to_string();
        }
        entry.pinned_date.clone()
    }

    /// Go: `CommitClaudeContinuity` body.
    pub fn commit(&mut self, key: &str, sequence: u64, message_id: &str, request_id: &str, prompt_id: &str, now: Instant) {
        let key = key.trim();
        let message_id = message_id.trim();
        let request_id = request_id.trim();
        if key.is_empty() || sequence == 0 || message_id.is_empty() {
            return;
        }
        let Some(entry) = self.entries.get_mut(key) else { return };
        if now > entry.expires_at || sequence < entry.minimum_sequence || sequence < entry.committed_sequence {
            return;
        }
        self.next_access += 1;
        entry.previous_message_id = message_id.to_string();
        entry.previous_request_id =
            if !request_id.is_empty() && REQUEST_ID_RE.is_match(request_id) { request_id.to_string() } else { String::new() };
        let prompt_id = prompt_id.trim();
        if !prompt_id.is_empty() && is_valid_claude_prompt_id(prompt_id) {
            entry.prompt_id = prompt_id.to_lowercase();
        }
        entry.committed_sequence = sequence;
        entry.last_access = self.next_access;
        entry.expires_at = now + CLAUDE_DIAGNOSTICS_TTL;
    }

    /// Go: `cleanupClaudeDiagnosticsLocked`: drops expired entries every 15 minutes.
    fn cleanup(&mut self, now: Instant) {
        if self.last_cleanup.is_some_and(|last| now.saturating_duration_since(last) < CLAUDE_DIAGNOSTICS_CLEANUP_PERIOD) {
            return;
        }
        self.entries.retain(|_, e| now <= e.expires_at);
        self.last_cleanup = Some(now);
    }

    /// Go: `evictClaudeDiagnosticsLocked`: at capacity, drops the 256 least recently used entries.
    fn evict(&mut self) {
        if self.entries.len() < CLAUDE_DIAGNOSTICS_MAX_ENTRIES {
            return;
        }
        let mut candidates: Vec<(u64, String)> = self.entries.iter().map(|(k, e)| (e.last_access, k.clone())).collect();
        candidates.sort_by_key(|(access, _)| *access);
        for (_, key) in candidates.into_iter().take(CLAUDE_DIAGNOSTICS_EVICT_BATCH_SIZE) {
            self.entries.remove(&key);
        }
    }
}

impl Default for ClaudeDiagnosticsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Go: `IsValidClaudePromptID`: strict RFC 4122 UUIDv4 text (8-4-4-4-12 hex, version 4, variant 10xx).
pub fn is_valid_claude_prompt_id(id: &str) -> bool {
    let id = id.trim().as_bytes();
    if id.len() != 36 {
        return false;
    }
    for (i, b) in id.iter().enumerate() {
        let ok = if matches!(i, 8 | 13 | 18 | 23) { *b == b'-' } else { b.is_ascii_hexdigit() };
        if !ok {
            return false;
        }
    }
    id[14] == b'4' && matches!(id[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

/// Go: `ClaudeDeterministicPromptID`: a UUIDv4-shaped id derived from sha256 of `seed`.
pub fn claude_deterministic_prompt_id(seed: &str) -> String {
    let mut digest: [u8; 32] = Sha256::digest(seed.as_bytes()).into();
    digest[6] = (digest[6] & 0x0f) | 0x40;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&digest[0..4]),
        hex::encode(&digest[4..6]),
        hex::encode(&digest[6..8]),
        hex::encode(&digest[8..10]),
        hex::encode(&digest[10..16]),
    )
}

/// Go: `BeginClaudeContinuity`: starts one request generation for a credential identity and
/// Claude conversation, returning the previous upstream message and request ids and the active
/// prompt id. A valid `explicit_prompt_id` is adopted; otherwise a new prompt turn (or a missing
/// prompt id) gets a fresh UUIDv4.
pub fn begin_claude_continuity(
    credential_identity: &str,
    session_id: &str,
    is_new_prompt_turn: bool,
    explicit_prompt_id: &str,
) -> ClaudeContinuityBegin {
    STATE.lock().begin(credential_identity, session_id, is_new_prompt_turn, explicit_prompt_id, Instant::now())
}

/// Go: `PinClaudeSessionDate`: the calendar date pinned for this continuity session, recording
/// `date` on the first call of a session and returning the pinned value afterwards. This keeps the
/// cloaked currentDate reminder byte-stable so a local-midnight flip cannot invalidate the
/// prompt-cache prefix. TTL expiry resets the entry (the next request re-anchors); an unknown key
/// (for example after a restart) returns `date` unchanged.
pub fn pin_claude_session_date(key: &str, date: &str) -> String {
    STATE.lock().pin_date(key, date)
}

/// Go: `BeginClaudeDiagnostics`: `(key, sequence, previous_message_id)`.
pub fn begin_claude_diagnostics(credential_identity: &str, session_id: &str) -> (String, u64, String) {
    let begin = begin_claude_continuity(credential_identity, session_id, false, "");
    (begin.key, begin.sequence, begin.previous_message_id)
}

/// Go: `CommitClaudeContinuity` (`prompt_id` is Go's optional variadic; "" for none). Advances
/// continuity only for a non-empty message id, so truncated streams never advance, and never
/// lets an older concurrently-started generation overwrite a newer one.
pub fn commit_claude_continuity(key: &str, sequence: u64, message_id: &str, request_id: &str, prompt_id: &str) {
    STATE.lock().commit(key, sequence, message_id, request_id, prompt_id, Instant::now());
}

/// Go: `CommitClaudeDiagnostics`.
pub fn commit_claude_diagnostics(key: &str, sequence: u64, message_id: &str) {
    commit_claude_continuity(key, sequence, message_id, "", "");
}

/// Go: `ResetClaudeDiagnosticsForTest`.
pub fn reset_claude_diagnostics_for_test() {
    *STATE.lock() = ClaudeDiagnosticsState::new();
}

/// Go: `IsClaudeNewPromptTurn`: false for probes/helpers and for tool-result continuations.
pub fn is_claude_new_prompt_turn(body: &[u8]) -> bool {
    if is_claude_probe_or_helper_request(body) {
        return false;
    }
    let root = crate::helps::parse_cache::parse(body);
    let messages = root.g("messages");
    if !messages.is_array() {
        return true;
    }
    let arr = messages.array();
    let Some(last) = arr.last() else { return true };
    if last.g("role").str() != "user" {
        return false;
    }
    let content = last.g("content");
    if content.is_array() && content.array().iter().any(|part| part.g("type").str() == "tool_result") {
        return false;
    }
    true
}

fn is_quota_probe_text(t: &str) -> bool {
    matches!(t, "quota" | "test" | "." | "probe")
}

/// Go: `isClaudeProbeRequest`: `max_tokens == 1` with no tools and a trivial single message.
fn is_claude_probe_request(root: &serde_json::Value) -> bool {
    let max_tokens = root.g("max_tokens");
    if !max_tokens.exists() || max_tokens.int() != 1 {
        return false;
    }
    let tools = root.g("tools");
    if tools.exists() && !tools.array().is_empty() {
        return false;
    }
    let messages = root.g("messages");
    if !messages.exists() || !messages.is_array() || messages.array().is_empty() {
        return true; // headless preflight probe without messages
    }
    let arr = messages.array();
    if arr.len() != 1 {
        return false;
    }
    let first = &arr[0];
    if first.g("role").str() != "user" {
        return false;
    }
    let content = first.g("content");
    if content.is_string() {
        return is_quota_probe_text(content.str().trim());
    }
    if content.is_array() {
        let mut non_reminder_count = 0;
        let mut matched = false;
        for part in content.array() {
            let t = part.g("text").str();
            let t = t.trim();
            if t.contains("<system-reminder>") {
                continue;
            }
            non_reminder_count += 1;
            if is_quota_probe_text(t) || (t == "Hi" && part.g("cache_control").exists()) {
                matched = true;
            }
        }
        return non_reminder_count == 1 && matched;
    }
    false
}

/// True when any text of `system` (string or block array) satisfies `matches`.
fn system_text_matches(root: &serde_json::Value, matches: &dyn Fn(&str) -> bool) -> bool {
    let system = root.g("system");
    if system.is_array() {
        system.array().iter().any(|part| matches(&part.g("text").str()))
    } else {
        matches(&system.str())
    }
}

/// True when any text of a message `content` (string or block array) satisfies `matches`.
fn content_text_matches(content: &cpa_json::Res<'_>, matches: &dyn Fn(&str) -> bool) -> bool {
    if content.is_array() {
        content.array().iter().any(|part| matches(&part.g("text").str()))
    } else {
        matches(&content.str())
    }
}

/// Go: `isClaudeTitleHelperInstruction`.
fn is_claude_title_helper_instruction(root: &serde_json::Value) -> bool {
    let matches_title_prompt = |t: &str| {
        t.contains("Return a short title")
            || t.contains("naming a coding session")
            || t.contains("Write the title in the predominant language")
            || t.contains("<session>")
    };
    if system_text_matches(root, &matches_title_prompt) {
        return true;
    }
    let messages = root.g("messages");
    messages.is_array() && messages.array().iter().any(|msg| content_text_matches(&msg.g("content"), &matches_title_prompt))
}

/// Go: `isClaudeTitleHelperRequest`: structured title schema, or a system-role title instruction.
fn is_claude_title_helper_request(root: &serde_json::Value) -> bool {
    let props = root.g("output_config.format.schema.properties");
    if props.exists() {
        if props.g("title").exists() && props.entries().len() == 1 {
            return is_claude_title_helper_instruction(root);
        }
        return false;
    }

    // Without a structured schema only a system-role block (top-level or in messages) counts;
    // ordinary user messages are never internal helpers.
    let matches_system_title = |t: &str| {
        t.contains("naming a coding session")
            || t.contains("Return a short title")
            || t.contains("Write the title in the predominant language")
    };
    if system_text_matches(root, &matches_system_title) {
        return true;
    }
    let messages = root.g("messages");
    messages.is_array()
        && messages
            .array()
            .iter()
            .any(|msg| msg.g("role").str() == "system" && content_text_matches(&msg.g("content"), &matches_system_title))
}

/// Go: `IsClaudeProbeOrHelperRequest`: a `max_tokens: 1` probe or an automated title helper,
/// which native Claude Code sends without `cc_prompt_id` or `cc_prev_req`.
pub fn is_claude_probe_or_helper_request(body: &[u8]) -> bool {
    let root = crate::helps::parse_cache::parse(body);
    is_claude_probe_request(&root) || is_claude_title_helper_request(&root)
}

/// Go: `IsClaudeSubagentRequest`: agent id headers, a `parent_session_id` in `metadata.user_id`, or
/// `cc_is_subagent=true` in the first system block.
pub fn is_claude_subagent_request(headers: &HeaderMap, body: &[u8]) -> bool {
    if !header_value_case_insensitive(headers, "X-Claude-Code-Agent-Id").is_empty()
        || !header_value_case_insensitive(headers, "X-Claude-Code-Parent-Agent-Id").is_empty()
    {
        return true;
    }
    let root = crate::helps::parse_cache::parse(body);
    if root.g("metadata.user_id.parent_session_id").exists() {
        return true;
    }
    let user_id = root.g("metadata.user_id").str();
    if !user_id.is_empty() && user_id.contains("\"parent_session_id\"") {
        return true;
    }
    // Only system[0].text or the billing header is inspected, never raw user message content.
    let system = root.g("system");
    if system.is_array() && !system.array().is_empty() {
        if system.g("0.text").str().contains("cc_is_subagent=true") {
            return true;
        }
    } else if system.is_string() && system.str().contains("cc_is_subagent=true") {
        return true;
    }
    false
}

/// Go: `ClaudePayloadHas1hTTL`: any tool, system or message content block with
/// `cache_control.ttl == "1h"`.
pub fn claude_payload_has_1h_ttl(payload: &[u8]) -> bool {
    if payload.is_empty() || !crate::helps::parse_cache::valid(payload) {
        return false;
    }
    let root = crate::helps::parse_cache::parse(payload);
    let has_1h = |item: &cpa_json::Res<'_>| {
        let cc = item.g("cache_control");
        cc.is_object() && cc.g("ttl").str() == "1h"
    };
    for field in ["tools", "system"] {
        let blocks = root.g(field);
        if blocks.is_array() && blocks.array().iter().any(has_1h) {
            return true;
        }
    }
    let messages = root.g("messages");
    if messages.is_array() {
        for msg in messages.array() {
            let content = msg.g("content");
            if content.is_array() && content.array().iter().any(has_1h) {
                return true;
            }
        }
    }
    false
}

/// Go: `ClaudeSubagentRequests1h`: 1h cache TTL requested through the payload or the
/// `extended-cache-ttl-2025-04-11` beta header.
pub fn claude_subagent_requests_1h(headers: &HeaderMap, body: &[u8]) -> bool {
    if claude_payload_has_1h_ttl(body) {
        return true;
    }
    header_values_case_insensitive(headers, "Anthropic-Beta").join(",").contains("extended-cache-ttl-2025-04-11")
}

/// First system block text when it carries the billing header prefix.
fn billing_text_of_first_system_block(root: &serde_json::Value) -> Option<String> {
    let system = root.g("system");
    if !system.is_array() || system.array().is_empty() {
        return None;
    }
    let text = system.g("0.text").str();
    text.starts_with(BILLING_HEADER_PREFIX).then_some(text)
}

/// `sjson.SetBytes(body, "system.0.text", text)`: replaces just that string value in place, so
/// every other byte of the body is preserved. The body is returned unchanged if the path is absent.
fn set_first_system_text(body: &[u8], text: &str) -> Vec<u8> {
    let Some(raw) = cpa_json::raw_at(body, "system.0.text") else { return body.to_vec() };
    // `raw_at` returns a subslice of `body`, so its offset is the pointer distance.
    let start = raw.as_ptr() as usize - body.as_ptr() as usize;
    let replacement = sjson_string(text);
    let mut out = Vec::with_capacity(body.len() + replacement.len());
    out.extend_from_slice(&body[..start]);
    out.extend_from_slice(&replacement);
    out.extend_from_slice(&body[start + raw.len()..]);
    out
}

/// Removes the `cc_prev_req` and `cc_prompt_id` fields of a billing header text.
fn strip_billing_tags_text(billing_text: &str) -> String {
    let cleaned = PREV_REQ_BILLING_RE.replace_all(billing_text, "");
    PROMPT_ID_BILLING_RE.replace_all(&cleaned, "").into_owned()
}

/// Go: `StripClaudeBillingTags`: removes `cc_prev_req` and `cc_prompt_id` from the billing header.
pub fn strip_claude_billing_tags(body: &[u8]) -> Vec<u8> {
    let root = crate::helps::parse_cache::parse(body);
    let Some(billing_text) = billing_text_of_first_system_block(&root) else { return body.to_vec() };
    let cleaned = strip_billing_tags_text(&billing_text);
    if cleaned == billing_text {
        return body.to_vec();
    }
    set_first_system_text(body, &cleaned)
}

/// Go: `InjectClaudeBillingTags`: re-appends `cc_prev_req` / `cc_prompt_id` (when non-empty) to
/// the billing header of the first system block.
pub fn inject_claude_billing_tags(body: &[u8], prev_req: &str, prompt_id: &str) -> Vec<u8> {
    let root = crate::helps::parse_cache::parse(body);
    let Some(billing_text) = billing_text_of_first_system_block(&root) else { return body.to_vec() };
    let mut cleaned = strip_billing_tags_text(&billing_text).trim().to_string();
    if !cleaned.ends_with(';') {
        cleaned.push(';');
    }
    if !prev_req.is_empty() {
        cleaned.push_str(&format!(" cc_prev_req={prev_req};"));
    }
    if !prompt_id.is_empty() {
        cleaned.push_str(&format!(" cc_prompt_id={prompt_id};"));
    }
    set_first_system_text(body, &cleaned)
}

/// Go: `ExtractClaudeBillingTags`: valid `(cc_prev_req, cc_prompt_id)` already present in the
/// billing header text ("" for each that is absent or malformed).
pub fn extract_claude_billing_tags(body: &[u8]) -> (String, String) {
    let root = crate::helps::parse_cache::parse(body);
    let system = root.g("system");
    let billing_text = if system.is_array() && !system.array().is_empty() {
        system.g("0.text").str()
    } else if system.is_string() {
        system.str()
    } else {
        String::new()
    };
    if billing_text.is_empty() || !billing_text.starts_with(BILLING_HEADER_PREFIX) {
        return (String::new(), String::new());
    }
    let tag_value = |tag: &str| -> Option<String> {
        let idx = billing_text.find(tag)?;
        let val = &billing_text[idx + tag.len()..];
        Some(val.split(';').next().unwrap_or(val).to_string())
    };
    let mut prev_req = String::new();
    let mut prompt_id = String::new();
    if let Some(val) = tag_value("cc_prev_req=")
        && REQUEST_ID_RE.is_match(&val)
    {
        prev_req = val;
    }
    if let Some(val) = tag_value("cc_prompt_id=")
        && is_valid_claude_prompt_id(&val)
    {
        prompt_id = val.to_lowercase();
    }
    (prev_req, prompt_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn diagnostics_track_completed_message_per_credential_session() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let first = s.begin("credential-a", "session-a", false, "", t0);
        assert!(!first.key.is_empty() && first.sequence == 1 && first.previous_message_id.is_empty());
        s.commit(&first.key, first.sequence, "msg_first", "", "", t0);
        let second = s.begin("credential-a", "session-a", false, "", t0);
        assert_eq!((second.sequence, second.previous_message_id.as_str()), (2, "msg_first"));

        assert!(s.begin("credential-a", "session-b", false, "", t0).previous_message_id.is_empty());
        assert!(s.begin("credential-b", "session-a", false, "", t0).previous_message_id.is_empty());
        assert_eq!(s.begin("", "session-a", false, "", t0), ClaudeContinuityBegin::default());
    }

    #[test]
    fn pin_session_date_anchors_first_request_and_reanchors_after_ttl() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let key = s.begin("credential", "session", false, "", t0).key;
        assert_eq!(s.pin_date(&key, "2026-08-01"), "2026-08-01");
        // Later requests of the same session keep the anchor even when the date flips.
        assert_eq!(s.pin_date(&key, "2026-08-02"), "2026-08-01");
        // TTL expiry resets the entry, so the session re-anchors to the current date.
        s.begin("credential", "session", false, "", at(t0, 3601));
        assert_eq!(s.pin_date(&key, "2026-08-02"), "2026-08-02");
        // Unknown keys (no continuity entry) fall back to the candidate date.
        assert_eq!(s.pin_date("unknown-key", "2026-08-03"), "2026-08-03");
    }

    #[test]
    fn expired_generation_commit_is_rejected() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let old = s.begin("credential", "session", false, "", t0);
        let later = at(t0, 3601);
        let new = s.begin("credential", "session", false, "", later);
        assert!(new.key == old.key && new.sequence > old.sequence && new.previous_message_id.is_empty());
        s.commit(&new.key, new.sequence, "msg_current", "", "", later);
        s.commit(&old.key, old.sequence, "msg_expired", "", "", later);
        assert_eq!(s.begin("credential", "session", false, "", later).previous_message_id, "msg_current");
    }

    #[test]
    fn cache_evicts_oldest_entries_within_capacity() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let first = s.begin("credential", "session-0", false, "", t0);
        let mut newest = String::new();
        for i in 1..=CLAUDE_DIAGNOSTICS_MAX_ENTRIES {
            newest = s.begin("credential", &format!("session-{i}"), false, "", t0).key;
        }
        assert!(s.entries.len() <= CLAUDE_DIAGNOSTICS_MAX_ENTRIES);
        assert!(!s.entries.contains_key(&first.key), "oldest entry was not evicted");
        assert!(s.entries.contains_key(&newest), "newest entry was evicted");

        let recreated = s.begin("credential", "session-0", false, "", t0);
        assert!(recreated.key == first.key && recreated.sequence > first.sequence);
        s.commit(&recreated.key, recreated.sequence, "msg_recreated", "", "", t0);
        s.commit(&first.key, first.sequence, "msg_evicted", "", "", t0);
        assert_eq!(s.begin("credential", "session-0", false, "", t0).previous_message_id, "msg_recreated");
    }

    #[test]
    fn late_older_commit_is_rejected() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let first = s.begin("credential", "session", false, "", t0);
        let second = s.begin("credential", "session", false, "", t0);
        s.commit(&first.key, second.sequence, "msg_newer", "", "", t0);
        s.commit(&first.key, first.sequence, "msg_older", "", "", t0);
        assert_eq!(s.begin("credential", "session", false, "", t0).previous_message_id, "msg_newer");
    }

    #[test]
    fn continuity_tracks_request_id_and_prompt_id() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let t1 = s.begin("cred-1", "sess-1", true, "", t0);
        assert!(t1.previous_message_id.is_empty() && t1.previous_request_id.is_empty() && !t1.prompt_id.is_empty());
        s.commit(&t1.key, t1.sequence, "msg_01aaa", "req_01bbb", &t1.prompt_id, t0);

        // Tool continuation reuses the prompt id.
        let t11 = s.begin("cred-1", "sess-1", false, "", t0);
        assert_eq!((t11.previous_message_id.as_str(), t11.previous_request_id.as_str()), ("msg_01aaa", "req_01bbb"));
        assert_eq!(t11.prompt_id, t1.prompt_id);
        s.commit(&t1.key, t11.sequence, "msg_01ccc", "req_01ddd", &t11.prompt_id, t0);

        let t2 = s.begin("cred-1", "sess-1", true, "", t0);
        assert_eq!((t2.previous_message_id.as_str(), t2.previous_request_id.as_str()), ("msg_01ccc", "req_01ddd"));
        assert_ne!(t2.prompt_id, t1.prompt_id);
    }

    #[test]
    fn overlapping_turns_keep_committed_prompt_id() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let t1 = s.begin("cred-1", "sess-1", true, "", t0);
        let t2 = s.begin("cred-1", "sess-1", true, "", t0);
        assert_ne!(t1.prompt_id, t2.prompt_id);
        s.commit(&t1.key, t1.sequence, "msg_01aaa", "req_01bbb", &t1.prompt_id, t0);

        let t11 = s.begin("cred-1", "sess-1", false, "", t0);
        assert_eq!(t11.previous_message_id, "msg_01aaa");
        assert_eq!(t11.prompt_id, t1.prompt_id, "uncommitted turn 2 must not overwrite the prompt id");

        s.commit(&t1.key, t2.sequence, "msg_01ccc", "req_01ddd", &t2.prompt_id, t0);
        let t21 = s.begin("cred-1", "sess-1", false, "", t0);
        assert_eq!((t21.previous_message_id.as_str(), t21.previous_request_id.as_str()), ("msg_01ccc", "req_01ddd"));
        assert_eq!(t21.prompt_id, t2.prompt_id);
    }

    #[test]
    fn continuity_clears_stale_request_id() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let t1 = s.begin("cred-1", "sess-1", true, "", t0);
        s.commit(&t1.key, t1.sequence, "msg_01aaa", "req_01bbb", &t1.prompt_id, t0);
        let t11 = s.begin("cred-1", "sess-1", false, "", t0);
        assert_eq!(t11.previous_request_id, "req_01bbb");
        // The upstream returned no request-id for turn 1.1.
        s.commit(&t1.key, t11.sequence, "msg_01ccc", "", &t11.prompt_id, t0);
        let t2 = s.begin("cred-1", "sess-1", true, "", t0);
        assert_eq!(t2.previous_message_id, "msg_01ccc");
        assert_eq!(t2.previous_request_id, "");
    }

    #[test]
    fn expired_entry_does_not_inherit_prompt_id() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let p1 = s.begin("cred-expire", "sess-expire", true, "", t0).prompt_id;
        assert!(!p1.is_empty());
        let p2 = s.begin("cred-expire", "sess-expire", false, "", at(t0, 3601)).prompt_id;
        assert!(!p2.is_empty() && p2 != p1);
    }

    #[test]
    fn explicit_prompt_id_is_adopted_lowercased() {
        let (mut s, t0) = (ClaudeDiagnosticsState::new(), Instant::now());
        let b = s.begin("c", "s", true, " 3C6489DC-BADC-42B2-BD28-49F8EBABFEDD ", t0);
        assert_eq!(b.prompt_id, "3c6489dc-badc-42b2-bd28-49f8ebabfedd");
        assert_ne!(s.begin("c", "s2", true, "not-a-uuid", t0).prompt_id, "not-a-uuid");
    }

    #[test]
    fn prompt_id_validation() {
        assert!(is_valid_claude_prompt_id(&Uuid::new_v4().to_string()));
        for invalid in [
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "3c6489dc-badc-42b2-0d28-49f8ebabfedd",
            "00000000-0000-0000-0000-000000000000",
            "",
            "not-a-uuid",
            "12345",
            "3c6489dc-badc-42b2-bd28-49f8ebabfedd-extra",
        ] {
            assert!(!is_valid_claude_prompt_id(invalid), "{invalid}");
        }
        assert!(is_valid_claude_prompt_id(&claude_deterministic_prompt_id("cpa:prompt:hello")));
        assert_eq!(claude_deterministic_prompt_id("x"), claude_deterministic_prompt_id("x"));
    }

    #[test]
    fn helper_predicates() {
        let probe = |s: &str| is_claude_probe_or_helper_request(s.as_bytes());
        assert!(probe(r#"{"model":"claude-fable-5-1","max_tokens":1,"messages":[{"role":"user","content":[{"type":"text","text":"Hi","cache_control":{"type":"ephemeral"}}]}]}"#));
        assert!(probe(r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#));
        assert!(!probe(r#"{"model":"claude-fable-5-1","max_tokens":1,"messages":[{"role":"user","content":"Hi"}]}"#));
        assert!(!probe(r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"what is 1+1?"}]}"#));
        assert!(!probe(r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":"Hi"}],"tools":[{"name":"t1","description":"tool"}]}"#));
        assert!(probe(r#"{"model":"claude-haiku-4-5-20251001","output_config":{"format":{"schema":{"properties":{"title":{"type":"string"}}}}},"messages":[{"role":"user","content":"Return a short title summarizing this conversation"}]}"#));
        assert!(!probe(r#"{"model":"claude-haiku-4-5-20251001","output_config":{"format":{"schema":{"properties":{"title":{"type":"string"}}}}},"messages":[{"role":"user","content":"what is the title of the book?"}]}"#));
        assert!(probe(r#"{"model":"claude-sonnet-5","system":[{"type":"text","text":"cli-identity"}],"messages":[{"role":"system","content":"Return a short title summarizing this conversation"}]}"#));
        // Multi-block content where only one block is "quota" is a real prompt.
        assert!(!probe(r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":[{"type":"text","text":"quota"},{"type":"text","text":"Please write an analysis of the codebase."}]}]}"#));
        assert!(probe(r#"{"model":"claude-sonnet-5","max_tokens":1,"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>some reminder</system-reminder>"},{"type":"text","text":"quota"}]}]}"#));

        assert!(!is_claude_new_prompt_turn(br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t1"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#));
        assert!(is_claude_new_prompt_turn(br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"explain sorting"}]}"#));

        let mut agent = HeaderMap::new();
        agent.insert("x-claude-code-agent-id", "sub-1".parse().expect("header value"));
        assert!(is_claude_subagent_request(&agent, b"{}"));
        assert!(is_claude_subagent_request(
            &HeaderMap::new(),
            br#"{"metadata":{"user_id":"{\"device_id\":\"dev\",\"session_id\":\"sess\",\"parent_session_id\":\"parent-1\"}"}}"#
        ));
        assert!(!is_claude_subagent_request(&HeaderMap::new(), b"{}"));
    }

    #[test]
    fn billing_tags_extract_strip_inject() {
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000; cc_prev_req=req_01abc; cc_prompt_id=3c6489dc-badc-42b2-bd28-49f8ebabfedd;"}]}"#;
        let (prev, prompt) = extract_claude_billing_tags(body);
        assert_eq!((prev.as_str(), prompt.as_str()), ("req_01abc", "3c6489dc-badc-42b2-bd28-49f8ebabfedd"));

        let stripped = strip_claude_billing_tags(body);
        assert_eq!(
            cpa_json::parse(&stripped).g("system.0.text").str(),
            "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000;"
        );
        let injected = inject_claude_billing_tags(&stripped, "req_02", "3c6489dc-badc-42b2-bd28-49f8ebabfedd");
        assert_eq!(
            cpa_json::parse(&injected).g("system.0.text").str(),
            "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli; cch=00000; cc_prev_req=req_02; cc_prompt_id=3c6489dc-badc-42b2-bd28-49f8ebabfedd;"
        );
        // Only the text value changes; whitespace and number formatting elsewhere survive.
        let spaced = br#"{"system": [ {"type":"text", "text": "x-anthropic-billing-header: a=b; cc_prompt_id=3c6489dc-badc-42b2-bd28-49f8ebabfedd;" } ], "n": 1.50 }"#;
        assert_eq!(
            String::from_utf8_lossy(&strip_claude_billing_tags(spaced)),
            r#"{"system": [ {"type":"text", "text": "x-anthropic-billing-header: a=b;" } ], "n": 1.50 }"#
        );
        // Bodies without a billing header are returned untouched.
        let plain = br#"{"system":[{"type":"text","text":"hi"}]}"#;
        assert_eq!(strip_claude_billing_tags(plain), plain);
        assert_eq!(inject_claude_billing_tags(plain, "req_1", "p"), plain);
    }

    #[test]
    fn payload_1h_ttl_and_subagent_requests() {
        let has = |s: &str| claude_payload_has_1h_ttl(s.as_bytes());
        assert!(!has(""));
        assert!(!has("not-json"));
        assert!(!has(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}]}"#));
        assert!(has(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#));
        assert!(has(r#"{"system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#));
        assert!(has(r#"{"tools":[{"name":"tool1","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#));
        assert!(!has(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"can you set ttl: 1h"}]}]}"#));

        let payload = br#"{"messages":[{"role":"user","content":"task"}]}"#;
        assert!(!claude_subagent_requests_1h(&HeaderMap::new(), payload));
        let mut beta = HeaderMap::new();
        beta.insert("anthropic-beta", "claude-code-20250219,extended-cache-ttl-2025-04-11".parse().expect("header value"));
        assert!(claude_subagent_requests_1h(&beta, payload));
    }

    /// Go's raw-byte prefilter (claude_json_prefilter.go) must never change a classifier answer,
    /// including when the matched text is spelled with `\u` escapes. This port has no prefilter
    /// (its parse is memoized and the tree walks are cheaper than the extra byte scans), so the
    /// answers are pinned directly.
    #[test]
    fn classifiers_keep_escaped_matches() {
        let escaped_title = r#"{"model":"m","system":[{"type":"text","text":"You are naming a coding session."}],"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(is_claude_probe_or_helper_request(escaped_title.as_bytes()), "escaped system title instruction");
        let plain_title = r#"{"model":"m","system":"Return a short title for this.","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(is_claude_probe_or_helper_request(plain_title.as_bytes()), "plain system title instruction");
        let schema_title = r#"{"model":"m","output_config":{"format":{"schema":{"properties":{"title":{"type":"string"}}}}},"messages":[{"role":"user","content":"<session>x</session>"}]}"#;
        assert!(is_claude_probe_or_helper_request(schema_title.as_bytes()), "title schema request");
        let ordinary = r#"{"model":"m","system":"You are Claude Code.","messages":[{"role":"user","content":"Return a short answer"}]}"#;
        assert!(!is_claude_probe_or_helper_request(ordinary.as_bytes()), "ordinary request");
        let escaped_1h = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert!(claude_payload_has_1h_ttl(escaped_1h.as_bytes()), "escaped 1h ttl");
        let five_minutes = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"took 11h","cache_control":{"type":"ephemeral","ttl":"5m"}}]}]}"#;
        assert!(!claude_payload_has_1h_ttl(five_minutes.as_bytes()), "5m ttl");
    }
}
