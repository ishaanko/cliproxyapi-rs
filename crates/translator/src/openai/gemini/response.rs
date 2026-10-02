//! OpenAI Chat Completions response -> Gemini generateContent response
//! (Go: openai/gemini/openai_gemini_response.go).

use std::collections::BTreeMap;

use cpa_json::{Map, Res, Value, J};

use crate::common;
use crate::registry::{Ctx, Param};

/// Streaming state (Go: ConvertOpenAIResponseToGeminiParams).
#[derive(Default)]
struct State {
    /// Tool calls accumulated across deltas, keyed by the OpenAI tool index.
    tool_calls_accumulator: BTreeMap<i64, ToolCallAccumulator>,
    content_accumulator: String,
    /// Never set to true in Go either, so the role-only first chunk path is dead.
    is_first_chunk: bool,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn tpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

/// Converts one OpenAI streaming line into Gemini JSON responses.
pub fn convert_openai_response_to_gemini(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let state = param.state(State::default);

    if raw.trim_ascii() == b"[DONE]" {
        return vec![];
    }
    let raw = raw.strip_prefix(b"data:").map_or(raw, |rest| rest.trim_ascii());

    let root = cpa_json::parse(raw);

    let choices = root.g("choices");
    if !choices.is_array() {
        return vec![];
    }

    // Empty choices array: usage-only chunk.
    if choices.array().is_empty() {
        let usage = root.g("usage");
        if usage.exists() {
            let mut template = tpl(r#"{"candidates":[],"usageMetadata":{}}"#);
            let model = root.g("model");
            if model.exists() {
                cpa_json::set(&mut template, "model", model.str());
            }
            set_gemini_usage_metadata_from_openai_usage(&mut template, &usage);
            return vec![cpa_json::to_vec(&template)];
        }
        return vec![];
    }

    let mut results: Vec<Vec<u8>> = Vec::new();

    for choice in choices.array() {
        // Base Gemini response without finishReason; set when known.
        let mut template = tpl(r#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#);
        let model = root.g("model");
        if model.exists() {
            cpa_json::set(&mut template, "model", model.str());
        }

        let delta = choice.g("delta");
        let base_template = template.clone();

        // Role (first chunk only).
        let role = delta.g("role");
        if role.exists() && state.is_first_chunk {
            if role.str() == "assistant" {
                cpa_json::set(&mut template, "candidates.0.content.role", "model");
            }
            state.is_first_chunk = false;
            results.push(cpa_json::to_vec(&template));
            continue;
        }

        let mut chunk_outputs: Vec<Vec<u8>> = Vec::new();

        let reasoning = delta.g("reasoning_content");
        if reasoning.exists() {
            for reasoning_text in extract_reasoning_texts(&reasoning) {
                if reasoning_text.is_empty() {
                    continue;
                }
                let mut reasoning_template = base_template.clone();
                cpa_json::set(&mut reasoning_template, "candidates.0.content.parts.0.thought", true);
                cpa_json::set(&mut reasoning_template, "candidates.0.content.parts.0.text", reasoning_text);
                chunk_outputs.push(cpa_json::to_vec(&reasoning_template));
            }
        }

        let content = delta.g("content");
        if content.exists() && !content.str().is_empty() {
            let content_text = content.str();
            state.content_accumulator.push_str(&content_text);

            let mut content_template = base_template.clone();
            cpa_json::set(&mut content_template, "candidates.0.content.parts.0.text", content_text);
            chunk_outputs.push(cpa_json::to_vec(&content_template));
        }

        if !chunk_outputs.is_empty() {
            results.append(&mut chunk_outputs);
            continue;
        }

        let tool_calls = delta.g("tool_calls");
        if tool_calls.is_array() {
            for tool_call in tool_calls.array() {
                let tool_index = tool_call.g("index").int();
                let tool_id = tool_call.g("id").str();
                let tool_type = tool_call.g("type").str();
                let function = tool_call.g("function");

                // Skip tool calls explicitly marked as another type.
                if !tool_type.is_empty() && tool_type != "function" {
                    continue;
                }
                // Deltas may omit the type field while still carrying function data.
                if !function.exists() {
                    continue;
                }

                let function_name = function.g("name").str();
                let function_args = function.g("arguments").str();

                // Created on first sight so later deltas without type can append arguments.
                let acc = state
                    .tool_calls_accumulator
                    .entry(tool_index)
                    .or_insert_with(|| ToolCallAccumulator { id: tool_id.clone(), name: function_name.clone(), arguments: String::new() });

                if !tool_id.is_empty() {
                    acc.id = tool_id;
                }
                if !function_name.is_empty() {
                    acc.name = function_name;
                }
                if !function_args.is_empty() {
                    acc.arguments.push_str(&function_args);
                }
            }
            // Nothing is emitted for tool call deltas; wait for completion.
            continue;
        }

        let finish_reason = choice.g("finish_reason");
        if finish_reason.is_string() && !finish_reason.str().is_empty() {
            let gemini_finish_reason = map_openai_finish_reason_to_gemini(&finish_reason.str());
            cpa_json::set(&mut template, "candidates.0.finishReason", gemini_finish_reason);

            // Flush accumulated tool calls. Go ranges over a map (random order); index order is
            // the only stable choice.
            if !state.tool_calls_accumulator.is_empty() {
                for (part_index, acc) in std::mem::take(&mut state.tool_calls_accumulator).into_values().enumerate() {
                    if !acc.id.is_empty() {
                        cpa_json::set(&mut template, &format!("candidates.0.content.parts.{part_index}.functionCall.id"), acc.id);
                    }
                    cpa_json::set(&mut template, &format!("candidates.0.content.parts.{part_index}.functionCall.name"), acc.name);
                    cpa_json::set(
                        &mut template,
                        &format!("candidates.0.content.parts.{part_index}.functionCall.args"),
                        parse_args_to_object(&acc.arguments),
                    );
                }
            }

            results.push(cpa_json::to_vec(&template));
            continue;
        }

        let usage = root.g("usage");
        if usage.exists() {
            set_gemini_usage_metadata_from_openai_usage(&mut template, &usage);
            results.push(cpa_json::to_vec(&template));
        }
    }
    results
}

fn map_openai_finish_reason_to_gemini(reason: &str) -> &'static str {
    match reason {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        // "stop", "tool_calls" (Gemini has no tool_calls reason) and anything else.
        _ => "STOP",
    }
}

/// Parses function arguments into a JSON object; `{}` when empty or unparseable.
fn parse_args_to_object(args: &str) -> Value {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Value::Object(Map::new());
    }

    // Strict JSON first.
    if cpa_json::valid(trimmed.as_bytes()) {
        let strict = cpa_json::parse_str(trimmed);
        if strict.is_object() {
            return strict;
        }
    }

    // Tolerant parse for streams whose values are barewords (e.g. 北京, celsius).
    let tolerant = tolerant_parse_json_object(trimmed);
    if tolerant.as_object().is_some_and(|m| !m.is_empty()) {
        return tolerant;
    }
    Value::Object(Map::new())
}

/// Sets `key` in `map` like sjson on an object: replace in place, else append. An empty key is
/// ignored (sjson rejects empty paths).
fn set_key(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !key.is_empty() {
        map.insert(key.to_string(), value);
    }
}

/// Parses a JSON-like object string into an object, tolerating bareword values (unquoted
/// strings) commonly seen in streamed tool calls, e.g. `{"location": 北京, "unit": celsius}`.
fn tolerant_parse_json_object(s: &str) -> Value {
    let empty = || Value::Object(Map::new());
    // Operate within the outermost braces.
    let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) else { return empty() };
    if start >= end {
        return empty();
    }
    let runes: Vec<char> = s[start + 1..end].chars().collect();
    let n = runes.len();
    let mut i = 0;
    let mut result = Map::new();
    let is_ws = |c: char| matches!(c, ' ' | '\n' | '\r' | '\t');

