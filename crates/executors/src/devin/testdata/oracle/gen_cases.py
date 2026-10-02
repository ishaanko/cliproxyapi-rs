# Builds cases.json, the inputs of the Go oracle (devin_oracle_test.go.txt).
#
# Re-recording ../golden.json (run from a scratch directory holding these files):
#   1. python3 gen_cases.py                       -> cases.json
#   2. copy devin_oracle_test.go.txt to internal/runtime/executor/devin_oracle_test.go of a copy
#      of the Go repo and run (GOFLAGS=-mod=mod):
#        DEVIN_ORACLE_IN=$PWD/cases.json DEVIN_ORACLE_OUT=$PWD/golden.json \
#          go test ./internal/runtime/executor -run TestDevinOracle -count=1
#   3. python3 finalize.py                        -> trims and writes ../golden.json
import json, gzip, struct, base64

def varint(v):
    out = bytearray()
    while v >= 0x80:
        out.append((v & 0x7f) | 0x80)
        v >>= 7
    out.append(v)
    return bytes(out)

def fb(n, b):
    if isinstance(b, str):
        b = b.encode()
    return varint((n << 3) | 2) + varint(len(b)) + b

def fv(n, v):
    return varint((n << 3) | 0) + varint(v)

def f64(n, d):
    return varint((n << 3) | 1) + struct.pack('<d', d)

def f32(n, f):
    return varint((n << 3) | 5) + struct.pack('<f', f)

def tc(id=None, name=None, args=None, invalid=None, err=None, custom=None):
    b = b''
    if id is not None: b += fb(1, id)
    if name is not None: b += fb(2, name)
    if args is not None: b += fb(3, args)
    if invalid is not None: b += fb(4, invalid)
    if err is not None: b += fb(5, err)
    if custom is not None: b += fv(6, custom)
    return fb(6, b)

def usage(prompt=None, completion=None, write=None, cached=None, status=None, headers=(), model=None, raw8=None):
    b = b''
    if prompt is not None: b += fv(2, prompt)
    if completion is not None: b += fv(3, completion)
    if write is not None: b += fv(4, write)
    if cached is not None: b += fv(5, cached)
    if status is not None: b += fv(6, status)
    for k, v in headers:
        b += fb(8, fb(1, k) + fb(2, v))
    if raw8 is not None: b += fb(8, raw8)
    if model is not None: b += fb(9, model)
    return fb(7, b)

def dim_group(title, metrics):
    g = fb(1, title)
    for k, v in metrics:
        g += fb(2, fb(4, f32(2, v)) + fb(5, k))
    return fb(28, g)

def fr(payload, flag=0):
    return {"flag": flag, "hex": payload.hex()}

EOS = lambda j='{}': fr(j.encode(), 2)

# ---------------------------------------------------------------- wire
def P(mid, source, content='', **kw):
    d = {"message_id": mid, "source": source, "content": content, "images": [], "tool_calls": [],
         "tool_call_id": "", "original_tool_call_id": "", "is_orphaned_tool": False, "thinking": "",
         "signature_b64": "", "signature_type": ""}
    d.update(kw)
    return d

def W(name, **kw):
    d = {"name": name, "session_token": "token-123", "device_seed": "device-seed-1", "chat_model_uid": "swe-2-high",
         "system_prompt": "", "prompts": [], "tools": [], "temperature": None, "max_tokens": 4000,
         "session_id": "session-1", "cascade_id": "cascade-1", "sensitive_words": [], "repeat": 1}
    d.update(kw)
    return d

