//! Explicit session identity extraction (Go: sdk/cliproxy/session/info.go).
//!
//! Resolves which client session a request belongs to from headers, the request body and
//! execution metadata, in the documented priority order. The LCP prefix matcher is not ported.

use cpa_json::{J, Res, Value};
use http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::executor::{Metadata, meta};

/// Request session description used for affinity and upstream reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub parent_session_id: String,
    pub agent_name: String,
    pub client_type: String,
    pub caller_scope: String,
    pub node_kind: String,
    pub is_fork: bool,
    pub is_compaction: bool,
    pub is_subagent: bool,
}

/// Validates an explicit client-provided identifier: printable, trimmed, at most 256 bytes.
pub fn normalize_explicit_id(raw: &str) -> String {
    if raw.chars().any(char::is_control) {
        return String::new();
    }
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 256 {
        return String::new();
    }
    raw.to_string()
}

/// Bounds an identifier to <= 256 bytes, keeping uniqueness via SHA-256 and never splitting a
/// UTF-8 character: `<190 byte prefix>#<64 hex>`.
pub fn bound_session_identity(id: &str) -> String {
    if id.len() <= 256 {
        return id.to_string();
    }
    let hash_hex = hex::encode(Sha256::digest(id.as_bytes()));
    let prefix_len = (255 - 1 - hash_hex.len()).min(id.len());
    let mut end = prefix_len;
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{hash_hex}", &id[..end])
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    for value in headers.get_all(name) {
        if let Ok(v) = value.to_str() {
            let n = normalize_explicit_id(v);
            if !n.is_empty() {
                return n;
            }
        }
    }
    String::new()
}

