//! Claude OAuth tool-name remapping (Go: claude_executor_request.go lines 1645-2836).
//!
//! Requests sent with a Claude OAuth token present every client tool as a Claude Code style MCP
//! tool (`mcp__<word>_<word>__<word>_<semantic>`, see [`helps::mcp_alias`]). The request-local
//! reverse map returned by [`prepare_claude_oauth_tool_names_for_upstream`] must be passed to the
//! `restore_*` functions so only aliases allocated for this request are mapped back.
//!
//! Bodies are edited at the byte level like Go's gjson/sjson code: only the replaced values change
//! and everything else (whitespace, key order, escapes) is preserved. [`Raw`] is a small
//! offset-tracking scanner that stands in for `gjson.Result.Index`/`Raw`.

use std::collections::{HashMap, HashSet};
use std::fmt;

use cpa_json::Res;
use cpa_runtime::executor::{ErrorCode, ExecError};

use crate::claude::helps::builtin_tools::{
    augment_claude_builtin_tool_registry, is_claude_server_tool_type,
};
use crate::claude::helps::mcp_alias::{allocate_claude_mcp_tool_alias, is_claude_mcp_tool_name};
use crate::helps::text::{json_payload, trim_space};

/// Secret used when the request carries no downstream caller API key.
const DEFAULT_ALIAS_SECRET: &str = "cpa-claude-mcp-default-caller";

