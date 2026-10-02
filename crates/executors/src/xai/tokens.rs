//! Local token estimate for xAI Responses requests (Go: xai_executor_tokens.go).

use bytes::Bytes;
use cpa_json::{J, Kind, Res};
use cpa_runtime::executor::{ExecError, Options, Request, Response};
use cpa_translator::Ctx;

use super::XaiExecutor;
use super::request::prepare_responses_request;
use crate::helps::token_count::{Tokenizer, tokenizer_for_model};

impl XaiExecutor {
    /// Go: CountTokens. o200k estimate over the prepared request; no upstream call.
    pub(super) async fn count_tokens_local(&self, req: &Request, opts: &Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        let prepared = prepare_responses_request(&cfg, req, opts, false)?;
        let enc = tokenizer_for_model("gpt-5")
            .map_err(|e| ExecError::new(0, format!("xai executor: tokenizer init failed: {e}")))?;
        let count = count_input_tokens(&enc, &prepared.body);
        let usage_json = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let translated = cpa_translator::translate_token_count(
            &Ctx::default(),
            prepared.to,
            prepared.response_format,
            count,
            usage_json.as_bytes(),
        );
        Ok(Response { payload: Bytes::from(translated), ..Default::default() })
    }
}

/// Go: countXAIInputTokens.
fn count_input_tokens(enc: &Tokenizer, body: &[u8]) -> i64 {
    if body.is_empty() {
        return 0;
    }
    let root = cpa_json::parse(body);
    let mut segments: Vec<String> = Vec::with_capacity(32);
    append_string(&mut segments, &root.g("instructions"));
    collect_input(&root.g("input"), &mut segments);
    collect_tools(&root.g("tools"), &mut segments);
    let text_format = root.g("text.format");
    if text_format.exists() {
        append_string(&mut segments, &text_format.g("name"));
        append_json(&mut segments, &text_format.g("schema"));
    }
    if segments.is_empty() {
        return 0;
    }
    enc.count(&segments.join("\n")) as i64
}

fn collect_input(input: &Res<'_>, segments: &mut Vec<String>) {
    if input.kind() == Kind::String {
        append_string(segments, input);
        return;
    }
    if !input.is_array() {
        return;
    }
    for item in input.array() {
        match item.g("type").str().as_str() {
            "message" => collect_content(&item.g("content"), segments),
            "function_call" => {
                append_string(segments, &item.g("name"));
                append_json(segments, &item.g("arguments"));
            }
            "function_call_output" => append_json(segments, &item.g("output")),
            "reasoning" => {
                for part in item.g("summary").array() {
                    append_string(segments, &part.g("text"));
                }
            }
            _ => {}
        }
    }
}

fn collect_content(content: &Res<'_>, segments: &mut Vec<String>) {
    if content.kind() == Kind::String {
        append_string(segments, content);
        return;
    }
    if !content.is_array() {
        return;
    }
    for part in content.array() {
        match part.g("type").str().as_str() {
            "text" | "input_text" | "output_text" => append_string(segments, &part.g("text")),
            "refusal" => append_string(segments, &part.g("refusal")),
            "input_image" => {
                append_string(segments, &part.g("image_url"));
                append_string(segments, &part.g("file_id"));
            }
            "input_file" => {
                for field in ["file_data", "file_url", "file_id", "filename"] {
                    append_string(segments, &part.g(field));
                }
            }
            "input_audio" => {
                append_string(segments, &part.g("data"));
                append_string(segments, &part.g("input_audio.data"));
            }
            _ => {}
        }
    }
}

fn collect_tools(tools: &Res<'_>, segments: &mut Vec<String>) {
    if !tools.is_array() {
        return;
    }
    for tool in tools.array() {
        if tool.g("type").str() != "function" {
            continue;
        }
        append_string(segments, &tool.g("name"));
        append_string(segments, &tool.g("description"));
        append_json(segments, &tool.g("parameters"));
    }
}

fn append_string(segments: &mut Vec<String>, value: &Res<'_>) {
    let text = value.str();
    let text = text.trim();
    if !text.is_empty() {
        segments.push(text.to_string());
    }
}

fn append_json(segments: &mut Vec<String>, value: &Res<'_>) {
    if !value.exists() {
        return;
    }
    if value.kind() == Kind::String {
        append_string(segments, value);
        return;
    }
    let raw = value.raw();
    let text = raw.trim();
    if !text.is_empty() {
        segments.push(text.to_string());
    }
}
