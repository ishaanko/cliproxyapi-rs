//! Replays `conformance/translator_cases.jsonl.gz` (inputs captured from the Go test suite,
//! expected outputs recorded from the Go translators) through the Rust registry.
//!
//! Usage: cpa-conformance [--pair client:upstream] [--kind request|stream|nonstream|token_count]
//!                        [--show N] [--cases PATH]
//!
//! Cases run in file order in one process, matching how the oracle recorded them (global
//! caches such as the signature cache evolve identically). Filtering skips cases, which can
//! occasionally change cache history; always confirm with a full run.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::panic::{catch_unwind, AssertUnwindSafe};

use cpa_json::{Map, Value};
use cpa_translator::{Ctx, Format, Param};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let pair_filter = arg("--pair");
    let kind_filter = arg("--kind");
    let show: usize = arg("--show").and_then(|s| s.parse().ok()).unwrap_or(0);
    let path = arg("--cases").unwrap_or_else(|| "conformance/translator_cases.jsonl.gz".into());

    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let reader: Box<dyn BufRead> = if path.ends_with(".gz") {
        Box::new(BufReader::new(flate2::read::GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    std::panic::set_hook(Box::new(|_| {}));
    let mut stats: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut shown = 0;
    for (lineno, line) in reader.lines().enumerate() {
        let line = line.expect("read line");
        let c: Value = serde_json::from_str(&line).expect("case json");
        let kind = c["kind"].as_str().unwrap_or_default().to_string();
        let client = c["client"].as_str().unwrap_or_default().to_string();
        let upstream = c["upstream"].as_str().unwrap_or_default().to_string();
        let pair = format!("{client}:{upstream}");
        if pair_filter.as_ref().is_some_and(|p| p != &pair) || kind_filter.as_ref().is_some_and(|k| k != &kind) {
            continue;
        }
        let expected = c["expect"].get("out").cloned().unwrap_or(Value::Null);
        let actual = catch_unwind(AssertUnwindSafe(|| run_case(&c))).unwrap_or_else(|e| {
            let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
            Value::String(format!("<panic: {}>", msg.unwrap_or_default()))
        });
        let ok = match diff(&kind, &expected, &actual) {
            None => true,
            Some(d) => {
                if shown < show {
                    shown += 1;
                    println!("--- FAIL case #{lineno} {kind} {pair} model={}\n{d}\n", c["model"]);
                }
                false
            }
        };
        let e = stats.entry(format!("{kind:<11} {pair}")).or_default();
        e.1 += 1;
        if ok {
            e.0 += 1;
        }
    }
    let (mut pass, mut total) = (0, 0);
    for (k, (p, t)) in &stats {
        pass += p;
        total += t;
        let mark = if p == t { " " } else { "x" };
        println!("{mark} {k:<50} {p:>5}/{t:<5}");
    }
    println!("TOTAL {pass}/{total}");
    if pass != total {
        std::process::exit(1);
    }
}

fn fmt(c: &Value, key: &str) -> Format {
    let s = c[key].as_str().unwrap_or_default();
    Format::parse(s).unwrap_or_else(|| panic!("unknown format {s}"))
}

fn s<'a>(c: &'a Value, key: &str) -> &'a str {
    c[key].as_str().unwrap_or_default()
}

fn run_case(c: &Value) -> Value {
    let (client, upstream) = (fmt(c, "client"), fmt(c, "upstream"));
    let model = s(c, "model");
    let ctx = Ctx::default();
    match s(c, "kind") {
        "request" => {
            let out = cpa_translator::translate_request(client, upstream, model, s(c, "body").as_bytes(), c["stream"].as_bool().unwrap_or(false));
            Value::String(String::from_utf8_lossy(&out).into_owned())
        }
        "nonstream" => {
            let mut p = Param::default();
            match cpa_translator::translate_non_stream(&ctx, upstream, client, model, s(c, "original").as_bytes(), s(c, "translated").as_bytes(), s(c, "body").as_bytes(), &mut p) {
                Some(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
                None => Value::Null,
            }
        }
        "stream" => {
            let mut p = Param::default();
            let lines = c["lines"].as_array().cloned().unwrap_or_default();
            Value::Array(
                lines
                    .iter()
                    .map(|l| {
                        let chunks = cpa_translator::translate_stream(&ctx, upstream, client, model, s(c, "original").as_bytes(), s(c, "translated").as_bytes(), l.as_str().unwrap_or_default().as_bytes(), &mut p);
                        Value::Array(chunks.into_iter().map(|b| Value::String(String::from_utf8_lossy(&b).into_owned())).collect())
                    })
                    .collect(),
            )
        }
        "token_count" => {
            let out = cpa_translator::translate_token_count(&ctx, upstream, client, c["count"].as_i64().unwrap_or(0), b"");
            Value::String(String::from_utf8_lossy(&out).into_owned())
        }
        k => panic!("unknown kind {k}"),
    }
}

// ------------------------------------------------------------------ comparison

/// Parse a translator output string into comparable structure: JSON when possible, SSE
/// frames as a list of {event?, data} objects, otherwise the trimmed string.
fn structure(s: &str) -> Value {
    let t = s.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return v;
    }
    if t.starts_with("event:") || t.starts_with("data:") {
        let mut frames = vec![];
        let mut cur = Map::new();
        for line in t.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                if !cur.is_empty() {
                    frames.push(Value::Object(std::mem::take(&mut cur)));
                }
            } else if let Some(ev) = line.strip_prefix("event:") {
                cur.insert("event".into(), Value::String(ev.trim().into()));
            } else if let Some(d) = line.strip_prefix("data:") {
                let d = d.trim();
                cur.insert("data".into(), serde_json::from_str(d).unwrap_or_else(|_| Value::String(d.into())));
            } else {
                cur.insert("other".into(), Value::String(line.into()));
            }
        }
        if !cur.is_empty() {
            frames.push(Value::Object(cur));
        }
        return Value::Array(frames);
    }
    Value::String(t.to_string())
}