/// Go: `isClaudeOAuthToken`.
pub fn is_claude_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Go: `claudeMCPAliasOptions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMcpAliasOptions {
    pub secret: String,
}

impl Default for ClaudeMcpAliasOptions {
    fn default() -> Self {
        Self {
            secret: DEFAULT_ALIAS_SECRET.to_string(),
        }
    }
}

/// Go: `resolveClaudeMCPAliasOptions(ctx)`. Alias identity belongs to the downstream caller, not
/// the upstream credential, so the secret is the caller's API key (Go reads it from
/// `ctx.Value("gin").Get("userApiKey")` via `helps.APIKeyFromContext`; the executor passes that
/// same string here, or `""` when the request has none).
pub fn resolve_claude_mcp_alias_options(downstream_api_key: &str) -> ClaudeMcpAliasOptions {
    let secret = downstream_api_key.trim();
    if secret.is_empty() {
        return ClaudeMcpAliasOptions::default();
    }
    ClaudeMcpAliasOptions {
        secret: secret.to_string(),
    }
}

/// Go: `claudeMCPAliasRestoreError`. Request-scoped failure while mapping an alias in a response
/// back to the client's tool name; Go reports no status code of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMcpAliasRestoreError(pub String);

impl fmt::Display for ClaudeMcpAliasRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClaudeMcpAliasRestoreError {}

impl ClaudeMcpAliasRestoreError {
    /// Request-scoped `ExecError` (no failover). Status is 500; non-stream callers that Go wraps
    /// with `wrapClaudeFastRequestError(.., httpResp.StatusCode, ..)` may overwrite `.status`.
    pub fn into_exec_error(self) -> ExecError {
        ExecError::new(500, self.0).with_code(ErrorCode::RequestScoped)
    }
}

type ReverseMap = HashMap<String, String>;

/// Go: `prepareClaudeOAuthToolNamesForUpstream`. Applies one request-local MCP symbol table to
/// `body` and returns the rewritten body plus the `upstream name -> client name` map.
pub fn prepare_claude_oauth_tool_names_for_upstream(
    body: &[u8],
    mcp_aliases: &ClaudeMcpAliasOptions,
) -> (Vec<u8>, ReverseMap) {
    remap_oauth_tool_names_with_options(body, mcp_aliases)
}

/// Go: `restoreClaudeOAuthToolNamesFromResponse`.
pub fn restore_claude_oauth_tool_names_from_response(
    body: &[u8],
    reverse_map: &ReverseMap,
) -> Result<Vec<u8>, ClaudeMcpAliasRestoreError> {
    reverse_remap_oauth_tool_names(body, reverse_map)
}

/// Go: `restoreClaudeOAuthToolNamesFromStreamLine`.
pub fn restore_claude_oauth_tool_names_from_stream_line(
    line: &[u8],
    reverse_map: &ReverseMap,
) -> Result<Vec<u8>, ClaudeMcpAliasRestoreError> {
    reverse_remap_oauth_tool_names_from_stream_line(line, reverse_map)
}

/// Go: `remapOAuthToolNames` (default caller secret).
pub fn remap_oauth_tool_names(body: &[u8]) -> (Vec<u8>, ReverseMap) {
    remap_oauth_tool_names_with_options(body, &ClaudeMcpAliasOptions::default())
}

/// Go: `remapOAuthToolNamesWithOptions`. Batched byte edits, falling back to the legacy path for
/// malformed JSON.
pub fn remap_oauth_tool_names_with_options(
    body: &[u8],
    mcp_aliases: &ClaudeMcpAliasOptions,
) -> (Vec<u8>, ReverseMap) {
    match remap_oauth_tool_names_with_batched_edits(body, mcp_aliases) {
        Some(result) => result,
        None => remap_oauth_tool_names_with_options_legacy(body, mcp_aliases),
    }
}

// ---------------------------------------------------------------- JSON node access

/// A value inside `src` located by byte offsets (gjson `Result.Index` + `Raw`). Scanning is
/// tolerant of truncated documents like gjson (end of input closes every open container), which
/// the legacy remap path relies on.
#[derive(Clone, Copy)]
struct Raw<'a> {
    src: &'a [u8],
    start: usize,
    end: usize,
}

fn skip_ws(src: &[u8], mut i: usize) -> usize {
    while src.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

/// Index just past the closing quote of the string starting at `start` (end of input when
/// unterminated).
fn string_end(src: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while let Some(&b) = src.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    src.len()
}

/// Index just past the value starting at `start` (end of input for unterminated containers).
fn value_end(src: &[u8], start: usize) -> Option<usize> {
    match *src.get(start)? {
        b'"' => Some(string_end(src, start)),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            while let Some(&b) = src.get(i) {
                match b {
                    b'"' => {
                        i = string_end(src, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            Some(src.len())
        }
        _ => {
            let len = src[start..]
                .iter()
                .position(|b| matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace())
                .unwrap_or(src.len() - start);
            Some(start + len)
        }
    }
}

/// Whether the quoted key bytes decode to `key`.
fn key_matches(quoted: &[u8], key: &str) -> bool {
    if quoted.len() < 2 {
        return false;
    }
    let inner = &quoted[1..quoted.len() - 1];
    if !inner.contains(&b'\\') {
        return inner == key.as_bytes();
    }
    serde_json::from_slice::<String>(quoted).is_ok_and(|k| k == key)
}

impl<'a> Raw<'a> {
    /// The document root.
    fn root(src: &'a [u8]) -> Option<Self> {
        let start = skip_ws(src, 0);
        let end = value_end(src, start)?;
        Some(Raw { src, start, end })
    }

    fn bytes(&self) -> &'a [u8] {
        &self.src[self.start..self.end.min(self.src.len())]
    }

    /// Calls `f(quoted key (objects only), child)` for every child until it returns false.
    /// `None` when this is not a container or the document is malformed beyond truncation.
    fn for_each_child(&self, mut f: impl FnMut(Option<&'a [u8]>, Raw<'a>) -> bool) -> Option<()> {
        let src = self.src;
        let is_object = match *src.get(self.start)? {
            b'{' => true,
            b'[' => false,
            _ => return None,
        };
        let mut i = skip_ws(src, self.start + 1);
        loop {
            match src.get(i) {
                None | Some(b'}') | Some(b']') => return Some(()),
                _ => {}
            }
            let mut key = None;
            if is_object {
                let key_end = string_end(src, i);
                key = Some(&src[i..key_end.min(src.len())]);
                i = skip_ws(src, key_end);
                match src.get(i) {
                    Some(b':') => {}
                    None => return Some(()),
                    Some(_) => return None,
                }
                i = skip_ws(src, i + 1);
            }
            let end = value_end(src, i)?;
            if !f(key, Raw { src, start: i, end }) {
                return Some(());
            }
            i = skip_ws(src, end);
            match src.get(i) {
                Some(b',') => i = skip_ws(src, i + 1),
                _ => return Some(()),
            }
        }
    }

    /// First member `key` of an object (None for non-objects or a missing key).
    fn member(&self, key: &str) -> Option<Raw<'a>> {
        let mut found = None;
        self.for_each_child(|k, child| {
            if k.is_some_and(|k| key_matches(k, key)) {
                found = Some(child);
                return false;
            }
            true
        })?;
        found
    }

    /// Elements when this node is an array.
    fn elements(&self) -> Option<Vec<Raw<'a>>> {
        if self.src.get(self.start) != Some(&b'[') {
            return None;
        }
        let mut out = Vec::new();
        self.for_each_child(|_, child| {
            out.push(child);
            true
        })?;
        Some(out)
    }

    /// gjson `Result.String()`.
    fn text(&self) -> String {
        let b = self.bytes();
        if b.len() >= 2 && b[0] == b'"' && !b.contains(&b'\\') {
            return String::from_utf8_lossy(&b[1..b.len() - 1]).into_owned();
        }
        Res::of(&cpa_json::parse(b)).str()
    }
}

/// Dotted-path lookup through [`Raw::member`] hops (gjson `Get` for plain key paths).
fn get<'a>(node: &Raw<'a>, path: &str) -> Option<Raw<'a>> {
    let mut parts = path.split('.');
    let mut cur = node.member(parts.next()?)?;
    for part in parts {
        cur = cur.member(part)?;
    }
    Some(cur)
}

fn get_text(node: &Raw<'_>, path: &str) -> String {
    get(node, path).map(|n| n.text()).unwrap_or_default()
}

// ---------------------------------------------------------------- sjson-style string edits

/// sjson `appendStringify`: strings with only plain ASCII are quoted verbatim; anything else goes
/// through Go's `json.Marshal` (HTML escaping on).
fn sjson_string(s: &str) -> String {
    let must_marshal = s
        .bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\');
    if !must_marshal {
        return format!("\"{s}\"");
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// sjson `deleteTailItem`: strips the previous key or comma from `buf` (the text before a value
/// being deleted). Returns the trimmed buffer and whether the following comma must go too.
fn delete_tail_item(mut buf: Vec<u8>) -> (Vec<u8>, bool) {
    let mut i = buf.len() as isize - 1;
    'outer: while i >= 0 {
        match buf[i as usize] {
            b'[' => return (buf, true),
            b',' => {
                buf.truncate(i as usize);
                return (buf, false);
            }
            b':' => {
                i -= 1;
                while i >= 0 {
                    if buf[i as usize] == b'"' {
                        i -= 1;
                        while i >= 0 {
                            if buf[i as usize] == b'"' {
                                i -= 1;
                                if i >= 0 && buf[i as usize] == b'\\' {
                                    // Go: `i--; continue` plus the loop's own `i--`.
                                    i -= 2;
                                    continue;
                                }
                                while i >= 0 {
                                    match buf[i as usize] {
                                        b'{' => {
                                            buf.truncate(i as usize + 1);
                                            return (buf, true);
                                        }
                                        b',' => {
                                            buf.truncate(i as usize);
                                            return (buf, false);
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
                break 'outer;
            }
            _ => {}
        }
        i -= 1;
    }
    (buf, false)
}

/// `sjson.Delete(json, key)` for a top-level object member; `None` when nothing changes.
fn sjson_delete_member(json: &[u8], key: &str) -> Option<Vec<u8>> {
    let member = Raw::root(json)?.member(key)?;
    if member.start == 0 {
        return None;
    }
    let (mut out, delete_next_comma) = delete_tail_item(json[..member.start].to_vec());
    let mut resume = member.end;
    if delete_next_comma {
        for (j, &b) in json[member.end..].iter().enumerate() {
            if b <= b' ' {
                continue;
            }
            if b == b',' {
                resume = member.end + j + 1;
            }
            break;
        }
    }
    out.extend_from_slice(&json[resume..]);
    Some(out)
}

/// `sjson.Set(json, key, string)` for an existing top-level object member.
fn sjson_set_member_string(json: &[u8], key: &str, value: &str) -> Option<Vec<u8>> {
    let member = Raw::root(json)?.member(key)?;
    let mut out = Vec::with_capacity(json.len() + value.len());
    out.extend_from_slice(&json[..member.start]);
    out.extend_from_slice(sjson_string(value).as_bytes());
    out.extend_from_slice(&json[member.end..]);
    Some(out)
}

/// Go: `claudeRawJSONEdit`.
struct RawEdit {
    start: usize,
    end: usize,
    replacement: String,
}

impl RawEdit {
    fn replace(node: &Raw<'_>, replacement: String) -> Self {
        RawEdit {
            start: node.start,
            end: node.end,
            replacement,
        }
    }
}

/// Go: `applyClaudeRawJSONEdits`. Applies non-overlapping edits in one copy; `None` for invalid
/// ranges.
fn apply_raw_edits(body: &[u8], mut edits: Vec<RawEdit>) -> Option<Vec<u8>> {
    if edits.is_empty() {
        return Some(body.to_vec());
    }
    edits.sort_by_key(|e| e.start);
    let mut final_size = body.len() as isize;
    let mut cursor = 0usize;
    for e in &edits {
        if e.start < cursor || e.end < e.start || e.end > body.len() {
            return None;
        }
        final_size += e.replacement.len() as isize - (e.end - e.start) as isize;
        if final_size < 0 {
            return None;
        }
        cursor = e.end;
    }
    let mut out = Vec::with_capacity(final_size as usize);
    cursor = 0;
    for e in &edits {
        out.extend_from_slice(&body[cursor..e.start]);
        out.extend_from_slice(e.replacement.as_bytes());
        cursor = e.end;
    }
    out.extend_from_slice(&body[cursor..]);
    Some(out)
}

// ---------------------------------------------------------------- reference traversal

/// Go: `claudeToolChangeNamePath`. Path, relative to a mid-conversation tool_addition or
/// tool_removal block, of the tool name that must carry the same MCP alias as tools[].
fn claude_tool_change_name_path(part: &Raw<'_>) -> Option<&'static str> {
    match get_text(part, "tool.type").as_str() {
        "tool_reference" => Some("tool.name"),
        "tool_definition" => {
            if get_text(part, "type") != "tool_addition"
                || is_claude_server_tool_type(&get_text(part, "tool.definition.type"))
            {
                return None;
            }
            Some("tool.definition.name")
        }
        _ => None,
    }
}

/// Calls `visit` with the name node of every tool reference in one content part (tool_use,
/// tool_reference, nested tool_result references, tool_search_tool_result references and, when
/// `tool_changes`, tool_addition/tool_removal). Stops and returns false when `visit` returns false.
fn walk_part<'a>(
    part: &Raw<'a>,
    tool_changes: bool,
    visit: &mut impl FnMut(&Raw<'a>) -> bool,
) -> bool {
    match get_text(part, "type").as_str() {
        "tool_use" => {
            if let Some(n) = part.member("name") {
                return visit(&n);
            }
        }
        "tool_reference" => {
            if let Some(n) = part.member("tool_name") {
                return visit(&n);
            }
        }
        "tool_result" => {
            let nested = part
                .member("content")
                .and_then(|c| c.elements())
                .unwrap_or_default();
            if !visit_tool_references(&nested, visit) {
                return false;
            }
        }
        "tool_search_tool_result" => {
            let refs = get(part, "content.tool_references")
                .and_then(|c| c.elements())
                .unwrap_or_default();
            if !visit_tool_references(&refs, visit) {
                return false;
            }
        }
        "tool_addition" | "tool_removal" if tool_changes => {
            if let Some(n) = claude_tool_change_name_path(part).and_then(|path| get(part, path)) {
                return visit(&n);
            }
        }
        _ => {}
    }
    true
}

/// Visits `tool_name` of every `type:"tool_reference"` item.
fn visit_tool_references<'a>(items: &[Raw<'a>], visit: &mut impl FnMut(&Raw<'a>) -> bool) -> bool {
    for item in items {
        if get_text(item, "type") != "tool_reference" {
            continue;
        }
        if let Some(n) = item.member("tool_name")
            && !visit(&n)
        {
            return false;
        }
    }
    true
}

/// Walks every `messages[].content[]` part (including tool_addition/tool_removal blocks).
fn walk_messages<'a>(messages: &Raw<'a>, mut visit: impl FnMut(&Raw<'a>) -> bool) -> bool {
    let Some(msgs) = messages.elements() else {
        return true;
    };
    for msg in &msgs {
        let Some(parts) = msg.member("content").and_then(|c| c.elements()) else {
            continue;
        };
        for part in &parts {
            if !walk_part(part, true, &mut visit) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------- forward alias table

/// Request-local forward table built from the declared tools (shared by both remap paths).
struct AliasPlan {
    forward: HashMap<String, String>,
    protected: HashSet<String>,
    reverse: ReverseMap,
}

impl AliasPlan {
    /// Go: the declaration pre-pass of `remapOAuthToolNamesWith*`. `decls` are the
    /// `(type, name)` pairs of `tools[]`, or `None` when `tools` is not an array.
    fn build(decls: Option<&[(String, String)]>, secret: &str) -> Self {
        let mut plan = AliasPlan {
            forward: HashMap::new(),
            protected: HashSet::new(),
            reverse: HashMap::new(),
        };
        // Default builtin seed; the typed built-ins Go adds from the body are covered by the loop below,
        // which reserves every declared name.
        let mut reserved = augment_claude_builtin_tool_registry(b"", None);
        let Some(decls) = decls else {
            return plan;
        };
        for (ty, name) in decls {
            if !name.is_empty() {
                reserved.insert(name.clone());
            }
            if is_claude_server_tool_type(ty) {
                plan.protected.insert(name.clone());
            }
        }
        let mut passthrough: Vec<&str> = Vec::new();
        for (ty, name) in decls {
            if is_claude_server_tool_type(ty) || name.is_empty() {
                continue;
            }
            if is_claude_mcp_tool_name(name) {
                passthrough.push(name);
                continue;
            }
            if plan.forward.contains_key(name) {
                continue;
            }
            let Some(alias) = allocate_claude_mcp_tool_alias(secret, name, Some(&reserved)) else {
                tracing::warn!(
                    "claude oauth mcp alias: no free alias left for tool {name:?}, forwarding the original name"
                );
                continue;
            };
            plan.forward.insert(name.clone(), alias.clone());
            reserved.insert(alias);
        }
        // Go: recordPassthroughMCPTools. Skipped when nothing was aliased so an untouched
        // request keeps an empty reverse map and the restore path stays a no-op.
        if !plan.forward.is_empty() {
            for name in passthrough {
                plan.record(name, name);
            }
        }
        plan
    }

    /// Go: `rewriteName`.
    fn rewrite_name(&self, name: &str) -> Option<String> {
        if name.is_empty() || self.protected.contains(name) || is_claude_mcp_tool_name(name) {
            return None;
        }
        self.forward
            .get(name)
            .filter(|n| n.as_str() != name)
            .cloned()
    }

    /// Go: `recordRename`. Keeps the first-seen original for an upstream name.
    fn record(&mut self, original: &str, renamed: &str) {
        self.reverse
            .entry(renamed.to_string())
            .or_insert_with(|| original.to_string());
    }
}

fn declared_tools(tools: &Raw<'_>) -> Option<Vec<(String, String)>> {
    let items = tools.elements()?;
    Some(
        items
            .iter()
            .map(|t| (get_text(t, "type"), get_text(t, "name")))
            .collect(),
    )
}

fn type_is_set(ty: &str) -> bool {
    !ty.trim().is_empty()
}

/// Whether the tools array needs rebuilding (a typed client tool exists, or a name gets renamed).
fn tools_need_rewrite(decls: &[(String, String)], plan: &AliasPlan) -> bool {
    for (ty, name) in decls {
        if is_claude_server_tool_type(ty) {
            continue;
        }
        if type_is_set(ty) {
            return true;
        }
        if plan.rewrite_name(name).is_some() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- forward remap

/// Go: `remapOAuthToolNamesWithBatchedEdits`. Records offsets from the original JSON and applies
/// every rename in one copy. `None` when the body is not valid JSON (callers fall back to the
/// legacy path).
pub fn remap_oauth_tool_names_with_batched_edits(
    body: &[u8],
    mcp_aliases: &ClaudeMcpAliasOptions,
) -> Option<(Vec<u8>, ReverseMap)> {
    if !cpa_json::valid(body) {
        return None;
    }
    remap_with_raw_edits(body, mcp_aliases)
}

/// Go: `remapOAuthToolNamesWithOptionsLegacy`, the fallback for malformed JSON. Go edits the
/// original bytes with repeated sjson sets over gjson's tolerant parse; the same byte-level result
/// comes from running the offset-based edits over the truncation-tolerant [`Raw`] scanner, so one
/// implementation serves both. Input that cannot be scanned at all is returned unchanged.
pub fn remap_oauth_tool_names_with_options_legacy(
    body: &[u8],
    mcp_aliases: &ClaudeMcpAliasOptions,
) -> (Vec<u8>, ReverseMap) {
    remap_with_raw_edits(body, mcp_aliases).unwrap_or_else(|| (body.to_vec(), HashMap::new()))
}

/// Shared core of the batched and legacy remap: builds the request-local alias table, then
/// collects `tools`, `tool_choice` and message-history edits against the original offsets.
fn remap_with_raw_edits(
    body: &[u8],
    mcp_aliases: &ClaudeMcpAliasOptions,
) -> Option<(Vec<u8>, ReverseMap)> {
    let root = Raw::root(body)?;
    let tools = root.member("tools");
    let decls = tools.as_ref().and_then(declared_tools);
    let mut plan = AliasPlan::build(decls.as_deref(), &mcp_aliases.secret);
    let mut edits: Vec<RawEdit> = Vec::new();

    // 1. Rebuild typed custom tools; the original array is replaced only after all offsets are
    // collected.
    if let (Some(tools), Some(decls)) = (&tools, &decls)
        && tools_need_rewrite(decls, &plan)
    {
        let items = tools.elements().unwrap_or_default();
        let mut tools_json: Vec<u8> = vec![b'['];
        for (i, (tool, (ty, name))) in items.iter().zip(decls).enumerate() {
            if i > 0 {
                tools_json.push(b',');
            }
            let mut tool_json = tool.bytes().to_vec();
            if is_claude_server_tool_type(ty) {
                tools_json.extend_from_slice(&tool_json);
                continue;
            }
            if type_is_set(ty)
                && let Some(updated) = sjson_delete_member(&tool_json, "type")
            {
                tool_json = updated;
            }
            if let Some(new_name) = plan.rewrite_name(name)
                && let Some(updated) = sjson_set_member_string(&tool_json, "name", &new_name)
            {
                tool_json = updated;
                plan.record(name, &new_name);
            }
            tools_json.extend_from_slice(&tool_json);
        }
        tools_json.push(b']');
        edits.push(RawEdit::replace(tools, String::from_utf8(tools_json).ok()?));
    }

    // 2. tool_choice
    if let Some(tool_choice) = root.member("tool_choice")
        && get_text(&tool_choice, "type") == "tool"
        && let Some(name_node) = tool_choice.member("name")
    {
        let name = name_node.text();
        if let Some(new_name) = plan.rewrite_name(&name) {
            edits.push(RawEdit::replace(&name_node, sjson_string(&new_name)));
            plan.record(&name, &new_name);
        }
    }

    // 3. messages (every offset still points into the original bytes)
    if let Some(messages) = root.member("messages") {
        walk_messages(&messages, |node| {
            let name = node.text();
            if let Some(new_name) = plan.rewrite_name(&name) {
                edits.push(RawEdit::replace(node, sjson_string(&new_name)));
                plan.record(&name, &new_name);
            }
            true
        });
    }

    let remapped = apply_raw_edits(body, edits)?;
    Some((remapped, plan.reverse))
}

// ---------------------------------------------------------------- alias resolver (reverse map)

/// Go: `claudeMCPAliasParts`.
struct AliasParts<'a> {
    server: &'a str,
    semantic: &'a str,
}

/// Go: `parseClaudeMCPAlias`.
fn parse_claude_mcp_alias(name: &str) -> Option<AliasParts<'_>> {
    if !is_claude_mcp_tool_name(name) {
        return None;
    }
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() {
        return None;
    }
    let (tool_id, semantic) = tool.split_once('_')?;
    if tool_id.is_empty() || semantic.is_empty() {
        return None;
    }
    Some(AliasParts { server, semantic })
}

/// Go: `claudeMCPAliasServer`.
fn claude_mcp_alias_server(name: &str) -> &str {
    name.strip_prefix("mcp__")
        .and_then(|r| r.split_once("__"))
        .map_or("", |(s, _)| s)
}

struct AliasEntry<'a> {
    alias: &'a str,
    original: &'a str,
    parts: AliasParts<'a>,
}

/// Go: `claudeMCPAliasResolver`. Exact lookup first, then fuzzy recovery for names the model
/// drifted away from (repeated server prefix, wrong tool id, extra words).
struct AliasResolver<'a> {
    exact: &'a ReverseMap,
    aliases: Vec<AliasEntry<'a>>,
    servers: HashSet<&'a str>,
    passthroughs: Vec<&'a str>,
}

impl<'a> AliasResolver<'a> {
    /// Go: `newClaudeMCPAliasResolver`.
    fn new(reverse_map: &'a ReverseMap) -> Self {
        let mut resolver = AliasResolver {
            exact: reverse_map,
            aliases: Vec::with_capacity(reverse_map.len()),
            servers: HashSet::new(),
            passthroughs: Vec::new(),
        };
        for (alias, original) in reverse_map {
            if alias == original {
                // Caller-owned MCP tool: exact passthrough and hybrid recovery only.
                resolver.passthroughs.push(original);
                continue;
            }
            let Some(parts) = parse_claude_mcp_alias(alias) else {
                continue;
            };
            resolver.servers.insert(parts.server);
            resolver.aliases.push(AliasEntry {
                alias,
                original,
                parts,
            });
        }
        resolver
    }

    /// Go: `claudeMCPAliasResolver.resolve`. `Ok(Some(original))` when `name` maps back,
    /// `Ok(None)` to forward it unchanged.
    fn resolve(&self, name: &str) -> Result<Option<String>, ClaudeMcpAliasRestoreError> {
        if let Some(original) = self.exact.get(name) {
            if original == name {
                // Caller-owned MCP tool: forward it exactly as the client declared it.
                return Ok(None);
            }
            return Ok(Some(original.clone()));
        }

        let server = claude_mcp_alias_server(name);
        if !self.servers.contains(server) {
            return Ok(None);
        }

        let canonical_prefix = format!("mcp__{server}__");
        let server_prefix = format!("{server}__");
        let mut normalized = name.to_string();
        let mut suffix = name.strip_prefix(canonical_prefix.as_str()).unwrap_or(name);
        while let Some(stripped) = suffix.strip_prefix(server_prefix.as_str()) {
            suffix = stripped;
            normalized = format!("{canonical_prefix}{suffix}");
            if let Some(original) = self.exact.get(&normalized) {
                return Ok(Some(original.clone()));
            }
        }

        let mut matched_original = "";
        let mut match_count = 0usize;
        for entry in &self.aliases {
            if entry.parts.server == server && name.ends_with(entry.alias) {
                matched_original = entry.original;
                match_count += 1;
            }
        }
        if match_count == 1 {
            return Ok(Some(matched_original.to_string()));
        }
        if match_count > 1 {
            return Err(restore_error(format!(
                "cannot restore Claude OAuth MCP tool alias {name:?}: matched multiple declared aliases"
            )));
        }

        if let Some(parts) = parse_claude_mcp_alias(&normalized) {
            for entry in &self.aliases {
                if entry.parts.server == parts.server && entry.parts.semantic == parts.semantic {
                    matched_original = entry.original;
                    match_count += 1;
                }
            }
        }
        // Extra words in the tool component still parse, but the semantic field is then wrong.
        // Fall through to an unambiguous suffix match so word-level repeats do not become 500s.
        if match_count == 0 {
            let suffix_matches: Vec<&AliasEntry<'_>> = self
                .aliases
                .iter()
                .filter(|e| {
                    e.parts.server == server
                        && normalized.ends_with(&format!("_{}", e.parts.semantic))
                })
                .collect();
            if suffix_matches.len() == 1 {
                matched_original = suffix_matches[0].original;
                match_count = 1;
            } else if suffix_matches.len() > 1 {
                // Several candidates (e.g. "_file" and "_read_file"): take the strictly longest
                // semantic when unambiguous.
                let mut longest = suffix_matches[0];
                let mut tie = false;
                for candidate in &suffix_matches[1..] {
                    if candidate.parts.semantic.len() > longest.parts.semantic.len() {
                        longest = candidate;
                        tie = false;
                    } else if candidate.parts.semantic.len() == longest.parts.semantic.len() {
                        tie = true;
                    }
                }
                if !tie {
                    matched_original = longest.original;
                    match_count = 1;
                } else {
                    match_count = suffix_matches.len();
                }
            }
            if match_count == 1 {
                tracing::debug!(
                    "claude oauth mcp alias: recovered drifted tool name {name:?} as {matched_original:?} via semantic suffix"
                );
            }
        }
        if match_count == 1 {
            return Ok(Some(matched_original.to_string()));
        }
        if match_count > 1 {
            return Err(restore_error(format!(
                "cannot restore Claude OAuth MCP tool alias {name:?}: semantic suffix matches multiple declared tools"
            )));
        }

        if !self.passthroughs.is_empty() {
            // Recovery 1: the model prepended the virtual server to the full caller MCP tool name
            // ("mcp__<virtual>__<real_server>__<tool>").
            let reprefixed = format!("mcp__{suffix}");
            if let Some(original) = self.exact.get(&reprefixed)
                && *original == reprefixed
            {
                tracing::debug!(
                    "claude oauth mcp alias: recovered hybrid passthrough tool name {name:?} as {original:?} via exact prefix"
                );
                return Ok(Some(original.clone()));
            }

            // Recovery 2: the model replaced the caller's server with the virtual server
            // ("mcp__<virtual>__<tool>"). Runs strictly after client-tool recovery so a client
            // tool is never eclipsed by a passthrough tool with the same suffix.
            let mut matched_passthrough = "";
            let mut passthrough_matches = 0usize;
            for pt in &self.passthroughs {
                let tool_part = pt
                    .strip_prefix("mcp__")
                    .and_then(|rest| rest.split_once("__"))
                    .map_or(*pt, |(_, tool)| tool);
                if tool_part == suffix {
                    matched_passthrough = pt;
                    passthrough_matches += 1;
                }
            }
            if passthrough_matches == 1 {
                tracing::debug!(
                    "claude oauth mcp alias: recovered hybrid passthrough tool name {name:?} as {matched_passthrough:?} via unique tool suffix"
                );
                return Ok(Some(matched_passthrough.to_string()));
            }
            if passthrough_matches > 1 {
                return Err(restore_error(format!(
                    "cannot restore Claude OAuth MCP tool alias {name:?}: passthrough tool suffix matches multiple declared tools"
                )));
            }
        }

        tracing::warn!(
            "claude oauth mcp alias: cannot restore tool name {name:?}: no unique request-local match; forwarding it unchanged"
        );
        Ok(None)
    }
}

fn restore_error(message: String) -> ClaudeMcpAliasRestoreError {
    ClaudeMcpAliasRestoreError(message)
}

// ---------------------------------------------------------------- reverse remap

/// Go: `reverseRemapOAuthToolNames`. Restores aliases in a non-stream response's `content[]`
/// using the per-request map; names outside the request-local virtual server pass unchanged.
pub fn reverse_remap_oauth_tool_names(
    body: &[u8],
    reverse_map: &ReverseMap,
) -> Result<Vec<u8>, ClaudeMcpAliasRestoreError> {
    if reverse_map.is_empty() {
        return Ok(body.to_vec());
    }
    let Some(parts) = Raw::root(body)
        .and_then(|r| r.member("content"))
        .and_then(|c| c.elements())
    else {
        return Ok(body.to_vec());
    };
    let resolver = AliasResolver::new(reverse_map);
    let mut edits = Vec::new();
    let mut failure = None;
    for part in &parts {
        let finished = walk_part(
            part,
            false,
            &mut |node| match resolver.resolve(&node.text()) {
                Ok(Some(original)) => {
                    edits.push(RawEdit::replace(node, sjson_string(&original)));
                    true
                }
                Ok(None) => true,
                Err(e) => {
                    failure = Some(e);
                    false
                }
            },
        );
        if !finished {
            break;
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(apply_raw_edits(body, edits).unwrap_or_else(|| body.to_vec()))
}

/// Go: `reverseRemapOAuthToolNamesFromStreamLine`. Restores the alias in one SSE line's
/// `content_block` (tool_use, tool_reference or tool_search_tool_result); other lines pass
/// through. Edited lines are re-emitted as `data: <json>` when the input was a `data:` line.
pub fn reverse_remap_oauth_tool_names_from_stream_line(
    line: &[u8],
    reverse_map: &ReverseMap,
) -> Result<Vec<u8>, ClaudeMcpAliasRestoreError> {
    if reverse_map.is_empty() {
        return Ok(line.to_vec());
    }
    let Some(payload) = json_payload(line) else {
        return Ok(line.to_vec());
    };
    if !cpa_json::valid(payload) {
        return Ok(line.to_vec());
    }
    let Some(content_block) = Raw::root(payload).and_then(|r| r.member("content_block")) else {
        return Ok(line.to_vec());
    };

    let resolver = AliasResolver::new(reverse_map);
    let mut edits = Vec::new();
    match get_text(&content_block, "type").as_str() {
        "tool_use" | "tool_reference" => {
            let field = if get_text(&content_block, "type") == "tool_use" {
                "name"
            } else {
                "tool_name"
            };
            let Some(node) = content_block.member(field) else {
                return Ok(line.to_vec());
            };
            match resolver.resolve(&node.text())? {
                Some(original) => edits.push(RawEdit::replace(&node, sjson_string(&original))),
                None => return Ok(line.to_vec()),
            }
        }
        "tool_search_tool_result" => {
            let Some(refs) =
                get(&content_block, "content.tool_references").and_then(|c| c.elements())
            else {
                return Ok(line.to_vec());
            };
            for r in &refs {
                if get_text(r, "type") != "tool_reference" {
                    continue;
                }
                let Some(node) = r.member("tool_name") else {
                    continue;
                };
                if let Some(original) = resolver.resolve(&node.text())? {
                    edits.push(RawEdit::replace(&node, sjson_string(&original)));
                }
            }
            if edits.is_empty() {
                return Ok(line.to_vec());
            }
        }
        _ => return Ok(line.to_vec()),
    }

    let Some(updated) = apply_raw_edits(payload, edits) else {
        return Ok(line.to_vec());
    };
    Ok(with_data_prefix(line, updated))
}

/// Re-adds the `data: ` prefix when the original line had one (Go: tail of the stream-line
/// rewriters).
fn with_data_prefix(line: &[u8], updated: Vec<u8>) -> Vec<u8> {
    if trim_space(line).starts_with(b"data:") {
        let mut out = Vec::with_capacity(updated.len() + 6);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&updated);
        out
    } else {
        updated
    }
}

// ---------------------------------------------------------------- legacy prefix helpers

/// Go: `applyClaudeToolPrefix`. Legacy proxy-prefix scheme (not on the main flow): prefixes
/// client tool names in tools[], tool_choice and message references, leaving built-in tools and
/// MCP names alone.
pub fn apply_claude_tool_prefix(body: &[u8], prefix: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return body.to_vec();
    }
    let Some(root) = Raw::root(body) else {
        return body.to_vec();
    };
    // Authoritative fallback seed list plus typed built-ins present in this body.
    let mut builtin = augment_claude_builtin_tool_registry(body, None);
    let mut edits = Vec::new();
    let eligible = |name: &str, builtin: &HashSet<String>| {
        !name.is_empty()
            && !name.starts_with(prefix)
            && !builtin.contains(name)
            && !is_claude_mcp_tool_name(name)
    };
    let mut push = |node: &Raw<'_>, name: &str| {
        edits.push(RawEdit::replace(
            node,
            sjson_string(&format!("{prefix}{name}")),
        ));
    };

    if let Some(tools) = root.member("tools").and_then(|t| t.elements()) {
        for tool in &tools {
            let name = get_text(tool, "name");
            // Typed tools (web_search, code_execution, ...) must keep their name.
            if tool.member("type").is_some_and(|t| !t.text().is_empty()) {
                if !name.is_empty() {
                    builtin.insert(name);
                }
                continue;
            }
            if name.is_empty() || name.starts_with(prefix) || is_claude_mcp_tool_name(&name) {
                continue;
            }
            if let Some(node) = tool.member("name") {
                push(&node, &name);
            }
        }
    }

    if let Some(tool_choice) = root.member("tool_choice")
        && get_text(&tool_choice, "type") == "tool"
        && let Some(node) = tool_choice.member("name")
    {
        let name = node.text();
        if eligible(&name, &builtin) {
            push(&node, &name);
        }
    }

    if let Some(messages) = root.member("messages").and_then(|m| m.elements()) {
        for msg in &messages {
            let Some(parts) = msg.member("content").and_then(|c| c.elements()) else {
                continue;
            };
            for part in &parts {
                match get_text(part, "type").as_str() {
                    "tool_use" | "tool_reference" => {
                        let field = if get_text(part, "type") == "tool_use" {
                            "name"
                        } else {
                            "tool_name"
                        };
                        if let Some(node) = part.member(field) {
                            let name = node.text();
                            if eligible(&name, &builtin) {
                                push(&node, &name);
                            }
                        }
                    }
                    "tool_result" => {
                        let nested = part
                            .member("content")
                            .and_then(|c| c.elements())
                            .unwrap_or_default();
                        for np in &nested {
                            if get_text(np, "type") != "tool_reference" {
                                continue;
                            }
                            if let Some(node) = np.member("tool_name") {
                                let name = node.text();
                                if eligible(&name, &builtin) {
                                    push(&node, &name);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    apply_raw_edits(body, edits).unwrap_or_else(|| body.to_vec())
}

/// Go: `stripClaudeToolPrefixFromResponse`. Removes `prefix` from tool names in a non-stream
/// response's `content[]` (tool_use, tool_reference and nested tool_result references).
pub fn strip_claude_tool_prefix_from_response(body: &[u8], prefix: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return body.to_vec();
    }
    let Some(parts) = Raw::root(body)
        .and_then(|r| r.member("content"))
        .and_then(|c| c.elements())
    else {
        return body.to_vec();
    };
    let mut edits = Vec::new();
    let mut strip = |node: &Raw<'_>| {
        let name = node.text();
        if let Some(rest) = name.strip_prefix(prefix) {
            edits.push(RawEdit::replace(node, sjson_string(rest)));
        }
    };
    for part in &parts {
        match get_text(part, "type").as_str() {
            "tool_use" => {
                if let Some(node) = part.member("name") {
                    strip(&node);
                }
            }
            "tool_reference" => {
                if let Some(node) = part.member("tool_name") {
                    strip(&node);
                }
            }
            "tool_result" => {
                let nested = part
                    .member("content")
                    .and_then(|c| c.elements())
                    .unwrap_or_default();
                for np in &nested {
                    if get_text(np, "type") == "tool_reference"
                        && let Some(node) = np.member("tool_name")
                    {
                        strip(&node);
                    }
                }
            }
            _ => {}
        }
    }
    apply_raw_edits(body, edits).unwrap_or_else(|| body.to_vec())
}

/// Go: `stripClaudeToolPrefixFromStreamLine`. Removes `prefix` from a tool_use/tool_reference
/// `content_block` name in one SSE line.
pub fn strip_claude_tool_prefix_from_stream_line(line: &[u8], prefix: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return line.to_vec();
    }
    let Some(payload) = json_payload(line) else {
        return line.to_vec();
    };
    if !cpa_json::valid(payload) {
        return line.to_vec();
    }
    let Some(content_block) = Raw::root(payload).and_then(|r| r.member("content_block")) else {
        return line.to_vec();
    };
    let field = match get_text(&content_block, "type").as_str() {
        "tool_use" => "name",
        "tool_reference" => "tool_name",
        _ => return line.to_vec(),
    };
    let Some(node) = content_block.member(field) else {
        return line.to_vec();
    };
    let name = node.text();
    let Some(rest) = name.strip_prefix(prefix) else {
        return line.to_vec();
    };
    match apply_raw_edits(payload, vec![RawEdit::replace(&node, sjson_string(rest))]) {
        Some(updated) => with_data_prefix(line, updated),
        None => line.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::helps::mcp_alias::claude_mcp_tool_alias;
    use cpa_json::J;

    /// `(name, input, Go output body, Go reverse map)` captured from the Go reference
    /// (`remapOAuthToolNamesWithOptions`, secret "differential-caller").
    type GoCase = (
        &'static str,
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );

    const GO_CASES: &[GoCase] = &[
        (
            "d1",
            "{\"model\":\"claude-opus-5\",\"tools\":[{\"name\":\"search_web\",\"input_schema\":{\"type\":\"object\"}},{\"name\":\"Search_Web\",\"input_schema\":{\"type\":\"object\"}}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"search_web\"},\"messages\":[{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search_web\",\"input\":{}},{\"type\":\"tool_reference\",\"tool_name\":\"Search_Web\"},{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":[{\"type\":\"tool_reference\",\"tool_name\":\"search_web\"}]},{\"type\":\"tool_use\",\"id\":\"toolu_unknown\",\"name\":\"not_declared\",\"input\":{}}]}]}",
            "{\"model\":\"claude-opus-5\",\"tools\":[{\"name\":\"mcp__embark_cruel__wheat_search_web\",\"input_schema\":{\"type\":\"object\"}},{\"name\":\"mcp__embark_cruel__taste_Search_Web\",\"input_schema\":{\"type\":\"object\"}}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"mcp__embark_cruel__wheat_search_web\"},\"messages\":[{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"mcp__embark_cruel__wheat_search_web\",\"input\":{}},{\"type\":\"tool_reference\",\"tool_name\":\"mcp__embark_cruel__taste_Search_Web\"},{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":[{\"type\":\"tool_reference\",\"tool_name\":\"mcp__embark_cruel__wheat_search_web\"}]},{\"type\":\"tool_use\",\"id\":\"toolu_unknown\",\"name\":\"not_declared\",\"input\":{}}]}]}",
            &[
                ("mcp__embark_cruel__taste_Search_Web", "Search_Web"),
                ("mcp__embark_cruel__wheat_search_web", "search_web"),
            ],
        ),
        (
            "d3",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__always_fetch_url\"},{\"name\":\"fetch_url\"}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"fetch_url\"}}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__always_fetch_url\"},{\"name\":\"mcp__embark_cruel__amateur_fetch_url\"}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"mcp__embark_cruel__amateur_fetch_url\"}}",
            &[
                (
                    "mcp__embark_cruel__always_fetch_url",
                    "mcp__embark_cruel__always_fetch_url",
                ),
                ("mcp__embark_cruel__amateur_fetch_url", "fetch_url"),
            ],
        ),
        (
            "d4",
            "{\"messages\":[{\"content\":[{\"name\":\"读取_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_\",\"type\":\"tool_use\"},{\"tool_name\":\"read_file\",\"type\":\"tool_reference\"}]}],\"tools\":[{\"name\":\"读取_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_\"},{\"name\":\"read_file\"}]}",
            "{\"messages\":[{\"content\":[{\"name\":\"mcp__embark_cruel__doll_very_long_tool_name_very_long_tool_name\",\"type\":\"tool_use\"},{\"tool_name\":\"mcp__embark_cruel__short_read_file\",\"type\":\"tool_reference\"}]}],\"tools\":[{\"name\":\"mcp__embark_cruel__doll_very_long_tool_name_very_long_tool_name\"},{\"name\":\"mcp__embark_cruel__short_read_file\"}]}",
            &[
                (
                    "mcp__embark_cruel__doll_very_long_tool_name_very_long_tool_name",
                    "读取_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_very_long_tool_name_",
                ),
                ("mcp__embark_cruel__short_read_file", "read_file"),
            ],
        ),
        (
            "d5",
            "{\n  \"messages\" : [ { \"content\" : [ { \"name\" : \"fetch\\u005furl\", \"input\":{}, \"type\" : \"tool_use\" } ], \"role\" : \"assistant\" } ],\n  \"unknown\" : {\"number\":1.2300,\"escaped\":\"a\\/b\\n<>&\"},\n  \"tool_choice\" : { \"name\" : \"fetch\\u005furl\", \"type\" : \"tool\" },\n  \"tools\" : [ { \"description\" : \"keep \\\"bytes\\\"\", \"name\" : \"fetch\\u005furl\", \"input_schema\" : { \"type\" : \"object\" } } ]\n}",
            "{\n  \"messages\" : [ { \"content\" : [ { \"name\" : \"mcp__embark_cruel__always_fetch_url\", \"input\":{}, \"type\" : \"tool_use\" } ], \"role\" : \"assistant\" } ],\n  \"unknown\" : {\"number\":1.2300,\"escaped\":\"a\\/b\\n<>&\"},\n  \"tool_choice\" : { \"name\" : \"mcp__embark_cruel__always_fetch_url\", \"type\" : \"tool\" },\n  \"tools\" : [{ \"description\" : \"keep \\\"bytes\\\"\", \"name\" : \"mcp__embark_cruel__always_fetch_url\", \"input_schema\" : { \"type\" : \"object\" } }]\n}",
            &[("mcp__embark_cruel__always_fetch_url", "fetch_url")],
        ),
        (
            "d6",
            "{\"tools\":[{\"name\":42}],\"tool_choice\":{\"type\":\"tool\",\"name\":42},\"messages\":[{\"content\":[{\"type\":\"tool_reference\",\"tool_name\":42}]}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__allow_42\"}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"mcp__embark_cruel__allow_42\"},\"messages\":[{\"content\":[{\"type\":\"tool_reference\",\"tool_name\":\"mcp__embark_cruel__allow_42\"}]}]}",
            &[("mcp__embark_cruel__allow_42", "42")],
        ),
        (
            "d7",
            "{\"tools\":[{\"type\":\"web_search_20250305\",\"name\":\"web_search\"},{\"name\":\"mcp__server__existing\"}],\"messages\":[{\"content\":[{\"type\":\"tool_reference\",\"tool_name\":\"unknown\"}]}]}",
            "{\"tools\":[{\"type\":\"web_search_20250305\",\"name\":\"web_search\"},{\"name\":\"mcp__server__existing\"}],\"messages\":[{\"content\":[{\"type\":\"tool_reference\",\"tool_name\":\"unknown\"}]}]}",
            &[],
        ),
        (
            "d8",
            "{\"tools\":[{\"name\":\"read_file\",\"input_schema\":{\"type\":\"object\"}},{\"name\":\"lookup_notes\",\"input_schema\":{\"type\":\"object\"},\"defer_loading\":true},{\"type\":\"web_search_20250305\",\"name\":\"web_search\"}],\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}]},{\"role\":\"system\",\"content\":[{\"type\":\"text\",\"text\":\"tools changed\"},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"lookup_notes\"}},{\"type\":\"tool_removal\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"read_file\"}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"web_search\"}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_definition\",\"definition\":{\"name\":\"lookup_notes\",\"input_schema\":{\"type\":\"object\"}}}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_definition\",\"definition\":{\"name\":\"db_query\",\"input_schema\":{\"type\":\"object\"}}}},{\"type\":\"tool_removal\",\"tool\":{\"type\":\"mcp_tool_reference\",\"server_name\":\"docs\",\"name\":\"read_file\"}},{\"type\":\"tool_removal\",\"tool\":\"read_file\"}]}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__short_read_file\",\"input_schema\":{\"type\":\"object\"}},{\"name\":\"mcp__embark_cruel__major_lookup_notes\",\"input_schema\":{\"type\":\"object\"},\"defer_loading\":true},{\"type\":\"web_search_20250305\",\"name\":\"web_search\"}],\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"mcp__embark_cruel__short_read_file\",\"input\":{}}]},{\"role\":\"system\",\"content\":[{\"type\":\"text\",\"text\":\"tools changed\"},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"mcp__embark_cruel__major_lookup_notes\"}},{\"type\":\"tool_removal\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"mcp__embark_cruel__short_read_file\"}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_reference\",\"name\":\"web_search\"}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_definition\",\"definition\":{\"name\":\"mcp__embark_cruel__major_lookup_notes\",\"input_schema\":{\"type\":\"object\"}}}},{\"type\":\"tool_addition\",\"tool\":{\"type\":\"tool_definition\",\"definition\":{\"name\":\"db_query\",\"input_schema\":{\"type\":\"object\"}}}},{\"type\":\"tool_removal\",\"tool\":{\"type\":\"mcp_tool_reference\",\"server_name\":\"docs\",\"name\":\"read_file\"}},{\"type\":\"tool_removal\",\"tool\":\"read_file\"}]}]}",
            &[
                ("mcp__embark_cruel__major_lookup_notes", "lookup_notes"),
                ("mcp__embark_cruel__short_read_file", "read_file"),
            ],
        ),
        (
            "t1",
            "{\"tools\":[{\"type\":\"custom\",\"name\":\"a\",\"input_schema\":{}}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__tip_a\",\"input_schema\":{}}]}",
            &[("mcp__embark_cruel__tip_a", "a")],
        ),
        (
            "t3",
            "{\"tools\":[{\"name\":\"a\", \"type\" : \"custom\" , \"x\":1}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__tip_a\" , \"x\":1}]}",
            &[("mcp__embark_cruel__tip_a", "a")],
        ),
        (
            "t4",
            "{\"tools\":[{ \"type\":\"custom\" , \"name\":\"a\"}]}",
            "{\"tools\":[{ \"name\":\"mcp__embark_cruel__tip_a\"}]}",
            &[("mcp__embark_cruel__tip_a", "a")],
        ),
        (
            "t6",
            "{\n \"tools\": [\n  {\n   \"name\": \"a\",\n   \"type\": \"custom\",\n   \"d\": \"x\"\n  },\n  {\n   \"type\": \"foo\",\n   \"name\": \"b\"\n  }\n ]\n}",
            "{\n \"tools\": [{\n   \"name\": \"mcp__embark_cruel__tip_a\",\n   \"d\": \"x\"\n  },{\n   \"name\": \"mcp__embark_cruel__quote_b\"\n  }]\n}",
            &[
                ("mcp__embark_cruel__quote_b", "b"),
                ("mcp__embark_cruel__tip_a", "a"),
            ],
        ),
        (
            "t7",
            "{\"tools\":[{\"type\":\" \",\"name\":\"a\"},{\"type\":\"web_search_20250305\",\"name\":\"w\"},{\"type\":\"custom\",\"description\":\"e\\\"sc,\",\"name\":\"c\"}]}",
            "{\"tools\":[{\"type\":\" \",\"name\":\"mcp__embark_cruel__tip_a\"},{\"type\":\"web_search_20250305\",\"name\":\"w\"},{\"description\":\"e\\\"sc,\",\"name\":\"mcp__embark_cruel__cat_c\"}]}",
            &[
                ("mcp__embark_cruel__cat_c", "c"),
                ("mcp__embark_cruel__tip_a", "a"),
            ],
        ),
        (
            "t8",
            "{\"tools\":[{\"name\":\"x\",\"description\":\"a\\\\\\\"b\",\"type\":\"custom\"}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__ripple_x\",\"description\":\"a\\\\\\\"b\"}]}",
            &[("mcp__embark_cruel__ripple_x", "x")],
        ),
        (
            "m1",
            "{\"tools\":[{\"name\":\"search_web\"}],\"messages\":[",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__wheat_search_web\"}],\"messages\":[",
            &[("mcp__embark_cruel__wheat_search_web", "search_web")],
        ),
        (
            "m2",
            "{\"tools\":[{\"type\":\"custom\",\"name\":\"search_web\"}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"search_web\"},\"messages\":[{\"content\":[{\"type\":\"tool_use\",\"name\":\"search_web\"}]}",
            "{\"tools\":[{\"name\":\"mcp__embark_cruel__wheat_search_web\"}],\"tool_choice\":{\"type\":\"tool\",\"name\":\"mcp__embark_cruel__wheat_search_web\"},\"messages\":[{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__embark_cruel__wheat_search_web\"}]}",
            &[("mcp__embark_cruel__wheat_search_web", "search_web")],
        ),
    ];

    fn opts(secret: &str) -> ClaudeMcpAliasOptions {
        ClaudeMcpAliasOptions {
            secret: secret.to_string(),
        }
    }

    fn rmap(pairs: &[(&str, &str)]) -> ReverseMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// gjson `Get(...).String()` on bytes.
    fn at(body: &[u8], path: &str) -> String {
        cpa_json::parse(body).g(path).str()
    }

    /// JSON string literal for `s`.
    fn q(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    fn tool_id_of(alias: &str) -> String {
        let rest = alias.strip_prefix("mcp__").unwrap();
        rest.split_once("__")
            .unwrap()
            .1
            .split_once('_')
            .unwrap()
            .0
            .to_string()
    }

    fn server_of(alias: &str) -> String {
        parse_claude_mcp_alias(alias).unwrap().server.to_string()
    }

    fn semantic_of(alias: &str) -> String {
        parse_claude_mcp_alias(alias).unwrap().semantic.to_string()
    }

    /// Non-stream response with one tool_use named `name`.
    fn tool_use_resp(name: &str) -> Vec<u8> {
        format!(
            r#"{{"content":[{{"type":"tool_use","id":"toolu_1","name":{},"input":{{}}}}]}}"#,
            q(name)
        )
        .into_bytes()
    }

    /// SSE `content_block_start` line with one tool_use named `name`.
    fn tool_use_line(name: &str) -> Vec<u8> {
        format!(
            r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_1","name":{},"input":{{}}}}}}"#,
            q(name)
        )
        .into_bytes()
    }

    fn line_name(line: &[u8]) -> String {
        at(json_payload(line).unwrap(), "content_block.name")
    }

    /// Asserts a drifted `name` restores to `want` in both the non-stream and stream paths.
    fn assert_restores(map: &ReverseMap, name: &str, want: &str) {
        let restored = reverse_remap_oauth_tool_names(&tool_use_resp(name), map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), want, "non-stream {name}");
        let line =
            reverse_remap_oauth_tool_names_from_stream_line(&tool_use_line(name), map).unwrap();
        assert_eq!(line_name(&line), want, "stream {name}");
    }

    /// Asserts `name` is forwarded untouched in both paths.
    fn assert_fails_open(map: &ReverseMap, name: &str) {
        let resp = tool_use_resp(name);
        assert_eq!(reverse_remap_oauth_tool_names(&resp, map).unwrap(), resp);
        let line = tool_use_line(name);
        assert_eq!(
            reverse_remap_oauth_tool_names_from_stream_line(&line, map).unwrap(),
            line
        );
    }

    #[test]
    fn remap_matches_go_bytes() {
        for (name, input, want_body, want_map) in GO_CASES {
            let options = opts("differential-caller");
            let (body, map) = remap_oauth_tool_names_with_options(input.as_bytes(), &options);
            assert_eq!(String::from_utf8(body).unwrap(), *want_body, "{name}: body");
            assert_eq!(map, rmap(want_map), "{name}: reverse map");
            // Valid JSON takes the batched path; truncated JSON only the legacy fallback.
            let batched = remap_oauth_tool_names_with_batched_edits(input.as_bytes(), &options);
            assert_eq!(
                batched.is_some(),
                cpa_json::valid(input.as_bytes()),
                "{name}: batched acceptance"
            );
        }
    }

    #[test]
    fn remap_without_edits_keeps_body_and_reverse_map_empty() {
        let body = br#"{"tools":[{"name":"mcp__context7__query-docs"},{"type":"web_search_20250305","name":"web_search"}]}"#;
        let (out, map) = remap_oauth_tool_names_with_options(body, &opts("no-proxied-tools"));
        assert!(map.is_empty(), "{map:?}");
        assert_eq!(out, body);
    }

    #[test]
    fn raw_edits_reject_invalid_ranges() {
        let body = br#"{"a":"one","b":"two"}"#;
        let edit = |start, end| RawEdit {
            start,
            end,
            replacement: String::new(),
        };
        for edits in [
            vec![edit(5, 10), edit(8, 12)],
            vec![edit(5, 4)],
            vec![edit(5, body.len() + 1)],
        ] {
            assert!(apply_raw_edits(body, edits).is_none());
        }
    }

    #[test]
    fn resolve_options_uses_downstream_key() {
        assert_eq!(
            resolve_claude_mcp_alias_options("").secret,
            "cpa-claude-mcp-default-caller"
        );
        assert_eq!(
            resolve_claude_mcp_alias_options("  ").secret,
            "cpa-claude-mcp-default-caller"
        );
        let one = resolve_claude_mcp_alias_options(" downstream-caller-one ");
        assert_eq!(one.secret, "downstream-caller-one");
        assert_ne!(
            resolve_claude_mcp_alias_options("downstream-caller-two").secret,
            one.secret
        );
    }

    #[test]
    fn restore_error_is_request_scoped() {
        let err = ClaudeMcpAliasRestoreError("probe".into()).into_exec_error();
        assert_eq!(err.code, Some(ErrorCode::RequestScoped));
        assert_eq!(err.message, "probe");
    }

    #[test]
    fn oauth_token_detection() {
        assert!(is_claude_oauth_token("sk-ant-oat01-abc"));
        assert!(!is_claude_oauth_token("sk-ant-api03-abc"));
    }

    #[test]
    fn all_client_names_use_mcp_aliases() {
        for original in ["Bash", "bash", "Glob", "glob"] {
            let body = format!(
                r#"{{"tools":[{{"name":{},"description":"Run a client tool","input_schema":{{"type":"object"}}}}]}}"#,
                q(original)
            );
            let (out, map) = remap_oauth_tool_names(body.as_bytes());
            let alias = at(&out, "tools.0.name");
            assert!(is_claude_mcp_tool_name(&alias), "{alias}");
            assert_eq!(map.get(&alias).map(String::as_str), Some(original));
            let reversed = reverse_remap_oauth_tool_names(&tool_use_resp(&alias), &map).unwrap();
            assert_eq!(at(&reversed, "content.0.name"), original);
        }
    }

    #[test]
    fn all_client_tools_as_mcp_with_history_and_restore() {
        let body = br#"{
            "tools":[
                {"type":"web_search_20250305","name":"web_search","max_uses":2},
                {"name":"bash","description":"client shell tool","input_schema":{"type":"object"}},
                {"name":"Read","description":"client read tool","input_schema":{"type":"object"}},
                {"name":"mcp__context7__query-docs","description":"existing MCP tool","input_schema":{"type":"object"}},
                {"name":"search_web","description":"unknown one","input_schema":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}},
                {"name":"Search_Web","description":"case-distinct unknown","input_schema":{"type":"object"}},
                {"name":"search_web","description":"repeated declaration","input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"tool","name":"search_web"},
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_unknown","name":"search_web","input":{"q":"go"}},
                    {"type":"tool_reference","tool_name":"Search_Web"}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_unknown","content":[{"type":"tool_reference","tool_name":"search_web"}]}
                ]}
            ]
        }"#;
        let (out, map) = remap_oauth_tool_names_with_options(body, &opts("credential-secret"));
        assert_eq!(at(&out, "tools.0.name"), "web_search");
        let bash = at(&out, "tools.1.name");
        let read = at(&out, "tools.2.name");
        assert!(is_claude_mcp_tool_name(&bash) && is_claude_mcp_tool_name(&read));
        assert_eq!(at(&out, "tools.1.description"), "client shell tool");
        assert_eq!(at(&out, "tools.1.input_schema.type"), "object");
        assert_eq!(at(&out, "tools.3.name"), "mcp__context7__query-docs");
        let search = at(&out, "tools.4.name");
        let case = at(&out, "tools.5.name");
        assert!(is_claude_mcp_tool_name(&search) && is_claude_mcp_tool_name(&case));
        assert_ne!(search, case);
        assert_eq!(at(&out, "tools.6.name"), search);
        assert!(search.ends_with("_search_web") && case.ends_with("_Search_Web"));
        assert!(search.len() <= 64 && case.len() <= 64);
        assert_eq!(at(&out, "tools.4.description"), "unknown one");
        assert_eq!(at(&out, "tools.4.input_schema.required.0"), "q");
        assert_eq!(at(&out, "tool_choice.name"), search);
        assert_eq!(at(&out, "messages.0.content.0.name"), search);
        assert_eq!(at(&out, "messages.0.content.0.id"), "toolu_unknown");
        assert_eq!(at(&out, "messages.0.content.1.tool_name"), case);
        assert_eq!(at(&out, "messages.1.content.0.content.0.tool_name"), search);
        assert_eq!(map[&search], "search_web");
        assert_eq!(map[&case], "Search_Web");
        assert_eq!(map[&bash], "bash");
        assert_eq!(map[&read], "Read");

        let response = format!(
            r#"{{"content":[
                {{"type":"tool_use","id":"toolu_unknown","name":{s},"input":{{}}}},
                {{"type":"tool_reference","tool_name":{c}}},
                {{"type":"tool_result","tool_use_id":"toolu_unknown","content":[{{"type":"tool_reference","tool_name":{s}}}]}}
            ]}}"#,
            s = q(&search),
            c = q(&case)
        );
        let restored = reverse_remap_oauth_tool_names(response.as_bytes(), &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), "search_web");
        assert_eq!(at(&restored, "content.1.tool_name"), "Search_Web");
        assert_eq!(at(&restored, "content.2.content.0.tool_name"), "search_web");

        let line =
            reverse_remap_oauth_tool_names_from_stream_line(&tool_use_line(&search), &map).unwrap();
        assert_eq!(line_name(&line), "search_web");
    }

    #[test]
    fn typed_custom_tools_use_mcp_alias_and_lose_type() {
        let body = br#"{
            "tools":[
                {"type":"custom","name":"client_custom","description":"keep","input_schema":{"type":"object","properties":{"value":{"type":"string"}}}},
                {"type":"web_search_20250305","name":"web_search","max_uses":2},
                {"type":"client_extension_v1","name":"client_extension","description":"extension","input_schema":{"type":"object"}}
            ],
            "tool_choice":{"type":"tool","name":"client_custom"},
            "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_custom","name":"client_custom","input":{}}]}]
        }"#;
        let (out, map) = remap_oauth_tool_names_with_options(body, &opts("caller-secret"));
        let alias = at(&out, "tools.0.name");
        assert!(is_claude_mcp_tool_name(&alias));
        assert!(!cpa_json::parse(&out).g("tools.0.type").exists());
        assert_eq!(at(&out, "tools.0.description"), "keep");
        assert_eq!(at(&out, "tools.1.name"), "web_search");
        assert_eq!(at(&out, "tools.1.type"), "web_search_20250305");
        let ext = at(&out, "tools.2.name");
        assert!(is_claude_mcp_tool_name(&ext) && !cpa_json::parse(&out).g("tools.2.type").exists());
        assert_eq!(at(&out, "tool_choice.name"), alias);
        assert_eq!(at(&out, "messages.0.content.0.name"), alias);
        assert_eq!(map[&alias], "client_custom");
        assert_eq!(map[&ext], "client_extension");
    }

    #[test]
    fn alias_avoids_client_collision() {
        let secret = "credential-secret";
        let candidate = claude_mcp_tool_alias(secret, "fetch_url", 0);
        let body = format!(
            r#"{{"tools":[{{"name":{},"input_schema":{{"type":"object"}}}},{{"name":"fetch_url","input_schema":{{"type":"object"}}}}]}}"#,
            q(&candidate)
        );
        let (out, map) = remap_oauth_tool_names_with_options(body.as_bytes(), &opts(secret));
        assert_eq!(at(&out, "tools.0.name"), candidate);
        let alias = at(&out, "tools.1.name");
        assert_ne!(alias, candidate);
        assert_eq!(map[&alias], "fetch_url");
    }

    #[test]
    fn semantic_alias_restores_long_original_and_is_stable() {
        let original = "Read.file/with a very long semantic name and Unicode 网页内容 that exceeds the wire limit";
        let body = format!(
            r#"{{"tools":[{{"name":{},"input_schema":{{"type":"object"}}}}]}}"#,
            q(original)
        );
        let options = opts("stable-caller");
        let (out, map) = remap_oauth_tool_names_with_options(body.as_bytes(), &options);
        let alias = at(&out, "tools.0.name");
        assert!(
            is_claude_mcp_tool_name(&alias) && alias.len() <= 64,
            "{alias}"
        );
        assert!(alias.contains("_Read_file_with_a_very_long"), "{alias}");
        assert_eq!(map[&alias], original);
        let (second, _) = remap_oauth_tool_names_with_options(body.as_bytes(), &options);
        assert_eq!(at(&second, "tools.0.name"), alias);
        let restored = reverse_remap_oauth_tool_names(&tool_use_resp(&alias), &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), original);
    }

    #[test]
    fn prepare_preserves_mcp_convention_and_history() {
        let body = br#"{"tools":[
            {"name":"search_web","input_schema":{"type":"object"}},
            {"name":"mcp__context7__query-docs","input_schema":{"type":"object"}},
            {"name":"bash","input_schema":{"type":"object"}}
        ],"tool_choice":{"type":"tool","name":"search_web"}}"#;
        let (out, map) =
            prepare_claude_oauth_tool_names_for_upstream(body, &opts("credential-secret"));
        let alias = at(&out, "tools.0.name");
        assert!(is_claude_mcp_tool_name(&alias) && !alias.starts_with("proxy_"));
        assert_eq!(at(&out, "tools.1.name"), "mcp__context7__query-docs");
        let bash = at(&out, "tools.2.name");
        assert!(is_claude_mcp_tool_name(&bash));
        assert_eq!(at(&out, "tool_choice.name"), alias);
        assert_eq!(map[&alias], "search_web");
        assert_eq!(map[&bash], "bash");

        let body = br#"{"tools":[
            {"name":"Bash","input_schema":{"type":"object","properties":{"cmd":{"type":"string"}}}},
            {"name":"glob","input_schema":{"type":"object","properties":{"filePattern":{"type":"string"}}}}
        ],"messages":[{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_01","name":"Bash","input":{}},
            {"type":"tool_use","id":"toolu_02","name":"glob","input":{}}
        ]}]}"#;
        let (out, map) =
            prepare_claude_oauth_tool_names_for_upstream(body, &opts("mixed-case-caller"));
        let (bash, glob) = (at(&out, "tools.0.name"), at(&out, "tools.1.name"));
        assert!(is_claude_mcp_tool_name(&bash) && is_claude_mcp_tool_name(&glob) && bash != glob);
        assert_eq!(at(&out, "messages.0.content.0.name"), bash);
        assert_eq!(at(&out, "messages.0.content.1.name"), glob);
        assert_eq!(map[&bash], "Bash");
        assert_eq!(map[&glob], "glob");
    }

    #[test]
    fn mixed_case_names_remain_distinct() {
        let body = br#"{"tools":[{"name":"Bash","input_schema":{"type":"object"}},{"name":"bash","input_schema":{"type":"object"}}]}"#;
        let (out, map) = remap_oauth_tool_names(body);
        let (upper, lower) = (at(&out, "tools.0.name"), at(&out, "tools.1.name"));
        assert!(
            is_claude_mcp_tool_name(&upper) && is_claude_mcp_tool_name(&lower) && upper != lower
        );
        assert_eq!(map[&upper], "Bash");
        assert_eq!(map[&lower], "bash");
    }

    #[test]
    fn stream_line_honors_per_request_map() {
        let map = rmap(&[("Glob", "glob")]);
        let bash = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"Bash","input":{}}}"#;
        let out = reverse_remap_oauth_tool_names_from_stream_line(bash, &map).unwrap();
        assert_eq!(out, bash);
        let glob = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_02","name":"Glob","input":{}}}"#;
        let out = reverse_remap_oauth_tool_names_from_stream_line(glob, &map).unwrap();
        assert!(String::from_utf8(out).unwrap().contains(r#""name":"glob""#));
    }

    #[test]
    fn restore_recovers_mangled_aliases() {
        let body = br#"{"tools":[{"name":"glob","input_schema":{"type":"object"}},{"name":"read","input_schema":{"type":"object"}}]}"#;
        let (remapped, map) =
            remap_oauth_tool_names_with_options(body, &opts("mangled-alias-caller"));
        let glob = at(&remapped, "tools.0.name");
        let read = at(&remapped, "tools.1.name");
        let server = server_of(&glob);
        let repeated = format!("mcp__{server}__{glob}");
        let mixed = format!(
            "mcp__{server}__{}_{}",
            tool_id_of(&glob),
            semantic_of(&read)
        );
        let response = format!(
            r#"{{"content":[
                {{"type":"tool_use","id":"toolu_glob","name":{r},"input":{{}}}},
                {{"type":"tool_reference","tool_name":{m}}},
                {{"type":"tool_result","tool_use_id":"toolu_read","content":[{{"type":"tool_reference","tool_name":{m}}}]}}
            ]}}"#,
            r = q(&repeated),
            m = q(&mixed)
        );
        let restored = reverse_remap_oauth_tool_names(response.as_bytes(), &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), "glob");
        assert_eq!(at(&restored, "content.1.tool_name"), "read");
        assert_eq!(at(&restored, "content.2.content.0.tool_name"), "read");

        assert_restores(&map, &repeated, "glob");
        let line = format!(
            r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_reference","tool_name":{}}}}}"#,
            q(&mixed)
        );
        let out = reverse_remap_oauth_tool_names_from_stream_line(line.as_bytes(), &map).unwrap();
        assert_eq!(
            at(json_payload(&out).unwrap(), "content_block.tool_name"),
            "read"
        );
    }

    #[test]
    fn restore_recovers_repeated_server_and_malformed_tool_ids() {
        let alias = "mcp__hmzqrngkulqv__xuo7jlxlpzee_Bash";
        let map = rmap(&[
            (alias, "Bash"),
            ("mcp__hmzqrngkulqv__aaaaaaaaaaaa_Bash", "OtherBash"),
        ]);
        for drifted in [
            "mcp__hmzqrngkulqv__hmzqrngkulqv__xuo7jlxlpzee_Bash",
            "mcp__hmzqrngkulqv__hmzqrngkulqv__hmzqrngkulqv__xuo7jlxlpzee_Bash",
        ] {
            assert_restores(&map, drifted, "Bash");
        }
        let map = rmap(&[(alias, "Bash")]);
        for drifted in [
            "mcp__hmzqrngkulqv__xuo7jlxlpze_Bash",
            "mcp__hmzqrngkulqv__xuo7jlxlpzeea_Bash",
            "mcp__hmzqrngkulqv__xuo7jlxlpze0_Bash",
            "mcp__hmzqrngkulqv__auo7jlxlpzee_Bash",
            "mcp__hmzqrngkulqv__hmzqrngkulqv__xuo7jlxlpze_Bash",
        ] {
            assert_restores(&map, drifted, "Bash");
        }
    }

    #[test]
    fn restore_with_bip39_aliases() {
        let body = br#"{"tools":[{"name":"Bash","input_schema":{"type":"object"}},{"name":"fetch_url","input_schema":{"type":"object"}}]}"#;
        let (remapped, map) = remap_oauth_tool_names_with_options(body, &opts("bip39-caller"));
        let bash = at(&remapped, "tools.0.name");
        assert_eq!(semantic_of(&bash), "Bash");
        let (server, tool_id) = (server_of(&bash), tool_id_of(&bash));
        let names = [
            bash.clone(),
            format!("mcp__{server}__{server}__{tool_id}_Bash"),
            format!("mcp__{server}__corruptedword_Bash"),
            format!("mcp__{server}__{tool_id}_{tool_id}_Bash"),
            format!("mcp__{server}__{tool_id}_cabin_Bash"),
        ];
        for name in &names {
            assert_restores(&map, name, "Bash");
        }
    }

    #[test]
    fn restore_rejects_ambiguous_aliases_and_fails_open_otherwise() {
        let body = br#"{"tools":[{"name":"tool.name"},{"name":"tool/name"}]}"#;
        let (remapped, map) =
            remap_oauth_tool_names_with_options(body, &opts("ambiguous-alias-caller"));
        let first = at(&remapped, "tools.0.name");
        let second = at(&remapped, "tools.1.name");
        assert_eq!(semantic_of(&first), semantic_of(&second));
        let mut unknown = "aaaaaaaaaaaa".to_string();
        if unknown == tool_id_of(&first) || unknown == tool_id_of(&second) {
            unknown = "bbbbbbbbbbbb".into();
        }
        let server = server_of(&first);
        let semantic = semantic_of(&first);
        let want = "semantic suffix matches multiple declared tools";
        for alias in [
            format!("mcp__{server}__{unknown}_{semantic}"),
            format!(
                "mcp__{server}__{}_{semantic}",
                &unknown[..unknown.len() - 1]
            ),
        ] {
            let err = reverse_remap_oauth_tool_names(&tool_use_resp(&alias), &map).unwrap_err();
            assert!(err.to_string().contains(want), "{err}");
            assert_eq!(err.into_exec_error().code, Some(ErrorCode::RequestScoped));
            let err = reverse_remap_oauth_tool_names_from_stream_line(&tool_use_line(&alias), &map)
                .unwrap_err();
            assert!(err.to_string().contains(want), "{err}");
        }
        assert_fails_open(&map, &format!("mcp__{server}__{unknown}_missing_tool"));
    }

    #[test]
    fn restore_overlapping_semantic_suffix_prefers_longest() {
        let body = br#"{"tools":[{"name":"file"},{"name":"read_file"}]}"#;
        let (remapped, map) =
            remap_oauth_tool_names_with_options(body, &opts("overlapping-caller"));
        let file = at(&remapped, "tools.0.name");
        let read_file = at(&remapped, "tools.1.name");
        let (server, tool_id) = (server_of(&read_file), tool_id_of(&read_file));
        let drifted = format!("mcp__{server}__{tool_id}_{tool_id}_read_file");
        let restored = reverse_remap_oauth_tool_names(&tool_use_resp(&drifted), &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), "read_file");
        let restored = reverse_remap_oauth_tool_names(&tool_use_resp(&file), &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), "file");
    }

    #[test]
    fn restore_preserves_unrelated_and_malformed_names() {
        let (_, map) = remap_oauth_tool_names_with_options(
            br#"{"tools":[{"name":"glob"}]}"#,
            &opts("unrelated-mcp-caller"),
        );
        assert_fails_open(&map, "mcp__external__query");

        let alias = "mcp__hmzqrngkulqv__xuo7jlxlpzee_clear_thinking";
        let map = rmap(&[(alias, "clear_thinking")]);
        assert_fails_open(
            &map,
            &format!("{alias}</parameter>\n<parameter name=\"merge\""),
        );
    }

    #[test]
    fn restore_undeclared_tool_under_virtual_server_fails_open() {
        let body = br#"{"tools":[{"name":"terminal","input_schema":{"type":"object"}}]}"#;
        let (remapped, map) =
            remap_oauth_tool_names_with_options(body, &opts("undeclared-tool-caller"));
        let server = server_of(&at(&remapped, "tools.0.name"));
        assert_fails_open(&map, &format!("mcp__{server}__tiny_terminap"));
    }

    #[test]
    fn caller_mcp_tools_on_virtual_server_collision_pass_through() {
        let secret = "virtual-server-collision";
        let server = server_of(&claude_mcp_tool_alias(secret, "probe", 0));
        let native = [
            format!("mcp__{server}__read_file"),
            format!("mcp__{server}__grep_read_file"),
            format!("mcp__{server}__write_file"),
        ];
        let body = format!(
            r#"{{"tools":[{{"name":"read_file","input_schema":{{"type":"object"}}}},{{"name":{}}},{{"name":{}}},{{"name":{}}}]}}"#,
            q(&native[0]),
            q(&native[1]),
            q(&native[2])
        );
        let (upstream, map) = remap_oauth_tool_names_with_options(body.as_bytes(), &opts(secret));
        let alias = map
            .iter()
            .find(|(k, v)| v.as_str() == "read_file" && *k != *v)
            .map(|(k, _)| k.clone())
            .unwrap();
        for (i, name) in native.iter().enumerate() {
            assert_eq!(&at(&upstream, &format!("tools.{}.name", i + 1)), name);
            let resp = tool_use_resp(name);
            let restored = restore_claude_oauth_tool_names_from_response(&resp, &map).unwrap();
            assert_eq!(restored, resp);
            assert_fails_open(&map, name);
        }
        let tool_part = alias.splitn(3, "__").nth(2).unwrap().to_string();
        for drifted in [
            alias.clone(),
            format!("mcp__{server}__{server}__{tool_part}"),
            format!("mcp__{server}__abandon_read_file"),
        ] {
            let restored =
                restore_claude_oauth_tool_names_from_response(&tool_use_resp(&drifted), &map)
                    .unwrap();
            assert_eq!(at(&restored, "content.0.name"), "read_file", "{drifted}");
        }
    }

    #[test]
    fn restore_hybrid_passthrough_tools() {
        let virtual_server = "mcp__ripple_middle__";
        let map = rmap(&[
            (format!("{virtual_server}blanket_Bash").as_str(), "Bash"),
            (format!("{virtual_server}brand_Read").as_str(), "Read"),
            (
                "mcp__acme__link_pull_request",
                "mcp__acme__link_pull_request",
            ),
            ("mcp__acme__list_threads", "mcp__acme__list_threads"),
        ]);
        for hybrid in [
            "mcp__ripple_middle__link_pull_request",
            "mcp__ripple_middle__acme__link_pull_request",
        ] {
            let restored =
                restore_claude_oauth_tool_names_from_response(&tool_use_resp(hybrid), &map)
                    .unwrap();
            assert_eq!(
                at(&restored, "content.0.name"),
                "mcp__acme__link_pull_request"
            );
            let line =
                restore_claude_oauth_tool_names_from_stream_line(&tool_use_line(hybrid), &map)
                    .unwrap();
            assert_eq!(line_name(&line), "mcp__acme__link_pull_request");
        }

        // A client tool wins over a passthrough tool with the same suffix.
        let map = rmap(&[
            (format!("{virtual_server}blanket_Bash").as_str(), "Bash"),
            ("mcp__shell__Bash", "mcp__shell__Bash"),
        ]);
        let resp = tool_use_resp(&format!("{virtual_server}Bash"));
        let restored = restore_claude_oauth_tool_names_from_response(&resp, &map).unwrap();
        assert_eq!(at(&restored, "content.0.name"), "Bash");

        // Two passthrough tools with the same suffix cannot be disambiguated.
        let map = rmap(&[
            (format!("{virtual_server}blanket_other").as_str(), "other"),
            ("mcp__srv1__query", "mcp__srv1__query"),
            ("mcp__srv2__query", "mcp__srv2__query"),
        ]);
        let resp = tool_use_resp(&format!("{virtual_server}query"));
        assert!(restore_claude_oauth_tool_names_from_response(&resp, &map).is_err());
    }

    #[test]
    fn restore_tool_search_results() {
        let body = br#"{"tools":[{"name":"task_list","input_schema":{"type":"object"}},{"name":"fetch_url","input_schema":{"type":"object"}}]}"#;
        let (remapped, map) =
            remap_oauth_tool_names_with_options(body, &opts("tool-search-caller"));
        let task = at(&remapped, "tools.0.name");
        let fetch = at(&remapped, "tools.1.name");
        assert!(task != "task_list" && fetch != "fetch_url");
        let resp = format!(
            r#"{{"id":"msg_01","type":"message","role":"assistant","content":[
                {{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_01","content":{{"type":"tool_search_tool_search_result","tool_references":[
                    {{"type":"tool_reference","tool_name":{t}}},{{"type":"tool_reference","tool_name":{f}}}]}}}},
                {{"type":"tool_use","id":"toolu_01","name":{t},"input":{{"action":"list"}}}}
            ]}}"#,
            t = q(&task),
            f = q(&fetch)
        );
        let restored = reverse_remap_oauth_tool_names(resp.as_bytes(), &map).unwrap();
        assert_eq!(
            at(&restored, "content.0.content.tool_references.0.tool_name"),
            "task_list"
        );
        assert_eq!(
            at(&restored, "content.0.content.tool_references.1.tool_name"),
            "fetch_url"
        );
        assert_eq!(at(&restored, "content.1.name"), "task_list");

        // The error variant has no tool_references and passes through byte for byte.
        let error_resp = br#"{"id":"msg_02","content":[{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_02","content":{"type":"tool_search_tool_result_error","error_code":"regex_compilation_failed"}}]}"#;
        assert_eq!(
            reverse_remap_oauth_tool_names(error_resp, &map).unwrap(),
            error_resp
        );

        let line = format!(
            r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_01","content":{{"type":"tool_search_tool_search_result","tool_references":[{{"type":"tool_reference","tool_name":{}}}]}}}}}}"#,
            q(&task)
        );
        let out = reverse_remap_oauth_tool_names_from_stream_line(line.as_bytes(), &map).unwrap();
        assert!(out.starts_with(b"data: "));
        assert_eq!(
            at(
                json_payload(&out).unwrap(),
                "content_block.content.tool_references.0.tool_name"
            ),
            "task_list"
        );
    }

    #[test]
    fn remap_tool_search_result_in_history_keeps_server_tools() {
        let body = br#"{
            "tools": [
                {"name": "task_list", "input_schema": {"type": "object"}},
                {"type": "advisor_20260301", "name": "advisor", "model": "claude-haiku-4-5-20251001"},
                {"type": "agent_toolset_20260401"}
            ],
            "messages": [{"role": "assistant", "content": [{
                "type": "tool_search_tool_result", "tool_use_id": "srvtoolu_01",
                "content": {"type": "tool_search_tool_search_result",
                    "tool_references": [{"type": "tool_reference", "tool_name": "task_list"}]}
            }]}]
        }"#;
        let (out, map) = remap_oauth_tool_names_with_options(body, &opts("history-caller"));
        let alias = map
            .iter()
            .find(|(k, v)| *v == "task_list" && *k != *v)
            .map(|(k, _)| k.clone())
            .unwrap();
        assert_eq!(at(&out, "tools.1.type"), "advisor_20260301");
        assert_eq!(at(&out, "tools.1.name"), "advisor");
        assert_eq!(at(&out, "tools.2.type"), "agent_toolset_20260401");
        assert_eq!(
            at(
                &out,
                "messages.0.content.0.content.tool_references.0.tool_name"
            ),
            alias
        );
    }

    #[test]
    fn remap_mid_conversation_tool_changes() {
        const TOOLS: &str = r#"[{"name":"read_file","description":"Read a file","input_schema":{"type":"object","properties":{}}},{"name":"lookup_notes","description":"Look up notes","input_schema":{"type":"object","properties":{}},"defer_loading":true},{"type":"web_search_20250305","name":"web_search"},{"name":"mcp__context7__query-docs","input_schema":{"type":"object"}}]"#;
        // (messages, [(path, original, aliased)])
        type Check = (&'static str, &'static str, bool);
        let cases: &[(&str, &[Check])] = &[
            (
                r#"[{"role":"user","content":"Reply with exactly: ok"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"lookup_notes"}}]}]"#,
                &[("messages.1.content.0.tool.name", "lookup_notes", true)],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_removal","tool":{"type":"tool_reference","name":"read_file"}}]}]"#,
                &[("messages.1.content.0.tool.name", "read_file", true)],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]},{"role":"system","content":[{"type":"text","text":"Tools changed."},{"type":"tool_removal","tool":{"type":"tool_reference","name":"read_file"}},{"type":"tool_addition","tool":{"type":"tool_reference","name":"lookup_notes"}}]}]"#,
                &[
                    ("messages.1.content.0.name", "read_file", true),
                    ("messages.3.content.0.text", "Tools changed.", false),
                    ("messages.3.content.1.tool.name", "read_file", true),
                    ("messages.3.content.2.tool.name", "lookup_notes", true),
                ],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"Bash"}},{"type":"tool_addition","tool":{"type":"tool_reference","name":"web_search"}},{"type":"tool_removal","tool":{"type":"tool_reference","name":"mcp__context7__query-docs"}}]}]"#,
                &[
                    ("messages.1.content.0.tool.name", "Bash", false),
                    ("messages.1.content.1.tool.name", "web_search", false),
                    (
                        "messages.1.content.2.tool.name",
                        "mcp__context7__query-docs",
                        false,
                    ),
                ],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"mcp_tool_reference","server_name":"docs","name":"read_file"}},{"type":"tool_removal","tool":{"type":"mcp_toolset_reference","server_name":"read_file"}}]}]"#,
                &[
                    ("messages.1.content.0.tool.name", "read_file", false),
                    ("messages.1.content.1.tool.server_name", "read_file", false),
                ],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"lookup_notes","description":"v2","input_schema":{"type":"object"}}}}]}]"#,
                &[(
                    "messages.1.content.0.tool.definition.name",
                    "lookup_notes",
                    true,
                )],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"db_query","input_schema":{"type":"object"}}}},{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"type":"web_search_20260209","name":"read_file"}}},{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"type":"mcp_toolset","mcp_server_name":"calendar"}}}]}]"#,
                &[
                    (
                        "messages.1.content.0.tool.definition.name",
                        "db_query",
                        false,
                    ),
                    (
                        "messages.1.content.1.tool.definition.name",
                        "read_file",
                        false,
                    ),
                    (
                        "messages.1.content.2.tool.definition.mcp_server_name",
                        "calendar",
                        false,
                    ),
                ],
            ),
            (
                r#"[{"role":"user","content":"hi"},{"role":"system","content":[{"type":"tool_addition"},{"type":"tool_addition","tool":"lookup_notes"},{"type":"tool_addition","tool":{"name":"lookup_notes"}},{"type":"tool_addition","tool":{"type":"tool_reference","tool_name":"lookup_notes"}},{"type":"tool_removal","tool":{"type":"tool_definition","definition":{"name":"lookup_notes"}}},{"type":"tool_addition","tool":{"type":"tool_reference","name":""}}]}]"#,
                &[
                    ("messages.1.content.1.tool", "lookup_notes", false),
                    ("messages.1.content.2.tool.name", "lookup_notes", false),
                    ("messages.1.content.3.tool.tool_name", "lookup_notes", false),
                    (
                        "messages.1.content.4.tool.definition.name",
                        "lookup_notes",
                        false,
                    ),
                    ("messages.1.content.5.tool.name", "", false),
                ],
            ),
        ];
        let declared_originals = [
            "read_file",
            "lookup_notes",
            "web_search",
            "mcp__context7__query-docs",
        ];
        for (messages, checks) in cases {
            let body = format!(
                r#"{{"model":"claude-opus-5-5","max_tokens":64,"tools":{TOOLS},"messages":{messages}}}"#
            );
            let (out, map) =
                remap_oauth_tool_names_with_options(body.as_bytes(), &opts("tool-change-caller"));
            let declared: HashMap<&str, String> = declared_originals
                .iter()
                .enumerate()
                .map(|(i, o)| (*o, at(&out, &format!("tools.{i}.name"))))
                .collect();
            for (path, original, aliased) in *checks {
                let want = if *aliased {
                    let alias = &declared[original];
                    assert!(
                        alias != original && is_claude_mcp_tool_name(alias),
                        "{original} -> {alias}"
                    );
                    assert_eq!(map.get(alias.as_str()).map(String::as_str), Some(*original));
                    alias.clone()
                } else {
                    original.to_string()
                };
                assert_eq!(at(&out, path), want, "{path}");
            }
            assert_eq!(at(&out, "tools.2.name"), "web_search");
            assert_eq!(at(&out, "tools.3.name"), "mcp__context7__query-docs");
        }
    }

    #[test]
    fn restore_matches_go_string_escaping() {
        // sjson stringify: plain ASCII is quoted verbatim, anything else goes through Go's
        // json.Marshal (HTML escapes on). Expected values are Go outputs.
        let cases = [
            (
                "mcp__aa_bb__cc_x",
                "we<ird>&\"é\n\\",
                r#""we\u003cird\u003e\u0026\"é\n\\""#,
            ),
            ("mcp__aa_bb__dd_y", "a<b>&", r#""a<b>&""#),
            (
                "mcp__aa_bb__ee_z",
                "tab\there\u{2028}end\u{7f}",
                "\"tab\\there\\u2028end\u{7f}\"",
            ),
            (
                "mcp__aa_bb__ff_w",
                "ctl\u{1}\u{8}\u{c}",
                r#""ctl\u0001\b\f""#,
            ),
        ];
        for (alias, original, want) in cases {
            let map = rmap(&[(alias, original)]);
            let resp = format!(
                r#"{{ "content" : [ {{"type":"tool_use","id":"t","name":{},"input":{{"a":1.50}}}}, {{"type":"tool_reference","tool_name":"other"}} ] }}"#,
                q(alias)
            );
            let want_resp = format!(
                r#"{{ "content" : [ {{"type":"tool_use","id":"t","name":{want},"input":{{"a":1.50}}}}, {{"type":"tool_reference","tool_name":"other"}} ] }}"#
            );
            let out = reverse_remap_oauth_tool_names(resp.as_bytes(), &map).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), want_resp);

            let line = format!(
                r#"  data:   {{"type":"content_block_start","content_block":{{"type":"tool_use","name":{}}}}}  "#,
                q(alias)
            );
            let want_line = format!(
                r#"data: {{"type":"content_block_start","content_block":{{"type":"tool_use","name":{want}}}}}"#
            );
            let out =
                reverse_remap_oauth_tool_names_from_stream_line(line.as_bytes(), &map).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), want_line);

            // Without a `data:` prefix the bare payload comes back.
            let bare = format!(
                r#"{{"type":"content_block_start","content_block":{{"type":"tool_use","name":{}}}}}"#,
                q(alias)
            );
            let want_bare = format!(
                r#"{{"type":"content_block_start","content_block":{{"type":"tool_use","name":{want}}}}}"#
            );
            let out =
                reverse_remap_oauth_tool_names_from_stream_line(bare.as_bytes(), &map).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), want_bare);
        }
    }

    #[test]
    fn legacy_prefix_helpers_match_go() {
        let input = br#"{"tools":[{"name":"alpha"},{"name":"proxy_bravo"}],"tool_choice":{"type":"tool","name":"charlie"},"messages":[{"role":"assistant","content":[{"type":"tool_use","name":"delta","id":"t1","input":{}}]}]}"#;
        let out = apply_claude_tool_prefix(input, "proxy_");
        assert_eq!(at(&out, "tools.0.name"), "proxy_alpha");
        assert_eq!(at(&out, "tools.1.name"), "proxy_bravo");
        assert_eq!(at(&out, "tool_choice.name"), "proxy_charlie");
        assert_eq!(at(&out, "messages.0.content.0.name"), "proxy_delta");

        // Typed built-ins, the default seed names and MCP names keep their name; byte layout is
        // preserved (Go output).
        let input = br#"{ "tools" : [ {"name":"a"}, {"type":"web_search_20250305","name":"w"} ], "tool_choice":{"type":"tool","name":"a"}, "messages":[{"content":[{"type":"tool_use","name":"a"},{"type":"tool_reference","tool_name":"w"}]}] }"#;
        let want = r#"{ "tools" : [ {"name":"proxy_a"}, {"type":"web_search_20250305","name":"w"} ], "tool_choice":{"type":"tool","name":"proxy_a"}, "messages":[{"content":[{"type":"tool_use","name":"proxy_a"},{"type":"tool_reference","tool_name":"w"}]}] }"#;
        assert_eq!(
            String::from_utf8(apply_claude_tool_prefix(input, "proxy_")).unwrap(),
            want
        );

        for builtin in ["web_search", "code_execution", "text_editor", "computer"] {
            let input = format!(
                r#"{{"tools":[{{"name":"Read"}}],"tool_choice":{{"type":"tool","name":"{b}"}},"messages":[{{"role":"assistant","content":[{{"type":"tool_use","name":"{b}","id":"toolu_1","input":{{}}}},{{"type":"tool_reference","tool_name":"{b}"}},{{"type":"tool_result","tool_use_id":"toolu_1","content":[{{"type":"tool_reference","tool_name":"{b}"}}]}}]}}]}}"#,
                b = builtin
            );
            let out = apply_claude_tool_prefix(input.as_bytes(), "proxy_");
            assert_eq!(at(&out, "tool_choice.name"), builtin);
            assert_eq!(at(&out, "messages.0.content.0.name"), builtin);
            assert_eq!(at(&out, "messages.0.content.1.tool_name"), builtin);
            assert_eq!(
                at(&out, "messages.0.content.2.content.0.tool_name"),
                builtin
            );
            assert_eq!(at(&out, "tools.0.name"), "proxy_Read");
        }

        let nested = br#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":[{"type":"tool_reference","tool_name":"mcp__nia__manage_resource"}]}]}]}"#;
        let out = apply_claude_tool_prefix(nested, "proxy_");
        assert_eq!(
            at(&out, "messages.0.content.0.content.0.tool_name"),
            "mcp__nia__manage_resource"
        );
        let string_content = br#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_123","content":"plain string result"}]}]}"#;
        assert_eq!(
            apply_claude_tool_prefix(string_content, "proxy_"),
            string_content
        );

        let resp = br#"{ "content" : [ {"type":"tool_use","name":"proxy_a"}, {"type":"tool_result","content":[{"type":"tool_reference","tool_name":"proxy_b"}]} ] }"#;
        let want = r#"{ "content" : [ {"type":"tool_use","name":"a"}, {"type":"tool_result","content":[{"type":"tool_reference","tool_name":"b"}]} ] }"#;
        assert_eq!(
            String::from_utf8(strip_claude_tool_prefix_from_response(resp, "proxy_")).unwrap(),
            want
        );
        let resp = br#"{"content":[{"type":"tool_reference","tool_name":"proxy_alpha"},{"type":"tool_reference","tool_name":"bravo"}]}"#;
        let out = strip_claude_tool_prefix_from_response(resp, "proxy_");
        assert_eq!(at(&out, "content.0.tool_name"), "alpha");
        assert_eq!(at(&out, "content.1.tool_name"), "bravo");

        let line = br#"data: {"type":"content_block_start","content_block":{"type":"tool_use","name":"proxy_alpha","id":"t1"},"index":0}"#;
        let out = strip_claude_tool_prefix_from_stream_line(line, "proxy_");
        assert_eq!(
            at(json_payload(&out).unwrap(), "content_block.name"),
            "alpha"
        );
        let line = br#"data: {"type":"content_block_start","content_block":{"type":"tool_reference","tool_name":"proxy_beta"},"index":0}"#;
        let out = strip_claude_tool_prefix_from_stream_line(line, "proxy_");
        assert_eq!(
            at(json_payload(&out).unwrap(), "content_block.tool_name"),
            "beta"
        );
    }
}