/// First non-empty header value among several names.
fn header_any(headers: &HeaderMap, names: &[&str]) -> String {
    names
        .iter()
        .map(|n| header_value(headers, n))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn cand(r: Res<'_>) -> String {
    normalize_explicit_id(&r.str())
}

/// Request body roots: the top-level object and the nested `request` object (Gemini CLI style).
struct Roots {
    root: Value,
    exists: bool,
    nested: Option<Value>,
}

impl Roots {
    fn new(payload: &[u8]) -> Self {
        if payload.is_empty() {
            return Roots { root: Value::Null, exists: false, nested: None };
        }
        let root = cpa_json::parse(payload);
        let exists = !root.is_null();
        let req = root.g("request");
        let nested = if req.exists() && !root.g("contents").exists() { req.into_value() } else { None };
        Roots { root, exists, nested }
    }

    /// Candidate id at `path` in the root, falling back to the nested request object.
    fn pick(&self, path: &str) -> String {
        let v = cand(self.root.g(path));
        if !v.is_empty() {
            return v;
        }
        match &self.nested {
            Some(n) => cand(n.g(path)),
            None => String::new(),
        }
    }

    fn pick_first(&self, paths: &[&str]) -> String {
        paths.iter().map(|p| self.pick(p)).find(|v| !v.is_empty()).unwrap_or_default()
    }

    /// Root-only lookup (no nested fallback), as the Go code does for a few keys.
    fn root_only(&self, path: &str) -> String {
        cand(self.root.g(path))
    }
}

const PARENT_PATHS: &[&str] = &[
    "parent_session_id", "parentSessionId", "parentSessionID",
    "parent_thread_id", "parentThreadId", "parentThreadID",
    "forked_from_thread_id", "forked_from_id",
    "parent_conversation_id", "parentConversationId", "parentConversationID",
    "parent_id", "parentId", "parentID",
    "parent_task_id", "parentTaskId", "parentTaskID",
    "parent_action_id", "parentActionId", "parentActionID",
    "parent_session", "parentSession",
    "parent_subagent_id", "parentSubagentId",
    "forkSource.sessionId", "fork_source.session_id",
    "previousSessionId", "previous_session_id",
    "metadata.parent_session_id", "metadata.parentSessionId", "metadata.parentSessionID",
    "metadata.parent_thread_id", "metadata.parentThreadId",
    "metadata.forked_from_thread_id", "metadata.forked_from_id",
    "metadata.parent_id", "metadata.parentId", "metadata.parentID",
    "metadata.parent_task_id", "metadata.parentTaskId", "metadata.parentTaskID",
    "metadata.parent_action_id", "metadata.parentActionId",
    "metadata.parent_subagent_id", "metadata.parentSubagentId",
    "metadata.parent_session", "metadata.parentSession",
    "metadata.parent_agent_id", "metadata.parentAgentId",
    "metadata.forkSource.sessionId", "metadata.previousSessionId",
    "extra_body.parent_session_id", "extra_body.parentSessionId", "extra_body.parentSessionID",
    "extra_body.parent_thread_id", "extra_body.parentThreadId",
    "extra_body.forked_from_thread_id", "extra_body.forked_from_id",
    "extra_body.parent_id", "extra_body.parentId", "extra_body.parentID",
    "extra_body.parent_task_id", "extra_body.parentTaskId",
    "extra_body.parent_action_id", "extra_body.parentActionId",
    "extra_body.parent_subagent_id", "extra_body.parentSubagentId",
    "extra_body.parent_session", "extra_body.parentSession",
];

const BODY_FORK_PATHS: &[&str] = &[
    "forked_from_thread_id", "forked_from_id",
    "forkSource.sessionId", "fork_source.session_id",
    "previousSessionId", "previous_session_id",
    "metadata.forked_from_thread_id", "metadata.forked_from_id",
    "metadata.forkSource.sessionId", "metadata.previousSessionId",
    "extra_body.forked_from_thread_id", "extra_body.forked_from_id",
    "extra_body.forkSource.sessionId", "extra_body.previousSessionId",
];

/// Claude Code `metadata.user_id`: JSON object or legacy `..._session_<uuid>` string. Returns
/// `(session_id, parent_session_id, agent_id)`.
pub fn claude_metadata_identities(payload: &[u8]) -> (String, String, String) {
    if payload.is_empty() {
        return Default::default();
    }
    let root = cpa_json::parse(payload);
    let mut user_id = root.g("metadata.user_id").str().trim().to_string();
    if user_id.is_empty() {
        let req = root.g("request");
        if req.exists() && !root.g("contents").exists() {
            user_id = req.g("metadata.user_id").str().trim().to_string();
        }
    }
    if user_id.is_empty() {
        return Default::default();
    }
    if user_id.starts_with('{') {
        let parsed = cpa_json::parse(user_id.as_bytes());
        let sid = cand(parsed.g("session_id"));
        let mut parent = cand(parsed.g("parent_session_id"));
        if parent.is_empty() {
            parent = cand(parsed.g("parent_agent_id"));
        }
        if parent.is_empty() {
            parent = cand(parsed.g("parent_id"));
        }
        let mut agent = cand(parsed.g("agent_id"));
        if agent.is_empty() {
            agent = cand(parsed.g("subagent_id"));
        }
        return (sid, parent, agent);
    }
    if let Some(sid) = legacy_claude_session(&user_id) {
        let sid = normalize_explicit_id(sid);
        let mut parent = cand(root.g("metadata.parent_agent_id"));
        if parent.is_empty() {
            parent = cand(root.g("metadata.parent_session_id"));
        }
        if parent.is_empty() {
            parent = cand(root.g("metadata.parent_id"));
        }
        let mut agent = cand(root.g("metadata.agent_id"));
        if agent.is_empty() {
            agent = cand(root.g("metadata.subagent_id"));
        }
        return (sid, parent, agent);
    }
    Default::default()
}

/// `_session_([a-f0-9-]+)$` without a regex dependency.
fn legacy_claude_session(user_id: &str) -> Option<&str> {
    let idx = user_id.rfind("_session_")?;
    let tail = &user_id[idx + "_session_".len()..];
    if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') {
        Some(tail)
    } else {
        None
    }
}

fn claude_agent_id(headers: &HeaderMap, r: &Roots, payload: &[u8]) -> String {
    let mut agent = header_value(headers, "X-Claude-Code-Agent-Id");
    if agent.is_empty() && r.exists {
        agent = cand(r.root.g("metadata.agent_id"));
        if agent.is_empty() {
            agent = cand(r.root.g("metadata.subagent_id"));
        }
        if agent.is_empty()
            && let Some(n) = &r.nested
        {
            agent = cand(n.g("metadata.agent_id"));
            if agent.is_empty() {
                agent = cand(n.g("metadata.subagent_id"));
            }
        }
    }
    if agent.is_empty() {
        agent = claude_metadata_identities(payload).2;
    }
    agent
}

