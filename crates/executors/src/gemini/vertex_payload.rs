//! Vertex-specific payload fixes (Go: helps/vertex_payload_helpers.go).

use cpa_json::Value;

/// Removes the OpenAI Responses call ids Vertex rejects from Gemini `functionCall` and
/// `functionResponse` parts. Only applies when the client protocol is `openai-response`;
/// payloads without such ids come back unchanged (Go: StripVertexOpenAIResponsesToolCallIDs).
pub fn strip_vertex_openai_responses_tool_call_ids(payload: &[u8], source_format: &str) -> Vec<u8> {
    if !source_format.trim().eq_ignore_ascii_case("openai-response") {
        return payload.to_vec();
    }
    let mut v = cpa_json::parse(payload);
    let Some(Value::Array(contents)) = cpa_json::get_mut(&mut v, "contents") else {
        return payload.to_vec();
    };
    let mut changed = false;
    for content in contents.iter_mut() {
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        for part in parts.iter_mut() {
            for key in ["functionCall", "functionResponse"] {
                if let Some(Value::Object(call)) = part.get_mut(key)
                    && call.shift_remove("id").is_some()
                {
                    changed = true;
                }
            }
        }
    }
    if changed { cpa_json::to_vec(&v) } else { payload.to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_json::J;

    #[test]
    fn strips_ids_only_for_openai_response_sources() {
        let payload = br#"{"contents":[{"role":"model","parts":[{"functionCall":{"id":"c1","name":"f","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"id":"c1","name":"f","response":{}}},{"text":"x"}]}]}"#;
        let out = strip_vertex_openai_responses_tool_call_ids(payload, " OpenAI-Response ");
        let v = cpa_json::parse(&out);
        assert!(!v.g("contents.0.parts.0.functionCall.id").exists());
        assert_eq!(v.g("contents.0.parts.0.functionCall.name").str(), "f");
        assert!(!v.g("contents.1.parts.0.functionResponse.id").exists());
        assert_eq!(strip_vertex_openai_responses_tool_call_ids(payload, "openai"), payload.to_vec());
        let clean = br#"{"contents":[{"parts":[{"text":"x"}]}]}"#;
        assert_eq!(strip_vertex_openai_responses_tool_call_ids(clean, "openai-response"), clean.to_vec());
    }
}
