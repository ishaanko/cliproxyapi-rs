//! Config `payload:` rules applied to outbound provider payloads (Go: helps/payload_helpers.go
//! and payload_mutations.go).
//!
//! Order of operations (matches Go): Codex-client integer normalization, `disable-image-generation`
//! stripping, `default` / `default-raw` (first write wins, only for paths absent from the original
//! request), `override` / `override-raw` (last write wins), then `filter` deletions. Rules match on
//! the upstream model and the client-requested model (glob `*`), optional protocol, source
//! protocol, header globs and JSON `match` / `not-match` / `exist` / `not-exist` conditions.
//! Paths are gjson/sjson paths under an optional `root` (for example `request` for the Gemini CLI
//! envelope) and may use `#(...)` queries that are expanded to concrete array indexes.

use std::collections::HashSet;

use cpa_config::{Config, DisableImageGenerationMode, PayloadModelRule};
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Value};
use cpa_runtime::executor::{Options, meta};
use http::HeaderMap;

use super::codex_tool_integers::{is_codex_user_agent, normalize_codex_tool_integer_types};

// ---------------------------------------------------------------- small mutations

/// Sets `path` to `value` unless it already holds that exact JSON string (other types are
/// normalized to the string).
pub fn set_string_if_different(payload: &mut Value, path: &str, value: &str) {
    let current = payload.g(path);
    if current.as_str() == Some(value) {
        return;
    }
    cpa_json::set(payload, path, value);
}

/// Sets `path` to `value` unless it already holds that exact JSON boolean.
pub fn set_bool_if_different(payload: &mut Value, path: &str, value: bool) {
    if payload.g(path).v() == Some(&Value::Bool(value)) {
        return;
    }
    cpa_json::set(payload, path, value);
}

/// Sets `path` to the parsed raw JSON `value` unless the existing value serializes identically.
/// Invalid raw JSON is ignored.
pub fn set_raw_if_different(payload: &mut Value, path: &str, value: &str) {
    let Ok(parsed) = serde_json::from_str::<Value>(value) else {
        return;
    };
    let current = payload.g(path);
    if current.v().is_some_and(|c| cpa_json::to_string(c) == cpa_json::to_string(&parsed)) {
        return;
    }
    cpa_json::set(payload, path, parsed);
}

/// Joins already-serialized JSON items into a JSON array without re-encoding them.
pub fn join_raw_json_array<T: AsRef<[u8]>>(items: &[T]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|i| i.as_ref().len() + 1).sum::<usize>() + 1);
    out.push(b'[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item.as_ref());
    }
    out.push(b']');
    out
}

/// Deletes a top-level or nested JSON field from a payload; returns the bytes unchanged when
/// the key is empty or the payload is not a JSON object (Go: DeleteJSONField).
pub fn delete_json_field(body: &[u8], key: &str) -> Vec<u8> {
    if key.is_empty() || body.is_empty() {
        return body.to_vec();
    }
    let mut v = cpa_json::parse(body);
    if !v.is_object() {
        return body.to_vec();
    }
    cpa_json::delete(&mut v, key);
    cpa_json::to_vec(&v)
}

// ---------------------------------------------------------------- request context accessors

/// Client-visible model for payload rules: `requested_model` metadata, else `fallback`.
pub fn payload_requested_model(opts: &Options, fallback: &str) -> String {
    let fallback = fallback.trim();
    match opts.metadata.get(meta::REQUESTED_MODEL).and_then(Value::as_str) {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => fallback.to_string(),
    }
}

/// Inbound HTTP request path from `request_path` metadata, "" when absent.
pub fn payload_request_path(opts: &Options) -> String {
    opts.metadata
        .get(meta::REQUEST_PATH)
        .and_then(Value::as_str)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- entry points

/// Everything one payload-rules run needs. `cfg == None` skips rules (Go: nil config).
#[derive(Clone, Copy, Default)]
pub struct PayloadRequest<'a> {
    pub cfg: Option<&'a Config>,
    /// Executor identifier ("codex", "claude", ...); Codex targets skip integer normalization.
    pub target_executor: &'a str,
    /// Upstream model after alias resolution.
    pub model: &'a str,
    /// Target protocol the payload is in.
    pub protocol: &'a str,
    /// Source (client) protocol, gated by rules' `from-protocol`.
    pub from_protocol: &'a str,
    /// Path prefix for all rule paths ("" or e.g. "request").
    pub root: &'a str,
    pub requested_model: &'a str,
    pub request_path: &'a str,
    pub headers: Option<&'a HeaderMap>,
}

