"""Randomized registry scenarios replayed against Go and Rust. Usage: registry_fuzz.py SEED COUNT"""
import json, random, sys
from common import *

random.seed(int(sys.argv[1]) if len(sys.argv) > 1 else 1)
CLIENTS = ['c1', 'c2', 'c3', 'c4']
PROVIDERS = ['gemini', 'claude', 'codex', '', 'Gemini', 'antigravity']
MODEL_IDS = ['m-a', 'm-b', 'm-c', 'm-d', 'M-E', ' m-f ']
created = {m: 1000 + 10 * i for i, m in enumerate(MODEL_IDS)}


def model(mid):
    m = {'id': mid, 'object': 'model', 'created': created[mid], 'owned_by': random.choice(['o1', 'o2']), 'type': random.choice(['claude', 'gemini', '']),
         'display_name': random.choice(['', 'Name ' + mid]), 'context_length': random.choice([0, 1000, 200000]),
         'max_completion_tokens': random.choice([0, 4096]), 'supports_web_search': random.random() < 0.3}
    if random.random() < 0.3:
        m['native_capabilities'] = {'web_search': random.choice([True, False])}
    if random.random() < 0.3:
        m['thinking'] = {'min': 1024, 'max': 8000, 'zero_allowed': True}
    if random.random() < 0.2:
        m['inputTokenLimit'] = 5
        m['supportedGenerationMethods'] = ['generateContent']
        m['name'] = 'models/' + mid
    return m


def scenario():
    steps = []
    for _ in range(random.randint(5, 30)):
        r = random.random()
        c = random.choice(CLIENTS)
        mid = random.choice(MODEL_IDS)
        if r < 0.28:
            k = random.choice([0, 1, 2, 3, 4])
            models = [model(random.choice(MODEL_IDS)) for _ in range(k)]
            steps.append({'op': 'register', 'client': c, 'provider': random.choice(PROVIDERS), 'models': models})
        elif r < 0.34:
            steps.append({'op': 'unregister', 'client': c})
        elif r < 0.44:
            steps.append({'op': 'suspend', 'client': c, 'model': mid, 'reason': random.choice(['', 'quota', 'Quota', 'other'])})
        elif r < 0.50:
            steps.append({'op': 'resume', 'client': c, 'model': mid})
        elif r < 0.58:
            steps.append({'op': 'quota', 'client': c, 'model': mid})
        elif r < 0.62:
            steps.append({'op': 'clear_quota', 'client': c, 'model': mid})
        elif r < 0.70:
            steps.append({'op': 'project', 'client': c, 'epoch_offset': random.choice([0, 0, 0, -1]), 'generation': random.choice([0, 1, 2]),
                          'projections': [{'model': random.choice(MODEL_IDS), 'suspended': random.random() < 0.5, 'reason': random.choice(['quota', 'x', '']), 'quota': random.random() < 0.5} for _ in range(random.randint(0, 3))]})
        elif r < 0.75:
            steps.append({'op': 'capabilities', 'client': c, 'epoch_offset': random.choice([0, 0, 1]), 'web_search': random.random() < 0.5})
        elif r < 0.78:
            steps.append({'op': 'cleanup'})
        else:
            steps.append({'op': 'query'})
        if random.random() < 0.25:
            steps.append({'op': 'query'})
    steps.append({'op': 'query'})
    for s in steps:
        if s['op'] == 'query':
            s['clients'] = CLIENTS
            s['model_ids'] = [m for m in MODEL_IDS] + ['ghost']
            s['providers'] = ['gemini', 'claude', 'codex', 'antigravity', 'Gemini', '']
    return steps


def show_diff(a, b, path):
    if isinstance(a, tuple) and isinstance(b, tuple) and a and b and a[0] == b[0] == 'o':
        da, db = dict(a[1]), dict(b[1])
        for k in sorted(set(da) | set(db)):
            if da.get(k) != db.get(k):
                show_diff(da.get(k), db.get(k), path + '.' + k)
                return
    elif isinstance(a, list) and isinstance(b, list) and len(a) == len(b):
        for i, (x, y) in enumerate(zip(a, b)):
            if x != y:
                show_diff(x, y, path + '[%d]' % i)
                return
    print('   at', path, '\n     GO', str(a)[:300], '\n     RS', str(b)[:300])


def strip_by_provider(x):
    # Go picks the info of an arbitrary client (map iteration order) for by_provider entries: compare ids only.
    if isinstance(x, dict) and 'by_provider' in x and isinstance(x['by_provider'], dict):
        x = dict(x)
        x['by_provider'] = {k: sorted(m['id'] for m in v) if v else None for k, v in x['by_provider'].items()}
    return x


N = int(sys.argv[2]) if len(sys.argv) > 2 else 300
ops = [{'op': 'registry_scenario', 'steps': scenario()} for _ in range(N)]
go_out = [[strip_by_provider(x) for x in sc] if isinstance(sc, list) else sc for sc in run_oracle(GO_ORACLE, ops)]
rust_out = [[strip_by_provider(x) for x in sc] if isinstance(sc, list) else sc for sc in run_oracle(RUST_ORACLE, ops)]
bad = []
for op, g, r in zip(ops, go_out, rust_out):
    gc, rc = loosen(sort_obj(canon_out(g))), loosen(sort_obj(canon_out(r)))
    if gc != rc:
        bad.append((op, g, r))
print('scenarios', len(ops), 'mismatches', len(bad))
for op, g, r in bad[:2]:
    # find first differing step
    for i, (gs, rs) in enumerate(zip(g, r)):
        gc, rc = loosen(sort_obj(canon_out(gs))), loosen(sort_obj(canon_out(rs)))
        if gc != rc:
            print('first diff at step', i, json.dumps(op['steps'][i])[:300])
            if isinstance(gs, dict):
                for k in gs:
                    a = loosen(sort_obj(canon_out(gs[k]))); b = loosen(sort_obj(canon_out(rs.get(k))))
                    if a != b:
                        print(' key', k)
                        show_diff(a, b, k)
            else:
                print('  GO', gs, 'RS', rs)
            break
    print('steps:')
    for s in op['steps'][:i + 1]:
        print('  ', json.dumps(s)[:260])
