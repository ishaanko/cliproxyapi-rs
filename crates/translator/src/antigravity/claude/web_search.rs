//! Claude `web_search` tool support for Antigravity: native Google Search request building and
//! grounding metadata translation (Go: antigravity/claude/web_search.go).

use std::collections::HashSet;

use cpa_core::registry::antigravity_web_search_model_for;
use cpa_json::{json, J, Value};

pub const WEB_SEARCH_SYSTEM_INSTRUCTION: &str = "You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar.";

struct GroundingSupport {
    start_index: i64,
    end_index: i64,
    chunk_urls: Vec<String>,
    chunk_title: String,
}

pub struct CitedTextBlock {
    pub text: String,
    pub citations: Vec<Value>,
}

fn supports_native_google_search(model: &str) -> bool {
    !antigravity_web_search_model_for(model).is_empty()
}

pub fn is_claude_typed_web_search_tool_type(tool_type: &str) -> bool {
    tool_type == "web_search_20250305" || tool_type == "web_search_20260209"
}

pub fn has_claude_typed_web_search_tool(payload: &Value) -> bool {
    let tools = payload.g("tools");
    tools.is_array() && tools.array().iter().any(|t| is_claude_typed_web_search_tool_type(&t.g("type").str()))
}

fn has_only_claude_typed_web_search_tools(payload: &Value) -> bool {
    let tools = payload.g("tools");
    if !tools.is_array() {
        return false;
    }
    let tools = tools.array();
    !tools.is_empty() && tools.iter().all(|t| is_claude_typed_web_search_tool_type(&t.g("type").str()))
}

fn allows_claude_web_search_tool_choice(payload: &Value) -> bool {
    let tool_choice = payload.g("tool_choice");
    if !tool_choice.exists() {
        return true;
    }
    if tool_choice.is_string() {
        return matches!(tool_choice.str().as_str(), "" | "auto" | "any");
    }
    if !tool_choice.is_object() {
        return false;
    }
    match tool_choice.g("type").str().as_str() {
        "" | "auto" | "any" => true,
        "tool" => tool_choice.g("name").str() == "web_search",
        _ => false,
    }
}

pub fn should_build_web_search_request(model: &str, payload: &Value) -> bool {
    supports_native_google_search(model) && has_only_claude_typed_web_search_tools(payload) && allows_claude_web_search_tool_choice(payload)
}

/// The Antigravity `requestType: web_search` request for a Claude request that only declares the
/// typed web search tool.
pub fn build_web_search_request(model: &str, payload: &Value) -> Vec<u8> {
    let query = extract_query(payload);
    let max_result_count = extract_max_uses(payload);
    let included_domains = extract_allowed_domains(payload);
    let mut out = json!({
        "model": "",
        "requestType": "web_search",
        "request": {
            "contents": [{"role": "user", "parts": [{"text": ""}]}],
            "systemInstruction": {"role": "user", "parts": [{"text": ""}]},
            "tools": [{"googleSearch": {"enhancedContent": {"imageSearch": {"maxResultCount": 5}}}}],
            "generationConfig": {"candidateCount": 1}
        }
    });
    cpa_json::set(&mut out, "model", model);
    cpa_json::set(&mut out, "request.contents.0.parts.0.text", query);
    cpa_json::set(&mut out, "request.systemInstruction.parts.0.text", WEB_SEARCH_SYSTEM_INSTRUCTION);
    cpa_json::set(&mut out, "request.tools.0.googleSearch.enhancedContent.imageSearch.maxResultCount", max_result_count);
    if !included_domains.is_empty() {
        cpa_json::set(&mut out, "request.tools.0.googleSearch.includedDomains", json!(included_domains));
    }
    cpa_json::to_vec(&out)
}