/// Applies the payload rules and reports which tracked paths (or their descendants) were
/// targeted by an applied rule (Go: ApplyPayloadConfigWithTrackedPathsForExecutor).
///
/// `original` is the pre-translation payload that `default` rules check for existing paths
/// (falls back to `payload` when empty).
pub fn apply_payload_config_tracked(
    req: &PayloadRequest<'_>,
    payload: &[u8],
    original: &[u8],
    tracked_paths: &[&str],
) -> (Vec<u8>, HashSet<String>) {
    let mut touched = HashSet::new();
    if payload.is_empty() {
        return (payload.to_vec(), touched);
    }
    let mut payload_bytes = payload.to_vec();
    if is_codex_user_agent(req.headers) && !is_codex_target_executor(req.target_executor) {
        payload_bytes = normalize_codex_tool_integer_types(payload, req.headers);
    }
    let Some(cfg) = req.cfg else {
        return (payload_bytes, touched);
    };

    let strip_images = should_strip_image_generation(cfg.disable_image_generation, req.request_path);
    let rules = &cfg.payload;
    let has_rules = !rules.default.is_empty()
        || !rules.default_raw.is_empty()
        || !rules.r#override.is_empty()
        || !rules.override_raw.is_empty()
        || !rules.filter.is_empty();
    if !strip_images && !has_rules {
        return (payload_bytes, touched);
    }
    let mut out = cpa_json::parse(&payload_bytes);
    if !out.is_object() && !out.is_array() {
        return (payload_bytes, touched);
    }
    let mut changed = false;
    let tracked: Vec<&str> = tracked_paths.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    let mut mark_touched = |resolved: &str| {
        for tp in &tracked {
            if payload_rule_targets_path(resolved, tp) {
                touched.insert((*tp).to_string());
            }
        }
    };

    // disable-image-generation runs before payload rules so payload overrides can re-enable it.
    if strip_images {
        changed |= remove_tool_type_with_root(&mut out, req.root, "image_generation");
        changed |= remove_tool_choice_with_root(&mut out, req.root, "image_generation");
    }

    if has_rules {
        let model = req.model.trim();
        let requested_model = req.requested_model.trim();
        if !model.is_empty() || !requested_model.is_empty() {
            let candidates = payload_model_candidates(model, requested_model);
            let source_bytes = if original.is_empty() { payload } else { original };
            let source = cpa_json::parse(source_bytes);
            let mut applied_defaults: HashSet<String> = HashSet::new();
            let matches = |models: &[PayloadModelRule], out: &Value| {
                payload_model_rules_match(models, req.protocol, req.from_protocol, req.headers, out, req.root, &candidates)
            };

            // default: first write wins per field across all matching rules.
            for rule in &rules.default {
                if !matches(&rule.models, &out) {
                    continue;
                }
                for (path, value) in &rule.params {
                    let full_path = build_payload_path(req.root, path);
                    if full_path.is_empty() {
                        continue;
                    }
                    for resolved in resolve_payload_rule_paths(&out, &full_path) {
                        if source.g(&resolved).exists() || applied_defaults.contains(&resolved) {
                            continue;
                        }
                        if cpa_json::set(&mut out, &resolved, param_value(value)) {
                            changed = true;
                            applied_defaults.insert(resolved.clone());
                            mark_touched(&resolved);
                        }
                    }
                }
            }
            // default-raw: same, with raw JSON values.
            for rule in &rules.default_raw {
                if !matches(&rule.models, &out) {
                    continue;
                }
                for (path, value) in &rule.params {
                    let full_path = build_payload_path(req.root, path);
                    if full_path.is_empty() {
                        continue;
                    }
                    for resolved in resolve_payload_rule_paths(&out, &full_path) {
                        if source.g(&resolved).exists() || applied_defaults.contains(&resolved) {
                            continue;
                        }
                        let Some(raw) = payload_raw_value(value) else {
                            continue;
                        };
                        if cpa_json::set(&mut out, &resolved, raw) {
                            changed = true;
                            applied_defaults.insert(resolved.clone());
                            mark_touched(&resolved);
                        }
                    }
                }
            }
            // override: last write wins per field across all matching rules.
            for rule in &rules.r#override {
                if !matches(&rule.models, &out) {
                    continue;
                }
                for (path, value) in &rule.params {
                    let full_path = build_payload_path(req.root, path);
                    if full_path.is_empty() {
                        continue;
                    }
                    for resolved in resolve_payload_rule_paths(&out, &full_path) {
                        let (applied, did_change) =
                            set_payload_value_if_different(&mut out, &resolved, &param_value(value));
                        changed |= did_change;
                        if applied {
                            mark_touched(&resolved);
                        }
                    }
                }
            }
            // override-raw
            for rule in &rules.override_raw {
                if !matches(&rule.models, &out) {
                    continue;
                }
                for (path, value) in &rule.params {
                    let full_path = build_payload_path(req.root, path);
                    if full_path.is_empty() {
                        continue;
                    }
                    let Some(raw) = payload_raw_value(value) else {
                        continue;
                    };
                    for resolved in resolve_payload_rule_paths(&out, &full_path) {
                        let (applied, did_change) = set_payload_raw_value_if_different(&mut out, &resolved, &raw);
                        changed |= did_change;
                        if applied {
                            mark_touched(&resolved);
                        }
                    }
                }
            }
            // filter: remove matching paths, last index first so earlier indexes stay valid.
            for rule in &rules.filter {
                if !matches(&rule.models, &out) {
                    continue;
                }
                for path in &rule.params {
                    let full_path = build_payload_path(req.root, path);
                    if full_path.is_empty() {
                        continue;
                    }
                    let resolved_paths = resolve_payload_rule_paths(&out, &full_path);
                    for resolved in resolved_paths.iter().rev() {
                        cpa_json::delete(&mut out, resolved);
                        changed = true;
                        mark_touched(resolved);
                    }
                }
            }
        }
    }

    if changed {
        (cpa_json::to_vec(&out), touched)
    } else {
        (payload_bytes, touched)
    }
}

