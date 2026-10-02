//! Devin Connect-RPC wire protocol (Go: helps/devin_wire.go).
//!
//! Hand-rolled protobuf for `GetChatMessageRequest` and the response frames, the Connect
//! envelope framing (flags 0x00 data, 0x01 gzip, 0x02 end-stream), the trailer error mapping and
//! the UTF-8 split buffer used while streaming text deltas.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::LazyLock;

use bytes::{Buf, Bytes, BytesMut};
use cpa_core::util::is_claude_code_attribution_system_text;
use cpa_translator::common::{
    is_devin_codex_app_automation_update, sanitize_devin_tool_description,
};
use futures_util::{Stream, StreamExt};
use rand::RngCore;

use super::pb;
use super::sensitive::SensitiveWordMatcher;
use crate::helps::proxy::BoundedLru;

pub const CONNECT_FLAG_DATA: u8 = 0x00;
pub const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
pub const CONNECT_FLAG_END_STREAM: u8 = 0x02;
const CONNECT_FLAG_COMPRESSED_END_STREAM: u8 = CONNECT_FLAG_COMPRESSED | CONNECT_FLAG_END_STREAM;

pub const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
pub const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

pub const DEFAULT_CLIENT_NAME: &str = "chisel";
pub const DEFAULT_CLIENT_VERSION: &str = "3000.10.21";
pub const FINGERPRINT_HEX_LEN: usize = 732;
pub const DEFAULT_MAX_TOKENS: i64 = 128_000;

const MAX_CONNECT_FRAME_SIZE: usize = 16 * 1024 * 1024;
const MAX_DECOMPRESSED_FRAME_SIZE: usize = 64 * 1024 * 1024;
const MAX_SESSION_TURN_COUNTERS: usize = 5000;

/// Tool definition in `GetChatMessageRequest` (field 10).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON schema bytes.
    pub parameters: Vec<u8>,
}

/// Tool call attached to an assistant prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Streaming tool call chunk (response field 6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallDelta {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub invalid_json_str: String,
    pub invalid_json_err: String,
    pub is_custom_tool_call: bool,
}

/// Image attachment of a prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Image {
    pub base64_data: String,
    pub mime_type: String,
}

/// One turn of the request history (repeated field 3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    pub message_id: String,
    /// 1 user, 2 assistant, 4 tool; values <= 0 encode as 1.
    pub source: i32,
    pub content: String,
    pub images: Vec<Image>,
    pub tool_calls: Vec<ToolCall>,
    /// For source 4 (tool result).
    pub tool_call_id: String,
    /// Retained when a tool result is downgraded to a user turn.
    pub original_tool_call_id: String,
    pub is_orphaned_tool: bool,
    pub thinking: String,
    pub signature: Vec<u8>,
    pub signature_type: String,
}

/// Token accounting from response field 7.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_write_tokens: i64,
    pub status_code: u64,
    pub request_id: String,
    pub model_name: String,
    pub headers: BTreeMap<String, String>,
}

/// Decoded content of one response frame. Text fields stay bytes: a multi-byte character may be
/// split across frames and is reassembled by [`Utf8SplitBuffer`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameResult {
    pub output_id: String,
    pub timestamp: u64,
    pub content_text: Vec<u8>,
    pub delta_tokens: u64,
    /// 2/4 stop, 10 tool calls.
    pub stop_reason: u64,
    pub tool_call_deltas: Vec<ToolCallDelta>,
    pub thinking_text: Vec<u8>,
    pub delta_signature: Vec<u8>,
    pub delta_signature_type: String,
    pub latency: f64,
    pub message_id: String,
    pub usage: Option<Usage>,
    pub response_dimension_groups: Vec<Vec<u8>>,
    pub unknown_field_numbers: Vec<i32>,
}

/// Malformed protobuf in a response frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WireError(pub String);

// ------------------------------------------------------------------ identity material

/// Device fingerprint (metadata field 31): random per call when `seed` is empty, otherwise the
/// deterministic SHA-256 chain shared with the auth crate.
pub fn generate_device_fingerprint(seed: &str) -> String {
    cpa_auth::devin::generate_device_fingerprint(seed)
}

/// `<32 hex trace id>-<16 hex span id>-1`.
pub fn generate_sentry_trace() -> String {
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
    format!("{}-{}-1", hex::encode(&b[..16]), hex::encode(&b[16..]))
}

