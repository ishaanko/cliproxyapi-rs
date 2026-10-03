//! JSON Schema cleaners for Gemini, Vertex and Antigravity tool schemas (Go: util/gemini_schema.go
//! and the keyword lists from claude_schema.go).
//!
//! Pass a single JSON schema to these functions, never a whole request document. Cleaning walks
//! every node and rewrites keys by name, and schema keywords such as `title`, `format`, `default`
//! and `const` are also ordinary data keys, so a request document would have its tool-call
//! arguments rewritten.
//!
//! The Go code edits one JSON string with gjson/sjson paths, phase after phase. This port keeps
//! that structure: it holds the document as a `Value` and applies the same path based edits with
//! `cpa_json`, so quirks of path handling (escaping only `. * ?`, root-level `const` landing under
//! an empty key, `null` for emptied string slices) carry over.
//!
//! Output differences from Go, all semantically neutral: the result is compact JSON (Go keeps the
//! input's whitespace for untouched parts), and strings are not HTML-escaped. Documents that are
//! not valid JSON are returned unchanged after the normalization phase (Go would still run its
//! lenient string edits on them).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use cpa_json::{J, Map, Value};

use super::gojson::{GoJsonStyle, go_json_canonicalize, go_json_sorted};
use super::translator::walk;

const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

/// JSON Schema keywords whose values are maps of subschemas.
pub const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