fn extract_max_uses(payload: &Value) -> i64 {
    const DEFAULT_MAX_RESULT_COUNT: i64 = 5;
    let tools = payload.g("tools");
    if !tools.is_array() {
        return DEFAULT_MAX_RESULT_COUNT;
    }
    for tool in tools.array() {
        if !is_claude_typed_web_search_tool_type(&tool.g("type").str()) {
            continue;
        }
        let max_uses = tool.g("max_uses").int();
        if max_uses > 0 {
            return max_uses;
        }
    }
    DEFAULT_MAX_RESULT_COUNT
}

/// Trimmed non-empty string domains of the first typed web search tool.
fn extract_allowed_domains(payload: &Value) -> Vec<String> {
    let tools = payload.g("tools");
    if !tools.is_array() {
        return vec![];
    }
    for tool in tools.array() {
        if !is_claude_typed_web_search_tool_type(&tool.g("type").str()) {
            continue;
        }
        let allowed = tool.g("allowed_domains");
        if !allowed.is_array() {
            return vec![];
        }
        return allowed
            .array()
            .iter()
            .filter(|d| d.is_string())
            .map(|d| d.str().trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
    }
    vec![]
}

fn extract_query(payload: &Value) -> String {
    let messages = payload.g("messages");
    if !messages.is_array() {
        return String::new();
    }
    for message in messages.array().iter().rev() {
        let role = message.g("role").str();
        if !role.is_empty() && role != "user" {
            continue;
        }
        let query = extract_text_content(&message.g("content"));
        if !query.is_empty() {
            return query;
        }
    }
    String::new()
}

fn extract_text_content(content: &cpa_json::Res<'_>) -> String {
    if content.is_string() {
        return content.str().trim().to_string();
    }
    if !content.is_array() {
        return String::new();
    }
    let mut b = String::new();
    for part in content.array() {
        let text = part.g("text").str();
        let text = text.trim();
        if !text.is_empty() {
            if !b.is_empty() {
                b.push('\n');
            }
            b.push_str(text);
        }
    }
    b.trim().to_string()
}

fn has_antigravity_google_search_tool(payload: &Value) -> bool {
    let tools = payload.g("request.tools");
    tools.is_array() && tools.array().iter().any(|t| t.g("googleSearch").exists())
}

/// Whether grounding metadata should become Claude web search blocks: the client declared the
/// typed tool and the translated request carries a `googleSearch` tool.
pub fn should_translate_grounding(original_request: &Value, request: &Value) -> bool {
    has_claude_typed_web_search_tool(original_request) && has_antigravity_google_search_tool(request)
}

pub fn grounding_metadata(root: &Value) -> Option<Value> {
    let g = root.g("response.candidates.0.groundingMetadata");
    if g.exists() {
        return Some(g.value());
    }
    root.g("candidates.0.groundingMetadata").into_value()
}

pub fn text_content(root: &Value) -> String {
    let mut parts = root.g("response.candidates.0.content.parts");
    if !parts.is_array() {
        parts = root.g("candidates.0.content.parts");
    }
    let mut out = String::new();
    if parts.is_array() {
        for part in parts.array() {
            let text = part.g("text");
            if text.exists() {
                out.push_str(&text.str());
            }
        }
    }
    out
}

fn query_from_grounding(grounding: &Value) -> String {
    let queries = grounding.g("webSearchQueries");
    if queries.is_array()
        && let Some(first) = queries.array().first() {
            return first.str();
        }
    String::new()
}

fn results_from_grounding(grounding: &Value) -> Value {
    let mut results = vec![];
    let chunks = grounding.g("groundingChunks");
    if !chunks.is_array() {
        return Value::Array(results);
    }
    let mut seen: HashSet<String> = HashSet::new();
    for chunk in chunks.array() {
        let web = chunk.g("web");
        if !web.exists() {
            continue;
        }
        let uri = web.g("uri").str().trim().to_string();
        if uri.is_empty() || !seen.insert(uri.clone()) {
            continue;
        }
        let mut result = json!({"type": "web_search_result", "page_age": null});
        let title = web.g("title");
        if title.exists() {
            cpa_json::set(&mut result, "title", title.str());
        }
        cpa_json::set(&mut result, "url", uri);
        results.push(result);
    }
    Value::Array(results)
}

fn parse_grounding_supports(grounding: &Value) -> Vec<GroundingSupport> {
    let chunks = grounding.g("groundingChunks");
    if !chunks.is_array() {
        return vec![];
    }
    let chunk_data: Vec<(String, String)> = chunks
        .array()
        .iter()
        .map(|chunk| {
            let web = chunk.g("web");
            if web.exists() {
                (web.g("uri").str(), web.g("title").str())
            } else {
                (String::new(), String::new())
            }
        })
        .collect();
    let supports = grounding.g("groundingSupports");
    if !supports.is_array() {
        return vec![];
    }
    let mut out = vec![];
    for support in supports.array() {
        let segment = support.g("segment");
        if !segment.exists() {
            continue;
        }
        let mut parsed = GroundingSupport {
            start_index: segment.g("startIndex").int(),
            end_index: segment.g("endIndex").int(),
            chunk_urls: vec![],
            chunk_title: String::new(),
        };
        let indices = support.g("groundingChunkIndices");
        if indices.is_array() {
            for idx in indices.array() {
                let i = idx.int();
                if i < 0 || i as usize >= chunk_data.len() {
                    continue;
                }
                let (url, title) = &chunk_data[i as usize];
                parsed.chunk_urls.push(url.clone());
                if parsed.chunk_title.is_empty() {
                    parsed.chunk_title = title.clone();
                }
            }
        }
        out.push(parsed);
    }
    out
}

/// Splits the answer text at the grounding segments' byte offsets into plain and cited blocks.
fn build_cited_text_blocks(text_content: &str, supports: &[GroundingSupport]) -> Vec<CitedTextBlock> {
    if supports.is_empty() {
        if text_content.is_empty() {
            return vec![];
        }
        return vec![CitedTextBlock { text: text_content.to_string(), citations: vec![] }];
    }
    let text = text_content.as_bytes();
    let slice = |start: usize, end: usize| String::from_utf8_lossy(&text[start..end]).into_owned();
    let mut blocks = vec![];
    let mut last_end: i64 = 0;
    for support in supports {
        if support.end_index <= last_end {
            continue;
        }
        if support.start_index > last_end {
            let start = last_end as usize;
            let end = (support.start_index.max(0) as usize).min(text.len());
            if start < end {
                blocks.push(CitedTextBlock { text: slice(start, end), citations: vec![] });
            }
        }
        let cited_start = support.start_index.max(last_end);
        let mut cited_text = String::new();
        if cited_start < support.end_index {
            let start = (cited_start.max(0) as usize).min(text.len());
            let end = (support.end_index.max(0) as usize).min(text.len());
            if start < end {
                cited_text = slice(start, end);
            }
        }
        if !cited_text.is_empty() && !support.chunk_urls.is_empty() {
            // Go marshals a map[string]any: keys sorted.
            let citation = json!({
                "cited_text": cited_text,
                "title": support.chunk_title,
                "type": "web_search_result_location",
                "url": support.chunk_urls[0],
            });
            blocks.push(CitedTextBlock { text: cited_text, citations: vec![citation] });
        }
        if support.end_index > last_end {
            last_end = support.end_index;
        }
    }
    if (last_end as usize) < text.len() {
        blocks.push(CitedTextBlock { text: slice(last_end as usize, text.len()), citations: vec![] });
    }
    blocks
}

/// Non-stream content array: server_tool_use, web_search_tool_result, then cited text blocks.
pub fn build_claude_web_search_content(tool_use_id: &str, text_content: &str, grounding: &Value) -> Value {
    let mut content = vec![];
    let mut server_tool_use = json!({"type": "server_tool_use", "id": "", "name": "web_search", "input": {}});
    cpa_json::set(&mut server_tool_use, "id", tool_use_id);
    let query = query_from_grounding(grounding);
    if !query.is_empty() {
        cpa_json::set(&mut server_tool_use, "input.query", query);
    }
    content.push(server_tool_use);

    let mut result = json!({"type": "web_search_tool_result", "tool_use_id": "", "content": []});
    cpa_json::set(&mut result, "tool_use_id", tool_use_id);
    cpa_json::set(&mut result, "content", results_from_grounding(grounding));
    content.push(result);

    for block in build_cited_text_blocks(text_content, &parse_grounding_supports(grounding)) {
        if block.text.is_empty() {
            continue;
        }
        let mut text_block = json!({"type": "text", "text": ""});
        cpa_json::set(&mut text_block, "text", block.text);
        if !block.citations.is_empty() {
            cpa_json::set(&mut text_block, "citations", Value::Array(block.citations));
        }
        content.push(text_block);
    }
    Value::Array(content)
}

/// Stream equivalent of [`build_claude_web_search_content`]; returns the next free block index.
pub fn append_claude_web_search_stream_blocks(
    append_event: &mut dyn FnMut(&str, String),
    start_index: i64,
    tool_use_id: &str,
    text_content: &str,
    grounding: &Value,
) -> i64 {
    let mut content_index = start_index;

    append_event(
        "content_block_start",
        cpa_json::to_string(&json!({"type": "content_block_start", "index": content_index, "content_block": {"type": "server_tool_use", "id": tool_use_id, "name": "web_search", "input": {}}})),
    );
    let query = query_from_grounding(grounding);
    if !query.is_empty() {
        let query_json = cpa_json::to_string(&json!({"query": query}));
        append_event(
            "content_block_delta",
            cpa_json::to_string(&json!({"type": "content_block_delta", "index": content_index, "delta": {"type": "input_json_delta", "partial_json": query_json}})),
        );
    }
    append_event("content_block_stop", cpa_json::to_string(&json!({"type": "content_block_stop", "index": content_index})));
    content_index += 1;

    append_event(
        "content_block_start",
        cpa_json::to_string(&json!({"type": "content_block_start", "index": content_index, "content_block": {"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": results_from_grounding(grounding)}})),
    );
    append_event("content_block_stop", cpa_json::to_string(&json!({"type": "content_block_stop", "index": content_index})));
    content_index += 1;

    for block in build_cited_text_blocks(text_content, &parse_grounding_supports(grounding)) {
        if block.text.is_empty() {
            continue;
        }
        let start = if block.citations.is_empty() {
            json!({"type": "content_block_start", "index": content_index, "content_block": {"type": "text", "text": ""}})
        } else {
            json!({"type": "content_block_start", "index": content_index, "content_block": {"citations": [], "type": "text", "text": ""}})
        };
        append_event("content_block_start", cpa_json::to_string(&start));
        for citation in &block.citations {
            append_event(
                "content_block_delta",
                cpa_json::to_string(&json!({"type": "content_block_delta", "index": content_index, "delta": {"type": "citations_delta", "citation": citation}})),
            );
        }
        for chunk in split_runes(&block.text, 50) {
            append_event(
                "content_block_delta",
                cpa_json::to_string(&json!({"type": "content_block_delta", "index": content_index, "delta": {"type": "text_delta", "text": chunk}})),
            );
        }
        append_event("content_block_stop", cpa_json::to_string(&json!({"type": "content_block_stop", "index": content_index})));
        content_index += 1;
    }
    content_index
}

fn split_runes(text: &str, chunk_size: usize) -> Vec<String> {
    if chunk_size == 0 || text.is_empty() {
        return vec![];
    }
    let runes: Vec<char> = text.chars().collect();
    runes.chunks(chunk_size).map(|c| c.iter().collect()).collect()
}

/// `srvtoolu_<unix nanos>` id for server tool blocks.
pub fn new_web_search_tool_use_id() -> String {
    let nanos = crate::common::unix_nano_now();
    format!("srvtoolu_{nanos}")
}
