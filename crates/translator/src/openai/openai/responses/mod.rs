//! Port of internal/translator/openai/openai/responses (Responses client -> Chat Completions
//! upstream).

mod request;
mod response;
mod tools;

use cpa_core::format::Format;

use cpa_json::{raw_at, raw_children, Res};

use crate::registry::{Registry, ResponseFns};

pub use request::convert_openai_responses_request_to_openai_chat_completions;
pub use response::{
    convert_openai_chat_completions_response_to_openai_responses,
    convert_openai_chat_completions_response_to_openai_responses_non_stream,
};
use response::finalize_tool_input;

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::OpenAI,
        Some(convert_openai_responses_request_to_openai_chat_completions),
        ResponseFns {
            stream: Some(convert_openai_chat_completions_response_to_openai_responses),
            non_stream: Some(convert_openai_chat_completions_response_to_openai_responses_non_stream),
            token_count: None,
            finalize: Some(finalize_tool_input),
        },
    );
}

use cpa_core::util::go_any;

/// The request JSON used to resolve tool declarations: the original request when valid, else the
/// translated one, else none.
fn pick_request_json<'a>(original: &'a [u8], translated: &'a [u8]) -> Option<&'a [u8]> {
    [original, translated].into_iter().find(|raw| !raw.is_empty() && cpa_json::valid(raw))
}

/// The original text of a value in a JSON document, so Go's verbatim `Raw` copies of objects and
/// arrays (client whitespace and escapes included) can be reproduced. Lookups are relative to the
/// value's own text; use [`RawSrc::children`] to walk array elements in one pass.
#[derive(Clone, Copy)]
struct RawSrc<'a> {
    text: Option<&'a str>,
}

impl<'a> RawSrc<'a> {
    /// A whole document.
    fn new(doc: &'a [u8]) -> Self {
        RawSrc { text: std::str::from_utf8(doc).ok() }
    }

    /// A source with no document: every lookup falls back to the parsed value.
    fn none() -> RawSrc<'static> {
        RawSrc { text: None }
    }

    /// The member or element at a plain dotted key/index `path`.
    fn child(&self, path: &str) -> RawSrc<'a> {
        RawSrc { text: self.text.and_then(|t| raw_at(t.as_bytes(), path)) }
    }

    /// Every element of the array (or member of the object) in one pass.
    fn children(&self) -> Vec<RawSrc<'a>> {
        let items = self.text.map(|t| raw_children(t.as_bytes(), "")).unwrap_or_default();
        items.into_iter().map(|t| RawSrc { text: Some(t) }).collect()
    }

    /// gjson `Raw`: original text, else the value's compact serialization.
    fn raw(&self, fallback: &Res<'_>) -> String {
        self.text.map_or_else(|| fallback.raw(), str::to_string)
    }

    /// gjson `String()`: containers come back as their raw text.
    fn string(&self, res: &Res<'_>) -> String {
        if res.is_object() || res.is_array() { self.raw(res) } else { res.str() }
    }
}