/// JSON Schema keywords with a single nested subschema or a slice of subschemas.
pub const SCHEMA_VALUE_KEYWORDS: [&str; 16] = [
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct CleanOptions {
    add_placeholder: bool,
    add_missing_array_items: bool,
    antigravity_semantics: bool,
    remove_tool_title: bool,
    remove_gemini_metadata: bool,
    flatten_unions: bool,
    force_enum_string_type: bool,
    drop_all_enums: bool,
    drop_boolean_enums: bool,
    preserve_additional_properties_false: bool,
    preserve_all_additional_properties: bool,
    preserve_standard_constraints: bool,
}

/// Transforms a tool schema for the Antigravity API (unsupported keywords dropped, types
/// flattened, constraints kept as description hints) and adds the `reason` placeholder property
/// that VALIDATED mode requires for empty object schemas.
pub fn clean_json_schema_for_antigravity(json_str: &str) -> String {
    clean_json_schema_for_antigravity_tool(json_str, true)
}

/// Antigravity function schema cleaning. The backend accepts enum members only as strings, but the
/// declared type still controls the JSON type of generated arguments, so numeric and boolean
/// types are not rewritten. `require_placeholder` is only for Claude VALIDATED mode.
pub fn clean_json_schema_for_antigravity_tool(json_str: &str, require_placeholder: bool) -> String {
    clean_json_schema(
        json_str,
        CleanOptions {
            add_placeholder: require_placeholder,
            add_missing_array_items: true,
            antigravity_semantics: true,
            remove_tool_title: !require_placeholder,
            flatten_unions: true,
            drop_all_enums: true,
            ..Default::default()
        },
    )
}

/// Antigravity response schema cleaning: no tool-only rewrites that would alter the client's
/// structured output contract. `additionalProperties: false` and string/number enums are kept;
/// unsupported constraints become description hints; `allOf` is merged; `anyOf`/`oneOf` select the
/// strongest branch (null branches become `nullable: true`); local `$ref`s are inlined.
pub fn clean_json_schema_for_antigravity_response(json_str: &str) -> String {
    clean_json_schema(
        json_str,
        CleanOptions {
            antigravity_semantics: true,
            flatten_unions: true,
            drop_boolean_enums: true,
            preserve_additional_properties_false: true,
            ..Default::default()
        },
    )
}

/// Transforms a JSON schema for Gemini tool calling: unsupported keywords removed, schema
/// simplified, no empty-schema placeholder.
pub fn clean_json_schema_for_gemini(json_str: &str) -> String {
    clean_json_schema(
        json_str,
        CleanOptions {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            ..Default::default()
        },
    )
}

/// Gemini cleaning for the `parametersJsonSchema` carrier: standard constraints (pattern,
/// minLength, maxLength, ...) and `additionalProperties` (boolean or schema) are preserved.
pub fn clean_json_schema_for_gemini_json_schema(json_str: &str) -> String {
    clean_json_schema(
        json_str,
        CleanOptions {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            flatten_unions: true,
            force_enum_string_type: true,
            preserve_all_additional_properties: true,
            preserve_standard_constraints: true,
            ..Default::default()
        },
    )
}

/// Stack size for documents nested deeply enough to endanger a 2 MiB worker stack.
const DEEP_STACK_BYTES: usize = 64 << 20;

/// Whether `text` nests more than 32 brackets (cheap scan that may over-count malformed text).
fn is_deeply_nested(text: &str) -> bool {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for &b in text.as_bytes() {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > 32 {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Runs `f` on a large temporary stack segment when `text` is deeply nested: the cleaners recurse
/// once per nesting level and Go's growable stacks have no such limit.
fn with_stack_for<R>(text: &str, f: impl FnOnce() -> R) -> R {
    if is_deeply_nested(text) {
        stacker::grow(DEEP_STACK_BYTES, f)
    } else {
        f()
    }
}

/// Cleaned schemas keyed by (options, input). Agents resend the same tool schemas on every
/// request and cleaning is a pure function of its input, so repeats are served from memory.
/// Bounded: a full cache is dropped wholesale. Values are `Arc<str>` and lookups take the read
/// lock, so concurrent hits hash and compare in parallel and copy the text after unlocking.
struct SchemaCache {
    by_options: HashMap<CleanOptions, HashMap<Arc<str>, Arc<str>>>,
    entries: usize,
    bytes: usize,
}

const SCHEMA_CACHE_MAX_ENTRIES: usize = 1024;
const SCHEMA_CACHE_MAX_BYTES: usize = 8 << 20;
/// Larger schemas are cleaned directly; they are rare and their copies would dominate the cache.
const SCHEMA_CACHE_MAX_INPUT: usize = 64 << 10;

static SCHEMA_CACHE: LazyLock<RwLock<SchemaCache>> =
    LazyLock::new(|| RwLock::new(SchemaCache { by_options: HashMap::new(), entries: 0, bytes: 0 }));

fn clean_json_schema(json_str: &str, options: CleanOptions) -> String {
    if json_str.len() > SCHEMA_CACHE_MAX_INPUT {
        return with_stack_for(json_str, || clean_json_schema_inner(json_str, options));
    }
    let hit = SCHEMA_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.by_options.get(&options).and_then(|m| m.get(json_str)).cloned());
    if let Some(hit) = hit {
        return hit.to_string();
    }
    let out = with_stack_for(json_str, || clean_json_schema_inner(json_str, options));
    let (key, value): (Arc<str>, Arc<str>) = (json_str.into(), out.as_str().into());
    if let Ok(mut cache) = SCHEMA_CACHE.write() {
        let cost = key.len() + value.len();
        if cache.entries >= SCHEMA_CACHE_MAX_ENTRIES || cache.bytes + cost > SCHEMA_CACHE_MAX_BYTES {
            cache.by_options.clear();
            cache.entries = 0;
            cache.bytes = 0;
        }
        if cache.by_options.entry(options).or_default().insert(key, value).is_none() {
            cache.entries += 1;
            cache.bytes += cost;
        }
    }
    out
}

fn clean_json_schema_inner(json_str: &str, options: CleanOptions) -> String {
    // Phase 0: normalize malformed schemas (bare property maps, boolean `required` from MCP tools).
    let mut text = normalize_malformed_schema_objects(json_str, options.add_missing_array_items);

    // Phase 1: convert and add hints.
    if options.antigravity_semantics {
        text = inline_local_refs_inner(&text);
    }
    if !cpa_json::valid(text.as_bytes()) {
        return text;
    }
    let mut doc = cpa_json::parse(text.as_bytes());
    let raw_source = RawSource::new(&text);
    convert_refs_to_hints(&mut doc, options.antigravity_semantics);
    let const_raws = convert_const_to_enum(&mut doc);
    convert_enum_values_to_strings(&mut doc, options.force_enum_string_type, &raw_source, &const_raws);
    add_enum_hints(&mut doc, &raw_source);
    drop_ignored_enums_to_hints(&mut doc, options, &raw_source);
    if !options.preserve_additional_properties_false && !options.preserve_all_additional_properties
    {
        add_additional_properties_hints(&mut doc);
    }
    move_constraints_to_description(&mut doc, options, &raw_source);
    if options.antigravity_semantics {
        move_not_to_description(&mut doc, &raw_source);
    }

    // Phase 2: flatten complex structures.
    merge_conditionals(&mut doc);
    merge_all_of(&mut doc);
    if options.flatten_unions {
        flatten_any_of_one_of(&mut doc);
    }
    flatten_type_arrays(&mut doc, options.antigravity_semantics);

    // Phase 3: cleanup.
    remove_unsupported_keywords(&mut doc, options);
    if options.remove_gemini_metadata {
        remove_keywords(&mut doc, &["nullable", "title"]);
        remove_placeholder_fields(&mut doc);
    } else if options.remove_tool_title {
        remove_keywords(&mut doc, &["title"]);
    }
    cleanup_required_fields(&mut doc);
    sanitize_array_items(&mut doc);

    // Phase 4: placeholder for empty object schemas (Claude VALIDATED mode).
    if options.add_placeholder {
        add_empty_schema_placeholder(&mut doc);
    }
    cpa_json::to_string(&doc)
}

// ---------------------------------------------------------------- phase 3 helpers

/// Ensures every node declaring `items` has `type: array` (Gemini's protobuf validator requires
/// it): a missing type is inferred as array, an explicit non-array type drops `items`.
fn sanitize_array_items(doc: &mut Value) {
    let mut paths = find_paths(doc, "items");
    sort_by_depth(&mut paths);
    for p in paths {
        let parent_path = trim_suffix(&p, ".items");
        if is_property_definition(&parent_path) {
            continue;
        }
        let type_path = join_path(&parent_path, "type");
        let t = doc.g(&type_path).str();
        if t.is_empty() {
            cpa_json::set(doc, &type_path, "array");
        } else if !t.eq_ignore_ascii_case("array") {
            cpa_json::delete(doc, &p);
        }
    }
}

/// Removes every occurrence of the given keywords (not directly under a name map).
fn remove_keywords(doc: &mut Value, keywords: &[&str]) {
    let mut delete_paths = Vec::new();
    let paths_by_field = find_paths_by_fields(doc, keywords);
    for key in keywords {
        for p in paths_by_field.get(*key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            delete_paths.push(p.clone());
        }
    }
    sort_by_depth(&mut delete_paths);
    for p in delete_paths {
        cpa_json::delete(doc, &p);
    }
}

/// Removes `required` entries equal to `name` from the array at `req_path`; deletes the array when
/// it becomes empty.
fn drop_required_name(doc: &mut Value, req_path: &str, name: &str) {
    let Some(Value::Array(items)) = doc.g(req_path).v().cloned() else {
        return;
    };
    // Go rebuilds the array from the entries' string forms.
    let filtered: Vec<Value> = items
        .iter()
        .map(val_str)
        .filter(|r| r != name)
        .map(Value::String)
        .collect();
    if filtered.is_empty() {
        cpa_json::delete(doc, req_path);
    } else {
        cpa_json::set(doc, req_path, Value::Array(filtered));
    }
}

fn val_str(v: &Value) -> String {
    cpa_json::Res::of(v).str()
}

/// Removes placeholder-only properties (`_` and the standard `reason`) and their required entries.
fn remove_placeholder_fields(doc: &mut Value) {
    let mut paths = find_paths(doc, "_");
    sort_by_depth(&mut paths);
    for p in paths {
        if !p.ends_with(".properties._") {
            continue;
        }
        cpa_json::delete(doc, &p);
        let parent_path = trim_suffix(&p, ".properties._");
        drop_required_name(doc, &join_path(&parent_path, "required"), "_");
    }

    let mut reason_paths = find_paths(doc, "reason");
    sort_by_depth(&mut reason_paths);
    for p in reason_paths {
        if !p.ends_with(".properties.reason") {
            continue;
        }
        let parent_path = trim_suffix(&p, ".properties.reason");
        let single_prop = matches!(doc.g(&join_path(&parent_path, "properties")).v(), Some(Value::Object(m)) if m.len() == 1);
        if !single_prop {
            continue;
        }
        if doc.g(&format!("{p}.description")).str() != PLACEHOLDER_REASON_DESCRIPTION {
            continue;
        }
        cpa_json::delete(doc, &p);
        drop_required_name(doc, &join_path(&parent_path, "required"), "reason");
    }
}

// ---------------------------------------------------------------- phase 0: normalization

/// Normalizes malformed schema nodes common in MCP tool definitions:
/// 1. bare property maps missing the `type: object` / `properties` wrappers are wrapped;
/// 2. boolean `required: true` on property definitions is stripped and promoted to the parent's
///    `required` array;
/// 3. tool array schemas missing `items` get a string item schema (when `add_missing_array_items`).
///
/// Returns the input untouched unless something was repaired; a repaired document is re-emitted
/// compactly with keys sorted at every level (Go marshals `map[string]any`).
fn normalize_malformed_schema_objects(json_str: &str, add_missing_array_items: bool) -> String {
    if json_str.is_empty() {
        return json_str.to_string();
    }
    // Go decodes the first JSON value and ignores trailing data.
    let Some(root) = parse_first_value(json_str) else {
        return json_str.to_string();
    };
    if root == Value::Bool(true) {
        return "{}".into();
    }
    let Value::Object(root_map) = root else {
        return json_str.to_string();
    };
    if is_api_request_document(&root_map) {
        return json_str.to_string();
    }
    let emit = |v: Value| {
        go_json_sorted(&v, GoJsonStyle::NO_HTML_ESCAPE).unwrap_or_else(|| json_str.to_string())
    };

    // Wrapped in a single-key {"schema": ...} by a caller: repair the inner schema and re-wrap.
    if root_map.len() == 1 {
        match root_map.get("schema") {
            Some(Value::Object(inner)) => {
                let (repaired, modified) = repair_schema_node(inner, add_missing_array_items);
                if !modified {
                    return json_str.to_string();
                }
                let mut wrapper = Map::new();
                wrapper.insert("schema".into(), Value::Object(repaired));
                return emit(Value::Object(wrapper));
            }
            Some(Value::Bool(true)) => {
                let mut wrapper = Map::new();
                wrapper.insert("schema".into(), Value::Object(Map::new()));
                return emit(Value::Object(wrapper));
            }
            _ => {}
        }
    }

    let (repaired, modified) = repair_schema_node(&root_map, add_missing_array_items);
    if !modified {
        return json_str.to_string();
    }
    emit(Value::Object(repaired))
}

fn is_known_schema_keyword_or_extension(key: &str) -> bool {
    key.starts_with("x-")
        || matches!(
            key,
            "properties"
                | "patternProperties"
                | "additionalProperties"
                | "items"
                | "prefixItems"
                | "$defs"
                | "definitions"
                | "dependentSchemas"
                | "dependentRequired"
                | "dependencies"
                | "if"
                | "then"
                | "else"
                | "not"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "contentSchema"
                | "additionalItems"
                | "default"
                | "const"
                | "example"
                | "examples"
                | "discriminator"
                | "xml"
                | "externalDocs"
                | "enumDescriptions"
                | "enumTitles"
        )
}

fn is_non_object_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(Value::Array(arr)) => {
            let has_object = arr
                .iter()
                .any(|item| matches!(item, Value::String(s) if s.eq_ignore_ascii_case("object")));
            !has_object && !arr.is_empty()
        }
        _ => false,
    }
}

fn is_array_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => s.eq_ignore_ascii_case("array"),
        Some(Value::Array(arr)) => arr
            .iter()
            .any(|item| matches!(item, Value::String(s) if s.eq_ignore_ascii_case("array"))),
        _ => false,
    }
}