fn claude_parent_agent_id(headers: &HeaderMap, r: &Roots) -> String {
    let mut p = header_value(headers, "X-Claude-Code-Parent-Agent-Id");
    if p.is_empty() && r.exists {
        p = cand(r.root.g("metadata.parent_agent_id"));
        if p.is_empty() {
            p = cand(r.root.g("metadata.parentAgentId"));
        }
        if p.is_empty()
            && let Some(n) = &r.nested
        {
            p = cand(n.g("metadata.parent_agent_id"));
            if p.is_empty() {
                p = cand(n.g("metadata.parentAgentId"));
            }
        }
    }
    p
}

fn is_body_fork_candidate(r: &Roots) -> bool {
    r.exists && !r.pick_first(BODY_FORK_PATHS).is_empty()
}

fn finalize(mut info: SessionInfo) -> Option<SessionInfo> {
    if info.session_id.is_empty() {
        return None;
    }
    info.session_id = bound_session_identity(&info.session_id);
    if !info.parent_session_id.is_empty() {
        info.parent_session_id = bound_session_identity(&info.parent_session_id);
    }
    if info.agent_name.is_empty() {
        info.agent_name = "main".into();
    }
    if info.client_type.is_empty() {
        info.client_type = "generic".into();
    }
    if info.parent_session_id == info.session_id {
        info.parent_session_id.clear();
    }
    Some(info)
}

