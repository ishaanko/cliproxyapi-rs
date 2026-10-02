//! Codex web_search_call items -> Claude server_tool_use / web_search_tool_result blocks
//! (Go: codex_claude_response_web_search.go).

use std::collections::HashSet;

use cpa_core::util::go_json_string;
use cpa_json::{Res, Value, json};

use super::response::State;
use crate::common::append_sse_event_bytes;

fn emit(output: &mut Vec<u8>, event: &str, payload: &Value) {
    append_sse_event_bytes(output, event, &cpa_json::to_vec(payload), 2);
}

fn append_server_tool_use(output: &mut Vec<u8>, params: &mut State, root: &Value, item: &Res<'_>) {
    let tool_use_id = tool_use_id(params, root, item);
    if tool_use_id.is_empty() {
        return;
    }
    let query = query(root, item);
    let already_started = params.web_search_tool_use_ids.contains(&tool_use_id);
    if already_started && query.is_empty() {
        return;
    }

    if !already_started {
        output.extend(params.stop_text_block());
        output.extend(params.finalize_thinking_block());
        let template = json!({"type": "content_block_start", "index": params.block_index, "content_block": {"type": "server_tool_use", "id": tool_use_id, "name": "web_search", "input": {}}});
        emit(output, "content_block_start", &template);
    }

    if !query.is_empty() {
        // Marshaled like Go's json.Marshal (HTML escaped) before being embedded as a string.
        let partial_json = format!("{{\"query\":{}}}", go_json_string(&query));
        let delta = json!({"type": "content_block_delta", "index": params.block_index, "delta": {"type": "input_json_delta", "partial_json": partial_json}});
        emit(output, "content_block_delta", &delta);
    }

    if !already_started {
        let stop = json!({"type": "content_block_stop", "index": params.block_index});
        emit(output, "content_block_stop", &stop);
        params.web_search_tool_use_ids.insert(tool_use_id);
        params.block_index += 1;
    }
}

/// Emits the server_tool_use (if still pending) and the web_search_tool_result for a finished
/// web_search_call item.
pub(super) fn append_web_search_tool_result(
    output: &mut Vec<u8>,
    params: &mut State,
    root: &Value,
    item: &Res<'_>,
) {
    let tool_use_id = tool_use_id(params, root, item);
    if tool_use_id.is_empty() {
        return;
    }
    append_server_tool_use(output, params, root, item);
    if params.web_search_tool_result_ids.contains(&tool_use_id) {
        return;
    }
    let result_content = result_content(&Res::of(root), item);
    if query(root, item).is_empty() && result_content.is_none() && !item.g("action").exists() {
        return;
    }

    let mut template = json!({"type": "content_block_start", "index": params.block_index, "content_block": {"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": []}});
    if let Some(content) = result_content {
        cpa_json::set(
            &mut template,
            "content_block.content",
            Value::Array(content),
        );
    }
    emit(output, "content_block_start", &template);

    let stop = json!({"type": "content_block_stop", "index": params.block_index});
    emit(output, "content_block_stop", &stop);
    params
        .web_search_tool_result_ids
        .insert(tool_use_id.clone());
    params.block_index += 1;
    if tool_use_id == params.last_web_search_tool_use_id {
        params.last_web_search_tool_use_id.clear();
    }
}

/// The call's id from the item or event, else the last generated one, else a new
/// `web_search_<block index>` id.
fn tool_use_id(params: &mut State, root: &Value, item: &Res<'_>) -> String {
    let root = Res::of(root);
    for path in ["id", "output_item_id", "call_id"] {
        let value = item.g(path).str().trim().to_string();
        if !value.is_empty() {
            return value;
        }
        let value = root.g(path).str().trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    if !params.last_web_search_tool_use_id.is_empty() {
        return params.last_web_search_tool_use_id.clone();
    }
    let value = item.g("item_id").str().trim().to_string();
    if !value.is_empty() {
        return value;
    }
    let value = root.g("item_id").str().trim().to_string();
    if !value.is_empty() {
        return value;
    }
    let id = format!("web_search_{}", params.block_index);
    params.last_web_search_tool_use_id = id.clone();
    id
}

fn query(root: &Value, item: &Res<'_>) -> String {
    let root = Res::of(root);
    query_of(&root, item)
}

fn query_of(root: &Res<'_>, item: &Res<'_>) -> String {
    for path in ["action.query", "query", "input.query"] {
        let value = item.g(path).str().trim().to_string();
        if !value.is_empty() {
            return value;
        }
        let value = root.g(path).str().trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

/// `web_search_result` blocks from the item's (or event's) `results`; `None` when neither holds
/// an array.
fn result_content(root: &Res<'_>, item: &Res<'_>) -> Option<Vec<Value>> {
    let mut results = item.g("results");
    if !results.is_array() {
        results = root.g("results");
    }
    if !results.is_array() {
        return None;
    }
    let mut blocks = Vec::new();
    for result in results.array() {
        let url = result.g("url").str().trim().to_string();
        if url.is_empty() {
            continue;
        }
        let mut title = result.g("title").str().trim().to_string();
        if title.is_empty() {
            title = url.clone();
        }
        blocks.push(
            json!({"type": "web_search_result", "title": title, "url": url, "page_age": null}),
        );
    }
    Some(blocks)
}

/// Appends the server_tool_use and web_search_tool_result blocks of a non-stream web_search_call.
pub(super) fn append_web_search_non_stream_blocks(
    content_blocks: &mut Vec<Value>,
    item: &Res<'_>,
    seen: &mut HashSet<String>,
) {
    let id = item.g("id").str().trim().to_string();
    if id.is_empty() || seen.contains(&id) {
        return;
    }
    let query = query_of(&Res::NONE, item);
    let result_content = result_content(&Res::NONE, item);
    if query.is_empty() && result_content.is_none() {
        return;
    }

    let mut use_block =
        json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": {}});
    if !query.is_empty() {
        cpa_json::set(&mut use_block, "input", json!({"query": query}));
    }
    content_blocks.push(use_block);

    let mut result_block =
        json!({"type": "web_search_tool_result", "tool_use_id": id, "content": []});
    if let Some(content) = result_content {
        cpa_json::set(&mut result_block, "content", Value::Array(content));
    }
    content_blocks.push(result_block);
    seen.insert(id);
}
