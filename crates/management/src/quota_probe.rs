//! Declarative quota probes (Go: `plugin_quota.go` `executeQuotaProbe`, `mapProbeResponse`):
//! a credential whose metadata holds a `quota_probe` object is queried over HTTP and the answer is
//! normalized into a [`QuotaFetchResponse`].

use cpa_auth::Auth;
use cpa_json::{J, Kind, Res};
use cpa_pluginapi::api::{QuotaBucket, QuotaFetchResponse, QuotaGroup, QuotaMetric, QuotaSubscription};
use serde_json::{Map, Value};

use crate::state::ManagementState;

/// ISO 4217 codes `golang.org/x/text/currency.ParseISO` recognizes for the `currency` format.
const ISO_CURRENCIES: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT", "BGN", "BHD", "BIF", "BMD", "BND",
    "BOB", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD", "CDF", "CHF", "CLP", "CNY", "COP", "CRC", "CUC", "CUP", "CVE", "CZK",
    "DJF", "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL", "GHS", "GIP", "GMD", "GNF", "GTQ", "GYD",
    "HKD", "HNL", "HRK", "HTG", "HUF", "IDR", "ILS", "INR", "IQD", "IRR", "ISK", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF",
    "KPW", "KRW", "KWD", "KYD", "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MMK", "MNT", "MOP",
    "MRU", "MUR", "MVR", "MWK", "MXN", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PGK", "PHP",
    "PKR", "PLN", "PYG", "QAR", "RON", "RSD", "RUB", "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SLL", "SOS",
    "SRD", "SSP", "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD", "TWD", "TZS", "UAH", "UGX", "USD",
    "UYU", "UZS", "VES", "VND", "VUV", "WST", "XAF", "XCD", "XOF", "XPF", "YER", "ZAR", "ZMW", "ZWL",
];

/// The probe outcome: `None` when the probe does not apply (no `url`), `Some(Err)` for failures
/// reported as 502.
pub(crate) async fn execute(st: &ManagementState, auth: &Auth, probe: &Map<String, Value>) -> Option<Result<QuotaFetchResponse, String>> {
    let mut url = text(probe, "url").trim().to_string();
    if url.is_empty() {
        return None;
    }
    let mut method = text(probe, "method").trim().to_uppercase();
    if method.is_empty() {
        method = "GET".into();
    }
    let mut needs_token = url.contains("$TOKEN$");
    let mut raw_data = text(probe, "data");
    if raw_data.contains("$TOKEN$") {
        needs_token = true;
    }
    let headers = probe
        .get("header")
        .and_then(Value::as_object)
        .or_else(|| probe.get("headers").and_then(Value::as_object))
        .cloned()
        .unwrap_or_default();
    if headers.values().any(|v| v.as_str().is_some_and(|s| s.contains("$TOKEN$"))) {
        needs_token = true;
    }

    let mut token = String::new();
    if needs_token {
        token = match crate::tools::resolve_token(st, auth, "").await {
            Ok(t) => t,
            Err(e) => return Some(Err(format!("probe authentication failed: {e}"))),
        };
        if token.is_empty() {
            return Some(Err("probe authentication token not found for credential".into()));
        }
        url = url.replace("$TOKEN$", &token);
        raw_data = raw_data.replace("$TOKEN$", &token);
    }

    let Ok(http_method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        return Some(Err(format!("build probe request: net/http: invalid method {method:?}")));
    };
    let proxy = crate::tools::select_proxy(&st.cfg(), Some(auth), "");
    let client = crate::tools::build_client(&proxy);
    let mut req = client.request(http_method, &url);
    if !raw_data.is_empty() {
        req = req.body(raw_data);
    }
    for (k, v) in &headers {
        if let Some(s) = v.as_str() {
            let s = if needs_token { s.replace("$TOKEN$", &token) } else { s.to_string() };
            req = req.header(k.as_str(), s);
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Some(Err(format!("probe request failed: {}", e.without_url()))),
    };
    let status = resp.status();
    let date = resp.headers().get(reqwest::header::DATE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Some(Err(format!("read probe response: {}", e.without_url()))),
    };
    if !status.is_success() {
        return Some(Err(format!("probe returned status {}: {}", status.as_u16(), String::from_utf8_lossy(&bytes))));
    }
    if !cpa_json::valid(&bytes) {
        return Some(Err("upstream probe response is not valid JSON".into()));
    }
    let server_offset_ms = date
        .and_then(|d| httpdate::parse_http_date(&d).ok())
        .map(|t| match t.duration_since(std::time::SystemTime::now()) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        })
        .unwrap_or(0);

    let doc = cpa_json::parse(&bytes);
    Some(probe_result(&doc, probe.get("mapping").and_then(Value::as_object), server_offset_ms))
}

