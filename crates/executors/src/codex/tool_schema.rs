//! Codex tool schema normalization (Go: helps/codex_tool_schema.go `NormalizeCodexToolSchemas`).
//!
//! Function tools in a Codex request are cleaned so strict upstream validators accept them:
//! `pattern` attributes using unsupported regex escapes (`\p{..}`, `\P{..}`, `\0`) are dropped,
//! and large pure-constant `oneOf`/`anyOf` unions (as MCP servers emit for enums with
//! descriptions) collapse into `enum` lists. Number-to-integer rewriting is deliberately not
//! done here (see `helps::codex_tool_integers`).
//!
//! The Go code edits the request as raw text (gjson reads, sjson splices) so untouched bytes
//! keep their original whitespace, number spelling and duplicate keys. [`gj`] ports the small
//! gjson/sjson subset needed for that, including their tolerance for malformed input.

use cpa_core::util::{
    GoJsonStyle, SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS, go_json_sorted, has_unsupported_unicode_property_escape,
};
use gj::{Key, Res, Ty};
use serde_json::Value;

/// Minimum number of union branches (oneOf / anyOf) before a pure constant union is rewritten
/// into an enum.
const CODEX_COMPLEX_UNION_BRANCH_THRESHOLD: usize = 8;

/// Normalizes the `tools` array of a Codex request (descending into `namespace` tools).
/// Returns `body` unchanged (same bytes) when nothing needed editing.
pub fn normalize_codex_tool_schemas(body: &[u8]) -> Vec<u8> {
    if body.is_empty() {
        return body.to_vec();
    }
    let Some(tools) = gj::get(body, "tools") else {
        return body.to_vec();
    };
    let Some(updated_tools) = normalize_tool_list(tools) else {
        return body.to_vec();
    };
    match gj::set_raw(body, &[Key::plain("tools")], &updated_tools) {
        Ok(out) => {
            tracing::debug!("codex: normalized tool schemas to prevent upstream failure");
            out
        }
        Err(()) => body.to_vec(),
    }
}

/// Batches changed elements so a long request (or a wide namespace) is copied once rather than
/// once per tool; the gaps between elements are copied verbatim to retain array formatting.
fn normalize_tool_list(tools: Res<'_>) -> Option<Vec<u8>> {
    if !tools.is_array() {
        return None;
    }
    let raw = tools.raw;
    let mut out: Option<Vec<u8>> = None;
    let mut offset = 0usize;
    gj::for_each_array(raw, |start, tool| {
        let Some(updated) = normalize_tool(tool) else {
            return true;
        };
        let out = out.get_or_insert_with(|| Vec::with_capacity(raw.len()));
        out.extend_from_slice(raw.get(offset..start).unwrap_or_default());
        out.extend_from_slice(&updated);
        offset = start + tool.raw.len();
        true
    });
    let mut out = out?;
    out.extend_from_slice(raw.get(offset..).unwrap_or_default());
    Some(out)
}

/// Normalizes one tool declaration; `None` when it is not a function/custom/namespace tool or
/// nothing changed.
fn normalize_tool(tool: Res<'_>) -> Option<Vec<u8>> {
    let tool_type = gj::get(tool.raw, "type").map(|t| t.string()).unwrap_or_default();
    // Namespace tools (e.g. multi-agent nested tools) hold their own tool list.
    if tool_type == "namespace" {
        let updated_tools = normalize_tool_list(gj::get(tool.raw, "tools")?)?;
        return gj::set_raw(tool.raw, &[Key::plain("tools")], &updated_tools).ok();
    }
    if tool_type != "function" && tool_type != "custom" {
        return None;
    }
    let params = gj::get(tool.raw, "parameters")?;
    if !params.is_object() {
        return None;
    }
    let updated_params = normalize_parameters(params)?;
    let updated_tool = gj::set_raw(tool.raw, &[Key::plain("parameters")], &updated_params).ok()?;
    tracing::debug!(
        "codex: normalized schema for tool {} to avoid upstream abort",
        gj::get(tool.raw, "name").map(|n| n.string()).unwrap_or_default()
    );
    Some(updated_tool)
}

/// Strips incompatible patterns from a parameters object and normalizes each property schema;
/// `None` when nothing changed.
fn normalize_parameters(params: Res<'_>) -> Option<Vec<u8>> {
    let mut raw_params = params.raw.to_vec();
    let mut changed = false;

    if let Some(sanitized) = strip_incompatible_patterns_from_json(&raw_params) {
        raw_params = sanitized;
        changed = true;
    }

    // Updates are computed against the pre-edit text and applied one by one; each targets a
    // distinct property so order does not matter.
    let updates: Vec<(Vec<u8>, Vec<u8>)> = match gj::get(&raw_params, "properties") {
        Some(properties) if properties.is_object() => gj::map(properties.raw)
            .into_iter()
            .filter_map(|(name, prop)| Some((name, normalize_property_schema(prop.raw)?)))
            .collect(),
        _ => Vec::new(),
    };
    for (name, updated_prop) in updates {
        let path = [Key::plain("properties"), Key::member(&name)];
        if let Ok(next) = gj::set_raw(&raw_params, &path, &updated_prop) {
            raw_params = next;
            changed = true;
        }
    }
    changed.then_some(raw_params)
}

