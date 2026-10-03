//! `fetch_devin_models`: fetches the Devin/Codeium model catalog through Connect-RPC and writes it
//! to a JSON file for offline catalog updates (Go: cmd/fetch_devin_models).

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use cpa_auth::types::Auth;
use cpa_config::Config;
use serde::Serialize;

use crate::common::{go_marshal, init_logging, list_auths, setup};
use crate::flags::{self, FlagDef, Kind};

const GET_CLI_MODEL_CONFIGS_URL: &str = "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const DEFAULT_USER_AGENT: &str = "connect-go/1.19.1 (go1.25.0)";

const FLAGS: &[FlagDef] = &[
    FlagDef { name: "auths-dir", kind: Kind::Str, default: "", usage: "Directory containing auth JSON files (overrides config auth-dir)" },
    FlagDef { name: "config", kind: Kind::Str, default: "", usage: "Configure File Path" },
    FlagDef { name: "output", kind: Kind::Str, default: "devin_models.json", usage: "Output JSON file path" },
    FlagDef { name: "raw", kind: Kind::Bool, default: "false", usage: "Dump all raw model configurations without aggregation" },
    FlagDef { name: "pretty", kind: Kind::Bool, default: "true", usage: "Pretty-print the output JSON" },
];

#[derive(Serialize)]
struct CatalogOutput {
    devin: Vec<ModelJson>,
}

/// One catalog entry; zero values and empty lists are omitted like Go's `omitempty`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelJson {
    pub id: String,
    pub object: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub owned_by: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub context_length: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub max_completion_tokens: i64,
    #[serde(rename = "supportedInputModalities", skip_serializing_if = "Vec::is_empty")]
    pub supported_input_modalities: Vec<String>,
    #[serde(rename = "supportedOutputModalities", skip_serializing_if = "Vec::is_empty")]
    pub supported_output_modalities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingJson {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// A model configuration as sent upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawModel {
    pub uid: String,
    pub label: String,
    pub context_length: i64,
    pub multimodal: bool,
    pub vendor_id: u64,
}

/// Program entry; returns the process exit code.
pub async fn run(args: Vec<String>) -> i32 {
    init_logging();
    let parsed = match flags::parse("fetch_devin_models", FLAGS, &args) {
        Ok(p) => p,
        Err(flags::Exit(code)) => return code,
    };
    match run_inner(&parsed).await {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

async fn run_inner(parsed: &flags::Flags) -> Result<(), String> {
    let env = setup(
        &parsed.string("auths-dir"),
        parsed.was_set("auths-dir"),
        &parsed.string("config"),
        &parsed.string("output"),
    )?;

    println!("Scanning auth files in: {}", env.auths_dir);
    let auths = list_auths(&env.auths_dir)?;

    let Some(chosen) = auths.iter().find(|a| !a.disabled && a.provider.trim().eq_ignore_ascii_case("devin")) else {
        return Err(format!("error: no enabled devin auth found in {}", env.auths_dir));
    };

    let mut api_key = chosen.attributes.get("api_key").cloned().unwrap_or_default();
    if api_key.is_empty() {
        api_key = chosen.metadata.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
    }
    if api_key.is_empty() {
        return Err(format!("error: devin auth {} has no api_key", chosen.id));
    }

    println!("Using auth: id={} label={}", chosen.id, chosen.label);
    println!("Fetching Devin model catalog from upstream...");

    let raw_models = fetch_raw_models(&env.cfg, chosen, &api_key, GET_CLI_MODEL_CONFIGS_URL)
        .await
        .map_err(|e| format!("error: fetch failed: {e}"))?;
    println!("Successfully fetched {} raw model configs from upstream.", raw_models.len());

    let devin = if parsed.boolean("raw") { format_raw_models(&raw_models) } else { aggregate_models(&raw_models) };
    let count = devin.len();
    let encoded = go_marshal(&CatalogOutput { devin }, parsed.boolean("pretty"))
        .map_err(|e| format!("error: failed to encode JSON: {e}"))?;

    if let Some(dir) = env.output_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("error: failed to create output directory: {e}"))?;
    }
    std::fs::write(&env.output_path, encoded).map_err(|e| format!("error: failed to write output file: {e}"))?;
    println!("Catalog written to: {} ({count} models)", env.output_path.display());
    Ok(())
}

