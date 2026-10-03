//! Anthropic server-tool detection (Go: helps/claude_builtin_tools.go).

use std::collections::HashSet;

use cpa_json::{J, Value};

/// Go: `defaultClaudeBuiltinToolNames`.
const DEFAULT_BUILTIN_TOOL_NAMES: [&str; 4] =
    ["web_search", "code_execution", "text_editor", "computer"];

/// Type prefixes of Anthropic-operated tools (Go: the list inside `IsClaudeServerToolType`).
const SERVER_TOOL_PREFIXES: [&str; 10] = [
    "advisor_",
    "agent_toolset_",
    "bash_",
    "code_execution_",
    "computer_",
    "memory_",
    "text_editor_",
    "tool_search_tool_",
    "web_fetch_",
    "web_search_",
];

/// Go: `IsClaudeServerToolType`. True for a recognized Anthropic-operated tool type; client
/// `type:"custom"` declarations are not server tools and stay eligible for MCP aliasing.
pub fn is_claude_server_tool_type(tool_type: &str) -> bool {
    let lowered = tool_type.trim().to_lowercase();
    SERVER_TOOL_PREFIXES.iter().any(|p| lowered.starts_with(p))
}

/// Go: `AugmentClaudeBuiltinToolRegistry`. Starts from the default seed names (when `registry`
/// is `None`) and adds the name of every server-typed tool declared in `body.tools`.
pub fn augment_claude_builtin_tool_registry(
    body: &[u8],
    registry: Option<HashSet<String>>,
) -> HashSet<String> {
    let mut registry = registry.unwrap_or_else(|| {
        DEFAULT_BUILTIN_TOOL_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    // Valid bodies only parse the `tools` array; invalid ones fall back to the tolerant parser.
    let tools = match cpa_json::raw_at(body, "tools") {
        Some(raw) if cpa_json::valid(body) => cpa_json::parse_str(raw),
        _ => crate::helps::parse_cache::parse(body)
            .g("tools")
            .into_value()
            .unwrap_or(Value::Null),
    };
    let Value::Array(items) = &tools else {
        return registry;
    };
    for tool in items {
        if !is_claude_server_tool_type(&tool.g("type").str()) {
            continue;
        }
        let name = tool.g("name").str();
        if !name.is_empty() {
            registry.insert(name);
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_seed_fallback() {
        let registry = augment_claude_builtin_tool_registry(b"", None);
        for name in DEFAULT_BUILTIN_TOOL_NAMES {
            assert!(registry.contains(name), "{name}");
        }
    }

    #[test]
    fn augments_known_typed_builtins_from_body() {
        let registry = augment_claude_builtin_tool_registry(
            br#"{
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search"},
                    {"type": "custom", "name": "client_custom"},
                    {"type": "custom_builtin_20250401", "name": "unknown_typed"},
                    {"name": "Read"}
                ]
            }"#,
            None,
        );
        assert!(registry.contains("web_search"));
        for name in ["client_custom", "unknown_typed", "Read"] {
            assert!(!registry.contains(name), "{name}");
        }
    }

    #[test]
    fn server_tool_type_prefixes() {
        for t in [
            "web_search_20250305",
            "code_execution_20250522",
            "tool_search_tool_regex_20251119",
            "advisor_20260301",
            "agent_toolset_20260401",
            "bash_20250124",
            "text_editor_20250728",
            "memory_20250818",
            "computer_20241022",
            "web_fetch_20260209",
        ] {
            assert!(is_claude_server_tool_type(t), "{t}");
        }
        for t in ["", "custom", "custom_builtin_20250401"] {
            assert!(!is_claude_server_tool_type(t), "{t}");
        }
    }
}
