import json

PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
e2e = []


def E(name, fmt, model, payload):
    e2e.append({"name": name, "source_format": fmt, "model": model,
                "payload": payload if isinstance(payload, str) else json.dumps(payload, separators=(",", ":")),
                "hex": "", "url": "", "err": ""})


E("claude_tool_use_result", "claude", "devin/swe-2", {"model": "devin/swe-2", "messages": [
    {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_abc_123", "name": "bash", "input": {"command": "ls"}}]},
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_abc_123", "content": "file.txt"}]}]})
E("openai_tool_call_result", "openai", "devin/swe-2", {"model": "devin/swe-2", "messages": [
    {"role": "assistant", "tool_calls": [{"id": "call_xyz_456", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"main.go\"}"}}]},
    {"role": "tool", "tool_call_id": "call_xyz_456", "content": "package main"}]})
E("responses_namespace_tools", "openai-response", "devin/gemini-3-7-flash", {"model": "devin/gemini-3-7-flash", "tools": [
    {"type": "function", "name": "exec_command", "description": "Execute a command", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
    {"type": "namespace", "name": "multi_agent_v1", "description": "Multi agent tools", "tools": [
        {"type": "function", "name": "close_agent", "description": "Close an agent", "parameters": {"type": "object", "properties": {"target": {"type": "string"}}}},
        {"type": "function", "name": "resume_agent", "description": "Resume an agent", "parameters": {"type": "object", "properties": {"id": {"type": "string"}}}}]}],
    "input": [{"type": "message", "role": "user", "content": "hello"}]})
E("responses_tools_filter_obfuscate", "openai-response", "devin/swe-2", {"model": "devin/swe-2", "tools": [
    {"type": "namespace", "name": "mcp__codex_app", "description": "Codex App tools", "tools": [
        {"type": "function", "name": "automation_update", "description": "Recurring automations", "parameters": {"type": "object", "properties": {"id": {"type": "string"}}}},
        {"type": "function", "name": "read_resource", "description": "Read a resource", "parameters": {"type": "object", "properties": {"uri": {"type": "string"}}}}]},
    {"type": "function", "name": "exec_command", "description": "Runs a command in a bash shell, returning output or a session ID for ongoing interaction.", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}},
    {"type": "function", "name": "write_stdin", "description": "Writes characters to an existing unified exec session and returns recent output.", "parameters": {"type": "object", "properties": {"session_id": {"type": "string"}}, "required": ["session_id"]}}],
    "input": [{"type": "message", "role": "user", "content": "hello"}]})
E("claude_tool_result_image", "claude", "devin/swe-2", {"model": "devin/swe-2", "messages": [
    {"role": "user", "content": "Inspect the screenshot returned by the tool."},
    {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_text_1", "name": "bash", "input": {"cmd": "pwd"}}, {"type": "tool_use", "id": "tool_image_1", "name": "screenshot", "input": {}}]},
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_text_1", "content": "/workspace"},
        {"type": "tool_result", "tool_use_id": "tool_image_1", "content": [{"type": "text", "text": "Captured window"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG}}]}]}]})
E("claude_full", "claude", "devin/claude-opus-4-6(high)", {"model": "devin/claude-opus-4-6", "max_tokens": 2048, "temperature": 0.4,
    "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=1;"}, {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}, {"type": "text", "text": "Be terse."}],
    "thinking": {"type": "enabled", "budget_tokens": 8000},
    "tools": [{"name": "bash", "description": "Run bash", "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}}],
    "messages": [
        {"role": "user", "content": [{"type": "text", "text": "look at this"}, {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}]},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "CAQSabcdef=="}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"cmd": "ls"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "a.txt"}]}, {"type": "text", "text": "thanks"}]}]})
E("claude_business_json_result", "claude", "devin/swe-2", {"model": "devin/swe-2", "messages": [
    {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_b", "name": "report", "input": {}}]},
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_b", "content": [{"type": "text", "text": "first"}, {"id": 42, "name": "report", "content": "body"}]}]}]})
E("openai_full", "openai", "devin/swe-2(medium)", {"model": "devin/swe-2", "temperature": 0.2, "max_tokens": 1000, "reasoning_effort": "high",
    "messages": [{"role": "system", "content": "Be helpful."}, {"role": "user", "content": [{"type": "text", "text": "see"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64," + PNG}}]},
                 {"role": "assistant", "content": "calling", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get", "arguments": "{\"a\":1}"}}]},
                 {"role": "tool", "tool_call_id": "call_1", "content": "result"}, {"role": "user", "content": "next"}],
    "tools": [{"type": "function", "function": {"name": "get", "description": "Get thing", "parameters": {"type": "object", "properties": {"a": {"type": "number"}}}}}]})
E("gemini_full", "gemini", "devin/gemini-3-flash", {"systemInstruction": {"parts": [{"text": "gem system"}]}, "generationConfig": {"temperature": 0.6, "maxOutputTokens": 500, "thinkingConfig": {"thinkingBudget": 5000}},
    "tools": [{"functionDeclarations": [{"name": "lookup", "description": "Look up", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}]}],
    "contents": [{"role": "user", "parts": [{"text": "hi"}, {"inlineData": {"mimeType": "image/png", "data": PNG}}]},
                 {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {"q": "x"}, "id": "fc1"}}]},
                 {"role": "user", "parts": [{"functionResponse": {"name": "lookup", "id": "fc1", "response": {"result": "found"}}}]}]})
E("responses_full", "openai-response", "devin/swe-2", {"model": "devin/swe-2", "instructions": "resp instructions", "temperature": 0.9, "max_output_tokens": 700,
    "reasoning": {"effort": "low"}, "input": [
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
        {"type": "function_call", "call_id": "fc_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
        {"type": "function_call_output", "call_id": "fc_1", "output": "out"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "more"}]}],
    "tools": [{"type": "function", "name": "shell", "description": "sh", "parameters": {"type": "object"}}]})
E("interactions_direct", "interactions", "devin/swe-2", {"system_instruction": "direct", "session_id": "my-session", "generation_config": {"temperature": 0.3, "max_output_tokens": 300, "thinking_level": "max"},
    "input": [{"type": "user_input", "content": [{"type": "text", "text": "hi"}]}]})
E("unknown_source_defaults", "", "devin/swe-2", {"input": [{"type": "user_input", "content": "x"}]})
E("empty_model", "interactions", "", {"input": [{"type": "user_input", "content": "x"}]})