    while i < n {
        while i < n && (is_ws(runes[i]) || runes[i] == ',') {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Expect a quoted key; otherwise skip to the next comma.
        if runes[i] != '"' {
            while i < n && runes[i] != ',' {
                i += 1;
            }
            continue;
        }

        let Some((key_token, next)) = parse_json_string_runes(&runes, i) else { break };
        let key_name = json_string_token_to_raw_string(&key_token);
        i = next;

        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i >= n || runes[i] != ':' {
            break;
        }
        i += 1;
        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }

        match runes[i] {
            '"' => match parse_json_string_runes(&runes, i) {
                // Malformed: treat as an empty string.
                None => {
                    set_key(&mut result, &key_name, Value::String(String::new()));
                    i = n;
                }
                Some((val_token, ni)) => {
                    set_key(&mut result, &key_name, Value::String(json_string_token_to_raw_string(&val_token)));
                    i = ni;
                }
            },
            '{' | '[' => match capture_bracketed(&runes, i) {
                None => i = n,
                Some((seg, ni)) => {
                    if cpa_json::valid(seg.as_bytes()) {
                        set_key(&mut result, &key_name, cpa_json::parse_str(&seg));
                    } else {
                        set_key(&mut result, &key_name, Value::String(seg));
                    }
                    i = ni;
                }
            },
            _ => {
                // Bare token up to the next comma; common atoms and numbers are interpreted.
                let mut j = i;
                while j < n && runes[j] != ',' {
                    j += 1;
                }
                let token: String = runes[i..j].iter().collect();
                let token = token.trim();
                let value = match token {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    _ => try_parse_number(token).unwrap_or_else(|| Value::String(token.to_string())),
                };
                set_key(&mut result, &key_name, value);
                i = j;
            }
        }

        while i < n && is_ws(runes[i]) {
            i += 1;
        }
        if i < n && runes[i] == ',' {
            i += 1;
        }
    }

