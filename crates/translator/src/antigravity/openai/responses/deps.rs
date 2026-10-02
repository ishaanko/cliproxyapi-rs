//! The gemini/openai/responses pieces this translator builds on (Go dot-imports that package).
//!
//! BLOCKED: that package is being ported separately and is not on main yet. Until it lands these
//! are inert placeholders; replace the whole file body with
//! `pub use crate::gemini::openai::responses::{...}` for the same names.

use cpa_json::Value;

use crate::registry::{Ctx, Param};

pub fn convert_openai_responses_request_to_gemini(_model: &str, raw: &[u8], _stream: bool) -> Vec<u8> {
    raw.to_vec()
}

pub fn has_only_responses_web_search_tools(_root: &Value) -> bool {
    false
}

pub fn allows_responses_web_search_tool_choice(_root: &Value) -> bool {
    false
}

pub fn extract_responses_web_search_allowed_domains(_root: &Value) -> Vec<String> {
    vec![]
}

pub fn convert_gemini_response_to_openai_responses(_ctx: &Ctx, _model: &str, _original: &[u8], _translated: &[u8], _raw: &[u8], _param: &mut Param) -> Vec<Vec<u8>> {
    vec![]
}

pub fn convert_gemini_response_to_openai_responses_non_stream(_ctx: &Ctx, _model: &str, _original: &[u8], _translated: &[u8], _raw: &[u8], _param: &mut Param) -> Option<Vec<u8>> {
    None
}