/// [`apply_payload_config_tracked`] without tracked paths (Go: ApplyPayloadConfigWithRequestForExecutor
/// and, with an empty `target_executor`, ApplyPayloadConfigWithRequest).
pub fn apply_payload_config(req: &PayloadRequest<'_>, payload: &[u8], original: &[u8]) -> Vec<u8> {
    apply_payload_config_tracked(req, payload, original, &[]).0
}

/// Reports whether an applied rule targeted `tracked_path` or one of its descendants.
pub fn apply_payload_config_with_request_tracked(
    req: &PayloadRequest<'_>,
    payload: &[u8],
    original: &[u8],
    tracked_path: &str,
) -> (Vec<u8>, bool) {
    let (out, touched) = apply_payload_config_tracked(req, payload, original, &[tracked_path]);
    (out, touched.contains(tracked_path))
}

fn is_codex_target_executor(target_executor: &str) -> bool {
    matches!(
        target_executor.trim().to_lowercase().as_str(),
        "codex" | "codex-websockets" | "codex_websockets"
    )
}

// ---------------------------------------------------------------- image generation gate

fn is_images_endpoint_request_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() {
        return false;
    }
    // Prefix routers may report a longer matched route, so suffixes count too.
    ["/images/generations", "/images/edits"].iter().any(|s| path.ends_with(s))
}

/// Whether the built-in image_generation tool must be removed for the mode and request path:
/// `All` strips everywhere, `Chat` only on non-images endpoints, `Off`/`Passthrough` never.
fn should_strip_image_generation(mode: DisableImageGenerationMode, request_path: &str) -> bool {
    match mode {
        DisableImageGenerationMode::All => true,
        DisableImageGenerationMode::Chat => !is_images_endpoint_request_path(request_path),
        _ => false,
    }
}

fn remove_tool_type_with_root(payload: &mut Value, root: &str, tool_type: &str) -> bool {
    let tool_type = tool_type.trim();
    if tool_type.is_empty() {
        return false;
    }
    let tools_path = build_payload_path(root, "tools");
    let tools = payload.g(&tools_path);
    let Some(Value::Array(items)) = tools.v() else {
        return false;
    };
    let is_target = |t: &Value| t.g("type").str() == tool_type;
    if !items.iter().any(is_target) {
        return false;
    }
    let filtered: Vec<Value> = items.iter().filter(|t| !is_target(t)).cloned().collect();
    cpa_json::set(payload, &tools_path, Value::Array(filtered))
}