/// Whether a map is a whole API request (tools/contents/messages/...) rather than a schema.
fn is_api_request_document(m: &Map<String, Value>) -> bool {
    let is_array = |k: &str| matches!(m.get(k), Some(Value::Array(_)));
    if [
        "tools",
        "contents",
        "messages",
        "functionDeclarations",
        "function_declarations",
    ]
    .iter()
    .any(|k| is_array(k))
    {
        return true;
    }
    matches!(m.get("request"), Some(Value::Object(req)) if is_api_request_document(req))
}

/// Go's `x == nil || x == ""` on a map entry: absent, null or empty string.
fn is_nil_or_empty_string(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null)) || matches!(v, Some(Value::String(s)) if s.is_empty())
}

/// Returns a repaired copy of a schema node and whether anything changed.
fn repair_schema_node(
    node: &Map<String, Value>,
    add_missing_array_items: bool,
) -> (Map<String, Value>, bool) {
    let mut modified = false;
    let mut clone = node.clone();

    // 1. Not declared as a primitive/array type: collect bare property definition maps.
    if !is_non_object_declared_type(clone.get("type")) {
        let bare_props: Map<String, Value> = clone
            .iter()
            .filter(|(k, v)| {
                matches!(v, Value::Object(_)) && !is_known_schema_keyword_or_extension(k)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if !bare_props.is_empty() {
            let (repaired_props, promoted_reqs, _) =
                repair_property_map(&bare_props, add_missing_array_items);
            for k in bare_props.keys() {
                clone.shift_remove(k);
            }
            if let Some(Value::Object(existing)) = clone.get("properties") {
                let mut new_props = existing.clone();
                for (k, v) in repaired_props {
                    new_props.insert(k, v);
                }
                clone.insert("properties".into(), Value::Object(new_props));
            } else {
                clone.insert("properties".into(), Value::Object(repaired_props));
                if !clone.contains_key("type") {
                    clone.insert("type".into(), Value::String("object".into()));
                }
            }
            if !promoted_reqs.is_empty() {
                promote_required(&mut clone, &promoted_reqs);
            }
            modified = true;
        }
    }

    // 2. Recurse into `properties`.
    if let Some(Value::Object(props)) = clone.get("properties") {
        let (repaired_props, promoted_reqs, props_mod) =
            repair_property_map(props, add_missing_array_items);
        if props_mod {
            clone.insert("properties".into(), Value::Object(repaired_props));
            modified = true;
        }
        if !promoted_reqs.is_empty() {
            promote_required(&mut clone, &promoted_reqs);
            modified = true;
        }
    }

    // Gemini and Antigravity reject tool array schemas without `items`, and `items` on non-arrays.
    if add_missing_array_items {
        if is_array_declared_type(clone.get("type")) {
            if !clone.contains_key("items") {
                let mut item_schema = Map::new();
                item_schema.insert("type".into(), Value::String("string".into()));
                clone.insert("items".into(), Value::Object(item_schema));
                modified = true;
            }
        } else if clone.contains_key("items") && is_nil_or_empty_string(clone.get("type")) {
            clone.insert("type".into(), Value::String("array".into()));
            modified = true;
        }
    }

    // 3. Recurse into the other standard schema containers.
    match clone.get("items") {
        Some(Value::Object(items)) => {
            let (repaired, m) = repair_schema_node(items, add_missing_array_items);
            if m {
                clone.insert("items".into(), Value::Object(repaired));
                modified = true;
            }
        }
        Some(Value::Array(list)) => {
            if let Some(repaired) = repair_schema_list(list, add_missing_array_items) {
                clone.insert("items".into(), Value::Array(repaired));
                modified = true;
            }
        }
        Some(Value::Bool(true)) => {
            clone.insert("items".into(), Value::Object(Map::new()));
            modified = true;
        }
        _ => {}
    }

    if let Some(Value::Object(add_props)) = clone.get("additionalProperties") {
        let (repaired, m) = repair_schema_node(add_props, add_missing_array_items);
        if m {
            clone.insert("additionalProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    if let Some(Value::Object(pat_props)) = clone.get("patternProperties") {
        let (repaired, _, m) = repair_property_map(pat_props, add_missing_array_items);
        if m {
            clone.insert("patternProperties".into(), Value::Object(repaired));
            modified = true;
        }
    }

    for key in [
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        match clone.get(key) {
            Some(Value::Object(sub)) => {
                let (repaired, m) = repair_schema_node(sub, add_missing_array_items);
                if m {
                    clone.insert(key.into(), Value::Object(repaired));
                    modified = true;
                }
            }
            Some(Value::Bool(true)) => {
                clone.insert(key.into(), Value::Object(Map::new()));
                modified = true;
            }
            _ => {}
        }
    }

    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list)) = clone.get(key)
            && let Some(repaired) = repair_schema_list(list, add_missing_array_items)
        {
            clone.insert(key.into(), Value::Array(repaired));
            modified = true;
        }
    }

    for key in ["$defs", "definitions", "dependentSchemas", "dependencies"] {
        let Some(Value::Object(defs)) = clone.get(key) else {
            continue;
        };
        let mut repaired_defs = Map::new();
        let mut defs_modified = false;
        for (dk, dv) in defs {
            match dv {
                Value::Object(def_map) => {
                    let (repaired, m) = repair_schema_node(def_map, add_missing_array_items);
                    repaired_defs.insert(dk.clone(), Value::Object(repaired));
                    defs_modified |= m;
                }
                Value::Bool(true) => {
                    repaired_defs.insert(dk.clone(), Value::Object(Map::new()));
                    defs_modified = true;
                }
                other => {
                    repaired_defs.insert(dk.clone(), other.clone());
                }
            }
        }
        if defs_modified {
            clone.insert(key.into(), Value::Object(repaired_defs));
            modified = true;
        }
    }

    (clone, modified)
}

/// Repairs a list of subschemas; `None` when nothing changed.
fn repair_schema_list(list: &[Value], add_missing_array_items: bool) -> Option<Vec<Value>> {
    let mut repaired = Vec::with_capacity(list.len());
    let mut modified = false;
    for item in list {
        match item {
            Value::Object(m) => {
                let (r, item_mod) = repair_schema_node(m, add_missing_array_items);
                repaired.push(Value::Object(r));
                modified |= item_mod;
            }
            Value::Bool(true) => {
                repaired.push(Value::Object(Map::new()));
                modified = true;
            }
            other => repaired.push(other.clone()),
        }
    }
    modified.then_some(repaired)
}

/// Repairs a map of named subschemas. Returns the repaired map, the names promoted from boolean
/// `required: true` (sorted) and whether anything changed.
fn repair_property_map(
    props: &Map<String, Value>,
    add_missing_array_items: bool,
) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted = Vec::new();
    let mut modified = false;
    for (k, v) in props {
        let Value::Object(child_map) = v else {
            if *v == Value::Bool(true) {
                out.insert(k.clone(), Value::Object(Map::new()));
                modified = true;
            } else {
                out.insert(k.clone(), v.clone());
            }
            continue;
        };
        let mut child_clone = child_map.clone();
        if let Some(Value::Bool(req)) = child_clone.get("required").cloned() {
            child_clone.shift_remove("required");
            modified = true;
            if req {
                promoted.push(k.clone());
            }
        }
        let (repaired_child, child_mod) = repair_schema_node(&child_clone, add_missing_array_items);
        modified |= child_mod;
        out.insert(k.clone(), Value::Object(repaired_child));
    }
    promoted.sort();
    (out, promoted, modified)
}