type TurnCounters = BoundedLru<String, std::sync::Arc<std::sync::atomic::AtomicU64>>;

static SESSION_TURNS: LazyLock<TurnCounters> =
    LazyLock::new(|| BoundedLru::new(MAX_SESSION_TURN_COUNTERS));

/// Next 0-based request ordinal of a session (field 15.2): 0 on the first request (omitted on the
/// wire), then 1, 2, ... Blank session ids always yield 0.
pub fn next_session_turn_index(session_id: &str) -> u64 {
    let id = session_id.trim();
    if id.is_empty() {
        return 0;
    }
    let counter = SESSION_TURNS
        .get_or_build::<std::convert::Infallible>(id.to_string(), || Ok(Default::default()))
        .unwrap_or_default();
    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Forgets a session's counter (tests and explicit session resets).
pub fn reset_session_turn_index(session_id: &str) {
    SESSION_TURNS.close_key(&session_id.trim().to_string());
}

// ------------------------------------------------------------------ Connect framing

/// `[flag][u32 big-endian length][payload]`.
pub fn wrap_connect_envelope_with_flag(flag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A data (flag 0x00) envelope.
pub fn wrap_connect_envelope(payload: &[u8]) -> Vec<u8> {
    wrap_connect_envelope_with_flag(CONNECT_FLAG_DATA, payload)
}

/// Why [`ConnectFrameReader::read_frame`] stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// Go `io.EOF`: the body ended before any byte of the next header or payload.
    #[error("EOF")]
    Eof,
    /// The body ended in the middle of a header or payload.
    #[error("unexpected EOF")]
    UnexpectedEof,
    /// Bad flag, oversized frame, gzip failure, or a transport error.
    #[error("{0}")]
    Invalid(String),
}

/// One decoded Connect frame; gzip payloads are already decompressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFrame {
    pub flag: u8,
    pub payload: Vec<u8>,
}

/// Incremental Connect frame decoder over an async byte stream (Go: ReadConnectFrame).
pub struct ConnectFrameReader<S> {
    stream: S,
    buf: BytesMut,
    eof: bool,
}

impl<S, E> ConnectFrameReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            eof: false,
        }
    }

    /// Pulls from the stream until `buf` holds `want` bytes. `Ok(false)` on end of body.
    async fn fill(&mut self, want: usize) -> Result<bool, FrameError> {
        while self.buf.len() < want {
            if self.eof {
                return Ok(false);
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(FrameError::Invalid(e.to_string())),
                None => self.eof = true,
            }
        }
        Ok(true)
    }

    /// Reads the next frame. A clean end of body is [`FrameError::Eof`].
    pub async fn read_frame(&mut self) -> Result<ConnectFrame, FrameError> {
        if !self.fill(5).await? {
            return Err(if self.buf.is_empty() {
                FrameError::Eof
            } else {
                FrameError::UnexpectedEof
            });
        }
        let flag = self.buf[0];
        if !matches!(
            flag,
            CONNECT_FLAG_DATA
                | CONNECT_FLAG_COMPRESSED
                | CONNECT_FLAG_END_STREAM
                | CONNECT_FLAG_COMPRESSED_END_STREAM
        ) {
            return Err(FrameError::Invalid(format!(
                "invalid connect frame flag: 0x{flag:02x}"
            )));
        }
        let length =
            u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if length > MAX_CONNECT_FRAME_SIZE {
            return Err(FrameError::Invalid(format!(
                "connect frame length {length} exceeds maximum limit ({MAX_CONNECT_FRAME_SIZE})"
            )));
        }
        self.buf.advance(5);
        if length > 0 && !self.fill(length).await? {
            // io.ReadFull: EOF only when nothing of the payload was read.
            return Err(if self.buf.is_empty() {
                FrameError::Eof
            } else {
                FrameError::UnexpectedEof
            });
        }
        let payload = self.buf.split_to(length).to_vec();
        if flag & CONNECT_FLAG_COMPRESSED == 0 {
            return Ok(ConnectFrame { flag, payload });
        }
        Ok(ConnectFrame {
            flag,
            payload: gunzip_limited(&payload)?,
        })
    }
}

