"""Scalar helper differential run (sanitizers, FixJSON, masks, apply-patch, OAuth, catalogs, ...)."""
import json, sys
from common import *

ops = []
def add(op, **kw):
    ops.append(dict(op=op, **kw))

# sanitizers
names = ['', 'abc', 'a b', '1abc', '.dot', ':colon', '-dash', '_under', 'x' * 70, '9' * 70, '9' * 63, '9' * 64, 'héllo wörld', 'mcp__server.tool:name', 'a/b/c', '日本語', '!', 'ok-name_1.2:3']
for n in names:
    add('sanitize_function_name', **{'in': n})
    add('sanitize_claude_function_name', **{'in': n})
for n in ['', 'toolu_abc', 'call:1', 'a b c', 'é']:
    if n:
        add('sanitize_claude_tool_id', **{'in': n})
# tool use ids
args = ['', '  ', '{"b":1,"a":[1.0,2e3]}', '{"a":"<x>&"}', 'not json', ' {"k": 1e999} ', '[1,2.50]', '"str"', '12345678901234567890', '{"u":"\\u00e9\\u2028"}']
for a in args:
    add('gemini_claude_tool_use_id', **{'in': 'call1', 'in2': 'name', 'in3': a})
add('gemini_claude_tool_use_id', **{'in': ' ', 'in2': 'name', 'in3': ''})
add('gemini_claude_tool_use_id', **{'in': 'c', 'in2': ' ', 'in3': ''})
for i in ['cpa_gemini_' + 'a' * 32, 'cpa_gemini_' + 'g' * 32, ' cpa_gemini_' + '0' * 32 + ' ', 'cpa_gemini_abc', 'x']:
    add('is_gemini_claude_tool_use_id', **{'in': i})
# FixJSON
for s in ["{'a': 1, 'b': '2'}", '{"t": \'He said "hi"\'}', "{'a': 'it\\'s'}", "{'a': '\\u00e9\\u12'}", "{'a': 'unterminated", "{'a': '\\n\\t\\q\\\\'}", '{"a": "it\'s"}', "['x','y']", "'日本'", '', "{'a':'\\/'}", "{'a':'\\\"'}"]:
    add('fix_json', **{'in': s})
# unicode escape
for s in ['\\p{L}', '\\P{N}', '\\0', '[^\\0]*', '\\\\p{L}', 'abc', '\\', 'a\\', '\\pL', '\\\\0', '\\d+\\p{', '\\x00']:
    add('unicode_escape', **{'in': s})
# mask
for k in ['', 'a', 'ab', 'abc', 'abcd', 'abcde', 'abcdefgh', 'abcdefghi', 'sk-1234567890abcdef', 'héllo-wörld-key', '日本語日本語日本語']:
    add('hide_api_key', **{'in': k})
for q in ['', 'a=b', 'key=secret123456', 'api_key=abcdefghijk&x=1', 'token=abc%20def&k=v', 'API-KEY=zzz', 'foo[]=1&key[]=abcdefghijk', 'secret=%ZZ', 'a&b=&token', 'x=1&&token=12345', 'Token=a+b+c+d+e+f', 'key=é%C3%A9']:
    add('mask_query', **{'in': q})
for k, v in [('Authorization', 'Bearer abcdefghijkl'), ('authorization', 'abc'), ('X-Api-Key', 'abcdefghijkl'), ('x-token', 'abcde'), ('Content-Type', 'application/json'), ('Authorization', '  Basic  abcdef '), ('X-Secret', 'ab'), ('apikey', 'abcdefghi')]:
    add('mask_header', **{'in': k, 'in2': v})
for n in ['', ' ', 'Foo', 'openai-compatibility', 'openai-compatible-x', 'OpenAI-Compatible-Y', ' my prov ']:
    add('openai_compat_key', **{'in': n})
# qualify
for a, b in [('functions', 'exec'), ('functions__', 'exec'), ('', 'x'), ('ns', ''), ('ns', 'ns'), ('ns', 'ns__x'), ('ns', 'mcp__srv__x'), (' ns ', ' c ')]:
    add('qualify', **{'in': a, 'in2': b})
# unwrap custom
for s in ['', '  ', '{}', '{"input":"abc"}', '{"input":{"a":1}}', '"plain"', 'raw text', '{"x":1}', '{"input":null}', '[1]', '  {"input": 5}  ', '{"input":""}']:
    add('unwrap_custom', **{'in': s})
# tool choice
fwd = json.dumps({'exec': 'exec_g', 'ns__fn': 'ns__fn_g'})
for t in ['"none"', '"AUTO"', '" required "', '"any"', '"bogus"', '{"type":"function","name":"exec"}', '{"type":"custom","custom":{"name":"x"}}', '{"type":"tool","function":{"name":"fn","namespace":"ns"}}', '{"type":"auto"}', '{"type":"allowed_tools"}', '{"name":"exec"}', '{}', '5', 'null', '{"type":"function","name":"fn","namespace":"ns"}', '{"type":"function"}']:
    add('tool_choice', **{'in': t, 'in2': fwd})
