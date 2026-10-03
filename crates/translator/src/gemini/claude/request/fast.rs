//! Single-pass Claude Messages -> Gemini request translation for canonical bodies.
//!
//! The bulk of a large request is `messages`; those are deserialized once into borrowed structs
//! (text stays as raw JSON literals, copied verbatim when canonical) and the Gemini `contents` are
//! written straight into a buffer. Everything small (system instruction, tools, tool choice,
//! thinking, sampling) is delegated to the very same functions the general conversion uses, run on
//! a miniature root document, and the two are spliced. Shapes this file does not understand return
//! `None` and the general `Value` conversion in the parent module runs; a differential test
//! compares both on generated bodies.

use std::collections::HashMap;

use cpa_core::util::{convert_claude_tool_result_content, sanitize_function_name};
use cpa_json::{Res, Value};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    apply_system_instruction, apply_tail, tool_name_from_claude_tool_use_id, GEMINI_CLAUDE_THOUGHT_SIGNATURE,
};
use crate::common::claude_message_system_reminder_text;
use crate::common::fast::{decode_literal, is_string_literal, push_json_str, push_literal, Field, Str};
use crate::gemini::common::default_safety_settings;

type Raw<'a> = &'a RawValue;

#[derive(Deserialize)]
struct Request<'a> {
    #[serde(default, borrow)]
    messages: Field<Vec<Message<'a>>>,
    #[serde(default, borrow)]
    system: Field<Raw<'a>>,
    #[serde(default, borrow)]
    tools: Field<Raw<'a>>,
    #[serde(default, borrow)]
    tool_choice: Field<Raw<'a>>,
    #[serde(default, borrow)]
    thinking: Field<Raw<'a>>,
    #[serde(default, borrow)]
    output_config: Field<Raw<'a>>,
    #[serde(default, borrow)]
    temperature: Field<Raw<'a>>,
    #[serde(default, borrow)]
    top_p: Field<Raw<'a>>,
    #[serde(default, borrow)]
    top_k: Field<Raw<'a>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(default, borrow)]
    role: Field<Raw<'a>>,
    #[serde(default, borrow)]
    content: Field<Raw<'a>>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    text: Field<Raw<'a>>,
    #[serde(default, borrow)]
    name: Field<Str<'a>>,
    #[serde(default, borrow)]
    id: Field<Str<'a>>,
    #[serde(default, borrow)]
    input: Field<Raw<'a>>,
    #[serde(default, borrow)]
    tool_use_id: Field<Str<'a>>,
    #[serde(default, borrow)]
    content: Field<Raw<'a>>,
    #[serde(default, borrow)]
    source: Field<Source<'a>>,
}

#[derive(Deserialize)]
struct Source<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Field<Str<'a>>,
    #[serde(default, borrow)]
    media_type: Field<Str<'a>>,
    #[serde(default, borrow)]
    data: Field<Str<'a>>,
}

impl<'a> Block<'a> {
    fn ty(&self) -> &str {
        self.ty.as_ref().map_or("", |t| &**t)
    }

    fn tool_use_id(&self) -> &str {
        self.tool_use_id.as_ref().map_or("", |t| &**t)
    }
}

/// What a Gemini part holds; reordering and the trailing-call check only need the kind.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    FunctionCall,
    FunctionResponse,
    InlineData,
}

/// A part's bytes: `buf[start..end]` of the turn at `turn`.
#[derive(Clone, Copy)]
struct PartRef {
    kind: Kind,
    turn: usize,
    start: usize,
    end: usize,
}

/// One content turn under construction: parts are written to `buf` and listed in `parts`.
struct Turn {
    role: &'static str,
    buf: Vec<u8>,
    parts: Vec<PartRef>,
}

impl Turn {
    fn new(role: &'static str, index: usize) -> (Self, usize) {
        (Turn { role, buf: Vec::new(), parts: Vec::new() }, index)
    }

    /// Writes one part with `f` and records it; `f` returning `None` declines the whole request.
    fn part(&mut self, index: usize, kind: Kind, f: impl FnOnce(&mut Vec<u8>) -> Option<()>) -> Option<()> {
        let start = self.buf.len();
        f(&mut self.buf)?;
        self.parts.push(PartRef { kind, turn: index, start, end: self.buf.len() });
        Some(())
    }
}

