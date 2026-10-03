//! Allocation-light conversion of canonical Gemini stream chunks to OpenAI chunks (see
//! [`crate::common::fast`]): text and thought parts, usage metadata and finish reasons. Chunks
//! with function calls, inline data, audio transcripts or any odd shape return `None` before
//! touching state, and the general `Value` path in the parent module converts them.

use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::ChatParams;
use crate::common::fast::{decode_literal, is_string_literal, push_int, push_json_str, push_literal, Field};
use crate::common::parse_create_time;

type Raw<'a> = &'a RawValue;

#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    candidates: Field<Vec<Candidate<'a>>>,
    #[serde(default, rename = "usageMetadata")]
    usage: Field<Usage>,
    #[serde(default, rename = "modelVersion", borrow)]
    model_version: Field<Raw<'a>>,
    #[serde(default, rename = "createTime", borrow)]
    create_time: Field<Raw<'a>>,
    #[serde(default, rename = "responseId", borrow)]
    response_id: Field<Raw<'a>>,
}

#[derive(Deserialize)]
struct Candidate<'a> {
    #[serde(default, borrow)]
    index: Field<Raw<'a>>,
    #[serde(default, rename = "finishReason", borrow)]
    finish_reason: Field<Raw<'a>>,
    #[serde(default, borrow)]
    content: Field<Content<'a>>,
}

#[derive(Deserialize)]
struct Content<'a> {
    #[serde(default, borrow)]
    parts: Field<Vec<Part<'a>>>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(default, borrow)]
    text: Field<Raw<'a>>,
    #[serde(default, borrow)]
    thought: Field<Raw<'a>>,
    // Parts of these kinds need the general path.
    #[serde(default, rename = "functionCall")]
    function_call: Field<IgnoredAny>,
    #[serde(default, rename = "function_call")]
    function_call_snake: Field<IgnoredAny>,
    #[serde(default, rename = "inlineData")]
    inline_data: Field<IgnoredAny>,
    #[serde(default, rename = "inline_data")]
    inline_data_snake: Field<IgnoredAny>,
    #[serde(default, rename = "audioTranscription")]
    audio_transcription: Field<IgnoredAny>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts: Field<i64>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached: Field<i64>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates: Field<i64>,
    #[serde(default, rename = "totalTokenCount")]
    total: Field<i64>,
    #[serde(default, rename = "promptTokenCount")]
    prompt: Field<i64>,
}