    Value::Object(result)
}

/// The JSON string token starting at `start` (quotes included) and the index just after it;
/// `None` when unterminated.
fn parse_json_string_runes(runes: &[char], start: usize) -> Option<(String, usize)> {
    if start >= runes.len() || runes[start] != '"' {
        return None;
    }
    let mut i = start + 1;
    let mut escaped = false;
    while i < runes.len() {
        let r = runes[i];
        if r == '\\' && !escaped {
            escaped = true;
            i += 1;
            continue;
        }
        if r == '"' && !escaped {
            return Some((runes[start..=i].iter().collect(), i + 1));
        }
        escaped = false;
        i += 1;
    }
    None
}

/// Unescapes a JSON string token (quotes included); falls back to stripping the quotes.
fn json_string_token_to_raw_string(token: &str) -> String {
    let parsed = cpa_json::parse_str(token);
    if let Some(s) = parsed.as_str() {
        return s.to_string();
    }
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        return token[1..token.len() - 1].to_string();
    }
    token.to_string()
}

/// A balanced `{...}` or `[...]` segment starting at `i` and the index just after it; `None` when
/// unbalanced.
fn capture_bracketed(runes: &[char], i: usize) -> Option<(String, usize)> {
    let start_rune = *runes.get(i)?;
    let end_rune = match start_rune {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let mut depth = 0i32;
    let mut j = i;
    let mut in_str = false;
    let mut escaped = false;
    while j < runes.len() {
        let r = runes[j];
        if in_str {
            if r == '\\' && !escaped {
                escaped = true;
                j += 1;
                continue;
            }
            if r == '"' && !escaped {
                in_str = false;
            } else {
                escaped = false;
            }
            j += 1;
            continue;
        }
        if r == '"' {
            in_str = true;
            j += 1;
            continue;
        }
        if r == start_rune {
            depth += 1;
        } else if r == end_rune {
            depth -= 1;
            if depth == 0 {
                return Some((runes[i..=j].iter().collect(), j + 1));
            }
        }
        j += 1;
    }
    None
}

/// An integer or float JSON number from a bare token (Go: ParseInt, ParseUint, ParseFloat).
fn try_parse_number(s: &str) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::from(i));
    }
    if let Ok(u) = s.parse::<u64>() {
        return Some(Value::from(u));
    }
    s.parse::<f64>().ok().map(cpa_json::num_f64)
}

