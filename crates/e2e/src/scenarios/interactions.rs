//! Gemini Interactions API entry (`/v1beta/interactions`), served by regular Gemini keys.

use serde_json::{Value, json};

use super::bodies::Family;
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::{Content, Script};
use crate::scenario::Scenario;

pub fn scenarios() -> Vec<Scenario> {
    let model = Family::Gemini.model();
    let post = |body: Value| Req::Http(HttpReq::post("/v1beta/interactions", body));
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("interactions.{id}"), desc, Script::ok(Content::Text), steps);
    vec![
        s("json", "Interactions request translated to generateContent", vec![post(json!({"model": model, "input": "hello"}))]),
        s("stream", "streaming Interactions request", vec![post(json!({"model": model, "input": "hello", "stream": true}))]),
        s(
            "errors",
            "invalid Interactions bodies",
            vec![
                post(json!({})),
                post(json!({"model": model, "agent": "deep-research", "input": "x"})),
                post(json!({"model": model, "input": "x", "stream": "yes"})),
                Req::Http(HttpReq::post("/v1beta/interactions", json!({})).raw("{bad")),
            ],
        ),
    ]
}
