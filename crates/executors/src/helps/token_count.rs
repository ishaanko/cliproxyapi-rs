//! Local token approximation for providers without a count endpoint (Go: helps/token_helpers.go).
//!
//! Uses tiktoken BPE encodings (Go uses tiktoken-go/tokenizer with the same model to encoding
//! mapping): `o200k_base` for gpt-5/4.1/4o/o-series and unknown models, `cl100k_base` for
//! gpt-4/3.5 and the empty model. Text is encoded as ordinary text (no special tokens).

use std::sync::LazyLock;

use cpa_json::{J, Res, Value};
use tiktoken_rs::CoreBPE;

static CL100K: LazyLock<Result<CoreBPE, String>> =
    LazyLock::new(|| tiktoken_rs::cl100k_base().map_err(|e| e.to_string()));
static O200K: LazyLock<Result<CoreBPE, String>> =
    LazyLock::new(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()));

/// A loaded tokenizer encoding.
#[derive(Clone, Copy)]
pub struct Tokenizer {
    bpe: &'static CoreBPE,
}

impl Tokenizer {
    /// Number of tokens in `text` encoded as ordinary text.
    pub fn count(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }
}

fn load(cell: &'static LazyLock<Result<CoreBPE, String>>) -> Result<Tokenizer, String> {
    match &**cell {
        Ok(bpe) => Ok(Tokenizer { bpe }),
        Err(err) => Err(err.clone()),
    }
}

/// Tokenizer suited to an OpenAI-style model id (prefix match, lower-cased and trimmed).
pub fn tokenizer_for_model(model: &str) -> Result<Tokenizer, String> {
    let m = model.trim().to_lowercase();
    if m.is_empty() {
        return load(&CL100K);
    }
    // gpt-4o and gpt-4.1 are checked before the generic gpt-4 prefix, as in Go.
    let o200k = ["gpt-5", "gpt-4.1", "gpt-4o", "o1", "o3", "o4"];
    if o200k.iter().any(|p| m.starts_with(p)) {
        return load(&O200K);
    }
    if m.starts_with("gpt-4") || m.starts_with("gpt-3") {
        return load(&CL100K);
    }
    load(&O200K)
}