/// Go `reorderGeminiUserParts`: text parts move in front of functionResponse parts, but only when
/// some text follows a functionResponse.
fn reorder(parts: &mut Vec<PartRef>) {
    let mut has_fr = false;
    let mut trailing_text = false;
    for p in parts.iter() {
        match p.kind {
            Kind::FunctionResponse => has_fr = true,
            Kind::Text if has_fr => {
                trailing_text = true;
                break;
            }
            _ => {}
        }
    }
    if has_fr && trailing_text {
        let (mut text, rest): (Vec<PartRef>, Vec<PartRef>) = parts.iter().copied().partition(|p| p.kind == Kind::Text);
        text.extend(rest);
        *parts = text;
    }
}

/// Go `AlignClaudeToolResults`: the order of block indices after sorting `tool_result` blocks to
/// the order of the preceding `tool_use` ids, or `None` to keep the blocks as they are.
fn align(blocks: &[Block<'_>], ids: &[String]) -> Option<Vec<usize>> {
    if ids.is_empty() {
        return None;
    }
    let results: Vec<usize> = blocks.iter().enumerate().filter(|(_, b)| b.ty() == "tool_result").map(|(i, _)| i).collect();
    if results.len() != ids.len() {
        return None;
    }
    let mut used = vec![false; results.len()];
    let mut picked = Vec::with_capacity(ids.len());
    for id in ids {
        let at = results.iter().enumerate().position(|(k, &i)| !used[k] && !id.is_empty() && blocks[i].tool_use_id() == id)?;
        used[at] = true;
        picked.push(results[at]);
    }
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    for (&slot, &result) in results.iter().zip(&picked) {
        order[slot] = result;
    }
    Some(order)
}

fn text_part(buf: &mut Vec<u8>, lit: &str) -> Option<()> {
    buf.extend_from_slice(br#"{"text":"#);
    push_literal(buf, lit)?;
    buf.push(b'}');
    Some(())
}

fn inline_data(buf: &mut Vec<u8>, mime: &str, data: &str) {
    buf.extend_from_slice(br#"{"inline_data":{"mime_type":"#);
    push_json_str(buf, mime);
    buf.extend_from_slice(br#","data":"#);
    push_json_str(buf, data);
    buf.extend_from_slice(b"}}");
}

/// Converts `raw` when it is a canonical body; `None` hands the request to the general path.
pub(super) fn convert(model_name: &str, raw: &[u8], _stream: bool) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(raw).ok()?;
    let req: Request<'_> = serde_json::from_str(text).ok()?;

    let messages: &[Message<'_>] = req.messages.as_ref().map_or(&[], |m| m.as_slice());
    let mut turns: Vec<Turn> = Vec::new();
    let mut tool_name_by_id: HashMap<String, String> = HashMap::new();
    let mut pending_tool_use_ids: Vec<String> = Vec::new();

    for message in messages {
        // Messages without a string role are skipped before anything else happens to them.
        let Some(role_raw) = message.role.as_ref() else { continue };
        if !is_string_literal(role_raw.get()) {
            continue;
        }
        let original_role = decode_literal(role_raw.get())?;
        let preceding_ids = match &*original_role {
            "system" | "developer" => Vec::new(),
            _ => std::mem::take(&mut pending_tool_use_ids),
        };

        let (role, is_user) = match &*original_role {
            "system" | "developer" => {
                // Mid-conversation system messages become one user reminder turn.
                let value: Option<Value> = match message.content.as_ref() {
                    Some(c) => Some(serde_json::from_str(c.get()).ok()?),
                    None => None,
                };
                let content = value.as_ref().map_or(Res::NONE, Res::of);
                if let Some(reminder) = claude_message_system_reminder_text(&content) {
                    let index = turns.len();
                    let (mut turn, _) = Turn::new("user", index);
                    turn.part(index, Kind::Text, |b| {
                        b.extend_from_slice(br#"{"text":"#);
                        push_json_str(b, &reminder);
                        b.push(b'}');
                        Some(())
                    })?;
                    turns.push(turn);
                }
                continue;
            }
            "user" => ("user", true),
            "assistant" => ("model", false),
            _ => return None,
        };

        let content = message.content.as_ref().map(|c| c.get());
        match content.and_then(|c| c.as_bytes().first()) {
            Some(b'[') => {
                let blocks: Vec<Block<'_>> = serde_json::from_str(content?).ok()?;
                let order = if is_user { align(&blocks, &preceding_ids) } else { None };
                let index = turns.len();
                let (mut turn, _) = Turn::new(role, index);
                for k in 0..blocks.len() {
                    let block = &blocks[order.as_ref().map_or(k, |o| o[k])];
                    push_block(&mut turn, index, block, &original_role, &mut tool_name_by_id, &mut pending_tool_use_ids)?;
                }
                if is_user {
                    reorder(&mut turn.parts);
                }
                turns.push(turn);
            }
            Some(b'"') => {
                let index = turns.len();
                let (mut turn, _) = Turn::new(role, index);
                turn.part(index, Kind::Text, |b| text_part(b, content.unwrap_or(r#""""#)))?;
                turns.push(turn);
            }
            // null, numbers, objects and absent content produce no turn.
            _ => {}
        }
    }

    // A trailing model turn with unanswered function calls is dropped.
    if turns.last().is_some_and(|t| t.role == "model" && t.parts.iter().any(|p| p.kind == Kind::FunctionCall)) {
        turns.pop();
    }

    let merged = merge(&turns);
    let mut contents = Vec::with_capacity(text.len() + text.len() / 8 + 256);
    for (n, (role, parts)) in merged.iter().enumerate() {
        if n > 0 {
            contents.push(b',');
        }
        contents.extend_from_slice(br#"{"role":""#);
        contents.extend_from_slice(role.as_bytes());
        contents.extend_from_slice(br#"","parts":["#);
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                contents.push(b',');
            }
            contents.extend_from_slice(&turns[p.turn].buf[p.start..p.end]);
        }
        contents.extend_from_slice(b"]}");
    }

    // Small parts through the shared general code, on a document holding only what they read.
    let mut mini = serde_json::Map::new();
    for (key, field) in [
        ("system", &req.system),
        ("tools", &req.tools),
        ("tool_choice", &req.tool_choice),
        ("thinking", &req.thinking),
        ("output_config", &req.output_config),
        ("temperature", &req.temperature),
        ("top_p", &req.top_p),
        ("top_k", &req.top_k),
    ] {
        if let Some(raw) = field.as_ref() {
            mini.insert(key.to_string(), serde_json::from_str(raw.get()).ok()?);
        }
    }
    let root = Value::Object(mini);
    let mut out = cpa_json::json!({ "contents": [] });
    cpa_json::set(&mut out, "model", model_name);
    apply_system_instruction(&mut out, &root);
    apply_tail(&mut out, &root, model_name);
    cpa_json::set(&mut out, "safetySettings", Value::Array(default_safety_settings()));
    let rest = cpa_json::to_vec(&out);

    // `rest` starts with the template's empty `contents`; swap in the real ones.
    const EMPTY: &[u8] = br#"{"contents":[]"#;
    let tail = rest.strip_prefix(EMPTY)?;
    if merged.is_empty() {
        return Some(rest);
    }
    let mut result = Vec::with_capacity(contents.len() + tail.len() + 16);
    result.extend_from_slice(br#"{"contents":["#);
    result.extend_from_slice(&contents);
    result.push(b']');
    result.extend_from_slice(tail);
    Some(result)
}

/// Go `MergeAdjacentGeminiContents` over the turns: consecutive user turns merge (parts
/// reordered), turns without parts are dropped (unless there is only one turn in total).
fn merge(turns: &[Turn]) -> Vec<(&'static str, Vec<PartRef>)> {
    if turns.len() <= 1 {
        return turns.iter().map(|t| (t.role, t.parts.clone())).collect();
    }
    let mut merged: Vec<(&'static str, Vec<PartRef>)> = Vec::with_capacity(turns.len());
    for t in turns {
        if t.parts.is_empty() {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && last.0 == "user"
            && t.role == "user"
        {
            last.1.extend_from_slice(&t.parts);
            reorder(&mut last.1);
            continue;
        }
        merged.push((t.role, t.parts.clone()));
    }
    merged
}

/// Writes the Gemini parts for one Claude content block into `turn`.
fn push_block(
    turn: &mut Turn,
    index: usize,
    block: &Block<'_>,
    original_role: &str,
    tool_name_by_id: &mut HashMap<String, String>,
    pending_tool_use_ids: &mut Vec<String>,
) -> Option<()> {
    match block.ty() {
        "text" => {
            let Some(raw) = block.text.as_ref() else { return Some(()) };
            let lit = raw.get();
            if !is_string_literal(lit) {
                return None;
            }
            if lit == r#""""# {
                return Some(());
            }
            turn.part(index, Kind::Text, |b| text_part(b, lit))
        }
        "tool_use" => {
            let name = block.name.as_ref().map_or("", |n| &**n);
            let id = block.id.as_ref().map_or("", |n| &**n);
            if !id.is_empty() && !name.is_empty() {
                tool_name_by_id.insert(id.to_string(), name.to_string());
            }
            let args_text = match block.input.as_ref() {
                None => return Some(()),
                Some(raw) => match raw.get().as_bytes().first()? {
                    b'{' => std::borrow::Cow::Borrowed(raw.get()),
                    b'"' => decode_literal(raw.get())?,
                    // Arrays, numbers and literals are never an object: no part.
                    _ => return Some(()),
                },
            };
            if !cpa_json::valid(args_text.as_bytes()) {
                return Some(());
            }
            let args = cpa_json::parse_str(&args_text);
            if !args.is_object() {
                return Some(());
            }
            turn.part(index, Kind::FunctionCall, |b| {
                b.extend_from_slice(br#"{"thoughtSignature":""#);
                b.extend_from_slice(GEMINI_CLAUDE_THOUGHT_SIGNATURE.as_bytes());
                b.extend_from_slice(br#"","functionCall":{"name":"#);
                push_json_str(b, &sanitize_function_name(name));
                b.extend_from_slice(br#","args":"#);
                b.extend_from_slice(&cpa_json::to_vec(&args));
                if !id.is_empty() {
                    b.extend_from_slice(br#","id":"#);
                    push_json_str(b, id);
                }
                b.extend_from_slice(b"}}");
                Some(())
            })?;
            if original_role == "assistant" {
                pending_tool_use_ids.push(id.to_string());
            }
            Some(())
        }
        "tool_result" => {
            let tool_call_id = block.tool_use_id();
            if tool_call_id.is_empty() {
                return Some(());
            }
            let mut func_name = tool_name_by_id.get(tool_call_id).cloned().unwrap_or_default();
            if func_name.is_empty() {
                func_name = tool_name_from_claude_tool_use_id(tool_call_id);
            }
            if func_name.is_empty() {
                func_name = tool_call_id.to_string();
            }
            let func_name = sanitize_function_name(&func_name);

            // The result: a string literal as is, anything else through the shared converter.
            enum Payload<'r> {
                Literal(&'r str),
                Text(String),
                Json(Vec<u8>),
            }
            let mut images = Vec::new();
            let result = match block.content.as_ref() {
                None => Payload::Literal(r#""""#),
                Some(raw) if is_string_literal(raw.get()) => Payload::Literal(raw.get()),
                Some(raw) => {
                    let value: Value = serde_json::from_str(raw.get()).ok()?;
                    let tr = convert_claude_tool_result_content(Some(&value));
                    images = tr.images;
                    if tr.result_is_raw {
                        // `$ref` results take the source-text route of the general path.
                        if tr.result.contains("$ref") {
                            return None;
                        }
                        let trimmed = tr.result.trim();
                        match if trimmed.is_empty() { None } else { serde_json::from_str::<Value>(trimmed).ok() } {
                            Some(v) => Payload::Json(cpa_json::to_vec(&v)),
                            None => Payload::Literal(r#""""#),
                        }
                    } else {
                        Payload::Text(tr.result)
                    }
                }
            };
            turn.part(index, Kind::FunctionResponse, |b| {
                b.extend_from_slice(br#"{"functionResponse":{"name":"#);
                push_json_str(b, &func_name);
                b.extend_from_slice(br#","response":{"result":"#);
                match &result {
                    Payload::Literal(lit) => push_literal(b, lit)?,
                    Payload::Text(s) => push_json_str(b, s),
                    Payload::Json(bytes) => b.extend_from_slice(bytes),
                }
                b.extend_from_slice(br#"},"id":"#);
                push_json_str(b, tool_call_id);
                b.extend_from_slice(b"}}");
                Some(())
            })?;
            for img in images {
                turn.part(index, Kind::InlineData, |b| {
                    inline_data(b, &img.mime_type, &img.data);
                    Some(())
                })?;
            }
            Some(())
        }
        "image" => {
            let Some(source) = block.source.as_ref() else { return Some(()) };
            if source.ty.as_ref().map_or("", |t| &**t) != "base64" {
                return Some(());
            }
            let mime = source.media_type.as_ref().map_or("", |t| &**t);
            let data = source.data.as_ref().map_or("", |t| &**t);
            if mime.is_empty() || data.is_empty() {
                return Some(());
            }
            turn.part(index, Kind::InlineData, |b| {
                inline_data(b, mime, data);
                Some(())
            })
        }
        _ => Some(()),
    }
}