/// Merges promoted property names into `clone["required"]` (existing string entries first,
/// deduplicated, empty names dropped; an empty result is stored as `null` like Go's nil slice).
fn promote_required(clone: &mut Map<String, Value>, promoted: &[String]) {
    let existing: Vec<String> = match clone.get("required") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let mut merged: Vec<String> = Vec::new();
    for s in existing.into_iter().chain(promoted.iter().cloned()) {
        if !s.is_empty() && !merged.contains(&s) {
            merged.push(s);
        }
    }
    clone.insert("required".into(), strings_value(&merged));
}

// ---------------------------------------------------------------- $ref handling

/// Resolves local `#/...` JSON Pointer references against the original schema before definition
/// containers are stripped. Each expansion gets its own copy, sibling keywords override the
/// referenced definition, and cycles end as a typed `See: <name>` hint instead of recursing. The
/// document is re-emitted compactly with sorted keys; documents without a `"$ref"` key (or that are
/// not valid JSON) are returned unchanged.
pub fn inline_local_refs(json_str: &str) -> String {
    with_stack_for(json_str, || inline_local_refs_inner(json_str))
}

fn inline_local_refs_inner(json_str: &str) -> String {
    if !json_str.contains("\"$ref\"") {
        return json_str.to_string();
    }
    let Some(root) = parse_first_value(json_str) else {
        return json_str.to_string();
    };
    let resolved = resolve_local_refs(&root, &root, &mut HashMap::new());
    go_json_sorted(&resolved, GoJsonStyle::MARSHAL_USE_NUMBER)
        .unwrap_or_else(|| json_str.to_string())
}

