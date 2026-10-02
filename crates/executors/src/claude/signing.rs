//! Claude Code CCH body signing (Go: executor/claude_signing.go).
//!
//! The CCH is an xxHash64 over the final request bytes, so everything here edits and hashes the
//! outgoing body at the byte level and never re-serializes JSON. The raw-edit helpers
//! ([`value_span`], [`sjson_string`], [`set_strings_at`]) are `pub(crate)` for the sibling
//! modules that must preserve bytes the same way (sensitive-word obfuscation).

use std::borrow::Cow;
use std::ops::Range;

use cpa_auth::Auth;
use cpa_config::{ClaudeKey, CloakConfig, Config};
use cpa_core::util::{GoJsonStyle, go_json_sorted, go_json_string, strip_claude_code_attribution_system};
use cpa_json::Value;
use url::Url;

use super::helps::upstream::is_anthropic_upstream_base;

/// xxHash64 seed of Claude Code's body hash.
pub const CLAUDE_CCH_SEED: u64 = 0x4D65_9218_E32A_3268;
/// Number of hex digits in the CCH.
pub const CLAUDE_CCH_LENGTH: usize = 5;
/// Placeholder digits hashed in place of the CCH.
pub const CLAUDE_CCH_ZERO: &str = "00000";

const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
/// Nesting limit for the byte scanner (matches `cpa_json::MAX_DEPTH`).
const MAX_DEPTH: usize = 1000;

/// Which kind of upstream a request is built for (Go: `claudeCCHUpstreamKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeCchUpstreamKind {
    Other,
    Anthropic,
    Vertex,
}

/// Failure while placing or computing a CCH (Go: plain `fmt.Errorf` values). Callers wrap it, e.g.
/// "finalize Claude CCH: ...".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ClaudeCchError(pub String);

impl ClaudeCchError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

// ---------------------------------------------------------------------------------------------
// xxHash64

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn read_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

fn read_u32(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_le_bytes(a)
}