b64 = lambda s: base64.b64encode(s.encode()).decode()
wire = [
    W("basic", system_prompt="you are a helpful assistant", temperature=0.7,
      prompts=[P("msg-1", 1, "hello"),
               P("msg-2", 2, "hi there", thinking="thinking step", signature_b64=b64("sealed.v1.test"), signature_type="sealed"),
               P("msg-3", 4, '{"result":"ok"}', tool_call_id="call-1")],
      tools=[{"name": "get_weather", "description": "lookup weather", "parameters": '{"type":"object"}'}]),
    W("images_and_defaults", chat_model_uid="swe-2-max", system_prompt="assistant instructions", temperature=0.0, max_tokens=0,
      prompts=[P("msg-img-1", 1, "[Image 1: pasted_image_1.png]\n\nocr this image",
                 images=[{"base64_data": "iVBORw0KGgoAAAANSUhEUgAA...", "mime_type": "image/png"},
                         {"base64_data": "  QUJD  ", "mime_type": ""},
                         {"base64_data": "   ", "mime_type": "image/jpeg"}])]),
    W("sensitive_words", system_prompt="System prompt containing SECRET_TOKEN and secret_token again", sensitive_words=["SECRET_TOKEN"],
      prompts=[P("msg-1", 2, "calling tool with SECRET_TOKEN in content",
                 tool_calls=[{"id": "call-1", "name": "test_tool", "arguments": '{"key":"SECRET_TOKEN"}'},
                             {"id": "", "name": "", "arguments": ""}])],
      max_tokens=100, session_token="tok", device_seed="seed", session_id="s", cascade_id="c"),
    W("tools_filtered", system_prompt="system prompt", chat_model_uid="swe-2", max_tokens=1000, prompts=[],
      tools=[{"name": "mcp__codex_app__automation_update", "description": "Recurring automations", "parameters": '{"type":"object"}'},
             {"name": "exec_command", "description": "Runs a command in a bash shell, returning output or a session ID for ongoing interaction.", "parameters": '{"type":"object"}'},
             {"name": "write_stdin", "description": "Writes characters to an existing unified exec session and returns recent output.", "parameters": '{"type":"object"}'},
             {"name": "task", "description": "Takes a task_id parameter identifying the task. More.", "parameters": ""},
             {"name": "", "description": "nameless", "parameters": "{}"},
             {"name": "empty_desc", "description": "", "parameters": ""}]),
    W("turn_sequence", prompts=[P("u1", 1, "hi")], system_prompt="", session_id="sess-sequential", cascade_id="casc-1", repeat=3,
      session_token="tok", device_seed="seed", max_tokens=1000),
    W("user_boundary_tool_continuation", system_prompt="", session_id="sess-tool", cascade_id="casc-tool", max_tokens=1000,
      prompts=[P("u1", 1, "read file"), P("a1", 2, "calling tool"), P("t1", 4, "file content", tool_call_id="call_1")]),
    W("user_boundary_two_users_repeat", system_prompt="", session_id="sess-two", cascade_id="casc-two", max_tokens=1000, repeat=2,
      prompts=[P("u1", 1, "a"), P("u2", 1, "b")]),
    W("source_default_and_cascade_default", session_id="sess-default", cascade_id="", max_tokens=-5, temperature=1.5,
      prompts=[P("m1", 0, "x")], system_prompt="x"),
    W("sanitized_system", system_prompt="x-anthropic-billing-header: cc_version=2.1;\nYou are Claude Code, Anthropic's official CLI for Claude.\nkeep me\r\nClaude Code is available as a CLI in your terminal\nlast line  ",
      prompts=[P("u", 1, "hi")], session_id="sess-sys", cascade_id="c"),
    W("unicode_seed_and_token", session_token="toké", device_seed="seed-你好", system_prompt="你好", prompts=[P("u", 1, "你好 \U0001F600")],
      session_id="sess-uni", cascade_id="c-uni"),
]

sanitize = [
    {"prompt": "x-anthropic-billing-header: cc_version=2.1.260;\nYou are Claude Code, Anthropic's official CLI for Claude.\nHelp the project with API and proxy.", "words": ["API", "proxy"]},
    {"prompt": "line one\r\nThis is for authorized security testing only\nkeep\ndestructive techniques, DoS attacks are bad\nFast mode for Claude Code is on\nCodex refers to the open-source agentic coding interface.\n- Don’t output ANSI escape codes directly — the CLI renderer applies them.\nEnd", "words": []},
    {"prompt": "\n\n  \n", "words": ["foo"]},
    {"prompt": "Foo FOO foo Bar baz", "words": ["foo", "ba", "a"]},
    {"prompt": "drop this secret line\nkeep that", "words": ["secret line"]},
    {"prompt": "café ÉCOLE école", "words": ["école"]},
    {"prompt": "a.b a-b", "words": ["a.b"]},
]