fn resolve_local_refs(root: &Value, value: &Value, active: &mut HashMap<String, bool>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_local_refs(root, item, active))
                .collect(),
        ),
        Value::Object(node) => {
            if let Some(Value::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = resolve_json_pointer(root, reference)
            {
                if active.get(reference).copied().unwrap_or(false) {
                    return Value::Object(cyclic_ref_fallback(node, target, reference));
                }
                active.insert(reference.clone(), true);
                let resolved_target = resolve_local_refs(root, target, active);
                active.remove(reference);
                if let Value::Object(mut out) = resolved_target {
                    for (key, item) in node {
                        if key == "$ref" {
                            continue;
                        }
                        out.insert(key.clone(), resolve_local_refs(root, item, active));
                    }
                    return Value::Object(out);
                }
            }
            Value::Object(
                node.iter()
                    .map(|(k, v)| (k.clone(), resolve_local_refs(root, v, active)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

fn resolve_json_pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let mut current = root;
    for raw_part in reference.strip_prefix("#/").unwrap_or(reference).split('/') {
        let part = raw_part.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(m) => m.get(&part)?,
            Value::Array(a) => {
                let index: i64 = part.parse().ok()?;
                a.get(usize::try_from(index).ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn cyclic_ref_fallback(
    node: &Map<String, Value>,
    target: &Value,
    reference: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    if let Value::Object(target_map) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = target_map.get(key) {
                out.insert(key.into(), v.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let hint = format!("See: {}", ref_name(reference));
    let description = match out.get("description") {
        Some(Value::String(d)) if !d.is_empty() => merge_hint(d, &hint),
        _ => hint,
    };
    out.insert("description".into(), Value::String(description));
    out
}

fn ref_name(reference: &str) -> String {
    match reference.rfind('/') {
        Some(index) if index + 1 < reference.len() => {
            reference[index + 1..].replace("~1", "/").replace("~0", "~")
        }
        _ => reference.to_string(),
    }
}

/// Keeps sibling keywords and converts only unresolved or external references to description
/// hints (local references were already expanded by [`inline_local_refs`] when applicable).
fn convert_refs_to_hints(doc: &mut Value, preserve_siblings: bool) {
    let mut paths = find_paths(doc, "$ref");
    sort_by_depth(&mut paths);
    for p in paths {
        let ref_val = doc.g(&p).str();
        let def_name = ref_name(&ref_val);
        let parent_path = trim_suffix(&p, ".$ref");
        let mut hint = format!("See: {def_name}");
        if !preserve_siblings {
            let existing = doc.g(&description_path(&parent_path)).str();
            if !existing.is_empty() {
                hint = format!("{existing} ({hint})");
            }
            let mut replacement = Map::new();
            replacement.insert("type".into(), Value::String("object".into()));
            replacement.insert("description".into(), Value::String(hint));
            set_value_at(doc, &parent_path, Value::Object(replacement));
            continue;
        }
        cpa_json::delete(doc, &p);
        append_hint(doc, &parent_path, &hint);
    }
}

// ---------------------------------------------------------------- phase 1: conversions and hints

/// Adds `enum: [const]` next to `const` when no enum exists (the value goes through Go's float64
/// decoding, so numbers are re-formatted and object keys sorted). Returns the Go raw text
/// (`json.Marshal` output, HTML-escaped) of every object/array element it created, keyed by the
/// element path, because the stringified enum later exposes that raw text.
fn convert_const_to_enum(doc: &mut Value) -> HashMap<String, String> {
    let mut raws = HashMap::new();
    for p in find_paths(doc, "const") {
        let Some(val) = doc.g(&p).v().cloned() else {
            continue;
        };
        let enum_path = format!("{}.enum", trim_suffix(&p, ".const"));
        if doc.g(&enum_path).exists() {
            continue;
        }
        let Some(canonical) = go_json_canonicalize(&val.to_string()) else {
            continue;
        };
        // serde_json drops the sign of a bare `-0`; keep it observable (gjson's String() gives "-0").
        let canonical = if canonical == "-0" {
            "-0.0".to_string()
        } else {
            canonical
        };
        if matches!(val, Value::Object(_) | Value::Array(_)) {
            raws.insert(format!("{enum_path}.0"), canonical.clone());
        }
        cpa_json::set(
            doc,
            &enum_path,
            Value::Array(vec![cpa_json::parse_str(&canonical)]),
        );
    }
    raws
}

/// Rewrites every enum array to strings (Gemini's proto schema requires it). With
/// `force_string_type` the sibling `type` becomes `string`; Antigravity keeps the declared type.
/// `const_raws` supplies the Go raw text for elements created by `convert_const_to_enum`.
fn convert_enum_values_to_strings(
    doc: &mut Value,
    force_string_type: bool,
    source: &RawSource,
    const_raws: &HashMap<String, String>,
) {
    for p in find_paths(doc, "enum") {
        let Some(Value::Array(items)) = doc.g(&p).v().cloned() else {
            continue;
        };
        let string_vals: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, item)| match const_raws.get(&format!("{p}.{i}")) {
                Some(raw) => raw.clone(),
                None => element_string(source, &p, i, item),
            })
            .collect();
        cpa_json::set(doc, &p, strings_value(&string_vals));
        if force_string_type {
            let parent_path = trim_suffix(&p, ".enum");
            cpa_json::set(doc, &join_path(&parent_path, "type"), "string");
        }
    }
}

/// Appends `Allowed: a, b, c` to the description of nodes with 2..=10 enum members.
fn add_enum_hints(doc: &mut Value, source: &RawSource) {
    for p in find_paths(doc, "enum") {
        let Some(Value::Array(items)) = doc.g(&p).v().cloned() else {
            continue;
        };
        if items.len() <= 1 || items.len() > 10 {
            continue;
        }
        let vals: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, item)| element_string(source, &p, i, item))
            .collect();
        append_hint(
            doc,
            &trim_suffix(&p, ".enum"),
            &format!("Allowed: {}", vals.join(", ")),
        );
    }
}

/// Antigravity does not enforce enum on function arguments and ignores boolean response enums:
/// keep a single value as a hint and drop the unenforced constraint.
fn drop_ignored_enums_to_hints(doc: &mut Value, options: CleanOptions, source: &RawSource) {
    for path in find_paths(doc, "enum") {
        let parent_path = trim_suffix(&path, ".enum");
        let should_drop = options.drop_all_enums
            || (options.drop_boolean_enums
                && doc.g(&join_path(&parent_path, "type")).str() == "boolean");
        if !should_drop {
            continue;
        }
        if let Some(Value::Array(items)) = doc.g(&path).v().cloned()
            && items.len() == 1
        {
            append_hint(
                doc,
                &parent_path,
                &format!("Allowed: {}", element_string(source, &path, 0, &items[0])),
            );
        }
        cpa_json::delete(doc, &path);
    }
}

fn add_additional_properties_hints(doc: &mut Value) {
    for p in find_paths(doc, "additionalProperties") {
        if matches!(doc.g(&p).v(), Some(Value::Bool(false))) {
            append_hint(
                doc,
                &trim_suffix(&p, ".additionalProperties"),
                "No extra properties allowed",
            );
        }
    }
}

const UNSUPPORTED_CONSTRAINTS: [&str; 12] = [
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "format",
    "default", // Claude rejects these in VALIDATED mode
    "examples",
];

fn constraint_keywords(options: CleanOptions) -> Vec<&'static str> {
    if options.preserve_standard_constraints {
        return Vec::new();
    }
    let mut keywords = UNSUPPORTED_CONSTRAINTS.to_vec();
    if options.antigravity_semantics {
        keywords.extend(["minimum", "maximum", "multipleOf"]);
    }
    keywords
}

/// Records unsupported constraints as `key: value` description hints (they are removed later by
/// [`remove_unsupported_keywords`]). Object and array values are quoted as written in `source`
/// (gjson's `Raw`), falling back to compact JSON when the node changed since.
fn move_constraints_to_description(doc: &mut Value, options: CleanOptions, source: &RawSource) {
    let constraints = constraint_keywords(options);
    if constraints.is_empty() {
        return;
    }
    let paths_by_field = find_paths_by_fields(doc, &constraints);
    for key in &constraints {
        for p in paths_by_field.get(*key).into_iter().flatten() {
            let Some(val) = doc.g(p).v().cloned() else {
                continue;
            };
            let parent_path = trim_suffix(p, &format!(".{key}"));
            if is_property_definition(&parent_path) {
                continue;
            }
            let text = match &val {
                Value::Object(_) | Value::Array(_) => raw_json_at(source, p, &val),
                other => val_str(other),
            };
            append_hint(doc, &parent_path, &format!("{key}: {text}"));
        }
    }
}

fn move_not_to_description(doc: &mut Value, source: &RawSource) {
    for path in find_paths(doc, "not") {
        let Some(value) = doc.g(&path).v().cloned() else {
            continue;
        };
        let parent_path = trim_suffix(&path, ".not");
        if is_property_definition(&parent_path) {
            continue;
        }
        append_hint(
            doc,
            &parent_path,
            &format!("not: {}", raw_json_at(source, &path, &value)),
        );
    }
}

/// gjson `String()` of array element `index` of the array at `array_path`: scalars as text,
/// objects and arrays as their raw JSON in `source`.
fn element_string(source: &RawSource, array_path: &str, index: usize, item: &Value) -> String {
    match item {
        Value::Object(_) | Value::Array(_) => {
            raw_json_at(source, &format!("{array_path}.{index}"), item)
        }
        other => val_str(other),
    }
}

/// The raw JSON text of the node at `path` in `source` (original whitespace and escapes), as gjson's
/// `Result.Raw` would give. Falls back to the compact serialization of `current` when the path does
/// not resolve or the node differs from `current` (it was edited by an earlier phase).
fn raw_json_at(source: &RawSource, path: &str, current: &Value) -> String {
    source
        .get(path)
        .filter(|raw| serde_json::from_str::<Value>(raw).is_ok_and(|parsed| parsed == *current))
        .map(str::to_string)
        .unwrap_or_else(|| current.to_string())
}

/// The schema text as parsed, indexed once (lazily, on the first hint that needs raw text) from
/// gjson-style path to the raw text of every object/array node. Nodes deeper than serde's raw
/// value recursion limit are not indexed and fall back to compact JSON.
struct RawSource<'a> {
    text: &'a str,
    index: std::cell::OnceCell<HashMap<String, &'a str>>,
}

impl<'a> RawSource<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            index: std::cell::OnceCell::new(),
        }
    }

    fn get(&self, path: &str) -> Option<&'a str> {
        let index = self.index.get_or_init(|| {
            let mut map = HashMap::new();
            index_raw_children(self.text, "", &mut map);
            map
        });
        index.get(path).copied()
    }
}

/// Records the raw text of every object/array descendant of `raw` under its path.
fn index_raw_children<'a>(raw: &'a str, path: &str, map: &mut HashMap<String, &'a str>) {
    use serde_json::value::RawValue;
    let children: Vec<(String, &'a RawValue)> = match raw.trim_start().as_bytes().first() {
        Some(b'{') => {
            match serde_json::from_str::<HashMap<std::borrow::Cow<'a, str>, &'a RawValue>>(raw) {
                Ok(m) => m
                    .into_iter()
                    .map(|(k, v)| (escape_gjson_path_key(&k), v))
                    .collect(),
                Err(_) => return,
            }
        }
        Some(b'[') => match serde_json::from_str::<Vec<&'a RawValue>>(raw) {
            Ok(v) => v
                .into_iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v))
                .collect(),
            Err(_) => return,
        },
        _ => return,
    };
    for (key, child) in children {
        let text = child.get();
        if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
            let child_path = join_path(path, &key);
            map.insert(child_path.clone(), text);
            index_raw_children(text, &child_path, map);
        }
    }
}

