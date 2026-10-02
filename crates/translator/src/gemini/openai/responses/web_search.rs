//! Native web search support for the Responses -> Gemini bridge (Go:
//! gemini_openai-responses_web_search.go): tool detection, grounding metadata extraction and
//! merging, and url_citation construction.
//!
//! Request roots are `&Value` documents; grounding metadata is an owned `Value` (`None` is Go's
//! zero `gjson.Result`). Items and citations are `Value`s instead of Go's raw byte slices.

use std::collections::{BTreeMap, HashMap, HashSet};

use cpa_core::registry::{antigravity_web_search_model_for, lookup_model_info, ModelInfo};
use cpa_json::{Res, Value, J};

/// Whether the model supports native web search. Explicit `native_capabilities.web_search =
/// false` in either the plain or the antigravity catalog entry vetoes; explicit true wins;
/// otherwise dynamic probes and the `supports_web_search` flag decide.
pub fn model_supports_web_search(model_id: &str) -> bool {
    let info = lookup_model_info(model_id, None);
    let info_ag = lookup_model_info(model_id, Some("antigravity"));
    let explicit = |i: &Option<ModelInfo>| i.as_ref().and_then(|i| i.native_capabilities.as_ref()).and_then(|n| n.web_search);

    if explicit(&info) == Some(false) || explicit(&info_ag) == Some(false) {
        return false;
    }
    if explicit(&info) == Some(true) || explicit(&info_ag) == Some(true) {
        return true;
    }
    if !antigravity_web_search_model_for(model_id).is_empty() {
        return true;
    }
    info.as_ref().is_some_and(|i| i.supports_web_search) || info_ag.as_ref().is_some_and(|i| i.supports_web_search)
}

/// OpenAI web search tool types: current, dated and legacy preview names.
fn is_responses_web_search_tool_type(tool_type: &str) -> bool {
    matches!(tool_type, "web_search" | "web_search_2025_08_26" | "web_search_preview" | "web_search_preview_2025_03_11")
}

/// True when `tools` contains an OpenAI web search tool.
pub fn has_responses_web_search_tool(root: &Value) -> bool {
    let tools = root.g("tools");
    tools.is_array() && tools.array().iter().any(|t| is_responses_web_search_tool_type(&t.g("type").str()))
}

/// True when `tools` is non-empty and every tool is a web search tool.
pub fn has_only_responses_web_search_tools(root: &Value) -> bool {
    let tools = root.g("tools");
    if !tools.is_array() {
        return false;
    }
    let mut has_search = false;
    for tool in tools.array() {
        if is_responses_web_search_tool_type(&tool.g("type").str()) {
            has_search = true;
            continue;
        }
        return false;
    }
    has_search
}

/// Whether `tool_choice` permits web search execution.
pub fn allows_responses_web_search_tool_choice(root: &Value) -> bool {
    let tool_choice = root.g("tool_choice");
    if !tool_choice.exists() {
        return true;
    }
    if tool_choice.is_string() {
        return matches!(tool_choice.str().as_str(), "" | "auto" | "required");
    }
    if tool_choice.is_object() {
        return match tool_choice.g("type").str().as_str() {
            "" | "auto" | "required" => true,
            "web_search" | "web_search_2025_08_26" | "web_search_preview" | "web_search_preview_2025_03_11" => true,
            "allowed_tools" => {
                let tools = tool_choice.g("tools");
                tools.is_array() && tools.array().iter().any(|t| is_responses_web_search_tool_type(&t.g("type").str()))
            }
            _ => false,
        };
    }
    false
}

