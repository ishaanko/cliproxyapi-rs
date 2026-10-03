//! DNS-SD TXT record construction and parsing (Go: internal/discovery/spec.go).

use std::collections::BTreeMap;

use crate::types::PRODUCT_CPA;

/// RFC 6763 section 6.3 single string limit.
pub const MAX_TXT_RECORD_BYTES: usize = 255;
/// Conservative LAN UDP packet safety boundary for all TXT strings together.
pub const MAX_TXT_BYTES: usize = 400;

/// Keeps printable ASCII only (Go: `sanitizeTXTValue`).
fn sanitize_txt_value(v: &str) -> String {
    v.chars().filter(|c| (' '..'\u{7f}').contains(c)).collect()
}

/// Input fields for generating safe DNS-SD TXT records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxtOptions {
    pub version: String,
    pub product: String,
    pub protocols: Vec<String>,
    pub features: Vec<String>,
    pub api_path_openai: String,
    pub api_path_anthropic: String,
    pub api_path_gemini: String,
    pub tls: bool,
    pub auth_required: bool,
    pub auth_methods: Vec<String>,
    pub instance_id: String,
    pub node_role: String,
    pub advertise_management: bool,
}

impl TxtOptions {
    /// Safe defaults for CPA TXT records (Go: `DefaultTXTOptions`).
    pub fn defaults() -> Self {
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self {
            version: "1".into(),
            product: PRODUCT_CPA.into(),
            protocols: strings(&["chat-completions", "responses", "messages", "generate-content", "interactions"]),
            features: strings(&["chat", "responses", "messages", "generate_content", "interactions"]),
            api_path_openai: "/v1".into(),
            api_path_anthropic: "/v1".into(),
            api_path_gemini: "/v1beta".into(),
            tls: false,
            auth_required: true,
            auth_methods: strings(&["api_key"]),
            instance_id: String::new(),
            node_role: "standalone".into(),
            advertise_management: false,
        }
    }
}

/// Builds `key=value` TXT strings within 400 bytes total and 255 bytes per string, dropping
/// entries that do not fit and keeping essential routing metadata first.
pub fn build_txt_records(mut opts: TxtOptions) -> Vec<String> {
    if opts.version.is_empty() {
        opts.version = "1".into();
    }
    if opts.product.is_empty() {
        opts.product = PRODUCT_CPA.into();
    }
    if opts.api_path_openai.is_empty() {
        opts.api_path_openai = "/v1".into();
    }
    if opts.api_path_anthropic.is_empty() {
        opts.api_path_anthropic = "/v1".into();
    }
    if opts.api_path_gemini.is_empty() {
        opts.api_path_gemini = "/v1beta".into();
    }

    // Priority order: 1 core, 2 standard, 3 optional (candidates are appended in that order).
    let mut candidates: Vec<(&str, String)> = Vec::new();
    let mut add = |key: &'static str, val: &str| {
        let val = sanitize_txt_value(val.trim());
        if !val.is_empty() {
            candidates.push((key, val));
        }
    };

    add("version", &opts.version);
    add("product", &opts.product);
    add("instance_id", &opts.instance_id);
    add("tls", if opts.tls { "1" } else { "0" });
    add("auth_required", if opts.auth_required { "true" } else { "false" });
    add("api_openai", &opts.api_path_openai);

    add("api_anthropic", &opts.api_path_anthropic);
    add("api_gemini", &opts.api_path_gemini);
    add("management", if opts.advertise_management { "true" } else { "false" });
    if !opts.auth_methods.is_empty() {
        add("auth_methods", &opts.auth_methods.join(","));
    }
    if !opts.protocols.is_empty() {
        add("protocols", &opts.protocols.join(","));
    }

    if !opts.node_role.is_empty() {
        add("node_role", &opts.node_role);
    }
    if !opts.features.is_empty() {
        add("features", &opts.features.join(","));
    }

    let mut records = Vec::new();
    let mut total_bytes = 0usize;
    for (key, val) in candidates {
        let entry = format!("{key}={val}");
        if entry.len() > MAX_TXT_RECORD_BYTES {
            continue;
        }
        let entry_cost = entry.len() + 1; // +1 for the DNS TXT length prefix byte
        if total_bytes + entry_cost > MAX_TXT_BYTES {
            continue;
        }
        records.push(entry);
        total_bytes += entry_cost;
    }
    records
}

/// Parses `key=value` strings into a map with lower-cased, trimmed keys. A later duplicate key
/// replaces an earlier one; entries without `=` map to an empty value.
pub fn parse_txt_records<S: AsRef<str>>(txt: &[S]) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for entry in txt {
        let entry = entry.as_ref();
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) => (k, v),
            None => (entry, ""),
        };
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        result.insert(key, value.to_string());
    }
    result
}
