"""Shared helpers for the base-layer differential harness (see ../README.md)."""
import glob
import gzip
import json
import os
import re
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, '../../../..'))
GO_ORACLE = os.environ['GO_ORACLE']        # built from ../go/base_oracle.go.txt
RUST_ORACLE = os.environ['RUST_ORACLE']    # built from ../rust
GO_REPO = os.environ['GO_REPO']            # CLIProxyAPI checkout (for Go test strings)
EXTRA_SCHEMAS = os.environ.get('EXTRA_SCHEMAS', os.path.join(os.environ.get('SCRATCH', '/tmp'), 'extra_schemas.json'))


def run_oracle(binary, ops):
    data = '\n'.join(json.dumps(o) for o in ops) + '\n'
    p = subprocess.run([binary], input=data, capture_output=True, text=True, timeout=1800)
    lines = p.stdout.strip('\n').split('\n') if p.stdout.strip() else []
    if len(lines) != len(ops):
        print('line count mismatch', binary, len(lines), len(ops), p.stderr[:500])
    return [json.loads(line) for line in lines]


def parse_strict(s):
    """Order-sensitive form of a JSON text: objects keep key order, numbers keep their raw text."""
    return json.loads(
        s,
        object_pairs_hook=lambda pairs: ('s', pairs),
        parse_float=lambda x: ('n', x),
        parse_int=lambda x: ('n', x),
    )


def canon_out(out):
    """Strings holding JSON are compared structurally and order-sensitively ('s'); result dicts
    ('o') are Go maps, whose key order is not meaningful."""
    if isinstance(out, str):
        try:
            return parse_strict(out)
        except Exception:
            return out
    if isinstance(out, dict):
        return ('o', [(k, canon_out(v)) for k, v in out.items()])
    if isinstance(out, list):
        return [canon_out(v) for v in out]
    return out


def loosen(v):
    """Treat null, [] and {} alike (Go nil vs empty collections)."""
    if v is None or v == [] or v == ('o', []):
        return None
    if isinstance(v, tuple) and v and v[0] == 'o':
        return ('o', sorted((k, loosen(x)) for k, x in v[1]))
    if isinstance(v, list):
        return [loosen(x) for x in v]
    return v


def sort_obj(v):
    if isinstance(v, tuple) and v and v[0] == 'o':
        return ('o', sorted((k, sort_obj(x)) for k, x in v[1]))
    if isinstance(v, list):
        return [sort_obj(x) for x in v]
    return v


def compare(ops, go_out, rust_out, order_insensitive_ops=(), loose_ops=()):
    bad = []
    for op, g, r in zip(ops, go_out, rust_out):
        name = op['op']
        gc, rc = canon_out(g), canon_out(r)
        if name in order_insensitive_ops:
            gc, rc = sort_obj(gc), sort_obj(rc)
        if name in loose_ops:
            gc, rc = loosen(gc), loosen(rc)
        if gc != rc:
            bad.append((op, g, r))
    return bad


def collect_schemas():
    """Tool schemas and whole request bodies from the conformance corpus and Go test strings."""
    schemas, bodies = set(), []
    keys = {'input_schema', 'parameters', 'parametersJsonSchema', 'schema', 'responseJsonSchema', 'responseSchema', 'response_schema'}

    def walk(v):
        if isinstance(v, dict):
            for k, x in v.items():
                if k in keys and isinstance(x, (dict, bool)):
                    schemas.add(json.dumps(x))
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)

    with gzip.open(os.path.join(REPO, 'conformance/translator_cases.jsonl.gz'), 'rt') as f:
        for line in f:
            d = json.loads(line)
            for fld in ('body', 'original', 'translated'):
                s = d.get(fld)
                if isinstance(s, str):
                    try:
                        v = json.loads(s)
                    except Exception:
                        continue
                    walk(v)
                    if isinstance(v, dict) and ('tools' in v or 'request' in v):
                        bodies.append(s)
    request_keys = {'messages', 'model', 'contents', 'request', 'tools', 'input', 'system', 'generationConfig'}
    paths = glob.glob(GO_REPO + '/internal/util/*_test.go') + glob.glob(GO_REPO + '/internal/registry/*_test.go')
    paths += glob.glob(GO_REPO + '/internal/translator/**/*_test.go', recursive=True)
    for path in paths:
        try:
            src = open(path).read()
        except Exception:
            continue
        for m in re.finditer(r'`([^`]*)`', src):
            s = m.group(1).strip()
            if s[:1] in '{[' and len(s) > 2:
                try:
                    v = json.loads(s)
                except Exception:
                    continue
                if isinstance(v, dict):
                    if not (set(v) & request_keys):
                        schemas.add(s)
                    walk(v)
                    if 'tools' in v or 'input' in v or 'request' in v:
                        bodies.append(s)
    return sorted(schemas), bodies
