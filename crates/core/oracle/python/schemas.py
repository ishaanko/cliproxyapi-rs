"""Schema cleaner / name map / Responses tool differential run (Go vs Rust)."""
import collections
import json
import os
import sys

from common import *


def main():
    schemas, bodies = collect_schemas()
    print('schemas', len(schemas), 'bodies', len(bodies))
    ops = []
    for s in schemas:
        for name in ['clean_gemini', 'clean_gemini_json_schema', 'clean_antigravity', 'clean_antigravity_response', 'inline_local_refs', 'normalize_claude_schema']:
            ops.append({'op': name, 'in': s})
        for flag in (True, False):
            ops.append({'op': 'clean_antigravity_tool', 'in': s, 'flag': flag})
    # extra hand-written schema edge cases
    extra = json.load(open(EXTRA_SCHEMAS)) if os.path.exists(EXTRA_SCHEMAS) else []
    for s in extra:
        for name in ['clean_gemini', 'clean_gemini_json_schema', 'clean_antigravity', 'clean_antigravity_response', 'inline_local_refs', 'normalize_claude_schema']:
            ops.append({'op': name, 'in': s})
        for flag in (True, False):
            ops.append({'op': 'clean_antigravity_tool', 'in': s, 'flag': flag})
    for b in bodies:
        ops.append({'op': 'tool_maps', 'in': b})
        ops.append({'op': 'responses_tools', 'in': b})
        ops.append({'op': 'strip_attribution', 'in': b})
        ops.append({'op': 'dedupe', 'in': b})
    go_out = run_oracle(GO_ORACLE, ops)
    rust_out = run_oracle(RUST_ORACLE, ops)
    bad = compare(ops, go_out, rust_out, order_insensitive_ops=('responses_tools',), loose_ops=('tool_maps', 'responses_tools'))
    print('ops', len(ops), 'mismatches', len(bad))
    by = collections.Counter(b[0]['op'] for b in bad)
    print(by)
    for op, g, r in bad[:int(sys.argv[1]) if len(sys.argv) > 1 else 5]:
        print('=' * 80)
        print('OP', op['op'], 'flag', op.get('flag'))
        print('IN ', op['in'][:1500])
        print('GO ', json.dumps(g)[:1500])
        print('RS ', json.dumps(r)[:1500])


main()
