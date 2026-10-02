//! Responses request normalization and the response event bridge for `apply_patch` (Go:
//! common/apply_patch_responses.go).

use std::collections::{BTreeSet, HashMap, HashSet};

use cpa_core::applypatch;
use cpa_core::util::{
    ResponsesToolDescriptor, collect_responses_tool_descriptors, collect_responses_tool_winners,
    qualify_responses_namespace_tool_name,
};
use cpa_json::{J, Res, Value};

use super::{
    ApplyPatchCallState, ApplyPatchErrorState, ApplyPatchInputDecoder, apply_patch_failure,
    apply_patch_input_delta, apply_patch_input_done,
};

fn is_custom_apply_patch(tool: &Value) -> bool {
    applypatch::is_custom_tool(tool)
}

/// Document order paths (`tools.3`, `tools.2.tools.1`, `input.4.tools.0`) of every tool
/// declaration that [`collect_responses_tool_descriptors`] reports, in the same order. Go compares
/// the winning descriptor's tool with the one being rewritten by byte offset; the path is the
/// equivalent identity here.
fn descriptor_paths(root: &Value) -> Vec<String> {
    fn tool_name(tool: &Value) -> String {
        let name = tool.g("name").str().trim().to_string();
        if !name.is_empty() {
            return name;
        }
        tool.g("function.name").str().trim().to_string()
    }
    fn declares(tool: &Value) -> bool {
        matches!(tool.g("type").str().trim(), "" | "function" | "custom") && !tool_name(tool).is_empty()
    }

    let mut sources: Vec<(String, &Vec<Value>)> = Vec::new();
    if let Some(Value::Array(tools)) = root.get("tools") {
        sources.push(("tools".to_string(), tools));
    }
    if let Some(Value::Array(input)) = root.get("input") {
        for (i, item) in input.iter().enumerate() {
            if item.g("type").str() == "additional_tools"
                && let Some(Value::Array(tools)) = item.get("tools")
            {
                sources.push((format!("input.{i}.tools"), tools));
            }
        }
    }

    let mut paths = Vec::new();
    for (prefix, tools) in sources {
        for (j, tool) in tools.iter().enumerate() {
            let path = format!("{prefix}.{j}");
            match tool.g("type").str().trim() {
                "" | "function" | "custom" => {
                    if declares(tool) {
                        paths.push(path);
                    }
                }
                "namespace" => {
                    let (key, children) = match (tool.get("tools"), tool.get("children")) {
                        (Some(Value::Array(c)), _) => ("tools", Some(c)),
                        (_, Some(Value::Array(c))) => ("children", Some(c)),
                        _ => ("tools", None),
                    };
                    for (k, child) in children.into_iter().flatten().enumerate() {
                        if declares(child) {
                            paths.push(format!("{path}.{key}.{k}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    paths
}

/// Adapts declarations and explicit custom patch history of a Responses request: `apply_patch`
/// custom tool declarations become function tools, history `custom_tool_call` items become
/// `function_call` items with wrapped arguments, and their outputs and tool choices follow. Winners
/// are collected before rewriting so normalization never changes declaration precedence. Invalid
/// JSON and a non-string patch history input are errors.
pub fn normalize_apply_patch_responses_request(raw: &[u8]) -> Result<Vec<u8>, String> {
    if !cpa_json::valid(raw) {
        return Err("invalid Responses request JSON".to_string());
    }
    let root = cpa_json::parse(raw);
    let winners = collect_responses_tool_winners(&root);
    let descriptors = collect_responses_tool_descriptors(&root);
    let paths = descriptor_paths(&root);
    let affected: HashSet<&str> = descriptors
        .iter()
        .filter(|d| is_custom_apply_patch(&d.tool))
        .map(|d| d.name.as_str())
        .collect();

    // Rewrites one tools array; `path` locates it in the document and `namespace` is the
    // enclosing namespace tool's name.
    let normalize_tools = |tools: &Res<'_>, path: &str, namespace: &str| -> Value {
        fn walk(
            tools: &Res<'_>,
            path: &str,
            namespace: &str,
            winners: &HashMap<String, ResponsesToolDescriptor>,
            affected: &HashSet<&str>,
            paths: &[String],
        ) -> Value {
            let mut items: Vec<Value> = Vec::new();
            for (index, tool) in tools.array().iter().enumerate() {
                let mut item = tool.value();
                let tool_path = format!("{path}.{index}");
                if tool.g("type").str() == "namespace" {
                    for key in ["tools", "children"] {
                        let children = tool.g(key);
                        if children.is_array() {
                            let rewritten = walk(
                                &children,
                                &format!("{tool_path}.{key}"),
                                &tool.g("name").str(),
                                winners,
                                affected,
                                paths,
                            );
                            cpa_json::set(&mut item, key, rewritten);
                            break;
                        }
                    }
                } else {
                    let mut name = tool.g("name").str();
                    if name.is_empty() {
                        name = tool.g("function.name").str();
                    }
                    let qualified = qualify_responses_namespace_tool_name(namespace, &name);
                    if let Some(winner) = winners.get(&qualified)
                        && affected.contains(qualified.as_str())
                    {
                        if paths.get(winner.order) != Some(&tool_path) {
                            continue;
                        }
                        if tool.v().is_some_and(is_custom_apply_patch) {
                            cpa_json::set(&mut item, "type", "function");
                            cpa_json::set(
                                &mut item,
                                "description",
                                tool.v().map(applypatch::description).unwrap_or_default(),
                            );
                            cpa_json::set(&mut item, "parameters", cpa_json::parse(&applypatch::parameters()));
                            cpa_json::delete(&mut item, "format");
                        }
                    }
                }
                items.push(item);
            }
            Value::Array(items)
        }
        walk(tools, path, namespace, &winners, &affected, &paths)
    };

    let mut doc = root.clone();
    let tools = root.g("tools");
    if tools.is_array() {
        cpa_json::set(&mut doc, "tools", normalize_tools(&tools, "tools", ""));
    }

    let input_res = root.g("input");
    let input = input_res.array();
    let patch_history: HashSet<String> = input
        .iter()
        .filter(|item| {
            item.g("type").str() == "custom_tool_call" && item.g("name").str().trim() == "apply_patch"
        })
        .map(|item| item.g("call_id").str())
        .collect();
    for (i, item) in input.iter().enumerate() {
        let path = format!("input.{i}");
        match item.g("type").str().as_str() {
            "additional_tools" => {
                let tools = item.g("tools");
                if tools.is_array() {
                    cpa_json::set(
                        &mut doc,
                        &format!("{path}.tools"),
                        normalize_tools(&tools, &format!("{path}.tools"), ""),
                    );
                }
            }
            "custom_tool_call" => {
                if item.g("name").str().trim() != "apply_patch" {
                    continue;
                }
                let patch_input = item.g("input");
                let Some(patch_input) = patch_input.as_str() else {
                    return Err("apply_patch history input must be a string".to_string());
                };
                cpa_json::set(&mut doc, &format!("{path}.type"), "function_call");
                cpa_json::set(&mut doc, &format!("{path}.arguments"), applypatch::wrap_input(patch_input));
                cpa_json::delete(&mut doc, &format!("{path}.input"));
            }
            "custom_tool_call_output" if patch_history.contains(&item.g("call_id").str()) => {
                cpa_json::set(&mut doc, &format!("{path}.type"), "function_call_output");
            }
            _ => {}
        }
    }

    fn normalize_choice(choice: &Res<'_>, winners: &HashMap<String, ResponsesToolDescriptor>) -> Value {
        let mut out = choice.value();
        let name = choice.g("name").str();
        let namespace = choice.g("namespace").str();
        if let Some(d) = winners.get(&qualify_responses_namespace_tool_name(&namespace, &name))
            && is_custom_apply_patch(&d.tool)
            && choice.g("type").str() == "custom"
        {
            cpa_json::set(&mut out, "type", "function");
        }
        for (i, child) in choice.g("tools").array().iter().enumerate() {
            cpa_json::set(&mut out, &format!("tools.{i}"), normalize_choice(child, winners));
        }
        out
    }
    let choice = root.g("tool_choice");
    if choice.is_object() {
        cpa_json::set(&mut doc, "tool_choice", normalize_choice(&choice, &winners));
    }
    Ok(cpa_json::to_vec(&doc))
}

#[derive(Debug)]
struct PatchRecord {
    state: ApplyPatchCallState,
    kind: String,
    qualified: String,
    source: String,
    patch: bool,
    named: bool,
    added: bool,
    input_done: bool,
    item_done: bool,
    snapshot: String,
    completed_item: String,
    has_snapshot: bool,
    pending: Vec<Vec<u8>>,
    evidence: Option<String>,
}

impl PatchRecord {
    fn new() -> Self {
        Self {
            state: ApplyPatchCallState {
                output_index: -1,
                ..Default::default()
            },
            kind: String::new(),
            qualified: String::new(),
            source: String::new(),
            patch: false,
            named: false,
            added: false,
            input_done: false,
            item_done: false,
            snapshot: String::new(),
            completed_item: String::new(),
            has_snapshot: false,
            pending: Vec::new(),
            evidence: None,
        }
    }

    fn identity_ready(&self) -> bool {
        !self.state.item_id.is_empty() && !self.state.call_id.is_empty() && self.state.output_index >= 0
    }
}

/// Converts Responses event payloads (JSON, no SSE framing) for streams whose original request
/// declared a custom `apply_patch` tool: upstream function-call argument events become custom
/// tool call input events, and identity evidence is retained even before a call receives its name.
/// A bridge is local to one response.
#[derive(Debug)]
pub struct ApplyPatchResponsesBridge {
    error_state: ApplyPatchErrorState,
    tools: HashMap<String, ResponsesToolDescriptor>,
    records: Vec<PatchRecord>,
    by_item_id: HashMap<String, usize>,
    by_call_id: HashMap<String, usize>,
    by_output_index: HashMap<i64, usize>,
    sequence: i64,
    last_sequence: i64,
    response_id: String,
    failed: bool,
    terminal: bool,
    active: bool,
    converted: bool,
}

/// Events produced for one input, plus the terminal error if the bridge failed on it (Go returns
/// both: the one-shot failure event and the error).
pub type BridgeOutput = (Vec<Vec<u8>>, Option<String>);

impl ApplyPatchResponsesBridge {
    /// Resolves the original declarations before any normalization.
    pub fn new(original_request: &[u8]) -> Self {
        let tools = collect_responses_tool_winners(&cpa_json::parse(original_request));
        let active = tools.values().any(|d| is_custom_apply_patch(&d.tool));
        Self {
            error_state: ApplyPatchErrorState::default(),
            tools,
            records: Vec::new(),
            by_item_id: HashMap::new(),
            by_call_id: HashMap::new(),
            by_output_index: HashMap::new(),
            sequence: 0,
            last_sequence: 0,
            response_id: String::new(),
            failed: false,
            terminal: false,
            active,
            converted: false,
        }
    }

    /// The original conversion error, if any (the `ToolInputError()` contract).
    pub fn tool_input_error(&self) -> Option<&str> {
        self.error_state.tool_input_error()
    }

    pub fn set_tool_input_error(&mut self, err: impl Into<String>) {
        self.error_state.set_tool_input_error(err);
    }

    fn next(&mut self) -> i64 {
        self.sequence += 1;
        self.sequence
    }

    fn failure(&mut self, err: String) -> BridgeOutput {
        if self.failed || self.terminal {
            return (Vec::new(), None);
        }
        self.failed = true;
        self.set_tool_input_error(err.clone());
        let sequence = self.next();
        (vec![apply_patch_failure(&self.response_id, sequence)], Some(err))
    }

    /// Terminates an executor-owned bridge with the same one-shot failure contract.
    pub fn fail(&mut self, err: impl Into<String>) -> BridgeOutput {
        self.failure(err.into())
    }

    fn descriptor(&self, item: &Res<'_>) -> Option<&ResponsesToolDescriptor> {
        let name = qualify_responses_namespace_tool_name(&item.g("namespace").str(), &item.g("name").str());
        self.tools.get(&name)
    }

    /// Checks every supplied identity, not just the first usable key. Conflicting unmatched keys
    /// and multiple matched records retain evidence until patch provenance is known. Returns the
    /// record index.
    fn resolve(&mut self, event: &Res<'_>, item: &Res<'_>) -> Result<usize, String> {
        let ids = [event.g("item_id").str(), item.g("id").str()];
        let calls = [event.g("call_id").str(), item.g("call_id").str()];
        let index = event.g("output_index");
        let index_value = index.int();
        let mut matched: BTreeSet<usize> = BTreeSet::new();
        for id in &ids {
            if let Some(&r) = self.by_item_id.get(id.as_str()).filter(|_| !id.is_empty()) {
                matched.insert(r);
            }
        }
        for id in &calls {
            if let Some(&r) = self.by_call_id.get(id.as_str()).filter(|_| !id.is_empty()) {
                matched.insert(r);
            }
        }
        if index.exists()
            && let Some(&r) = self.by_output_index.get(&index_value)
        {
            matched.insert(r);
        }
        // The first matching record in discovery order (records are only ever appended).
        let r = match matched.first() {
            Some(&r) => r,
            None => {
                self.records.push(PatchRecord::new());
                self.records.len() - 1
            }
        };
        let mut bad = matched.len() > 1;
        {
            let state = &self.records[r].state;
            for id in &ids {
                if !id.is_empty() && !state.item_id.is_empty() && state.item_id != *id {
                    bad = true;
                }
            }
            for id in &calls {
                if !id.is_empty() && !state.call_id.is_empty() && state.call_id != *id {
                    bad = true;
                }
            }
            if index.exists() && state.output_index >= 0 && state.output_index != index_value {
                bad = true;
            }
        }
        if !ids[0].is_empty() && !ids[1].is_empty() && ids[0] != ids[1] {
            bad = true;
        }
        if !calls[0].is_empty() && !calls[1].is_empty() && calls[0] != calls[1] {
            bad = true;
        }
        let descriptor = self.descriptor(item).map(|d| (d.name.clone(), d.local_name.clone(), d.namespace.clone(), is_custom_apply_patch(&d.tool)));
        let mut incoming_patch = descriptor.as_ref().is_some_and(|(_, _, _, custom)| *custom)
            && item.g("type").str() != "custom_tool_call";
        if bad {
            let err_identity = "conflicting apply_patch call identity".to_string();
            self.records[r].evidence = Some(err_identity.clone());
            for &candidate in &matched {
                self.records[candidate].evidence = Some(err_identity.clone());
                if self.records[candidate].patch {
                    incoming_patch = true;
                }
            }
            // Keep aliases for unmatched conflicting keys, too: their later patch provenance must
            // not create a fresh record and erase the earlier contradiction.
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.by_item_id.entry(id.clone()).or_insert(r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.by_call_id.entry(id.clone()).or_insert(r);
            }
            if index.exists() {
                self.by_output_index.entry(index_value).or_insert(r);
            }
            if self.records[r].patch || incoming_patch {
                return Err(err_identity);
            }
        } else {
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.item_id = id.clone();
                self.by_item_id.insert(id.clone(), r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.call_id = id.clone();
                self.by_call_id.insert(id.clone(), r);
            }
            if index.exists() {
                self.records[r].state.output_index = index_value;
                self.by_output_index.insert(index_value, r);
            }
        }
        let kind = item.g("type").str();
        let record = &mut self.records[r];
        if !kind.is_empty() {
            if !record.kind.is_empty() && record.kind != kind {
                record.evidence = Some("conflicting apply_patch call type".to_string());
            }
            if record.kind.is_empty() {
                record.kind = kind;
            }
        }
        let name = item.g("name").str();
        if !name.is_empty() {
            let namespace = item.g("namespace").str();
            let mut qualified = qualify_responses_namespace_tool_name(&namespace, &name);
            if let Some((descriptor_name, _, _, _)) = &descriptor {
                qualified = descriptor_name.clone();
            }
            if record.named && record.qualified != qualified {
                record.evidence = Some("conflicting apply_patch call name".to_string());
            }
            record.named = true;
            record.qualified = qualified;
            record.state.name = name;
            record.state.namespace = namespace;
            if let Some((_, local_name, descriptor_namespace, _)) = &descriptor {
                record.state.name = local_name.clone();
                record.state.namespace = descriptor_namespace.clone();
            }
        }
        if incoming_patch {
            record.patch = true;
        }
        if record.patch
            && let Some(evidence) = &record.evidence
        {
            return Err(evidence.clone());
        }
        Ok(r)
    }

    fn restore_item(&self, item: &[u8], r: usize, input: &str, added: bool) -> Vec<u8> {
        let record = &self.records[r];
        let mut item = cpa_json::parse(item);
        if record.patch {
            cpa_json::set(&mut item, "type", "custom_tool_call");
            cpa_json::delete(&mut item, "arguments");
            cpa_json::set(&mut item, "input", input);
        }
        if let Some(d) = self.tools.get(&record.qualified)
            && !d.namespace.is_empty()
        {
            cpa_json::set(&mut item, "name", d.local_name.as_str());
            cpa_json::set(&mut item, "namespace", d.namespace.as_str());
        }
        if record.patch && !added {
            if !record.state.item_id.is_empty() {
                cpa_json::set(&mut item, "id", record.state.item_id.as_str());
            }
            if !record.state.call_id.is_empty() {
                cpa_json::set(&mut item, "call_id", record.state.call_id.as_str());
            }
            cpa_json::set(&mut item, "name", record.state.name.as_str());
        }
        cpa_json::to_vec(&item)
    }

    fn item_event(&mut self, kind: &str, item: &[u8], r: usize) -> Vec<u8> {
        let mut out = cpa_json::parse_str(r#"{"type":"","output_index":0,"sequence_number":0,"item":{}}"#);
        cpa_json::set(&mut out, "type", kind);
        cpa_json::set(&mut out, "output_index", self.records[r].state.output_index);
        let sequence = self.next();
        cpa_json::set(&mut out, "sequence_number", sequence);
        cpa_json::set(&mut out, "item", cpa_json::parse(item));
        cpa_json::to_vec(&out)
    }

    fn snapshot(&mut self, r: usize, arguments: &Res<'_>, final_: bool) -> Result<(), String> {
        if !arguments.exists() {
            return Ok(());
        }
        let Some(args) = arguments.as_str() else {
            return Err("apply_patch arguments snapshot must be a string".to_string());
        };
        if args.is_empty() && !final_ {
            return Ok(());
        }
        let mut decoder = ApplyPatchInputDecoder::default();
        decoder.finish(args)?;
        let record = &mut self.records[r];
        if record.has_snapshot {
            let mut previous = ApplyPatchInputDecoder::default();
            let _ = previous.finish(&record.snapshot);
            if previous.input() != decoder.input() {
                return Err("conflicting apply_patch arguments snapshot".to_string());
            }
        }
        if !decoder.input().starts_with(record.state.decoder.input()) {
            return Err("apply_patch snapshot conflicts with streamed input".to_string());
        }
        record.snapshot = args.to_string();
        record.has_snapshot = true;
        Ok(())
    }

    fn patch_event(&mut self, raw: &[u8], r: usize) -> Result<Vec<Vec<u8>>, String> {
        if !self.records[r].identity_ready() {
            return Err("unresolved apply_patch call identity".to_string());
        }
        let root = cpa_json::parse(raw);
        let kind = root.g("type").str();
        let item = root.g("item");
        self.converted = true;
        let mut out: Vec<Vec<u8>> = Vec::new();
        if item.exists() {
            if item.g("type").str() != "function_call" {
                return Err("conflicting apply_patch call type".to_string());
            }
            self.snapshot(r, &item.g("arguments"), kind == "response.output_item.done")?;
        }
        if !self.records[r].added {
            let mut added = if item.exists() {
                item.value()
            } else {
                cpa_json::parse_str(r#"{"type":"function_call","name":"","arguments":""}"#)
            };
            let state = &self.records[r].state;
            cpa_json::set(&mut added, "name", state.name.as_str());
            if !state.item_id.is_empty() {
                cpa_json::set(&mut added, "id", state.item_id.as_str());
            }
            if !state.call_id.is_empty() {
                cpa_json::set(&mut added, "call_id", state.call_id.as_str());
            }
            if !state.namespace.is_empty() {
                cpa_json::set(&mut added, "namespace", state.namespace.as_str());
            }
            let restored = self.restore_item(&cpa_json::to_vec(&added), r, "", true);
            out.push(self.item_event("response.output_item.added", &restored, r));
            self.records[r].added = true;
        }
        match kind.as_str() {
            "response.function_call_arguments.delta" => {
                let fragment = root.g("delta").str();
                if self.records[r].input_done {
                    if !fragment.is_empty() {
                        return Err("apply_patch arguments received after completion".to_string());
                    }
                    return Ok(out);
                }
                self.records[r].source.push_str(&fragment);
                let delta = self.records[r].state.push_arguments(&fragment)?;
                if self.records[r].has_snapshot {
                    let mut snapshot = ApplyPatchInputDecoder::default();
                    let _ = snapshot.finish(&self.records[r].snapshot);
                    if !snapshot.input().starts_with(self.records[r].state.decoder.input()) {
                        return Err("apply_patch stream conflicts with snapshot".to_string());
                    }
                }
                if !delta.is_empty() {
                    let sequence = self.next();
                    out.push(apply_patch_input_delta(&self.records[r].state, &delta, sequence));
                }
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                let arguments = if item.exists() {
                    item.g("arguments")
                } else {
                    root.g("arguments")
                };
                if arguments.exists() {
                    self.snapshot(r, &arguments, true)?;
                }
                let record = &mut self.records[r];
                let final_args = if record.has_snapshot {
                    record.snapshot.clone()
                } else {
                    record.source.clone()
                };
                let (tail, input) = record.state.finish_arguments(&final_args)?;
                if !self.records[r].input_done {
                    if !tail.is_empty() && !self.records[r].source.is_empty() {
                        let sequence = self.next();
                        out.push(apply_patch_input_delta(&self.records[r].state, &tail, sequence));
                    }
                    let sequence = self.next();
                    out.push(apply_patch_input_done(&self.records[r].state, &input, sequence));
                    self.records[r].input_done = true;
                }
                if kind == "response.output_item.done" && !self.records[r].item_done {
                    let completed = self.restore_item(&cpa_json::to_vec(&item.value()), r, &input, false);
                    self.records[r].completed_item = String::from_utf8_lossy(&completed).into_owned();
                    out.push(self.item_event(&kind, &completed, r));
                    self.records[r].item_done = true;
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn transform_item_event(&mut self, raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut raw = raw.to_vec();
        let root = cpa_json::parse(&raw);
        let mut item_value = root.g("item").value();
        let mut has_item = root.g("item").exists();
        if !has_item && root.g("name").exists() {
            // Arguments events can supply late names and identities at the root.
            item_value = cpa_json::parse_str(r#"{"type":"function_call"}"#);
            for key in ["name", "namespace", "call_id"] {
                let value = root.g(key);
                if value.exists() {
                    cpa_json::set(&mut item_value, key, value.value());
                }
            }
            has_item = true;
        }
        let item = if has_item { Res::owned(item_value) } else { Res::NONE };
        let root_res = Res::of(&root);
        let r = self.resolve(&root_res, &item)?;
        let real_item_exists = root.g("item").exists();
        // A known name is provenance, not readiness. Retain the real source events until both
        // upstream IDs and the output index can identify every emitted event.
        let (is_patch, wait) = {
            let rec = &self.records[r];
            (
                rec.patch,
                (!rec.named && (rec.kind.is_empty() || rec.kind == "function_call"))
                    || (rec.patch && !rec.identity_ready()),
            )
        };
        if wait {
            if is_patch {
                let arguments = if real_item_exists {
                    item.g("arguments")
                } else {
                    root.g("arguments")
                };
                let kind = root.g("type").str();
                self.snapshot(
                    r,
                    &arguments,
                    kind == "response.output_item.done" || kind == "response.function_call_arguments.done",
                )?;
            }
            self.records[r].pending.push(raw);
            return Ok(Vec::new());
        }
        let pending = std::mem::take(&mut self.records[r].pending);
        let mut out: Vec<Vec<u8>> = Vec::new();
        if self.records[r].patch {
            let mut events = pending;
            events.push(raw);
            for event in events {
                out.extend(self.patch_event(&event, r)?);
            }
            return Ok(out);
        }
        out.extend(pending);
        if real_item_exists
            && self.records[r].kind == "function_call"
            && self.tools.get(&self.records[r].qualified).is_some_and(|d| !d.namespace.is_empty())
        {
            let restored = self.restore_item(&cpa_json::to_vec(&item.value()), r, "", false);
            let mut doc = cpa_json::parse(&raw);
            cpa_json::set(&mut doc, "item", cpa_json::parse(&restored));
            raw = cpa_json::to_vec(&doc);
        }
        if root.g("type").str() == "response.output_item.done" && item.exists() {
            self.records[r].item_done = true;
            self.records[r].completed_item = cpa_json::parse(&raw).g("item").raw();
        }
        out.push(raw);
        Ok(out)
    }

    /// Rewrites a terminal response (envelope or bare) so converted calls appear as custom tool
    /// calls, returning the new body and the events that must precede it.
    fn envelope(&mut self, raw: &[u8], stream: bool) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
        let root = cpa_json::parse(raw);
        let mut doc = root.clone();
        let (path, response) = if root.g("response").exists() {
            ("response.output", root.g("response"))
        } else {
            ("output", Res::of(&root))
        };
        let output_res = response.g("output");
        let output = output_res.array();
        let mut preceding: Vec<Vec<u8>> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut items: Vec<Vec<u8>> = Vec::new();
        for (i, item) in output.iter().enumerate() {
            let mut index = i as i64;
            // A terminal snapshot may omit earlier completed items: array position is not identity.
            let known = self
                .by_item_id
                .get(item.g("id").str().as_str())
                .or_else(|| self.by_call_id.get(item.g("call_id").str().as_str()))
                .copied();
            let (item_id, call_id) = (item.g("id").str(), item.g("call_id").str());
            match known {
                Some(k) if self.records[k].state.output_index >= 0 => {
                    index = self.records[k].state.output_index;
                }
                None if !item_id.is_empty() || !call_id.is_empty() => {
                    let previous_has_identity = self.by_output_index.get(&index).is_some_and(|&p| {
                        let s = &self.records[p].state;
                        !s.item_id.is_empty() || !s.call_id.is_empty()
                    });
                    if previous_has_identity {
                        for record in &self.records {
                            if record.state.output_index >= index {
                                index = record.state.output_index + 1;
                            }
                        }
                    }
                }
                _ => {}
            }
            let event_raw = patch_envelope_item(index, item);
            let event = cpa_json::parse(&event_raw);
            let r = self.resolve(&Res::of(&event), item)?;
            seen.insert(r);
            let item_path = format!("{path}.{i}");
            if self.records[r].patch {
                let pending = std::mem::take(&mut self.records[r].pending);
                for source in pending {
                    preceding.extend(self.patch_event(&source, r)?);
                }
                preceding.extend(self.patch_event(&event_raw, r)?);
                let restored = self.restore_item(
                    &cpa_json::to_vec(&item.value()),
                    r,
                    self.records[r].state.decoder.input(),
                    false,
                );
                cpa_json::set(&mut doc, &item_path, cpa_json::parse(&restored));
            } else if item.g("type").str() == "function_call"
                && self.tools.get(&self.records[r].qualified).is_some_and(|d| !d.namespace.is_empty())
            {
                let restored = self.restore_item(&cpa_json::to_vec(&item.value()), r, "", false);
                cpa_json::set(&mut doc, &item_path, cpa_json::parse(&restored));
            }
            items.push(doc.g(&item_path).raw().into_bytes());
        }
        for r in 0..self.records.len() {
            if seen.contains(&r) || (!self.records[r].patch && !self.converted) {
                continue;
            }
            if self.records[r].input_done && !self.records[r].item_done {
                let input = self.records[r].state.decoder.input().to_string();
                let completed = self.restore_item(br#"{"type":"function_call","status":"completed"}"#, r, &input, false);
                self.records[r].completed_item = String::from_utf8_lossy(&completed).into_owned();
                preceding.push(self.item_event("response.output_item.done", &completed, r));
                self.records[r].item_done = true;
            }
            if self.records[r].item_done {
                let mut index = self.records[r].state.output_index;
                if index < 0 || index as usize > items.len() {
                    index = items.len() as i64;
                }
                items.insert(index as usize, self.records[r].completed_item.clone().into_bytes());
            }
        }
        if items.len() != output.len() {
            let joined = super::super::join_raw_array(&items);
            cpa_json::set(&mut doc, path, cpa_json::parse(&joined));
        }
        if stream {
            self.finish()?;
            for record in &mut self.records {
                preceding.append(&mut record.pending);
            }
            if self.converted {
                let sequence = self.next();
                cpa_json::set(&mut doc, "sequence_number", sequence);
            }
        }
        Ok((cpa_json::to_vec(&doc), preceding))
    }

    /// Converts one event payload. A failure is terminal and emitted only once: the returned
    /// events then hold the `response.failed` payload and the second element the error.
    pub fn transform(&mut self, event: &[u8]) -> BridgeOutput {
        if self.failed || self.terminal {
            return (Vec::new(), None);
        }
        if !self.active {
            return (vec![event.to_vec()], None);
        }
        let root = cpa_json::parse(event);
        let id = root.g("response.id").str();
        if !id.is_empty() {
            self.response_id = id;
        }
        let seq = root.g("sequence_number").int();
        if seq > self.sequence {
            self.sequence = seq;
        }
        let kind = root.g("type").str();
        let result: Result<Vec<Vec<u8>>, String> = match kind.as_str() {
            "response.output_item.added"
            | "response.output_item.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done" => self.transform_item_event(event),
            "response.completed" | "response.incomplete" | "response.done" => {
                self.envelope(event, true).map(|(final_event, mut out)| {
                    out.push(final_event);
                    self.terminal = true;
                    out
                })
            }
            "response.failed" => {
                self.terminal = true;
                Ok(vec![event.to_vec()])
            }
            _ => Ok(vec![event.to_vec()]),
        };
        let mut out = match result {
            Ok(out) => out,
            Err(err) => return self.failure(err),
        };
        for event in out.iter_mut() {
            let mut seq = cpa_json::parse(event).g("sequence_number").int();
            // Native custom payloads are opaque. Only a stream that acquired a function patch call
            // needs resequencing of its other compatibility events.
            let native_custom = kind.starts_with("response.custom_tool_call_input.")
                || root.g("item.type").str() == "custom_tool_call";
            if self.converted && !native_custom && seq <= self.last_sequence {
                seq = self.next();
                let mut doc = cpa_json::parse(event);
                cpa_json::set(&mut doc, "sequence_number", seq);
                *event = cpa_json::to_vec(&doc);
            }
            if seq > self.last_sequence {
                self.last_sequence = seq;
            }
        }
        (out, None)
    }

    /// Converts a non-streaming response (a bare response or a terminal event envelope).
    pub fn transform_non_stream(&mut self, response: &[u8]) -> Result<Vec<u8>, String> {
        if !self.active {
            return Ok(response.to_vec());
        }
        match self.envelope(response, false) {
            Ok((out, _)) => Ok(out),
            Err(err) => {
                self.failed = true;
                self.set_tool_input_error(err.clone());
                Err(err)
            }
        }
    }

    /// Rejects acquired calls whose final arguments have not been validated.
    pub fn finish(&self) -> Result<(), String> {
        if let Some(err) = self.tool_input_error() {
            return Err(err.to_string());
        }
        if self.terminal {
            return Ok(());
        }
        if self.records.iter().any(|r| r.patch && !r.input_done) {
            return Err("incomplete apply_patch tool arguments received from upstream".to_string());
        }
        Ok(())
    }
}

fn patch_envelope_item(index: i64, item: &Res<'_>) -> Vec<u8> {
    let mut out = cpa_json::parse_str(r#"{"type":"response.output_item.done","output_index":0,"item":{}}"#);
    cpa_json::set(&mut out, "output_index", index);
    cpa_json::set(&mut out, "item", item.value());
    cpa_json::to_vec(&out)
}