trailers = [json.dumps(x) if not isinstance(x, str) else x for x in [
    '', '   ', '{}', ' {} ', 'not json', '[]', '"x"', 'null', '{"error":null}', '{"error":"str"}', '{"error":{}}',
    '{"error":{"code":5,"message":"x"}}', '{"error":{"code":"internal","message":null}}',
    '{"Error":{"Code":"UNAUTHENTICATED","Message":"Bad"}}',
    {"error": {"code": "failed_precondition", "message": "User monthly ACU quota exhausted (trace ID: 12345)"}},
    {"error": {"code": "failed_precondition", "message": "Client version 3000.1.0 is no longer supported"}},
    {"error": {"code": "failed_precondition", "message": "credit"}},
    {"error": {"code": "unauthenticated", "message": "Invalid or expired session token"}},
    {"error": {"code": "invalid_argument", "message": "an internal error occurred (trace ID: fa43)"}},
    {"error": {"code": "invalid_argument", "message": "field 'model' cannot be empty"}},
    {"error": {"code": "internal", "message": "database timeout"}},
    {"error": {"code": "unknown_future_error", "message": "something unusual"}},
    {"error": {"code": "canceled", "message": "client canceled"}},
    {"error": {"code": "deadline_exceeded", "message": "upstream deadline exceeded"}},
    {"error": {"code": "permission_denied", "message": "The model is currently in high demand"}},
    {"error": {"code": "permission_denied", "message": "model access is not allowed"}},
    {"error": {"code": "permission_denied", "message": "HIGH DEMAND: please retry"}},
    {"error": {"code": "resource_exhausted", "message": "rate limit exceeded"}},
    {"error": {"code": "unavailable", "message": "try later"}},
    {"error": {"code": "", "message": "no code"}},
]]

models = [{"model": m, "level": l, "budget": b, "out": ""} for m, l, b in [
    ("", "", 0), ("   ", "high", 0), ("claude-fable-5-1-max", "", 0), ("swe-2-high", "", 0), ("swe-2", "", 0),
    ("swe-2", "minimal", 0), ("swe-2", "low", 0), ("swe-2", "xhigh", 0), ("swe-2", "max", 0), ("swe-2(max)", "", 0),
    ("swe-2:high", "", 0), ("swe-2:low", "max", 0), ("DEVIN/swe-2", "", 0), ("devin/swe-2(medium)", "low", 0),
    ("claude-sonnet-4-5", "", 0), ("claude-sonnet-4-5", "high", 0), ("claude.sonnet.4.5", "", 0), ("claude-sonnet-4-5-thinking", "", 0),
    ("claude-haiku-4-5", "high", 0), ("gpt-4-1", "", 0), ("gpt-4.1", "high", 0),
    ("model_gpt_5_2", "", 0), ("model_gpt_5_2", "high", 0), ("model-gpt-5-2", "max", 0), ("model_gpt_5_2", "minimal", 0), ("MODEL_GPT_5_2", "none", 0),
    ("model_google_gemini_3_0_flash", "", 0), ("model_google_gemini_3_0_flash", "xhigh", 0), ("model_google_gemini_3_0_flash", "none", 0),
    ("model_claude_4_5_opus", "", 0), ("model_claude_4_5_opus", "high", 0), ("model_claude_4_5_opus", "none", 0),
    ("gemini-3-flash", "", 0), ("gemini-3-flash", "medium", 0), ("gemini-3.8-flash", "low", 0),
    ("swe-1-7", "", 0), ("swe-1-7", "medium", 0), ("swe-1-7", "high", 0), ("swe-1-6", "fast", 0), ("swe-1-6", "", 0),
    ("glm-5-2", "", 0), ("glm-5-2", "none", 0), ("glm-5-2", "max", 0), ("glm-5-2", "low", 0), ("glm-5-2-1m", "none", 0), ("glm-5-2-1m", "max", 0), ("glm-5-2-1m", "", 0),
    ("claude-opus-4-6", "", 0), ("claude-opus-4-6", "high", 0), ("claude-opus-4-6", "none", 0), ("claude-sonnet-4-6", "medium", 0),
    ("claude-opus-4-6-1m", "high", 0), ("claude-opus-4-6-1m", "", 0), ("claude-sonnet-4-6-1m", "high", 0), ("claude-sonnet-4-6-1m", "off", 0),
    ("swe-2", "", 1000), ("swe-2", "", 8000), ("swe-2", "", 20000), ("swe-2", "", 40000), ("swe-2", "auto", 0), ("swe-2", "adaptive", 0), ("swe-2", "disabled", 0),
    ("gpt-5-6-sol", "", 0), ("gpt-5-6-sol", "high", 0), ("gpt-5-6-sol", "none", 0), ("gpt-5-6-sol", "xhigh", 0),
    ("totally-unknown-model", "", 0), ("totally-unknown-model", "high", 0), ("totally.unknown", "", 0),
    ("foo-high", "", 0), ("foo_thinking", "low", 0), ("foo-low-fast", "", 0), ("devin/foo-thinking-1m", "", 0), ("Foo-MAX", "", 0),
    ("swe-2( high )", "", 0), ("swe-2()", "", 0), ("swe-2:", "", 0), (":high", "", 0),
]]