fn meta_str(metadata: &Metadata, key: &str) -> Option<String> {
    metadata.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Generic "prefix:id" header session with an optional parent and subagent labelling.
fn simple_header_session(
    info: &mut SessionInfo,
    client_type: &str,
    prefix: &str,
    sid: &str,
    parent_header: String,
    parent_candidate: &str,
    default_agent: &str,
) {
    info.client_type = client_type.into();
    info.session_id = format!("{prefix}{sid}");
    if !parent_header.is_empty() && parent_header != sid {
        info.parent_session_id = format!("{prefix}{parent_header}");
        info.agent_name = "subagent".into();
    } else if !parent_candidate.is_empty() && parent_candidate != sid {
        info.parent_session_id = format!("{prefix}{parent_candidate}");
        info.agent_name = "subagent".into();
    } else {
        info.agent_name = default_agent.into();
    }
}

/// Extracts the session hierarchy from request attributes. Priority:
/// 1 X-Claude-Code-Session-Id, 2 Claude `metadata.user_id`, 3 Session-Id / Codex turn metadata,
/// 4 X-Http-Session-Id, 5 X-Session-ID / X-Session-Affinity / X-Slot-Session-Id / X-Task-Id /
/// X-Conversation-Id / X-Thread-Id / X-Client-Request-Id, 6 body keys (cachedContent, thread_id,
/// session_id, task_id, prompt_cache_key, conversation, metadata.user_id, conversation_id),
/// 7 `execution_session_id` metadata, 8 `lcp_affinity_session_id` metadata.
pub fn extract_session_info(headers: &HeaderMap, payload: &[u8], metadata: &Metadata) -> Option<SessionInfo> {
    let mut info = SessionInfo::default();
    if let Some(scope) = meta_str(metadata, meta::CALLER_SCOPE) {
        info.caller_scope = scope.trim().to_string();
    }
    let r = Roots::new(payload);
    let mut parent_candidate = String::new();
    if r.exists {
        parent_candidate = r.pick_first(PARENT_PATHS);
        if parent_candidate.is_empty() {
            parent_candidate = claude_metadata_identities(payload).1;
        }
    }

    // 1. Anthropic / Claude Code headers.
    let sid = header_value(headers, "X-Claude-Code-Session-Id");
    if !sid.is_empty() {
        info.client_type = "claude".into();
        let agent_id = claude_agent_id(headers, &r, payload);
        let parent_agent = claude_parent_agent_id(headers, &r);
        if !agent_id.is_empty() && agent_id != "main" {
            info.agent_name = agent_id.clone();
            info.parent_session_id = format!("claude:{sid}");
            if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent_id {
                info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
            } else if !parent_candidate.is_empty() && parent_candidate != sid {
                info.parent_session_id = format!("claude:{parent_candidate}");
            }
            info.session_id = format!("claude:{sid}:agent:{agent_id}");
        } else {
            info.agent_name = "main".into();
            info.session_id = format!("claude:{sid}");
            if !parent_candidate.is_empty() && parent_candidate != sid {
                info.parent_session_id = format!("claude:{parent_candidate}");
                info.agent_name = "subagent".into();
            }
        }
        return finalize(info);
    }

    // 2. Claude Code metadata.user_id in the payload (outranks generic headers).
    if !payload.is_empty() {
        let (sid, parent_sid, agent_id) = claude_metadata_identities(payload);
        if !sid.is_empty() {
            info.client_type = "claude".into();
            let mut agent_id = agent_id;
            if agent_id.is_empty() {
                agent_id = header_value(headers, "X-Claude-Code-Agent-Id");
            }
            if agent_id.is_empty() && r.exists {
                agent_id = cand(r.root.g("metadata.agent_id"));
                if agent_id.is_empty() {
                    agent_id = cand(r.root.g("metadata.subagent_id"));
                }
                if agent_id.is_empty()
                    && let Some(n) = &r.nested
                {
                    agent_id = cand(n.g("metadata.agent_id"));
                    if agent_id.is_empty() {
                        agent_id = cand(n.g("metadata.subagent_id"));
                    }
                }
            }
            let parent_agent = claude_parent_agent_id(headers, &r);
            if !agent_id.is_empty() && agent_id != "main" {
                info.session_id = format!("claude:{sid}:agent:{agent_id}");
                info.parent_session_id = format!("claude:{sid}");
                if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent_id {
                    info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
                } else if !parent_sid.is_empty() && parent_sid != sid {
                    info.parent_session_id = format!("claude:{parent_sid}");
                } else if !parent_candidate.is_empty() && parent_candidate != sid {
                    info.parent_session_id = format!("claude:{parent_candidate}");
                }
                info.agent_name = agent_id;
            } else {
                info.session_id = format!("claude:{sid}");
                if !parent_sid.is_empty() && parent_sid != sid {
                    info.parent_session_id = format!("claude:{parent_sid}");
                    info.agent_name = "subagent".into();
                } else if !parent_candidate.is_empty() && parent_candidate != sid {
                    info.parent_session_id = format!("claude:{parent_candidate}");
                    info.agent_name = "subagent".into();
                } else {
                    info.agent_name = "main".into();
                }
            }
            return finalize(info);
        }
    }

    // 3. OpenAI / Codex CLI headers.
    let mut sid = header_any(headers, &["Session-Id", "Session_id"]);
    let mut tid = header_any(headers, &["Thread-Id", "Thread_id"]);
    let codex_turn_meta = headers
        .get_all("X-Codex-Turn-Metadata")
        .iter()
        .next()
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let turn = if codex_turn_meta.is_empty() { Value::Null } else { cpa_json::parse(codex_turn_meta.as_bytes()) };
    let turn_exists = !turn.is_null();
    if sid.is_empty() && turn_exists {
        sid = cand(turn.g("session_id"));
    }
    if tid.is_empty() && turn_exists {
        tid = cand(turn.g("thread_id"));
    }
    if tid.is_empty() && !sid.is_empty() && r.exists {
        tid = r.pick_first(&["thread_id", "threadId", "metadata.thread_id"]);
    }
    if !sid.is_empty() || !tid.is_empty() {
        info.client_type = "codex".into();
        let mut parent_thread = header_any(headers, &["x-codex-parent-thread-id", "X-Codex-Parent-Thread-Id"]);
        if parent_thread.is_empty() && turn_exists {
            parent_thread = cand(turn.g("parent_thread_id"));
        }
        let mut forked_from = String::new();
        if turn_exists {
            forked_from = cand(turn.g("forked_from_thread_id"));
            if forked_from.is_empty() {
                forked_from = cand(turn.g("forked_from_id"));
            }
        }
        if forked_from.is_empty() && r.exists {
            forked_from = r.pick_first(&[
                "forked_from_thread_id",
                "forked_from_id",
                "metadata.forked_from_thread_id",
                "metadata.forked_from_id",
                "extra_body.forked_from_thread_id",
                "extra_body.forked_from_id",
            ]);
        }
        let mut clean_agent_name = String::new();
        if turn_exists {
            let raw = turn.g("agent_name").str();
            let raw = raw.strip_prefix("/root/").unwrap_or(&raw);
            let raw = raw.strip_prefix('/').unwrap_or(raw);
            let raw = normalize_explicit_id(raw.trim());
            if !raw.is_empty() && raw != "root" && raw != "main" {
                clean_agent_name = raw;
            }
        }
        let sub_val = header_value(headers, "X-Openai-Subagent");
        let mut subagent_signal = !sub_val.is_empty() && !sub_val.eq_ignore_ascii_case("false") && sub_val != "0";
        if turn_exists && turn.g("subagent_kind").str() == "thread_spawn" {
            subagent_signal = true;
        }

        // 1. Fork detection.
        if !forked_from.is_empty() {
            let mut fork_session = if tid.is_empty() { sid.clone() } else { tid.clone() };
            if fork_session == forked_from && !sid.is_empty() && sid != forked_from {
                fork_session = sid.clone();
            }
            info.session_id = format!("codex:{fork_session}");
            info.parent_session_id = format!("codex:{forked_from}");
            info.agent_name = "main".into();
            info.is_fork = true;
            info.is_subagent = false;
            return finalize(info);
        }

        // 2. Subagent detection (multi-agent v2).
        if subagent_signal
            || (!tid.is_empty() && !sid.is_empty() && tid != sid)
            || (!parent_thread.is_empty() && parent_thread != tid && parent_thread != sid)
        {
            let child = if tid.is_empty() { sid.clone() } else { tid.clone() };
            let parent_sid = if parent_thread.is_empty() { sid.clone() } else { parent_thread.clone() };
            if !clean_agent_name.is_empty() && !sid.is_empty() {
                info.session_id = format!("codex:{sid}:agent:{clean_agent_name}");
                info.agent_name = clean_agent_name.clone();
                if !parent_sid.is_empty() {
                    info.parent_session_id = format!("codex:{parent_sid}");
                } else if !parent_candidate.is_empty() && parent_candidate != sid {
                    info.parent_session_id = format!("codex:{parent_candidate}");
                }
            } else {
                info.session_id = format!("codex:{child}");
                info.agent_name = if clean_agent_name.is_empty() { "subagent".into() } else { clean_agent_name.clone() };
                if !parent_sid.is_empty() && parent_sid != child {
                    info.parent_session_id = format!("codex:{parent_sid}");
                } else if !parent_candidate.is_empty() && parent_candidate != child {
                    info.parent_session_id = format!("codex:{parent_candidate}");
                }
            }
            info.is_subagent = true;
            return finalize(info);
        }

        // 3. Normal interactive session.
        let session_id = if sid.is_empty() { tid.clone() } else { sid.clone() };
        info.session_id = format!("codex:{session_id}");
        if !parent_thread.is_empty() && parent_thread != session_id {
            info.parent_session_id = format!("codex:{parent_thread}");
            info.agent_name = "subagent".into();
            info.is_subagent = true;
        } else if !parent_candidate.is_empty() && parent_candidate != session_id {
            info.parent_session_id = format!("codex:{parent_candidate}");
            info.agent_name = "subagent".into();
            info.is_subagent = true;
        } else {
            info.agent_name = "main".into();
        }
        return finalize(info);
    }

    // 4. Antigravity CLI headers.
    let sid = header_value(headers, "X-Http-Session-Id");
    if !sid.is_empty() {
        let parent = header_any(headers, &["X-Parent-Session-ID", "X-Parent-Session-Id", "X-Parent-ID", "X-Parent-Id"]);
        simple_header_session(&mut info, "agy", "agy:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }

    // 5. OpenCode / Pi slot / task / generic headers.
    let sid = header_value(headers, "X-Session-ID");
    if !sid.is_empty() {
        let parent = header_any(headers, &["X-Parent-Session-ID", "X-Parent-Session-Id", "X-Parent-ID", "X-Parent-Id"]);
        simple_header_session(&mut info, "generic", "header:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }
    let sid = header_value(headers, "X-Session-Affinity");
    if !sid.is_empty() {
        let parent = header_any(
            headers,
            &["X-Parent-Session-Affinity", "X-Parent-Session-ID", "X-Parent-ID", "X-Parent-Id"],
        );
        simple_header_session(&mut info, "opencode", "affinity:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }
    let sid = header_value(headers, "X-Slot-Session-Id");
    if !sid.is_empty() {
        let parent = header_any(
            headers,
            &[
                "X-Parent-Slot-Session-Id",
                "X-Parent-Session-ID",
                "X-Parent-Session-Id",
                "X-Parent-ID",
                "X-Parent-Id",
            ],
        );
        simple_header_session(&mut info, "pi", "slot:", &sid, parent, &parent_candidate, "slot");
        return finalize(info);
    }
    let task_id = header_any(headers, &["X-Task-ID", "X-Task-Id", "X-Task_ID"]);
    if !task_id.is_empty() {
        let parent = header_any(
            headers,
            &[
                "X-Parent-Task-ID",
                "X-Parent-Task-Id",
                "X-Parent-Session-ID",
                "X-Parent-Session-Id",
                "X-Parent-ID",
                "X-Parent-Id",
            ],
        );
        simple_header_session(&mut info, "task", "task:", &task_id, parent, &parent_candidate, "main");
        return finalize(info);
    }
    let sid = header_value(headers, "X-Conversation-Id");
    if !sid.is_empty() {
        let parent = header_any(headers, &["X-Parent-Conversation-Id", "X-Parent-ID"]);
        simple_header_session(&mut info, "conv", "conv:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }
    let sid = header_value(headers, "X-Thread-Id");
    if !sid.is_empty() {
        let parent = header_any(headers, &["X-Parent-Thread-Id", "X-Parent-ID"]);
        simple_header_session(&mut info, "openai-thread", "thread:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }
    let sid = header_value(headers, "X-Client-Request-Id");
    if !sid.is_empty() {
        let parent = header_any(headers, &["X-Parent-Session-ID", "X-Parent-ID", "X-Parent-Id"]);
        simple_header_session(&mut info, "generic", "clientreq:", &sid, parent, &parent_candidate, "main");
        return finalize(info);
    }

    // 6. Payload inspection.
    if !payload.is_empty() && r.exists {
        // Gemini context caching.
        for path in ["cachedContent", "cached_content"] {
            let cache_id = r.pick(path);
            if !cache_id.is_empty() {
                info.client_type = "gemini".into();
                info.session_id = format!("geminicache:{cache_id}");
                if !parent_candidate.is_empty() && parent_candidate != cache_id {
                    info.parent_session_id = format!("geminicache:{parent_candidate}");
                    info.agent_name = "subagent".into();
                } else {
                    info.agent_name = "main".into();
                }
                return finalize(info);
            }
        }

        // OpenAI thread in payload.
        for path in ["thread_id", "threadId", "metadata.thread_id"] {
            let tid = r.pick(path);
            if !tid.is_empty() {
                info.client_type = "openai-thread".into();
                info.session_id = format!("thread:{tid}");
                if !parent_candidate.is_empty() && parent_candidate != tid {
                    info.parent_session_id = format!("thread:{parent_candidate}");
                    if is_body_fork_candidate(&r) {
                        info.is_fork = true;
                        info.is_subagent = false;
                        info.agent_name = "main".into();
                    } else {
                        info.agent_name = "subagent".into();
                        info.is_subagent = true;
                    }
                } else {
                    info.agent_name = "main".into();
                }
                return finalize(info);
            }
        }

        // Generic session in payload.
        let mut agent_id = r.root_only("metadata.agent_id");
        if agent_id.is_empty() {
            agent_id = r.root_only("metadata.subagent_id");
        }
        if agent_id.is_empty() {
            agent_id = header_value(headers, "X-Claude-Code-Agent-Id");
        }
        if agent_id.is_empty() {
            agent_id = header_value(headers, "x-agent-id");
        }
        if agent_id.is_empty()
            && let Some(n) = &r.nested
        {
            agent_id = cand(n.g("metadata.agent_id"));
            if agent_id.is_empty() {
                agent_id = cand(n.g("metadata.subagent_id"));
            }
        }
        for path in [
            "session_id", "sessionId", "sessionID",
            "child_session_id", "childSessionId",
            "metadata.session_id", "metadata.sessionId", "metadata.sessionID",
            "metadata.child_session_id",
            "extra_body.session_id", "extra_body.sessionId", "extra_body.sessionID",
        ] {
            let sid = r.pick(path);
            if sid.is_empty() {
                continue;
            }
            info.client_type = "generic".into();
            if !agent_id.is_empty() && agent_id != "main" {
                info.session_id = format!("session:{sid}:agent:{agent_id}");
                info.parent_session_id = format!("session:{sid}");
                if !parent_candidate.is_empty() && parent_candidate != sid {
                    info.parent_session_id = format!("session:{parent_candidate}");
                }
                info.agent_name = agent_id.clone();
            } else {
                info.session_id = format!("session:{sid}");
                if !parent_candidate.is_empty() && parent_candidate != sid {
                    info.parent_session_id = format!("session:{parent_candidate}");
                    if is_body_fork_candidate(&r) {
                        info.is_fork = true;
                        info.is_subagent = false;
                        info.agent_name = "main".into();
                    } else {
                        info.agent_name = "subagent".into();
                        info.is_subagent = true;
                    }
                } else {
                    info.agent_name = "main".into();
                }
            }
            return finalize(info);
        }

        // Task / action in payload (Roo Code, Cline, OpenHands).
        for path in [
            "task_id", "taskId", "taskID",
            "action_id", "actionId", "actionID",
            "metadata.task_id", "metadata.taskId", "metadata.taskID",
            "metadata.action_id", "metadata.actionId", "metadata.actionID",
            "extra_body.task_id", "extra_body.taskId", "extra_body.taskID",
        ] {
            let tid = r.pick(path);
            if tid.is_empty() {
                continue;
            }
            info.client_type = "task".into();
            info.session_id = format!("task:{tid}");
            if !parent_candidate.is_empty() && parent_candidate != tid {
                info.parent_session_id = format!("task:{parent_candidate}");
                if is_body_fork_candidate(&r) {
                    info.is_fork = true;
                    info.is_subagent = false;
                    info.agent_name = "main".into();
                } else {
                    info.agent_name = "subagent".into();
                    info.is_subagent = true;
                }
            } else {
                info.agent_name = "main".into();
            }
            return finalize(info);
        }

        // Prompt cache key and conversation object.
        let mut conversation_id = String::new();
        let mut conversation = r.root.g("conversation").into_value();
        if conversation.is_none()
            && let Some(n) = &r.nested
        {
            conversation = n.g("conversation").into_value();
        }
        if let Some(conv) = &conversation {
            let sid = cand(conv.g("id"));
            if !sid.is_empty() {
                conversation_id = format!("conv:{sid}");
            } else if let Some(s) = conv.as_str() {
                let sid = normalize_explicit_id(s);
                if !sid.is_empty() {
                    conversation_id = format!("conv:{sid}");
                }
            }
        }
        let mut pck = r.root_only("prompt_cache_key");
        if pck.is_empty() {
            pck = r.root_only("promptCacheKey");
        }
        if pck.is_empty()
            && let Some(n) = &r.nested
        {
            pck = cand(n.g("prompt_cache_key"));
            if pck.is_empty() {
                pck = cand(n.g("promptCacheKey"));
            }
        }
        if !pck.is_empty() {
            info.client_type = "generic".into();
            info.session_id = format!("pck:{pck}");
            if !parent_candidate.is_empty() && parent_candidate != pck {
                info.parent_session_id = format!("pck:{parent_candidate}");
                info.agent_name = "subagent".into();
            } else {
                info.agent_name = "main".into();
            }
            return finalize(info);
        }
        if !conversation_id.is_empty() {
            info.client_type = "conv".into();
            info.session_id = conversation_id.clone();
            if !parent_candidate.is_empty() && format!("conv:{parent_candidate}") != conversation_id {
                info.parent_session_id = format!("conv:{parent_candidate}");
                info.agent_name = "subagent".into();
            } else {
                info.agent_name = "main".into();
            }
            return finalize(info);
        }

        // Plain metadata.user_id.
        let user_id = r.pick("metadata.user_id");
        if !user_id.is_empty() {
            info.client_type = "generic".into();
            info.session_id = format!("user:{user_id}");
            info.agent_name = "main".into();
            return finalize(info);
        }

        // Legacy conversation string paths.
        for path in [
            "conversation_id", "conversationId", "chat_id", "chatId",
            "metadata.conversation_id", "extra_body.conversation_id",
        ] {
            let cid = r.pick(path);
            if cid.is_empty() {
                continue;
            }
            info.client_type = "conv".into();
            info.session_id = format!("conv:{cid}");
            if !parent_candidate.is_empty() && parent_candidate != cid {
                info.parent_session_id = format!("conv:{parent_candidate}");
                info.agent_name = "subagent".into();
            } else {
                info.agent_name = "main".into();
            }
            return finalize(info);
        }
    }

    // 7. Execution session metadata.
    if let Some(execution_id) = meta_str(metadata, meta::EXECUTION_SESSION_ID) {
        let execution_id = normalize_explicit_id(&execution_id);
        if !execution_id.is_empty() {
            info.client_type = "generic".into();
            info.session_id = format!("execution:{execution_id}");
            info.agent_name = "main".into();
            return finalize(info);
        }
    }

    // 8. LCP affinity session id carried in metadata.
    if let Some(lcp_id) = meta_str(metadata, LCP_AFFINITY_SESSION_ID) {
        let lcp_id = normalize_explicit_id(&lcp_id);
        if !lcp_id.is_empty() {
            info.client_type = "lcp".into();
            info.session_id = lcp_id.clone();
            match meta_str(metadata, meta::PARENT_SESSION_ID) {
                Some(parent) => {
                    let parent = normalize_explicit_id(&parent);
                    if !parent.is_empty() && parent != lcp_id {
                        info.parent_session_id = parent;
                        if metadata.get(meta::IS_COMPACTION).and_then(Value::as_bool).unwrap_or(false) {
                            info.is_compaction = true;
                            info.is_fork = false;
                            info.node_kind = "compaction".into();
                            info.agent_name = "main".into();
                        } else {
                            info.agent_name = "subagent".into();
                            info.is_fork = true;
                            info.node_kind = "fork".into();
                        }
                    } else {
                        info.agent_name = "main".into();
                    }
                }
                None => info.agent_name = "main".into(),
            }
            return finalize(info);
        }
    }
    None
}

/// Metadata key the Go LCP matcher uses; honoured on input only (the matcher is not ported).
pub const LCP_AFFINITY_SESSION_ID: &str = "lcp_affinity_session_id";

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn claude_header_session_with_agent() {
        let h = headers(&[("x-claude-code-session-id", "s1"), ("x-claude-code-agent-id", "a1")]);
        let info = extract_session_info(&h, b"", &Metadata::new()).unwrap();
        assert_eq!(info.session_id, "claude:s1:agent:a1");
        assert_eq!(info.parent_session_id, "claude:s1");
        assert_eq!(info.client_type, "claude");
    }

    #[test]
    fn claude_user_id_json_and_legacy() {
        let body = br#"{"metadata":{"user_id":"{\"session_id\":\"abc\",\"parent_session_id\":\"p\"}"}}"#;
        let info = extract_session_info(&HeaderMap::new(), body, &Metadata::new()).unwrap();
        assert_eq!((info.session_id.as_str(), info.parent_session_id.as_str()), ("claude:abc", "claude:p"));
        let legacy = br#"{"metadata":{"user_id":"user_x_account__session_0a1b-2c"}}"#;
        let info = extract_session_info(&HeaderMap::new(), legacy, &Metadata::new()).unwrap();
        assert_eq!(info.session_id, "claude:0a1b-2c");
    }

    #[test]
    fn header_priority_codex_then_generic_then_body() {
        let h = headers(&[("session-id", "c1"), ("x-session-id", "g1")]);
        assert_eq!(extract_session_info(&h, b"", &Metadata::new()).unwrap().session_id, "codex:c1");
        let h = headers(&[("x-session-id", "g1")]);
        assert_eq!(extract_session_info(&h, b"", &Metadata::new()).unwrap().session_id, "header:g1");
        let body = br#"{"prompt_cache_key":"k","conversation":{"id":"c"}}"#;
        assert_eq!(extract_session_info(&HeaderMap::new(), body, &Metadata::new()).unwrap().session_id, "pck:k");
        assert!(extract_session_info(&HeaderMap::new(), br#"{"messages":[]}"#, &Metadata::new()).is_none());
    }

    #[test]
    fn long_ids_are_bounded_with_hash() {
        let id = "x".repeat(300);
        let b = bound_session_identity(&id);
        assert!(b.len() <= 255 && b.contains('#'));
        assert_eq!(normalize_explicit_id(&id), "");
        assert_eq!(normalize_explicit_id("a\nb"), "");
    }

    #[test]
    fn subagent_header_marks_codex_subagent() {
        let h = headers(&[("session-id", "root"), ("thread-id", "child")]);
        let info = extract_session_info(&h, b"", &Metadata::new()).unwrap();
        assert!(info.is_subagent);
        assert_eq!((info.session_id.as_str(), info.parent_session_id.as_str()), ("codex:child", "codex:root"));
    }
}