add('tool_choice', **{'in': '', 'in2': fwd})
# tool result
for c in ['"hello"', '[{"type":"text","text":"a"}]', '[{"type":"text","text":"a"},{"type":"text","text":"b"}]', '[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}},{"type":"text","text":"t"}]', '[{"type":"image","source":{"type":"base64","media_type":"image/png","data":""}}]', '{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}}', '{"a":1}', '5', 'null', 'true', '[]', '', '[{"type":"image","source":{"type":"url","url":"x"}}]', '[1,"x"]']:
    add('tool_result', **{'in': c})
# apply patch
tools = ['{"type":"custom","name":"apply_patch","description":"This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"type":"grammar","definition":"start: x\\n*** Environment ID: e"}}', '{"type":"custom","name":" apply_patch\\n"}', '{"type":"function","name":"apply_patch"}', '{}', '', '{"description":"Edit.","format":{"definition":""}}']
inputs = ['{"input":"a<b>&c"}', '{"input":"x"} ', '{"input":1}', '{"other":"x"}', '', '[]', '{"input":"a","b":2}', '{"input":"a"}{}', ' { "input" : "\\u00e9" } ', '{"input":"a"', '{"input":"a","input":"b"}', '{}', 'null']
for i, t in enumerate(tools):
    add('applypatch', **{'in': t, 'in2': inputs[i % len(inputs)], 'in3': 'patch <&>   "q"\n'})
for i in inputs:
    add('applypatch', **{'in': tools[0], 'in2': i, 'in3': 'x'})
# oauth
for s in ['', '  ', 'garbage', '?code=abc&state=xyz', 'localhost:1455/cb?code=a&state=b', 'code=abc&state=xyz', 'http://localhost/cb?code=abc#st', 'http://localhost/cb#code=a&state=b', 'http://localhost/cb?error=denied&error_description=nope', 'http://localhost/cb?error_description=only', 'http://x/?code=a%23b', 'abc/def', 'code=abc%23state', 'http://localhost/cb?state=s', 'http://localhost/cb?code=%20a%20&state=%20', 'http://localhost/cb#code%3Da%26state%3Db', ':::', 'a:b']:
    add('oauth_callback', **{'in': s})
# antigravity ua
for s in ['', ' ', 'antigravity/hub/2.2.1 darwin/arm64', 'antigravity/1.23.2 windows/amd64', 'Antigravity/Hub/3.0.0 x google-api-nodejs-client/9.9.9', 'antigravity/hub/ ', 'curl/8', 'antigravity/hub/2.2.1\tdarwin', 'antigravity/ google-api-nodejs-client/1', ' google-api-nodejs-client/1', 'ANTIGRAVITY/2.0.0 linux/x64 Google-Api-Nodejs-Client/3']:
    add('antigravity_ua', **{'in': s})
for e in ['json', 'png', 'jpg', 'PNG', 'webp', 'pdf', 'mp4', 'zzz', '', 'ez', 'ice', 'xlsx', 'md', 'txt']:
    add('mime', **{'in': e})
add('catalogs')
add('native_flags')
add('codex_client')
for m in ['gpt-image-2', 'claude-sonnet-4-5', 'claude-haiku-4-5-20251001', 'devin/swe-2', 'nope', '']:
    add('lookup_static', **{'in': m})
for m in ['devin/swe-2', 'swe-2', 'DEVIN/SWE-1-6-SLOW', 'swe-1-6-fast', 'claude-fable-5-1-high', 'nope', '', 'gpt-6-astra-xhigh-fast']:
    add('lookup_devin', **{'in': m})
for m, c in [('gpt-5.5', 'codex'), ('gpt-image-2', 'codex'), ('grok-imagine-image', 'xai'), ('devin/swe-2', 'devin'), ('claude-sonnet-4-5', 'gemini'), ('x', 'nope')]:
    add('lookup_static_channel', **{'in': m, 'in2': c})
# dedupe / strip
add('dedupe', **{'in': '[{"name":"a","x":1},{"name":"a"},{"description":"no name"},{"name":""},{"name":"b"}, {"name":"b"}]'})
add('dedupe', **{'in': '{"name":"a"}'})
add('dedupe', **{'in': 'bad'})
add('strip_attribution', **{'in': '{"system":"x-anthropic-billing-header: cc=1","messages":[]}'})
add('strip_attribution', **{'in': '{"system":"  x-anthropic-billing-header: cc=1","messages":[]}'})
add('strip_attribution', **{'in': '{"system":[{"type":"text","text":"x-anthropic-billing-header: a"},{"type":"text","text":"keep"}]}'})
add('strip_attribution', **{'in': '{"system":[{"type":"text","text":"x-anthropic-billing-header: a"}]}'})
add('strip_attribution', **{'in': '{"system":[{"type":"text","text":"keep"}]}'})
add('strip_attribution', **{'in': '{"system":5}'})
add('strip_attribution', **{'in': 'nope'})

go_out = run_oracle(GO_ORACLE, ops)
rust_out = run_oracle(RUST_ORACLE, ops)
bad = compare(ops, go_out, rust_out, order_insensitive_ops={o['op'] for o in ops}, loose_ops=('lookup_static',))
print('ops', len(ops), 'mismatches', len(bad))
for op, g, r in bad[:40]:
    print('=' * 60)
    print(json.dumps(op)[:600])
    print('GO', json.dumps(g)[:700])
    print('RS', json.dumps(r)[:700])