// Protobuf wire helpers (the subset of `protowire` the tool needs).

fn append_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// `ConsumeVarint`: `(value, length)`; `None` when truncated or longer than 10 bytes.
fn consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &byte) in b.iter().enumerate().take(10) {
        if i == 9 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Some((v, i + 1));
        }
    }
    None
}

/// `ConsumeTag`: `(field number, wire type, length)`; field numbers must be 1..=i32::MAX.
fn consume_tag(b: &[u8]) -> Option<(u32, u8, usize)> {
    let (v, n) = consume_varint(b)?;
    if v >> 3 > i32::MAX as u64 || v >> 3 < 1 {
        return None;
    }
    Some(((v >> 3) as u32, (v & 7) as u8, n))
}

/// `ConsumeBytes`: `(payload, total length)`.
fn consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (len, n) = consume_varint(b)?;
    let len = usize::try_from(len).ok()?;
    let end = n.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((&b[n..end], end))
}

/// Group nesting limit when skipping unknown fields. Go allows 10000 (goroutine stacks grow); the
/// limit is lower here so hostile input cannot overflow the thread stack.
const MAX_GROUP_DEPTH: i32 = 100;

/// `ConsumeFieldValue`: length of the value of a field of type `typ`; groups recurse.
fn consume_field_value(num: u32, typ: u8, b: &[u8], depth: i32) -> Option<usize> {
    match typ {
        0 => consume_varint(b).map(|(_, n)| n),
        5 => (b.len() >= 4).then_some(4),
        1 => (b.len() >= 8).then_some(8),
        2 => consume_bytes(b).map(|(_, n)| n),
        3 => {
            if depth < 0 {
                return None;
            }
            let total = b.len();
            let mut rest = b;
            loop {
                let (num2, typ2, n) = consume_tag(rest)?;
                rest = &rest[n..];
                if typ2 == 4 {
                    return (num == num2).then(|| total - rest.len());
                }
                let n = consume_field_value(num2, typ2, rest, depth - 1)?;
                rest = &rest[n..];
            }
        }
        _ => None,
    }
}

/// The `GetCliModelConfigs` request body: field 1 holds the client metadata.
fn build_request_body(api_key: &str) -> Vec<u8> {
    let metadata = cpa_executors::devin::wire::build_client_metadata_bytes(api_key, "", "");
    let mut body = Vec::new();
    append_varint(&mut body, (1 << 3) | 2);
    append_varint(&mut body, metadata.len() as u64);
    body.extend_from_slice(&metadata);
    body
}

async fn fetch_raw_models(cfg: &Config, auth: &Auth, api_key: &str, url: &str) -> Result<Vec<RawModel>, String> {
    let timeout = Duration::from_secs(30);
    let client = cpa_executors::helps::proxy::new_proxy_aware_http_client("", Some(cfg), Some(auth), Some(timeout));
    let request = client
        .post(url)
        .header("Authorization", format!("Basic {api_key}-{api_key}"))
        .header("Content-Type", "application/proto")
        .header("Connect-Protocol-Version", "1")
        .header("User-Agent", DEFAULT_USER_AGENT)
        .body(build_request_body(api_key));
    let response = tokio::time::timeout(timeout, async {
        let response = request.send().await.map_err(|e| format!("do request: {e}"))?;
        let status = response.status();
        if status.as_u16() != 200 {
            let body = response.bytes().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body[..body.len().min(1024)]).into_owned();
            // The Go tool passes the body snippet through the proxy URL redactor.
            return Err(format!("upstream returned status {}: {}", status.as_u16(), cpa_misc::proxyutil::redact(&snippet)));
        }
        response.bytes().await.map_err(|e| format!("read response body: {e}"))
    })
    .await
    .map_err(|_| "do request: context deadline exceeded".to_string())??;
    parse_raw_models_proto(&response)
}

