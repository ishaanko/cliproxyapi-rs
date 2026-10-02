//! Devin tool declaration helpers (Go: common/devin_tools.go).

use std::sync::LazyLock;

use regex::Regex;

/// Whether a tool declaration is the `automation_update` method of the `mcp__codex_app`
/// namespace (either as namespace + tool, or as the joined `mcp__codex_app__automation_update`).
pub fn is_devin_codex_app_automation_update(namespace: &str, tool_name: &str) -> bool {
    let (namespace, tool) = (namespace.trim(), tool_name.trim());
    if namespace.eq_ignore_ascii_case("mcp__codex_app") && tool.eq_ignore_ascii_case("automation_update") {
        return true;
    }
    tool.eq_ignore_ascii_case("mcp__codex_app__automation_update")
}

const EXEC_COMMAND_TARGET_PHRASE: &str = "returning output or a session ID for ongoing interaction";
const EXEC_COMMAND_OBFUSCATED_PHRASE: &str = "returning output or an session ID for ongoing interaction";
const WRITE_STDIN_TARGET_PHRASE: &str =
    "Writes characters to an existing unified exec session and returns recent output.";
const WRITE_STDIN_OBFUSCATED_PHRASE: &str =
    "Writes characters to a existing unified exec session and returns recent output.";

static EXEC_COMMAND_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)returning output or a session ID for ongoing interaction").expect("static regex")
});
static WRITE_STDIN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Writes characters to an existing unified exec session and returns recent output(\.?)")
        .expect("static regex")
});

/// Replaces "returning output or a session ID for ongoing interaction" with "... an session ID ..."
/// (case-insensitively when the exact phrase is absent).
pub fn obfuscate_exec_command_description(desc: &str) -> String {
    if desc.contains(EXEC_COMMAND_OBFUSCATED_PHRASE) {
        return desc.to_string();
    }
    if desc.contains(EXEC_COMMAND_TARGET_PHRASE) {
        return desc.replace(EXEC_COMMAND_TARGET_PHRASE, EXEC_COMMAND_OBFUSCATED_PHRASE);
    }
    EXEC_COMMAND_REGEX
        .replace_all(desc, regex::NoExpand(EXEC_COMMAND_OBFUSCATED_PHRASE))
        .into_owned()
}

/// Replaces "Writes characters to an existing unified exec session and returns recent output."
/// with "Writes characters to a existing ..." (case-insensitively, keeping an optional final dot).
pub fn obfuscate_write_stdin_description(desc: &str) -> String {
    if desc.contains(WRITE_STDIN_OBFUSCATED_PHRASE) {
        return desc.to_string();
    }
    if desc.contains(WRITE_STDIN_TARGET_PHRASE) {
        return desc.replace(WRITE_STDIN_TARGET_PHRASE, WRITE_STDIN_OBFUSCATED_PHRASE);
    }
    WRITE_STDIN_REGEX
        .replace_all(desc, "Writes characters to a existing unified exec session and returns recent output${1}")
        .into_owned()
}

/// Applies description obfuscation for Devin function tools (`exec_command`, `write_stdin`, also
/// as the suffix of a namespaced name).
pub fn sanitize_devin_tool_description(tool_name: &str, desc: &str) -> String {
    if desc.is_empty() {
        return desc.to_string();
    }
    let clean_tool = tool_name.trim().to_lowercase();
    let mut desc = desc.to_string();
    if clean_tool == "exec_command" || clean_tool.ends_with("__exec_command") {
        desc = obfuscate_exec_command_description(&desc);
    }
    if clean_tool == "write_stdin" || clean_tool.ends_with("__write_stdin") {
        desc = obfuscate_write_stdin_description(&desc);
    }
    desc
}