fn xx_round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn xx_merge(acc: u64, val: u64) -> u64 {
    (acc ^ xx_round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

/// XXH64 of `data` (the algorithm behind Go's `pierrec/xxHash/xxHash64`).
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut rest = data;
    let mut h = if data.len() >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while rest.len() >= 32 {
            v1 = xx_round(v1, read_u64(&rest[0..]));
            v2 = xx_round(v2, read_u64(&rest[8..]));
            v3 = xx_round(v3, read_u64(&rest[16..]));
            v4 = xx_round(v4, read_u64(&rest[24..]));
            rest = &rest[32..];
        }
        let mut h = v1.rotate_left(1).wrapping_add(v2.rotate_left(7)).wrapping_add(v3.rotate_left(12)).wrapping_add(v4.rotate_left(18));
        for v in [v1, v2, v3, v4] {
            h = xx_merge(h, v);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(data.len() as u64);
    while rest.len() >= 8 {
        h ^= xx_round(0, read_u64(rest));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        h ^= u64::from(read_u32(rest)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = &rest[4..];
    }
    for &b in rest {
        h ^= u64::from(b).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

// ---------------------------------------------------------------------------------------------
// Raw byte edits (sjson-equivalent, without re-serializing the rest of the body)

/// Byte range of the value at plain dotted `path` in `body` (first match, like gjson `Index`).
pub(crate) fn value_span(body: &[u8], path: &str) -> Option<Range<usize>> {
    let raw = cpa_json::raw_at(body, path)?;
    let start = (raw.as_ptr() as usize).checked_sub(body.as_ptr() as usize)?;
    Some(start..start + raw.len())
}

/// sjson's string encoding for `SetBytes`: strings with `"`, `\`, control or non-ASCII bytes go
/// through `json.Marshal` (HTML-escaped); anything else is quoted verbatim (so `<` stays literal).
pub(crate) fn sjson_string(s: &str) -> String {
    if s.bytes().any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\') {
        go_json_string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// `json.Encoder` with `SetEscapeHTML(false)` over a string (Go: `marshalJSONStringWithoutHTMLEscape`).
fn json_string_no_html(s: &str) -> String {
    go_json_sorted(&Value::String(s.to_string()), GoJsonStyle::NO_HTML_ESCAPE).unwrap_or_else(|| format!("\"{s}\""))
}

/// Sets existing string values in place (sjson `SetBytes` on paths that exist), leaving every
/// other byte untouched. Paths that do not exist are skipped. All paths are resolved against the
/// original body, which is equivalent to sequential sets because only leaf strings change.
pub(crate) fn set_strings_at(body: &[u8], edits: &[(String, String)]) -> Vec<u8> {
    let mut spans: Vec<(Range<usize>, String)> =
        edits.iter().filter_map(|(path, value)| value_span(body, path).map(|span| (span, sjson_string(value)))).collect();
    if spans.is_empty() {
        return body.to_vec();
    }
    spans.sort_by_key(|(span, _)| span.start);
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for (span, text) in spans {
        if span.start < last {
            continue;
        }
        out.extend_from_slice(&body[last..span.start]);
        out.extend_from_slice(text.as_bytes());
        last = span.end;
    }
    out.extend_from_slice(&body[last..]);
    out
}

fn splice(body: &[u8], span: Range<usize>, replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() - span.len() + replacement.len());
    out.extend_from_slice(&body[..span.start]);
    out.extend_from_slice(replacement);
    out.extend_from_slice(&body[span.end..]);
    out
}

/// sjson `SetRawBytes` for a top-level key: replaces the existing value, or appends the member
/// before the closing brace of the (whitespace-trimmed) root object.
pub(crate) fn set_raw_top_level(body: &[u8], key: &str, raw: &[u8]) -> Result<Vec<u8>, ClaudeCchError> {
    if let Some(span) = value_span(body, key) {
        return Ok(splice(body, span, raw));
    }
    let start = body.iter().position(|b| !is_json_space(*b));
    let root: &[u8] = match start {
        None => b"{}",
        Some(start) => {
            let mut scanner = Scanner { body, pos: start, edits: Vec::new() };
            scanner.parse_value(false, 0).map_err(|_| ClaudeCchError::new("json must be an object or array"))?;
            &body[start..scanner.pos]
        }
    };
    if root.first() != Some(&b'{') {
        return Err(ClaudeCchError::new("json must be an object or array"));
    }
    let end = root.len() - 1;
    let comma = root[1..].iter().find(|b| **b > b' ').is_some_and(|b| *b != b'}' && *b != b']');
    let mut out = Vec::with_capacity(root.len() + key.len() + raw.len() + 6);
    out.extend_from_slice(&root[..end]);
    if comma {
        out.push(b',');
    }
    out.extend_from_slice(sjson_string(key).as_bytes());
    out.push(b':');
    out.extend_from_slice(raw);
    out.push(b'}');
    Ok(out)
}

fn is_json_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// gjson-style view of `system.0.text`: raw span plus the decoded text and whether it is a string.
struct SystemText {
    span: Range<usize>,
    text: String,
    is_string: bool,
}

fn system_first_text(body: &[u8]) -> Option<SystemText> {
    let span = value_span(body, "system.0.text")?;
    let raw = std::str::from_utf8(&body[span.clone()]).unwrap_or_default();
    let is_string = raw.starts_with('"');
    let text = if is_string { cpa_json::parse_str(raw).as_str().unwrap_or_default().to_string() } else { raw.to_string() };
    Some(SystemText { span, text, is_string })
}

fn is_billing_text(system_text: &Option<SystemText>) -> bool {
    system_text.as_ref().is_some_and(|t| t.is_string && t.text.starts_with(BILLING_PREFIX))
}

/// `{"type":"text","text":...}` without HTML escaping (Go: `buildTextBlock` with no cache control).
fn build_text_block(text: &str) -> String {
    format!(r#"{{"type":"text","text":{}}}"#, json_string_no_html(text))
}

// ---------------------------------------------------------------------------------------------
// Placeholder and signing

/// Inserts the billing block fallback and CCH placeholder when missing, then signs the body
/// (Go: `finalizeAnthropicMessagesBodyCCH`). `fallback_billing` may be empty.
pub fn finalize_anthropic_messages_body_cch(body: &[u8], fallback_billing: &str) -> Result<Vec<u8>, ClaudeCchError> {
    let with_placeholder = ensure_claude_billing_header_cch_placeholder(body, fallback_billing)?;
    sign_anthropic_messages_body(&with_placeholder)
}

/// Whether a confirmed native helper request still needs CPA's billing-header fallback (Go:
/// `claudeBodyNeedsBillingFallback`): true exactly when the body carries a `system` field.
pub fn claude_body_needs_billing_fallback(body: &[u8]) -> bool {
    cpa_json::raw_at(body, "system").is_some()
}

/// Ensures `system.0.text` is a billing block with a `cch=00000;` placeholder after
/// `cc_entrypoint=...;` (Go: `ensureClaudeBillingHeaderCCHPlaceholder`). A missing billing block is
/// prepended from `fallback_billing` when that is non-empty.
pub fn ensure_claude_billing_header_cch_placeholder(body: &[u8], fallback_billing: &str) -> Result<Vec<u8>, ClaudeCchError> {
    let mut body: Cow<'_, [u8]> = Cow::Borrowed(body);
    let mut billing = system_first_text(&body);
    if !is_billing_text(&billing) {
        if fallback_billing.is_empty() {
            return Ok(body.into_owned());
        }
        body = Cow::Owned(prepend_claude_billing_system_block(&body, fallback_billing)?);
        billing = system_first_text(&body);
    }
    if claude_billing_cch_digits_offset(&body).is_some() {
        return Ok(body.into_owned());
    }

    let Some(billing) = billing else { return Ok(body.into_owned()) };
    let text = &billing.text;
    let Some(entrypoint) = text.find("cc_entrypoint=") else { return Ok(body.into_owned()) };
    let Some(entrypoint_end) = text[entrypoint..].find(';') else { return Ok(body.into_owned()) };
    let insert_at = entrypoint + entrypoint_end + 1;
    let updated = format!("{} cch=00000;{}", &text[..insert_at], &text[insert_at..]);
    Ok(splice(&body, billing.span, sjson_string(&updated).as_bytes()))
}

/// Prepends `billing_text` as a text block of `system` (Go: `prependClaudeBillingSystemBlock`): a
/// string system becomes two blocks, an array gets the block in front, anything else is replaced.
fn prepend_claude_billing_system_block(body: &[u8], billing_text: &str) -> Result<Vec<u8>, ClaudeCchError> {
    let billing_block = build_text_block(billing_text);
    let system_raw = cpa_json::raw_at(body, "system");
    let mut array: Vec<u8> = Vec::new();
    match system_raw.map(|r| (r, r.as_bytes().first().copied())) {
        Some((raw, Some(b'"'))) => {
            let original = cpa_json::parse_str(raw);
            let original_block = build_text_block(original.as_str().unwrap_or_default());
            array.push(b'[');
            array.extend_from_slice(billing_block.as_bytes());
            array.push(b',');
            array.extend_from_slice(original_block.as_bytes());
            array.push(b']');
        }
        Some((raw, Some(b'['))) => {
            let trimmed = raw.trim_matches(|c: char| c.is_whitespace());
            array.push(b'[');
            array.extend_from_slice(billing_block.as_bytes());
            if trimmed == "[]" {
                array.push(b']');
            } else {
                array.push(b',');
                array.extend_from_slice(&trimmed.as_bytes()[1..]);
            }
        }
        _ => {
            array.push(b'[');
            array.extend_from_slice(billing_block.as_bytes());
            array.push(b']');
        }
    }
    set_raw_top_level(body, "system", &array).map_err(|e| ClaudeCchError::new(format!("prepend Claude CCH billing block: {e}")))
}

/// Whether CPA signs the outgoing body with a CCH (Go: `claudeCCHSigningEnabled`).
///
/// A Claude OAuth credential always signs (CPA restores the first-party shape a downstream Claude
/// Code omitted). Vertex always signs. Everything else signs only when it opted into the CLI
/// profile and `origin` (the concrete request URL) is first-party Anthropic.
pub fn claude_cch_signing_enabled(api_key: &str, kind: ClaudeCchUpstreamKind, cli_fingerprint: bool, origin: &str) -> bool {
    if is_claude_oauth_token(api_key) {
        return true;
    }
    if kind == ClaudeCchUpstreamKind::Vertex {
        return true;
    }
    if !cli_fingerprint {
        return false;
    }
    kind == ClaudeCchUpstreamKind::Anthropic && is_anthropic_upstream_base(origin)
}

/// Whether an API key is a Claude OAuth access token (Go: `isClaudeOAuthToken`).
pub(crate) fn is_claude_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Reproduces Claude Code's final-body CCH; only the five CCH digits of the body change (Go:
/// `signAnthropicMessagesBody`). Bodies without a billing CCH placeholder are returned unchanged.
pub fn sign_anthropic_messages_body(body: &[u8]) -> Result<Vec<u8>, ClaudeCchError> {
    let Some(offset) = claude_billing_cch_digits_offset(body) else { return Ok(body.to_vec()) };

    let mut unsigned = body.to_vec();
    unsigned[offset..offset + CLAUDE_CCH_LENGTH].copy_from_slice(CLAUDE_CCH_ZERO.as_bytes());
    let normalized = normalize_claude_cch_input(&unsigned).map_err(|e| ClaudeCchError::new(format!("normalize Claude CCH input: {e}")))?;
    let cch = format!("{:05x}", xxh64(&normalized, CLAUDE_CCH_SEED) & 0xF_FFFF);
    unsigned[offset..offset + CLAUDE_CCH_LENGTH].copy_from_slice(cch.as_bytes());
    Ok(unsigned)
}

/// Offset in `body` of the five CCH digits inside the billing block of `system.0.text` (Go:
/// `claudeBillingCCHDigitsOffset`). The digits are located in the raw JSON string, followed by `;`.
pub fn claude_billing_cch_digits_offset(body: &[u8]) -> Option<usize> {
    let billing = system_first_text(body)?;
    if !billing.is_string || !billing.text.starts_with(BILLING_PREFIX) {
        return None;
    }

    let raw = &body[billing.span.clone()];
    let mut search_from = 0;
    while search_from < raw.len() {
        let relative = find_bytes(&raw[search_from..], b"cch=")?;
        let prefix = search_from + relative;
        let digits = prefix + 4;
        let end = digits + CLAUDE_CCH_LENGTH;
        if end < raw.len() && raw[end] == b';' && is_lower_hex(&raw[digits..end]) {
            return Some(billing.span.start + digits);
        }
        search_from = prefix + 4;
    }
    None
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn is_lower_hex(value: &[u8]) -> bool {
    value.len() == CLAUDE_CCH_LENGTH && value.iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

// ---------------------------------------------------------------------------------------------
// Hash view normalization (byte-level JSON scanner)

struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<Range<usize>>,
}

/// Builds the hash view of a body without reserializing JSON (Go: `normalizeClaudeCCHInput`):
/// `model` string values are emptied and the dispatch-only members `max_tokens`, `fallbacks` and
/// `fallback_credit_token` are removed at every nesting level.
pub fn normalize_claude_cch_input(body: &[u8]) -> Result<Vec<u8>, ClaudeCchError> {
    if !json_valid(body) {
        return Err(ClaudeCchError::new("invalid JSON body"));
    }

    let mut scanner = Scanner { body, pos: 0, edits: Vec::new() };
    scanner.parse_value(true, 0)?;
    scanner.skip_whitespace();
    if scanner.pos != body.len() {
        return Err(ClaudeCchError::new(format!("unexpected JSON data at byte {}", scanner.pos)));
    }

    let mut edits = scanner.edits;
    edits.sort_by_key(|e| e.start);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for edit in edits {
        if edit.start < last || edit.end > body.len() {
            return Err(ClaudeCchError::new(format!("overlapping CCH normalization edit at byte {}", edit.start)));
        }
        normalized.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

impl Scanner<'_> {
    fn parse_value(&mut self, collect: bool, depth: usize) -> Result<(), ClaudeCchError> {
        self.skip_whitespace();
        if self.pos >= self.body.len() {
            return Err(ClaudeCchError::new(format!("missing JSON value at byte {}", self.pos)));
        }
        if depth > MAX_DEPTH {
            return Err(ClaudeCchError::new("JSON nesting too deep"));
        }

        match self.body[self.pos] {
            b'{' => self.parse_object(collect, depth),
            b'[' => self.parse_array(collect, depth),
            b'"' => self.parse_string().map(|_| ()),
            _ => {
                let start = self.pos;
                while self.pos < self.body.len() {
                    if matches!(self.body[self.pos], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(ClaudeCchError::new(format!("missing JSON value at byte {start}")));
                }
                Ok(())
            }
        }
    }

    fn parse_object(&mut self, collect: bool, depth: usize) -> Result<(), ClaudeCchError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }

        let mut members: Vec<Member> = Vec::new();
        let mut comma_before: Option<usize> = None;
        loop {
            self.skip_whitespace();
            let member_start = self.pos;
            let (key_start, key_end) = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(ClaudeCchError::new(format!("missing object colon at byte {}", self.pos)));
            }
            self.skip_whitespace();

            let key = &self.body[key_start..key_end];
            let excluded = collect && is_excluded_key(key);
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (value_start, value_end) = self.parse_string()?;
                self.add_edit(value_start + 1, value_end - 1);
            } else {
                self.parse_value(collect && !excluded, depth + 1)?;
            }
            let member_end = self.pos;
            self.skip_whitespace();

            let comma_after = if self.consume(b',') { Some(self.pos - 1) } else { None };
            members.push(Member { start: member_start, end: member_end, comma_before, comma_after, excluded });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.consume(b'}') {
                return Err(ClaudeCchError::new(format!("missing object end at byte {}", self.pos)));
            }
            break;
        }

        if collect {
            self.add_excluded_member_edits(&members);
        }
        Ok(())
    }

    fn parse_array(&mut self, collect: bool, depth: usize) -> Result<(), ClaudeCchError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(collect, depth + 1)?;
            self.skip_whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(ClaudeCchError::new(format!("missing array end at byte {}", self.pos)));
            }
            return Ok(());
        }
    }

    /// Returns `(start, end)` of the string including quotes.
    fn parse_string(&mut self) -> Result<(usize, usize), ClaudeCchError> {
        if self.body.get(self.pos) != Some(&b'"') {
            return Err(ClaudeCchError::new(format!("missing JSON string at byte {}", self.pos)));
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(ClaudeCchError::new(format!("unterminated JSON string at byte {start}")))
    }

    /// Removes each run of consecutive excluded members, reproducing Claude Code's comma handling
    /// (including the quirk that a trailing run of two or more leaves the preceding comma).
    fn add_excluded_member_edits(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            if end + 1 < members.len() {
                // A following member exists, so the run's last member has a comma after it.
                let comma_after = members[end].comma_after.map_or(0, |c| c + 1);
                self.add_edit(members[start].start, comma_after);
            } else if start > 0 && end > start {
                self.add_edit(members[start].start, members[end].end);
            } else if start > 0 {
                self.add_edit(members[start].comma_before.unwrap_or(0), members[end].end);
            } else {
                self.add_edit(members[start].start, members[end].end);
            }
            start = end + 1;
        }
    }

    fn add_edit(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.edits.push(start..end);
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.body.len() && is_json_space(self.body[self.pos]) {
            self.pos += 1;
        }
    }

    fn consume(&mut self, c: u8) -> bool {
        if self.body.get(self.pos) != Some(&c) {
            return false;
        }
        self.pos += 1;
        true
    }
}

fn is_excluded_key(key: &[u8]) -> bool {
    matches!(key, b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\"")
}

// ---------------------------------------------------------------------------------------------
// JSON validity (Go: json.Valid, which accepts lone surrogate escapes and arbitrary bytes in strings)

pub(crate) fn json_valid(body: &[u8]) -> bool {
    let mut v = Validator { b: body, i: 0 };
    v.ws();
    v.value(0) && {
        v.ws();
        v.i == body.len()
    }
}

struct Validator<'a> {
    b: &'a [u8],
    i: usize,
}

impl Validator<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && is_json_space(self.b[self.i]) {
            self.i += 1;
        }
    }

    fn lit(&mut self, s: &[u8]) -> bool {
        if self.b[self.i..].starts_with(s) {
            self.i += s.len();
            true
        } else {
            false
        }
    }

    fn digits(&mut self) -> bool {
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        self.i > start
    }

    fn value(&mut self, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        match self.b.get(self.i) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string(),
            Some(b't') => self.lit(b"true"),
            Some(b'f') => self.lit(b"false"),
            Some(b'n') => self.lit(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }

    fn number(&mut self) -> bool {
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return false,
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !self.digits() {
                return false;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !self.digits() {
                return false;
            }
        }
        true
    }

    fn string(&mut self) -> bool {
        self.i += 1;
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'"' => {
                    self.i += 1;
                    return true;
                }
                b'\\' => {
                    match self.b.get(self.i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 2,
                        Some(b'u') => {
                            let hex = self.b.get(self.i + 2..self.i + 6);
                            if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                                return false;
                            }
                            self.i += 6;
                        }
                        _ => return false,
                    };
                }
                c if c < 0x20 => return false,
                _ => self.i += 1,
            }
        }
        false
    }

    fn array(&mut self, depth: usize) -> bool {
        self.i += 1;
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return true;
        }
        loop {
            self.ws();
            if !self.value(depth + 1) {
                return false;
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }

    fn object(&mut self, depth: usize) -> bool {
        self.i += 1;
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return true;
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') || !self.string() {
                return false;
            }
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return false;
            }
            self.i += 1;
            self.ws();
            if !self.value(depth + 1) {
                return false;
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Kimi attribution and key config lookups (same Go file)

/// Whether an endpoint host is Kimi's API (Go: `isKimiAPIEndpoint`).
pub fn is_kimi_api_endpoint(endpoint: &str) -> bool {
    match Url::parse(endpoint.trim()) {
        Ok(parsed) => parsed.host_str().is_some_and(|h| h.eq_ignore_ascii_case("api.kimi.com") || h.eq_ignore_ascii_case("api.kimi.ai")),
        Err(_) => false,
    }
}

/// Whether the request goes to Kimi by provider id or endpoint (Go: `isKimiMessagesUpstream`).
pub fn is_kimi_messages_upstream(auth: Option<&Auth>, endpoint: &str) -> bool {
    if let Some(auth) = auth {
        let provider = auth.provider.trim().to_lowercase();
        if matches!(provider.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com") {
            return true;
        }
    }
    is_kimi_api_endpoint(endpoint)
}

/// Removes the Claude Code billing/CCH attribution block from a Kimi Messages body unless the
/// caller opted into the full CLI profile (Go: `stripDefaultKimiClaudeCodeAttribution`).
pub fn strip_default_kimi_claude_code_attribution(auth: Option<&Auth>, endpoint: &str, cli_fingerprint: bool, body: &[u8]) -> Vec<u8> {
    if cli_fingerprint || !is_kimi_messages_upstream(auth, endpoint) {
        return body.to_vec();
    }
    strip_claude_code_attribution_system(body)
}

/// Credentials of a Claude auth: `(api_key, base_url)` from attributes, falling back to the
/// `access_token` metadata entry for the key (Go: `claudeCreds`). Local copy for this module.
pub(crate) fn claude_creds(auth: &Auth) -> (String, String) {
    let api_key = auth.attributes.get("api_key").cloned().unwrap_or_default();
    let base_url = auth.attributes.get("base_url").cloned().unwrap_or_default();
    if api_key.is_empty() {
        let token = auth.metadata.get("access_token").and_then(Value::as_str).unwrap_or_default().to_string();
        return (token, base_url);
    }
    (api_key, base_url)
}

/// The `claude-api-key` config entry matching an auth's key (and base URL when both set) (Go:
/// `resolveClaudeKeyConfig`).
pub fn resolve_claude_key_config<'a>(cfg: &'a Config, auth: &Auth) -> Option<&'a ClaudeKey> {
    let (api_key, base_url) = claude_creds(auth);
    if api_key.is_empty() {
        return None;
    }
    cfg.claude_key.iter().find(|entry| {
        let cfg_key = entry.api_key.trim();
        let cfg_base = entry.base_url.trim();
        if cfg_key.to_lowercase() != api_key.to_lowercase() {
            return false;
        }
        !(!base_url.is_empty() && !cfg_base.is_empty() && cfg_base.to_lowercase() != base_url.to_lowercase())
    })
}