fn gunzip_limited(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    let mut limited = (&mut decoder).take(MAX_DECOMPRESSED_FRAME_SIZE as u64 + 1);
    match limited.read_to_end(&mut out) {
        Ok(_) => {}
        // GzDecoder reads the header lazily; Go reports that as the "decompress" failure.
        Err(e) if out.is_empty() => {
            return Err(FrameError::Invalid(format!(
                "decompress gzip connect frame: {e}"
            )));
        }
        Err(e) => {
            return Err(FrameError::Invalid(format!(
                "read decompressed connect frame: {e}"
            )));
        }
    }
    if out.len() > MAX_DECOMPRESSED_FRAME_SIZE {
        return Err(FrameError::Invalid(format!(
            "decompressed frame size exceeds maximum limit ({MAX_DECOMPRESSED_FRAME_SIZE})"
        )));
    }
    Ok(out)
}

// ------------------------------------------------------------------ request encoding

/// Go `runtime.GOOS` spelling of the host OS.
fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Serialized field 1 (ClientMetadata). An empty `os_name` means the host OS.
pub fn build_client_metadata_bytes(
    session_token: &str,
    device_seed: &str,
    os_name: &str,
) -> Vec<u8> {
    let os_name = if os_name.is_empty() { go_os() } else { os_name };
    let mut b = Vec::new();
    pb::put_str(&mut b, 1, DEFAULT_CLIENT_NAME);
    pb::put_str(&mut b, 2, DEFAULT_CLIENT_VERSION);
    pb::put_str(&mut b, 3, session_token);
    pb::put_str(&mut b, 4, "en");
    pb::put_str(&mut b, 5, os_name);
    pb::put_str(&mut b, 7, DEFAULT_CLIENT_VERSION);
    pb::put_str(&mut b, 12, DEFAULT_CLIENT_NAME);
    pb::put_str(&mut b, 31, &generate_device_fingerprint(device_seed));
    b
}

/// Inputs of [`build_get_chat_message_request`].
pub struct ChatRequest<'a> {
    pub session_token: &'a str,
    pub device_seed: &'a str,
    pub chat_model_uid: &'a str,
    pub system_prompt: &'a str,
    pub prompts: &'a [Prompt],
    pub tools: &'a [Tool],
    pub temperature: Option<f64>,
    pub max_tokens: i64,
    /// Random when empty.
    pub session_id: &'a str,
    /// Defaults to the session id.
    pub cascade_id: &'a str,
    pub matcher: Option<&'a SensitiveWordMatcher>,
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Description fixups applied to every tool before it is encoded or logged.
fn prepare_tool_description(name: &str, description: &str) -> String {
    // Claude Code subagent tools say "task_id" but the Devin tool runtime expects "taskId".
    const SNAKE: &str = "Takes a task_id parameter identifying the task";
    let desc = if description.contains(SNAKE) {
        description.replace(SNAKE, "Takes a taskId parameter identifying the task")
    } else {
        description.to_string()
    };
    sanitize_devin_tool_description(name, &desc)
}

