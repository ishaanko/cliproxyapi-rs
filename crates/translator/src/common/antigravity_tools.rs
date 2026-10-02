//! Antigravity intrinsic tool name collisions (Go: common/antigravity_tools.go).
//!
//! Antigravity agents (Interactions API) ship intrinsic sandbox tools such as `read_file`,
//! `write_file` and `execute_code`. When a client-defined function re-declares one of those
//! names, Google returns 500 "Unknown Error", so colliding client tools are renamed with
//! [`EXTERNAL_TOOL_PREFIX`] on the way upstream and the prefix is stripped on every response path.

pub const EXTERNAL_TOOL_PREFIX: &str = "external_";

/// Client-facing tool names that collide with the agent's built-in sandbox tools.
const COLLIDING_TOOL_NAMES: [&str; 3] = ["read_file", "write_file", "execute_code"];

/// Maps a client-facing tool name to the name sent upstream; names that do not collide are
/// returned unchanged.
pub fn antigravity_tool_name_to_upstream(name: &str) -> String {
    if COLLIDING_TOOL_NAMES.contains(&name) {
        return format!("{EXTERNAL_TOOL_PREFIX}{name}");
    }
    name.to_string()
}

/// Strips the external prefix from an upstream tool name when the base name is an intrinsic tool
/// that was prefixed to avoid collisions; other names pass through unchanged.
pub fn antigravity_upstream_tool_name_to_client(name: &str) -> String {
    if let Some(base) = name.strip_prefix(EXTERNAL_TOOL_PREFIX)
        && COLLIDING_TOOL_NAMES.contains(&base)
    {
        return base.to_string();
    }
    name.to_string()
}