fn remove_tool_choice_with_root(payload: &mut Value, root: &str, tool_type: &str) -> bool {
    let tool_type = tool_type.trim();
    if tool_type.is_empty() {
        return false;
    }
    let path = build_payload_path(root, "tool_choice");
    let choice = payload.g(&path);
    let remove = match choice.v() {
        None => false,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case(tool_type),
        Some(v @ (Value::Object(_) | Value::Array(_))) => {
            let choice_type = v.g("type").str();
            let choice_type = choice_type.trim();
            choice_type.eq_ignore_ascii_case(tool_type)
                || (choice_type.eq_ignore_ascii_case("tool")
                    && v.g("name").str().trim().eq_ignore_ascii_case(tool_type))
        }
        Some(_) => false,
    };
    if remove {
        cpa_json::delete(payload, &path);
    }
    remove
}

// ---------------------------------------------------------------- rule matching

fn payload_model_rules_match(
    rules: &[PayloadModelRule],
    protocol: &str,
    from_protocol: &str,
    headers: Option<&HeaderMap>,
    payload: &Value,
    root: &str,
    models: &[String],
) -> bool {
    if rules.is_empty() || models.is_empty() {
        return false;
    }
    for model in models {
        for entry in rules {
            let name = entry.name.trim();
            if name.is_empty() {
                continue;
            }
            let ep = entry.protocol.trim();
            if !ep.is_empty() && !protocol.is_empty() && !ep.eq_ignore_ascii_case(protocol) {
                continue;
            }
            if !payload_from_protocol_matches(&entry.from_protocol, from_protocol) {
                continue;
            }
            if !payload_headers_match(headers, &entry.headers) {
                continue;
            }
            if !match_model_pattern(name, model) {
                continue;
            }
            if payload_model_rule_conditions_match(payload, root, entry) {
                return true;
            }
        }
    }
    false
}

/// `(full path, expected value)` of every non-blank `match` / `not-match` condition entry.
fn condition_paths<V: serde::Serialize>(
    root: &str,
    conditions: &[std::collections::BTreeMap<String, V>],
) -> Vec<(String, Value)> {
    conditions
        .iter()
        .flat_map(|c| c.iter())
        .filter(|(p, _)| !p.trim().is_empty())
        .map(|(p, v)| (build_payload_path(root, p), param_value(v)))
        .collect()
}

fn payload_model_rule_conditions_match(payload: &Value, root: &str, rule: &PayloadModelRule) -> bool {
    // match: every condition must hold.
    if !condition_paths(root, &rule.r#match).iter().all(|(p, v)| payload_path_matches_value(payload, p, v)) {
        return false;
    }
    // not-match: no condition may hold.
    if condition_paths(root, &rule.not_match).iter().any(|(p, v)| payload_path_matches_value(payload, p, v)) {
        return false;
    }
    let exist_paths = |paths: &Vec<String>| {
        paths.iter().filter(|p| !p.trim().is_empty()).map(|p| build_payload_path(root, p)).collect::<Vec<_>>()
    };
    if !exist_paths(&rule.exist).iter().all(|p| payload_path_exists(payload, p)) {
        return false;
    }
    if exist_paths(&rule.not_exist).iter().any(|p| payload_path_exists(payload, p)) {
        return false;
    }
    true
}

fn payload_path_matches_value(payload: &Value, path: &str, value: &Value) -> bool {
    resolve_payload_rule_paths(payload, path).iter().any(|resolved| {
        let result = payload.g(resolved);
        result.v().is_some_and(|actual| json_equal(actual, value))
    })
}

fn payload_path_exists(payload: &Value, path: &str) -> bool {
    resolve_payload_rule_paths(payload, path)
        .iter()
        .any(|resolved| payload.g(resolved).v().is_some_and(|v| !v.is_null()))
}