/// Decodes the response: repeated field 1 submessages, each a model configuration.
pub fn parse_raw_models_proto(mut b: &[u8]) -> Result<Vec<RawModel>, String> {
    let mut results = Vec::new();
    while !b.is_empty() {
        let (num, typ, n) = consume_tag(b).ok_or("malformed protobuf tag")?;
        b = &b[n..];
        if num == 1 && typ == 2 {
            let (sub, m) = consume_bytes(b).ok_or("malformed protobuf submessage")?;
            b = &b[m..];
            let model = parse_single_model_config(sub);
            if !model.uid.is_empty() {
                results.push(model);
            }
        } else {
            let skip = consume_field_value(num, typ, b, MAX_GROUP_DEPTH).ok_or("failed to skip field")?;
            b = &b[skip..];
        }
    }
    Ok(results)
}

fn parse_single_model_config(mut b: &[u8]) -> RawModel {
    let mut m = RawModel::default();
    while !b.is_empty() {
        let Some((num, typ, n)) = consume_tag(b) else { break };
        b = &b[n..];
        let consumed = match (num, typ) {
            (1, 2) | (22, 2) => consume_bytes(b).map(|(val, len)| {
                let text = String::from_utf8_lossy(val).into_owned();
                if num == 1 { m.label = text } else { m.uid = text }
                len
            }),
            (5, 0) | (10, 0) | (18, 0) => consume_varint(b).map(|(val, len)| {
                match num {
                    5 => m.multimodal = val == 1,
                    10 => m.vendor_id = val,
                    _ => m.context_length = val as i64,
                }
                len
            }),
            _ => None,
        };
        let skip = match consumed {
            Some(len) => len,
            None => match consume_field_value(num, typ, b, MAX_GROUP_DEPTH) {
                Some(len) => len,
                None => break,
            },
        };
        b = &b[skip..];
    }
    m
}

fn vendor_name(id: u64, uid: &str) -> &'static str {
    match id {
        1 => "cognition",
        2 => "openai",
        3 => "anthropic",
        4 => "google",
        6 => "deepseek",
        7 => "moonshot",
        9 => "zhipu",
        11 => "nvidia",
        _ if uid.to_lowercase().contains("grok") => "xai",
        _ => "devin",
    }
}

fn modalities(multimodal: bool) -> Vec<String> {
    let mut m = vec!["text".to_string()];
    if multimodal {
        m.push("image".into());
    }
    m
}

/// `--raw`: one entry per upstream configuration, no aggregation.
pub fn format_raw_models(raw: &[RawModel]) -> Vec<ModelJson> {
    raw.iter()
        .map(|r| ModelJson {
            id: r.uid.clone(),
            object: "model".into(),
            kind: "devin".into(),
            owned_by: vendor_name(r.vendor_id, &r.uid).into(),
            display_name: r.label.clone(),
            context_length: r.context_length,
            max_completion_tokens: 64000,
            supported_input_modalities: modalities(r.multimodal),
            supported_output_modalities: vec!["text".into()],
            thinking: None,
        })
        .collect()
}

/// (suffix, effort, text re-added to the base id)
const COMPOUND_SUFFIXES: &[(&str, &str, &str)] = &[
    ("-low-fast", "low", ""),
    ("-medium-fast", "medium", ""),
    ("-high-fast", "high", ""),
    ("-xhigh-fast", "xhigh", ""),
    ("-max-fast", "max", ""),
    ("-none-fast", "none", ""),
    ("-low-priority", "low", ""),
    ("-medium-priority", "medium", ""),
    ("-high-priority", "high", ""),
    ("-xhigh-priority", "xhigh", ""),
    ("-max-priority", "max", ""),
    ("-none-priority", "none", ""),
    ("-thinking-1m", "", "-1m"),
    ("-thinking", "", ""),
    ("-max-1m", "max", "-1m"),
    ("-none-1m", "none", "-1m"),
];

