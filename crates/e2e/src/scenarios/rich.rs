//! Rich requests (system prompt, images, tool round trips, sampling options) in every client
//! dialect against every upstream family: the upstream request body shows how each request
//! translator maps the same conversation.

use serde_json::{Value, json};

use super::bodies::{FAMILIES, Family};
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::Script;
use crate::mock::script::Content;
use crate::scenario::Scenario;

const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";

fn weather_schema() -> Value {
    json!({"type":"object","properties":{"city":{"type":"string"},"unit":{"type":"string","enum":["c","f"]}},"required":["city"]})
}

fn chat(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "messages": [
            {"role": "system", "content": "You are a terse weather bot."},
            {"role": "user", "content": [
                {"type": "text", "text": "What does this look like and what is the weather in Paris?"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_B64}")}}
            ]},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_abc123", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_abc123", "content": "18C and sunny"},
            {"role": "user", "content": "Thanks. And in Rome?"}
        ],
        "tools": [
            {"type": "function", "function": {"name": "get_weather", "description": "Get the weather", "parameters": weather_schema()}},
            {"type": "function", "function": {"name": "get_time", "description": "Get the time", "parameters": {"type": "object", "properties": {}}}}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "temperature": 0.2,
        "top_p": 0.9,
        "max_tokens": 300,
        "stop": ["END"],
        "user": "user-1"
    })
}

fn responses(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "instructions": "You are a terse weather bot.",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "What does this look like and what is the weather in Paris?"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_B64}")}
            ]},
            {"type": "function_call", "call_id": "call_abc123", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_abc123", "output": "18C and sunny"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Thanks. And in Rome?"}]}
        ],
        "tools": [
            {"type": "function", "name": "get_weather", "description": "Get the weather", "parameters": weather_schema()},
            {"type": "function", "name": "get_time", "description": "Get the time", "parameters": {"type": "object", "properties": {}}}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "temperature": 0.2,
        "top_p": 0.9,
        "max_output_tokens": 300,
        "metadata": {"k": "v"}
    })
}

fn claude(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "max_tokens": 300,
        "system": [{"type": "text", "text": "You are a terse weather bot.", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "What does this look like and what is the weather in Paris?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG_B64}}
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_abc123", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_abc123", "content": "18C and sunny"}]},
            {"role": "user", "content": "Thanks. And in Rome?"}
        ],
        "tools": [
            {"name": "get_weather", "description": "Get the weather", "input_schema": weather_schema()},
            {"name": "get_time", "description": "Get the time", "input_schema": {"type": "object", "properties": {}}}
        ],
        "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        "temperature": 0.2,
        "top_p": 0.9,
        "stop_sequences": ["END"],
        "metadata": {"user_id": "user-1"}
    })
}

fn gemini() -> Value {
    json!({
        "systemInstruction": {"parts": [{"text": "You are a terse weather bot."}]},
        "contents": [
            {"role": "user", "parts": [
                {"text": "What does this look like and what is the weather in Paris?"},
                {"inlineData": {"mimeType": "image/png", "data": PNG_B64}}
            ]},
            {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"result": "18C and sunny"}}}]},
            {"role": "user", "parts": [{"text": "Thanks. And in Rome?"}]}
        ],
        "tools": [{"functionDeclarations": [
            {"name": "get_weather", "description": "Get the weather", "parameters": weather_schema()},
            {"name": "get_time", "description": "Get the time"}
        ]}],
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
        "generationConfig": {"temperature": 0.2, "topP": 0.9, "maxOutputTokens": 300, "stopSequences": ["END"]}
    })
}

struct Dialect {
    name: &'static str,
    request: fn(Family, bool) -> HttpReq,
}

const DIALECTS: [Dialect; 4] = [
    Dialect { name: "chat", request: |f, s| HttpReq::post("/v1/chat/completions", chat(f.model(), s)) },
    Dialect { name: "responses", request: |f, s| HttpReq::post("/v1/responses", responses(f.model(), s)) },
    Dialect { name: "claude", request: |f, s| HttpReq::post("/v1/messages", claude(f.model(), s)) },
    Dialect {
        name: "gemini",
        request: |f, s| {
            let method = if s { "streamGenerateContent" } else { "generateContent" };
            HttpReq::post(&super::bodies::gemini_path(f.model(), method), gemini())
        },
    },
];

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for d in &DIALECTS {
        for f in FAMILIES {
            for stream in [false, true] {
                let mode = if stream { "stream" } else { "json" };
                out.push(Scenario::new(
                    format!("rich.{}.{}.{mode}", d.name, f.label()),
                    format!("multi-turn request with system prompt, image, tool round trip and sampling options: {} -> {}", d.name, f.label()),
                    Script::ok(Content::Text),
                    vec![Req::Http((d.request)(f, stream))],
                ));
            }
        }
    }
    out
}
