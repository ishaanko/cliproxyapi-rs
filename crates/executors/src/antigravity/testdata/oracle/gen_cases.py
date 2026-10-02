#!/usr/bin/env python3
"""Generates the executor-level oracle input cases (inputs only; Go fills in `expect`)."""
import json, hashlib, sys

cases = []

def sse(*objs):
    return "".join("data: " + json.dumps(o, separators=(",", ":")) + "\n\n" for o in objs)

def chunk(parts, finish=None, usage=None, trace=None, model="gemini-3.7-flash", rid="resp-1", role="model"):
    resp = {"candidates": [{"content": {"role": role, "parts": parts}}], "modelVersion": model, "responseId": rid}
    if finish:
        resp["candidates"][0]["finishReason"] = finish
    if usage:
        resp["usageMetadata"] = usage
    out = {"response": resp}
    if trace:
        out["traceId"] = trace
    return out

USAGE = {"promptTokenCount": 11, "candidatesTokenCount": 5, "totalTokenCount": 16}
OK_STREAM = sse(chunk([{"text": "ok"}], "STOP", USAGE))
OK_JSON = json.dumps(chunk([{"text": "ok"}], "STOP", USAGE), separators=(",", ":"))

def add(name, kind, model, fmt, payload, upstream=None, status=200, **kw):
    if upstream is None:
        agg = any(x in model for x in ("claude", "gemini-3-pro", "gemini-3.1-flash-image"))
        upstream = OK_JSON if (kind == "execute" and not agg) else OK_STREAM
        if kind == "count":
            upstream = json.dumps({"totalTokens": 42})
    kw.pop("aggregated", None)
    c = {"name": name, "kind": kind, "model": model, "source_format": fmt, "payload": payload,
         "upstream": {"status": status, "body": upstream}}
    c.update(kw)
    cases.append(c)

def both(name, model, fmt, payload, **kw):
    add(name + "/stream", "stream", model, fmt, payload, **kw)

# ---------------------------------------------------------------- request shaping
OPENAI_SIMPLE = {"model": "m", "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hello"}], "max_tokens": 100000, "temperature": 0.3}
for model in ["gemini-3.7-flash", "gemini-3-pro-high", "claude-sonnet-4-5-thinking", "gemini-2.5-flash"]:
    both("openai-simple/" + model, model, "openai", OPENAI_SIMPLE)