const SIMPLE_EFFORT_SUFFIXES: &[(&str, &str)] = &[
    ("-none", "none"),
    ("-minimal", "minimal"),
    ("-low", "low"),
    ("-medium", "medium"),
    ("-high", "high"),
    ("-xhigh", "xhigh"),
    ("-max", "max"),
];

const UPPER_SUFFIXES: &[(&str, &str)] = &[
    ("_NONE", "none"),
    ("_MINIMAL", "minimal"),
    ("_LOW", "low"),
    ("_MEDIUM", "medium"),
    ("_HIGH", "high"),
    ("_XHIGH", "xhigh"),
    ("_MAX", "max"),
    ("_THINKING", "high"),
];

/// Splits a model uid into its base id and reasoning effort (`""` when it has none).
pub fn split_devin_uid(uid: &str) -> (String, String) {
    if uid == "swe-1-6-slow" {
        return (uid.to_string(), String::new());
    }
    if uid == "swe-1-6-fast" {
        return ("swe-1-6".to_string(), String::new());
    }

    let upper = uid.to_uppercase();
    for (suffix, effort) in UPPER_SUFFIXES {
        if upper.ends_with(suffix) {
            // Suffixes are ASCII, so the byte offset is a char boundary of the original too.
            let base = uid.get(..uid.len().saturating_sub(suffix.len())).unwrap_or("");
            return (base.to_string(), effort.to_string());
        }
    }
    for (suffix, effort, readd) in COMPOUND_SUFFIXES {
        if let Some(base) = uid.strip_suffix(suffix) {
            return (format!("{base}{readd}"), effort.to_string());
        }
    }
    for (suffix, effort) in SIMPLE_EFFORT_SUFFIXES {
        if let Some(base) = uid.strip_suffix(suffix) {
            return (base.to_string(), effort.to_string());
        }
    }
    (uid.to_string(), String::new())
}

struct AggEntry {
    base_id: String,
    display_name: String,
    vendor_id: u64,
    context_length: i64,
    multimodal: bool,
    levels: BTreeSet<String>,
}

/// Default view: effort variants of one model merge into a single entry with thinking levels
/// (seeded from the built-in Devin catalog), in first-seen order.
pub fn aggregate_models(raw: &[RawModel]) -> Vec<ModelJson> {
    let mut grouped: HashMap<String, AggEntry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for r in raw {
        let (mut base, level) = split_devin_uid(&r.uid);
        if base.is_empty() {
            base = r.uid.clone();
        }
        let is_base = base == r.uid;

        let entry = grouped.entry(base.clone()).or_insert_with(|| {
            let mut levels = BTreeSet::new();
            if let Some(info) = cpa_core::registry::lookup_devin_model(&base)
                && let Some(thinking) = &info.thinking
            {
                for l in &thinking.levels {
                    if !l.is_empty() && l != "priority" {
                        levels.insert(l.clone());
                    }
                }
            }
            order.push(base.clone());
            AggEntry {
                base_id: base.clone(),
                display_name: clean_display_name(&r.label),
                vendor_id: r.vendor_id,
                context_length: r.context_length,
                multimodal: r.multimodal,
                levels,
            }
        });

        if is_base {
            entry.display_name = clean_display_name(&r.label);
            if r.vendor_id != 0 {
                entry.vendor_id = r.vendor_id;
            }
        }
        if r.multimodal {
            entry.multimodal = true;
        }
        if r.context_length > entry.context_length {
            entry.context_length = r.context_length;
        }
        if !level.is_empty() && level != "priority" {
            entry.levels.insert(level);
        }
    }

    order
        .iter()
        .filter_map(|id| grouped.remove(id))
        .map(|entry| {
            let thinking = (!entry.levels.is_empty()).then(|| {
                let mut levels: Vec<String> = entry.levels.into_iter().collect();
                sort_levels(&mut levels);
                ThinkingJson { levels }
            });
            ModelJson {
                owned_by: vendor_name(entry.vendor_id, &entry.base_id).into(),
                id: entry.base_id,
                object: "model".into(),
                kind: "devin".into(),
                display_name: entry.display_name,
                context_length: entry.context_length,
                max_completion_tokens: 64000,
                supported_input_modalities: modalities(entry.multimodal),
                supported_output_modalities: vec!["text".into()],
                thinking,
            }
        })
        .collect()
}