/// Deep JSON equality as Go's `reflect.DeepEqual` over decoded values: numbers compare as
/// float64, objects ignore key order.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            match (x.to_string().parse::<f64>(), y.to_string().parse::<f64>()) {
                (Ok(x), Ok(y)) => x == y,
                _ => x == y,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_equal(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, xv)| y.get(k).is_some_and(|yv| json_equal(xv, yv)))
        }
        _ => a == b,
    }
}

fn payload_from_protocol_matches(pattern: &str, from_protocol: &str) -> bool {
    let pattern = normalize_payload_from_protocol(pattern);
    if pattern.is_empty() {
        return true;
    }
    let from_protocol = normalize_payload_from_protocol(from_protocol);
    if from_protocol.is_empty() {
        return false;
    }
    pattern.eq_ignore_ascii_case(&from_protocol)
}

fn normalize_payload_from_protocol(protocol: &str) -> String {
    let protocol = protocol.trim().to_lowercase();
    match protocol.as_str() {
        "openai-response" | "openai-responses" | "response" => "responses".into(),
        _ => protocol,
    }
}

fn payload_headers_match(headers: Option<&HeaderMap>, rules: &std::collections::BTreeMap<String, String>) -> bool {
    if rules.is_empty() {
        return true;
    }
    for (key, pattern) in rules {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let Some(headers) = headers else {
            return false;
        };
        let Ok(name) = http::HeaderName::from_bytes(key.as_bytes()) else {
            return false;
        };
        let values: Vec<&str> = headers.get_all(&name).iter().filter_map(|v| v.to_str().ok()).collect();
        if !values.iter().any(|v| match_model_pattern(pattern, v)) {
            return false;
        }
    }
    true
}

/// Rule candidates: upstream model, then the requested model without and with its thinking suffix
/// (deduplicated case-insensitively).
fn payload_model_candidates(model: &str, requested_model: &str) -> Vec<String> {
    let model = model.trim();
    let requested_model = requested_model.trim();
    let mut candidates: Vec<String> = Vec::with_capacity(3);
    let mut add = |value: &str| {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        if !candidates.iter().any(|c| c.eq_ignore_ascii_case(value)) {
            candidates.push(value.to_string());
        }
    };
    if !model.is_empty() {
        add(model);
    }
    if !requested_model.is_empty() {
        let parsed = parse_suffix(requested_model);
        let base = parsed.model_name.trim();
        if !base.is_empty() {
            add(base);
        }
        if parsed.has_suffix {
            add(requested_model);
        }
    }
    candidates
}

/// Joins an optional root with a relative parameter path.
fn build_payload_path(root: &str, path: &str) -> String {
    let r = root.trim();
    let p = path.trim();
    if r.is_empty() {
        return p.to_string();
    }
    if p.is_empty() {
        return r.to_string();
    }
    let p = p.strip_prefix('.').unwrap_or(p);
    format!("{r}.{p}")
}

fn payload_rule_targets_path(path: &str, tracked: &str) -> bool {
    if tracked.is_empty() || path.is_empty() {
        return false;
    }
    path == tracked || path.starts_with(&format!("{tracked}.")) || tracked.starts_with(&format!("{path}."))
}

// ---------------------------------------------------------------- path resolution

/// Expands `#(...)` / `#(...)#` query segments into concrete array indexes against `payload`.
/// Returns an empty list when a query matches nothing. Paths without queries pass through.
fn resolve_payload_rule_paths(payload: &Value, path: &str) -> Vec<String> {
    let path = path.trim();
    if path.is_empty() {
        return Vec::new();
    }
    if !path.contains("#(") {
        return vec![path.to_string()];
    }
    let parts = split_payload_rule_path(path);
    let mut paths = vec![String::new()];
    for part in &parts {
        let Some((query, all_matches)) = parse_payload_query_path_part(part) else {
            for p in &mut paths {
                *p = append_payload_path_part(p, part);
            }
            continue;
        };
        let mut next_paths = Vec::with_capacity(paths.len());
        for base in &paths {
            let array = if base.is_empty() {
                Some(payload.clone())
            } else {
                payload.g(base).v().cloned()
            };
            let Some(Value::Array(items)) = array else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                if !payload_query_matches(item, query) {
                    continue;
                }
                next_paths.push(append_payload_path_part(base, &index.to_string()));
                if !all_matches {
                    break;
                }
            }
        }
        paths = next_paths;
        if paths.is_empty() {
            return Vec::new();
        }
    }
    paths
}

