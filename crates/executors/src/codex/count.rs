//! Token counting and credential refresh of the Codex HTTP executor (Go: codex_executor_tokens.go
//! and codex_executor_auth.go `Refresh`).

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::codex::{CodexAuth, apply_refresh_to_auth};
use cpa_auth::error::AuthFlowError;
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Res, Value};
use cpa_runtime::executor::{ExecError, Options, Request, Response};
use cpa_translator::{Ctx, Format};

use super::CodexExecutor;
use super::request::{is_native_request, thinking_error};
use super::terminal::status_error;
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::translate::{RequestTranslation, translate_request};
use crate::helps::token_count::{Tokenizer, tokenizer_for_model};

impl CodexExecutor {
    /// Local token estimate of the translated Codex request, rendered in the client format.
    pub(super) async fn count_tokens_impl(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let _ = auth;
        let cfg = self.config();
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Codex;
        let translation = RequestTranslation::new(&opts.headers, Some(&cfg), from, to, &base_model, false)
            .compat(api_key_model_is_compat(&req))
            .target_executor("codex");
        let (body, updates_changed) = translate_request(&translation, &req.payload);
        let body = apply_request_thinking(&body, &req, &opts, from.as_str(), to.as_str(), "codex", updates_changed).map_err(thinking_error)?;
        let mut parsed = cpa_json::parse(&body);
        if parsed.g("model").as_str() != Some(base_model.as_str()) {
            cpa_json::set(&mut parsed, "model", base_model.as_str());
        }
        for path in ["previous_response_id", "generate", "prompt_cache_retention", "safety_identifier", "stream_options"] {
            cpa_json::delete(&mut parsed, path);
        }
        if parsed.g("stream").v() != Some(&Value::Bool(false)) {
            cpa_json::set(&mut parsed, "stream", false);
        }
        if !is_native_request(&req.payload, &opts) {
            let instructions = parsed.g("instructions");
            if !instructions.exists() || instructions.is_null() {
                cpa_json::set(&mut parsed, "instructions", "");
            }
        }
        let tokenizer = tokenizer_for_codex_model(&base_model).map_err(|e| status_error(0, format!("codex executor: tokenizer init failed: {e}")))?;
        let count = count_input_tokens(&tokenizer, &parsed);
        let usage = format!(r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#);
        let payload = cpa_translator::translate_token_count(&Ctx::default(), to, response_format, count, usage.as_bytes());
        Ok(Response { payload: Bytes::from(payload), ..Default::default() })
    }

    /// Exchanges the refresh token and writes the new tokens back (Go: CodexExecutor.Refresh).
    pub(super) async fn refresh_auth(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let refresh_token = auth.metadata.get("refresh_token").and_then(Value::as_str).unwrap_or_default();
        if refresh_token.is_empty() {
            return Ok(auth.clone());
        }
        let service = CodexAuth::new(&auth.proxy_url).map_err(refresh_error)?;
        let token_data = service.refresh_tokens_with_retry(refresh_token, 3).await.map_err(refresh_error)?;
        let mut refreshed = auth.clone();
        apply_refresh_to_auth(&mut refreshed, &token_data);
        Ok(refreshed)
    }
}

fn refresh_error(err: AuthFlowError) -> ExecError {
    let mut out = ExecError::new(err.status_code().unwrap_or(0), err.to_string());
    out.retry_after = err.retry_after();
    out
}

/// Tokenizer by model family; unknown models use cl100k (Go: tokenizerForCodexModel).
fn tokenizer_for_codex_model(model: &str) -> Result<Tokenizer, String> {
    let sanitized = model.trim().to_lowercase();
    if sanitized.starts_with("gpt-5") || sanitized.starts_with("gpt-4.1") || sanitized.starts_with("gpt-4o") {
        return tokenizer_for_model(&sanitized);
    }
    tokenizer_for_model("gpt-4")
}

fn push_trimmed(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

/// Joins the countable text of a Codex request and tokenizes it once (Go: countCodexInputTokens).
fn count_input_tokens(tokenizer: &Tokenizer, root: &Value) -> i64 {
    let mut segments: Vec<String> = Vec::new();
    push_trimmed(&mut segments, &root.g("instructions").str());

    let input = root.g("input");
    if input.is_array() {
        for item in input.array() {
            match item.g("type").str().as_str() {
                "message" => {
                    let content = item.g("content");
                    if content.is_array() {
                        for part in content.array() {
                            push_trimmed(&mut segments, &part.g("text").str());
                        }
                    }
                }
                "function_call" => {
                    push_trimmed(&mut segments, &item.g("name").str());
                    push_trimmed(&mut segments, &item.g("arguments").str());
                }
                "function_call_output" => push_trimmed(&mut segments, &item.g("output").str()),
                _ => push_trimmed(&mut segments, &item.g("text").str()),
            }
        }
    }

    let tools = root.g("tools");
    if tools.is_array() {
        for tool in tools.array() {
            push_trimmed(&mut segments, &tool.g("name").str());
            push_trimmed(&mut segments, &tool.g("description").str());
            push_schema(&mut segments, &tool.g("parameters"));
        }
    }

    let text_format = root.g("text.format");
    if text_format.exists() {
        push_trimmed(&mut segments, &text_format.g("name").str());
        push_schema(&mut segments, &text_format.g("schema"));
    }

    let text = segments.join("\n");
    if text.is_empty() { 0 } else { tokenizer.count(&text) as i64 }
}

/// Schemas count as their raw JSON, or the string itself when given as a string.
fn push_schema(segments: &mut Vec<String>, node: &Res<'_>) {
    if node.exists() {
        let text = if node.is_string() { node.str() } else { node.raw() };
        push_trimmed(segments, &text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_instructions_messages_tools_and_schemas() {
        let tokenizer = tokenizer_for_codex_model("gpt-5").unwrap();
        let body = cpa_json::parse(
            br#"{"instructions":" be brief ","input":[{"type":"message","content":[{"type":"input_text","text":"hello there"}]},
            {"type":"function_call","name":"f","arguments":"{}"},{"type":"function_call_output","output":"ok"}],
            "tools":[{"name":"f","description":"d","parameters":{"type":"object"}}],"text":{"format":{"name":"n","schema":"S"}}}"#,
        );
        let expected = tokenizer.count("be brief\nhello there\nf\n{}\nok\nf\nd\n{\"type\":\"object\"}\nn\nS") as i64;
        assert_eq!(count_input_tokens(&tokenizer, &body), expected);
        assert_eq!(count_input_tokens(&tokenizer, &cpa_json::parse(b"{}")), 0);
    }

    #[test]
    fn unknown_models_use_cl100k() {
        let text = "naïve café 日本語";
        let unknown = tokenizer_for_codex_model("mystery").unwrap();
        let gpt4 = tokenizer_for_model("gpt-4").unwrap();
        assert_eq!(unknown.count(text), gpt4.count(text));
    }
}
