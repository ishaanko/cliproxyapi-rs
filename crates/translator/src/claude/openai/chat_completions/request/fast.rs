//! Single-pass OpenAI Chat -> Claude request translation for canonical bodies.
//!
//! The body is deserialized once into borrowed structs (message and tool text stay as raw JSON
//! literals) and the Claude request is written straight into one buffer; string literals that are
//! already in canonical form are copied with a memcpy. Only the shapes this file understands are
//! accepted: anything else (cache_control, response_format, reasoning, file parts, null or
//! odd-typed fields, ...) returns `None` and the general `Value` based conversion in the parent
//! module runs, which remains the reference for every other input. A differential test compares
//! both on generated bodies.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use cpa_core::util::{normalize_claude_tool_input_schema, sanitize_claude_function_name, sanitize_claude_tool_id};
use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use crate::common::fast::{decode_literal, is_string_literal, push_int, push_json_str, push_literal, Field, Str};

type Raw<'a> = &'a RawValue;

#[derive(Deserialize)]
struct Request<'a> {
    #[serde(default, borrow)]
    model: Field<Raw<'a>>,
    #[serde(default, borrow)]
    messages: Field<Vec<Message<'a>>>,
    #[serde(default, borrow)]
    tools: Field<Vec<Tool<'a>>>,
    #[serde(default, borrow)]
    tool_choice: Field<Raw<'a>>,
    #[serde(default, borrow)]
    parallel_tool_calls: Field<Raw<'a>>,
    #[serde(default, borrow)]
    max_tokens: Field<Raw<'a>>,
    #[serde(default, borrow)]
    max_completion_tokens: Field<Raw<'a>>,
    #[serde(default, borrow)]
    top_p: Field<Raw<'a>>,
    #[serde(default, borrow)]
    stop: Field<Raw<'a>>,
    #[serde(default, borrow)]
    user: Field<Raw<'a>>,
    // Keys the general path reads (user id seeds, reasoning, structured output, summary intent):
    // when any is present the general path decides.
    #[serde(default)]
    metadata: Field<IgnoredAny>,
    #[serde(default)]
    prompt_cache_key: Field<IgnoredAny>,
    #[serde(default)]
    session_id: Field<IgnoredAny>,
    #[serde(default, rename = "sessionId")]
    session_id_camel: Field<IgnoredAny>,
    #[serde(default)]
    conversation: Field<IgnoredAny>,
    #[serde(default)]
    conversation_id: Field<IgnoredAny>,
    #[serde(default)]
    input: Field<IgnoredAny>,
    #[serde(default)]
    contents: Field<IgnoredAny>,
    #[serde(default)]
    instructions: Field<IgnoredAny>,
    #[serde(default)]
    system: Field<IgnoredAny>,
    #[serde(default, rename = "systemInstruction")]
    system_instruction_camel: Field<IgnoredAny>,
    #[serde(default)]
    system_instruction: Field<IgnoredAny>,
    #[serde(default)]
    reasoning_effort: Field<IgnoredAny>,
    #[serde(default)]
    response_format: Field<IgnoredAny>,
    #[serde(default)]
    extra_body: Field<IgnoredAny>,
    #[serde(default)]
    google: Field<IgnoredAny>,
    #[serde(default)]
    thinking: Field<IgnoredAny>,
    #[serde(default)]
    reasoning: Field<IgnoredAny>,
    #[serde(default)]
    include_reasoning: Field<IgnoredAny>,
    #[serde(default, rename = "generationConfig")]
    generation_config_camel: Field<IgnoredAny>,
    #[serde(default)]
    generation_config: Field<IgnoredAny>,
    #[serde(default)]
    request: Field<IgnoredAny>,
}

