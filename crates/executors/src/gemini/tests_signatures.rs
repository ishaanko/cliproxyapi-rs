//! Thought-signature handling through the Gemini and Vertex executors: signatures minted by other
//! providers must never reach the upstream, native Gemini ones must survive.

use bytes::Bytes;
use cpa_json::J;
use cpa_runtime::executor::{Executor, Options, Request};
use cpa_translator::Format;

use super::test_support::{config_rx, json_reply, key_auth, mock_upstream};
use super::{GeminiExecutor, GeminiVertexExecutor};

const OK_BODY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#;
const CLAUDE_CAIS_SIGNATURE: &str = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";

/// A protobuf-wrapped Gemini 3 thought signature: field 2 holding field 1 holding six bytes.
fn native_gemini3_signature() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode([0x12, 0x08, 0x0a, 0x06, 0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34])
}

fn function_call_history(signature: &str) -> String {
    format!(
        r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"search","args":{{"q":"go"}}}},"thoughtSignature":"{signature}"}}]}},{{"role":"user","parts":[{{"functionResponse":{{"name":"search","response":{{"result":"found"}}}}}}]}}]}}"#
    )
}

fn gemini_request(payload: String) -> Request {
    Request {
        model: "gemini-2.5-flash".into(),
        payload: Bytes::from(payload),
        format: Format::Gemini,
        metadata: Default::default(),
    }
}

async fn run(provider: &str, req: Request, opts: Options) -> Vec<u8> {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let auth = key_auth(provider, &base);
    if provider == "gemini" {
        GeminiExecutor::new(config_rx()).execute(&auth, req, opts).await.expect("execute");
    } else {
        GeminiVertexExecutor::new(config_rx()).execute(&auth, req, opts).await.expect("execute");
    }
    seen.recv().await.unwrap().body
}

#[tokio::test]
async fn claude_signatures_never_reach_the_upstream() {
    let claude = format!(
        r#"{{"model":"claude-3-7-sonnet-20250219","messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"Let me think...","signature":"{CLAUDE_CAIS_SIGNATURE}"}},{{"type":"text","text":"Here is the response."}}]}},{{"role":"user","content":[{{"type":"text","text":"Follow up question."}}]}}]}}"#
    );
    let mut req = Request {
        model: "gemini-2.5-flash".into(),
        payload: Bytes::from(claude),
        format: Format::Claude,
        metadata: Default::default(),
    };
    req.metadata.insert(
        "cliproxy.resolved_api_key_model_info".into(),
        serde_json::json!({"id": "gemini-2.5-flash", "is_compat": true}),
    );
    for provider in ["gemini", "vertex"] {
        let body = run(provider, req.clone(), Options::new(Format::Claude)).await;
        assert!(
            !String::from_utf8_lossy(&body).contains(CLAUDE_CAIS_SIGNATURE),
            "{provider} leaked the Claude signature"
        );
    }
}

#[tokio::test]
async fn function_call_signatures_native_kept_foreign_replaced_by_bypass() {
    let native = native_gemini3_signature();
    for provider in ["gemini", "vertex"] {
        for (signature, expected) in [
            (CLAUDE_CAIS_SIGNATURE.to_string(), "skip_thought_signature_validator".to_string()),
            (native.clone(), native.clone()),
        ] {
            let body =
                run(provider, gemini_request(function_call_history(&signature)), Options::new(Format::Gemini)).await;
            // The synthetic leading user turn shifts the history by one.
            let sig = cpa_json::parse(&body).g("contents.1.parts.0.thoughtSignature").str();
            assert_eq!(sig, expected, "{provider}");
        }
    }
}
