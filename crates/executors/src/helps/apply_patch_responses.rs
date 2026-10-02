//! `apply_patch` over Responses events for non-native executors (Go:
//! helps/apply_patch_responses.go).
//!
//! An executor that owns the Codex `apply_patch` contract (not inferred from the wire format)
//! creates an [`ApplyPatchResponsesState`] per response. It wraps the common bridge
//! (`cpa_translator::common::ApplyPatchResponsesBridge`) and adds the xAI-style folded namespace
//! dispatcher: a function call whose arguments are a wrapper `{"name": child, "arguments": ...}`
//! is expanded back into the child call, with identity and argument consistency checks.
//!
//! Errors are `String` messages with Go's text; functions returning events together with an error
//! return `(events, Some(err))` like Go's `([][]byte, error)`.

use std::collections::{HashMap, HashSet};

use cpa_core::applypatch;
use cpa_core::util::{ResponsesToolDescriptor, collect_responses_tool_winners, qualify_responses_namespace_tool_name};
use cpa_json::{J, Kind, Value};
use cpa_translator::Format;
use cpa_translator::common::{ApplyPatchResponsesBridge, join_raw_array, normalize_apply_patch_responses_request};

use super::text::trim_space;

/// Events produced plus the terminal error, if any.
pub type Output = (Vec<Vec<u8>>, Option<String>);

/// Opts a non-Codex executor into the patch contract: normalizes the Responses request so
/// `apply_patch` custom declarations are valid for the upstream. `original` (the client request)
/// lets Chat requests keep their ordinary function-tool winner.
pub fn normalize_apply_patch_responses_request_with_original(
    body: &[u8],
    original: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let body = match original {
        Some(original) => prefer_chat_function_patch_tools(original, body),
        None => body.to_vec(),
    };
    normalize_apply_patch_responses_request(&body)
}

/// Same as [`normalize_apply_patch_responses_request_with_original`] without an original.
pub fn normalize_apply_patch_responses_request_body(body: &[u8]) -> Result<Vec<u8>, String> {
    normalize_apply_patch_responses_request_with_original(body, None)
}

/// Preserves the Chat request converter's winner rule: a custom `apply_patch` declaration is
/// dropped when the original Chat request declared an ordinary function of that name and the
/// declarations still carry the function.
fn prefer_chat_function_patch_tools(original: &[u8], declarations: &[u8]) -> Vec<u8> {
    let original = cpa_json::parse(original);
    let ordinary: HashSet<String> = original
        .g("tools")
        .array()
        .iter()
        .filter(|t| t.g("type").str() == "function")
        .map(|t| t.g("function.name").str())
        .collect();
    if ordinary.is_empty() {
        return declarations.to_vec();
    }
    let mut decls = cpa_json::parse(declarations);
    let tools = decls.g("tools").value();
    let items = tools.as_array().cloned().unwrap_or_default();
    let available: HashSet<String> = items
        .iter()
        .filter(|t| t.g("type").str() == "function")
        .map(|t| t.g("name").str())
        .collect();
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|tool| {
            let name = tool.g("name").str();
            !(applypatch::is_custom_tool(tool) && ordinary.contains(&name) && available.contains(&name))
        })
        .collect();
    cpa_json::set(&mut decls, "tools", Value::Array(kept));
    cpa_json::to_vec(&decls)
}

#[derive(Default)]
struct PatchDispatcherCall {
    namespace: String,
    events: Vec<Vec<u8>>,
    snapshots: Vec<Vec<u8>>,
    source: String,
    originals: Vec<Vec<u8>>,
    completed: bool,
    ordinary: bool,
    /// Output index of the call, -1 until known.
    index: i64,
    name: String,
    arguments: String,
}

/// Request-local patch bridge state owned explicitly by a non-native executor.
pub struct ApplyPatchResponsesState {
    pub bridge: ApplyPatchResponsesBridge,
    tools: HashMap<String, ResponsesToolDescriptor>,
    /// Dispatcher tool name to its namespace.
    dispatchers: HashMap<String, String>,
    by_dispatcher_key: HashMap<String, usize>,
    records: Vec<PatchDispatcherCall>,
    upstream: Option<Vec<u8>>,
    event_line: Option<Vec<u8>>,
    active: bool,
    failed: bool,
    closed: bool,
    transport_done: bool,
}

