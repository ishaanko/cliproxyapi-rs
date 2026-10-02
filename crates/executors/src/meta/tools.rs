//! Meta Muse tool sanitizing (Go: helps/meta_tools.go).

use cpa_json::J;
use serde_json::Value;

/// Strips `search_content_types` from `web_search` tools (also inside `namespace` tools): Meta
/// accepts that field only on `web_search_preview`. Returns the input bytes when nothing changes.
pub fn sanitize_meta_web_search_tools(body: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    let Some(Value::Array(tools)) = root.g("tools").v().cloned() else {
        return body.to_vec();
    };
    let mut paths: Vec<String> = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        if tool.g("type").str() == "web_search" && tool.g("search_content_types").exists() {
            paths.push(format!("tools.{index}.search_content_types"));
        }
        if tool.g("type").str() == "namespace"
            && let Some(Value::Array(subtools)) = tool.g("tools").v()
        {
            for (sub_index, subtool) in subtools.iter().enumerate() {
                if subtool.g("type").str() == "web_search" && subtool.g("search_content_types").exists() {
                    paths.push(format!("tools.{index}.tools.{sub_index}.search_content_types"));
                }
            }
        }
    }
    if paths.is_empty() {
        return body.to_vec();
    }
    for path in &paths {
        cpa_json::delete(&mut root, path);
    }
    cpa_json::to_vec(&root)
}
