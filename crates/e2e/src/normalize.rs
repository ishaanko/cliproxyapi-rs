//! Masks values that are random or clock-dependent by construction (v4/v7 uuids, generated id
//! tails, near-now timestamps, ports, temp paths) with consistent placeholders. Anything else
//! that merely happens to vary is caught by `record`, which learns the differing paths of a
//! scenario from two runs (see `golden::learn_volatile`).
//!
//! Deterministic values (for example UUIDv5 derived from an API key) are deliberately kept.

use std::collections::{BTreeMap, HashMap};

use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde_json::Value;

/// Generated-id shapes: a literal prefix (kept, so `chatcmpl-` and `chatcmpl_` stay distinct)
/// followed by a random alphanumeric tail (masked). Tails containing "mock" come from the mock
/// upstream and must pass through unchanged.
static ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(chatcmpl-|chatcmpl_|cmpl-|call_|toolu_|msg_|resp_|fc_|rs_|req_|item_|ctc_|ctco_|run_)([A-Za-z0-9]{8,})").expect("id regex")
});
/// Random UUIDs only (version 4 and 7); name-based UUIDs (v5) are deterministic and kept.
static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[47][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}").expect("uuid regex")
});
/// Generated ids embed the unix nanosecond clock (`<name>-<19 digits>-<counter>`, `interaction_<19 digits>`).
static NANOS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(^|[^0-9])(\d{19})([^0-9]|$)").expect("nanos regex"));
/// bcrypt hashes are salted randomly on every server start.
static BCRYPT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$2[aby]\$\d\d\$[./A-Za-z0-9]{53}").expect("bcrypt regex"));
/// Remaining ban time in the management IP-ban message.
static BAN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"Try again in [0-9hms.]+").expect("ban regex"));
/// Ten-minute wall-clock buckets in management request histories (`20:20-20:30`).
static BUCKET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{2}:\d{2}-\d{2}:\d{2}\b").expect("bucket regex"));
/// `X-Cpa-Trace-Id: <YYYYMMDDHHMMSS>-<auth index>-<request id>`: only the timestamp is masked here
/// (the request id is a v7 uuid; the auth index is deterministic and stays, it checks credential ids).
static TS14_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{14}-").expect("ts14 regex"));
/// RFC 1123 HTTP dates (upstream `Date` headers echoed into bodies).
static HTTP_DATE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun), \d{2} (?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d{4} \d{2}:\d{2}:\d{2} GMT").expect("http date regex")
});
static RFC3339_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)").expect("rfc3339 regex")
});

/// Keys whose values are always volatile regardless of content.
const VOLATILE_KEYS: &[&str] =
    &["latency_ms", "ttft_ms", "request_id", "trace_id", "execution_id", "elapsed_ms", "reset_seconds", "reset_time", "Date", "date"];

/// Request headers dropped from upstream captures (derived from the body or the connection).
const DROP_REQUEST_HEADERS: &[&str] = &["host", "content-length", "sec-websocket-key"];

pub struct Normalizer {
    mock_port: u16,
    server_port: u16,
    work_dir: String,
    /// Unix seconds at harness start; values within a day of it are "now".
    now: i64,
    seen: HashMap<String, usize>,
    counters: HashMap<String, usize>,
    /// `(auth index, placeholder)` of credential files, whose index hashes the absolute path.
    auth_indexes: Vec<(String, String)>,
}

impl Normalizer {
    pub fn new(mock_port: u16, server_port: u16, work_dir: &str) -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        Normalizer { mock_port, server_port, work_dir: work_dir.to_string(), now, seen: HashMap::new(), counters: HashMap::new(), auth_indexes: vec![] }
    }

    /// Registers a credential file (`<type>:<absolute path>` is the auth index seed) so its index,
    /// which depends on where the work dir lives, is masked as `<auth-index:name>`.
    pub fn mask_auth_file(&mut self, auth_type: &str, path: &str, name: &str) {
        use sha2::{Digest, Sha256};
        let sum = Sha256::digest(format!("{}:{path}", auth_type.trim().to_lowercase()).as_bytes());
        let index: String = sum[..8].iter().map(|b| format!("{b:02x}")).collect();
        self.auth_indexes.push((index, format!("<auth-index:{name}>")));
    }

    /// Stable index for `original` within `kind`: the same value always gets the same number.
    fn number(&mut self, kind: &str, original: &str) -> usize {
        let key = format!("{kind}\0{original}");
        if let Some(n) = self.seen.get(&key) {
            return *n;
        }
        let n = self.counters.entry(kind.to_string()).and_modify(|n| *n += 1).or_insert(1);
        self.seen.insert(key, *n);
        *n
    }

    fn near_now(&self, secs: i64) -> bool {
        (secs - self.now).abs() < 86_400
    }

    pub fn string(&mut self, s: &str) -> String {
        let mut out = s.to_string();
        for (index, placeholder) in &self.auth_indexes {
            out = out.replace(index.as_str(), placeholder);
        }
        let mut out = out
            .replace(&format!("127.0.0.1:{}", self.mock_port), "<mock>")
            .replace(&format!("127.0.0.1:{}", self.server_port), "<server>")
            .replace(&format!("localhost:{}", self.server_port), "<server>")
            .replace(&self.work_dir, "<work>");
        out = UUID_RE.replace_all(&out, |c: &Captures| format!("<uuid:{}>", self.number("uuid", &c[0]))).into_owned();
        out = ID_RE
            .replace_all(&out, |c: &Captures| {
                if c[2].contains("mock") {
                    return c[0].to_string();
                }
                format!("{}<id:{}>", &c[1], self.number(&c[1], &c[2]))
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
        out = HTTP_DATE_RE.replace_all(&out, "<http-date>").into_owned();
        out = RFC3339_RE
            .replace_all(&out, |c: &Captures| match chrono::DateTime::parse_from_rfc3339(&c[0]) {
                Ok(t) if self.near_now(t.timestamp()) => "<now>".to_string(),
                _ => c[0].to_string(),
            })
            .into_owned();
        out
    }

    /// Normalizes a JSON tree in place: strings, near-now numbers and volatile keys.
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
                // File-backed credentials hash their absolute path into `auth_index`.
                let file_backed = m.get("path").and_then(Value::as_str).is_some_and(|p| p.contains(&self.work_dir));
                for (k, x) in m.iter_mut() {
                    if VOLATILE_KEYS.contains(&k.as_str()) && !x.is_null() {
                        *x = Value::String(format!("<{}>", k.to_lowercase()));
                    } else if file_backed && matches!(k.as_str(), "auth_index" | "auth-index") {
                        *x = Value::String("<file-auth-index>".into());
                    } else {
                        self.value(x);
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
            // reqwest always sends `Accept: */*` when no Accept is set; Go sends none. The two are
            // equivalent to every upstream, so the literal default is treated as absent. Any other
            // Accept value is still compared.
            .filter(|(k, v)| !(k.as_str() == "accept" && v.as_str() == "*/*"))
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

/// Model listings come out of Go map iteration in random order; sort them by id/name. Only for
/// client responses of the model-list endpoints (other lists have a fixed order that matters).
pub fn sort_model_listing(v: &mut Value) {
    let Value::Object(m) = v else { return };
    for key in ["data", "models"] {
        if let Some(Value::Array(items)) = m.get_mut(key) {
            let key_of = |v: &Value| v.get("id").or_else(|| v.get("name")).and_then(|k| k.as_str()).map(str::to_string);
            if items.iter().all(|v| v.is_object() && key_of(v).is_some()) {
                items.sort_by_key(|v| key_of(v).unwrap_or_default());
            }
        }
    }
}