// ---------------------------------------------------------------- phase 2: flattening

/// Copies properties of `then`/`else` branches into the parent when absent there.
fn merge_conditionals(doc: &mut Value) {
    let paths_by_field = find_paths_by_fields(doc, &["then", "else"]);
    let mut paths = Vec::new();
    for key in ["then", "else"] {
        for p in paths_by_field.get(key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            paths.push(p.clone());
        }
    }
    sort_by_depth(&mut paths);

    for p in paths {
        let Some(Value::Object(props)) = doc.g(&join_path(&p, "properties")).v().cloned() else {
            continue;
        };
        let parent_path = if p.ends_with(".then") {
            trim_suffix(&p, ".then")
        } else if p.ends_with(".else") {
            trim_suffix(&p, ".else")
        } else if p == "then" || p == "else" {
            String::new()
        } else {
            continue;
        };
        for (key, value) in props {
            let dest_path = join_path(
                &parent_path,
                &format!("properties.{}", escape_gjson_path_key(&key)),
            );
            if !doc.g(&dest_path).exists() {
                cpa_json::set(doc, &dest_path, value);
            }
        }
    }
}

/// Merges `allOf` branches into the parent (never replacing parent definitions) and removes the
/// keyword. `required` lists are unioned; conditional keywords are dropped.
fn merge_all_of(doc: &mut Value) {
    let mut paths = find_paths(doc, "allOf");
    sort_by_depth(&mut paths);
    for p in paths {
        let Some(Value::Array(all_of)) = doc.g(&p).v().cloned() else {
            continue;
        };
        let parent_path = trim_suffix(&p, ".allOf");
        for item in &all_of {
            let Value::Object(fields) = item else {
                continue;
            };
            for (field, value) in fields {
                match field.as_str() {
                    "required" => {
                        let Value::Array(required) = value else {
                            continue;
                        };
                        let req_path = join_path(&parent_path, "required");
                        let mut current = get_strings(doc, &req_path);
                        for r in required {
                            let name = val_str(r);
                            if !current.contains(&name) {
                                current.push(name);
                            }
                        }
                        cpa_json::set(doc, &req_path, strings_value(&current));
                    }
                    // Conditional applicability cannot be represented by the upstream schema.
                    "if" | "then" | "else" | "allOf" => {}
                    _ => {
                        let destination = join_path(&parent_path, &escape_gjson_path_key(field));
                        merge_missing_schema_at_path(doc, &destination, value);
                    }
                }
            }
        }
        cpa_json::delete(doc, &p);
    }
}

/// Recursively fills absent fields without replacing any existing definition: the parent schema is
/// canonical, merged branches may only enrich gaps in it.
fn merge_missing_schema_at_path(doc: &mut Value, destination: &str, incoming: &Value) {
    let existing_is_object = match doc.g(destination).v() {
        None => {
            cpa_json::set(doc, destination, incoming.clone());
            return;
        }
        Some(existing) => existing.is_object(),
    };
    let Value::Object(incoming_map) = incoming else {
        return;
    };
    if !existing_is_object {
        return;
    }
    for (key, value) in incoming_map {
        let child = join_path(destination, &escape_gjson_path_key(key));
        merge_missing_schema_at_path(doc, &child, value);
    }
}

/// Collapses `anyOf`/`oneOf`: with sibling `properties` the branch properties are merged into the
/// parent; otherwise the strongest branch (object > array > scalar > null) replaces the parent,
/// keeping the parent description, marking `nullable` for null branches and hinting the accepted
/// types.
fn flatten_any_of_one_of(doc: &mut Value) {
    for key in ["anyOf", "oneOf"] {
        let mut paths = find_paths(doc, key);
        sort_by_depth(&mut paths);
        for p in paths {
            let Some(Value::Array(items)) = doc.g(&p).v().cloned() else {
                continue;
            };
            if items.is_empty() {
                continue;
            }
            let parent_path = trim_suffix(&p, &format!(".{key}"));
            let parent_has_props = parent_has_properties(doc, &parent_path);

            let item_type_is_null = |item: &Value| item.g("type").str() == "null";

            // The parent already defines properties: merge branch properties into it instead of
            // replacing it with a single branch.
            if parent_has_props {
                let mut has_null = false;
                for item in &items {
                    if item_type_is_null(item) {
                        has_null = true;
                    }
                    if let Some(Value::Object(branch_props)) = item.g("properties").v() {
                        for (prop_key, prop_val) in branch_props {
                            let dest_path = join_path(
                                &parent_path,
                                &format!("properties.{}", escape_gjson_path_key(prop_key)),
                            );
                            merge_missing_schema_at_path(doc, &dest_path, prop_val);
                        }
                    }
                }
                if has_null {
                    cpa_json::set(doc, &join_path(&parent_path, "nullable"), true);
                }
                cpa_json::delete(doc, &p);
                continue;
            }

            let parent_desc = doc.g(&description_path(&parent_path)).str();
            let (best_idx, all_types) = select_best(&items);
            let mut selected = items[best_idx].clone();
            let has_null = items.iter().any(item_type_is_null);
            if has_null && !item_type_is_null(&items[best_idx]) {
                cpa_json::set(&mut selected, "nullable", true);
            }
            if !parent_desc.is_empty() {
                merge_description_raw(&mut selected, &parent_desc);
            }
            if all_types.len() > 1 {
                append_hint_raw(
                    &mut selected,
                    &format!("Accepts: {}", all_types.join(" | ")),
                );
            }
            set_value_at(doc, &parent_path, selected);
        }
    }
}

/// Whether the node at `parent_path` (the whole document for "") has an object `properties`.
fn parent_has_properties(doc: &Value, parent_path: &str) -> bool {
    let properties = doc.g(&join_path(parent_path, "properties"));
    matches!(properties.v(), Some(Value::Object(_)))
}

/// Index of the strongest union branch and the (non-empty) type names of all branches in order.
fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let mut best_idx = 0;
    let mut best_score: i32 = -1;
    let mut types = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut t = item.g("type").str();
        let score;
        if t == "object" || item.g("properties").exists() {
            score = 3;
            if t.is_empty() {
                t = "object".into();
            }
        } else if t == "array" || item.g("items").exists() {
            score = 2;
            if t.is_empty() {
                t = "array".into();
            }
        } else if !t.is_empty() && t != "null" {
            score = 1;
        } else if t == "null" {
            score = 0;
        } else {
            score = 0;
            t = String::new();
        }
        if !t.is_empty() {
            types.push(t);
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    (best_idx, types)
}