/// Cloak config of the matching `claude-api-key` entry (Go: `resolveClaudeKeyCloakConfig`).
pub fn resolve_claude_key_cloak_config<'a>(cfg: &'a Config, auth: &Auth) -> Option<&'a CloakConfig> {
    resolve_claude_key_config(cfg, auth)?.cloak.as_ref()
}

/// Whether mid-conversation system messages are rebuilt into top-level system, by auth attribute
/// `rebuild_mid_system_message` or the matching config entry (Go: `rebuildMidSystemMessageEnabled`).
pub fn rebuild_mid_system_message_enabled(cfg: &Config, auth: &Auth) -> bool {
    if auth.attributes.get("rebuild_mid_system_message").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return true;
    }
    resolve_claude_key_config(cfg, auth).is_some_and(|entry| entry.rebuild_mid_system_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

    fn cch_of(body: &[u8]) -> String {
        let offset = claude_billing_cch_digits_offset(body).expect("billing CCH present");
        String::from_utf8_lossy(&body[offset..offset + CLAUDE_CCH_LENGTH]).into_owned()
    }

    fn meta(replacement: &str) -> String {
        BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, &format!(r#""metadata":{{{replacement}}}"#), 1)
    }

    #[test]
    fn xxh64_published_vectors() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        assert_eq!(xxh64(b"Nobody inspects the spammish repetition", 0), 0xFBCE_A83C_8A37_8BF1);
    }

    #[test]
    fn sign_known_vectors() {
        let order_body = r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#;
        let cases: Vec<(&str, String, &str)> = vec![
            ("base", BASE.to_string(), "7ee87"),
            ("model value ignored", BASE.replacen(r#""model":"model-a""#, r#""model":"model-b""#, 1), "7ee87"),
            ("max tokens ignored", BASE.replacen(r#""max_tokens":1"#, r#""max_tokens":2"#, 1), "7ee87"),
            ("message changes hash", BASE.replacen(r#""text":"x""#, r#""text":"y""#, 1), "b9cc8"),
            ("system changes hash", BASE.replacen(r#""system-x""#, r#""system-y""#, 1), "a30d3"),
            ("metadata changes hash", BASE.replacen(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#, 1), "7a89d"),
            (
                "thinking changes hash",
                BASE.replacen(r#""thinking":{"type":"adaptive","display":"omitted"}"#, r#""thinking":{"type":"disabled"}"#, 1),
                "7205c",
            ),
            (
                "context changes hash",
                BASE.replacen(
                    r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#,
                    r#""context_management":{"edits":[]}"#,
                    1,
                ),
                "05073",
            ),
            ("effort changes hash", BASE.replacen(r#""effort":"high""#, r#""effort":"low""#, 1), "12366"),
            ("stream changes hash", BASE.replacen(r#""stream":true"#, r#""stream":false"#, 1), "60400"),
            (
                "tool changes hash",
                BASE.replacen(r#""tools":[]"#, r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#, 1),
                "3d78d",
            ),
            ("extra field changes hash", BASE.replacen(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#, 1), "2d622"),
            ("field order remains significant", order_body.to_string(), "e5b6c"),
            ("nested model value ignored", meta(r#""user_id":"meta-x","model":"a""#), "0601b"),
            ("nested max tokens member omitted", meta(r#""user_id":"meta-x","max_tokens":2"#), "7ee87"),
            ("top level fallbacks member omitted", BASE.replacen(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#, 1), "7ee87"),
            ("nested fallbacks member omitted", meta(r#""user_id":"meta-x","fallbacks":[{"model":"nested-a"}]"#), "7ee87"),
            ("top level fallback credit token omitted", BASE.replacen(r#""stream":true}"#, r#""stream":true,"fallback_credit_token":"a"}"#, 1), "7ee87"),
            ("nested fallback credit token omitted", meta(r#""user_id":"meta-x","fallback_credit_token":"a""#), "7ee87"),
            (
                "trailing dispatch run keeps native comma",
                meta(r#""user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]"#),
                "4589b",
            ),
            (
                "model before trailing dispatch run",
                meta(r#""user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token""#),
                "2d312",
            ),
            (
                "model splits dispatch runs",
                meta(r#""user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]"#),
                "0601b",
            ),
            ("ordinary nested member remains", meta(r#""user_id":"meta-x","plain":"a""#), "8d74c"),
            (
                "billing block only",
                r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_string(),
                "f2edb",
            ),
        ];
        for (name, body, want) in cases {
            let signed = sign_anthropic_messages_body(body.as_bytes()).expect("sign");
            assert_eq!(cch_of(&signed), want, "{name}\nbody: {}", String::from_utf8_lossy(&signed));
        }
    }

    #[test]
    fn signing_preserves_final_serialized_bytes() {
        let literal = "keep literal cch=00000; in the message";
        let body = BASE.replacen(r#""text":"x""#, &format!(r#""text":"{literal}""#), 1);
        let signed = sign_anthropic_messages_body(body.as_bytes()).expect("sign");
        let parsed = cpa_json::parse(&signed);
        assert_eq!(cpa_json::J::g(&parsed, "messages.0.content.0.text").str(), literal);

        let offset = claude_billing_cch_digits_offset(&signed).expect("signed CCH");
        let mut unsigned = signed.clone();
        unsigned[offset..offset + CLAUDE_CCH_LENGTH].copy_from_slice(CLAUDE_CCH_ZERO.as_bytes());
        assert_eq!(unsigned, body.as_bytes());
    }

    #[test]
    fn finalize_inserts_missing_placeholder() {
        let body = BASE.replacen(" cch=00000;", "", 1);
        let signed = finalize_anthropic_messages_body_cch(body.as_bytes(), "").expect("finalize");
        assert_eq!(cch_of(&signed), "7ee87");
        let parsed = cpa_json::parse(&signed);
        let billing = cpa_json::J::g(&parsed, "system.0.text").str();
        assert!(billing.contains("cc_entrypoint=sdk-cli; cch=7ee87;"), "{billing}");
    }

    #[test]
    fn finalize_adds_missing_billing_block() {
        use cpa_json::J;
        let body = br#"{"model":"claude-opus-4-6","system":"keep this system text","messages":[{"role":"user","content":"hello"}],"max_tokens":128}"#;
        let fallback = "x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;";
        let signed = finalize_anthropic_messages_body_cch(body, fallback).expect("finalize");
        let parsed = cpa_json::parse(&signed);
        assert!(parsed.g("system.0.text").str().starts_with("x-anthropic-billing-header:"));
        assert_eq!(parsed.g("system.1.text").str(), "keep this system text");
        assert!(claude_billing_cch_digits_offset(&signed).is_some());
        // The edit is byte-level: bytes before `system` are untouched.
        assert!(signed.starts_with(br#"{"model":"claude-opus-4-6","system":[{"type":"text","text":"x-anthropic-billing-header:"#));
    }

    #[test]
    fn prepend_billing_block_shapes() {
        let fallback = "x-anthropic-billing-header: cc_entrypoint=cli;";
        let block = r#"{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"}"#;
        let cases = [
            (r#"{"a":1}"#, format!(r#"{{"a":1,"system":[{block}]}}"#)),
            (r#"{"system":[],"a":1}"#, format!(r#"{{"system":[{block}],"a":1}}"#)),
            (r#"{"system":[{"type":"text","text":"s"}]}"#, format!(r#"{{"system":[{block},{{"type":"text","text":"s"}}]}}"#)),
            (r#"{"system":{"k":1}}"#, format!(r#"{{"system":[{block}]}}"#)),
            (r#"{"system":"a<b"}"#, format!(r#"{{"system":[{block},{{"type":"text","text":"a<b"}}]}}"#)),
            // A missing key is appended to the whitespace-trimmed root.
            ("  {\n \"a\": 1 }  ", format!("{{\n \"a\": 1 ,\"system\":[{block}]}}")),
            ("{}", format!(r#"{{"system":[{block}]}}"#)),
            // `[ ]` is not byte-equal to `[]`, so the Go code splices it as-is (invalid JSON).
            (r#"{"system":[ ],"a":1}"#, format!(r#"{{"system":[{block}, ],"a":1}}"#)),
        ];
        for (body, want) in cases {
            let got = prepend_claude_billing_system_block(body.as_bytes(), fallback).expect("prepend");
            assert_eq!(String::from_utf8_lossy(&got), want, "{body}");
        }
    }

    /// Oracle value from the Go `finalizeAnthropicMessagesBodyCCH`.
    #[test]
    fn finalize_string_system_vector() {
        let body = br#"{"system":"a<b","messages":[]}"#;
        let signed = finalize_anthropic_messages_body_cch(body, "x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli;").expect("finalize");
        assert_eq!(
            String::from_utf8_lossy(&signed),
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=83957;"},{"type":"text","text":"a<b"}],"messages":[]}"#
        );
    }

    #[test]
    fn signing_enabled_policy() {
        const ANTHROPIC: &str = "https://api.anthropic.com/v1/messages?beta=true";
        const GATEWAY: &str = "https://gateway.example/v1/messages?beta=true";
        use ClaudeCchUpstreamKind::{Anthropic, Other, Vertex};
        let cases = [
            ("official API key default", "key-123", Anthropic, false, ANTHROPIC, false),
            ("official API key opt-in", "key-123", Anthropic, true, ANTHROPIC, true),
            ("Kimi API key default", "key-123", Anthropic, false, "https://api.kimi.com/v1/messages", false),
            ("Kimi API key opt-in", "key-123", Anthropic, true, "https://api.kimi.com/v1/messages", false),
            ("gateway API key opt-in", "key-123", Anthropic, true, GATEWAY, false),
            ("anthropic host over http opt-in", "key-123", Anthropic, true, "http://api.anthropic.com/v1/messages", false),
            ("anthropic host explicit 443 opt-in", "key-123", Anthropic, true, "https://api.anthropic.com:443/v1/messages", true),
            ("anthropic lookalike host opt-in", "key-123", Anthropic, true, "https://api.anthropic.com.evil.example/v1/messages", false),
            ("Claude OAuth", "sk-ant-oat-custom", Anthropic, false, ANTHROPIC, true),
            ("Claude OAuth custom gateway", "sk-ant-oat-custom", Anthropic, false, GATEWAY, true),
            ("other provider Claude OAuth", "sk-ant-oat-other", Other, false, GATEWAY, true),
            ("Vertex provider API key", "key-123", Vertex, false, "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/l/publishers/anthropic/models/m:streamRawPredict", true),
            ("other provider API key", "key-123", Other, false, GATEWAY, false),
            ("other provider API key opt-in", "key-123", Other, true, GATEWAY, false),
        ];
        for (name, key, kind, cli, origin, want) in cases {
            assert_eq!(claude_cch_signing_enabled(key, kind, cli, origin), want, "{name}");
        }
    }

    #[test]
    fn normalize_preserves_raw_json() {
        let cases = [
            (r#"{"model":"claude","keep":1}"#, r#"{"model":"","keep":1}"#),
            (r#"{"max_tokens":1,"keep":2}"#, r#"{"keep":2}"#),
            (r#"{"keep":1,"fallbacks":[{"model":"x"}],"tail":2}"#, r#"{"keep":1,"tail":2}"#),
            (r#"{"keep":1,"fallback_credit_token":"secret"}"#, r#"{"keep":1}"#),
            (r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"secret"}"#, "{}"),
            (r#"{"keep":1,"max_tokens":1,"fallbacks":[],"tail":2}"#, r#"{"keep":1,"tail":2}"#),
            (r#"{"keep":1,"max_tokens":1,"fallbacks":[]}"#, r#"{"keep":1,}"#),
            (r#"{"outer":{"model":"x","max_tokens":1,"keep":"y"}}"#, r#"{"outer":{"model":"","keep":"y"}}"#),
            (
                r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
                r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
            ),
        ];
        for (body, want) in cases {
            let got = normalize_claude_cch_input(body.as_bytes()).expect("normalize");
            assert_eq!(String::from_utf8_lossy(&got), want, "{body}");
        }
    }

    #[test]
    fn json_validity_matches_go() {
        assert!(json_valid(br#"{"a":"\ud83d","b":[1,-2.5e3,true,null]}"#));
        assert!(!json_valid(br#"{"a":1,}"#));
        assert!(!json_valid(br#"{"a":01}"#));
        assert!(!json_valid(b"{\"a\":\"x\ny\"}"));
        assert!(!json_valid(br#"{"a":"\x"}"#));
        assert!(!json_valid(br#"{"a":1} x"#));
        assert!(normalize_claude_cch_input(b"{\"a\":").is_err());
    }
}