impl ApplyPatchResponsesState {
    /// `source` is the client format; `original` the client request and `declarations` the
    /// Responses request whose winning declarations the bridge resolves.
    pub fn new(source: Format, original: &[u8], declarations: &[u8]) -> Self {
        let declarations = if source == Format::OpenAI {
            prefer_chat_function_patch_tools(original, declarations)
        } else {
            declarations.to_vec()
        };
        let tools = collect_responses_tool_winners(&cpa_json::parse(&declarations));
        let active = tools.values().any(|d| applypatch::is_custom_tool(&d.tool));
        Self {
            bridge: ApplyPatchResponsesBridge::new(&declarations),
            tools,
            dispatchers: HashMap::new(),
            by_dispatcher_key: HashMap::new(),
            records: Vec::new(),
            upstream: None,
            event_line: None,
            active,
            failed: false,
            closed: false,
            transport_done: false,
        }
    }

    /// Whether this state owns a winning patch declaration.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Marks an xAI folded namespace dispatcher that contains a winning custom patch.
    pub fn add_dispatcher(&mut self, name: &str, namespace: &str) {
        if self.tools.values().any(|d| d.namespace == namespace && applypatch::is_custom_tool(&d.tool)) {
            self.dispatchers.insert(name.to_string(), namespace.to_string());
        }
    }

    fn tool_is_patch(&self, qualified: &str) -> bool {
        self.tools.get(qualified).is_some_and(|d| applypatch::is_custom_tool(&d.tool))
    }

    fn dispatcher_in(&self, name: &str, namespace: &str) -> bool {
        self.dispatchers.get(name).is_some_and(|ns| ns == namespace)
    }

    /// The record matching any identity key of `root` (the earliest record when several match).
    fn dispatcher(&self, root: &Value) -> Option<usize> {
        let matched: HashSet<usize> =
            dispatcher_keys(root).iter().filter_map(|k| self.by_dispatcher_key.get(k).copied()).collect();
        (0..self.records.len()).find(|i| matched.contains(i))
    }

    fn new_dispatcher_candidate(&mut self, root: &Value) -> usize {
        let idx = self.records.len();
        self.records.push(PatchDispatcherCall { index: -1, ..Default::default() });
        for key in dispatcher_keys(root) {
            self.by_dispatcher_key.entry(key).or_insert(idx);
        }
        idx
    }

    fn clear_dispatchers(&mut self) {
        self.by_dispatcher_key.clear();
        self.records.clear();
        self.upstream = None;
    }

    /// Retains upstream evidence before namespace restoration. A wrapper alone never proves
    /// dispatcher provenance: only a declared dispatcher name does.
    pub fn remember_dispatcher_event(&mut self, event: &[u8]) {
        if self.failed || self.closed || self.transport_done || self.dispatchers.is_empty() {
            return;
        }
        self.upstream = Some(event.to_vec());
        self.remember_dispatcher_arguments(event);
    }

    /// Keeps full snapshots on every matched candidate, including unnamed calls. The common
    /// bridge retains all identity/type contradictions until acquisition.
    pub fn remember_dispatcher_arguments(&mut self, event: &[u8]) {
        if self.failed || self.closed || self.transport_done || self.dispatchers.is_empty() {
            return;
        }
        let root = cpa_json::parse(event);
        if root.g("type").str() != "response.function_call_arguments.done" {
            return;
        }
        self.upstream = Some(event.to_vec());
        if self.dispatcher(&root).is_none() {
            self.new_dispatcher_candidate(&root);
        }
        let mut seen: HashSet<usize> = HashSet::new();
        for key in dispatcher_keys(&root) {
            if let Some(&idx) = self.by_dispatcher_key.get(&key)
                && seen.insert(idx)
            {
                self.records[idx].snapshots.push(event.to_vec());
            }
        }
    }