/// Flattens `type: [..]` arrays to one type: `array` when `items` exists and is listed, else the
/// first non-null type (`string` when none). Extra types become an `Accepts:` hint; `null` becomes
/// native `nullable: true` (Antigravity) or drops the property from its parent's `required`.
fn flatten_type_arrays(doc: &mut Value, preserve_native_nullable: bool) {
    let mut paths = find_paths(doc, "type");
    sort_by_depth(&mut paths);

    let mut nullable_fields: Vec<(String, Vec<String>)> = Vec::new();

    for p in paths {
        let Some(Value::Array(items)) = doc.g(&p).v().cloned() else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        let mut has_null = false;
        let mut non_null_types: Vec<String> = Vec::new();
        for item in &items {
            let s = val_str(item);
            if s == "null" {
                has_null = true;
            } else if !s.is_empty() {
                non_null_types.push(s);
            }
        }

        let parent_path = trim_suffix(&p, ".type");
        let items_path = join_path(&parent_path, "items");

        let mut first_type = "string".to_string();
        if !non_null_types.is_empty() {
            first_type =
                if doc.g(&items_path).exists() && non_null_types.iter().any(|t| t == "array") {
                    "array".into()
                } else {
                    non_null_types[0].clone()
                };
        }

        cpa_json::set(doc, &p, first_type.as_str());

        if first_type != "array" && doc.g(&items_path).exists() {
            cpa_json::delete(doc, &items_path);
        }
        if non_null_types.len() > 1 {
            append_hint(
                doc,
                &parent_path,
                &format!("Accepts: {}", non_null_types.join(" | ")),
            );
        }

        if has_null {
            if preserve_native_nullable {
                cpa_json::set(doc, &join_path(&parent_path, "nullable"), true);
                append_hint(doc, &parent_path, "(nullable)");
                continue;
            }
            let parts = split_gjson_path(&p);
            if parts.len() >= 3 && parts[parts.len() - 3] == "properties" {
                let field_name_escaped = &parts[parts.len() - 2];
                let field_name = unescape_gjson_path_key(field_name_escaped);
                let object_path = parts[..parts.len() - 3].join(".");
                match nullable_fields
                    .iter_mut()
                    .find(|(path, _)| *path == object_path)
                {
                    Some((_, fields)) => fields.push(field_name),
                    None => nullable_fields.push((object_path.clone(), vec![field_name])),
                }
                append_hint(
                    doc,
                    &join_path(&object_path, &format!("properties.{field_name_escaped}")),
                    "(nullable)",
                );
            }
        }
    }

    for (object_path, fields) in nullable_fields {
        let req_path = join_path(&object_path, "required");
        let Some(Value::Array(required)) = doc.g(&req_path).v().cloned() else {
            continue;
        };
        let filtered: Vec<Value> = required
            .iter()
            .map(val_str)
            .filter(|r| !fields.contains(r))
            .map(Value::String)
            .collect();
        if filtered.is_empty() {
            cpa_json::delete(doc, &req_path);
        } else {
            cpa_json::set(doc, &req_path, Value::Array(filtered));
        }
    }
}

// ---------------------------------------------------------------- phase 3: cleanup

/// Deletes keywords the upstream rejects (constraints, `$ref`/`$defs`, conditionals, vendor
/// metadata, ...) outside name maps, then every `x-*` extension field.
fn remove_unsupported_keywords(doc: &mut Value, options: CleanOptions) {
    let mut keywords = constraint_keywords(options);
    keywords.extend([
        "$schema",
        "$defs",
        "definitions",
        "const",
        "$ref",
        "$id",
        "id",
        "additionalProperties",
        "$anchor",
        "$vocabulary",
        "$dynamicRef",
        "$dynamicAnchor",
        "propertyNames",
        "patternProperties", // Gemini doesn't support these schema keywords
        "if",
        "then",
        "else",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted", // schema metadata fields unsupported by Gemini
        "additionalItems",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
    ]);
    if options.antigravity_semantics {
        keywords.push("not");
    }

    let mut delete_paths = Vec::new();
    let paths_by_field = find_paths_by_fields(doc, &keywords);
    for key in &keywords {
        for p in paths_by_field.get(*key).into_iter().flatten() {
            if is_property_definition(&trim_suffix(p, &format!(".{key}"))) {
                continue;
            }
            if *key == "additionalProperties" {
                if options.preserve_all_additional_properties {
                    continue;
                }
                if options.preserve_additional_properties_false
                    && matches!(doc.g(p).v(), Some(Value::Bool(false)))
                {
                    continue;
                }
            }
            delete_paths.push(p.clone());
        }
    }
    sort_by_depth(&mut delete_paths);
    for p in delete_paths {
        cpa_json::delete(doc, &p);
    }
    remove_extension_fields(doc);
}

/// Removes all `x-*` extension fields (OpenAPI/JSON Schema extensions Google APIs reject).
fn remove_extension_fields(doc: &mut Value) {
    let mut paths = Vec::new();
    walk_for_extensions(doc, "", &mut paths);
    for p in paths {
        cpa_json::delete(doc, &p);
    }
}

/// Collects delete paths of `x-*` keys; children of collected keys are skipped and arrays are
/// walked back to front so earlier indexes stay valid while deleting.
fn walk_for_extensions(value: &Value, path: &str, paths: &mut Vec<String>) {
    match value {
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate().rev() {
                walk_for_extensions(item, &join_path(path, &i.to_string()), paths);
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                let child_path = join_path(path, &escape_gjson_path_key(key));
                if key.starts_with("x-") && !is_property_definition(path) {
                    paths.push(child_path);
                    continue;
                }
                walk_for_extensions(val, &child_path, paths);
            }
        }
        _ => {}
    }
}

/// Drops `required` entries that name no existing property (and `required` without `properties`).
fn cleanup_required_fields(doc: &mut Value) {
    for p in find_paths(doc, "required") {
        let parent_path = trim_suffix(&p, ".required");
        let props_path = join_path(&parent_path, "properties");

        let Some(Value::Array(req)) = doc.g(&p).v().cloned() else {
            continue;
        };
        let Some(Value::Object(props)) = doc.g(&props_path).v().cloned() else {
            cpa_json::delete(doc, &p);
            continue;
        };
        let props = Value::Object(props);
        let valid: Vec<Value> = req
            .iter()
            .filter(|r| cpa_json::get(&props, &escape_gjson_path_key(&val_str(r))).exists())
            .map(|r| Value::String(val_str(r)))
            .collect();
        if valid.len() != req.len() {
            if valid.is_empty() {
                cpa_json::delete(doc, &p);
            } else {
                cpa_json::set(doc, &p, Value::Array(valid));
            }
        }
    }
}

// ---------------------------------------------------------------- phase 4: placeholders

/// Adds a required `reason` placeholder property to empty object schemas, and a minimal required
/// `_` boolean to nested object schemas that have properties but none required (Claude VALIDATED
/// mode requires at least one required property).
fn add_empty_schema_placeholder(doc: &mut Value) {
    let mut paths = find_paths(doc, "type");
    // Deepest first so nested objects are handled before their parents.
    sort_by_depth(&mut paths);

    for p in paths {
        if doc.g(&p).str() != "object" {
            continue;
        }
        let parent_path = trim_suffix(&p, ".type");
        let props_path = join_path(&parent_path, "properties");
        let req_path = join_path(&parent_path, "required");

        let props_val = doc.g(&props_path).v().cloned();
        let has_required_properties =
            matches!(doc.g(&req_path).v(), Some(Value::Array(a)) if !a.is_empty());

        let needs_placeholder = match &props_val {
            None => true,
            Some(Value::Object(m)) => m.is_empty(),
            Some(_) => false,
        };

        if needs_placeholder {
            let reason_path = join_path(&props_path, "reason");
            cpa_json::set(doc, &format!("{reason_path}.type"), "string");
            cpa_json::set(
                doc,
                &format!("{reason_path}.description"),
                PLACEHOLDER_REASON_DESCRIPTION,
            );
            cpa_json::set(doc, &req_path, strings_value(&["reason".to_string()]));
            continue;
        }

        // Properties exist but none are required: add a minimal placeholder, except at the top level.
        if matches!(props_val, Some(Value::Object(_))) && !has_required_properties {
            if parent_path.is_empty() {
                continue;
            }
            let placeholder_path = join_path(&props_path, "_");
            if !doc.g(&placeholder_path).exists() {
                cpa_json::set(doc, &format!("{placeholder_path}.type"), "boolean");
            }
            cpa_json::set(doc, &req_path, strings_value(&["_".to_string()]));
        }
    }
}

