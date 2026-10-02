//! The compat request converter is not registered (executors pick it for compatibility
//! endpoints), so the golden corpus never calls it. Ported from codex_claude_compat_test.go.

use base64::Engine;
use cpa_json::{J, Value};

use super::{convert_claude_request_to_codex, convert_claude_request_to_codex_with_compat};

fn thinking_request(signature: &str) -> Vec<u8> {
    format!(r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"reason","signature":{signature}}}]}}]}}"#).into_bytes()
}

fn compat(body: &[u8]) -> Value {
    cpa_json::parse(&convert_claude_request_to_codex_with_compat(
        "deepseek-v4",
        body,
        false,
    ))
}

fn plain(body: &[u8]) -> Value {
    cpa_json::parse(&convert_claude_request_to_codex("deepseek-v4", body, false))
}

fn input_len(v: &Value) -> i64 {
    v.g("input.#").int()
}

#[test]
fn compat_keeps_empty_and_unknown_signatures_only() {
    // Empty signature: dropped by default, kept (as empty encrypted_content) with compat.
    let empty = thinking_request(r#""""#);
    assert_eq!(input_len(&plain(&empty)), 0);
    let out = compat(&empty);
    assert_eq!(out.g("input.0.type").str(), "reasoning");
    assert!(out.g("input.0.encrypted_content").exists());

    // Unknown format: kept verbatim with compat only.
    let unknown = thinking_request(r#""opaque-encrypted-reasoning-token-xyz""#);
    assert_eq!(
        compat(&unknown).g("input.0.encrypted_content").str(),
        "opaque-encrypted-reasoning-token-xyz"
    );
    assert_eq!(input_len(&plain(&unknown)), 0);

    // Whitespace and null signatures fall back to the empty-signature path.
    assert_eq!(
        compat(&thinking_request(r#""   ""#))
            .g("input.0.encrypted_content")
            .str(),
        "   "
    );
    let null = compat(&thinking_request("null"));
    assert_eq!(null.g("input.0.type").str(), "reasoning");
    assert_eq!(null.g("input.0.encrypted_content").str(), "");

    // Non-string signatures are never kept.
    for sig in ["12345", r#"{"opaque":"data"}"#, "true", r#"["arr"]"#] {
        assert_eq!(
            input_len(&compat(&thinking_request(sig))),
            0,
            "signature {sig}"
        );
    }

    // A valid Claude signature is not replayable on Codex.
    let claude_sig = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";
    assert_eq!(
        input_len(&compat(&thinking_request(&format!("\"{claude_sig}\"")))),
        0
    );

    // A prefixed GPT signature is normalized to its raw form.
    let mut raw = vec![0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    let gpt_sig = base64::engine::general_purpose::URL_SAFE.encode(raw);
    let out = compat(&thinking_request(&format!("\"gpt#{gpt_sig}\"")));
    assert_eq!(out.g("input.0.type").str(), "reasoning");
    assert_eq!(out.g("input.0.encrypted_content").str(), gpt_sig);
}

#[test]
fn compat_flushes_messages_around_an_escaped_unknown_signature() {
    let body = r#"{"messages":[{"role":"assistant","content":[{"type":"text","text":"before"},{"type":"thinking","thinking":"reason","signature":"enc:\"token\"\\with\\unicode-一"},{"type":"text","text":"after"}]}]}"#;
    let out = compat(body.as_bytes());
    assert_eq!(input_len(&out), 3);
    assert_eq!(out.g("input.0.content.0.text").str(), "before");
    assert_eq!(out.g("input.1.type").str(), "reasoning");
    assert_eq!(
        out.g("input.1.encrypted_content").str(),
        "enc:\"token\"\\with\\unicode-\u{4e00}"
    );
    assert_eq!(out.g("input.2.content.0.text").str(), "after");
}