/// Encodes a whole `GetChatMessageRequest` protobuf payload (no Connect envelope).
pub fn build_get_chat_message_request(req: &ChatRequest<'_>) -> Vec<u8> {
    let max_tokens = if req.max_tokens <= 0 {
        DEFAULT_MAX_TOKENS
    } else {
        req.max_tokens
    };
    let session_id = if req.session_id.is_empty() {
        new_uuid()
    } else {
        req.session_id.to_string()
    };
    let cascade_id = if req.cascade_id.is_empty() {
        session_id.as_str()
    } else {
        req.cascade_id
    };

    let mut out = Vec::with_capacity(
        4096 + req.system_prompt.len()
            + req
                .prompts
                .iter()
                .map(|p| 256 + p.content.len())
                .sum::<usize>(),
    );

    // 1. ClientMetadata
    pb::put_bytes(
        &mut out,
        1,
        &build_client_metadata_bytes(req.session_token, req.device_seed, go_os()),
    );

    // 2. System prompt
    if !req.system_prompt.is_empty() {
        let sanitized = sanitize_system_prompt(req.system_prompt, req.matcher);
        if !sanitized.is_empty() {
            pb::put_str(&mut out, 2, &sanitized);
        }
    }

    // 3. History prompts
    for p in req.prompts {
        let mut pb_bytes = Vec::new();
        let msg_id = if p.message_id.is_empty() {
            new_uuid()
        } else {
            p.message_id.clone()
        };
        pb::put_str(&mut pb_bytes, 1, &msg_id);
        let source = if p.source <= 0 { 1 } else { p.source };
        pb::put_varint_field(&mut pb_bytes, 2, source as u64);
        pb::put_str(&mut pb_bytes, 3, &p.content);

        for tc in &p.tool_calls {
            let mut tc_bytes = Vec::new();
            if !tc.id.is_empty() {
                pb::put_str(&mut tc_bytes, 1, &tc.id);
            }
            if !tc.name.is_empty() {
                pb::put_str(&mut tc_bytes, 2, &tc.name);
            }
            if !tc.arguments.is_empty() {
                pb::put_str(&mut tc_bytes, 3, &tc.arguments);
            }
            pb::put_bytes(&mut pb_bytes, 6, &tc_bytes);
        }
        if !p.tool_call_id.is_empty() {
            pb::put_str(&mut pb_bytes, 7, &p.tool_call_id);
        }
        for img in &p.images {
            let data = img.base64_data.trim();
            if data.is_empty() {
                continue;
            }
            let mut img_bytes = Vec::new();
            pb::put_str(&mut img_bytes, 1, data);
            let mime = img.mime_type.trim();
            pb::put_str(
                &mut img_bytes,
                2,
                if mime.is_empty() { "image/png" } else { mime },
            );
            pb::put_bytes(&mut pb_bytes, 10, &img_bytes);
        }
        if !p.thinking.is_empty() {
            pb::put_str(&mut pb_bytes, 11, &p.thinking);
        }
        if !p.signature.is_empty() {
            pb::put_bytes(&mut pb_bytes, 12, &p.signature);
        }
        if !p.signature_type.is_empty() {
            pb::put_str(&mut pb_bytes, 18, &p.signature_type);
        }
        pb::put_bytes(&mut out, 3, &pb_bytes);
    }

    // 4. Fixed flags
    pb::put_varint_field(&mut out, 7, 5);

    // 5. Completion config
    let mut f8 = Vec::new();
    pb::put_varint_field(&mut f8, 1, 1);
    pb::put_varint_field(&mut f8, 2, max_tokens as u64);
    pb::put_varint_field(&mut f8, 3, 400);
    pb::put_fixed64_field(&mut f8, 5, req.temperature.unwrap_or(1.0).to_bits());
    pb::put_varint_field(&mut f8, 7, 40);
    pb::put_fixed64_field(&mut f8, 8, f64::from(0.95f32).to_bits());
    pb::put_bytes(&mut out, 8, &f8);

    // 6. Tools
    for tool in req.tools {
        if tool.name.is_empty() || is_devin_codex_app_automation_update("", &tool.name) {
            continue;
        }
        let mut t = Vec::new();
        pb::put_str(&mut t, 1, &tool.name);
        let desc = prepare_tool_description(&tool.name, &tool.description);
        if !desc.is_empty() {
            pb::put_str(&mut t, 2, &desc);
        }
        if !tool.parameters.is_empty() {
            pb::put_bytes(&mut t, 3, &tool.parameters);
        }
        pb::put_bytes(&mut out, 10, &t);
    }

    // 7. Thread session metadata: 15.1 session, 15.2 turn index (omitted at 0), 15.3 = 4,
    //    15.4 = 14 on user-turn boundaries.
    let turn_index = next_session_turn_index(&session_id);
    let mut f15 = Vec::new();
    pb::put_str(&mut f15, 1, &session_id);
    if turn_index > 0 {
        pb::put_varint_field(&mut f15, 2, turn_index);
    }
    pb::put_varint_field(&mut f15, 3, 4);
    if let Some(last) = req.prompts.last()
        && last.source == 1
        && (turn_index == 0
            || req.prompts.len() < 2
            || req.prompts[req.prompts.len() - 2].source != 1)
    {
        pb::put_varint_field(&mut f15, 4, 14);
    }
    pb::put_bytes(&mut out, 15, &f15);

    // 8. Cascade id (prompt cache key), 9. fixed flag, 10. model uid
    pb::put_str(&mut out, 16, cascade_id);
    pb::put_varint_field(&mut out, 20, 1);
    pb::put_str(&mut out, 21, req.chat_model_uid);
    out
}

