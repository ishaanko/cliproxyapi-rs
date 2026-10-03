//! Typed accessors over `serde_json::Value` mirroring the Go helpers that read
//! `map[string]any` (getString, getFloat, getBool, getAnyString, getBoolNested).

use serde_json::Value;

/// `getString`: the string at `key`, or "" when absent or not a string.
pub fn get_string(m: &Value, key: &str) -> String {
    match m.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// `getFloat`: the number at `key`, or 0.
pub fn get_float(m: &Value, key: &str) -> f64 {
    match m.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `getBool`: the bool at `key`, or false.
pub fn get_bool(m: &Value, key: &str) -> bool {
    matches!(m.get(key), Some(Value::Bool(true)))
}

/// `getBoolNested`: walks `keys` through objects and reads the final bool.
pub fn get_bool_nested(m: &Value, keys: &[&str]) -> bool {
    let mut current = m;
    for (i, key) in keys.iter().enumerate() {
        if i == keys.len() - 1 {
            return get_bool(current, key);
        }
        match current.get(key) {
            Some(next @ Value::Object(_)) => current = next,
            _ => return false,
        }
    }
    false
}

/// `getAnyString`: `fmt.Sprintf("%v", v)` of the value, "" when absent or null.
pub fn get_any_string(m: &Value, key: &str) -> String {
    match m.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(v) => go_sprint_v(v),
    }
}

/// `%v` formatting of a decoded JSON value (floats via the shortest `%g`, maps with sorted keys).
pub fn go_sprint_v(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => go_float_v(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(go_sprint_v).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries.iter().map(|(k, v)| format!("{k}:{}", go_sprint_v(v))).collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// `%v` of a float64: shortest round-trip digits, exponent form when exp < -4 or exp >= 21.
pub fn go_float_v(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e21).contains(&abs) {
        let s = format!("{f:e}");
        if let Some((mantissa, exp)) = s.split_once('e') {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            return format!("{mantissa}e{sign}{digits:0>2}");
        }
        return s;
    }
    format!("{f}")
}

/// `fmt.Sprintf("%.0f", f)`: round half to even like Go's correctly rounded formatting.
pub fn fmt_f0(f: f64) -> String {
    format!("{f:.0}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn any_string_matches_go_percent_v() {
        let v = json!({"a": 5, "b": 1.5, "c": true, "d": null, "e": [1, "x"], "f": {"z": 1, "y": 2}, "g": 1e21});
        assert_eq!(get_any_string(&v, "a"), "5");
        assert_eq!(get_any_string(&v, "b"), "1.5");
        assert_eq!(get_any_string(&v, "c"), "true");
        assert_eq!(get_any_string(&v, "d"), "");
        assert_eq!(get_any_string(&v, "e"), "[1 x]");
        assert_eq!(get_any_string(&v, "f"), "map[y:2 z:1]");
        assert_eq!(get_any_string(&v, "g"), "1e+21");
        assert_eq!(get_any_string(&v, "missing"), "");
    }

    #[test]
    fn nested_bool_walks_objects() {
        let v = json!({"quota-exceeded": {"switch-project": true}});
        assert!(get_bool_nested(&v, &["quota-exceeded", "switch-project"]));
        assert!(!get_bool_nested(&v, &["quota-exceeded", "nope"]));
        assert!(!get_bool_nested(&v, &["routing", "x"]));
    }
}