/// Splits on `.` outside of parentheses and quotes (backslash escapes the next byte).
fn split_payload_rule_path(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut depth, mut quote, mut escaped) = (0usize, 0usize, 0u8, false);
    for (i, &ch) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == b'\\' {
            escaped = true;
            continue;
        }
        if quote != 0 {
            if ch == quote {
                quote = 0;
            }
            continue;
        }
        match ch {
            b'"' | b'\'' => quote = ch,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'.' if depth == 0 => {
                parts.push(&path[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&path[start..]);
    parts
}

/// `#(query)` -> (query, false), `#(query)#` -> (query, true); anything else is not a query part.
fn parse_payload_query_path_part(part: &str) -> Option<(&str, bool)> {
    if !part.starts_with("#(") {
        return None;
    }
    let close = find_payload_query_close(part)?;
    let suffix = &part[close + 1..];
    if !suffix.is_empty() && suffix != "#" {
        return None;
    }
    Some((part[2..close].trim(), suffix == "#"))
}

fn find_payload_query_close(part: &str) -> Option<usize> {
    let bytes = part.as_bytes();
    let (mut quote, mut escaped, mut depth) = (0u8, false, 1usize);
    for (i, &ch) in bytes.iter().enumerate().skip(2) {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == b'\\' {
            escaped = true;
            continue;
        }
        if quote != 0 {
            if ch == quote {
                quote = 0;
            }
            continue;
        }
        match ch {
            b'"' | b'\'' => quote = ch,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn append_payload_path_part(path: &str, part: &str) -> String {
    if path.is_empty() {
        part.to_string()
    } else if part.is_empty() {
        path.to_string()
    } else {
        format!("{path}.{part}")
    }
}

fn payload_query_matches(item: &Value, query: &str) -> bool {
    split_payload_logical(query, "||").iter().any(|or_part| {
        let parts = split_payload_logical(or_part, "&&");
        !parts.is_empty() && parts.iter().all(|term| payload_query_term_matches(item, term))
    })
}

/// Splits on a two-character logical operator outside quotes.
fn split_payload_logical(query: &str, operator: &str) -> Vec<String> {
    let bytes = query.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut quote, mut escaped) = (0usize, 0u8, false);
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if escaped {
            escaped = false;
        } else if ch == b'\\' {
            escaped = true;
        } else if quote != 0 {
            if ch == quote {
                quote = 0;
            }
        } else if ch == b'"' || ch == b'\'' {
            quote = ch;
        } else if query[i..].starts_with(operator) {
            parts.push(query[start..i].trim().to_string());
            i += operator.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(query[start..].trim().to_string());
    parts
}

fn payload_query_term_matches(item: &Value, term: &str) -> bool {
    let term = term.trim();
    if term.is_empty() {
        return false;
    }
    let wrapped = Value::Array(vec![item.clone()]);
    wrapped.g(&format!("#({term})")).exists()
}

// ---------------------------------------------------------------- value application

/// YAML floats arrive as JSON numbers with a fractional text form; Go's sjson writes float64
/// with `strconv 'f' -1`, so integral floats lose the `.0`. Re-format non-integer-text numbers.
fn normalize_numbers(value: &Value) -> Value {
    match value {
        Value::Number(n) => {
            let text = n.to_string();
            if text.contains(['.', 'e', 'E'])
                && let Ok(f) = text.parse::<f64>() {
                    return cpa_json::num_f64(f);
                }
            value.clone()
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), normalize_numbers(v))).collect()),
        _ => value.clone(),
    }
}

/// Sets `path` unless it already holds an equal value. Returns `(applied, changed)`: `applied`
/// is true when the value is in place afterwards (even if it already was), `changed` when the
/// document was modified.
fn set_payload_value_if_different(payload: &mut Value, path: &str, value: &Value) -> (bool, bool) {
    let current = payload.g(path);
    let same = match value {
        Value::String(s) => current.as_str() == Some(s.as_str()),
        Value::Bool(b) => current.v() == Some(&Value::Bool(*b)),
        Value::Null => current.v().is_some_and(Value::is_null),
        other => current
            .v()
            .is_some_and(|c| cpa_json::to_string(c) == cpa_json::to_string(other)),
    };
    if same {
        return (true, false);
    }
    let ok = cpa_json::set(payload, path, value.clone());
    (ok, ok)
}

fn set_payload_raw_value_if_different(payload: &mut Value, path: &str, value: &Value) -> (bool, bool) {
    let current = payload.g(path);
    if current.v().is_some_and(|c| cpa_json::to_string(c) == cpa_json::to_string(value)) {
        return (true, false);
    }
    let ok = cpa_json::set(payload, path, value.clone());
    (ok, ok)
}

/// The JSON value a `*-raw` rule parameter stands for: strings are parsed as raw JSON (invalid
/// JSON and null are skipped), other values are used as they are.
fn payload_raw_value(value: &impl serde::Serialize) -> Option<Value> {
    match serde_json::to_value(value).unwrap_or(Value::Null) {
        Value::Null => None,
        Value::String(raw) => serde_json::from_str::<Value>(&raw).ok(),
        other => Some(normalize_numbers(&other)),
    }
}

/// A config rule value (YAML-typed) as the JSON value written into the payload.
fn param_value(value: &impl serde::Serialize) -> Value {
    normalize_numbers(&serde_json::to_value(value).unwrap_or(Value::Null))
}

/// Glob match where `*` matches any run of characters (including none). Both sides are trimmed;
/// an empty pattern never matches.
pub fn match_model_pattern(pattern: &str, model: &str) -> bool {
    let pattern = pattern.trim().as_bytes();
    let model = model.trim().as_bytes();
    if pattern.is_empty() {
        return false;
    }
    if pattern == b"*" {
        return true;
    }
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;
    while si < model.len() {
        if pi < pattern.len() && pattern[pi] == model[si] {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = si;
            pi += 1;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            si = match_idx;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_config::{PayloadFilterRule, PayloadRule};
    use serde_json::json;

    fn model_rule(name: &str) -> PayloadModelRule {
        PayloadModelRule { name: name.into(), ..Default::default() }
    }

    fn rule(models: Vec<PayloadModelRule>, params: &[(&str, Value)]) -> PayloadRule {
        PayloadRule { models, params: params.iter().map(|(k, v)| (k.to_string(), serde_json::from_value(v.clone()).unwrap())).collect() }
    }

    fn run(cfg: &Config, payload: &str, original: &str) -> Value {
        let req = PayloadRequest {
            cfg: Some(cfg),
            target_executor: "claude",
            model: "gpt-5",
            protocol: "openai",
            from_protocol: "claude",
            requested_model: "gpt-5(high)",
            ..Default::default()
        };
        cpa_json::parse(&apply_payload_config(&req, payload.as_bytes(), original.as_bytes()))
    }

    #[test]
    fn defaults_respect_original_and_first_write_wins() {
        let mut cfg = Config::default();
        cfg.payload.default = vec![
            rule(vec![model_rule("gpt-*")], &[("temperature", json!(0.5)), ("top_p", json!(1))]),
            rule(vec![model_rule("*-5")], &[("temperature", json!(0.9))]),
        ];
        // top_p exists in the original request, so the default must not apply.
        let out = run(&cfg, r#"{"model":"x","top_p":0.3}"#, r#"{"model":"x","top_p":0.3}"#);
        assert_eq!(out.g("temperature").float(), 0.5);
        assert_eq!(out.g("top_p").float(), 0.3);
    }

    #[test]
    fn override_last_wins_and_float_formatting() {
        let mut cfg = Config::default();
        cfg.payload.r#override = vec![
            rule(vec![model_rule("gpt-5")], &[("reasoning.effort", json!("low")), ("temperature", json!(1.0))]),
            rule(vec![model_rule("gpt-5")], &[("reasoning.effort", json!("high"))]),
        ];
        let req = PayloadRequest { cfg: Some(&cfg), model: "gpt-5", ..Default::default() };
        let out = apply_payload_config(&req, br#"{"model":"gpt-5"}"#, b"");
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"model":"gpt-5","reasoning":{"effort":"high"},"temperature":1}"#);
    }

    #[test]
    fn raw_rules_and_filter_with_queries() {
        let mut cfg = Config::default();
        cfg.payload.override_raw = vec![rule(vec![model_rule("gpt-5")], &[("metadata", json!(r#"{"a":[1,2]}"#))])];
        cfg.payload.filter = vec![PayloadFilterRule {
            models: vec![model_rule("gpt-5")],
            params: vec!["tools.#(type==\"image_generation\")#.cache".into(), "stop".into()],
        }];
        let payload = r#"{"stop":["x"],"tools":[{"type":"function"},{"type":"image_generation","cache":1}]}"#;
        let out = run(&cfg, payload, "");
        assert_eq!(out.g("metadata.a.1").int(), 2);
        assert!(!out.g("stop").exists());
        assert!(!out.g("tools.1.cache").exists());
        assert_eq!(out.g("tools.1.type").str(), "image_generation");
    }

    #[test]
    fn conditions_gate_rules() {
        let mut cfg = Config::default();
        let mut gated = model_rule("gpt-5");
        gated.r#match = vec![[("reasoning.effort".to_string(), serde_json::from_value(json!("high")).unwrap())].into()];
        gated.not_exist = vec!["stop".into()];
        gated.from_protocol = "claude".into();
        gated.protocol = "OpenAI".into();
        cfg.payload.r#override = vec![rule(vec![gated], &[("flag", json!(true))])];
        assert!(run(&cfg, r#"{"reasoning":{"effort":"high"}}"#, "").g("flag").bool());
        assert!(!run(&cfg, r#"{"reasoning":{"effort":"low"}}"#, "").g("flag").exists());
        assert!(!run(&cfg, r#"{"reasoning":{"effort":"high"},"stop":"x"}"#, "").g("flag").exists());
    }

    #[test]
    fn header_and_requested_model_gates() {
        let mut cfg = Config::default();
        let mut gated = model_rule("gpt-5(high)");
        gated.headers = [("X-Client".to_string(), "codex-*".to_string())].into();
        cfg.payload.r#override = vec![rule(vec![gated], &[("flag", json!(1))])];
        let empty = HeaderMap::new();
        let mut req = PayloadRequest {
            cfg: Some(&cfg),
            model: "upstream",
            requested_model: "gpt-5(high)",
            headers: Some(&empty),
            ..Default::default()
        };
        assert!(!cpa_json::parse(&apply_payload_config(&req, b"{}", b"")).g("flag").exists());
        let mut headers = HeaderMap::new();
        headers.insert("x-client", "codex-cli".parse().unwrap());
        req.headers = Some(&headers);
        assert_eq!(cpa_json::parse(&apply_payload_config(&req, b"{}", b"")).g("flag").int(), 1);
    }

    #[test]
    fn image_generation_stripping_and_tracked_paths() {
        let mut cfg = Config::default();
        cfg.disable_image_generation = DisableImageGenerationMode::Chat;
        cfg.payload.r#override = vec![rule(vec![model_rule("*")], &[("tools", json!([]))])];
        let payload = br#"{"request":{"tools":[{"type":"image_generation"}],"tool_choice":{"type":"image_generation"}}}"#;
        let req = PayloadRequest {
            cfg: Some(&cfg),
            model: "m",
            root: "request",
            request_path: "/v1/images/generations",
            ..Default::default()
        };
        // Images endpoint keeps the tool in chat mode.
        let (out, touched) = apply_payload_config_tracked(&req, payload, b"", &["request.tools"]);
        assert!(touched.contains("request.tools"));
        assert_eq!(cpa_json::parse(&out).g("request.tool_choice.type").str(), "image_generation");
        cfg.payload.r#override.clear();
        let req = PayloadRequest { cfg: Some(&cfg), model: "m", root: "request", request_path: "/v1/chat/completions", ..Default::default() };
        let out = cpa_json::parse(&apply_payload_config(&req, payload, b""));
        assert_eq!(out.g("request.tools.#").int(), 0);
        assert!(!out.g("request.tool_choice").exists());
    }

    #[test]
    fn glob_matching() {
        assert!(match_model_pattern("gpt-*", "gpt-5"));
        assert!(match_model_pattern("*-5", "gpt-5"));
        assert!(match_model_pattern("gemini-*-pro", "gemini-2.5-pro"));
        assert!(!match_model_pattern("gemini-*-pro", "gemini-2.5-flash"));
        assert!(match_model_pattern("*", ""));
        assert!(!match_model_pattern("", "x"));
    }
}