# ---------------------------------------------------------------- parse
def C(t, **kw):
    d = {"type": "text", "text": t}
    d.update(kw)
    return d

parse = []
def PA(name, payload, original=''):
    parse.append({"name": name, "payload": payload if isinstance(payload, str) else json.dumps(payload, separators=(",", ":")), "original": original if isinstance(original, str) else json.dumps(original, separators=(",", ":")), "out": None})

PA("standard", {
    "system_instruction": "You are a helpful coding assistant.",
    "generation_config": {"temperature": 0.8, "max_output_tokens": 16000, "thinking_level": "high"},
    "previous_interaction_id": "session-uuid-1",
    "input": [
        {"type": "user_input", "content": [C("hello")]},
        {"type": "thought", "content": [C("planning...")], "signature": "c2VhbGVkLnYxLnRlc3Q="},
        {"type": "model_output", "content": [C("I can help with that.")]},
        {"type": "function_call", "name": "read_file", "id": "call_1", "arguments": {"path": "main.go"}},
        {"type": "function_result", "call_id": "call_1", "result": "package main\n"},
    ],
    "tools": [{"name": "read_file", "description": "Read file content", "parameters": {"type": "object"}}],
})
PA("camel_case_and_precedence", {
    "systemInstruction": "  sys  ", "generationConfig": {"temperature": 0, "max_output_tokens": 0, "thinking_config": {"thinking_budget": 9000}},
    "sessionId": "  sess-camel ", "conversation_id": "conv", "previous_interaction_id": "prev",
    "input": [{"type": "user_input", "text": "plain text field"}],
})
PA("multiple_thoughts_zero_temp", {
    "generation_config": {"temperature": 0},
    "input": [{"type": "user_input", "content": "string content"},
              {"type": "thought", "content": [C("one")]}, {"type": "thought", "content": [C("two")], "thought_signature": "claude#CAQSabc"},
              {"type": "model_output", "content": [C("a"), C("b")]}, {"type": "model_output", "text": "c", "signature": "gpt#gAAAAxyz"},
              {"type": "function_call", "name": "t1", "call_id": "c1", "arguments": "{\"x\":1}"},
              {"type": "function_call", "name": "t2", "id": "c2", "arguments": {"y": [1, 2]}},
              {"type": "function_call", "name": "t3"},
              {"type": "function_result", "id": "c2", "output": {"content": "wrapped"}},
              {"type": "function_result", "call_id": "", "content": [C("anon")]},
              {"type": "function_result", "call_id": "unknown", "result": "orphan"},
              {"type": "function_result", "call_id": "c1"}]
})
PA("temperature_from_original", {"input": [{"type": "user_input", "content": [C("hi")]}], "temperature": 0.3},
   {"temperature": 0.9, "session_id": "orig-sess"})