/// The normalized quota of a probe answer: through the declared `mapping`, else from the
/// answer's own normalized shape. `server_offset_ms` fills `serverTimeOffsetMs` when the answer
/// does not carry one.
fn probe_result(doc: &Value, mapping: Option<&Map<String, Value>>, server_offset_ms: i64) -> Result<QuotaFetchResponse, String> {
    if let Some(mapping) = mapping {
        return match map_probe_response(doc, mapping) {
            Ok(mut mapped) => {
                if mapped.server_time_offset_ms == 0 {
                    mapped.server_time_offset_ms = server_offset_ms;
                }
                Ok(mapped)
            }
            Err(e) => Err(format!("probe response mapping failed: {e}")),
        };
    }
    if let Value::Object(raw) = doc {
        // Optional plugin data must not invalidate core quota fields.
        let mut core = raw.clone();
        core.retain(|k, _| !k.eq_ignore_ascii_case("summary"));
        if let Ok(mut quota) = serde_json::from_value::<QuotaFetchResponse>(Value::Object(core)) {
            let has_plan = quota.subscription.as_ref().is_some_and(|s| !s.plan.trim().is_empty());
            let mut filtered: Vec<QuotaGroup> = Vec::new();
            let groups = doc.g("groups");
            if groups.is_array() {
                for (g_idx, grp) in groups.array().into_iter().enumerate() {
                    let Some(orig) = quota.groups.get(g_idx) else { break };
                    let buckets = grp.g("buckets");
                    if !buckets.is_array() {
                        continue;
                    }
                    let mut valid: Vec<QuotaBucket> = Vec::new();
                    for (b_idx, bkt) in buckets.array().into_iter().enumerate() {
                        let Some(orig_bucket) = orig.buckets.get(b_idx) else { break };
                        let mut rem = bkt.g("remainingFraction");
                        if !rem.exists() {
                            rem = bkt.g("remaining_fraction");
                        }
                        if let Some(frac) = parse_numeric_fraction(&rem) {
                            let mut bucket = orig_bucket.clone();
                            bucket.remaining_fraction = frac;
                            valid.push(bucket);
                        }
                    }
                    if !valid.is_empty() {
                        let mut group = orig.clone();
                        group.buckets = valid;
                        filtered.push(group);
                    }
                }
            }
            quota.groups = filtered;
            quota.summary = filter_usable_quota_summary(doc);
            if has_plan || !quota.groups.is_empty() || !quota.summary.is_empty() {
                if quota.server_time_offset_ms == 0 {
                    quota.server_time_offset_ms = server_offset_ms;
                }
                return Ok(quota);
            }
        }
    }
    Err("upstream probe response does not match normalized quota shape or declared mapping".into())
}

