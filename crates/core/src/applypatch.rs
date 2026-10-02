//! The Codex custom `apply_patch` tool contract (Go: internal/client/codex/apply-patch).
//!
//! Codex declares `apply_patch` as a FREEFORM custom tool; upstreams without custom tools get a
//! function tool whose single string argument `input` carries the patch text. These helpers
//! recognize the custom declaration, build the function schema/description and wrap/unwrap the
//! JSON arguments. Encoders match Go's `json.Marshal` (HTML escaping included).

use cpa_json::{J, Value};

use crate::util::go_json_string;

const PARAMETERS_JSON: &str = r#"{"type":"object","properties":{"input":{"type":"string","description":"The complete apply_patch patch text."}},"required":["input"],"additionalProperties":false}"#;

const PATCH_INSTRUCTIONS: &str = "Call this function with a JSON object whose input field contains the complete patch text.
Use the Codex apply_patch format, not a conventional git unified diff.
Start with *** Begin Patch and end with *** End Patch.
Use *** Add File: path, *** Delete File: path, or *** Update File: path.
Every added-file content line starts with +.
For updates, use @@; context lines start with one space, removed lines with -, and added lines with +.
Use *** Move to: path for a rename and *** End of File when required by the patch grammar.
Example input:
*** Begin Patch
*** Update File: src/main.go
@@
-old
+new
*** End Patch";

/// Whether the declaration is the custom `apply_patch` tool (exact `custom` type, trimmed name).
pub fn is_custom_tool(tool: &Value) -> bool {
    tool.g("type").str() == "custom" && tool.g("name").str().trim() == "apply_patch"
}

/// The patch input schema as JSON bytes (a fresh copy per call).
pub fn parameters() -> Vec<u8> {
    PARAMETERS_JSON.as_bytes().to_vec()
}

/// Function description: the original description (minus the "do not wrap in JSON" line), the JSON
/// wrapper instructions, and the original patch grammar when the declaration carries one.
pub fn description(tool: &Value) -> String {
    let original = tool.g("description").str().replace(
        "This is a FREEFORM tool, so do not wrap the patch in JSON.",
        "",
    );
    let mut description = String::new();
    if !original.trim().is_empty() {
        description.push_str(&original);
        description.push_str("\n\n");
    }
    description.push_str(PATCH_INSTRUCTIONS);
    let grammar = tool.g("format.definition").str();
    if !grammar.is_empty() {
        if grammar.contains("*** Environment ID:") {
            description.push_str("\n\nUse *** Environment ID: as specified by the patch grammar.");
        }
        description.push_str("\n\nOriginal patch grammar:\n");
        description.push_str(&grammar);
    }
    description
}

/// Encodes the complete patch text as function arguments: `{"input":"..."}`.
pub fn wrap_input(input: &str) -> String {
    format!("{{\"input\":{}}}", go_json_string(input))
}

/// Decodes function arguments that must be exactly one JSON object with one string field `input`
/// and nothing after it.
pub fn unwrap_input(arguments: &str) -> Result<String, String> {
    let mut cursor = Cursor {
        text: arguments,
        pos: 0,
    };

    match cursor.next_non_ws() {
        None => return Err("decode apply_patch arguments object: EOF".into()),
        Some('{') => cursor.pos += 1,
        Some(_) => return Err("apply_patch arguments must be a JSON object".into()),
    }

    match cursor.next_non_ws() {
        Some('"') => {}
        Some('}') => return Err("apply_patch arguments must contain the input field".into()),
        _ => return Err("decode apply_patch input key: unexpected token".into()),
    }
    let key = cursor
        .read_value()
        .map_err(|e| format!("decode apply_patch input key: {e}"))?;
    if key.as_str() != Some("input") {
        return Err("apply_patch arguments must contain the input field".into());
    }

    if cursor.next_non_ws() != Some(':') {
        return Err("decode apply_patch input value: expected ':'".into());
    }
    cursor.pos += 1;
    if cursor.next_non_ws().is_none() {
        return Err("decode apply_patch input value: EOF".into());
    }
    let input = match cursor
        .read_value()
        .map_err(|e| format!("decode apply_patch input value: {e}"))?
    {
        Value::String(s) => s,
        _ => return Err("apply_patch input must be a string".into()),
    };

    match cursor.next_non_ws() {
        Some('}') => cursor.pos += 1,
        None => return Err("decode apply_patch arguments closing brace: EOF".into()),
        Some(_) => return Err("apply_patch arguments must contain only one input field".into()),
    }

    if cursor.next_non_ws().is_some() {
        return Err("apply_patch arguments must not contain trailing JSON".into());
    }
    Ok(input)
}

/// Encodes patch text for use inside a JSON string, without the surrounding quotes.
pub fn escape_input_fragment(fragment: &str) -> String {
    let encoded = go_json_string(fragment);
    encoded[1..encoded.len() - 1].to_string()
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl Cursor<'_> {
    fn next_non_ws(&mut self) -> Option<char> {
        let rest = &self.text[self.pos..];
        let trimmed = rest.trim_start_matches([' ', '\t', '\n', '\r']);
        self.pos += rest.len() - trimmed.len();
        trimmed.chars().next()
    }

    /// Reads one JSON value at the cursor and advances past it.
    fn read_value(&mut self) -> Result<Value, serde_json::Error> {
        let mut stream =
            serde_json::Deserializer::from_str(&self.text[self.pos..]).into_iter::<Value>();
        let value = stream.next().unwrap_or_else(|| Ok(Value::Null))?;
        self.pos += stream.byte_offset();
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_round_trip() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+<hello> & \"world\"\n*** End Patch\n";
        let wrapped = wrap_input(patch);
        assert!(wrapped.contains("\\u003chello\\u003e"));
        assert_eq!(unwrap_input(&wrapped).unwrap(), patch);
    }

    #[test]
    fn unwrap_is_strict() {
        for bad in [
            "",
            "[]",
            "{}",
            r#"{"other":"x"}"#,
            r#"{"input":1}"#,
            r#"{"input":"a","input":"b"}"#,
            r#"{"input":"a","x":1}"#,
            r#"{"input":"a"} {}"#,
            r#"{"input":"a""#,
        ] {
            assert!(unwrap_input(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(unwrap_input(" {\n\"input\" : \"a\" } \n").unwrap(), "a");
    }

    #[test]
    fn custom_tool_recognition() {
        let t = |s: &str| cpa_json::parse_str(s);
        assert!(is_custom_tool(&t(
            r#"{"type":"custom","name":" \tapply_patch\n"}"#
        )));
        assert!(!is_custom_tool(&t(
            r#"{"type":" custom ","name":"apply_patch"}"#
        )));
        assert!(!is_custom_tool(&t(
            r#"{"type":"function","name":"apply_patch"}"#
        )));
        assert!(!is_custom_tool(&t(
            r#"{"type":"custom","name":"APPLY_PATCH"}"#
        )));
        assert!(!is_custom_tool(&Value::Null));
    }

    #[test]
    fn description_rules() {
        let tool = cpa_json::parse_str(
            r#"{"type":"custom","name":"apply_patch","description":"This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"definition":"start: x\n*** Environment ID: y"}}"#,
        );
        let d = description(&tool);
        assert!(!d.contains("do not wrap the patch in JSON"));
        assert!(d.starts_with("Call this function"));
        assert!(d.contains("Use *** Environment ID: as specified by the patch grammar."));
        assert!(d.ends_with("*** Environment ID: y"));
    }
}