/// Lines of Claude Code / Codex identity text that must not reach the Devin upstream, plus
/// configured sensitive words (matching lines dropped, remaining occurrences obfuscated).
pub fn sanitize_system_prompt(prompt: &str, matcher: Option<&SensitiveWordMatcher>) -> String {
    if prompt.is_empty() {
        return String::new();
    }
    // "Don't output ANSI escape codes directly - the CLI renderer applies them." with typographic
    // apostrophe and em dash, spelled with escapes.
    const ANSI_LINE: &str =
        "- Don\u{2019}t output ANSI escape codes directly \u{2014} the CLI renderer applies them.";
    let normalized = prompt.replace("\r\n", "\n");
    let mut kept: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        let trimmed = line.trim();
        if is_claude_code_attribution_system_text(trimmed)
            || trimmed.starts_with("You are Claude Code")
            || trimmed.contains("authorized security testing")
            || trimmed.contains("destructive techniques, DoS attacks")
            || trimmed.contains("Claude Code is available as a CLI")
            || trimmed.contains("Fast mode for Claude Code")
            || trimmed.contains("Codex refers to the open-source agentic coding interface")
            || trimmed.contains(ANSI_LINE)
        {
            continue;
        }
        if matcher.is_some_and(|m| m.matches(trimmed)) {
            continue;
        }
        kept.push(line);
    }
    let res = kept.join("\n").trim().to_string();
    match matcher {
        Some(m) if !res.is_empty() => m.obfuscate_text(&res),
        _ => res,
    }
}

/// Tool declarations in the shape sent upstream (automation_update dropped, descriptions fixed);
/// used by request logging helpers and tests.
pub fn upstream_tools(tools: &[Tool]) -> Vec<Tool> {
    tools
        .iter()
        .filter(|t| !t.name.is_empty() && !is_devin_codex_app_automation_update("", &t.name))
        .map(|t| Tool {
            name: t.name.clone(),
            description: prepare_tool_description(&t.name, &t.description),
            parameters: t.parameters.clone(),
        })
        .collect()
}

// ------------------------------------------------------------------ response decoding

fn lossy(b: &[u8]) -> String {
    go_lossy(b)
}

/// Decodes one response frame payload. An error discards the frame (callers skip it).
pub fn parse_frame(payload: &[u8]) -> Result<FrameResult, WireError> {
    let mut res = FrameResult::default();
    let mut pos = 0;
    while pos < payload.len() {
        let (num, typ, n) = pb::get_tag(&payload[pos..])
            .ok_or_else(|| WireError(format!("consume tag error at offset {pos}")))?;
        pos += n;
        match typ {
            pb::VARINT => {
                let (v, vn) = pb::get_varint(&payload[pos..])
                    .ok_or_else(|| WireError(format!("consume varint error at offset {pos}")))?;
                pos += vn;
                match num {
                    2 => res.timestamp = v,
                    4 => res.delta_tokens = v,
                    5 => res.stop_reason = v,
                    _ => {}
                }
            }
            pb::FIXED64 => {
                let v = pb::get_fixed64(&payload[pos..])
                    .ok_or_else(|| WireError(format!("consume fixed64 error at offset {pos}")))?;
                pos += 8;
                if num == 12 {
                    res.latency = f64::from_bits(v);
                }
            }
            pb::FIXED32 => {
                pb::get_fixed32(&payload[pos..])
                    .ok_or_else(|| WireError(format!("consume fixed32 error at offset {pos}")))?;
                pos += 4;
            }
            pb::BYTES => {
                let (val, bn) = pb::get_bytes(&payload[pos..])
                    .ok_or_else(|| WireError(format!("consume bytes error at offset {pos}")))?;
                pos += bn;
                match num {
                    1 => res.output_id = lossy(val),
                    2 => res.timestamp = parse_timestamp(val),
                    3 => res.content_text.extend_from_slice(val),
                    6 => {
                        if let Ok(tc) = parse_tool_call_delta(val) {
                            res.tool_call_deltas.push(tc);
                        }
                    }
                    7 => res.usage = Some(parse_usage_field(val)),
                    9 => res.thinking_text.extend_from_slice(val),
                    10 => res.delta_signature.extend_from_slice(val),
                    17 => res.message_id = lossy(val),
                    21 => res.delta_signature_type = lossy(val),
                    28 => res.response_dimension_groups.push(val.to_vec()),
                    _ => res.unknown_field_numbers.push(num as i32),
                }
            }
            other => {
                return Err(WireError(format!(
                    "unsupported wire type {other} at offset {pos}"
                )));
            }
        }
    }
    Ok(res)
}