fn text(m: &Map<String, Value>, key: &str) -> String {
    m.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `filterUsableQuotaSummary`.
fn filter_usable_quota_summary(doc: &Value) -> Vec<QuotaMetric> {
    let Value::Object(raw) = doc else { return Vec::new() };
    let summary = raw
        .get("summary")
        .or_else(|| raw.iter().find(|(k, _)| k.eq_ignore_ascii_case("summary")).map(|(_, v)| v));
    let Some(Value::Array(items)) = summary else { return Vec::new() };
    let mut usable = Vec::new();
    for raw_metric in items {
        let metric_res = Res::of(raw_metric);
        let key_r = metric_res.g("key").into_value();
        let label_r = metric_res.g("label").into_value();
        let value = metric_res.g("value");
        let (Some(Value::String(key)), Some(Value::String(label))) = (key_r, label_r) else { continue };
        let (key, label) = (key.trim().to_string(), label.trim().to_string());
        let number = value.float();
        if key.is_empty() || label.is_empty() || value.kind() != Kind::Number || !number.is_finite() {
            continue;
        }
        let mut metric = QuotaMetric { key, label, value: number, ..Default::default() };
        if let Some(Value::String(unit)) = metric_res.g("unit").into_value() {
            metric.unit = unit.trim().to_string();
        }
        if let Some(Value::String(format)) = metric_res.g("format").into_value() {
            match format.trim() {
                "number" => metric.format = "number".into(),
                "currency" => {
                    if let Some(Value::String(code)) = metric_res.g("currency").into_value() {
                        let code = code.trim().to_uppercase();
                        if ISO_CURRENCIES.contains(&code.as_str()) {
                            metric.format = "currency".into();
                            metric.currency = code;
                        }
                    }
                }
                _ => {}
            }
        }
        usable.push(metric);
    }
    usable
}

/// `parseNumericFraction`.
fn parse_numeric_fraction(res: &Res<'_>) -> Option<f64> {
    if !res.exists() {
        return None;
    }
    match res.kind() {
        Kind::Number => Some(res.float()).filter(|v| v.is_finite()),
        Kind::String => {
            let s = res.str();
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            s.parse::<f64>().ok().filter(|v| v.is_finite())
        }
        _ => None,
    }
}

/// Non-empty (after trim) string at `path`, like `res.Exists() && strings.TrimSpace(res.String()) != ""`.
fn lookup_text(doc: &Value, path: &str) -> Option<String> {
    let res = doc.g(path);
    (res.exists() && !res.str().trim().is_empty()).then(|| res.str())
}

fn string_of(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `mapProbeResponse`.
fn map_probe_response(doc: &Value, mapping: &Map<String, Value>) -> Result<QuotaFetchResponse, String> {
    let mut out = QuotaFetchResponse::default();
    fn sub(out: &mut QuotaFetchResponse) -> &mut QuotaSubscription {
        out.subscription.get_or_insert_with(Default::default)
    }
    if let Some(path) = string_of(mapping, "plan").filter(|p| !p.is_empty())
        && let Some(v) = lookup_text(doc, &path)
    {
        sub(&mut out).plan = v;
    }
    let tier_path = string_of(mapping, "tier_name").filter(|p| !p.is_empty()).or_else(|| string_of(mapping, "tierName").filter(|p| !p.is_empty()));
    if let Some(path) = tier_path
        && let Some(v) = lookup_text(doc, &path)
    {
        sub(&mut out).tier_name = v;
    }
    let tier_id_path = string_of(mapping, "tier_id").filter(|p| !p.is_empty()).or_else(|| string_of(mapping, "tierId").filter(|p| !p.is_empty()));
    if let Some(path) = tier_id_path
        && let Some(v) = lookup_text(doc, &path)
    {
        sub(&mut out).tier_id = v;
    }

    if let Some(Value::Array(groups)) = mapping.get("groups") {
        for rg in groups {
            let Value::Object(gm) = rg else { continue };
            let mut group = QuotaGroup::default();
            let name_path = string_of(gm, "display_name").or_else(|| string_of(gm, "displayName"));
            if let Some(path) = name_path {
                group.display_name = lookup_text(doc, &path).unwrap_or(path);
            }
            if let Some(buckets_path) = string_of(gm, "buckets_path").filter(|p| !p.is_empty()) {
                let array = doc.g(&buckets_path);
                if array.is_array() && !array.array().is_empty() {
                    let key = |name: &str, default: &str| {
                        let v = string_of(gm, name).unwrap_or_default();
                        if v.is_empty() { default.to_string() } else { v }
                    };
                    let win_key = key("window_key", "window");
                    let rem_frac_key = key("remaining_fraction_key", "remaining_fraction");
                    let rem_amt_key = string_of(gm, "remaining_amount_key").unwrap_or_default();
                    let tot_amt_key = string_of(gm, "total_amount_key").unwrap_or_default();
                    let reset_key = key("reset_time_key", "reset_time");
                    let desc_key = key("description_key", "description");
                    for item in array.array() {
                        let mut frac = None;
                        if !rem_frac_key.is_empty() {
                            frac = parse_numeric_fraction(&item.g(&rem_frac_key));
                        }
                        if frac.is_none() && !rem_amt_key.is_empty() && !tot_amt_key.is_empty() {
                            let rem = parse_numeric_fraction(&item.g(&rem_amt_key));
                            let tot = parse_numeric_fraction(&item.g(&tot_amt_key));
                            if let (Some(rem), Some(tot)) = (rem, tot)
                                && tot > 0.0
                            {
                                frac = Some(rem / tot);
                            }
                        }
                        let Some(frac) = frac else { continue };
                        group.buckets.push(QuotaBucket {
                            window: item.g(&win_key).str(),
                            remaining_fraction: frac,
                            reset_time: item.g(&reset_key).str(),
                            description: item.g(&desc_key).str(),
                        });
                    }
                }
            }
            if let Some(Value::Array(raw_buckets)) = gm.get("buckets") {
                for rb in raw_buckets {
                    let Value::Object(bm) = rb else { continue };
                    let mut frac = None;
                    if let Some(rf) = string_of(bm, "remaining_fraction").filter(|s| !s.is_empty()) {
                        frac = parse_numeric_fraction(&doc.g(&rf));
                    }
                    if frac.is_none()
                        && let Some(rem) = string_of(bm, "remaining_amount").filter(|s| !s.is_empty())
                        && let Some(tot) = string_of(bm, "total_amount").filter(|s| !s.is_empty())
                    {
                        let rem = parse_numeric_fraction(&doc.g(&rem));
                        let tot = parse_numeric_fraction(&doc.g(&tot));
                        if let (Some(rem), Some(tot)) = (rem, tot)
                            && tot > 0.0
                        {
                            frac = Some(rem / tot);
                        }
                    }
                    let Some(frac) = frac else { continue };
                    let mut bucket = QuotaBucket { remaining_fraction: frac, ..Default::default() };
                    let resolve = |literal: String| {
                        let res = doc.g(&literal);
                        if res.exists() { res.str() } else { literal }
                    };
                    if let Some(w) = string_of(bm, "window") {
                        bucket.window = resolve(w);
                    }
                    if let Some(d) = string_of(bm, "description") {
                        bucket.description = resolve(d);
                    }
                    if let Some(r) = string_of(bm, "reset_time") {
                        bucket.reset_time = resolve(r);
                    }
                    group.buckets.push(bucket);
                }
            }
            if !group.buckets.is_empty() {
                out.groups.push(group);
            }
        }
    }

    let total_buckets: usize = out.groups.iter().map(|g| g.buckets.len()).sum();
    let has_plan = out.subscription.as_ref().is_some_and(|s| !s.plan.trim().is_empty());
    out.summary = filter_usable_quota_summary(doc);
    if total_buckets == 0 && !has_plan && out.summary.is_empty() {
        return Err("response mapping did not match any valid quota fields in upstream response".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(doc: Value, mapping: Option<Value>) -> Result<QuotaFetchResponse, String> {
        probe_result(&doc, mapping.as_ref().and_then(Value::as_object), 0)
    }

    #[test]
    fn normalized_answers_pass_through() {
        let got = run(
            json!({"subscription": {"plan": "ProbePro"}, "groups": [{"displayName": "API Limits", "buckets": [{"window": "monthly", "remainingFraction": 0.65, "resetTime": "2026-10-01T00:00:00Z"}]}]}),
            None,
        )
        .unwrap();
        assert_eq!(got.subscription.unwrap().plan, "ProbePro");
        assert_eq!(got.groups.len(), 1);
        assert_eq!(got.groups[0].buckets[0].remaining_fraction, 0.65);
    }

    #[test]
    fn summary_only_answers_are_accepted() {
        let got = run(json!({"summary": [{"key": "balance", "label": "Balance", "value": 42, "unit": "credits"}]}), None).unwrap();
        assert_eq!((got.summary[0].key.as_str(), got.summary[0].value, got.summary[0].unit.as_str()), ("balance", 42.0, "credits"));
        let mapped = run(json!({"summary": [{"key": "balance", "label": "Balance", "value": 42}]}), Some(json!({"plan": "missing.plan"}))).unwrap();
        assert_eq!(mapped.summary.len(), 1);
    }

    #[test]
    fn a_malformed_optional_summary_is_ignored_and_the_key_matches_any_case() {
        let got = run(json!({"subscription": {"plan": "ProbePro"}, "Summary": "usage text"}), None).unwrap();
        assert!(got.summary.is_empty());
        assert_eq!(got.subscription.unwrap().plan, "ProbePro");
    }

    #[test]
    fn summary_entries_need_string_identifiers_and_a_numeric_value() {
        let got = filter_usable_quota_summary(&json!({"summary": [{"key": 123, "label": true, "value": 1}, {"key": "balance", "label": "Balance", "value": 0}, {"key": "k", "label": "L"}]}));
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].key.as_str(), got[0].value), ("balance", 0.0));
    }

    #[test]
    fn currency_formats_need_a_valid_code_and_optional_metadata_must_be_strings() {
        let got = filter_usable_quota_summary(&json!({"summary": [
            {"key": "invalid", "label": "Invalid", "value": 1, "format": "currency", "currency": "US"},
            {"key": "valid", "label": "Valid", "value": 2, "format": "currency", "currency": "usd"},
            {"key": "odd", "label": "Odd", "value": 42, "unit": 123, "format": true, "currency": ["USD"]},
        ]}));
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].format.as_str(), got[0].currency.as_str()), ("", ""));
        assert_eq!((got[1].format.as_str(), got[1].currency.as_str()), ("currency", "USD"));
        assert_eq!((got[2].unit.as_str(), got[2].format.as_str(), got[2].currency.as_str()), ("", "", ""));
    }

    #[test]
    fn a_summary_without_a_value_fails_the_probe() {
        assert!(run(json!({"summary": [{"key": "balance", "label": "Balance"}]}), None).is_err());
    }

    #[test]
    fn malformed_normalized_groups_fail_the_probe() {
        assert!(run(json!({"groups": [{}]}), None).is_err());
        assert!(run(json!({"groups": [{"buckets": [{}]}]}), None).is_err());
    }

    #[test]
    fn exhausted_quota_is_a_valid_answer() {
        let got = run(json!({"groups": [{"displayName": "Daily", "buckets": [{"window": "daily", "remainingFraction": 0}]}]}), None).unwrap();
        assert_eq!(got.groups[0].buckets[0].remaining_fraction, 0.0);
    }

    #[test]
    fn mapping_derives_fractions_from_amounts() {
        let doc = json!({
            "user": {"tier": "Enterprise"},
            "Summary": [{"key": "credits_used", "label": "Credits used", "value": 40}],
            "packages": [{"period": "monthly", "used": 40, "total": 200, "remain": 160, "expires": "2026-10-15T00:00:00Z"}],
        });
        let mapping = json!({
            "plan": "user.tier",
            "groups": [{"display_name": "Resource Packages", "buckets_path": "packages", "window_key": "period", "remaining_amount_key": "remain", "total_amount_key": "total", "reset_time_key": "expires"}],
        });
        let got = probe_result(&doc, mapping.as_object(), 7).unwrap();
        assert_eq!(got.subscription.unwrap().plan, "Enterprise");
        assert_eq!(got.summary[0].key, "credits_used");
        let bucket = &got.groups[0].buckets[0];
        assert_eq!((bucket.window.as_str(), bucket.reset_time.as_str(), bucket.remaining_fraction), ("monthly", "2026-10-15T00:00:00Z", 0.8));
        assert_eq!(got.server_time_offset_ms, 7);
    }

    #[test]
    fn mapping_rejects_non_numeric_fractions_and_empty_results() {
        let mapping = json!({"groups": [{"display_name": "API Limits", "buckets": [{"remaining_fraction": "quota.remaining"}]}]});
        assert!(run(json!({"quota": {"remaining": "unknown", "total": "unlimited"}}), Some(mapping)).is_err());
        assert!(run(json!({"x": 1}), Some(json!({"plan": "missing"}))).is_err());
    }
}