/// Approximate prompt token count of an OpenAI chat completions payload: role, name, content
/// text, tool calls, tool and function declarations, tool choice, response format, `input` and
/// `prompt` are joined and tokenized once.
pub fn count_openai_chat_tokens(enc: &Tokenizer, payload: &[u8]) -> i64 {
    if payload.is_empty() {
        return 0;
    }
    let root = cpa_json::parse(payload);
    let mut segments: Vec<String> = Vec::with_capacity(32);
    collect_messages(&root.g("messages"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    collect_functions(&root.g("functions"), &mut segments);
    collect_tool_choice(&root.g("tool_choice"), &mut segments);
    collect_response_format(&root.g("response_format"), &mut segments);
    add(&mut segments, &root.g("input").str());
    add(&mut segments, &root.g("prompt").str());
    let joined = segments.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        return 0;
    }
    enc.count(joined) as i64
}

/// Minimal usage object understood by the response translators.
pub fn build_openai_usage_json(count: i64) -> Vec<u8> {
    format!(r#"{{"usage":{{"prompt_tokens":{count},"completion_tokens":0,"total_tokens":{count}}}}}"#).into_bytes()
}

fn add(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

fn collect_messages(messages: &Res<'_>, segments: &mut Vec<String>) {
    if !messages.is_array() {
        return;
    }
    for message in messages.array() {
        add(segments, &message.g("role").str());
        add(segments, &message.g("name").str());
        collect_content(&message.g("content"), segments);
        collect_tool_calls(&message.g("tool_calls"), segments);
        collect_function_call(&message.g("function_call"), segments);
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    if !content.exists() {
        return;
    }
    match content.v() {
        Some(Value::String(s)) => add(segments, s),
        Some(Value::Array(_)) => {
            for part in content.array() {
                match part.g("type").str().as_str() {
                    "text" | "input_text" | "output_text" => add(segments, &part.g("text").str()),
                    "image_url" => add(segments, &part.g("image_url.url").str()),
                    "input_audio" | "output_audio" | "audio" => add(segments, &part.g("id").str()),
                    "tool_result" => {
                        add(segments, &part.g("name").str());
                        collect_content(&part.g("content"), segments);
                    }
                    _ => {
                        if part.is_array() {
                            collect_content(&part, segments);
                        } else if matches!(part.v(), Some(Value::Object(_))) {
                            add(segments, &part.raw());
                        } else {
                            add(segments, &part.str());
                        }
                    }
                }
            }
        }
        Some(Value::Object(_)) => add(segments, &content.raw()),
        _ => {}
    }
}

fn collect_tool_calls(calls: &Res<'_>, segments: &mut Vec<String>) {
    if !calls.is_array() {
        return;
    }
    for call in calls.array() {
        add(segments, &call.g("id").str());
        add(segments, &call.g("type").str());
        let function = call.g("function");
        if function.exists() {
            add(segments, &function.g("name").str());
            add(segments, &function.g("description").str());
            add(segments, &function.g("arguments").str());
            let params = function.g("parameters");
            if params.exists() {
                add(segments, &params.raw());
            }
        }
    }
}

fn collect_function_call(call: &Res<'_>, segments: &mut Vec<String>) {
    if !call.exists() {
        return;
    }
    add(segments, &call.g("name").str());
    add(segments, &call.g("arguments").str());
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    if !tools.exists() {
        return;
    }
    if tools.is_array() {
        for tool in tools.array() {
            append_tool(&tool, segments);
        }
        return;
    }
    append_tool(tools, segments);
}

fn collect_functions(functions: &Res<'_>, segments: &mut Vec<String>) {
    if !functions.is_array() {
        return;
    }
    for function in functions.array() {
        add(segments, &function.g("name").str());
        add(segments, &function.g("description").str());
        let params = function.g("parameters");
        if params.exists() {
            add(segments, &params.raw());
        }
    }
}

fn collect_tool_choice(choice: &Res<'_>, segments: &mut Vec<String>) {
    if !choice.exists() {
        return;
    }
    if choice.is_string() {
        add(segments, &choice.str());
        return;
    }
    add(segments, &choice.raw());
}

fn collect_response_format(format: &Res<'_>, segments: &mut Vec<String>) {
    if !format.exists() {
        return;
    }
    add(segments, &format.g("type").str());
    add(segments, &format.g("name").str());
    for key in ["json_schema", "schema"] {
        let schema = format.g(key);
        if schema.exists() {
            add(segments, &schema.raw());
        }
    }
}

fn append_tool(tool: &Res<'_>, segments: &mut Vec<String>) {
    if !tool.exists() {
        return;
    }
    add(segments, &tool.g("type").str());
    add(segments, &tool.g("name").str());
    add(segments, &tool.g("description").str());
    let function = tool.g("function");
    if function.exists() {
        add(segments, &function.g("name").str());
        add(segments, &function.g("description").str());
        let params = function.g("parameters");
        if params.exists() {
            add(segments, &params.raw());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_to_encoding_mapping() {
        // gpt-4o/gpt-5 use o200k, gpt-4/3.5 use cl100k; the two differ on non-ASCII text.
        let text = "héllo wörld, 你好,世界! hello world";
        let (o200k, cl100k) = (tokenizer_for_model("gpt-5-mini").unwrap(), tokenizer_for_model("gpt-4").unwrap());
        assert_eq!(tokenizer_for_model("GPT-4o").unwrap().count(text), o200k.count(text));
        assert_eq!(tokenizer_for_model("").unwrap().count(text), cl100k.count(text));
        assert_eq!(tokenizer_for_model("gpt-3.5-turbo").unwrap().count(text), cl100k.count(text));
        assert_eq!(tokenizer_for_model("claude-x").unwrap().count(text), o200k.count(text));
        assert!(o200k.count(text) > 0);
    }

    #[test]
    fn chat_payload_token_counts() {
        let enc = tokenizer_for_model("gpt-4").unwrap();
        assert_eq!(count_openai_chat_tokens(&enc, b""), 0);
        assert_eq!(count_openai_chat_tokens(&enc, b"{}"), 0);
        let payload = br#"{"messages":[{"role":"user","content":"hello world"}]}"#;
        // "user\nhello world" in cl100k: user + \n + hello + world.
        assert_eq!(count_openai_chat_tokens(&enc, payload), enc.count("user\nhello world") as i64);
        let with_tools = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"tools":[{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object"}}}],"tool_choice":"auto"}"#;
        let expected = "user\nhi\nfunction\nf\nd\n{\"type\":\"object\"}\nauto";
        assert_eq!(count_openai_chat_tokens(&enc, with_tools), enc.count(expected) as i64);
        assert_eq!(
            String::from_utf8(build_openai_usage_json(7)).unwrap(),
            r#"{"usage":{"prompt_tokens":7,"completion_tokens":0,"total_tokens":7}}"#
        );
    }
}