PA("temperature_from_translated", {"input": [{"type": "user_input", "content": [C("hi")]}], "temperature": 0.3}, {"model": "m"})
PA("images", {"input": [{"type": "user_input", "content": [
    C("look"),
    {"type": "image", "data": "AAA", "mime_type": "image/jpeg"},
    {"type": "input_image", "source": {"data": "BBB", "media_type": "image/webp"}},
    {"type": "image_url", "image_url": {"url": "data:image/gif;base64,CCC"}},
    {"type": "image_url", "image_url": "data:;base64,DDD"},
    {"type": "image", "inline_data": {"data": "EEE", "mime_type": "image/png"}},
    {"type": "image", "url": "https://example.com/x.png"},
    {"type": "image_url", "image_url": {"url": "data:image/png;base64"}},
]}]})
PA("tool_results_structured", {"input": [
    {"type": "function_call", "name": "a", "id": "c1", "arguments": {}},
    {"type": "function_call", "name": "b", "id": "c2", "arguments": {}},
    {"type": "function_call", "name": "c", "id": "c3", "arguments": {}},
    {"type": "function_call", "name": "d", "id": "c4", "arguments": {}},
    {"type": "function_call", "name": "e", "id": "c5", "arguments": {}},
    {"type": "function_result", "call_id": "c1", "result": [C("part1"), C("part2"), {"type": "image", "data": "IMG", "mime_type": "image/png"}]},
    {"type": "function_result", "call_id": "c2", "result": {"type": "tool_result", "tool_use_id": "t", "content": "inner"}},
    {"type": "function_result", "call_id": "c3", "result": [{"status": "ok", "n": 1}, "str", 5]},
    {"type": "function_result", "call_id": "c4", "result": {"type": "text", "text": "business", "extra": 1}},
    {"type": "function_result", "call_id": "c5", "result": {"result": {"output": [C("deep")]}, "cache_control": {"type": "ephemeral"}}},
    {"type": "function_result", "call_id": "x", "result": ""},
    {"type": "function_result", "call_id": "y", "result": [C("only", cache_control={"type": "ephemeral"}), C("mixed", foo=1)]},
    {"type": "function_result", "call_id": "z", "result": 42},
    {"type": "function_result", "call_id": "w", "result": None},
]})
PA("messages_fallback", {"messages": [
    {"role": "system", "content": "sys msg"}, {"role": "developer", "content": "ignored dev"},
    {"role": "user", "content": [C("q"), {"type": "image_url", "image_url": {"url": "data:image/png;base64,XYZ"}}]},
    {"role": "assistant", "content": "calling", "tool_calls": [
        {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"a\":1}"}},
        {"call_id": "call_2", "name": "other", "arguments": {"b": 2}}, {"id": "call_3", "function": {"name": "none"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "result one"},
    {"role": "tool", "id": "call_2", "content": [C("result two")]},
    {"role": "tool", "content": "no id"},
    {"role": "tool", "tool_call_id": "zzz", "content": "orphan"},
    {"role": "weird", "content": "x"},
]})
PA("tools_variants", {"input": [], "tools": [
    {"name": "plain", "description": "d", "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}},
    {"function_declarations": [{"name": "fd1", "description": "x", "parameters": {"type": "object"}}, {"name": "", "description": "skip"}]},
    {"functionDeclarations": [{"name": "fd2", "parametersJsonSchema": {"type": "object", "title": "T"}}]},
    {"type": "namespace", "name": "mcp__codex_app", "tools": [{"name": "automation_update"}, {"name": "ok_child", "description": "child", "parameters": {"type": "object"}}]},
    {"type": "namespace", "name": "MCP__CODEX_APP", "children": [{"name": "Automation_Update"}, {"name": "kid2"}]},
    {"type": "namespace", "name": "other_ns", "tools": [{"name": "nested"}]},
    {"name": "mcp__codex_app__automation_update", "description": "drop"},
    {"name": "exec_command", "description": "Runs a command in a bash shell, returning output or a session ID for ongoing interaction."},
    {"description": "no name"},
    {"name": "noparams"},
]})
PA("original_supplements_claude", {"input": [
    {"type": "user_input", "content": [C("first")]},
    {"type": "thought", "content": [C("t1")]},
    {"type": "model_output", "content": [C("a1")]},
    {"type": "function_call", "name": "tool", "id": "toolu_1", "arguments": {}},
    {"type": "function_result", "call_id": "toolu_1", "result": "text only"},
    {"type": "function_result", "call_id": "toolu_gone", "result": "orphan text"},
    {"type": "user_input", "content": [C("second")]},
    {"type": "model_output", "content": [C("a2")]},
]}, {"messages": [
    {"role": "user", "content": [{"type": "text", "text": "first"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "USERIMG"}}]},
    {"role": "assistant", "content": [{"type": "thinking", "thinking": "orig thought", "signature": "sealed.v1.origsig"}, {"type": "text", "text": "a1"}, {"type": "tool_use", "id": "toolu_1", "name": "tool", "input": {}}]},
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "text only"}, {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "TOOLIMG"}}]},
                                 {"type": "tool_result", "tool_use_id": "toolu_gone", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "GONEIMG"}}]}]},
    {"role": "user", "content": [{"type": "text", "text": "second"}]},
    {"role": "assistant", "content": [{"type": "thinking", "thinking": "second thought", "signature": "AYsignature"}, {"type": "text", "text": "a2"}]},
]})
PA("original_supplements_openai_tool", {"input": [
    {"type": "function_call", "name": "tool", "id": "call_1", "arguments": {}},
    {"type": "function_result", "call_id": "call_1", "result": "ok"},
]}, {"messages": [{"role": "tool", "tool_call_id": "call_1", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,TOOL1"}}]}]})
PA("no_session_anywhere", {"input": [{"type": "user_input", "content": [C("x")]}]})
PA("not_json", "not json at all", "also not json")
PA("input_not_array_messages_not_array", {"input": "text", "messages": "x", "tools": "x"})

# ---------------------------------------------------------------- signatures
sigs = [{"in": s, "bytes_b64": "", "type": "", "detect": ""} for s in [
    "", "   ", "sealed.v1.abcde12345", "CAQStest12345", "claude#CAQStest12345", "gpt#gAAAAxx", "gemini#AYxx", "gAAAAABk1234567890",
    "AY12345", "c2VhbGVkLnYxLnRlc3Q=", base64.b64encode(b"CAQSxxxxxxxx").decode(), base64.b64encode(b"gAAAAxxxxx").decode(),
    base64.b64encode(b"\x01\x02\x03").decode(), base64.b64encode(b"CAIS12345678").decode(), "random-unknown-signature",
    "AAAA", "dGVzdA==", "dGVzdA", "  sealed.v1.padded  ", "line\nbreak", "Zm9v\nYmFy",
]]

uuids = [{"in": s, "out": ""} for s in [
    "", "  ", "123e4567-e89b-12d3-a456-426614174000", "  123e4567-e89b-12d3-a456-426614174000 ", "123E4567-E89B-12D3-A456-426614174000",
    "lcp:abc123", "conv:xyz", "session-uuid-1", "urn:uuid:123e4567-e89b-12d3-a456-426614174000", "123e4567e89b12d3a456426614174000",
    "{123e4567-e89b-12d3-a456-426614174000}", "not a uuid 你",
]]
# uuids with empty input generate random uuids; drop them from golden
uuids = [u for u in uuids if u["in"].strip() != ""]

# ---------------------------------------------------------------- streams
REQ = {
    "interactions": '{"model":"devin/swe-2","stream":true,"input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}]}',
    "openai": '{"model":"devin/swe-2","stream":true,"messages":[{"role":"user","content":"hi"}]}',
    "claude": '{"model":"devin/swe-2","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hello world how are you"}],"system":"be nice"}',
    "openai-response": '{"model":"devin/swe-2","stream":true,"input":"hi"}',
    "gemini": '{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}',
}

streams = []
def S(name, frames, req=None):
    streams.append({"name": name, "frames": frames, "requests": dict(REQ, **(req or {})), "interactions": "", "consume_err": "", "consume_code": 0, "formats": []})

S("text_only", [fr(fb(1, "id") + fb(3, "Hello ")), fr(fb(3, "world") + fv(5, 2)),
                fr(usage(prompt=10, completion=5, cached=3)), EOS()])
S("thinking_sig_then_content", [fr(fb(9, "think a") + fb(10, "sealed.v1.abc") + fb(21, "sealed")), fr(fb(9, " more")),
                                fr(fb(3, "answer")), EOS()])
S("interleaved_thinking_content", [fr(fb(9, "thought 1")), fr(fb(3, "content 1")), fr(fb(9, "thought 2")), fr(fb(3, "content 2")), EOS()])
S("tool_call_single", [fr(tc("toolu_1", "bash", '{"comm')), fr(tc("toolu_1", None, 'and":"ls"}')),
                       fr(usage(prompt=100, completion=50, write=7, cached=20, status=0, headers=[("x-request-id", "req_1"), ("openai-version", "2020")], model="swe-2-high") + fv(5, 10)),
                       EOS()])
S("sequential_tools_diff_id", [fr(tc("title_0", "title", '{"title":"Triage"}')), fr(tc("bash_1", "bash", '{"command":"gh issue view"}')), EOS()])
S("tool_then_post_text", [fr(tc("t1", "bash", "{}")), fr(fb(3, "done ") + fb(3, "now")), EOS()])
S("pre_text_tool_post_text", [fr(fb(3, "let me look")), fr(tc("t1", "bash", '{"a":1}')), fr(fb(3, "found it")), EOS()])
S("thinking_then_tool_same_frame", [fr(fb(9, "plan") + tc("t1", "bash", '{"c":1}') + fb(3, "text after")), EOS()])
S("thinking_pending_then_more_thinking", [fr(fb(9, "p1")), fr(tc("t1", "bash", "{}")), fr(fb(3, "buffered")), fr(fb(9, "p2")), fr(fb(3, "tail")), EOS()])
S("tool_no_id_continuation", [fr(tc("t1", "bash", '{"x"')), fr(tc(None, None, ':1}')), EOS()])
S("tool_late_name", [fr(tc("t1", None, None)), fr(tc("t1", "later_name", '{"k":"v"}')), EOS()])
S("tool_no_id_first", [fr(tc(None, "anon", '{"k":1}')), fr(tc("late_id", None, None)), EOS()])
S("trailer_error_immediate", [EOS(json.dumps({"error": {"code": "failed_precondition", "message": "User monthly ACU quota exhausted"}}))])
S("trailer_error_mid_stream", [fr(fb(3, "partial")), EOS(json.dumps({"error": {"code": "resource_exhausted", "message": "rate limit"}}))])
S("trailer_error_after_thinking", [fr(fb(9, "thinking") + fb(10, "sig")), EOS(json.dumps({"error": {"code": "unavailable", "message": "later"}}))])
S("truncated_no_eos", [fr(fb(3, "partial"))])
S("truncated_partial_frame", [fr(fb(3, "partial")), {"flag": 0, "hex": "", "raw_tail": "00000000ff"}])
S("empty_stream", [])
S("eos_only", [EOS()])
S("stop_max_tokens", [fr(fb(3, "partial") + fv(5, 3)), EOS()])
S("stop_incomplete", [fr(fb(3, "partial") + fv(5, 1)), EOS()])
S("stop_content_filter", [fr(fb(3, "blocked") + fv(5, 11)), EOS()])
S("stop_tool_calls", [fr(tc("t", "n", "{}") + fv(5, 10)), EOS()])
S("dimension_groups_usage", [fr(fb(3, "hi") + dim_group("Token Usage", [("input_tokens", 575.0), ("output_tokens", 5.0), ("cached_input_tokens", 128.0)])), EOS()])
S("dimension_fill_only_zero", [fr(usage(prompt=3, completion=9) + dim_group("Token Usage", [("input_tokens", 575.0), ("output_tokens", 5.0), ("cached_input_tokens", 128.0)])), EOS()])
S("usage_merge_across_frames", [fr(usage(prompt=5, headers=[("a", "1")], model="m1")), fr(usage(completion=7, cached=2, write=4, headers=[("b", "2"), ("Request-Id", "req_x")], model="m2")), EOS()])
S("custom_tool_invalid_json", [fr(tc("c1", "custom_bash", None, invalid="pwd && ls", err="syntax error", custom=1)), fr(tc("c1", None, None, invalid=" -la")), EOS()])
S("utf8_split_content", [fr(fb(3, b"\xe4\xbd")), fr(fb(3, b"\xa0\xe5\xa5")), fr(fb(3, b"\xbd!")), EOS()])
S("utf8_split_thinking_and_invalid", [fr(fb(9, b"\xf0\x9f")), fr(fb(9, b"\x98\x80 ok")), fr(fb(3, b"bad\xffbyte")), fr(fb(3, b"tail\xe4")), EOS()])
S("gzip_frame", [{"flag": 1, "hex": gzip.compress(fb(3, "compressed hello")).hex()}, EOS()])
S("gzip_eos", [fr(fb(3, "x")), {"flag": 3, "hex": gzip.compress(b'{}').hex()}])
S("bad_gzip", [{"flag": 1, "hex": "deadbeef"}])
S("invalid_flag", [fr(fb(3, "x")), {"flag": 4, "hex": "00"}])
S("signature_only", [fr(fb(10, "sealed.v1.only") + fb(21, "sealed")), fr(fb(3, "text")), EOS()])
S("late_signature_after_content", [fr(fb(9, "think")), fr(fb(3, "content")), fr(fb(10, "sealed.v1.late") + fb(21, "sealed")), EOS()])
S("signature_split_frames", [fr(fb(9, "t") + fb(10, "sealed.v1.")), fr(fb(10, "part2")), fr(fb(3, "c")), EOS()])
S("unknown_fields_and_bad_frames", [fr(fb(99, "unknown") + fb(3, "kept")), fr(b"\x0a\xff\xff"), fr(fv(2, 123) + fv(4, 9) + f64(12, 1.5) + f32(30, 1.0) + fb(3, "x")), EOS()])
S("timestamp_field_bytes", [fr(fb(2, fv(1, 1700000000)) + fb(17, "msg-id") + fb(3, "t")), EOS()])
S("many_tools_130", [fr(tc("t%d" % i, "fn%d" % i, '{"i":%d}' % i)) for i in range(130)] + [EOS()])
S("tool_args_html_and_unicode", [fr(tc("t1", "n", '{"html":"<b>&</b>","u":"你"}')), fr(fb(3, "<tag> & 你")), EOS()])
S("invalid_json_args_nonstream", [fr(tc("t1", "n", "not json {")), EOS()])
S("text_with_special_chars", [fr(fb(3, 'quote " backslash \\ newline \n tab \t ctrl \x01 emoji \U0001F600')), EOS()])
S("trailer_malformed_ignored", [fr(fb(3, "x")), EOS("not json")])
S("trailer_error_no_object", [fr(fb(3, "x")), EOS('{"error":"str"}')])
APPLY_PATCH_REQ = '{"model":"devin/swe-2","stream":true,"input":"hi","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: x"}}]}'
S("apply_patch_valid_call", [fr(tc("call_p1", "apply_patch", '{"input":"*** Begin Patch\\n*** Add File: a.txt\\n+hi\\n*** End Patch"}')), EOS()], {"openai-response": APPLY_PATCH_REQ})
S("apply_patch_legacy_invalid_json", [fr(tc("call_p2", "apply_patch", None, invalid="*** Begin Patch", custom=1)), EOS()], {"openai-response": APPLY_PATCH_REQ, "interactions": APPLY_PATCH_REQ})
S("apply_patch_truncated", [fr(tc("call_p3", "apply_patch", '{"input":"*** Begin'))], {"openai-response": APPLY_PATCH_REQ})
S("data_after_eos_ignored", [fr(fb(3, "x")), EOS(), fr(fb(3, "never"))])

# raw_tail support: handled in Go by appending raw bytes
from e2e_cases import e2e
out = {"e2e": e2e, "wire": wire, "sanitize": sanitize, "trailers": [{"payload": t, "status": 0, "err": ""} for t in trailers],
       "models": models, "parse": parse, "sigs": sigs, "uuids": uuids, "streams": streams}
json.dump(out, open('cases.json', 'w'), indent=1)
print(len(wire), len(models), len(parse), len(streams))