fn parse_tool_call_delta(data: &[u8]) -> Result<ToolCallDelta, WireError> {
    let mut tc = ToolCallDelta::default();
    let mut pos = 0;
    let bad = || WireError("malformed tool call delta".into());
    while pos < data.len() {
        let (num, typ, n) = pb::get_tag(&data[pos..]).ok_or_else(bad)?;
        pos += n;
        match typ {
            pb::VARINT => {
                let (v, vn) = pb::get_varint(&data[pos..]).ok_or_else(bad)?;
                pos += vn;
                if num == 6 {
                    tc.is_custom_tool_call = v != 0;
                }
            }
            pb::BYTES => {
                let (val, bn) = pb::get_bytes(&data[pos..]).ok_or_else(bad)?;
                pos += bn;
                match num {
                    1 => tc.id = lossy(val),
                    2 => tc.name = lossy(val),
                    3 => tc.arguments = lossy(val),
                    4 => tc.invalid_json_str = lossy(val),
                    5 => tc.invalid_json_err = lossy(val),
                    _ => {}
                }
            }
            other => pos += pb::skip_field(num, other, &data[pos..]).ok_or_else(bad)?,
        }
    }
    Ok(tc)
}

/// Seconds of a `google.protobuf.Timestamp` (field 1); stops at the first non-varint field.
fn parse_timestamp(data: &[u8]) -> u64 {
    let mut pos = 0;
    let mut secs = 0;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if typ != pb::VARINT {
            break;
        }
        let Some((v, vn)) = pb::get_varint(&data[pos..]) else {
            break;
        };
        pos += vn;
        if num == 1 {
            secs = v;
        }
    }
    secs
}

/// Name/value pair of one upstream header echoed in the usage field (field 7, sub-field 8).
fn parse_header_field(data: &[u8]) -> (String, String) {
    let (mut key, mut val) = (String::new(), String::new());
    let mut pos = 0;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if typ == pb::BYTES {
            let Some((b, bn)) = pb::get_bytes(&data[pos..]) else {
                return (key, val);
            };
            pos += bn;
            match num {
                1 => key = lossy(b),
                2 => val = lossy(b),
                _ => {}
            }
        } else {
            let Some(skip) = pb::skip_field(num, typ, &data[pos..]) else {
                return (key, val);
            };
            pos += skip;
        }
    }
    (key, val)
}

fn is_printable_ascii(b: &[u8]) -> bool {
    b.iter().all(|c| (32..=126).contains(c))
}

/// Decodes the usage message (field 7); malformed tails keep what was read so far.
pub fn parse_usage_field(data: &[u8]) -> Usage {
    let mut u = Usage::default();
    let mut pos = 0;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        match typ {
            pb::VARINT => {
                let Some((v, vn)) = pb::get_varint(&data[pos..]) else {
                    return u;
                };
                pos += vn;
                match num {
                    2 => u.prompt_tokens = u.prompt_tokens.wrapping_add(v as i64),
                    3 => u.completion_tokens = v as i64,
                    4 => u.cache_write_tokens = u.cache_write_tokens.wrapping_add(v as i64),
                    5 => u.cached_tokens = v as i64,
                    6 => u.status_code = v,
                    _ => {}
                }
            }
            pb::BYTES => {
                let Some((val, bn)) = pb::get_bytes(&data[pos..]) else {
                    return u;
                };
                pos += bn;
                match num {
                    8 => {
                        let (k, v) = parse_header_field(val);
                        if !k.is_empty() {
                            if (k.eq_ignore_ascii_case("x-request-id")
                                || k.eq_ignore_ascii_case("request-id"))
                                && !v.is_empty()
                            {
                                u.request_id = v.clone();
                            }
                            u.headers.insert(k, v);
                        } else if !val.is_empty()
                            && is_printable_ascii(val)
                            && u.request_id.is_empty()
                        {
                            u.request_id = lossy(val);
                        }
                    }
                    9 => u.model_name = lossy(val),
                    _ => {}
                }
            }
            other => {
                let Some(skip) = pb::skip_field(num, other, &data[pos..]) else {
                    return u;
                };
                pos += skip;
            }
        }
    }
    u
}