/// Integer value of a JSON integer literal (`None` for anything else).
fn int_literal(raw: &str) -> Option<i64> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// `,"usage":{...}` as `set_usage` lays it out.
fn push_usage(out: &mut Vec<u8>, u: &Usage) {
    let thoughts = u.thoughts.as_ref().copied().unwrap_or(0);
    let cached = u.cached.as_ref().copied().unwrap_or(0);
    out.extend_from_slice(br#","usage":{"completion_tokens":"#);
    push_int(out, u.candidates.as_ref().copied().unwrap_or(0).wrapping_add(thoughts));
    if let Some(total) = u.total.as_ref() {
        out.extend_from_slice(br#","total_tokens":"#);
        push_int(out, *total);
    }
    out.extend_from_slice(br#","prompt_tokens":"#);
    push_int(out, u.prompt.as_ref().copied().unwrap_or(0));
    if thoughts > 0 {
        out.extend_from_slice(br#","completion_tokens_details":{"reasoning_tokens":"#);
        push_int(out, thoughts);
        out.push(b'}');
    }
    if cached > 0 {
        out.extend_from_slice(br#","prompt_tokens_details":{"cached_tokens":"#);
        push_int(out, cached);
        out.push(b'}');
    }
    out.push(b'}');
}

/// A string field's literal (`None` = decline for a non-string).
fn lit<'a>(f: &Field<Raw<'a>>) -> Option<Option<&'a str>> {
    match f.as_ref() {
        None => Some(None),
        Some(r) if is_string_literal(r.get()) => Some(Some(r.get())),
        Some(_) => None,
    }
}

pub(super) fn convert(p: &mut ChatParams, raw: &[u8]) -> Option<Vec<Vec<u8>>> {
    let text = std::str::from_utf8(raw).ok()?;
    if text.is_empty() {
        return Some(vec![]);
    }
    let chunk: Chunk<'_> = serde_json::from_str(text).ok()?;

    let model = lit(&chunk.model_version)?;
    let id = lit(&chunk.response_id)?;
    let create_time = match lit(&chunk.create_time)? {
        Some(l) => parse_create_time(&decode_literal(l)?),
        None => None,
    };
    let created = create_time.unwrap_or(p.unix_timestamp);

    // Everything before the candidate-specific part, and the usage tail.
    let mut head = Vec::with_capacity(160);
    head.extend_from_slice(br#"{"id":"#);
    match id {
        Some(l) => push_literal(&mut head, l)?,
        None => head.extend_from_slice(br#""""#),
    }
    head.extend_from_slice(br#","object":"chat.completion.chunk","created":"#);
    push_int(&mut head, created);
    head.extend_from_slice(br#","model":"#);
    match model {
        Some(l) => push_literal(&mut head, l)?,
        None => head.extend_from_slice(br#""model""#),
    }
    head.extend_from_slice(br#","choices":[{"index":"#);
    let mut usage_tail = Vec::new();
    if let Some(u) = chunk.usage.as_ref() {
        push_usage(&mut usage_tail, u);
    }
    usage_tail.push(b'}');
    let has_usage = chunk.usage.exists();

    // Candidate outputs are computed first; state changes are applied once the chunk is accepted.
    struct Planned {
        index: i64,
        finish: Option<String>,
        out: Vec<u8>,
    }
    let mut planned: Vec<Planned> = Vec::new();
    let candidates = chunk.candidates.as_ref();
    if let Some(candidates) = candidates {
        for candidate in candidates {
            let index = match candidate.index.as_ref() {
                None => 0,
                Some(r) => int_literal(r.get())?,
            };
            // Candidates sharing an index see each other's finish reason; leave that to the general path.
            if planned.iter().any(|pl| pl.index == index) {
                return None;
            }
            let finish = match lit(&candidate.finish_reason)? {
                Some(l) => Some(decode_literal(l)?.to_uppercase()),
                None => None,
            };

            let mut role = false;
            let (mut content, mut reasoning): (Option<&str>, Option<&str>) = (None, None);
            if let Some(c) = candidate.content.as_ref()
                && let Some(parts) = c.parts.as_ref()
            {
                for part in parts {
                    if part.function_call.exists()
                        || part.function_call_snake.exists()
                        || part.inline_data.exists()
                        || part.inline_data_snake.exists()
                        || part.audio_transcription.exists()
                    {
                        return None;
                    }
                    let Some(t) = part.text.as_ref() else { continue };
                    if !is_string_literal(t.get()) {
                        return None;
                    }
                    let thought = match part.thought.as_ref().map(|r| r.get()) {
                        None | Some("false") => false,
                        Some("true") => true,
                        Some(_) => return None,
                    };
                    role = true;
                    if thought {
                        reasoning = Some(t.get());
                    } else {
                        content = Some(t.get());
                    }
                }
            }

            // The state this candidate sees after this chunk's finish reason is recorded.
            let known_finish = match &finish {
                Some(f) => f.clone(),
                None => p.upstream_finish_reason.get(&index).cloned().unwrap_or_default(),
            };
            let final_chunk = !known_finish.is_empty() && has_usage;
            let saw_tool_call = p.saw_tool_call.get(&index).copied().unwrap_or(false);

            let mut out = Vec::with_capacity(head.len() + 200 + content.map_or(0, str::len) + reasoning.map_or(0, str::len));
            out.extend_from_slice(&head);
            push_int(&mut out, index);
            out.extend_from_slice(br#","delta":{"role":"#);
            out.extend_from_slice(if role { br#""assistant""# } else { b"null" });
            out.extend_from_slice(br#","content":"#);
            match content {
                Some(l) => push_literal(&mut out, l)?,
                None => out.extend_from_slice(b"null"),
            }
            out.extend_from_slice(br#","reasoning_content":"#);
            match reasoning {
                Some(l) => push_literal(&mut out, l)?,
                None => out.extend_from_slice(b"null"),
            }
            out.extend_from_slice(br#","tool_calls":null},"finish_reason":"#);
            if final_chunk {
                let reason = if saw_tool_call {
                    "tool_calls"
                } else if known_finish == "MAX_TOKENS" {
                    "max_tokens"
                } else {
                    "stop"
                };
                push_json_str(&mut out, reason);
                out.extend_from_slice(br#","native_finish_reason":"#);
                push_json_str(&mut out, &known_finish.to_lowercase());
            } else {
                out.extend_from_slice(br#"null,"native_finish_reason":null"#);
            }
            out.extend_from_slice(b"}]");
            out.extend_from_slice(&usage_tail);
            planned.push(Planned { index, finish, out });
        }
    }

    // Accepted: apply state changes.
    if let Some(ts) = create_time {
        p.unix_timestamp = ts;
    }
    match candidates {
        Some(_) => Some(
            planned
                .into_iter()
                .map(|pl| {
                    if let Some(f) = pl.finish {
                        p.upstream_finish_reason.insert(pl.index, f);
                    }
                    pl.out
                })
                .collect(),
        ),
        None if has_usage => {
            // A pure usage chunk: the base template with the usage appended.
            let mut out = Vec::with_capacity(head.len() + 160);
            out.extend_from_slice(br#"{"id":"#);
            match id {
                Some(l) => push_literal(&mut out, l)?,
                None => out.extend_from_slice(br#""""#),
            }
            out.extend_from_slice(br#","object":"chat.completion.chunk","created":"#);
            push_int(&mut out, p.unix_timestamp);
            out.extend_from_slice(br#","model":"#);
            match model {
                Some(l) => push_literal(&mut out, l)?,
                None => out.extend_from_slice(br#""model""#),
            }
            out.extend_from_slice(
                br#","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]"#,
            );
            out.extend_from_slice(&usage_tail);
            Some(vec![out])
        }
        None => Some(vec![]),
    }
}