const DISPLAY_NAME_SUFFIXES: &[&str] = &[
    " Low Fast", " Medium Fast", " High Fast", " XHigh Fast", " Max Fast",
    " Low Thinking Fast", " Medium Thinking Fast", " High Thinking Fast",
    " XHigh Thinking Fast", " Max Thinking Fast", " No Thinking Fast",
    " Low Thinking", " Medium Thinking", " High Thinking", " XHigh Thinking",
    " Max Thinking", " No Thinking",
    " Low", " Medium", " High", " XHigh", " Max", " None", " Minimal",
    " Thinking", " Fast",
];

/// Strips effort and speed words from the end of a label, repeatedly.
fn clean_display_name(label: &str) -> String {
    let mut trimmed = label.trim().to_string();
    loop {
        let lower = trimmed.to_lowercase();
        let Some(suffix) = DISPLAY_NAME_SUFFIXES.iter().find(|s| lower.ends_with(&s.to_lowercase())) else {
            break;
        };
        // The suffixes are ASCII, so cutting their byte length keeps a char boundary unless the
        // label has multi-byte text inside the matched tail (Go would slice bytes the same way).
        let cut = trimmed.len().saturating_sub(suffix.len());
        trimmed = trimmed.get(..cut).unwrap_or("").trim().to_string();
    }
    trimmed
}