/// Token usage from field 28 groups: the "Token Usage" group carries `input_tokens`,
/// `output_tokens` and `cached_input_tokens` as float32 dimensions. Each entry may be a group
/// payload or an envelope holding tag 28. Returns `(prompt, completion, cached, found)`.
pub fn parse_response_dimension_groups<T: AsRef<[u8]>>(groups: &[T]) -> (i64, i64, i64, bool) {
    let (mut prompt, mut completion, mut cached, mut found) = (0i64, 0i64, 0i64, false);
    for g in groups {
        let mut g_bytes = g.as_ref();
        if g_bytes.is_empty() {
            continue;
        }
        // Unwrap an outer envelope carrying tag 28.
        if let Some((28, pb::BYTES, n)) = pb::get_tag(g_bytes)
            && let Some((inner, _)) = pb::get_bytes(&g_bytes[n..])
        {
            g_bytes = inner;
        }
        let (title, metrics) = parse_dimension_group(g_bytes);
        if title.eq_ignore_ascii_case("Token Usage") {
            for (key, val) in metrics {
                match key.as_str() {
                    "input_tokens" => {
                        prompt = val as i64;
                        found = true;
                    }
                    "output_tokens" => {
                        completion = val as i64;
                        found = true;
                    }
                    "cached_input_tokens" => {
                        cached = val as i64;
                        found = true;
                    }
                    _ => {}
                }
            }
            if found {
                return (prompt, completion, cached, true);
            }
        }
    }
    (prompt, completion, cached, found)
}

/// `(title, [(metric key, float32 value)])` of one dimension group.
fn parse_dimension_group(data: &[u8]) -> (String, Vec<(String, f32)>) {
    let mut title = String::new();
    let mut metrics = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if typ != pb::BYTES {
            let Some(skip) = pb::skip_field(num, typ, &data[pos..]) else {
                break;
            };
            pos += skip;
            continue;
        }
        let Some((gb, gbn)) = pb::get_bytes(&data[pos..]) else {
            break;
        };
        pos += gbn;
        match num {
            1 => title = lossy(gb),
            2 => {
                let (key, val) = parse_dimension_metric(gb);
                if !key.is_empty() {
                    metrics.push((key, val));
                }
            }
            _ => {}
        }
    }
    (title, metrics)
}

fn parse_dimension_metric(data: &[u8]) -> (String, f32) {
    let (mut key, mut val) = (String::new(), 0f32);
    let mut pos = 0;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if typ != pb::BYTES {
            let Some(skip) = pb::skip_field(num, typ, &data[pos..]) else {
                break;
            };
            pos += skip;
            continue;
        }
        let Some((mb, mbn)) = pb::get_bytes(&data[pos..]) else {
            break;
        };
        pos += mbn;
        match num {
            5 => key = lossy(mb),
            4 => val = parse_dimension_value(mb).unwrap_or(val),
            _ => {}
        }
    }
    (key, val)
}

/// The fixed32 field 2 of a dimension message.
fn parse_dimension_value(data: &[u8]) -> Option<f32> {
    let mut pos = 0;
    let mut val = None;
    while pos < data.len() {
        let Some((num, typ, n)) = pb::get_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if typ == pb::FIXED32 {
            let Some(v) = pb::get_fixed32(&data[pos..]) else {
                break;
            };
            pos += 4;
            if num == 2 {
                val = Some(f32::from_bits(v));
            }
        } else {
            let Some(skip) = pb::skip_field(num, typ, &data[pos..]) else {
                break;
            };
            pos += skip;
        }
    }
    val
}

// ------------------------------------------------------------------ trailer errors

/// Connect end-stream error mapped to an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailerError {
    pub status: u16,
    /// `devin upstream error (<code>): <message>`.
    pub message: String,
}