/// The search query from Responses input: a string input, flat `input_text` parts, the last user
/// message, or the instructions as a fallback.
pub fn extract_responses_web_search_query(root: &Value) -> String {
    let input = root.g("input");
    if input.is_string() {
        return input.str().trim().to_string();
    }
    if input.is_array() {
        let items = input.array();
        // A flat array of content parts, e.g. [{"type":"input_text",...}].
        let mut flat_parts: Vec<String> = Vec::new();
        let mut is_flat_parts = true;
        for item in &items {
            if item.g("type").str() == "input_text" {
                let text = item.g("text").str().trim().to_string();
                if !text.is_empty() {
                    flat_parts.push(text);
                }
            } else if item.g("role").exists() {
                is_flat_parts = false;
                break;
            }
        }
        if is_flat_parts && !flat_parts.is_empty() {
            return flat_parts.join("\n");
        }

        for item in items.iter().rev() {
            let role = item.g("role").str();
            if !role.is_empty() && role != "user" {
                continue;
            }
            let content = item.g("content");
            if content.is_string() && !content.str().trim().is_empty() {
                return content.str().trim().to_string();
            }
            if content.is_array() {
                let text_parts: Vec<String> = content
                    .array()
                    .iter()
                    .map(|p| p.g("text").str().trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !text_parts.is_empty() {
                    return text_parts.join("\n");
                }
            }
            let text = item.g("text").str().trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    let instructions = root.g("instructions").str();
    if !instructions.trim().is_empty() {
        return instructions.trim().to_string();
    }
    String::new()
}

/// Allowed domains from the first web search tool that has `filters.allowed_domains`.
pub fn extract_responses_web_search_allowed_domains(root: &Value) -> Vec<String> {
    let tools = root.g("tools");
    if !tools.is_array() {
        return Vec::new();
    }
    for tool in tools.array() {
        if !is_responses_web_search_tool_type(&tool.g("type").str()) {
            continue;
        }
        let allowed = tool.g("filters.allowed_domains");
        if !allowed.is_array() {
            continue;
        }
        return allowed.array().iter().map(|d| d.str().trim().to_string()).filter(|d| !d.is_empty()).collect();
    }
    Vec::new()
}

/// `groundingMetadata` of the first candidate, in a direct or `response`-wrapped Gemini payload.
pub fn extract_grounding_metadata(root: &Value) -> Option<Value> {
    ["candidates.0.groundingMetadata", "response.candidates.0.groundingMetadata"]
        .iter()
        .find_map(|path| root.g(path).into_value())
}

/// Non-blank `webSearchQueries`, trimmed.
pub fn extract_grounding_queries(grounding_metadata: &Value) -> Vec<String> {
    let queries = grounding_metadata.g("webSearchQueries");
    if !queries.is_array() {
        return Vec::new();
    }
    queries.array().iter().map(|q| q.str().trim().to_string()).filter(|q| !q.is_empty()).collect()
}

/// `{"type":"url","url":...}` sources from `groundingChunks`, deduplicated by URI.
pub fn extract_grounding_sources(grounding_metadata: &Value) -> Vec<Value> {
    let mut sources = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for chunk in grounding_metadata.g("groundingChunks").array() {
        let uri = chunk.g("web.uri").str().trim().to_string();
        if uri.is_empty() || !seen.insert(uri.clone()) {
            continue;
        }
        let mut src = cpa_json::parse_str(r#"{"type":"url","url":""}"#);
        cpa_json::set(&mut src, "url", uri);
        sources.push(src);
    }
    sources
}

/// A Responses `web_search_call` output item.
pub fn build_responses_web_search_call_item(id: &str, query: &str, queries: &[String], sources: &[Value]) -> Value {
    let mut item = cpa_json::parse_str(r#"{"id":"","type":"web_search_call","status":"completed","action":{"type":"search","query":""}}"#);
    cpa_json::set(&mut item, "id", id);
    cpa_json::set(&mut item, "action.query", query);
    if !queries.is_empty() {
        cpa_json::set(&mut item, "action.queries", queries.to_vec());
    }
    if !sources.is_empty() {
        cpa_json::set(&mut item, "action.sources", Value::Array(sources.to_vec()));
    }
    item
}

/// True when the metadata has a non-blank web search query or a chunk with a non-blank URI.
pub fn has_valid_web_grounding(grounding_metadata: &Value) -> bool {
    let queries = grounding_metadata.g("webSearchQueries");
    if queries.is_array() && queries.array().iter().any(|q| !q.str().trim().is_empty()) {
        return true;
    }
    let chunks = grounding_metadata.g("groundingChunks");
    chunks.is_array() && chunks.array().iter().any(|c| !c.g("web.uri").str().trim().is_empty())
}

fn is_blank_metadata(gm: Option<&Value>) -> bool {
    gm.is_none_or(|v| v.to_string().trim().is_empty())
}

/// Merges incremental grounding metadata into the accumulated one: queries are unioned in order,
/// chunks are deduplicated by URI with a cumulative raw-index remap (`_chunkIndexRemap`,
/// `_rawChunkCount` bookkeeping keys), and supports are remapped and deduplicated.
pub fn merge_grounding_metadata(existing: Option<&Value>, new: Option<&Value>) -> Option<Value> {
    if is_blank_metadata(existing) && is_blank_metadata(new) {
        return existing.cloned();
    }
    let empty = Value::Object(Default::default());
    let existing_gm = if is_blank_metadata(existing) { &empty } else { existing.unwrap_or(&empty) };
    let Some(new_gm) = new.filter(|_| !is_blank_metadata(new)) else {
        return Some(existing_gm.clone());
    };
    let mut merged = existing_gm.clone();
    if !merged.is_object() {
        merged = empty.clone();
    }

    // 1. webSearchQueries (order preserved, no duplicates).
    let existing_queries = extract_grounding_queries(existing_gm);
    let new_queries = extract_grounding_queries(new_gm);
    if !new_queries.is_empty() {
        let mut seen: HashSet<&str> = HashSet::new();
        let merged_queries: Vec<&str> = existing_queries
            .iter()
            .chain(new_queries.iter())
            .map(String::as_str)
            .filter(|q| seen.insert(q))
            .collect();
        cpa_json::set(&mut merged, "webSearchQueries", merged_queries.into_iter().map(String::from).collect::<Vec<_>>());
    }

    // 2. groundingChunks with index remapping.
    let existing_chunks_res = existing_gm.g("groundingChunks");
    let existing_chunks = existing_chunks_res.array();
    let new_chunks_res = new_gm.g("groundingChunks");
    let new_chunks = new_chunks_res.array();

    let mut cumulative_remap: HashMap<i64, i64> = HashMap::new();
    let remap_json = existing_gm.g("_chunkIndexRemap");
    if remap_json.exists() && remap_json.is_object() {
        for (k, v) in remap_json.entries() {
            if let Ok(old_idx) = k.parse::<i64>() {
                cumulative_remap.insert(old_idx, v.int());
            }
        }
    }
    let raw_count_res = existing_gm.g("_rawChunkCount");
    let prev_raw_count: i64 = if raw_count_res.exists() { raw_count_res.int() } else { existing_chunks.len() as i64 };
    if cumulative_remap.is_empty() && !existing_chunks.is_empty() {
        for i in 0..existing_chunks.len() as i64 {
            cumulative_remap.insert(i, i);
        }
    }

    let mut merged_chunks: Vec<Value> = Vec::new();
    let mut uri_to_merged: HashMap<String, usize> = HashMap::new();
    let mut raw_to_merged: HashMap<String, usize> = HashMap::new();

    for (i, chunk) in existing_chunks.iter().enumerate() {
        merged_chunks.push(chunk.value());
        let uri = chunk.g("web.uri").str().trim().to_string();
        if !uri.is_empty() {
            uri_to_merged.entry(uri).or_insert(i);
        }
        raw_to_merged.insert(chunk.raw(), i);
    }

    for (i, chunk) in new_chunks.iter().enumerate() {
        let i = i as i64;
        let uri = chunk.g("web.uri").str().trim().to_string();
        let title = chunk.g("web.title").str().trim().to_string();
        let new_raw_idx = prev_raw_count + i;
        if !uri.is_empty() {
            if let Some(&existing_idx) = uri_to_merged.get(&uri) {
                cumulative_remap.insert(new_raw_idx, existing_idx as i64);
                if prev_raw_count == 0 {
                    cumulative_remap.insert(i, existing_idx as i64);
                }
                if !title.is_empty() && merged_chunks[existing_idx].g("web.title").str().trim().is_empty() {
                    cpa_json::set(&mut merged_chunks[existing_idx], "web.title", title);
                }
                continue;
            }
        } else if let Some(&existing_idx) = raw_to_merged.get(&chunk.raw()) {
            cumulative_remap.insert(new_raw_idx, existing_idx as i64);
            if prev_raw_count == 0 {
                cumulative_remap.insert(i, existing_idx as i64);
            }
            continue;
        }

        let new_idx = merged_chunks.len();
        merged_chunks.push(chunk.value());
        if !uri.is_empty() {
            uri_to_merged.insert(uri, new_idx);
        }
        raw_to_merged.insert(chunk.raw(), new_idx);
        cumulative_remap.insert(new_raw_idx, new_idx as i64);
        if prev_raw_count == 0 {
            cumulative_remap.insert(i, new_idx as i64);
        }
    }

    if !merged_chunks.is_empty() {
        cpa_json::set(&mut merged, "groundingChunks", Value::Array(merged_chunks));
    }

    let total_raw_count = prev_raw_count + new_chunks.len() as i64;
    if !cumulative_remap.is_empty() {
        let mut keys: Vec<i64> = cumulative_remap.keys().copied().collect();
        keys.sort_unstable();
        let mut remap = cpa_json::Map::new();
        for k in keys {
            remap.insert(k.to_string(), Value::from(cumulative_remap[&k]));
        }
        cpa_json::set(&mut merged, "_chunkIndexRemap", Value::Object(remap));
        cpa_json::set(&mut merged, "_rawChunkCount", total_raw_count);
    }

    // 3. groundingSupports with remapped chunk indices.
    let existing_supports_res = existing_gm.g("groundingSupports");
    let existing_supports = existing_supports_res.array();
    let new_supports_res = new_gm.g("groundingSupports");
    let new_supports = new_supports_res.array();
    let existing_chunk_count = existing_chunks.len() as i64;

    let support_key = |part_index: i64, start: i64, end: i64, indices: &[i64]| {
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        format!("{part_index}:{start}:{end}:{sorted:?}")
    };

    // Returns the deduplicated remapped indices and whether the support needs rewriting.
    let remap_support_indices = |orig: &[Res<'_>], is_existing: bool| -> (Vec<i64>, bool) {
        let mut remapped: Vec<i64> = Vec::new();
        let mut need_rewrite = false;
        for idx_res in orig {
            let old_idx = idx_res.int();
            let mut target = old_idx;
            // Existing supports hold cumulative raw stream indices; unresolved (pending) ones
            // resolve strictly through the cumulative remap. Indices of the current frame are
            // stream-wide raw chunk indices, resolved the same way.
            let should_remap = !is_existing || existing_chunk_count == 0 || old_idx >= existing_chunk_count;
            if should_remap
                && let Some(&t) = cumulative_remap.get(&old_idx) {
                    target = t;
                    if target != old_idx {
                        need_rewrite = true;
                    }
                }
            if !remapped.contains(&target) {
                remapped.push(target);
            }
        }
        if remapped.len() != orig.len() {
            need_rewrite = true;
        }
        (remapped, need_rewrite)
    };

    let mut seen_supports: HashSet<String> = HashSet::new();
    let mut merged_supports: Vec<Value> = Vec::new();
    for (supports, is_existing) in [(&existing_supports, true), (&new_supports, false)] {
        for s in supports {
            let part_index = if s.g("segment.partIndex").exists() { s.g("segment.partIndex").int() } else { 0 };
            let start = s.g("segment.startIndex").int();
            let end = s.g("segment.endIndex").int();
            let (remapped, need_rewrite) = remap_support_indices(&s.g("groundingChunkIndices").array(), is_existing);
            if !seen_supports.insert(support_key(part_index, start, end, &remapped)) {
                continue;
            }
            let mut raw_support = s.value();
            if need_rewrite {
                cpa_json::set(&mut raw_support, "groundingChunkIndices", Value::Array(remapped.into_iter().map(Value::from).collect()));
            }
            merged_supports.push(raw_support);
        }
    }
    if !merged_supports.is_empty() {
        cpa_json::set(&mut merged, "groundingSupports", Value::Array(merged_supports));
    }

    // 4. searchEntryPoint follows the newest frame.
    let sep = new_gm.g("searchEntryPoint");
    if let Some(v) = sep.v() {
        cpa_json::set(&mut merged, "searchEntryPoint", v.clone());
    }

    // 5. retrievalQueries is kept from the first frame that has it.
    let rq = new_gm.g("retrievalQueries");
    if let Some(v) = rq.v()
        && !existing_gm.g("retrievalQueries").exists() {
            cpa_json::set(&mut merged, "retrievalQueries", v.clone());
        }

    Some(merged)
}

fn citation_key(a: &Value) -> String {
    format!("{}:{}:{}", a.g("url").str(), a.g("start_index").int(), a.g("end_index").int())
}

/// Merges two citation lists, deduplicating by URL and rune offsets; a late citation replaces an
/// existing one that has an empty title.
pub fn merge_citation_annotations(existing: &[Value], late: &[Value]) -> Vec<Value> {
    if existing.is_empty() {
        return late.to_vec();
    }
    if late.is_empty() {
        return existing.to_vec();
    }
    let mut result: Vec<Value> = Vec::with_capacity(existing.len() + late.len());
    let mut key_to_index: HashMap<String, usize> = HashMap::new();
    for a in existing {
        let key = citation_key(a);
        if let std::collections::hash_map::Entry::Vacant(e) = key_to_index.entry(key) {
            e.insert(result.len());
            result.push(a.clone());
        }
    }
    for a in late {
        let key = citation_key(a);
        if let Some(&idx) = key_to_index.get(&key) {
            if result[idx].g("title").str().is_empty() && !a.g("title").str().is_empty() {
                result[idx] = a.clone();
            }
        } else {
            key_to_index.insert(key, result.len());
            result.push(a.clone());
        }
    }
    result
}

/// Go `utf8.RuneCount`: invalid bytes count as one rune each.
pub(super) fn go_rune_count(bytes: &[u8]) -> i64 {
    let mut count = 0i64;
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => return count + s.chars().count() as i64,
            Err(e) => {
                let valid = e.valid_up_to();
                // The valid prefix is UTF-8 by construction.
                count += String::from_utf8_lossy(&rest[..valid]).chars().count() as i64 + 1;
                rest = &rest[valid + 1..];
            }
        }
    }
}

/// UTF-8 byte offset to 0-based rune offset in `text`.
fn byte_offset_to_rune_offset(text: &str, byte_offset: i64) -> i64 {
    if byte_offset <= 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    if byte_offset >= bytes.len() as i64 {
        return go_rune_count(bytes);
    }
    go_rune_count(&bytes[..byte_offset as usize])
}

/// Where a Gemini content part sits inside an OpenAI response message.
#[derive(Debug, Clone, Default)]
pub struct GeminiPartMapping {
    pub part_index: i64,
    pub message_index: i64,
    pub start_rune_in_msg: i64,
    pub part_text: String,
}

/// A citation range within one message.
#[derive(Debug, Clone, Copy)]
pub struct MessageRuneRange {
    pub message_index: i64,
    pub start_index: i64,
    pub end_index: i64,
}

/// Maps a byte range over the concatenated part texts to per-message rune ranges.
fn map_byte_offsets_to_rune_ranges(mappings: &[GeminiPartMapping], mut start_byte: i64, mut end_byte: i64) -> Vec<MessageRuneRange> {
    if mappings.is_empty() {
        return Vec::new();
    }
    if start_byte < 0 {
        start_byte = 0;
    }
    if start_byte >= end_byte {
        return Vec::new();
    }

    let mut spans: Vec<(&GeminiPartMapping, i64, i64)> = Vec::with_capacity(mappings.len());
    let mut cum_bytes = 0i64;
    for m in mappings {
        let p_bytes = m.part_text.len() as i64;
        spans.push((m, cum_bytes, cum_bytes + p_bytes));
        cum_bytes += p_bytes;
    }
    if start_byte >= cum_bytes {
        return Vec::new();
    }
    if end_byte > cum_bytes {
        end_byte = cum_bytes;
    }

    let mut ranges: Vec<MessageRuneRange> = Vec::new();
    for (m, cum_start, cum_end) in spans {
        let overlap_start = start_byte.max(cum_start);
        let overlap_end = end_byte.min(cum_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let part_start_rune = byte_offset_to_rune_offset(&m.part_text, overlap_start - cum_start);
        let part_end_rune = byte_offset_to_rune_offset(&m.part_text, overlap_end - cum_start);
        if part_end_rune <= part_start_rune || part_start_rune < 0 {
            continue;
        }
        let start_rune = m.start_rune_in_msg + part_start_rune;
        let end_rune = m.start_rune_in_msg + part_end_rune;
        match ranges.last_mut() {
            Some(last) if last.message_index == m.message_index && last.end_index == start_rune => last.end_index = end_rune,
            _ => ranges.push(MessageRuneRange { message_index: m.message_index, start_index: start_rune, end_index: end_rune }),
        }
    }
    ranges
}

/// `url_citation` annotations per message index, with part-level byte offsets translated to
/// message-level rune offsets using `mappings` (or `message_texts[0]` when there are none).
pub fn build_responses_url_citations_for_messages(
    grounding_metadata: &Value,
    mappings: &[GeminiPartMapping],
    message_texts: &[String],
) -> BTreeMap<i64, Vec<Value>> {
    let chunks_res = grounding_metadata.g("groundingChunks");
    let chunks = chunks_res.array();
    let supports_res = grounding_metadata.g("groundingSupports");
    let supports = supports_res.array();
    let mut result: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
    if supports.is_empty() || chunks.is_empty() {
        return result;
    }

    // Coalesce adjacent mappings sharing part and message index.
    let mut coalesced: Vec<GeminiPartMapping> = Vec::new();
    for m in mappings {
        match coalesced.last_mut() {
            Some(last) if last.part_index == m.part_index && last.message_index == m.message_index => last.part_text.push_str(&m.part_text),
            _ => coalesced.push(m.clone()),
        }
    }
    let mappings = coalesced;

    let mut seen: HashSet<String> = HashSet::new();
    for support in &supports {
        let segment = support.g("segment");
        let has_part_index = segment.g("partIndex").exists();
        let part_index = segment.g("partIndex").int();
        let start_byte = segment.g("startIndex").int();
        let end_byte = segment.g("endIndex").int();

        let ranges = if has_part_index {
            let mut part_mappings: Vec<GeminiPartMapping> = mappings.iter().filter(|m| m.part_index == part_index).cloned().collect();
            if part_mappings.is_empty() && part_index == 0 && mappings.len() == 1 {
                part_mappings = mappings.clone();
            }
            map_byte_offsets_to_rune_ranges(&part_mappings, start_byte, end_byte)
        } else if !mappings.is_empty() {
            map_byte_offsets_to_rune_ranges(&mappings, start_byte, end_byte)
        } else if let Some(msg_text) = message_texts.first() {
            let start_rune = byte_offset_to_rune_offset(msg_text, start_byte);
            let end_rune = byte_offset_to_rune_offset(msg_text, end_byte);
            if end_rune > start_rune && start_rune >= 0 {
                vec![MessageRuneRange { message_index: 0, start_index: start_rune, end_index: end_rune }]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if ranges.is_empty() {
            continue;
        }

        for idx_result in support.g("groundingChunkIndices").array() {
            let idx = idx_result.int();
            if idx < 0 || idx >= chunks.len() as i64 {
                continue;
            }
            let chunk = &chunks[idx as usize];
            let uri = chunk.g("web.uri").str().trim().to_string();
            let title = chunk.g("web.title").str().trim().to_string();
            if uri.is_empty() {
                continue;
            }
            for r in &ranges {
                let key = format!("{}:{}:{}:{}", r.message_index, uri, r.start_index, r.end_index);
                if !seen.insert(key) {
                    continue;
                }
                let mut cite = cpa_json::parse_str(r#"{"type":"url_citation","url":"","title":"","start_index":0,"end_index":0}"#);
                cpa_json::set(&mut cite, "url", uri.as_str());
                cpa_json::set(&mut cite, "title", title.as_str());
                cpa_json::set(&mut cite, "start_index", r.start_index);
                cpa_json::set(&mut cite, "end_index", r.end_index);
                result.entry(r.message_index).or_default().push(cite);
            }
        }
    }
    result
}

/// `url_citation` annotations for a single message. `text` (Go's variadic) enables converting
/// UTF-8 byte offsets to rune offsets.
pub fn build_responses_url_citations(grounding_metadata: &Value, text: Option<&str>) -> Vec<Value> {
    let full_text = text.unwrap_or_default();
    let mappings = if full_text.is_empty() {
        Vec::new()
    } else {
        vec![GeminiPartMapping { part_index: 0, message_index: 0, start_rune_in_msg: 0, part_text: full_text.to_string() }]
    };
    let texts: Vec<String> = text.map(|t| vec![t.to_string()]).unwrap_or_default();
    let res_map = build_responses_url_citations_for_messages(grounding_metadata, &mappings, &texts);
    if let Some(c) = res_map.get(&0).filter(|c| !c.is_empty()) {
        return c.clone();
    }
    res_map.into_values().find(|c| !c.is_empty()).unwrap_or_default()
}