fn sort_levels(levels: &mut [String]) {
    let rank = |l: &str| match l {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        "fast" => 7,
        "priority" => 8,
        _ => 99,
    };
    levels.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_devin_uid_cases() {
        for (uid, base, effort) in [
            ("claude-opus-5-low-fast", "claude-opus-5", "low"),
            ("claude-opus-5-max-fast", "claude-opus-5", "max"),
            ("gpt-6-astra-low-priority", "gpt-6-astra", "low"),
            ("gpt-6-astra-high", "gpt-6-astra", "high"),
            ("MODEL_GPT_5_2_LOW", "MODEL_GPT_5_2", "low"),
            ("MODEL_GPT_5_2_HIGH", "MODEL_GPT_5_2", "high"),
            ("MODEL_GOOGLE_GEMINI_3_0_FLASH_MINIMAL", "MODEL_GOOGLE_GEMINI_3_0_FLASH", "minimal"),
            ("claude-opus-4-6-thinking", "claude-opus-4-6", ""),
            ("claude-opus-4-6-thinking-1m", "claude-opus-4-6-1m", ""),
            ("glm-5-2-max-1m", "glm-5-2-1m", "max"),
            ("swe-1-6-slow", "swe-1-6-slow", ""),
            ("swe-1-6-fast", "swe-1-6", ""),
            ("swe-2", "swe-2", ""),
        ] {
            assert_eq!(split_devin_uid(uid), (base.to_string(), effort.to_string()), "{uid}");
        }
    }

    fn raw(uid: &str, label: &str, vendor: u64) -> RawModel {
        RawModel { uid: uid.into(), label: label.into(), context_length: 1_000_000, multimodal: true, vendor_id: vendor }
    }

    #[test]
    fn aggregate_merges_thinking_variants() {
        let aggregated = aggregate_models(&[
            raw("test-opus-model", "Test Opus Model", 3),
            raw("test-opus-model-low-fast", "Test Opus Model Low Fast", 3),
            raw("test-opus-model-high-fast", "Test Opus Model High Fast", 3),
            raw("test-astra-model-low-priority", "Test Astra Model Low Thinking Fast", 2),
            raw("test-astra-model-high-priority", "Test Astra Model High Thinking Fast", 2),
        ]);
        assert_eq!(aggregated.len(), 2);
        let opus = &aggregated[0];
        assert_eq!((opus.id.as_str(), opus.display_name.as_str(), opus.owned_by.as_str()), ("test-opus-model", "Test Opus Model", "anthropic"));
        assert_eq!(opus.thinking.as_ref().map(|t| t.levels.clone()), Some(vec!["low".to_string(), "high".to_string()]));
        let astra = &aggregated[1];
        assert_eq!((astra.id.as_str(), astra.display_name.as_str(), astra.owned_by.as_str()), ("test-astra-model", "Test Astra Model", "openai"));
        assert_eq!(astra.thinking.as_ref().map(|t| t.levels.clone()), Some(vec!["low".to_string(), "high".to_string()]));
    }

    #[test]
    fn aggregate_preserves_catalog_thinking_levels() {
        let aggregated = aggregate_models(&[raw("claude-fable-5-1", "Claude Fable 5.1", 3)]);
        assert_eq!(aggregated.len(), 1);
        assert!(aggregated[0].thinking.as_ref().is_some_and(|t| !t.levels.is_empty()));
    }

    #[test]
    fn protobuf_response_round_trip() {
        fn field_bytes(num: u32, val: &[u8]) -> Vec<u8> {
            let mut b = Vec::new();
            append_varint(&mut b, u64::from(num) << 3 | 2);
            append_varint(&mut b, val.len() as u64);
            b.extend_from_slice(val);
            b
        }
        fn field_varint(num: u32, val: u64) -> Vec<u8> {
            let mut b = Vec::new();
            append_varint(&mut b, u64::from(num) << 3);
            append_varint(&mut b, val);
            b
        }
        let mut model = field_bytes(1, b"Label");
        model.extend(field_varint(5, 1));
        model.extend(field_varint(10, 3));
        model.extend(field_varint(18, 200_000));
        model.extend(field_bytes(22, b"uid-1"));
        model.extend(field_varint(99, 7)); // unknown fields are skipped
        let mut body = field_bytes(1, &model);
        body.extend(field_bytes(1, &field_bytes(1, b"no uid"))); // dropped: empty uid
        body.extend(field_varint(2, 9));

        let parsed = parse_raw_models_proto(&body).unwrap();
        assert_eq!(
            parsed,
            vec![RawModel { uid: "uid-1".into(), label: "Label".into(), context_length: 200_000, multimodal: true, vendor_id: 3 }]
        );
        assert!(parse_raw_models_proto(&[0x0a, 0x05, 1]).is_err());
        assert!(parse_raw_models_proto(&[0x00]).is_err()); // field number 0
    }

    #[test]
    fn request_body_wraps_client_metadata_in_field_one() {
        let body = build_request_body("key");
        let metadata = cpa_executors::devin::wire::build_client_metadata_bytes("key", "", "");
        // Field 1, length-delimited; the device fingerprint (the tail) is random without a seed,
        // so compare everything before it.
        assert_eq!(body[0], 0x0a);
        let (len, n) = consume_varint(&body[1..]).unwrap();
        assert_eq!(len as usize, metadata.len());
        assert_eq!(body.len(), 1 + n + metadata.len());
        let fixed = 60;
        assert_eq!(&body[1 + n..1 + n + fixed], &metadata[..fixed]);
    }

    #[test]
    fn display_names_lose_effort_words() {
        assert_eq!(clean_display_name("Test Astra Model Low Thinking Fast"), "Test Astra Model");
        assert_eq!(clean_display_name("  Claude Opus 4.6 Thinking "), "Claude Opus 4.6");
        assert_eq!(clean_display_name("GPT 5 xhigh"), "GPT 5");
    }

    #[test]
    fn raw_output_keeps_every_variant_and_omits_empty_fields() {
        let out = format_raw_models(&[RawModel { uid: "grok-3".into(), label: "Grok".into(), ..Default::default() }]);
        assert_eq!(out[0].owned_by, "xai");
        let json = String::from_utf8(go_marshal(&out[0], false).unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"id":"grok-3","object":"model","type":"devin","owned_by":"xai","display_name":"Grok","max_completion_tokens":64000,"supportedInputModalities":["text"],"supportedOutputModalities":["text"]}"#
        );
    }
}