impl Request<'_> {
    /// True when a key only the general path understands is present.
    fn needs_general_path(&self) -> bool {
        [
            self.metadata.exists(),
            self.prompt_cache_key.exists(),
            self.session_id.exists(),
            self.session_id_camel.exists(),
            self.conversation.exists(),
            self.conversation_id.exists(),
            self.input.exists(),
            self.contents.exists(),
            self.instructions.exists(),
            self.system.exists(),
            self.system_instruction_camel.exists(),
            self.system_instruction.exists(),
            self.reasoning_effort.exists(),
            self.response_format.exists(),
            self.extra_body.exists(),
            self.google.exists(),
            self.thinking.exists(),
            self.reasoning.exists(),
            self.include_reasoning.exists(),
            self.generation_config_camel.exists(),
            self.generation_config.exists(),
            self.request.exists(),
        ]
        .contains(&true)
    }
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(default, borrow)]
    role: Field<Str<'a>>,
    #[serde(default, borrow)]
    content: Field<Raw<'a>>,
    #[serde(default, borrow)]
    tool_calls: Field<Vec<ToolCall<'a>>>,
    #[serde(default, borrow)]
    tool_call_id: Field<Str<'a>>,
    #[serde(default)]
    cache_control: Field<IgnoredAny>,
}

#[derive(Deserialize)]
struct ToolCall<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    id: Field<Str<'a>>,
    #[serde(default, borrow)]
    function: Field<CallFunction<'a>>,
}

#[derive(Deserialize)]
struct CallFunction<'a> {
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
    #[serde(default, borrow)]
    arguments: Field<Raw<'a>>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    text: Field<Raw<'a>>,
    #[serde(default, borrow)]
    image_url: Field<ImageUrl<'a>>,
    #[serde(default)]
    cache_control: Field<IgnoredAny>,
}

#[derive(Deserialize)]
struct ImageUrl<'a> {
    #[serde(default, borrow)]
    url: Field<Str<'a>>,
}

#[derive(Deserialize)]
struct Tool<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    function: Field<ToolFunction<'a>>,
    #[serde(default, borrow)]
    strict: Field<Raw<'a>>,
    #[serde(default)]
    cache_control: Field<IgnoredAny>,
}

#[derive(Deserialize)]
struct ToolFunction<'a> {
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
    #[serde(default, borrow)]
    description: Field<Raw<'a>>,
    #[serde(default, borrow)]
    parameters: Field<Raw<'a>>,
    #[serde(default, rename = "parametersJsonSchema", borrow)]
    parameters_json_schema: Field<Raw<'a>>,
    #[serde(default, borrow)]
    strict: Field<Raw<'a>>,
    #[serde(default)]
    cache_control: Field<IgnoredAny>,
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    function: Field<ChoiceFunction<'a>>,
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
}

#[derive(Deserialize)]
struct ChoiceFunction<'a> {
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
}

/// A message's `content` classified by its JSON type.
enum Content<'a> {
    Absent,
    /// A string literal, quotes included.
    Text(&'a str),
    /// An array's raw text; callers parse the elements they need.
    Array(&'a str),
    /// null, number, bool or object: contributes no blocks.
    Other,
}

fn content<'a>(raw: &Field<Raw<'a>>) -> Content<'a> {
    let Some(raw) = raw.as_ref() else { return Content::Absent };
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'"') => Content::Text(text),
        Some(b'[') => Content::Array(text),
        _ => Content::Other,
    }
}

/// The unescaped text of an optional string field; `None` (decline) for a present non-string.
fn opt_string<'a>(raw: &Field<Raw<'a>>) -> Option<Option<Cow<'a, str>>> {
    match raw.as_ref() {
        None => Some(None),
        Some(r) if is_string_literal(r.get()) => Some(Some(decode_literal(r.get())?)),
        Some(_) => None,
    }
}

/// Appends `{"type":"text","text":<literal>}`.
fn push_text_block(out: &mut Vec<u8>, lit: &str) -> Option<()> {
    out.extend_from_slice(br#"{"type":"text","text":"#);
    push_literal(out, lit)?;
    out.push(b'}');
    Some(())
}

/// A text literal for a `text` field: the field's own literal, `""` when absent, `None` otherwise.
fn text_literal<'a>(raw: &Field<Raw<'a>>) -> Option<&'a str> {
    match raw.as_ref() {
        None => Some(r#""""#),
        Some(r) if is_string_literal(r.get()) => Some(r.get()),
        Some(_) => None,
    }
}