    fn expand_dispatcher(&mut self, event: &[u8], original: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut event = event.to_vec();
        let root = cpa_json::parse(&event);
        let raw = cpa_json::parse(original);
        let kind = root.g("type").str();
        let mut call = self.dispatcher(&root);
        let mut name = dispatcher_event_name(&raw);
        let declared_ns = self.dispatchers.get(&name).cloned();
        let declared = declared_ns.is_some();
        let namespace = declared_ns.unwrap_or_default();
        if call.is_none() && !self.dispatchers.is_empty() {
            let item_is_call = raw.g("item.type").str() == "function_call";
            let added = kind == "response.output_item.added" && item_is_call;
            if (declared && item_is_call)
                || added
                || kind == "response.function_call_arguments.delta"
                || kind == "response.function_call_arguments.done"
            {
                call = Some(self.new_dispatcher_candidate(&root));
            }
        }
        let Some(ci) = call else {
            return Ok(vec![event]);
        };
        self.bridge.check_identity(&event)?;
        for key in dispatcher_keys(&root) {
            self.by_dispatcher_key.entry(key).or_insert(ci);
        }
        if self.records[ci].index < 0 && root.g("output_index").exists() {
            self.records[ci].index = root.g("output_index").int();
        }
        if self.records[ci].ordinary && !declared {
            return Ok(vec![event]);
        }
        self.records[ci].events.push(event.clone());
        self.records[ci].originals.push(original.to_vec());
        if declared {
            let call = &mut self.records[ci];
            if !call.namespace.is_empty() && call.namespace != namespace {
                return Err("conflicting apply_patch dispatcher namespace".into());
            }
            call.namespace = namespace.clone();
            call.ordinary = false;
        }
        if kind == "response.function_call_arguments.delta" {
            let delta = root.g("delta").str();
            if self.records[ci].completed && !delta.is_empty() {
                let child = qualify_responses_namespace_tool_name(&self.records[ci].namespace, &self.records[ci].name);
                if self.tool_is_patch(&child) {
                    return Err("apply_patch dispatcher arguments received after completion".into());
                }
                return Ok(vec![event]);
            }
            self.records[ci].source.push_str(&delta);
        }
        if self.records[ci].namespace.is_empty() {
            if !name.is_empty() {
                // A late ordinary name releases untouched arguments, even if they look like a wrapper.
                let call = &mut self.records[ci];
                call.ordinary = true;
                call.originals.clear();
                return Ok(std::mem::take(&mut call.events));
            }
            return Ok(Vec::new());
        }
        let call_namespace = self.records[ci].namespace.clone();
        let completed = self.records[ci].completed;
        if kind == "response.function_call_arguments.delta" && !completed {
            return Ok(Vec::new());
        }
        let late = completed
            && matches!(
                kind.as_str(),
                "response.function_call_arguments.done"
                    | "response.function_call_arguments.delta"
                    | "response.output_item.added"
            );
        if kind != "response.output_item.done" && !late {
            return Ok(Vec::new());
        }
        let path = if kind == "response.function_call_arguments.done" || kind == "response.function_call_arguments.delta" {
            ""
        } else {
            "item."
        };

        let mut wrappers: Vec<String> = Vec::new();
        if !self.records[ci].source.is_empty() {
            wrappers.push(self.records[ci].source.clone());
        }
        for snapshot in &self.records[ci].snapshots {
            let arguments = cpa_json::parse(snapshot).g("arguments").str();
            if cpa_json::parse_str(&arguments).g("name").exists() {
                wrappers.push(arguments);
            }
        }
        for pending in &self.records[ci].originals {
            let p = cpa_json::parse(pending);
            let arguments = p.g("item.arguments").str();
            let pname = dispatcher_event_name(&p);
            if (pname.is_empty() || self.dispatcher_in(&pname, &call_namespace))
                && cpa_json::parse_str(&arguments).g("name").exists()
            {
                wrappers.push(arguments);
            }
        }
        // Callers without a pre-restoration copy retain the original full-source contract.
        if wrappers.is_empty() {
            for pending in &self.records[ci].events {
                let args = cpa_json::parse(pending).g("arguments").str();
                if !cpa_json::parse_str(&args).g("name").str().is_empty() {
                    wrappers.push(args);
                }
            }
        }
        let mut source = Value::Null;
        for wrapper in &wrappers {
            if cpa_json::valid(wrapper.as_bytes()) && !cpa_json::parse_str(wrapper).g("name").str().is_empty() {
                source = cpa_json::parse_str(wrapper);
            }
        }
        name = root.g(&format!("{path}name")).str();
        if name.is_empty() || self.dispatcher_in(&name, &call_namespace) {
            name = source.g("name").str();
            if name.is_empty() {
                name = self.records[ci].name.clone();
            }
            event = set_value(&event, &format!("{path}name"), name.clone().into());
            event = set_value(&event, &format!("{path}namespace"), call_namespace.clone().into());
        }
        if root.g(&format!("{path}namespace")).str().is_empty() {
            event = set_value(&event, &format!("{path}namespace"), call_namespace.clone().into());
        }
        if declared || dispatcher_event_name(&raw).is_empty() || kind == "response.function_call_arguments.done" {
            let wrapper = cpa_json::parse_str(&raw.g(&format!("{path}arguments")).str());
            if !wrapper.g("name").str().is_empty() {
                event = set_value(&event, &format!("{path}arguments"), patch_dispatcher_arguments(&wrapper).into());
            }
        }
        if !root.g(&format!("{path}arguments")).exists() {
            let mut encoded = patch_dispatcher_arguments(&source);
            if encoded.is_empty() {
                encoded = self.records[ci].arguments.clone();
            }
            if !encoded.is_empty() {
                event = set_value(&event, &format!("{path}arguments"), encoded.into());
            }
        }
        let root = cpa_json::parse(&event);
        let last = self.records[ci].events.len() - 1;
        self.records[ci].events[last] = event.clone();
        let qualified = qualify_responses_namespace_tool_name(&call_namespace, &name);
        let descriptor_name = self.tools.get(&qualified).map(|d| d.name.clone()).unwrap_or_default();
        let patch = self.tool_is_patch(&qualified);
        let mut final_arguments = root.g(&format!("{path}arguments")).str();
        if kind == "response.output_item.added" && final_arguments.is_empty() {
            final_arguments = self.records[ci].arguments.clone();
        }
        if patch {
            for snapshot in &self.records[ci].snapshots {
                if cpa_json::parse(snapshot).g("arguments").kind() != Kind::String {
                    return Err("apply_patch dispatcher arguments snapshot must be a string".into());
                }
            }
        }
        for wrapper_raw in &wrappers {
            let wrapper = cpa_json::parse_str(wrapper_raw);
            let child = qualify_responses_namespace_tool_name(&call_namespace, &wrapper.g("name").str());
            if !patch && !self.tool_is_patch(&child) {
                continue;
            }
            let input = applypatch::unwrap_input(&patch_dispatcher_arguments(&wrapper));
            let final_input = applypatch::unwrap_input(&final_arguments);
            let consistent = cpa_json::valid(wrapper_raw.as_bytes())
                && wrapper.g("name").str() == name
                && matches!((&input, &final_input), (Ok(a), Ok(b)) if a == b);
            if !consistent {
                return Err("conflicting apply_patch dispatcher arguments".into());
            }
        }

        // Retain completed aliases and source evidence until the response actually closes.
        // Repeated snapshots validate only the new event, never replay completed progress.
        let start = if completed { self.records[ci].events.len() - 1 } else { 0 };
        let mut out: Vec<Vec<u8>> = Vec::new();
        for i in start..self.records[ci].events.len() {
            let mut pending = self.records[ci].events[i].clone();
            let p = cpa_json::parse(&pending);
            let original_root = cpa_json::parse(&self.records[ci].originals[i]);
            if patch {
                for ns_path in ["namespace", "item.namespace"] {
                    let supplied = original_root.g(ns_path).str();
                    if !supplied.is_empty() && supplied != call_namespace {
                        return Err("conflicting apply_patch dispatcher namespace".into());
                    }
                }
                for supplied in [dispatcher_event_name(&original_root), dispatcher_event_name(&p)] {
                    if !supplied.is_empty()
                        && !self.dispatcher_in(&supplied, &call_namespace)
                        && qualify_responses_namespace_tool_name(&call_namespace, &supplied) != descriptor_name
                    {
                        return Err("conflicting apply_patch dispatcher child".into());
                    }
                }
            }
            let p_type = p.g("type").str();
            let pending_path = match p_type.as_str() {
                "response.function_call_arguments.delta" => {
                    if patch {
                        // A dispatcher envelope is not incremental child input.
                        continue;
                    }
                    ""
                }
                "response.output_item.added" | "response.output_item.done" => "item.",
                "response.function_call_arguments.done" => "",
                _ => {
                    out.push(pending);
                    continue;
                }
            };
            if p_type != "response.function_call_arguments.delta" {
                let pending_name = p.g(&format!("{pending_path}name")).str();
                if pending_name.is_empty() || self.dispatcher_in(&pending_name, &call_namespace) {
                    pending = set_value(&pending, &format!("{pending_path}name"), name.clone().into());
                    pending = set_value(&pending, &format!("{pending_path}namespace"), call_namespace.clone().into());
                }
                if p.g(&format!("{pending_path}namespace")).str().is_empty() {
                    pending = set_value(&pending, &format!("{pending_path}namespace"), call_namespace.clone().into());
                }
                // Only actual upstream wrappers are unwrapped, not restored child contents.
                let args = original_root.g(&format!("{pending_path}arguments"));
                if patch && args.exists() && args.kind() != Kind::String {
                    return Err("apply_patch dispatcher arguments snapshot must be a string".into());
                }
                let original_name = dispatcher_event_name(&original_root);
                if !args.str().is_empty()
                    && (original_name.is_empty() || self.dispatcher_in(&original_name, &call_namespace) || pending_path.is_empty())
                {
                    let wrapper = cpa_json::parse_str(&args.str());
                    let wrapper_name = wrapper.g("name").str();
                    if !wrapper_name.is_empty() {
                        if patch && wrapper_name != name {
                            return Err("conflicting apply_patch dispatcher snapshot".into());
                        }
                        pending = set_value(&pending, &format!("{pending_path}arguments"), patch_dispatcher_arguments(&wrapper).into());
                    }
                }
            }
            out.push(pending);
        }
        let call = &mut self.records[ci];
        call.completed = true;
        call.name = name;
        call.arguments = final_arguments;
        Ok(out)
    }

