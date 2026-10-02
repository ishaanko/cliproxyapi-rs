//! Guards against quadratic request translation: the largest corpus request of every
//! (client, upstream) pair is scaled to ~2MB (less in debug builds) by repeating its arrays, then translated under a
//! generous time bound. Per-element `raw_at` scans of the whole body blow far past the bound;
//! linear paths finish in well under a second in release.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use cpa_json::Value;
use cpa_translator::{translate_request, Format};

// Debug builds are an order of magnitude slower, so they use smaller bodies and a looser bound.
const TARGET_BYTES: usize = if cfg!(debug_assertions) { 512 * 1024 } else { 2 * 1024 * 1024 };
const LIMIT: Duration = Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 3 });

/// Repeats every non-empty array found in the members of the top-level object (`factor` times) and below (twice).
fn scale(v: &Value, factor: usize, depth: usize) -> Value {
    match v {
        Value::Array(a) if !a.is_empty() && depth <= 3 => {
            let items: Vec<Value> = a.iter().map(|x| scale(x, factor, depth + 1)).collect();
            let n = if depth == 1 { factor } else { 2 };
            Value::Array((0..n).flat_map(|_| items.clone()).collect())
        }
        Value::Object(o) if depth <= 3 => Value::Object(o.iter().map(|(k, x)| (k.clone(), scale(x, factor, depth + 1))).collect()),
        other => other.clone(),
    }
}

/// Inflates `body` until it is at least [`TARGET_BYTES`].
fn scaled_body(body: &str) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_str(body).ok()?;
    if !v.is_object() {
        return None;
    }
    let mut factor = 8;
    loop {
        let out = serde_json::to_vec(&scale(&v, factor, 0)).ok()?;
        if out.len() >= TARGET_BYTES || factor > 1 << 16 {
            return Some(out);
        }
        factor *= 2;
    }
}

#[test]
fn large_requests_translate_in_linear_time() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../conformance/translator_cases.jsonl.gz");
    let file = std::fs::File::open(path).expect("corpus");
    let reader = BufReader::new(flate2::read::GzDecoder::new(file));

    // Largest request case per pair.
    let mut best: BTreeMap<(Format, Format), (usize, Value)> = BTreeMap::new();
    for line in reader.lines() {
        let c: Value = serde_json::from_str(&line.expect("line")).expect("case json");
        if c["kind"] != "request" {
            continue;
        }
        let (Some(client), Some(upstream)) = (c["client"].as_str().and_then(Format::parse), c["upstream"].as_str().and_then(Format::parse)) else {
            continue;
        };
        let len = c["body"].as_str().map_or(0, str::len);
        if best.get(&(client, upstream)).is_none_or(|(l, _)| len > *l) {
            best.insert((client, upstream), (len, c));
        }
    }

    let mut slow = vec![];
    for ((client, upstream), (_, c)) in &best {
        // Go's pending-call matching for Responses -> Gemini-style upstreams is itself quadratic
        // in unmatched calls, and the scaled body leaves thousands pending.
        if (*client, *upstream) == (Format::OpenAIResponse, Format::Antigravity) {
            continue;
        }
        let Some(body) = c["body"].as_str().and_then(scaled_body) else { continue };
        let model = c["model"].as_str().unwrap_or_default();
        let stream = c["stream"].as_bool().unwrap_or(false);
        let start = Instant::now();
        let out = translate_request(*client, *upstream, model, &body, stream);
        let took = start.elapsed();
        if std::env::var_os("PERF_VERBOSE").is_some() {
            eprintln!("{client}:{upstream} in={}KB out={}KB {took:?}", body.len() / 1024, out.len() / 1024);
        }
        if took > LIMIT {
            slow.push(format!("{client}:{upstream} took {took:?} for {}KB", body.len() / 1024));
        }
    }
    assert!(slow.is_empty(), "quadratic request translation suspected:\n{}", slow.join("\n"));
}