TOOLS_OPENAI = {
    "model": "m",
    "messages": [{"role": "user", "content": "weather in paris?"}],
    "tools": [
        {"type": "function", "function": {"name": "get_weather", "description": "weather", "parameters": {
            "type": "object", "properties": {
                "city": {"type": "string", "description": "city", "minLength": 2, "pattern": "^[a-z]+$"},
                "unit": {"type": "string", "enum": ["c", "f"], "default": "c"},
                "opts": {"anyOf": [{"type": "object", "properties": {"x": {"type": "integer", "format": "int32"}}}, {"type": "null"}]},
                "ref": {"$ref": "#/$defs/Thing"}},
            "required": ["city"], "additionalProperties": False, "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {"Thing": {"type": "object", "properties": {"id": {"type": "string"}}}}}}},
        {"type": "function", "function": {"name": "noop", "description": "no args", "parameters": {"type": "object", "properties": {}}}},
    ],
    "response_format": {"type": "json_schema", "json_schema": {"name": "r", "schema": {"type": "object", "properties": {"a": {"type": "string", "title": "A"}}, "additionalProperties": False}}},
}
for model in ["gemini-3.7-flash", "gemini-3-pro-high", "gemini-3.1-pro-low", "claude-sonnet-4-5-thinking"]:
    both("openai-tools/" + model, model, "openai", TOOLS_OPENAI)

CLAUDE_TOOLS = {
    "model": "claude-sonnet-4-5", "max_tokens": 4096, "system": [{"type": "text", "text": "You are helpful. secret-word here."}],
    "messages": [{"role": "user", "content": [{"type": "text", "text": "list files"}]}],
    "tools": [{"name": "bash", "description": "run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}],
    "thinking": {"type": "enabled", "budget_tokens": 2048},
    "stream": True,
}
both("claude-tools/claude", "claude-sonnet-4-5-thinking", "claude", CLAUDE_TOOLS)
both("claude-tools/gemini", "gemini-3.7-flash", "claude", CLAUDE_TOOLS)
both("claude-tools/sensitive", "gemini-3.7-flash", "claude", CLAUDE_TOOLS, sensitive_words=["secret-word", "helpful"])
both("claude-tools/suffix", "claude-sonnet-4-5-thinking(8000)", "claude", CLAUDE_TOOLS)
both("claude-tools/credits", "claude-sonnet-4-5-thinking", "claude", CLAUDE_TOOLS, credits_enabled=True, credits_requested=True)
both("claude-tools/credits-off", "claude-sonnet-4-5-thinking", "claude", CLAUDE_TOOLS, credits_enabled=False, credits_requested=True)

CLAUDE_THINKING_HISTORY = {
    "model": "claude-sonnet-4-5", "max_tokens": 1024,
    "messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "bogus"}, {"type": "text", "text": "hello"}]},
        {"role": "user", "content": "again"},
    ],
}
both("claude-thinking-history/gemini", "gemini-3.7-flash", "claude", CLAUDE_THINKING_HISTORY)
both("claude-thinking-history/claude", "claude-sonnet-4-5-thinking", "claude", CLAUDE_THINKING_HISTORY)

GEMINI_NATIVE = {
    "contents": [{"role": "model", "parts": [{"text": "I begin"}]}, {"role": "user", "parts": [{"text": "go on"}]}, {"role": "model", "parts": [{"text": "done"}]}],
    "systemInstruction": {"parts": [{"text": "sys"}]},
    "generationConfig": {"maxOutputTokens": 999999, "temperature": 0.1, "responseMimeType": "application/json",
                         "responseSchema": {"type": "object", "properties": {"a": {"type": "string", "title": "A", "default": "x"}}, "additionalProperties": False}},
    "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}],
    "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
    "tools": [{"functionDeclarations": [{"name": "f", "description": "d", "parametersJsonSchema": {"type": "object", "properties": {"n": {"type": "integer", "exclusiveMinimum": 0}}}}]}],
}
for model in ["gemini-3.7-flash", "claude-sonnet-4-5-thinking"]:
    both("gemini-native/" + model, model, "gemini", GEMINI_NATIVE)

RESPONSES_HISTORY = {
    "model": "m",
    "input": [
        {"type": "function_call", "call_id": "call-1", "name": "run", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "call-1", "output": "ok"},
    ],
    "tools": [{"type": "function", "name": "run", "description": "d", "parameters": {"type": "object", "properties": {}}}],
}
both("responses-history/gemini", "gemini-3.6-flash-high", "openai-response", RESPONSES_HISTORY)
both("responses-history/claude", "claude-sonnet-4-5-thinking", "openai-response", RESPONSES_HISTORY)
both("openai-leading-tool/gemini", "gemini-3.6-flash-high", "openai", {"messages": [
    {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "run", "arguments": "{}"}}]},
    {"role": "tool", "tool_call_id": "call-1", "content": "ok"}]})

both("image/model", "gemini-3.1-flash-image", "openai", {"messages": [{"role": "user", "content": "draw a cat"}]})
both("alt-param", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, alt="json")
both("derived-session", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, metadata={"derived_session_id": "ctx:v1:abc"})
both("derived-session/req", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, req_metadata={"derived_session_id": "ctx:v1:req"})
both("custom-ua-headers", "gemini-3.7-flash", "openai", OPENAI_SIMPLE,
     auth_attributes={"user_agent": "antigravity/hub/9.9.9 linux/amd64 extra", "header:X-Custom": "yes"})
both("base-url-trailing-slash", "gemini-3.7-flash", "openai", OPENAI_SIMPLE)

for fmt, payload in [("openai", OPENAI_SIMPLE), ("claude", CLAUDE_TOOLS), ("gemini", GEMINI_NATIVE)]:
    add("count/" + fmt, "count", "gemini-3.7-flash", fmt, payload)
add("count/claude-model", "count", "claude-sonnet-4-5-thinking", "claude", CLAUDE_TOOLS)
add("count/alt", "count", "gemini-3.7-flash", "gemini", GEMINI_NATIVE, alt="json")
add("count/error-429", "count", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, status=429,
    upstream=json.dumps({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "slow", "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "7s"}]}}))

# ---------------------------------------------------------------- responses
SIG = "CqcECqQEARFNMg8iZ3xVniKaulgBlzhJJDlc"
MULTI = sse(
    chunk([{"text": "thinking...", "thought": True}]),
    chunk([{"text": " more", "thought": True, "thoughtSignature": SIG}]),
    chunk([{"text": "Answer: "}]),
    chunk([{"text": "42"}], None, {"promptTokenCount": 20, "candidatesTokenCount": 7, "totalTokenCount": 40, "thoughtsTokenCount": 13}),
    chunk([{"functionCall": {"name": "get_weather", "args": {"city": "Paris", "n": 2}, "id": "call_1"}, "thoughtSignature": SIG + "2"}]),
    chunk([{"functionCall": {"name": "noop", "args": {}, "id": "call_2"}}], "STOP", {"promptTokenCount": 20, "candidatesTokenCount": 9, "totalTokenCount": 42, "thoughtsTokenCount": 13}),
)
for fmt, payload in [("openai", TOOLS_OPENAI), ("openai-response", RESPONSES_HISTORY), ("claude", CLAUDE_TOOLS), ("gemini", GEMINI_NATIVE)]:
    add("resp-multi/stream/" + fmt, "stream", "gemini-3.7-flash", fmt, payload, upstream=MULTI)
    add("resp-multi/aggregated/" + fmt, "execute", "gemini-3-pro-high", fmt, payload, upstream=MULTI)
    add("resp-multi/claude-model/" + fmt, "execute", "claude-sonnet-4-5-thinking", fmt, payload, upstream=MULTI)

NOFINISH = sse(chunk([{"thoughtSignature": SIG, "functionCall": {"name": "bash", "args": {"command": "ls"}, "id": "call_5"}}], None,
                     {"promptTokenCount": 100, "candidatesTokenCount": 17, "totalTokenCount": 220, "thoughtsTokenCount": 94}))
for fmt, payload in [("openai", OPENAI_SIMPLE), ("openai-response", RESPONSES_HISTORY), ("claude", CLAUDE_TOOLS), ("gemini", GEMINI_NATIVE)]:
    add("resp-nofinish/stream/" + fmt, "stream", "gemini-3.7-flash", fmt, payload, upstream=NOFINISH)
    add("resp-empty/stream/" + fmt, "stream", "gemini-3.7-flash", fmt, payload, upstream="")

SPLIT_USAGE = sse(
    chunk([{"text": "a"}], None, {"promptTokenCount": 5, "candidatesTokenCount": 1, "totalTokenCount": 6}, trace="t1"),
    chunk([{"text": "b"}], "STOP", None, trace="t1"),
    {"response": {"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7}}, "traceId": "t1"},
)
for fmt, payload in [("openai", OPENAI_SIMPLE), ("claude", CLAUDE_TOOLS), ("gemini", GEMINI_NATIVE)]:
    add("resp-split-usage/stream/" + fmt, "stream", "gemini-3.7-flash", fmt, payload, upstream=SPLIT_USAGE)

IMG = sse(chunk([{"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}], "STOP", USAGE))
add("resp-image/stream/openai", "stream", "gemini-3.1-flash-image", "openai", OPENAI_SIMPLE, upstream=IMG)
add("resp-image/execute/openai", "execute", "gemini-3.1-flash-image", "openai", OPENAI_SIMPLE, upstream=IMG)
add("resp-image/execute/claude", "execute", "gemini-3.1-flash-image", "claude", CLAUDE_TOOLS, upstream=IMG)

for fmt, payload in [("openai", OPENAI_SIMPLE), ("claude", CLAUDE_TOOLS), ("gemini", GEMINI_NATIVE)]:
    add("resp-json/execute/" + fmt, "execute", "gemini-2.5-flash", fmt, payload, upstream=json.dumps(chunk([{"text": "hi "}, {"text": "there"}], "STOP", USAGE)))
    add("resp-json/execute-no-usage/" + fmt, "execute", "gemini-2.5-flash", fmt, payload, upstream=json.dumps(chunk([{"text": "hi"}], "MAX_TOKENS")))

QUOTA = json.dumps({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED", "metadata": {"quotaResetDelay": "3h"}}]}})
RATE_SHORT = json.dumps({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "slow", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"}, {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "30s"}]}})
RATE_INSTANT = json.dumps({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "slow", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"}, {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "0.84s"}]}})
for kind in ["execute", "stream"]:
    add("err-429-quota/" + kind, kind, "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=429, upstream=QUOTA, auth_id="auth-q-" + kind)
    add("err-429-rate-instant/" + kind, kind, "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=429, upstream=RATE_INSTANT, auth_id="auth-i-" + kind)
    add("err-429-text/" + kind, kind, "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=429, upstream='{"error":{"message":"Please retry after 12s."}}', auth_id="auth-t-" + kind)
    add("err-500/" + kind, kind, "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=500, upstream="boom")
    add("err-401/" + kind, kind, "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=401, upstream='{"error":{"status":"UNAUTHENTICATED"}}')
    add("err-400-sig/" + kind, kind, "gemini-3.7-flash", "openai", OPENAI_SIMPLE, status=400, upstream='{"error":{"message":"Corrupted thought signature."}}')

# short cooldown: the first request records a cooldown, the second hits the precheck
add("cooldown/first", "stream", "gemini-2.5-flash", "openai", OPENAI_SIMPLE, status=429, upstream=RATE_SHORT, auth_id="auth-cd")
add("cooldown/second", "stream", "gemini-2.5-flash", "openai", OPENAI_SIMPLE, auth_id="auth-cd")
add("cooldown/other-model", "stream", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, auth_id="auth-cd")
add("cooldown/second-execute", "execute", "gemini-2.5-flash", "openai", OPENAI_SIMPLE, auth_id="auth-cd")
add("cooldown/bypass-credits", "stream", "gemini-2.5-flash", "openai", OPENAI_SIMPLE, auth_id="auth-cd", credits_enabled=True, credits_requested=True)

# ---------------------------------------------------------------- compaction
add("compact/alt", "execute", "gemini-3.7-flash", "openai-response",
    {"model": "m", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "work on x"}]}]},
    alt="responses/compact", upstream=OK_JSON)

# ---------------------------------------------------------------- replay sequences
def replay_turns(prefix, model, headers):
    tools = [{"type": "function", "function": {"name": "get_weather", "description": "w", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
    turn1 = {"model": "m", "messages": [{"role": "system", "content": "sys"}, {"role": "user", "content": "weather in paris?"}], "tools": tools}
    resp1 = sse(
        chunk([{"text": "let me think", "thought": True, "thoughtSignature": "SIGTHOUGHT1"}]),
        chunk([{"text": "Checking."}]),
        chunk([{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}, "id": "call_a"}, "thoughtSignature": "SIGCALL1"}]),
        chunk([{"functionCall": {"name": "get_weather", "args": {"city": "Lyon"}, "id": "call_b"}}], "STOP", USAGE),
    )
    add(prefix + "/turn1", "stream", model, "openai", turn1, upstream=resp1, headers=headers)
    turn2 = {"model": "m", "messages": [
        {"role": "system", "content": "sys"}, {"role": "user", "content": "weather in paris?"},
        {"role": "assistant", "content": "Checking.", "tool_calls": [
            {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
            {"id": "call_b", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Lyon\"}"}}]},
        {"role": "tool", "tool_call_id": "call_a", "content": "sunny"},
        {"role": "tool", "tool_call_id": "call_b", "content": "rainy"},
    ], "tools": tools}
    add(prefix + "/turn2", "stream", model, "openai", turn2, upstream=OK_STREAM, headers=headers)
    # context drift: the system prompt changed between turns
    turn2d = json.loads(json.dumps(turn2)); turn2d["messages"][0]["content"] = "sys changed"
    add(prefix + "/turn2-drift", "stream", model, "openai", turn2d, upstream=OK_STREAM, headers=headers)
    # bad tool id (client changed the call)
    turn2b = json.loads(json.dumps(turn2)); turn2b["messages"][2]["tool_calls"][0]["function"]["arguments"] = "{\"city\":\"Rome\"}"
    add(prefix + "/turn2-changed-args", "stream", model, "openai", turn2b, upstream=OK_STREAM, headers=headers)

replay_turns("replay-openai", "gemini-3.7-flash", {"Session-Id": "sess-openai-1"})

# claude client on a gemini model: tool ids are reserved provenance ids
def gemini_claude_id(call_id, name, args):
    canon = json.dumps(args, sort_keys=True, separators=(",", ":"), ensure_ascii=False).replace("<", "\\u003c").replace(">", "\\u003e").replace("&", "\\u0026")
    h = hashlib.sha256("\0".join([call_id, name, canon]).encode()).hexdigest()[:32]
    return "cpa_gemini_" + h

def claude_replay_turns(prefix, model, headers, system="You are an agent."):
    tools = [{"name": "bash", "description": "run", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}, "timeout": {"type": "integer", "default": 30}}, "required": ["command"]}}]
    base = {"model": "claude-sonnet-4-5", "max_tokens": 1024, "system": system, "tools": tools, "stream": True,
            "metadata": {"user_id": "user_abc_account__session_0a1b2c3d-1111-2222-3333-444455556666"}}
    turn1 = dict(base, messages=[{"role": "user", "content": "list and count"}])
    resp1 = sse(
        chunk([{"text": "plan", "thought": True, "thoughtSignature": "CLAUDESIGTHOUGHT"}]),
        chunk([{"functionCall": {"name": "bash", "args": {"command": "ls", "timeout": 30}, "id": "c1"}, "thoughtSignature": "CLAUDESIGCALL1"}]),
        chunk([{"functionCall": {"name": "bash", "args": {"command": "wc"}, "id": "c2"}}], "STOP", USAGE),
    )
    add(prefix + "/turn1", "stream", model, "claude", turn1, upstream=resp1, headers=headers)
    id1 = gemini_claude_id("c1", "bash", {"command": "ls", "timeout": 30})
    id2 = gemini_claude_id("c2", "bash", {"command": "wc"})
    turn2 = dict(base, messages=[
        {"role": "user", "content": "list and count"},
        {"role": "assistant", "content": [{"type": "text", "text": "plan"}, {"type": "tool_use", "id": id1, "name": "bash", "input": {"command": "ls"}}, {"type": "tool_use", "id": id2, "name": "bash", "input": {"command": "wc"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id1, "content": "a b"}, {"type": "tool_result", "tool_use_id": id2, "content": "2"}]},
    ])
    add(prefix + "/turn2", "stream", model, "claude", turn2, upstream=OK_STREAM, headers=headers)
    # same history, but the ledger lane differs (other Claude Code agent)
    h2 = dict(headers); h2["X-Claude-Code-Agent-Id"] = "agent-7"
    add(prefix + "/turn2-other-agent", "stream", model, "claude", turn2, upstream=OK_STREAM, headers=h2)
    # history with reserved ids but an empty ledger (new session): ids get degraded
    h3 = {"X-Claude-Code-Session-Id": "fresh-session-xyz"}
    add(prefix + "/turn2-fresh-session", "stream", model, "claude", turn2, upstream=OK_STREAM, headers=h3)
    add(prefix + "/turn2-fresh-session-execute", "execute", model, "claude", turn2, upstream=OK_JSON, headers=h3)

claude_replay_turns("replay-claude", "gemini-3.7-flash", {"X-Claude-Code-Session-Id": "cc-session-1"})
claude_replay_turns("replay-claude-meta", "gemini-3.7-flash", {})

# scope sources: execution session metadata, prompt cache key
for scope_name, kw in [("exec-session", {"metadata": {"execution_session_id": "exec-42"}}),
                       ("prompt-cache", {"__pck": True}),
                       ("derived", {"metadata": {"derived_session_id": "ctx:v1:d1"}})]:
    pck = kw.pop("__pck", False)
    tools = [{"type": "function", "function": {"name": "t", "description": "d", "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}}]
    t1 = {"messages": [{"role": "user", "content": "q-" + scope_name}], "tools": tools}
    t2 = {"messages": [{"role": "user", "content": "q-" + scope_name},
                       {"role": "assistant", "content": None, "tool_calls": [{"id": "k1", "type": "function", "function": {"name": "t", "arguments": "{\"x\":\"1\"}"}}]},
                       {"role": "tool", "tool_call_id": "k1", "content": "r"}], "tools": tools}
    if pck:
        t1["prompt_cache_key"] = "pck-1"; t2["prompt_cache_key"] = "pck-1"
    r1 = sse(chunk([{"functionCall": {"name": "t", "args": {"x": "1"}, "id": "k1"}, "thoughtSignature": "SIG-" + scope_name}], "STOP", USAGE))
    add("replay-scope-" + scope_name + "/turn1", "stream", "gemini-3.7-flash", "openai", t1, upstream=r1, **kw)
    add("replay-scope-" + scope_name + "/turn2", "stream", "gemini-3.7-flash", "openai", t2, upstream=OK_STREAM, **kw)

# gemini-native client replay with text signatures
g1 = {"contents": [{"role": "user", "parts": [{"text": "hello there"}]}]}
r_g1 = sse(chunk([{"text": "hmm", "thought": True}]), chunk([{"text": " ok", "thought": True, "thoughtSignature": "GTHOUGHTSIG"}]),
           chunk([{"text": "Hi!"}]), chunk([{"text": " there", "thoughtSignature": "GTEXTSIG"}], "STOP", USAGE))
add("replay-gemini/turn1", "stream", "gemini-3.7-flash", "gemini", g1, upstream=r_g1, headers={"Session-Id": "gem-1"})
g2 = {"contents": [{"role": "user", "parts": [{"text": "hello there"}]},
                   {"role": "model", "parts": [{"text": "hmm ok", "thought": True}, {"text": "Hi! there"}]},
                   {"role": "user", "parts": [{"text": "next"}]}]}
add("replay-gemini/turn2", "stream", "gemini-3.7-flash", "gemini", g2, upstream=OK_STREAM, headers={"Session-Id": "gem-1"})
g2d = json.loads(json.dumps(g2)); g2d["contents"][0]["parts"][0]["text"] = "hello changed"
add("replay-gemini/turn2-drift", "stream", "gemini-3.7-flash", "gemini", g2d, upstream=OK_STREAM, headers={"Session-Id": "gem-1"})

# invalid history: functionResponse without a call (400 from pairing validation)
add("invalid-history", "stream", "gemini-3.7-flash", "gemini", {"contents": [
    {"role": "user", "parts": [{"text": "q"}]},
    {"role": "user", "parts": [{"functionResponse": {"name": "x", "response": {"a": 1}}}]}]}, headers={"Session-Id": "inv-1"})

# gemini native parallel responses out of order + unknown names
GP = {"contents": [
    {"role": "user", "parts": [{"text": "go"}]},
    {"role": "model", "parts": [{"functionCall": {"name": "a", "args": {}, "id": "ida"}, "thoughtSignature": "s1"}, {"functionCall": {"name": "b", "args": {}, "id": "idb"}}]},
    {"role": "user", "parts": [{"functionResponse": {"name": "unknown", "id": "idb", "response": {"r": 2}}}, {"functionResponse": {"name": "a", "id": "ida", "response": {"r": 1}}}]}]}
add("parallel-responses-reorder", "stream", "gemini-3.7-flash", "gemini", GP)
add("parallel-responses-reorder/claude", "stream", "claude-sonnet-4-5-thinking", "gemini", GP)

# web search typed tool (claude) -> native googleSearch
WS = {"model": "claude-sonnet-4-5", "max_tokens": 100, "messages": [{"role": "user", "content": "search the web for rust"}],
      "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}]}
add("websearch/claude", "stream", "gemini-3.7-flash", "claude", WS)
add("websearch/claude-execute", "execute", "gemini-3-pro-high", "claude", WS)
add("websearch/responses", "stream", "gemini-3.7-flash", "openai-response", {"model": "m", "input": "search rust", "tools": [{"type": "web_search"}]})

# interactions client
add("interactions/stream", "stream", "gemini-3.7-flash", "interactions", {"model": "m", "input": "hello"}, response_format="interactions")

# original request differs from payload
add("original-payload", "stream", "gemini-3.7-flash", "openai", OPENAI_SIMPLE, original_payload=TOOLS_OPENAI)

# alt on execute
add("alt-param/execute", "execute", "gemini-2.5-flash", "gemini", GEMINI_NATIVE, alt="json")

import base64
SIG4959 = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"
carrier = "cpa-gemini-responses-carrier-v1:next:function:" + base64.b64encode(SIG4959.encode()).decode().rstrip("=")
ISSUE4959 = {"model": "gemini-3.7-flash-high", "input": [
    {"type": "reasoning", "id": "rs_resp_test_detached_before_0", "summary": [], "encrypted_content": carrier},
    {"type": "function_call", "call_id": "call_bash_1", "name": "Bash", "arguments": "{\"command\":\"true\"}"},
    {"type": "function_call_output", "call_id": "call_bash_1", "output": "ok"},
    {"role": "assistant", "content": [{"type": "output_text", "text": "first"}]},
    {"role": "assistant", "content": [{"type": "output_text", "text": "second"}]}]}
add("issue4959/gemini", "stream", "gemini-3.7-flash-high", "openai-response", ISSUE4959)
add("issue4959/claude", "stream", "claude-sonnet-4-5-thinking", "openai-response", ISSUE4959)

SPLIT_TERMINAL = ("data: " + json.dumps({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": "first"}]}, "finishReason": "STOP"}], "modelVersion": "gemini-3.7-flash", "responseId": "resp-split"}, "traceId": "trace-split"}, separators=(",", ":")) + "\n\n"
    + "data: " + json.dumps({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}}], "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 22, "totalTokenCount": 33}, "modelVersion": "gemini-3.7-flash", "responseId": "resp-split"}, "traceId": "trace-split"}, separators=(",", ":")) + "\n\n")
for fmt, payload in [("openai", OPENAI_SIMPLE), ("claude", CLAUDE_TOOLS), ("openai-response", RESPONSES_HISTORY), ("gemini", GEMINI_NATIVE)]:
    add("resp-split-terminal/" + fmt, "stream", "gemini-3.7-flash", fmt, payload, upstream=SPLIT_TERMINAL)

SUMMARY_JSON = json.dumps({"response": {"candidates": [{"content": {"parts": [{"text": "Summary of previous conversation"}], "role": "model"}}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}}})
COMPACT_IN = {"model": "gemini-3.7-flash", "stream": True, "input": [
    {"type": "message", "role": "user", "content": "synthetic context"},
    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "synthetic previous answer"}]},
    {"type": "compaction_trigger"}]}
add("compact/trigger-stream/gemini", "stream", "gemini-3.7-flash", "openai-response", COMPACT_IN, upstream=SUMMARY_JSON, response_format="openai-response")
add("compact/trigger-execute/gemini", "execute", "gemini-3.7-flash", "openai-response", COMPACT_IN, upstream=SUMMARY_JSON)
add("compact/trigger-stream/claude", "stream", "claude-sonnet-4-5-thinking", "openai-response", COMPACT_IN,
    upstream=sse(chunk([{"text": "Claude summary"}], "STOP", USAGE)))
add("compact/trigger-string-input", "execute", "gemini-3.7-flash", "openai-response", {"model": "m", "input": "plain string input", "tools": [{"type": "function", "name": "x"}]}, alt="responses/compact", upstream=SUMMARY_JSON)
add("compact/stream-compact-alt", "stream", "gemini-3.7-flash", "openai-response", COMPACT_IN, alt="responses/compact", upstream=SUMMARY_JSON)
add("compact/capsule-expansion", "stream", "gemini-3.7-flash", "openai-response",
    {"model": "m", "input": [{"type": "compaction", "encrypted_content": "__CAPSULE__"}, {"type": "message", "role": "user", "content": "continue"}]})
add("compact/capsule-expansion-execute", "execute", "gemini-3.7-flash", "openai-response",
    {"model": "m", "input": [{"type": "compaction", "encrypted_content": "__CAPSULE__"}, {"type": "message", "role": "user", "content": "continue"}]},
    original_payload={"model": "m", "input": [{"type": "compaction", "encrypted_content": "__CAPSULE__"}, {"type": "message", "role": "user", "content": "continue"}]})
add("compact/capsule-invalid", "stream", "gemini-3.7-flash", "openai-response",
    {"model": "m", "input": [{"type": "compaction", "encrypted_content": "garbage"}]})

json.dump(cases, sys.stdout, indent=1)