/// Converts a complete OpenAI response into a Gemini response.
pub fn convert_openai_response_to_gemini_non_stream(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);

    let mut out = tpl(r#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#);
    let model = root.g("model");
    if model.exists() {
        cpa_json::set(&mut out, "model", model.str());
    }

    let mut all_parts: Vec<Value> = Vec::new();

    let choices = root.g("choices");
    if choices.is_array() {
        for choice in choices.array() {
            let choice_idx = choice.g("index").int();
            let message = choice.g("message");

            let role = message.g("role");
            if role.exists() && role.str() == "assistant" {
                cpa_json::set(&mut out, "candidates.0.content.role", "model");
            }

            // Parts of every choice are written from index 0, merging into earlier choices' parts.
            let mut part_index = 0usize;
            let ensure_part = |parts: &mut Vec<Value>, idx: usize| {
                while parts.len() <= idx {
                    parts.push(Value::Object(Map::new()));
                }
            };

            let reasoning = message.g("reasoning_content");
            if reasoning.exists() {
                for reasoning_text in extract_reasoning_texts(&reasoning) {
                    if reasoning_text.is_empty() {
                        continue;
                    }
                    ensure_part(&mut all_parts, part_index);
                    let part = &mut all_parts[part_index];
                    cpa_json::set(part, "thought", true);
                    cpa_json::set(part, "text", reasoning_text);
                    part_index += 1;
                }
            }

            let content = message.g("content");
            if content.exists() && !content.str().is_empty() {
                ensure_part(&mut all_parts, part_index);
                cpa_json::set(&mut all_parts[part_index], "text", content.str());
                part_index += 1;
            }

            let tool_calls = message.g("tool_calls");
            if tool_calls.is_array() {
                for tool_call in tool_calls.array() {
                    if tool_call.g("type").str() == "function" {
                        let function = tool_call.g("function");
                        let function_name = function.g("name").str();
                        let function_args = function.g("arguments").str();
                        let function_id = tool_call.g("id").str();

                        ensure_part(&mut all_parts, part_index);
                        let part = &mut all_parts[part_index];
                        if !function_id.is_empty() {
                            cpa_json::set(part, "functionCall.id", function_id);
                        }
                        cpa_json::set(part, "functionCall.name", function_name);
                        cpa_json::set(part, "functionCall.args", parse_args_to_object(&function_args));
                        part_index += 1;
                    }
                }
            }

            let finish_reason = choice.g("finish_reason");
            if finish_reason.is_string() && !finish_reason.str().is_empty() {
                cpa_json::set(&mut out, "candidates.0.finishReason", map_openai_finish_reason_to_gemini(&finish_reason.str()));
            }

            cpa_json::set(&mut out, "candidates.0.index", choice_idx);
        }

        if !all_parts.is_empty() {
            cpa_json::set(&mut out, "candidates.0.content.parts", Value::Array(all_parts));
        }
    }

    let usage = root.g("usage");
    if usage.exists() {
        set_gemini_usage_metadata_from_openai_usage(&mut out, &usage);
    }

    Some(cpa_json::to_vec(&out))
}

/// Gemini countTokens response body.
pub fn gemini_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    common::gemini_token_count_json(count)
}

fn reasoning_tokens_from_usage(usage: &Res<'_>) -> i64 {
    if usage.exists() {
        for path in ["completion_tokens_details.reasoning_tokens", "output_tokens_details.reasoning_tokens"] {
            let v = usage.g(path);
            if v.exists() {
                return v.int();
            }
        }
    }
    0
}

fn set_gemini_usage_metadata_from_openai_usage(out: &mut Value, usage: &Res<'_>) {
    let prompt_tokens = token_count_from_usage(usage, &["prompt_tokens", "input_tokens"]);
    let completion_tokens = token_count_from_usage(usage, &["completion_tokens", "output_tokens"]);
    let total_tokens = token_count_from_usage(usage, &["total_tokens"]);
    if let Some(prompt) = prompt_tokens {
        cpa_json::set(out, "usageMetadata.promptTokenCount", prompt);
    }
    if let Some(completion) = completion_tokens {
        cpa_json::set(out, "usageMetadata.candidatesTokenCount", completion);
    }
    if let Some(total) = total_tokens {
        cpa_json::set(out, "usageMetadata.totalTokenCount", total);
    } else if prompt_tokens.is_some() || completion_tokens.is_some() {
        cpa_json::set(
            out,
            "usageMetadata.totalTokenCount",
            prompt_tokens.unwrap_or(0).wrapping_add(completion_tokens.unwrap_or(0)),
        );
    }
    let reasoning_tokens = reasoning_tokens_from_usage(usage);
    if reasoning_tokens > 0 {
        cpa_json::set(out, "usageMetadata.thoughtsTokenCount", reasoning_tokens);
    }
    let cached_tokens = cached_tokens_from_usage(usage);
    if cached_tokens > 0 {
        cpa_json::set(out, "usageMetadata.cachedContentTokenCount", cached_tokens);
    }
}

/// The first of `paths` present in `usage`, as an integer.
fn token_count_from_usage(usage: &Res<'_>, paths: &[&str]) -> Option<i64> {
    paths.iter().map(|p| usage.g(p)).find(|v| v.exists()).map(|v| v.int())
}

fn cached_tokens_from_usage(usage: &Res<'_>) -> i64 {
    if usage.exists() {
        for path in ["prompt_tokens_details.cached_tokens", "input_tokens_details.cached_tokens"] {
            let v = usage.g(path);
            if v.exists() {
                return v.int();
            }
        }
    }
    0
}

fn extract_reasoning_texts(node: &Res<'_>) -> Vec<String> {
    let mut texts = Vec::new();
    if !node.exists() {
        return texts;
    }
    if node.is_array() {
        for value in node.array() {
            texts.extend(extract_reasoning_texts(&value));
        }
        return texts;
    }
    if node.is_string() {
        texts.push(node.str());
    } else if node.is_object() {
        let text = node.g("text");
        if text.exists() {
            texts.push(text.str());
        }
    }
    texts
}
