//! Port of internal/translator/openai/openai/responses (Responses client -> Chat Completions
//! upstream).

mod request;
mod response;
mod tools;

use cpa_core::format::Format;
use cpa_core::util::{go_json_sorted, GoJsonStyle};
use cpa_json::Value;

use std::borrow::Cow;

use cpa_json::Res;

use crate::openai::claude::raw::{raw_at, Seg};
use crate::registry::{Registry, ResponseFns};

pub use request::convert_openai_responses_request_to_openai_chat_completions;
pub use response::{
    convert_openai_chat_completions_response_to_openai_responses,
    convert_openai_chat_completions_response_to_openai_responses_non_stream, finalize_tool_input,
};

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::OpenAI,
        Some(convert_openai_responses_request_to_openai_chat_completions),
        ResponseFns {
            stream: Some(convert_openai_chat_completions_response_to_openai_responses),
            non_stream: Some(convert_openai_chat_completions_response_to_openai_responses_non_stream),
            token_count: None,
        },
    );
}

/// What Go's `gjson.Result.Value()` followed by a marshal produces: object keys sorted, numbers
/// round-tripped through float64. Used where Go sets `v.Value()` or `[]interface{}` into sjson.
fn go_any(v: Value) -> Value {
    match go_json_sorted(&v, GoJsonStyle::MARSHAL_ANY) {
        Some(s) => cpa_json::parse_str(&s),
        None => v,
    }
}

/// The request JSON used to resolve tool declarations: the original request when valid, else the
/// translated one, else none.
fn pick_request_json<'a>(original: &'a [u8], translated: &'a [u8]) -> Option<&'a [u8]> {
    [original, translated].into_iter().find(|raw| !raw.is_empty() && cpa_json::valid(raw))
}

/// Where a value sits in a JSON document, so Go's verbatim `Raw` copies of objects and arrays
/// (client whitespace and escapes included) can be reproduced from the original bytes.
#[derive(Clone)]
struct RawSrc<'a> {
    doc: Cow<'a, [u8]>,
    path: Vec<Seg<'static>>,
}

impl<'a> RawSrc<'a> {
    fn new(doc: &'a [u8], path: Vec<Seg<'static>>) -> Self {
        RawSrc { doc: Cow::Borrowed(doc), path }
    }

    /// A source with no document: every lookup falls back to the parsed value.
    fn none() -> RawSrc<'static> {
        RawSrc { doc: Cow::Borrowed(&[]), path: Vec::new() }
    }

    fn child(&self, seg: Seg<'static>) -> RawSrc<'_> {
        let mut path = self.path.clone();
        path.push(seg);
        RawSrc { doc: Cow::Borrowed(&self.doc), path }
    }

    /// gjson `Raw`: original text, else the value's compact serialization.
    fn raw(&self, fallback: &Res<'_>) -> String {
        raw_at(&self.doc, &self.path).map_or_else(|| fallback.raw(), str::to_string)
    }

    /// gjson `String()`: containers come back as their raw text.
    fn string(&self, res: &Res<'_>) -> String {
        if res.is_object() || res.is_array() { self.raw(res) } else { res.str() }
    }
}