/// Case-insensitive object key lookup (encoding/json field matching).
fn json_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Option<&'a serde_json::Value> {
    obj.get(name).or_else(|| {
        obj.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

/// A JSON string field; `Err(())` for a non-string, non-null value (json.Unmarshal type error).
fn json_string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<String, ()> {
    match json_field(obj, name) {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(()),
    }
}

/// Inspects an end-stream trailer. `None` when it is empty, `{}`, unparsable, or carries no
/// `error` object.
pub fn parse_trailer_error(payload: &[u8]) -> Option<TrailerError> {
    let trimmed = crate::helps::text::trim_space(payload);
    if trimmed.is_empty() || trimmed == b"{}" {
        return None;
    }
    let root: serde_json::Value = serde_json::from_slice(trimmed).ok()?;
    let err = json_field(root.as_object()?, "error")?.as_object()?;
    let code = json_string_field(err, "code").ok()?;
    let message = json_string_field(err, "message").ok()?;

    let code_lower = code.to_lowercase();
    let msg_lower = message.to_lowercase();
    let status = match code_lower.as_str() {
        "invalid_argument" if msg_lower.contains("internal error") => 502,
        "invalid_argument" => 400,
        "internal" => 502,
        "unauthenticated" => 401,
        "permission_denied" if msg_lower.contains("high demand") => 429,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "unavailable" => 503,
        "canceled" => 499,
        "deadline_exceeded" => 504,
        "failed_precondition"
            if ["quota", "credit", "acu", "exhausted", "limit"]
                .iter()
                .any(|w| msg_lower.contains(w)) =>
        {
            429
        }
        "failed_precondition" => 400,
        _ => 502,
    };
    Some(TrailerError {
        status,
        message: format!("devin upstream error ({code}): {message}"),
    })
}

// ------------------------------------------------------------------ UTF-8 handling

/// Go `string(b)` followed by JSON encoding: every invalid byte becomes U+FFFD (one per byte).
pub fn go_lossy(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut rest = b;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (valid, bad) = rest.split_at(e.valid_up_to());
                // from_utf8 only reports validity of this prefix.
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                let bad_len = e.error_len().unwrap_or(bad.len());
                out.extend(std::iter::repeat_n('\u{FFFD}', bad_len));
                rest = &bad[bad_len..];
            }
        }
    }
}

/// Size of the first rune of `b` (invalid bytes count as one), Go `utf8.DecodeRune` semantics.
/// The flag is true for an invalid encoding (`RuneError` with size 1).
fn decode_rune_size(b: &[u8]) -> (usize, bool) {
    let window = &b[..b.len().min(4)];
    match std::str::from_utf8(window) {
        Ok(s) => (s.chars().next().map_or(1, char::len_utf8), false),
        Err(e) if e.valid_up_to() > 0 => {
            // The first rune is complete; the error is further along.
            let s = std::str::from_utf8(&window[..e.valid_up_to()]).unwrap_or_default();
            (s.chars().next().map_or(1, char::len_utf8), false)
        }
        Err(_) => (1, true),
    }
}

/// Go `utf8.FullRune`: whether `b` starts with a complete encoding (or an invalid one that
/// decodes as a one-byte error).
fn full_rune(b: &[u8]) -> bool {
    let window = &b[..b.len().min(4)];
    match std::str::from_utf8(window) {
        Ok(_) => true,
        Err(e) => e.valid_up_to() > 0 || e.error_len().is_some(),
    }
}

/// Buffers an incomplete trailing UTF-8 sequence between chunks.
#[derive(Debug, Default)]
pub struct Utf8SplitBuffer {
    remainder: Vec<u8>,
}

impl Utf8SplitBuffer {
    /// Consumes `chunk` (after any pending remainder) and returns the complete text.
    pub fn feed(&mut self, chunk: &[u8]) -> String {
        let mut combined = std::mem::take(&mut self.remainder);
        combined.extend_from_slice(chunk);
        if combined.is_empty() {
            return String::new();
        }
        let mut valid_until = 0;
        while valid_until < combined.len() {
            let rest = &combined[valid_until..];
            let (size, invalid) = decode_rune_size(rest);
            if invalid {
                if rest.len() < 4 && !full_rune(rest) {
                    break;
                }
                valid_until += 1;
                continue;
            }
            valid_until += size;
        }
        self.remainder = combined[valid_until..].to_vec();
        go_lossy(&combined[..valid_until])
    }
}

#[cfg(test)]
mod tests;
