//! Masks volatile values (generated ids, timestamps, ports, temp paths) with consistent
//! placeholders so captures from different runs and different servers compare equal.

use std::collections::{BTreeMap, HashMap};

use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde_json::Value;

/// Generated-id shapes. Ids containing "mock" come from the mock upstream and must pass
/// through unchanged, so only ids that the server generated are masked.
static ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(chatcmpl-|call_|toolu_|msg_|resp_|fc_|rs_|req_|item_|ctc_|ctco_|run_|cmpl-|chatcmpl_)([A-Za-z0-9_-]{8,})").expect("id regex")
});
static UUID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").expect("uuid regex"));
/// Generated ids embed the unix nanosecond clock (`<name>-<19 digits>-<counter>`, `interaction_<19 digits>`).
static NANOS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(^|[^0-9])(\d{19})([^0-9]|$)").expect("nanos regex"));
/// bcrypt hashes are salted randomly on every server start.
static BCRYPT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$2[aby]\$\d\d\$[./A-Za-z0-9]{53}").expect("bcrypt regex"));
/// Remaining ban time in the management IP-ban message.
static BAN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"Try again in [0-9hms.]+").expect("ban regex"));
/// Ten-minute wall-clock buckets in management request histories (`20:20-20:30`).
static BUCKET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{2}:\d{2}-\d{2}:\d{2}\b").expect("bucket regex"));
static TS14_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{14}-").expect("ts14 regex"));
static RFC3339_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)").expect("rfc3339 regex")
});

/// Keys whose values are always volatile regardless of content.
const VOLATILE_KEYS: &[&str] =
    &["latency_ms", "ttft_ms", "request_id", "trace_id", "execution_id", "elapsed_ms", "reset_seconds", "reset_time", "Date", "date"];

/// Request headers dropped from upstream captures (transport detail).
const DROP_REQUEST_HEADERS: &[&str] = &["host", "content-length", "connection", "accept-encoding", "sec-websocket-key"];

pub struct Normalizer {
    mock_port: u16,
    server_port: u16,
    work_dir: String,
    /// Unix seconds at harness start; values within a day of it are "now".
    now: i64,
    seen: HashMap<String, String>,
    counters: HashMap<&'static str, usize>,
}

impl Normalizer {
    pub fn new(mock_port: u16, server_port: u16, work_dir: &str) -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        Normalizer { mock_port, server_port, work_dir: work_dir.to_string(), now, seen: HashMap::new(), counters: HashMap::new() }
    }

    fn placeholder(&mut self, kind: &'static str, original: &str) -> String {
        let key = format!("{kind}\0{original}");
        if let Some(p) = self.seen.get(&key) {
            return p.clone();
        }
        let n = self.counters.entry(kind).and_modify(|n| *n += 1).or_insert(1);
        let p = format!("<{kind}:{n}>");
        self.seen.insert(key, p.clone());
        p
    }

    fn near_now(&self, secs: i64) -> bool {
        (secs - self.now).abs() < 86_400
    }

    pub fn string(&mut self, s: &str) -> String {
        let mut out = s
            .replace(&format!("127.0.0.1:{}", self.mock_port), "<mock>")
            .replace(&format!("127.0.0.1:{}", self.server_port), "<server>")
            .replace(&format!("localhost:{}", self.server_port), "<server>")
            .replace(&self.work_dir, "<work>");
        out = UUID_RE.replace_all(&out, |c: &Captures| self.placeholder("uuid", &c[0].to_lowercase())).into_owned();
        out = ID_RE
            .replace_all(&out, |c: &Captures| {
                if c[2].contains("mock") {
                    return c[0].to_string();
                }
                let kind = match &c[1] {
                    "chatcmpl-" | "chatcmpl_" | "cmpl-" => "chatcmpl",
                    "call_" => "call",
                    "toolu_" => "toolu",
                    "msg_" => "msg",
                    "resp_" => "resp",
                    "fc_" => "fc",
                    "rs_" => "rs",
                    "req_" => "req",
                    "item_" => "item",
                    "ctc_" => "ctc",
                    "ctco_" => "ctco",
                    _ => "id",
                };
                self.placeholder(kind, &c[0])
            })
            .into_owned();
        out = BCRYPT_RE.replace_all(&out, "<bcrypt>").into_owned();
        out = BUCKET_RE.replace_all(&out, "<time-bucket>").into_owned();
        out = BAN_RE.replace_all(&out, "Try again in <duration>").into_owned();
        out = TS14_RE.replace_all(&out, "<ts14>-").into_owned();
        out = NANOS_RE
            .replace_all(&out, |c: &Captures| {
                let near = c[2].parse::<i64>().is_ok_and(|ns| self.near_now(ns / 1_000_000_000));
                if near { format!("{}<nanos>{}", &c[1], &c[3]) } else { c[0].to_string() }
            })
            .into_owned();
        out = RFC3339_RE
            .replace_all(&out, |c: &Captures| {
                match chrono::DateTime::parse_from_rfc3339(&c[0]) {
                    Ok(t) if self.near_now(t.timestamp()) => "<now>".to_string(),
                    _ => c[0].to_string(),
                }
            })
            .into_owned();
        out
    }

    /// Normalizes a JSON tree in place: strings, near-now numbers, volatile keys, model-list order.
    pub fn value(&mut self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.string(s),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if self.near_now(i) {
                        *v = Value::String("<now>".into());
                    } else if self.near_now(i / 1000) && i > 100_000_000_000 {
                        *v = Value::String("<now_ms>".into());
                    }
                }
            }
            Value::Array(a) => {
                for x in a.iter_mut() {
                    self.value(x);
                }
            }
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if VOLATILE_KEYS.contains(&k.as_str()) && !x.is_null() {
                        *x = Value::String(format!("<{k}>"));
                    } else {
                        self.value(x);
                    }
                    if (k == "data" || k == "models") && let Value::Array(items) = x {
                        sort_listing(items);
                    }
                }
            }
            _ => {}
        }
    }

    /// Upstream request headers: drop transport noise, mask volatile values.
    pub fn request_headers(&mut self, headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        headers
            .iter()
            .filter(|(k, _)| !DROP_REQUEST_HEADERS.contains(&k.as_str()))
            .map(|(k, v)| {
                let v = if k == "user-agent" && v.starts_with("Go-http-client") { "<go-default-ua>".to_string() } else { self.string(v) };
                (k.clone(), v)
            })
            .collect()
    }

    pub fn headers(&mut self, headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        headers.iter().map(|(k, v)| (k.clone(), self.string(v))).collect()
    }
}

/// Model listings come out of Go map iteration in random order; sort them by id/name.
fn sort_listing(items: &mut [Value]) {
    let key_of = |v: &Value| v.get("id").or_else(|| v.get("name")).and_then(|k| k.as_str()).map(str::to_string);
    if items.iter().all(|v| v.is_object() && key_of(v).is_some()) {
        items.sort_by_key(|v| key_of(v).unwrap_or_default());
    }
}