fn structure_output(kind: &str, v: &Value) -> Value {
    match (kind, v) {
        ("stream", Value::Array(lines)) => Value::Array(
            lines
                .iter()
                .map(|chunks| match chunks {
                    Value::Array(cs) => Value::Array(cs.iter().map(|c| structure(c.as_str().unwrap_or_default())).collect()),
                    other => other.clone(),
                })
                .collect(),
        ),
        (_, Value::String(s)) => structure(s),
        (_, other) => other.clone(),
    }
}

const TS_KEYS: [&str; 6] = ["created", "created_at", "createTime", "updated", "createdAt", "completed_at"];

fn is_id_key(k: &str) -> bool {
    k == "id" || k.ends_with("_id") || k.ends_with("Id")
}

/// Generated identifiers: long-ish with a random-looking alphanumeric tail.
fn looks_generated(s: &str) -> bool {
    let tail = s.rsplit(['_', '-']).next().unwrap_or(s);
    s.len() >= 12 && tail.len() >= 8 && tail.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Replace generated ids with first-appearance placeholders and timestamps with "<ts>".
fn mask(v: &mut Value, ids: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if TS_KEYS.contains(&k.as_str()) && (val.is_number() || val.is_string()) {
                    *val = Value::String("<ts>".into());
                    continue;
                }
                if is_id_key(k) {
                    if let Value::String(s) = val {
                        if looks_generated(s) {
                            let n = ids.iter().position(|x| x == s).unwrap_or_else(|| {
                                ids.push(s.clone());
                                ids.len() - 1
                            });
                            *val = Value::String(format!("<id#{n}>"));
                            continue;
                        }
                    }
                }
                mask(val, ids);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| mask(x, ids)),
        _ => {}
    }
}

fn num_eq(a: &cpa_json::Number, b: &cpa_json::Number) -> bool {
    a.to_string() == b.to_string() || a.to_string().parse::<f64>().ok() == b.to_string().parse::<f64>().ok()
}

/// First difference as `path: expected != actual`, or None when equal.
fn first_diff(e: &Value, a: &Value, path: &str) -> Option<String> {
    match (e, a) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, ev) in x {
                match y.get(k) {
                    None => return Some(format!("{path}.{k}: missing in actual (expected {})", trunc(ev))),
                    Some(av) => {
                        if let Some(d) = first_diff(ev, av, &format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                }
            }
            y.keys().find(|k| !x.contains_key(*k)).map(|k| format!("{path}.{k}: unexpected in actual ({})", trunc(&y[k])))
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (ev, av)) in x.iter().zip(y).enumerate() {
                if let Some(d) = first_diff(ev, av, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            (x.len() != y.len()).then(|| format!("{path}: len {} != {} (expected tail {})", x.len(), y.len(), trunc(&Value::Array(x.iter().skip(y.len()).take(2).cloned().collect()))))
        }
        (Value::Number(x), Value::Number(y)) if num_eq(x, y) => None,
        _ if e == a => None,
        _ => Some(format!("{path}: expected {} != actual {}", trunc(e), trunc(a))),
    }
}

fn trunc(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 300 { format!("{}…", &s[..s.char_indices().nth(300).map(|x| x.0).unwrap_or(s.len())]) } else { s }
}

fn diff(kind: &str, expected: &Value, actual: &Value) -> Option<String> {
    let mut e = structure_output(kind, expected);
    let mut a = structure_output(kind, actual);
    mask(&mut e, &mut vec![]);
    mask(&mut a, &mut vec![]);
    first_diff(&e, &a, "$")
}