// ---------------------------------------------------------------- helpers

fn find_paths(doc: &Value, field: &str) -> Vec<String> {
    let mut paths = Vec::new();
    walk(doc, "", field, &mut paths);
    paths
}

fn find_paths_by_fields(doc: &Value, fields: &[&str]) -> HashMap<String, Vec<String>> {
    let mut paths: HashMap<String, Vec<String>> = HashMap::new();
    walk_for_fields(doc, "", fields, &mut paths);
    paths
}

fn walk_for_fields(
    value: &Value,
    path: &str,
    fields: &[&str],
    paths: &mut HashMap<String, Vec<String>>,
) {
    let mut visit = |key: String, val: &Value| {
        let safe_key = escape_gjson_path_key(&key);
        let child_path = if path.is_empty() {
            safe_key
        } else {
            format!("{path}.{safe_key}")
        };
        if fields.contains(&key.as_str()) {
            paths.entry(key).or_default().push(child_path.clone());
        }
        walk_for_fields(val, &child_path, fields, paths);
    };
    match value {
        Value::Object(m) => m.iter().for_each(|(k, v)| visit(k.clone(), v)),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(i, v)| visit(i.to_string(), v)),
        _ => {}
    }
}

/// Stable sort, deepest paths first.
fn sort_by_depth(paths: &mut [String]) {
    paths.sort_by_cached_key(|p| std::cmp::Reverse(split_gjson_path(p).len()));
}

fn trim_suffix(path: &str, suffix: &str) -> String {
    if path == suffix.strip_prefix('.').unwrap_or(suffix) {
        return String::new();
    }
    path.strip_suffix(suffix).unwrap_or(path).to_string()
}

fn join_path(base: &str, suffix: &str) -> String {
    if base.is_empty() {
        suffix.to_string()
    } else {
        format!("{base}.{suffix}")
    }
}

/// Sets `value` at `path`, replacing the whole document for an empty path.
fn set_value_at(doc: &mut Value, path: &str, value: Value) {
    if path.is_empty() {
        *doc = value;
    } else {
        cpa_json::set(doc, path, value);
    }
}

/// Schema keywords whose value maps author-chosen names to subschemas; a key directly under one of
/// them is a name, never a schema keyword.
const SCHEMA_NAME_MAP_KEYWORDS: [&str; 5] = [
    "properties",
    "patternProperties",
    "dependentSchemas",
    "$defs",
    "definitions",
];

/// Whether `path` points at a map whose keys are names chosen by the tool author, so a key spelled
/// like a schema keyword there must be preserved.
///
/// A trailing `.properties` is not enough: a tool may declare a property named `properties`, whose
/// schema then sits at a path ending in `.properties` while being an ordinary schema node. Each
/// name-map keyword at the end of the path flips the answer (`properties` is a map,
/// `properties.properties` the schema of a property named `properties`,
/// `properties.properties.properties` that schema's own map), so only the parity of the trailing
/// run matters and any prefix the schema is nested under is ignored.
fn is_property_definition(path: &str) -> bool {
    let segments = split_gjson_path(path);
    let trailing = segments
        .iter()
        .rev()
        .take_while(|segment| {
            SCHEMA_NAME_MAP_KEYWORDS.contains(&unescape_gjson_path_key(segment).as_str())
        })
        .count();
    trailing % 2 == 1
}

fn description_path(parent_path: &str) -> String {
    if parent_path.is_empty() || parent_path == "@this" {
        "description".into()
    } else {
        format!("{parent_path}.description")
    }
}

/// Combines an existing description with a hint. Cleaning is not always a single pass (a schema may
/// be cleaned by a translator and again by an executor), so an already-present hint is kept as is.
fn merge_hint(existing: &str, hint: &str) -> String {
    if existing.is_empty() {
        return hint.to_string();
    }
    // A hint added to an empty description is stored bare and later hints follow it, so the bare
    // form may sit alone, lead the description, or appear parenthesised further along.
    if existing == hint
        || existing.starts_with(&format!("{hint} ("))
        || existing.contains(&format!("({hint})"))
    {
        return existing.to_string();
    }
    format!("{existing} ({hint})")
}

fn append_hint(doc: &mut Value, parent_path: &str, hint: &str) {
    let desc_path = description_path(parent_path);
    let merged = merge_hint(&doc.g(&desc_path).str(), hint);
    cpa_json::set(doc, &desc_path, merged);
}

fn append_hint_raw(schema: &mut Value, hint: &str) {
    let merged = merge_hint(&schema.g("description").str(), hint);
    cpa_json::set(schema, "description", merged);
}

fn merge_description_raw(schema: &mut Value, parent_desc: &str) {
    let child_desc = schema.g("description").str();
    if child_desc.is_empty() {
        cpa_json::set(schema, "description", parent_desc);
    } else if child_desc != parent_desc {
        cpa_json::set(
            schema,
            "description",
            format!("{parent_desc} ({child_desc})"),
        );
    }
}

/// String items of the array at `path` (empty when absent or not an array).
fn get_strings(doc: &Value, path: &str) -> Vec<String> {
    match doc.g(path).v() {
        Some(Value::Array(items)) => items.iter().map(val_str).collect(),
        _ => Vec::new(),
    }
}

/// A string slice as JSON; an empty slice is `null` like Go's `json.Marshal` of a nil `[]string`.
fn strings_value(items: &[String]) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        Value::Array(items.iter().cloned().map(Value::String).collect())
    }
}

/// Escapes `.`, `*` and `?` in a key for use inside a gjson/sjson path (other special characters
/// are left alone, as in Go).
pub(super) fn escape_gjson_path_key(key: &str) -> String {
    if !key.contains(['.', '*', '?']) {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len() + 2);
    for c in key.chars() {
        if matches!(c, '.' | '*' | '?') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn unescape_gjson_path_key(key: &str) -> String {
    if !key.contains('\\') {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
            continue;
        }
        out.push(c);
    }
    out
}

/// Splits a path on unescaped dots, keeping escapes in the parts. "" has no parts.
fn split_gjson_path(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push('\\');
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            '.' => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

/// Parses the first JSON value of `text`, ignoring trailing data (Go's `Decoder.Decode`). Unlike
/// plain serde_json this accepts documents nested up to `cpa_json::MAX_DEPTH`.
fn parse_first_value(text: &str) -> Option<Value> {
    use serde::Deserialize;
    let (mut depth, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for &b in text.as_bytes() {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if max > cpa_json::MAX_DEPTH {
        return None;
    }
    let mut de = serde_json::Deserializer::from_str(text);
    de.disable_recursion_limit();
    Value::deserialize(serde_stacker::Deserializer::new(&mut de)).ok()
}