/// Recursively removes `pattern` attributes that strict upstream validators reject (see
/// `has_unsupported_unicode_property_escape`). Schema-aware: only subschemas under known JSON
/// Schema keyword locations are visited, so `pattern` keys inside user data (description,
/// default, enum) survive. When anything is removed the whole document is re-encoded the way
/// Go's `json.Encoder` would (sorted keys, no HTML escaping); `None` when unchanged or when
/// `raw` is not a single valid JSON value.
fn strip_incompatible_patterns_from_json(raw: &[u8]) -> Option<Vec<u8>> {
    // Fast path: no candidate escape present. Patterns arrive JSON-escaped, so a literal
    // backslash before '0' is `\\0` in the raw bytes.
    const NEEDLES: [&[u8]; 4] = [b"\\p{", b"\\P{", b"\\u", b"\\\\0"];
    if !NEEDLES.iter().any(|n| contains(raw, n)) {
        return None;
    }
    if !cpa_json::valid(raw) {
        return None;
    }
    let mut root = cpa_json::parse(raw);
    if root.is_null() || !strip_incompatible_patterns(&mut root) {
        return None;
    }
    let encoded = go_json_sorted(&root, GoJsonStyle::NO_HTML_ESCAPE)?;
    Some(encoded.trim().as_bytes().to_vec())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Removes incompatible patterns from one schema node and its subschemas; true when changed.
fn strip_incompatible_patterns(v: &mut Value) -> bool {
    let mut changed = false;
    match v {
        Value::Object(schema) => {
            let bad_pattern = matches!(
                schema.get("pattern"),
                Some(Value::String(p)) if has_unsupported_unicode_property_escape(p)
            );
            if bad_pattern {
                schema.shift_remove("pattern");
                changed = true;
            }

            // Regex keys under patternProperties are checked themselves.
            if let Some(Value::Object(pattern_props)) = schema.get_mut("patternProperties") {
                let keys: Vec<String> = pattern_props.keys().cloned().collect();
                for key in keys {
                    if has_unsupported_unicode_property_escape(&key) {
                        pattern_props.shift_remove(&key);
                        changed = true;
                    } else if let Some(sub) = pattern_props.get_mut(&key) {
                        changed |= strip_incompatible_patterns(sub);
                    }
                }
            }

            for map_key in SCHEMA_MAP_KEYWORDS {
                if map_key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = schema.get_mut(map_key) {
                    for sub in sub_map.values_mut() {
                        changed |= strip_incompatible_patterns(sub);
                    }
                }
            }

            for val_key in SCHEMA_VALUE_KEYWORDS {
                match schema.get_mut(val_key) {
                    Some(sub @ Value::Object(_)) => changed |= strip_incompatible_patterns(sub),
                    Some(Value::Array(items)) => {
                        for item in items {
                            changed |= strip_incompatible_patterns(item);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= strip_incompatible_patterns(item);
            }
        }
        _ => {}
    }
    changed
}

/// Rewrites a property whose `oneOf`/`anyOf` is a large union of unique pure `const` branches
/// into an `enum` (or drops the union when an identical `enum` already exists); `None` when
/// the property must stay untouched.
fn normalize_property_schema(prop_raw: &[u8]) -> Option<Vec<u8>> {
    if prop_raw.first() != Some(&b'{') {
        return None;
    }
    let has_one_of = gj::get(prop_raw, "oneOf").is_some();
    let has_any_of = gj::get(prop_raw, "anyOf").is_some();
    // Both on one property: leave untouched to preserve compound constraints.
    if has_one_of && has_any_of {
        return None;
    }
    let union_name = if has_one_of {
        "oneOf"
    } else if has_any_of {
        "anyOf"
    } else {
        return None;
    };

    let union = gj::get(prop_raw, union_name)?;
    if !union.is_array() {
        return None;
    }
    let branches = gj::array(union.raw);
    if branches.len() < CODEX_COMPLEX_UNION_BRANCH_THRESHOLD {
        return None;
    }

    let mut const_raw_values: Vec<&[u8]> = Vec::with_capacity(branches.len());
    let mut const_semantic_keys: Vec<String> = Vec::with_capacity(branches.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::with_capacity(branches.len());
    for branch in &branches {
        let (canonical_key, raw_json) = pure_const_branch(*branch)?;
        // A duplicate semantic value in oneOf violates exclusivity; keep the original schema.
        if !seen.insert(canonical_key.clone()) {
            return None;
        }
        const_semantic_keys.push(canonical_key);
        const_raw_values.push(raw_json);
    }
    if const_raw_values.is_empty() {
        return None;
    }

    let existing_enum = gj::get(prop_raw, "enum");
    if let Some(existing) = existing_enum.filter(|e| e.is_array()) {
        let mut existing_keys = Vec::new();
        for v in gj::array(existing.raw) {
            existing_keys.push(canonical_json_value_key(v)?);
        }
        // Only remove the redundant union when the existing enum is provably identical.
        if equal_canonical_sets(&existing_keys, &const_semantic_keys) {
            return Some(gj::delete(prop_raw, union_name));
        }
        return None;
    }

    // Migrate the pure const union to an enum using raw JSON tokens to avoid any numeric
    // precision loss.
    let raw_enum = [b"[".as_slice(), &const_raw_values.join(&b","[..]), b"]"].concat();
    let with_enum = gj::set_raw(prop_raw, &[Key::plain("enum")], &raw_enum).ok()?;
    Some(gj::delete(&with_enum, union_name))
}

/// For a branch that is exactly `{"const": X}` plus optional `description`/`title`: the
/// canonical key of X and its raw token.
fn pure_const_branch(branch: Res<'_>) -> Option<(String, &[u8])> {
    if !branch.is_object() {
        return None;
    }
    let const_val = gj::get(branch.raw, "const")?;
    // No other schema validation constraints may exist in this branch.
    for (key, _) in gj::map(branch.raw) {
        if key != b"const" && key != b"description" && key != b"title" {
            return None;
        }
    }
    let key = canonical_json_value_key(const_val)?;
    Some((key, const_val.raw))
}

/// Type-tagged semantic key of a scalar: `1` and `1.0` share a key, `"1"` and `1` do not.
/// Arrays and objects have none.
fn canonical_json_value_key(val: Res<'_>) -> Option<String> {
    match val.ty {
        Ty::String => Some(format!("s:{}", val.string())),
        Ty::Number => {
            let raw = String::from_utf8_lossy(val.raw);
            let raw = raw.trim();
            Some(match rat_key(raw) {
                Some(key) => format!("n:{key}"),
                None => format!("n:{raw}"),
            })
        }
        Ty::True => Some("b:true".into()),
        Ty::False => Some("b:false".into()),
        Ty::Null => Some("null".into()),
        Ty::Json => None,
    }
}

/// Canonical form of a decimal literal (`[+-]digits[.digits][e[+-]digits]`), equal for equal
/// rational values; `None` where Go's `big.Rat.SetString` would reject the text. Only compared
/// against other keys, so the exact spelling is irrelevant.
fn rat_key(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    let neg = match b.first() {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let int_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_digits = &s[int_start..i];
    let mut frac_digits = "";
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        frac_digits = &s[frac_start..i];
    }
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    let mut exp: i64 = 0;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        let exp_neg = match b.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let exp_start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if exp_start == i {
            return None;
        }
        for d in &b[exp_start..i] {
            exp = exp.saturating_mul(10).saturating_add(i64::from(d - b'0'));
        }
        if exp > 1_000_000 {
            return None;
        }
        if exp_neg {
            exp = -exp;
        }
    }
    if i != b.len() {
        return None;
    }
    let mut mantissa = format!("{int_digits}{frac_digits}");
    exp -= frac_digits.len() as i64;
    let trimmed = mantissa.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some("0".into());
    }
    mantissa = trimmed.to_string();
    let without_zeros = mantissa.trim_end_matches('0');
    exp += (mantissa.len() - without_zeros.len()) as i64;
    Some(format!("{}{without_zeros}e{exp}", if neg { "-" } else { "" }))
}

/// Set equality of two key lists; false when `a` contains duplicates.
fn equal_canonical_sets(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let set_a: std::collections::HashSet<&String> = a.iter().collect();
    b.iter().all(|v| set_a.contains(v)) && set_a.len() == a.len()
}

/// Tiny raw-byte port of the gjson/sjson subset used above. Behavior (including how malformed
/// input is read) follows tidwall/gjson v1.18.0 and tidwall/sjson v1.2.5. Also used by
/// `input_ids`.
pub(crate) mod gj {
    use cpa_core::util::go_json_string;

    /// gjson `Type`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Ty {
        Null,
        False,
        Number,
        String,
        True,
        Json,
    }

    /// gjson `Result` for a value that exists: its type, raw text and offset in the searched
    /// buffer (relative to the slice that was searched).
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Res<'a> {
        pub ty: Ty,
        pub raw: &'a [u8],
        pub index: usize,
    }

    impl Res<'_> {
        pub fn is_object(&self) -> bool {
            self.ty == Ty::Json && self.raw.first() == Some(&b'{')
        }

        pub fn is_array(&self) -> bool {
            self.ty == Ty::Json && self.raw.first() == Some(&b'[')
        }

        /// gjson `String()`.
        pub fn string(&self) -> String {
            match self.ty {
                Ty::Null => String::new(),
                Ty::False => "false".into(),
                Ty::True => "true".into(),
                Ty::Json => String::from_utf8_lossy(self.raw).into_owned(),
                Ty::String => {
                    let inner = self.raw.get(1..self.raw.len().saturating_sub(1)).unwrap_or_default();
                    if inner.contains(&b'\\') {
                        String::from_utf8_lossy(&unescape(inner)).into_owned()
                    } else {
                        String::from_utf8_lossy(inner).into_owned()
                    }
                }
                Ty::Number => {
                    let digits = self.raw.strip_prefix(b"-").unwrap_or(self.raw);
                    if digits.iter().all(u8::is_ascii_digit) {
                        return String::from_utf8_lossy(self.raw).into_owned();
                    }
                    let f = std::str::from_utf8(self.raw).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                    cpa_json::format_float(f)
                }
            }
        }
    }

    /// One object key of a path: unescaped name plus whether gjson treats it as a wildcard
    /// pattern (`*` / `?`).
    #[derive(Clone, Copy)]
    pub(crate) struct Key<'k> {
        name: &'k [u8],
        wild: bool,
    }

    impl<'k> Key<'k> {
        /// A plain key such as `tools` (no wildcards).
        pub fn plain(name: &'k str) -> Self {
            Key { name: name.as_bytes(), wild: false }
        }

        /// An object member name used literally in a path. The Go callers escape `\`, `.` and
        /// `:`, which makes the name a single path component; `*` and `?` stay wildcards.
        pub fn member(name: &'k [u8]) -> Self {
            Key { name, wild: name.iter().any(|b| matches!(b, b'*' | b'?')) }
        }
    }

    /// gjson `Get(json, key)` for one plain top-level key.
    pub(crate) fn get<'a>(json: &'a [u8], key: &str) -> Option<Res<'a>> {
        get_path(json, &[Key::plain(key)])
    }

    /// gjson `Get` for a dotted path of object keys. Like gjson, scans to the first `{` or `[`
    /// and tolerates malformed text; a later duplicate parent is tried when the first lacks
    /// the rest of the path.
    pub(crate) fn get_path<'a>(json: &'a [u8], path: &[Key<'_>]) -> Option<Res<'a>> {
        for (i, &c) in json.iter().enumerate() {
            if c == b'{' {
                return parse_object(json, i + 1, path).1;
            }
            if c == b'[' {
                // Array root with a non-numeric key never matches.
                return None;
            }
        }
        None
    }

    /// gjson `parseObject`: `i` is just past the opening brace. Returns the position to resume
    /// scanning from and the match, if any.
    fn parse_object<'a>(json: &'a [u8], mut i: usize, path: &[Key<'_>]) -> (usize, Option<Res<'a>>) {
        let len = json.len();
        let part = path[0];
        let more = path.len() > 1;
        while i < len {
            // Find the next key string.
            let mut key: &[u8] = &[];
            let mut kesc = false;
            let mut ok = false;
            while i < len {
                if json[i] == b'"' {
                    i += 1;
                    let s = i;
                    let mut found = false;
                    while i < len {
                        let c = json[i];
                        if c > b'\\' {
                            i += 1;
                            continue;
                        }
                        if c == b'"' {
                            key = &json[s..i];
                            i += 1;
                            ok = true;
                            found = true;
                            break;
                        }
                        if c == b'\\' {
                            i += 1;
                            while i < len {
                                let c = json[i];
                                if c > b'\\' {
                                    i += 1;
                                    continue;
                                }
                                if c == b'"' && !escaped_quote(json, i) {
                                    key = &json[s..i];
                                    kesc = true;
                                    i += 1;
                                    ok = true;
                                    found = true;
                                    break;
                                }
                                i += 1;
                            }
                            break;
                        }
                        i += 1;
                    }
                    if !found {
                        key = &json[s.min(len)..];
                        kesc = false;
                        ok = false;
                    }
                    break;
                }
                if json[i] == b'}' {
                    return (i + 1, None);
                }
                i += 1;
            }
            if !ok {
                return (i, None);
            }
            let unescaped;
            let key_cmp: &[u8] = if kesc {
                unescaped = unescape(key);
                &unescaped
            } else {
                key
            };
            let pmatch = if part.wild { glob_match(part.name, key_cmp) } else { part.name == key_cmp };
            let hit = pmatch && !more;
            // Consume the value.
            while i < len {
                let mut num = false;
                match json[i] {
                    b'"' => {
                        let start = i;
                        let (ni, vok) = parse_string(json, i + 1);
                        i = ni;
                        if !vok {
                            return (i, None);
                        }
                        if hit {
                            return (i, Some(Res { ty: Ty::String, raw: &json[start..i], index: start }));
                        }
                    }
                    b'{' if pmatch && !hit => {
                        let (ni, r) = parse_object(json, i + 1, &path[1..]);
                        i = ni;
                        if r.is_some() {
                            return (i, r);
                        }
                    }
                    b'{' | b'[' => {
                        // An array value never matches the remaining object keys.
                        let start = i;
                        i = squash_end(json, i);
                        if hit {
                            return (i, Some(Res { ty: Ty::Json, raw: &json[start..i], index: start }));
                        }
                    }
                    c @ (b'n' | b't' | b'f') => {
                        if c == b'n' && i + 1 < len && json[i + 1] != b'u' {
                            num = true;
                        } else {
                            let start = i;
                            let (ni, val) = parse_literal(json, i);
                            i = ni;
                            if hit {
                                let ty = match c {
                                    b't' => Ty::True,
                                    b'f' => Ty::False,
                                    _ => Ty::Null,
                                };
                                return (i, Some(Res { ty, raw: val, index: start }));
                            }
                        }
                    }
                    b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                    _ => {
                        i += 1;
                        continue;
                    }
                }
                if num {
                    let start = i;
                    let (ni, val) = parse_number(json, i);
                    i = ni;
                    if hit {
                        return (i, Some(Res { ty: Ty::Number, raw: val, index: start }));
                    }
                }
                break;
            }
        }
        (i, None)
    }

    /// True when the quote at `i` is preceded by an odd number of backslashes.
    fn escaped_quote(json: &[u8], i: usize) -> bool {
        if i == 0 || json[i - 1] != b'\\' {
            return false;
        }
        let mut n = 0usize;
        let mut j = i as isize - 2;
        while j > 0 {
            if json[j as usize] != b'\\' {
                break;
            }
            n += 1;
            j -= 1;
        }
        n.is_multiple_of(2)
    }

    /// gjson `parseString`: `i` is just past the opening quote. Returns the index after the
    /// closing quote and whether one was found.
    fn parse_string(json: &[u8], mut i: usize) -> (usize, bool) {
        while i < json.len() {
            let c = json[i];
            if c > b'\\' {
                i += 1;
                continue;
            }
            if c == b'"' {
                return (i + 1, true);
            }
            if c == b'\\' {
                i += 1;
                while i < json.len() {
                    let c = json[i];
                    if c > b'\\' {
                        i += 1;
                        continue;
                    }
                    if c == b'"' && !escaped_quote(json, i) {
                        return (i + 1, true);
                    }
                    i += 1;
                }
                break;
            }
            i += 1;
        }
        (i.min(json.len()), false)
    }

    /// End (exclusive) of the container starting at `start`, or the end of input when it is
    /// unterminated (gjson `squash` / `parseSquash`; parentheses count as brackets too).
    fn squash_end(json: &[u8], start: usize) -> usize {
        let len = json.len();
        let mut depth = 1i32;
        let mut i = start + 1;
        while i < len {
            match json[i] {
                b'"' => {
                    i += 1;
                    while i < len {
                        if json[i] == b'"' {
                            let mut n = 0usize;
                            let mut j = i;
                            while j > 0 && json[j - 1] == b'\\' {
                                n += 1;
                                j -= 1;
                            }
                            // Stop counting at the string start.
                            if n.is_multiple_of(2) {
                                break;
                            }
                        }
                        i += 1;
                    }
                }
                b'{' | b'[' | b'(' => depth += 1,
                b'}' | b']' | b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        len
    }

    fn parse_literal(json: &[u8], start: usize) -> (usize, &[u8]) {
        let mut i = start + 1;
        while i < json.len() && json[i].is_ascii_lowercase() {
            i += 1;
        }
        (i, &json[start..i])
    }

    fn parse_number(json: &[u8], start: usize) -> (usize, &[u8]) {
        let mut i = start + 1;
        while i < json.len() {
            let c = json[i];
            if c <= b' ' || c == b',' || c == b']' || c == b'}' {
                break;
            }
            i += 1;
        }
        (i, &json[start..i])
    }

    /// gjson `parseAny` with `hit`: the next value at or after `i`. `None` when the text ends
    /// or a string is unterminated.
    fn parse_any(json: &[u8], mut i: usize) -> (usize, Option<Res<'_>>) {
        let len = json.len();
        while i < len {
            let c = json[i];
            if c == b'{' || c == b'[' {
                let end = squash_end(json, i);
                return (end, Some(Res { ty: Ty::Json, raw: &json[i..end], index: i }));
            }
            if c <= b' ' {
                i += 1;
                continue;
            }
            let mut num = false;
            match c {
                b'"' => {
                    let start = i;
                    let (ni, ok) = parse_string(json, i + 1);
                    if !ok {
                        return (ni, None);
                    }
                    return (ni, Some(Res { ty: Ty::String, raw: &json[start..ni], index: start }));
                }
                b'n' | b't' | b'f' => {
                    if c == b'n' && i + 1 < len && json[i + 1] != b'u' {
                        num = true;
                    } else {
                        let (ni, val) = parse_literal(json, i);
                        let ty = match c {
                            b't' => Ty::True,
                            b'f' => Ty::False,
                            _ => Ty::Null,
                        };
                        return (ni, Some(Res { ty, raw: val, index: i }));
                    }
                }
                b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                _ => {}
            }
            if num {
                let (ni, val) = parse_number(json, i);
                return (ni, Some(Res { ty: Ty::Number, raw: val, index: i }));
            }
            i += 1;
        }
        (i, None)
    }

    /// gjson `ForEach` over a JSON array text: calls `f(start, element)` where `start` is the
    /// offset gjson reports for the element (relative to `json`). Stops when `f` returns false.
    pub(crate) fn for_each_array<'a>(json: &'a [u8], mut f: impl FnMut(usize, Res<'a>) -> bool) {
        let len = json.len();
        let mut i = 0;
        while i < len {
            if json[i] == b'[' {
                i += 1;
                break;
            }
            if json[i] == b'{' || json[i] > b' ' {
                return;
            }
            i += 1;
        }
        while i < len {
            while i < len && (json[i] <= b' ' || json[i] == b',' || json[i] == b':') {
                i += 1;
            }
            let s = i;
            let (ni, value) = parse_any(json, i);
            i = ni;
            let Some(value) = value else {
                return;
            };
            if !f(s, value) {
                return;
            }
            i += 1;
        }
    }

    /// gjson `tonum`: the number token at the start of `json`.
    fn tonum(json: &[u8]) -> &[u8] {
        for i in 1..json.len() {
            let c = json[i];
            if c <= b'-' {
                if c <= b' ' || c == b',' {
                    return &json[..i];
                }
            } else if c == b']' || c == b'}' {
                return &json[..i];
            }
        }
        json
    }

    fn tolit(json: &[u8]) -> &[u8] {
        for i in 1..json.len() {
            if !json[i].is_ascii_lowercase() {
                return &json[..i];
            }
        }
        json
    }

    /// gjson `tostr`: the string token at the start of `json` (lead byte is a quote).
    fn tostr(json: &[u8]) -> &[u8] {
        let mut i = 1;
        while i < json.len() {
            let c = json[i];
            if c > b'\\' {
                i += 1;
                continue;
            }
            if c == b'"' {
                return &json[..i + 1];
            }
            if c == b'\\' {
                i += 1;
                while i < json.len() {
                    let c = json[i];
                    if c > b'\\' {
                        i += 1;
                        continue;
                    }
                    if c == b'"' && !escaped_quote(json, i) {
                        return &json[..i + 1];
                    }
                    i += 1;
                }
                return if i + 1 < json.len() { &json[..i + 1] } else { &json[..i.min(json.len())] };
            }
            i += 1;
        }
        json
    }

    /// Shared walk of gjson `arrayOrMap`: yields `(is_key_position, value)` for each token of
    /// the container whose opening byte is `vc`.
    fn array_or_map<'a>(json: &'a [u8], vc: u8, mut emit: impl FnMut(Res<'a>)) {
        let len = json.len();
        let mut i = 0;
        loop {
            if i >= len {
                return;
            }
            if json[i] == vc {
                i += 1;
                break;
            }
            if json[i] > b' ' {
                return;
            }
            i += 1;
        }
        while i < len {
            let c = json[i];
            if c <= b' ' {
                i += 1;
                continue;
            }
            if c == b']' || c == b'}' {
                break;
            }
            let (ty, raw) = match c {
                b'0'..=b'9' | b'-' => (Ty::Number, tonum(&json[i..])),
                b'{' | b'[' => {
                    let end = squash_end(json, i);
                    (Ty::Json, &json[i..end])
                }
                b'n' => (Ty::Null, tolit(&json[i..])),
                b't' => (Ty::True, tolit(&json[i..])),
                b'f' => (Ty::False, tolit(&json[i..])),
                b'"' => (Ty::String, tostr(&json[i..])),
                _ => {
                    i += 1;
                    continue;
                }
            };
            emit(Res { ty, raw, index: i });
            i += raw.len();
        }
    }

    /// gjson `Array()` of an array text.
    pub(crate) fn array(json: &[u8]) -> Vec<Res<'_>> {
        let mut out = Vec::new();
        array_or_map(json, b'[', |v| out.push(v));
        out
    }

    /// gjson `Map()` of an object text as ordered `(key, value)` pairs; the first occurrence of
    /// a duplicate key wins.
    pub(crate) fn map(json: &[u8]) -> Vec<(Vec<u8>, Res<'_>)> {
        let mut out: Vec<(Vec<u8>, Res<'_>)> = Vec::new();
        let mut count = 0usize;
        let mut key: Vec<u8> = Vec::new();
        array_or_map(json, b'{', |v| {
            if count.is_multiple_of(2) {
                key = match v.ty {
                    Ty::String => v.string().into_bytes(),
                    _ => Vec::new(),
                };
            } else if !out.iter().any(|(k, _)| *k == key) {
                out.push((std::mem::take(&mut key), v));
            }
            count += 1;
        });
        out
    }

    /// gjson `unescape`: lenient JSON string unescape that stops at the first bad escape.
    fn unescape(json: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(json.len());
        let mut i = 0;
        while i < json.len() {
            let c = json[i];
            if c < b' ' {
                return out;
            }
            if c != b'\\' {
                out.push(c);
                i += 1;
                continue;
            }
            i += 1;
            let Some(&e) = json.get(i) else {
                return out;
            };
            match e {
                b'\\' => out.push(b'\\'),
                b'/' => out.push(b'/'),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'"' => out.push(b'"'),
                b'u' => {
                    if i + 5 > json.len() {
                        return out;
                    }
                    let mut r = hex4(&json[i + 1..]);
                    i += 5;
                    if (0xD800..0xE000).contains(&r) {
                        // Need a trailing low surrogate; a lone one decodes to U+FFFD below.
                        if json.len() - i >= 6 && json[i] == b'\\' && json[i + 1] == b'u' {
                            let lo = hex4(&json[i + 2..]);
                            r = match (0xD800..0xDC00).contains(&r) && (0xDC00..0xE000).contains(&lo) {
                                true => 0x10000 + ((r - 0xD800) << 10) + (lo - 0xDC00),
                                false => 0xFFFD,
                            };
                            i += 6;
                        }
                    }
                    let ch = char::from_u32(r).unwrap_or('\u{FFFD}');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    i -= 1;
                }
                _ => return out,
            }
            i += 1;
        }
        out
    }

    /// Go `strconv.ParseUint(s[:4], 16, 64)` ignoring errors (invalid -> 0).
    fn hex4(b: &[u8]) -> u32 {
        std::str::from_utf8(b.get(..4).unwrap_or_default())
            .ok()
            .and_then(|s| u32::from_str_radix(s, 16).ok())
            .unwrap_or(0)
    }

    /// tidwall/match glob: `*` any run, `?` any single character, `\\c` the literal `c`.
    fn glob_match(pattern: &[u8], s: &[u8]) -> bool {
        #[derive(PartialEq)]
        enum Tok {
            Star,
            Any,
            Lit(char),
        }
        let mut toks = Vec::new();
        let mut chars = String::from_utf8_lossy(pattern).chars().collect::<Vec<_>>().into_iter();
        while let Some(c) = chars.next() {
            match c {
                '*' => toks.push(Tok::Star),
                '?' => toks.push(Tok::Any),
                '\\' => match chars.next() {
                    Some(l) => toks.push(Tok::Lit(l)),
                    None => return false,
                },
                l => toks.push(Tok::Lit(l)),
            }
        }
        let t: Vec<char> = String::from_utf8_lossy(s).chars().collect();
        let (mut pi, mut ti) = (0usize, 0usize);
        let (mut star, mut mark) = (None, 0usize);
        while ti < t.len() {
            match toks.get(pi) {
                Some(Tok::Any) => {
                    pi += 1;
                    ti += 1;
                }
                Some(Tok::Lit(l)) if *l == t[ti] => {
                    pi += 1;
                    ti += 1;
                }
                Some(Tok::Star) => {
                    star = Some(pi);
                    mark = ti;
                    pi += 1;
                }
                _ => match star {
                    Some(sp) => {
                        pi = sp + 1;
                        mark += 1;
                        ti = mark;
                    }
                    None => return false,
                },
            }
        }
        while toks.get(pi) == Some(&Tok::Star) {
            pi += 1;
        }
        pi == toks.len()
    }

    // ---- sjson ----

    /// sjson `appendStringify`: a JSON string literal, using Go `json.Marshal` when the text
    /// needs escaping (which also HTML-escapes `<`, `>` and `&`).
    pub(crate) fn stringify(s: &str) -> String {
        let must_marshal = s.bytes().any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\');
        if must_marshal { go_json_string(s) } else { format!("\"{s}\"") }
    }

    /// sjson `SetRawBytes(json, path, raw)`: replaces the value at `path`, or appends the key
    /// to the enclosing object when it is missing. `Err` where sjson reports an error. A path
    /// gjson cannot resolve to a position leaves `json` unchanged (sjson's "no change").
    pub(crate) fn set_raw(json: &[u8], path: &[Key<'_>], raw: &[u8]) -> Result<Vec<u8>, ()> {
        // sjson sends paths containing `| # @ * ?` down its "complex path" route: one gjson
        // `Get` over the whole path, replacing the match in place and ignoring misses.
        let complex = path.iter().any(|k| k.name.iter().any(|b| matches!(b, b'|' | b'#' | b'@' | b'*' | b'?')));
        if complex {
            // After a `.`, gjson reads `@modifier`, `[..]` and `{..}` as pipes and `|` splits
            // the path; none of those resolve to an offset.
            let piped = path.iter().enumerate().any(|(i, k)| {
                k.name.contains(&b'|')
                    || (i > 0 && matches!(k.name.first(), Some(b'[' | b'{')))
                    || (i > 0 && is_modifier_component(k.name))
            });
            if piped {
                return Ok(json.to_vec());
            }
            return Ok(match get_path(json, path) {
                Some(res) if res.index != 0 => splice(json, res, raw),
                _ => json.to_vec(),
            });
        }
        let mut buf = Vec::with_capacity(json.len() + raw.len());
        match append_raw_paths(&mut buf, json, path, raw, false) {
            Ok(()) => Ok(buf),
            Err(SetErr::NoChange) => Ok(json.to_vec()),
            Err(SetErr::Fail) => Err(()),
        }
    }

    /// gjson `isDotPiperChar` for an `@` component: a built-in modifier name (the component
    /// text ends at the first `.`, `|` or `:` of the escaped key).
    fn is_modifier_component(name: &[u8]) -> bool {
        if name.first() != Some(&b'@') {
            return false;
        }
        let mut escaped = Vec::with_capacity(name.len() + 2);
        for &b in &name[1..] {
            if matches!(b, b'\\' | b'.' | b':') {
                escaped.push(b'\\');
            }
            escaped.push(b);
        }
        let end = escaped.iter().position(|b| matches!(b, b'.' | b'|' | b':')).unwrap_or(escaped.len());
        matches!(
            &escaped[..end],
            b"pretty" | b"ugly" | b"reverse" | b"this" | b"flatten" | b"join" | b"valid" | b"keys" | b"values"
                | b"tostr" | b"fromstr" | b"group" | b"dig"
        )
    }

    /// sjson `SetBytes(json, "path", string)` for a single plain key.
    pub(crate) fn set_string(json: &[u8], key: &str, value: &str) -> Result<Vec<u8>, ()> {
        set_raw(json, &[Key::plain(key)], stringify(value).as_bytes())
    }

    /// sjson `DeleteBytes(json, key)` for a single plain key; on error or a missing key the
    /// input is returned unchanged.
    pub(crate) fn delete(json: &[u8], key: &str) -> Vec<u8> {
        let mut buf = Vec::with_capacity(json.len());
        match append_raw_paths(&mut buf, json, &[Key::plain(key)], b"", true) {
            Ok(()) => buf,
            Err(_) => json.to_vec(),
        }
    }

    fn splice(json: &[u8], res: Res<'_>, raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(json.len() + raw.len());
        out.extend_from_slice(&json[..res.index]);
        out.extend_from_slice(raw);
        out.extend_from_slice(&json[res.index + res.raw.len()..]);
        out
    }

    /// Whether `gjson.Get(json, key)` treats the path as a sub-selector (`{..}` / `[..]`) or a
    /// static value (`!true`, `!5`). Those results never carry an offset, so sjson cannot
    /// replace them in place.
    fn is_selector_syntax(path: &[u8]) -> bool {
        if path.len() < 2 {
            return false;
        }
        let rest = &path[1..];
        match path[0] {
            b'{' | b'[' => true,
            b'!' => {
                let end = rest.iter().position(|b| matches!(b, b'|' | b'.')).unwrap_or(rest.len());
                matches!(rest[0], b'{' | b'[' | b'"' | b'+' | b'-' | b'0'..=b'9')
                    || matches!(
                        String::from_utf8_lossy(&rest[..end]).to_lowercase().as_str(),
                        "true" | "false" | "null" | "nan" | "inf"
                    )
            }
            _ => false,
        }
    }

    enum SetErr {
        NoChange,
        Fail,
    }

    /// sjson `appendRawPaths` for object paths (numeric array indexes are not needed here).
    fn append_raw_paths(
        buf: &mut Vec<u8>,
        jstr: &[u8],
        paths: &[Key<'_>],
        raw: &[u8],
        del: bool,
    ) -> Result<(), SetErr> {
        // gjson reads some leading characters as selector/modifier syntax, which never yields an
        // offset, so such keys are "not found" here (and get appended again).
        let found = if is_selector_syntax(paths[0].name) { None } else { get_path(jstr, &paths[..1]) };
        if let Some(res) = found.filter(|r| r.index > 0) {
            if paths.len() > 1 {
                buf.extend_from_slice(&jstr[..res.index]);
                append_raw_paths(buf, res.raw, &paths[1..], raw, del)?;
                buf.extend_from_slice(&jstr[res.index + res.raw.len()..]);
                return Ok(());
            }
            buf.extend_from_slice(&jstr[..res.index]);
            let mut exidx = 0usize;
            if del {
                let del_next_comma = delete_tail_item(buf);
                if del_next_comma {
                    let mut j = 0usize;
                    let mut i = res.index + res.raw.len();
                    while i < jstr.len() {
                        if jstr[i] <= b' ' {
                            i += 1;
                            j += 1;
                            continue;
                        }
                        if jstr[i] == b',' {
                            exidx = j + 1;
                        }
                        break;
                    }
                }
            } else {
                buf.extend_from_slice(raw);
            }
            buf.extend_from_slice(&jstr[res.index + res.raw.len() + exidx..]);
            return Ok(());
        }
        if del {
            return Err(SetErr::NoChange);
        }
        // Key missing: append it to the enclosing object.
        let key_name = paths[0].name;
        if !key_name.is_empty() && key_name.iter().all(u8::is_ascii_digit) {
            // Numeric keys build arrays in sjson; never needed for the callers here.
            return Err(SetErr::Fail);
        }
        let blank = jstr.iter().all(|&b| b <= b' ');
        // gjson `Parse`: from the first bracket; anything that is not an object or array is
        // replaced by an empty object.
        let obj_raw: &[u8] = if blank { b"{}" } else { parse_root(jstr).unwrap_or(b"{}") };
        if obj_raw[0] != b'{' {
            // Non-numeric key into an array (only "-1" appends in sjson).
            return Err(SetErr::Fail);
        }
        let mut comma = false;
        for &c in &obj_raw[1..] {
            if c <= b' ' {
                continue;
            }
            if c == b'}' || c == b']' {
                break;
            }
            comma = true;
            break;
        }
        let mut end = obj_raw.len() - 1;
        while end > 0 {
            if obj_raw[end] == b'}' {
                break;
            }
            end -= 1;
        }
        buf.extend_from_slice(&obj_raw[..end]);
        if comma {
            buf.push(b',');
        }
        append_build(buf, paths, raw);
        buf.push(b'}');
        Ok(())
    }

    /// gjson `Parse(json).Raw` for containers: the text from the first `{`/`[`; a non-space
    /// first byte of any other kind yields nothing.
    fn parse_root(json: &[u8]) -> Option<&[u8]> {
        for (i, &c) in json.iter().enumerate() {
            if c == b'{' || c == b'[' {
                return Some(&json[i..]);
            }
            if c > b' ' {
                return None;
            }
        }
        None
    }

    /// sjson `appendBuild` for object paths: `"k1":{"k2":raw}`.
    fn append_build(buf: &mut Vec<u8>, paths: &[Key<'_>], raw: &[u8]) {
        buf.extend_from_slice(stringify(&String::from_utf8_lossy(paths[0].name)).as_bytes());
        buf.push(b':');
        if paths.len() > 1 {
            buf.push(b'{');
            append_build(buf, &paths[1..], raw);
            buf.push(b'}');
        } else {
            buf.extend_from_slice(raw);
        }
    }

    /// sjson `deleteTailItem`: trims the key (and a preceding comma) that precedes a value
    /// about to be deleted. True when the comma after the value must be removed as well.
    fn delete_tail_item(buf: &mut Vec<u8>) -> bool {
        let mut i = buf.len() as isize - 1;
        while i >= 0 {
            match buf[i as usize] {
                b'[' => return true,
                b',' => {
                    buf.truncate(i as usize);
                    return false;
                }
                b':' => {
                    // Delete the tail string (the key).
                    i -= 1;
                    while i >= 0 {
                        if buf[i as usize] == b'"' {
                            i -= 1;
                            while i >= 0 {
                                if buf[i as usize] == b'"' {
                                    i -= 1;
                                    if i >= 0 && buf[i as usize] == b'\\' {
                                        // Go's `i--; continue` also runs the loop's own `i--`.
                                        i -= 2;
                                        continue;
                                    }
                                    while i >= 0 {
                                        match buf[i as usize] {
                                            b'{' => {
                                                buf.truncate(i as usize + 1);
                                                return true;
                                            }
                                            b',' => {
                                                buf.truncate(i as usize);
                                                return false;
                                            }
                                            _ => {}
                                        }
                                        i -= 1;
                                    }
                                }
                                i -= 1;
                            }
                            break;
                        }
                        i -= 1;
                    }
                    return false;
                }
                _ => {}
            }
            i -= 1;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use cpa_json::J;
    use http::{HeaderMap, HeaderValue};

    use super::*;
    use crate::helps::codex_tool_integers::normalize_codex_tool_integer_types;

    fn out_json(out: &[u8]) -> Value {
        cpa_json::parse(out)
    }

    fn user_agent(ua: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(http::header::USER_AGENT, HeaderValue::from_str(ua).unwrap());
        h
    }

    #[test]
    fn complex_one_of_simplified_preserves_properties() {
        // Minimal repro from issue #5551: 13-branch oneOf inside a property schema
        let input = br#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "t1",
			"description": "test tool",
			"strict": true,
			"parameters": {
				"type": "object",
				"properties": {
					"action": {
						"type": "string",
						"enum": ["p.list","m.list","s.list","s.create","s.send","s.fork","s.status","s.messages","sch.list","sch.create","sch.run","sch.delete","sch.toggle"],
						"oneOf": [
							{"const": "p.list", "description": "List projects"},
							{"const": "m.list", "description": "List models"},
							{"const": "s.list", "description": "List sessions"},
							{"const": "s.create", "description": "Create session"},
							{"const": "s.send", "description": "Send prompt"},
							{"const": "s.fork", "description": "Fork session"},
							{"const": "s.status", "description": "Session status"},
							{"const": "s.messages", "description": "Session messages"},
							{"const": "sch.list", "description": "List schedule"},
							{"const": "sch.create", "description": "Create schedule"},
							{"const": "sch.run", "description": "Run schedule"},
							{"const": "sch.delete", "description": "Delete schedule"},
							{"const": "sch.toggle", "description": "Toggle schedule"}
						],
						"description": "Action to perform"
					},
					"target": {
						"type": "string",
						"description": "Target ID"
					}
				},
				"required": ["action"]
			}
		}]
	}"#;

        let out = normalize_codex_tool_schemas(input);
        let v = out_json(&out);
        let tool = v.g("tools.0");

        // Complex oneOf must be removed from action property
        assert!(!tool.g("parameters.properties.action.oneOf").exists(), "{}", tool.g("parameters").raw());
        // Action type and enum must be PRESERVED
        assert_eq!(tool.g("parameters.properties.action.type").str(), "string");
        assert_eq!(tool.g("parameters.properties.action.enum").array().len(), 13);
        // Sibling property 'target' must be PRESERVED
        assert_eq!(tool.g("parameters.properties.target.type").str(), "string");
        // 'required' array must be PRESERVED
        assert_eq!(tool.g("parameters.required.0").str(), "action");
        // Tool name and strict mode preserved
        assert_eq!(tool.g("name").str(), "t1");
        assert!(tool.g("strict").bool());
    }

    #[test]
    fn dotted_property_name() {
        // A property whose key contains a dot (e.g. "my.action") must not be split into nested paths
        let input = br#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "dotted_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"my.action": {
						"type": "string",
						"enum": ["1", "2", "3", "4", "5", "6", "7", "8"],
						"oneOf": [
							{"const": "1"}, {"const": "2"}, {"const": "3"}, {"const": "4"},
							{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"}
						]
					}
				}
			}
		}]
	}"#;

        let out = normalize_codex_tool_schemas(input);
        let v = out_json(&out);
        let tool = v.g("tools.0");

        // Ensure properties contains "my.action", NOT nested object "my": {"action": ...}
        let dotted_prop = tool.g(r"parameters.properties.my\.action");
        assert!(dotted_prop.exists(), "{}", tool.g("parameters").raw());
        assert!(!dotted_prop.g("oneOf").exists(), "oneOf should be removed from my.action");
        assert_eq!(dotted_prop.g("type").str(), "string");
        // Verify that "my" was NOT created as an object containing "action"
        assert!(!tool.g("parameters.properties.my.action").exists());
    }

    #[test]
    fn colon_property_name() {
        // A property whose key starts with or contains a colon must not be read as control syntax
        let input = br#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "colon_tool",
			"parameters": {
				"type": "object",
				"properties": {
					":action": {
						"type": "string",
						"enum": ["1", "2", "3", "4", "5", "6", "7", "8"],
						"oneOf": [
							{"const": "1"}, {"const": "2"}, {"const": "3"}, {"const": "4"},
							{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"}
						]
					}
				}
			}
		}]
	}"#;

        let out = normalize_codex_tool_schemas(input);
        let v = out_json(&out);
        let tool = v.g("tools.0");

        let colon_prop = tool.g(r"parameters.properties.\:action");
        assert!(colon_prop.exists(), "{}", tool.g("parameters").raw());
        assert!(!colon_prop.g("oneOf").exists(), "oneOf should be removed from :action");
        assert!(!tool.g("parameters.properties.action").exists(), ":action was written to action without colon");
    }

    /// Runs one property through the normalizer and returns `tools.0` of the output.
    fn normalized_tool(input: &str) -> Value {
        out_json(&normalize_codex_tool_schemas(input.as_bytes())).g("tools.0").value()
    }

    #[test]
    fn numeric_duplicate_const_not_touched() {
        // 1 and 1.0 are mathematically equal in JSON Schema. Having both in oneOf violates
        // exclusivity, so the union must remain completely untouched.
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "num_dup_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"val": {
						"type": "number",
						"oneOf": [
							{"const": 1}, {"const": 1.0}, {"const": 2}, {"const": 3},
							{"const": 4}, {"const": 5}, {"const": 6}, {"const": 7}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.val.oneOf").exists(), "oneOf with 1 and 1.0 must NOT become enum");
    }

    #[test]
    fn large_integer_precision_preserved() {
        // Number larger than 2^53 must not lose precision
        let large_int = "9007199254740993";
        let input = format!(
            r#"{{
		"model": "gpt-5.5",
		"tools": [{{
			"type": "function",
			"name": "large_int_tool",
			"parameters": {{
				"type": "object",
				"properties": {{
					"id": {{
						"type": "integer",
						"oneOf": [
							{{"const": {large_int}}},
							{{"const": 1}}, {{"const": 2}}, {{"const": 3}},
							{{"const": 4}}, {{"const": 5}}, {{"const": 6}}, {{"const": 7}}
						]
					}}
				}}
			}}
		}}]
	}}"#
        );
        let out = normalize_codex_tool_schemas(input.as_bytes());
        let v = out_json(&out);
        assert!(!v.g("tools.0.parameters.properties.id.oneOf").exists(), "oneOf should be deleted");
        // The first enum item must keep the exact digits (raw token, not float-rounded).
        let first = cpa_json::raw_at(&out, "tools.0.parameters.properties.id.enum.0");
        assert_eq!(first, Some(large_int), "large integer precision was lost");
    }

    #[test]
    fn unicode_duplicate_const_not_touched() {
        // "\u0061" and "a" have the same semantic string value: not distinct constants.
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "dup_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"val": {
						"type": "string",
						"oneOf": [
							{"const": "a"}, {"const": "\u0061"}, {"const": "c"}, {"const": "d"},
							{"const": "e"}, {"const": "f"}, {"const": "g"}, {"const": "h"}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.val.oneOf").exists(), "duplicate semantic values must NOT become enum");
    }

    #[test]
    fn type_preserving_comparison() {
        // Existing enum has strings "1".."8" but oneOf has numbers 1..8: not identical.
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "type_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"val": {
						"enum": ["1", "2", "3", "4", "5", "6", "7", "8"],
						"oneOf": [
							{"const": 1}, {"const": 2}, {"const": 3}, {"const": 4},
							{"const": 5}, {"const": 6}, {"const": 7}, {"const": 8}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.val.oneOf").exists(), "oneOf must NOT be deleted");
    }

    #[test]
    fn both_one_of_and_any_of_untouched() {
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "compound_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"val": {
						"oneOf": [
							{"const": "1"}, {"const": "2"}, {"const": "3"}, {"const": "4"},
							{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"}
						],
						"anyOf": [
							{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"},
							{"const": "9"}, {"const": "10"}, {"const": "11"}, {"const": "12"}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.val.oneOf").exists() && tool.g("parameters.properties.val.anyOf").exists());
    }

    #[test]
    fn migrates_const_branches_to_enum() {
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "t2",
			"parameters": {
				"type": "object",
				"properties": {
					"mode": {
						"type": "string",
						"oneOf": [
							{"const": "m1"}, {"const": "m2"}, {"const": "m3"}, {"const": "m4"},
							{"const": "m5"}, {"const": "m6"}, {"const": "m7"}, {"const": "m8"},
							{"const": "m9"}, {"const": "m10"}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(!tool.g("parameters.properties.mode.oneOf").exists(), "oneOf should be deleted");
        assert_eq!(tool.g("parameters.properties.mode.enum").array().len(), 10);
    }

    #[test]
    fn non_matching_enum_not_touched() {
        // Existing enum has 9 values while the const branches cover only 8.
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "t3",
			"parameters": {
				"type": "object",
				"properties": {
					"status": {
						"type": "string",
						"enum": ["1", "2", "3", "4", "5", "6", "7", "8", "extra"],
						"oneOf": [
							{"const": "1"}, {"const": "2"}, {"const": "3"}, {"const": "4"},
							{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.status.oneOf").exists());
    }

    #[test]
    fn non_const_union_not_touched() {
        // Branches carry extra constraints (pattern, type) and are not pure consts.
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "t4",
			"parameters": {
				"type": "object",
				"properties": {
					"data": {
						"oneOf": [
							{"type": "string", "pattern": "^[a-z]+$"},
							{"type": "number", "minimum": 0},
							{"type": "boolean"},
							{"type": "null"},
							{"type": "array"},
							{"type": "object"},
							{"type": "integer"},
							{"type": "string", "pattern": "^[0-9]+$"}
						]
					}
				}
			}
		}]
	}"#,
        );
        assert!(tool.g("parameters.properties.data.oneOf").exists());
    }

    #[test]
    fn simple_tool_preserved() {
        let tool = normalized_tool(
            r#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "function",
			"name": "lookup",
			"strict": true,
			"parameters": {
				"type": "object",
				"properties": {
					"query": {"type": "string"}
				},
				"required": ["query"],
				"additionalProperties": false
			}
		}]
	}"#,
        );
        assert_eq!(tool.g("parameters.properties.query.type").str(), "string", "{}", tool.g("parameters").raw());
        assert!(tool.g("strict").bool());
    }

    #[test]
    fn namespace_tool_simplified() {
        let input = br#"{
		"model": "gpt-5.5",
		"tools": [{
			"type": "namespace",
			"name": "mcp",
			"tools": [{
				"type": "function",
				"name": "complex_tool",
				"parameters": {
					"type": "object",
					"properties": {
						"action": {
							"type": "string",
							"oneOf": [
								{"const": "1"}, {"const": "2"}, {"const": "3"}, {"const": "4"},
								{"const": "5"}, {"const": "6"}, {"const": "7"}, {"const": "8"}
							]
						}
					}
				}
			}]
		}]
	}"#;
        let v = out_json(&normalize_codex_tool_schemas(input));
        let nested = v.g("tools.0.tools.0");
        assert!(!nested.g("parameters.properties.action.oneOf").exists(), "nested tool oneOf should be simplified");
        assert_eq!(nested.g("parameters.properties.action.enum").array().len(), 8);
    }

    #[test]
    fn strips_unsupported_unicode_property_escape_patterns() {
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "Artifact",
			"parameters": {
				"type": "object",
				"properties": {
					"field": {
						"type": "string",
						"description": "field to edit",
						"pattern": "^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$"
					},
					"asset_id": {
						"type": "string",
						"pattern": "^[0-9a-f]{32}$"
					}
				},
				"required": ["field"]
			}
		}]
	}"#;

        let out = normalize_codex_tool_schemas(input);
        let v = out_json(&out);
        let params = v.g("tools.0.parameters");

        // Unsupported pattern must be removed
        assert!(!params.g("properties.field.pattern").exists(), "{}", params.g("properties.field.pattern").raw());
        assert_eq!(params.g("properties.field.type").str(), "string");
        // Valid pattern must be preserved
        assert_eq!(params.g("properties.asset_id.pattern").str(), "^[0-9a-f]{32}$");
        // Idempotence test
        assert_eq!(normalize_codex_tool_schemas(&out), out);
    }

    #[test]
    fn strips_octal_nul_pattern_escape() {
        // Claude Code's Artifact tool guards file paths with the octal NUL escape; strict
        // validators reject it while accepting \x00, so the pattern has to come off.
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "Artifact",
			"parameters": {
				"type": "object",
				"properties": {
					"file_paths": {
						"type": "array",
						"minItems": 1,
						"items": {
							"type": "string",
							"minLength": 1,
							"maxLength": 1024,
							"pattern": "^[^\\0]*$"
						}
					},
					"asset_id": {
						"type": "string",
						"pattern": "^[0-9a-f]{32}$"
					},
					"hex_nul": {
						"type": "string",
						"pattern": "^[^\\x00]*$"
					}
				},
				"required": ["file_paths"]
			}
		}]
	}"#;

        let out = normalize_codex_tool_schemas(input);
        let v = out_json(&out);
        let params = v.g("tools.0.parameters");

        // The octal NUL pattern must be removed, while the rest of the item schema stays.
        assert!(
            !params.g("properties.file_paths.items.pattern").exists(),
            "{}",
            params.g("properties.file_paths.items.pattern").raw()
        );
        assert_eq!(params.g("properties.file_paths.items.type").str(), "string");
        for (path, want) in [
            ("properties.file_paths.items.minLength", "1"),
            ("properties.file_paths.items.maxLength", "1024"),
            ("properties.file_paths.minItems", "1"),
        ] {
            assert_eq!(params.g(path).str(), want, "{path}");
        }
        // A plain pattern is valid and must survive.
        assert_eq!(params.g("properties.asset_id.pattern").str(), "^[0-9a-f]{32}$");
        // The hex NUL spelling is the one strict validators accept, so it must survive.
        assert_eq!(params.g("properties.hex_nul.pattern").str(), r"^[^\x00]*$");
        // Idempotence test
        assert_eq!(normalize_codex_tool_schemas(&out), out);
    }

    #[test]
    fn preserves_non_schema_pattern_keys() {
        // default/enum metadata holding a nested object with a 'pattern' key is user data, not
        // a schema, and must not be mutated.
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "config_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"regex_config": {
						"type": "object",
						"default": {
							"pattern": "\\p{L}+"
						},
						"enum": [
							{"pattern": "\\p{N}+"}
						]
					},
					"real_schema": {
						"type": "string",
						"pattern": "\\p{L}+"
					}
				}
			}
		}]
	}"#;

        let v = out_json(&normalize_codex_tool_schemas(input));
        let params = v.g("tools.0.parameters");

        // Real schema pattern must be removed
        assert!(!params.g("properties.real_schema.pattern").exists(), "{}", params.g("properties.real_schema").raw());
        // User data under default and enum must be PRESERVED
        assert_eq!(params.g("properties.regex_config.default.pattern").str(), r"\p{L}+");
        assert_eq!(params.g("properties.regex_config.enum.0.pattern").str(), r"\p{N}+");
    }

    #[test]
    fn covers_all_schema_keyword_locations() {
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "deep_tool",
			"parameters": {
				"type": "object",
				"$defs": {
					"custom_type": {
						"type": "string",
						"pattern": "\\p{L}+"
					}
				},
				"additionalProperties": {
					"type": "string",
					"pattern": "\\p{N}+"
				},
				"patternProperties": {
					"^s_": {
						"type": "string",
						"pattern": "\\p{M}+"
					}
				},
				"if": {
					"properties": {
						"flag": {
							"type": "string",
							"pattern": "\\p{P}+"
						}
					}
				},
				"then": {
					"properties": {
						"val": {
							"type": "string",
							"pattern": "\\p{S}+"
						}
					}
				},
				"else": {
					"properties": {
						"other": {
							"type": "string",
							"pattern": "\\p{Z}+"
						}
					}
				}
			}
		}]
	}"#;

        let v = out_json(&normalize_codex_tool_schemas(input));
        let params = v.g("tools.0.parameters");

        // All subschemas in schema-aware locations must have their incompatible patterns stripped
        for path in [
            "$defs.custom_type.pattern",
            "additionalProperties.pattern",
            "patternProperties.^s_.pattern",
            "if.properties.flag.pattern",
            "then.properties.val.pattern",
            "else.properties.other.pattern",
        ] {
            assert!(!params.g(path).exists(), "expected {path} to be removed");
        }
        // Subschema types must be preserved
        assert_eq!(params.g("$defs.custom_type.type").str(), "string");
    }

    #[test]
    fn malformed_or_empty_parameters_fallback() {
        // Malformed JSON, non-object parameters, null, and empty payloads must not panic
        let cases: [&[u8]; 5] = [
            br#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":null}]}"#,
            br#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":"not_an_object"}]}"#,
            br#"{"model":"gpt-5.6","tools":[{"type":"function","name":"t","parameters":{"type":"object"}}]}"#,
            br#"{"model":"gpt-5.6","tools":[]}"#,
            br#"{"model":"gpt-5.6"}"#,
        ];
        for (i, c) in cases.iter().enumerate() {
            assert!(!normalize_codex_tool_schemas(c).is_empty(), "case {i}: unexpected empty output");
        }
    }

    #[test]
    fn json_unicode_escape_bypass_prevention() {
        // Patterns encoded with JSON Unicode escapes (\u005c for '\', \u0070 for 'p') decode to
        // \p{...} / \P{...} and must not be skipped by the fast-path check.
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "escape_bypass_tool",
			"parameters": {
				"type": "object",
				"properties": {
					"p1": {
						"type": "string",
						"pattern": "\u005c\u0070{L}+"
					},
					"p2": {
						"type": "string",
						"pattern": "\u005cp{Cc}"
					},
					"p3": {
						"type": "string",
						"pattern": "\u005c\u0050{N}+"
					},
					"valid": {
						"type": "string",
						"pattern": "^[0-9a-f]{32}$"
					}
				}
			}
		}]
	}"#;

        let v = out_json(&normalize_codex_tool_schemas(input));
        let params = v.g("tools.0.parameters");
        for p in ["p1", "p2", "p3"] {
            assert!(!params.g(&format!("properties.{p}.pattern")).exists(), "expected properties.{p}.pattern to be removed");
        }
        assert_eq!(params.g("properties.valid.pattern").str(), "^[0-9a-f]{32}$");
    }

    #[test]
    fn pattern_properties_key_sanitization() {
        let input = br#"{
		"model": "gpt-5.6",
		"tools": [{
			"type": "function",
			"name": "pattern_props_tool",
			"parameters": {
				"type": "object",
				"patternProperties": {
					"^\\\\p{L}+$": {
						"type": "string"
					},
					"^[a-z]+$": {
						"type": "number"
					}
				}
			}
		}]
	}"#;

        let v = out_json(&normalize_codex_tool_schemas(input));
        let params = v.g("tools.0.parameters");
        let pattern_props = params.g("patternProperties");
        let keys: Vec<&str> = pattern_props.entries().into_iter().map(|(k, _)| k).collect();

        // Key with \p{L}+ must be removed
        assert!(!keys.contains(&r"^\p{L}+$"), "{}", pattern_props.raw());
        // Safe key must be preserved
        assert!(keys.contains(&"^[a-z]+$"), "{}", pattern_props.raw());
    }

    #[test]
    fn does_not_normalize_integer_types() {
        let input = br#"{
		"tools": [
			{
				"type": "function",
				"name": "exec_command",
				"parameters": {
					"type": "object",
					"properties": {
						"yield_time_ms": {"type": "number"}
					}
				}
			}
		]
	}"#;
        let v = out_json(&normalize_codex_tool_schemas(input));
        assert_eq!(v.g("tools.0.parameters.properties.yield_time_ms.type").str(), "number");
    }

    // ---- helps/codex_tool_schema_batch_test.go ----

    const BATCH_UNION_TOOL: &str = r#"{"type":"function","name":"choose","parameters":{"type":"object","properties":{"action":{"oneOf":[{"const":"a"},{"const":"b"},{"const":"c"},{"const":"d"},{"const":"e"},{"const":"f"},{"const":"g"},{"const":"h"}]}}}}"#;
    const BATCH_SIMPLE_TOOL: &str =
        r#"{"type":"function","name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}"#;

    /// Replaces element `index` of the array at `path` (a plain key) the way sjson's
    /// `SetRawBytes(body, "<path>.<index>", ...)` does.
    fn set_array_element(json: &[u8], path: &str, index: usize, raw: &[u8]) -> Vec<u8> {
        let Some(array) = gj::get(json, path) else {
            return json.to_vec();
        };
        let mut hit = None;
        let mut n = 0;
        gj::for_each_array(array.raw, |start, el| {
            if n == index {
                hit = Some((array.index + start, el.raw.len()));
                return false;
            }
            n += 1;
            true
        });
        match hit {
            Some((at, len)) => [&json[..at], raw, &json[at + len..]].concat(),
            None => json.to_vec(),
        }
    }

    /// Frozen pre-batching traversal: one edit per tool, applied to the evolving body.
    fn sequential_tool_schemas(body: &[u8]) -> Vec<u8> {
        let mut body = body.to_vec();
        let Some(tools) = gj::get(&body, "tools").filter(Res::is_array) else {
            return body;
        };
        let originals: Vec<Vec<u8>> = gj::array(tools.raw).iter().map(|t| t.raw.to_vec()).collect();
        for (index, raw) in originals.iter().enumerate() {
            let tool = Res { ty: Ty::Json, raw, index: 0 };
            if let Some(updated) = sequential_tool(tool) {
                body = set_array_element(&body, "tools", index, &updated);
            }
        }
        body
    }

    fn sequential_tool(tool: Res<'_>) -> Option<Vec<u8>> {
        let is_namespace = gj::get(tool.raw, "type").map(|t| t.string()).as_deref() == Some("namespace");
        if !is_namespace {
            return normalize_tool(tool);
        }
        let nested = gj::get(tool.raw, "tools").filter(Res::is_array)?;
        let children = gj::array(nested.raw);
        if children.is_empty() {
            return None;
        }
        let mut raw = tool.raw.to_vec();
        let mut changed = false;
        for (index, child) in children.iter().enumerate() {
            if let Some(updated) = sequential_tool(*child) {
                raw = set_array_element(&raw, "tools", index, &updated);
                changed = true;
            }
        }
        changed.then_some(raw)
    }

    #[test]
    fn matches_sequential_edits() {
        let namespace = |tools: &str| format!(r#"{{"type":"namespace", "name":"workspace", "tools": [ {tools} ]}}"#);
        let request = |tools: &str| {
            format!(
                "{{\r\n  \"input\": \"tool_search \\u5de5\", \"tools\": [\n\t{tools}\n  ], \"metadata\": {{\"number\": 900719925474099312345, \"value\": 1.00e+9}}\r\n}}"
            )
        };
        let (u, s) = (BATCH_UNION_TOOL, BATCH_SIMPLE_TOOL);
        let inputs: Vec<String> = vec![
            String::new(),
            "{}".into(),
            r#"{"tools":null}"#.into(),
            r#"{"tools":[]}"#.into(),
            r#"{"tools":"unchanged"}"#.into(),
            r#"{"tools":{}}"#.into(),
            request(s),
            request(u),
            request(&format!("{s}, \n{u},\t{s}")),
            request(&format!("{u}, \n{s},\t{u}")),
            request(&namespace(&format!("{u}, \n{s},\t{u}"))),
            request(&format!("{},\n{u}", namespace(&format!("{},\t{u}", namespace(u))))),
            request(&format!(r#"null, 123, {{"type":"namespace","tools":[]}}, {u}"#)),
            format!(r#"{{"tools":[{u}],"tools":[{s}]}}"#),
            format!(r#"{{"tools":[{u}],"incomplete":"#),
            format!(r#"{{"tools":[{u},"#),
        ];
        for (index, input) in inputs.iter().enumerate() {
            let body = input.as_bytes();
            let want = sequential_tool_schemas(body);
            let got = normalize_codex_tool_schemas(body);
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want),
                "case {index}: output differs from sequential edits"
            );
        }
    }

    // ---- NormalizeCodexToolIntegerTypes (internal/client/codex/tool-schema and helps tests) ----

    #[test]
    fn integer_types_codex_client_tool_field_types() {
        let input = br#"{
		"tools": [
			{
				"type": "function",
				"name": "exec_command",
				"parameters": {
					"type": "object",
					"properties": {
						"cmd": {"type": "string"},
						"yield_time_ms": {"type": "number"},
						"max_output_tokens": {"type": "number"},
						"timeout_ms": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "write_stdin",
				"parameters": {
					"type": "object",
					"properties": {
						"session_id": {"type": "number"},
						"yield_time_ms": {"type": "number"},
						"max_output_tokens": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "sleep",
				"parameters": {
					"type": "object",
					"properties": {
						"duration_ms": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "wait_agent",
				"parameters": {
					"type": "object",
					"properties": {
						"timeout_ms": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "wait",
				"parameters": {
					"type": "object",
					"properties": {
						"yield_time_ms": {"type": "number"},
						"max_tokens": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "tool_search",
				"parameters": {
					"type": "object",
					"properties": {
						"limit": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "test_sync_tool",
				"parameters": {
					"type": "object",
					"properties": {
						"sleep_before_ms": {"type": "number"},
						"sleep_after_ms": {"type": "number"},
						"participants": {"type": "number"},
						"timeout_ms": {"type": "number"}
					}
				}
			},
			{
				"type": "function",
				"name": "unrelated_tool",
				"parameters": {
					"type": "object",
					"properties": {
						"timeout_ms": {"type": "number"}
					}
				}
			}
		],
		"input": [
			{
				"type": "additional_tools",
				"tools": [
					{
						"type": "function",
						"name": "functions__exec_command",
						"parameters": {
							"type": "object",
							"properties": {
								"yield_time_ms": {"type": ["number", "null"]}
							}
						}
					}
				]
			}
		]
	}"#;
        let type_at = |out: &[u8], path: &str| out_json(out).g(path).str();

        // non-codex user agent preserves numbers
        let out = normalize_codex_tool_integer_types(input, Some(&user_agent("curl/8.7.1")));
        assert_eq!(type_at(&out, "tools.0.parameters.properties.yield_time_ms.type"), "number");

        // nil or empty headers leaves payload untouched
        let out = normalize_codex_tool_integer_types(input, None);
        assert_eq!(type_at(&out, "tools.0.parameters.properties.yield_time_ms.type"), "number");
        let out = normalize_codex_tool_integer_types(input, Some(&HeaderMap::new()));
        assert_eq!(type_at(&out, "tools.0.parameters.properties.yield_time_ms.type"), "number");

        // codex user agent normalizes specified fields
        let out = normalize_codex_tool_integer_types(input, Some(&user_agent("codex-tui/0.154.0 (Mac OS 26.5.2; arm64)")));
        let v = out_json(&out);
        let tool_map: std::collections::HashMap<String, Value> =
            v.g("tools").array().iter().map(|t| (t.g("name").str(), t.value())).collect();
        let expected: [(&str, &[&str]); 7] = [
            ("exec_command", &["yield_time_ms", "max_output_tokens", "timeout_ms"]),
            ("write_stdin", &["session_id", "yield_time_ms", "max_output_tokens"]),
            ("sleep", &["duration_ms"]),
            ("wait_agent", &["timeout_ms"]),
            ("wait", &["yield_time_ms", "max_tokens"]),
            ("tool_search", &["limit"]),
            ("test_sync_tool", &["sleep_before_ms", "sleep_after_ms", "participants", "timeout_ms"]),
        ];
        for (tool_name, fields) in expected {
            let tool = tool_map.get(tool_name).unwrap_or_else(|| panic!("missing tool: {tool_name}"));
            for field in fields {
                assert_eq!(
                    tool.g(&format!("parameters.properties.{field}.type")).str(),
                    "integer",
                    "tool {tool_name} field {field}"
                );
            }
        }
        assert_eq!(tool_map["exec_command"].g("parameters.properties.cmd.type").str(), "string");
        assert_eq!(tool_map["unrelated_tool"].g("parameters.properties.timeout_ms.type").str(), "number");
        let add_tool_node = v.g("input.0.tools.0.parameters.properties.yield_time_ms.type");
        let add_tool_type = add_tool_node.array();
        assert!(
            add_tool_type.len() == 2 && add_tool_type[0].str() == "integer" && add_tool_type[1].str() == "null",
            "additional_tools yield_time_ms type"
        );
    }

    #[test]
    fn integer_types_array_with_number_and_integer_deduplicates() {
        let input = br#"{
			"tools": [
				{
					"type": "function",
					"name": "sleep",
					"parameters": {
						"type": "object",
						"properties": {
							"duration_ms": {"type": ["number", "integer", "null"]}
						}
					}
				}
			]
		}"#;
        let out = normalize_codex_tool_integer_types(input, Some(&user_agent("codex-tui/0.154.0")));
        let v = out_json(&out);
        let arr_node = v.g("tools.0.parameters.properties.duration_ms.type");
        let arr = arr_node.array();
        assert!(arr.len() == 2 && arr[0].str() == "integer" && arr[1].str() == "null", "deduplicated type = {arr:?}");
    }

    #[test]
    fn integer_types_claude_input_schema() {
        let input = br#"{
			"tools": [
				{
					"name": "exec_command",
					"input_schema": {
						"type": "object",
						"properties": {
							"yield_time_ms": {"type": "number"},
							"timeout_ms": {"type": "number"}
						}
					}
				}
			]
		}"#;
        let v = out_json(&normalize_codex_tool_integer_types(input, Some(&user_agent("codex-tui/0.154.0"))));
        assert_eq!(v.g("tools.0.input_schema.properties.yield_time_ms.type").str(), "integer");
        assert_eq!(v.g("tools.0.input_schema.properties.timeout_ms.type").str(), "integer");
    }

    #[test]
    fn integer_types_gemini_function_declarations() {
        let input = br#"{
			"tools": [
				{
					"function_declarations": [
						{
							"name": "sleep",
							"parameters": {
								"type": "object",
								"properties": {
									"duration_ms": {"type": "number"}
								}
							}
						}
					]
				}
			]
		}"#;
        let v = out_json(&normalize_codex_tool_integer_types(input, Some(&user_agent("codex-desktop/0.159.0"))));
        assert_eq!(v.g("tools.0.function_declarations.0.parameters.properties.duration_ms.type").str(), "integer");
    }

    #[test]
    fn integer_types_preserves_pattern_and_one_of_on_unrelated_schemas() {
        let input = br#"{
			"tools": [
				{
					"type": "function",
					"name": "custom_schema_tool",
					"parameters": {
						"type": "object",
						"properties": {
							"regex_field": {
								"type": "string",
								"pattern": "\\p{L}+"
							},
							"choice_field": {
								"oneOf": [{"const": "a"}, {"const": "b"}]
							}
						}
					}
				}
			]
		}"#;
        let v = out_json(&normalize_codex_tool_integer_types(input, Some(&user_agent("codex-tui/0.154.0"))));
        assert!(v.g("tools.0.parameters.properties.regex_field.pattern").exists());
        assert!(v.g("tools.0.parameters.properties.choice_field.oneOf").exists());
    }

    #[test]
    fn integer_types_third_party_mcp_tools_are_not_modified() {
        let input = br#"{
			"tools": [
				{
					"type": "function",
					"name": "mcp__server__sleep",
					"parameters": {"type": "object", "properties": {"duration_ms": {"type": "number"}}}
				},
				{
					"type": "function",
					"name": "mcp__server__exec_command",
					"parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}
				},
				{
					"type": "function",
					"name": "functions__sleep",
					"parameters": {"type": "object", "properties": {"duration_ms": {"type": "number"}}}
				},
				{
					"type": "function",
					"name": "collab__exec_command",
					"parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}
				}
			]
		}"#;
        let v = out_json(&normalize_codex_tool_integer_types(input, Some(&user_agent("codex-tui/0.154.0"))));
        let tool_map: std::collections::HashMap<String, Value> =
            v.g("tools").array().iter().map(|t| (t.g("name").str(), t.value())).collect();
        let ty = |tool: &str, field: &str| tool_map[tool].g(&format!("parameters.properties.{field}.type")).str();
        assert_eq!(ty("mcp__server__sleep", "duration_ms"), "number");
        assert_eq!(ty("mcp__server__exec_command", "yield_time_ms"), "number");
        assert_eq!(ty("functions__sleep", "duration_ms"), "integer");
        assert_eq!(ty("collab__exec_command", "yield_time_ms"), "integer");
    }

    #[test]
    fn integer_types_gemini_parameters_json_schema() {
        let input = br#"{
			"tools": [
				{
					"functionDeclarations": [
						{
							"name": "exec_command",
							"parametersJsonSchema": {
								"type": "object",
								"properties": {
									"yield_time_ms": {"type": "number"}
								}
							}
						}
					]
				}
			]
		}"#;
        let v = out_json(&normalize_codex_tool_integer_types(input, Some(&user_agent("codex-desktop/0.159.0"))));
        assert_eq!(
            v.g("tools.0.functionDeclarations.0.parametersJsonSchema.properties.yield_time_ms.type").str(),
            "integer"
        );
    }
}