/// Appends the Claude block for a user/assistant/tool content part. `Ok(false)` = part skipped.
fn push_part(out: &mut Vec<u8>, part: &Part<'_>) -> Option<bool> {
    if part.cache_control.exists() {
        return None;
    }
    match part.ty.as_ref().map(|t| &**t) {
        Some("text") => {
            push_text_block(out, text_literal(&part.text)?)?;
            Some(true)
        }
        Some("image_url") => {
            let url = part.image_url.as_ref().and_then(|i| i.url.as_ref()).map(|u| &**u).unwrap_or("");
            Some(push_image(out, url))
        }
        Some("file") => None,
        _ => Some(false),
    }
}

/// Go `convertImageURL`: false when the url is empty or an unusable data URL.
fn push_image(out: &mut Vec<u8>, url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    if url.starts_with("data:") {
        let Some((head, data)) = url.split_once(',') else { return false };
        let media_part = head.split(';').next().unwrap_or(head);
        let mut media_type = media_part.strip_prefix("data:").unwrap_or(media_part);
        if media_type.is_empty() {
            media_type = "application/octet-stream";
        }
        out.extend_from_slice(br#"{"type":"image","source":{"type":"base64","media_type":"#);
        push_json_str(out, media_type);
        out.extend_from_slice(br#","data":"#);
        push_json_str(out, data);
        out.extend_from_slice(b"}}");
    } else {
        out.extend_from_slice(br#"{"type":"image","source":{"type":"url","url":"#);
        push_json_str(out, url);
        out.extend_from_slice(b"}}");
    }
    true
}

/// Comma-separated JSON items.
#[derive(Default)]
struct List {
    buf: Vec<u8>,
    n: usize,
}

impl List {
    /// Appends one item written by `f`; `f` returning `Some(false)` drops it again.
    fn push(&mut self, f: impl FnOnce(&mut Vec<u8>) -> Option<bool>) -> Option<bool> {
        let start = self.buf.len();
        if self.n > 0 {
            self.buf.push(b',');
        }
        match f(&mut self.buf) {
            Some(true) => {
                self.n += 1;
                Some(true)
            }
            other => {
                self.buf.truncate(start);
                other
            }
        }
    }
}

/// Consecutive messages of one role merge into one Claude message; assistant `tool_use` blocks
/// move behind the other blocks (Go: `ClaudeMessageAccumulator`).
#[derive(Default)]
struct Turns {
    out: List,
    role: Option<&'static str>,
    blocks: List,
    tool_uses: List,
}

impl Turns {
    /// Starts (or continues) a turn of `role` once the message produced at least one block.
    fn begin(&mut self, role: &'static str) {
        if self.role.is_some_and(|r| r != role) {
            self.flush();
        }
        self.role = Some(role);
    }

    fn flush(&mut self) {
        let Some(role) = self.role.take() else { return };
        let (blocks, tools) = (std::mem::take(&mut self.blocks), std::mem::take(&mut self.tool_uses));
        if blocks.n + tools.n == 0 {
            return;
        }
        let _ = self.out.push(|o| {
            o.extend_from_slice(br#"{"role":""#);
            o.extend_from_slice(role.as_bytes());
            o.extend_from_slice(br#"","content":["#);
            o.extend_from_slice(&blocks.buf);
            if blocks.n > 0 && tools.n > 0 {
                o.push(b',');
            }
            o.extend_from_slice(&tools.buf);
            o.extend_from_slice(b"]}");
            Some(true)
        });
    }
}

/// The user id Claude gets in `metadata.user_id` (Go: `DeriveClaudeUserID`) for a body without any
/// of the explicit id/seed keys (those make the request ineligible for this path).
fn derive_user_id(req: &Request<'_>, messages: &[Message<'_>]) -> Option<String> {
    // `user` counts only as a non-blank string; other types are skipped.
    if let Some(raw) = req.user.as_ref()
        && is_string_literal(raw.get())
    {
        let user = decode_literal(raw.get())?;
        if !user.trim().is_empty() {
            return Some(user.into_owned());
        }
    }

    let mut seed = String::new();
    if let Some(text) = first_user_text(messages)? {
        seed.push_str("content:");
        seed.push_str(&text);
    }
    if seed.is_empty()
        && let Some(model) = opt_string(&req.model)?
    {
        let model = model.trim();
        if !model.is_empty() {
            seed.push_str("model:");
            seed.push_str(model);
        }
    }
    if seed.is_empty() {
        return Some("unknown".into());
    }
    Some(hex::encode(Sha256::digest(seed.as_bytes())))
}

/// The first non-empty trimmed text of a `user` message (strings, or `text` parts joined by
/// newlines), mirroring `first_stable_request_content` for chat messages. `None` = decline.
fn first_user_text(messages: &[Message<'_>]) -> Option<Option<String>> {
    for message in messages {
        let role = message.role.as_ref().map(|r| r.trim().to_lowercase()).unwrap_or_default();
        if role != "user" {
            continue;
        }
        let text = match content(&message.content) {
            Content::Text(lit) => decode_literal(lit)?.trim().to_string(),
            Content::Array(arr) => {
                let parts: Vec<Part<'_>> = serde_json::from_str(arr).ok()?;
                let mut texts: Vec<String> = Vec::new();
                for part in &parts {
                    if part.ty.as_ref().is_none_or(|t| &**t != "text") {
                        continue;
                    }
                    let Some(raw) = part.text.as_ref() else { continue };
                    if !is_string_literal(raw.get()) {
                        return None;
                    }
                    let t = decode_literal(raw.get())?.trim().to_string();
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
                texts.join("\n").trim().to_string()
            }
            Content::Absent | Content::Other => String::new(),
        };
        if !text.is_empty() {
            return Some(Some(text));
        }
    }
    Some(None)
}

/// Integer value of a JSON number literal, `None` for anything else (declines).
fn int_literal(raw: &str) -> Option<i64> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Converts `raw` when it is a canonical body; `None` hands the request to the general path.
pub(super) fn convert(model_name: &str, raw: &[u8], stream: bool) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(raw).ok()?;
    let req: Request<'_> = serde_json::from_str(text).ok()?;
    if req.needs_general_path() {
        return None;
    }
    let messages: &[Message<'_>] = req.messages.as_ref().map_or(&[], |m| m.as_slice());
    let user_id = derive_user_id(&req, messages)?;

    // Last `tool` message per tool_call_id: duplicates collapse onto the first position with the
    // content of the last.
    let mut last_tool: HashMap<&str, usize> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role.as_ref().is_some_and(|r| &**r == "tool")
            && let Some(id) = m.tool_call_id.as_ref().filter(|id| !id.is_empty())
        {
            last_tool.insert(id, i);
        }
    }

    let mut system = List::default();
    let mut turns = Turns::default();
    let mut emitted: HashSet<&str> = HashSet::new();

    for message in messages {
        if message.cache_control.exists() {
            return None;
        }
        let role = message.role.as_ref().map(|r| &**r).unwrap_or("");
        match role {
            "system" | "developer" => match content(&message.content) {
                Content::Text(lit) if lit != r#""""# => {
                    system.push(|o| push_text_block(o, lit).map(|()| true))?;
                }
                Content::Array(arr) => {
                    let parts: Vec<Part<'_>> = serde_json::from_str(arr).ok()?;
                    for part in &parts {
                        if part.cache_control.exists() {
                            return None;
                        }
                        if part.ty.as_ref().is_some_and(|t| &**t == "text") {
                            let lit = text_literal(&part.text)?;
                            system.push(|o| push_text_block(o, lit).map(|()| true))?;
                        }
                    }
                }
                _ => {}
            },
            "user" | "assistant" => {
                let static_role = if role == "user" { "user" } else { "assistant" };
                let mut blocks = List::default();
                match content(&message.content) {
                    Content::Text(lit) if lit != r#""""# => {
                        blocks.push(|o| push_text_block(o, lit).map(|()| true))?;
                    }
                    Content::Array(arr) => {
                        let parts: Vec<Part<'_>> = serde_json::from_str(arr).ok()?;
                        for part in &parts {
                            blocks.push(|o| push_part(o, part))?;
                        }
                    }
                    _ => {}
                }

                let mut tool_uses = List::default();
                if role == "assistant"
                    && let Some(calls) = message.tool_calls.as_ref()
                {
                    for call in calls {
                        if call.ty.as_ref().is_none_or(|t| &**t != "function") {
                            continue;
                        }
                        let id = call.id.as_ref().filter(|id| !id.is_empty())?;
                        let function = call.function.as_ref();
                        let name = function.and_then(|f| f.name.as_ref()).map(|n| &**n).unwrap_or("");
                        let args = function.map(|f| &f.arguments);
                        tool_uses.push(|o| {
                            o.extend_from_slice(br#"{"type":"tool_use","id":"#);
                            push_json_str(o, &sanitize_claude_tool_id(id));
                            o.extend_from_slice(br#","name":"#);
                            push_json_str(o, &sanitize_claude_function_name(name));
                            o.extend_from_slice(br#","input":"#);
                            push_tool_input(o, args)?;
                            o.push(b'}');
                            Some(true)
                        })?;
                    }
                }

                if blocks.n + tool_uses.n > 0 {
                    turns.begin(static_role);
                    append(&mut turns.blocks, &blocks);
                    append(&mut turns.tool_uses, &tool_uses);
                }
            }
            "tool" => {
                let raw_id = message.tool_call_id.as_ref().map(|s| &**s).unwrap_or("");
                if raw_id.is_empty() {
                    return None;
                }
                if !emitted.insert(raw_id) {
                    continue;
                }
                let target = messages.get(*last_tool.get(raw_id)?)?;
                if target.cache_control.exists() {
                    return None;
                }
                turns.begin("user");
                turns.blocks.push(|o| {
                    o.extend_from_slice(br#"{"type":"tool_result","tool_use_id":"#);
                    push_json_str(o, &sanitize_claude_tool_id(raw_id));
                    o.extend_from_slice(br#","content":"#);
                    push_tool_result_content(o, &target.content)?;
                    o.push(b'}');
                    Some(true)
                })?;
            }
            _ => {}
        }
    }
    turns.flush();

    // System-only inputs keep a minimal conversational turn.
    if turns.out.n == 0 && system.n > 0 {
        turns.out.push(|o| {
            o.extend_from_slice(br#"{"role":"user","content":[{"type":"text","text":""}]}"#);
            Some(true)
        })?;
    }

    let mut out = Vec::with_capacity(text.len() + text.len() / 8 + 512);
    out.extend_from_slice(br#"{"model":"#);
    push_json_str(&mut out, model_name);
    out.extend_from_slice(br#","max_tokens":"#);
    let max_tokens = [&req.max_tokens, &req.max_completion_tokens].into_iter().find_map(Field::as_ref);
    match max_tokens {
        Some(raw) => push_int(&mut out, int_literal(raw.get())?),
        None => out.extend_from_slice(b"32000"),
    }
    out.extend_from_slice(br#","messages":["#);
    out.extend_from_slice(&turns.out.buf);
    out.extend_from_slice(br#"],"metadata":{"user_id":"#);
    push_json_str(&mut out, &user_id);
    out.push(b'}');

    if let Some(top_p) = req.top_p.as_ref() {
        let text = top_p.get();
        let first = *text.as_bytes().first()?;
        if !(first == b'-' || first.is_ascii_digit()) {
            return None;
        }
        let f: f64 = text.parse().ok()?;
        if !f.is_finite() {
            return None;
        }
        out.extend_from_slice(br#","top_p":"#);
        out.extend_from_slice(cpa_json::format_float(f).as_bytes());
    }

    if let Some(stop) = req.stop.as_ref() {
        let text = stop.get();
        match text.as_bytes().first()? {
            b'"' => {
                out.extend_from_slice(br#","stop_sequences":["#);
                push_literal(&mut out, text)?;
                out.push(b']');
            }
            b'[' => {
                let items: Vec<Raw<'_>> = serde_json::from_str(text).ok()?;
                if !items.is_empty() {
                    out.extend_from_slice(br#","stop_sequences":["#);
                    for (i, item) in items.iter().enumerate() {
                        if !is_string_literal(item.get()) {
                            return None;
                        }
                        if i > 0 {
                            out.push(b',');
                        }
                        push_literal(&mut out, item.get())?;
                    }
                    out.push(b']');
                }
            }
            _ => return None,
        }
    }

    out.extend_from_slice(if stream { br#","stream":true"# } else { br#","stream":false"# });

    if system.n > 0 {
        out.extend_from_slice(br#","system":["#);
        out.extend_from_slice(&system.buf);
        out.push(b']');
    }

    let tools_written = push_tools(&mut out, &req)?;
    push_tool_choice(&mut out, &req, tools_written)?;
    out.push(b'}');
    Some(out)
}

/// Appends every item of `src` to `dst`.
fn append(dst: &mut List, src: &List) {
    if src.n == 0 {
        return;
    }
    if dst.n > 0 {
        dst.buf.push(b',');
    }
    dst.buf.extend_from_slice(&src.buf);
    dst.n += src.n;
}

/// `tool_use.input`: the arguments when they are a valid JSON object, else `{}`.
fn push_tool_input(out: &mut Vec<u8>, args: Option<&Field<Raw<'_>>>) -> Option<()> {
    let Some(raw) = args.and_then(Field::as_ref) else {
        out.extend_from_slice(b"{}");
        return Some(());
    };
    if !is_string_literal(raw.get()) {
        return None;
    }
    let args = decode_literal(raw.get())?;
    if !args.is_empty() && cpa_json::valid(args.as_bytes()) {
        let value = cpa_json::parse_str(&args);
        if value.is_object() {
            out.extend_from_slice(&cpa_json::to_vec(&value));
            return Some(());
        }
    }
    out.extend_from_slice(b"{}");
    Some(())
}

/// `tool_result.content`: a string, or an array of Claude parts (Go: `convertToolResultContent`).
fn push_tool_result_content(out: &mut Vec<u8>, raw: &Field<Raw<'_>>) -> Option<()> {
    match content(raw) {
        Content::Absent => out.extend_from_slice(br#""""#),
        Content::Text(lit) => push_literal(out, lit)?,
        Content::Array(arr) => {
            // Elements may be bare strings or part objects.
            let items: Vec<Raw<'_>> = serde_json::from_str(arr).ok()?;
            let mut parts = List::default();
            for item in &items {
                let text = item.get();
                match text.as_bytes().first()? {
                    b'"' => {
                        parts.push(|o| push_text_block(o, text).map(|()| true))?;
                    }
                    b'{' => {
                        let part: Part<'_> = serde_json::from_str(text).ok()?;
                        parts.push(|o| push_part(o, &part))?;
                    }
                    _ => return None,
                }
            }
            if parts.n == 0 && !items.is_empty() {
                return None;
            }
            out.push(b'[');
            out.extend_from_slice(&parts.buf);
            out.push(b']');
        }
        Content::Other => return None,
    }
    Some(())
}

/// Appends `,"tools":[...]` when at least one function tool converts; reports whether it did.
fn push_tools(out: &mut Vec<u8>, req: &Request<'_>) -> Option<bool> {
    let Some(tools) = req.tools.as_ref() else { return Some(false) };
    let mut list = List::default();
    for tool in tools {
        if tool.ty.as_ref().is_none_or(|t| &**t != "function") {
            continue;
        }
        if tool.cache_control.exists() {
            return None;
        }
        let function = tool.function.as_ref();
        if function.is_some_and(|f| f.cache_control.exists()) {
            return None;
        }
        let name = function.and_then(|f| f.name.as_ref()).map(|n| &**n).unwrap_or("");
        let description = function.map_or(Some(r#""""#), |f| text_literal(&f.description))?;
        let parameters = function.and_then(|f| f.parameters.as_ref().or(f.parameters_json_schema.as_ref()));
        let strict = function.and_then(|f| f.strict.as_ref()).or(tool.strict.as_ref());
        list.push(|o| {
            o.extend_from_slice(br#"{"name":"#);
            push_json_str(o, &sanitize_claude_function_name(name));
            o.extend_from_slice(br#","description":"#);
            push_literal(o, description)?;
            o.extend_from_slice(br#","input_schema":"#);
            let schema = normalize_claude_tool_input_schema(parameters.map_or("", |p| p.get()).as_bytes());
            o.extend_from_slice(&schema);
            match strict.map(|s| s.get()) {
                Some("true") => o.extend_from_slice(br#","strict":true"#),
                Some("false") => o.extend_from_slice(br#","strict":false"#),
                _ => {}
            }
            o.push(b'}');
            Some(true)
        })?;
    }
    if list.n == 0 {
        return Some(false);
    }
    out.extend_from_slice(br#","tools":["#);
    out.extend_from_slice(&list.buf);
    out.push(b']');
    Some(true)
}

/// What `tool_choice` maps to.
enum ChoiceKind {
    None,
    Auto,
    Any,
    Tool(String),
}

/// Appends `,"tool_choice":{...}` per the mapping and `parallel_tool_calls: false`.
fn push_tool_choice(out: &mut Vec<u8>, req: &Request<'_>, has_tools: bool) -> Option<()> {
    let mut kind: Option<ChoiceKind> = None;
    if let Some(raw) = req.tool_choice.as_ref() {
        let text = raw.get();
        kind = match text.as_bytes().first()? {
            b'"' => match &*decode_literal(text)? {
                "none" => Some(ChoiceKind::None),
                "auto" => Some(ChoiceKind::Auto),
                "required" => Some(ChoiceKind::Any),
                _ => None,
            },
            // null, numbers and booleans select nothing.
            b'n' | b't' | b'f' | b'-' | b'0'..=b'9' => None,
            b'{' => {
                let choice: Choice<'_> = serde_json::from_str(text).ok()?;
                match choice.ty.as_ref().map(|t| &**t).unwrap_or("") {
                    "allowed_tools" => return None,
                    "none" => Some(ChoiceKind::None),
                    "auto" => Some(ChoiceKind::Auto),
                    "required" | "any" => Some(ChoiceKind::Any),
                    "function" => {
                        let mut name = choice.function.as_ref().and_then(|f| f.name.as_ref()).map(|n| n.to_string()).unwrap_or_default();
                        if name.is_empty() {
                            name = choice.name.as_ref().map(|n| n.to_string()).unwrap_or_default();
                        }
                        Some(if name.is_empty() { ChoiceKind::None } else { ChoiceKind::Tool(name) })
                    }
                    _ => None,
                }
            }
            _ => return None,
        };
    }
    // Without any tools the mapping still applies when the client sent a choice (the general
    // path only forces `none` for allowed_tools).
    let parallel_off = req.parallel_tool_calls.as_ref().is_some_and(|p| p.get() == "false");
    let (kind, disable) = match (kind, parallel_off) {
        (Some(k), off) => {
            let disable = off && !matches!(k, ChoiceKind::None);
            (Some(k), disable)
        }
        (None, true) if has_tools => (Some(ChoiceKind::Auto), true),
        (None, _) => (None, false),
    };
    let Some(kind) = kind else { return Some(()) };
    out.extend_from_slice(br#","tool_choice":{"type":"#);
    match &kind {
        ChoiceKind::None => out.extend_from_slice(br#""none""#),
        ChoiceKind::Auto => out.extend_from_slice(br#""auto""#),
        ChoiceKind::Any => out.extend_from_slice(br#""any""#),
        ChoiceKind::Tool(name) => {
            out.extend_from_slice(br#""tool","name":"#);
            push_json_str(out, &sanitize_claude_function_name(name));
        }
    }
    if disable {
        out.extend_from_slice(br#","disable_parallel_tool_use":true"#);
    }
    out.push(b'}');
    Some(())
}