    fn unfinished_dispatcher(&self) -> bool {
        self.records.iter().any(|c| !c.namespace.is_empty() && !c.completed)
    }

    fn fail(&mut self, err: String) -> Output {
        if self.failed {
            return (Vec::new(), Some(err));
        }
        self.failed = true;
        self.clear_dispatchers();
        self.bridge.fail(err)
    }

    /// Converts one upstream Responses event payload (JSON without SSE framing).
    pub fn transform(&mut self, event: &[u8]) -> Output {
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        if self.active && trim_space(event) == b"[DONE]" {
            if let Err(err) = self.finish() {
                return self.fail(err);
            }
            self.transport_done = true;
            return (vec![event.to_vec()], None);
        }
        if self.closed {
            return (Vec::new(), None);
        }
        let original = self.upstream.take().unwrap_or_else(|| event.to_vec());
        let mut event = event.to_vec();
        let mut preceding: Vec<Vec<u8>> = Vec::new();
        let root = cpa_json::parse(&event);
        let kind = root.g("type").str();
        let completion = matches!(kind.as_str(), "response.completed" | "response.incomplete" | "response.done");
        if completion {
            let original_items = cpa_json::parse(&original).g("response.output").value();
            let original_items = original_items.as_array().cloned().unwrap_or_default();
            let items = root.g("response.output").value();
            for (i, item) in items.as_array().cloned().unwrap_or_default().iter().enumerate() {
                let mut done = cpa_json::parse_str(r#"{"type":"response.output_item.done"}"#);
                cpa_json::set(&mut done, "item", item.clone());
                let item_id = item.g("id").str();
                let item_call_id = item.g("call_id").str();
                // Explicit IDs take priority over array position in a sparse terminal snapshot.
                let mut call = self.dispatcher(&done);
                if call.is_none() && item_id.is_empty() && item_call_id.is_empty() {
                    cpa_json::set(&mut done, "output_index", i as i64);
                    call = self.dispatcher(&done);
                }
                let Some(ci) = call.filter(|ci| !self.records[*ci].ordinary) else {
                    continue;
                };
                let index = self.records[ci].index;
                cpa_json::set(&mut done, "output_index", if index >= 0 { index } else { i as i64 });
                let mut original_done = done.clone();
                // Filtering may shift array positions, but restoration preserves both IDs.
                let matches = |c: &Value| c.g("id").str() == item_id && c.g("call_id").str() == item_call_id;
                if i < original_items.len() && matches(&original_items[i]) {
                    cpa_json::set(&mut original_done, "item", original_items[i].clone());
                } else if (!item_id.is_empty() || !item_call_id.is_empty())
                    && let Some(found) = original_items.iter().find(|c| matches(c)) {
                        cpa_json::set(&mut original_done, "item", found.clone());
                    }
                match self.expand_dispatcher(&cpa_json::to_vec(&done), &cpa_json::to_vec(&original_done)) {
                    Ok(events) => {
                        if let Some(last) = events.last() {
                            let restored = cpa_json::parse(last).g("item").value();
                            event = set_value(&event, &format!("response.output.{i}"), restored);
                        }
                        preceding.extend(events);
                    }
                    Err(err) => return self.fail(err),
                }
            }
            if self.unfinished_dispatcher() {
                return self.fail("incomplete apply_patch namespace dispatcher received from upstream".into());
            }
            // Unproven candidates remain ordinary; let the common bridge resolve or flush them.
            for call in &mut self.records {
                if call.namespace.is_empty() && !call.ordinary {
                    preceding.append(&mut call.events);
                }
            }
        }
        let expanded = self.expand_dispatcher(&event, &original);
        let events = match expanded {
            Ok(events) => {
                preceding.extend(events);
                preceding
            }
            Err(err) => return self.fail(err),
        };
        let mut out: Vec<Vec<u8>> = Vec::new();
        for e in &events {
            let (converted, err) = self.bridge.transform(e);
            out.extend(converted);
            if err.is_some() {
                self.failed = true;
                self.clear_dispatchers();
                return (out, err);
            }
        }
        if completion || kind == "response.failed" {
            self.closed = true;
            self.clear_dispatchers();
        }
        (out, None)
    }

    /// Validates source response closure independently of completed tool input. The common
    /// bridge's own `finish` stays argument-only because it also runs inside terminal conversion.
    pub fn finish(&mut self) -> Result<(), String> {
        self.bridge.finish()?;
        if self.closed || !self.active {
            return Ok(());
        }
        if self.unfinished_dispatcher() {
            return Err("incomplete apply_patch namespace dispatcher received from upstream".into());
        }
        Err("incomplete apply_patch source response received from upstream".into())
    }

    /// Processes one SSE line, preserving framing and updating `event:` lines to match converted
    /// JSON. A premature `[DONE]` is checked before any success marker is published.
    pub fn stream(&mut self, line: &[u8]) -> Output {
        if !self.active {
            return (vec![line.to_vec()], None);
        }
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        if line.starts_with(b"event:") {
            self.event_line = Some(line.to_vec());
            return (Vec::new(), None);
        }
        if !line.starts_with(b"data:") {
            return (vec![line.to_vec()], None);
        }
        let payload = trim_space(&line[5..]).to_vec();
        let (events, err) = if payload == b"[DONE]" {
            match self.finish() {
                Err(err) => self.fail(err),
                Ok(()) => {
                    // JSON completion and transport completion are separate boundaries.
                    self.transport_done = true;
                    self.clear_dispatchers();
                    self.event_line = None;
                    return (vec![line.to_vec()], None);
                }
            }
        } else {
            self.transform(&payload)
        };
        if events.len() == 1 && events[0] == payload && err.is_none() {
            let mut out = Vec::new();
            if let Some(event_line) = self.event_line.take() {
                out.push(event_line);
            }
            out.push(line.to_vec());
            return (out, None);
        }
        let mut out = Vec::new();
        for event in &events {
            if let Some(event_line) = &self.event_line {
                if *event == payload {
                    out.push(event_line.clone());
                } else {
                    out.push(format!("event: {}", cpa_json::parse(event).g("type").str()).into_bytes());
                }
            }
            // Each expanded event must be a complete SSE frame, even if the source had only one.
            let mut data = b"data: ".to_vec();
            data.extend_from_slice(event);
            data.extend_from_slice(b"\n\n");
            out.push(data);
        }
        self.event_line = None;
        (out, err)
    }

    /// Emits the local failure once on EOF without a validated completion.
    pub fn finish_stream(&mut self) -> Output {
        if self.failed || self.transport_done {
            return (Vec::new(), None);
        }
        match self.finish() {
            Ok(()) => (Vec::new(), None),
            Err(err) => {
                let (events, failure) = self.fail(err);
                let framed = events
                    .into_iter()
                    .map(|e| {
                        let mut data = b"data: ".to_vec();
                        data.extend_from_slice(&e);
                        data.extend_from_slice(b"\n\n");
                        data
                    })
                    .collect();
                (framed, failure)
            }
        }
    }
}

/// Identity keys of an event: item ids, call ids and output index.
fn dispatcher_keys(root: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    for path in ["item.id", "item_id"] {
        let id = root.g(path).str();
        if !id.is_empty() {
            keys.push(format!("item:{id}"));
        }
    }
    for path in ["item.call_id", "call_id"] {
        let id = root.g(path).str();
        if !id.is_empty() {
            keys.push(format!("call:{id}"));
        }
    }
    let index = root.g("output_index");
    if index.exists() {
        keys.push(format!("index:{}", index.int()));
    }
    keys
}

/// Arguments of a dispatcher wrapper as text: strings verbatim, other JSON as compact text.
fn patch_dispatcher_arguments(wrapper: &Value) -> String {
    match wrapper.g("arguments").v() {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn dispatcher_event_name(root: &Value) -> String {
    if root.g("item").exists() { root.g("item.name").str() } else { root.g("name").str() }
}

/// Sets `path` on the JSON event and re-serializes it (Go: `sjson.SetBytes`).
fn set_value(event: &[u8], path: &str, value: Value) -> Vec<u8> {
    let mut v = cpa_json::parse(event);
    cpa_json::set(&mut v, path, value);
    cpa_json::to_vec(&v)
}

/// Joins raw JSON items (re-exported for executors that rebuild `tools`).
pub fn join_raw_json(items: &[Vec<u8>]) -> Vec<u8> {
    join_raw_array(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decl() -> Vec<u8> {
        serde_json::to_vec(&json!({"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"g"}}]})).unwrap()
    }

    #[test]
    fn inactive_without_patch_declaration() {
        let mut s = ApplyPatchResponsesState::new(Format::OpenAIResponse, b"{}", br#"{"tools":[]}"#);
        assert!(!s.active());
        let (out, err) = s.stream(b"data: {\"type\":\"response.created\"}");
        assert_eq!(out, vec![b"data: {\"type\":\"response.created\"}".to_vec()]);
        assert!(err.is_none());
    }

    #[test]
    fn premature_eof_and_done_fail_once() {
        let mut s = ApplyPatchResponsesState::new(Format::OpenAIResponse, &decl(), &decl());
        assert!(s.active());
        let (events, err) = s.finish_stream();
        assert_eq!(err.as_deref(), Some("incomplete apply_patch source response received from upstream"));
        assert_eq!(events.len(), 1);
        assert!(events[0].starts_with(b"data: ") && events[0].ends_with(b"\n\n"));
        let frame = String::from_utf8(events[0].clone()).unwrap();
        assert!(frame.contains("response.failed"));
        // The failure is one-shot.
        assert_eq!(s.finish_stream(), (Vec::new(), None));
        assert_eq!(s.stream(b"data: [DONE]"), (Vec::new(), None));
    }

    #[test]
    fn done_after_completion_passes_through() {
        let mut s = ApplyPatchResponsesState::new(Format::OpenAIResponse, &decl(), &decl());
        let completed = br#"{"type":"response.completed","response":{"id":"r","output":[]}}"#;
        let (out, err) = s.transform(completed);
        assert!(err.is_none());
        assert!(!out.is_empty());
        let (out, err) = s.stream(b"data: [DONE]");
        assert_eq!((out, err), (vec![b"data: [DONE]".to_vec()], None));
    }

    #[test]
    fn dispatcher_expansion_restores_child_call() {
        let decls = serde_json::to_vec(&json!({"tools":[
            {"type":"namespace","name":"ns","tools":[
                {"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"g"}}]}]}))
        .unwrap();
        let mut s = ApplyPatchResponsesState::new(Format::OpenAIResponse, &decls, &decls);
        s.add_dispatcher("ns_dispatch", "ns");
        let patch = "*** Begin Patch\n*** End Patch";
        let wrapper = json!({"name":"apply_patch","arguments":applypatch::wrap_input(patch)}).to_string();
        let done = json!({"type":"response.output_item.done","output_index":0,
            "item":{"type":"function_call","id":"fc_1","call_id":"c1","name":"ns_dispatch","arguments":wrapper}});
        let bytes = serde_json::to_vec(&done).unwrap();
        s.remember_dispatcher_event(&bytes);
        let (out, err) = s.transform(&bytes);
        assert!(err.is_none(), "{err:?}");
        let joined = out.iter().map(|e| String::from_utf8_lossy(e).into_owned()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("custom_tool_call"), "{joined}");
        assert!(joined.contains("apply_patch"));
    }
}
