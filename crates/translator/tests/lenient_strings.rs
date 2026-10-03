//! Bodies whose strings hold unpaired surrogate escapes or invalid UTF-8 are valid JSON to Go's
//! gjson (`Valid` only checks structure, escape letters and four hex digits). They must flow
//! through translation like any other body; treating them as invalid would reduce the whole
//! request to `{}`.

use cpa_json::J;
use cpa_translator::{translate_request, Format};

fn bodies() -> Vec<Vec<u8>> {
    let strings: [&[u8]; 4] = [br"\ud800", br"\udc00x", b"\xff", b"\xed\xa0\x80"];
    strings
        .iter()
        .map(|s| {
            let mut body = br#"{"model":"m","max_tokens":16,"messages":[{"role":"user","content":""#.to_vec();
            body.extend_from_slice(s);
            body.extend_from_slice(br#""}]}"#);
            body
        })
        .collect()
}

#[test]
fn odd_strings_do_not_collapse_the_request() {
    for body in bodies() {
        assert!(cpa_json::valid(&body), "{body:?}");
        for (client, upstream, field) in [
            (Format::OpenAI, Format::Claude, "messages"),
            (Format::Claude, Format::OpenAI, "messages"),
            (Format::OpenAI, Format::Gemini, "contents"),
            (Format::Claude, Format::Gemini, "contents"),
        ] {
            let out = translate_request(client, upstream, "m", &body, false);
            let v = cpa_json::parse(&out);
            assert!(
                v.g(&format!("{field}.#")).int() > 0,
                "{client:?}->{upstream:?} lost the request for {body:?}: {}",
                String::from_utf8_lossy(&out)
            );
        }
    }
}
