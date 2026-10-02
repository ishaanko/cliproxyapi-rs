"""Turn captured Go translator calls into oracle cases (deduped), then record oracle outputs."""
import json, hashlib, subprocess, sys
mp = json.load(open('translator_map.json'))
fnmap = {}
for e in mp:
    fnmap[(e['dir'], e['request'])] = ('request', e)
    if e.get('stream'): fnmap[(e['dir'], e['stream'])] = ('stream', e)
    if e.get('nonstream'): fnmap[(e['dir'], e['nonstream'])] = ('nonstream', e)
MAX = 200_000
cases, seen, seqs = [], set(), {}
def add(c):
    h = hashlib.sha1(json.dumps(c, sort_keys=True).encode()).hexdigest()
    if h not in seen:
        seen.add(h); cases.append(c)
for line in open('capture.jsonl'):
    r = json.loads(line)
    kind, e = fnmap.get((r['dir'], r['fn']), (None, None))
    if not kind: continue
    base = {'client': e['client'], 'upstream': e['upstream'], 'model': r['model']}
    if kind == 'request':
        if len(r['body']) > MAX: continue
        add({'kind': 'request', **base, 'stream': r['stream'], 'body': r['body']})
    elif kind == 'nonstream':
        if len(r['raw']) + len(r['orig']) > MAX: continue
        add({'kind': 'nonstream', **base, 'original': r['orig'], 'translated': r['req'], 'body': r['raw']})
    else:
        k = (r['pid'], r['seq'])
        s = seqs.get(k)
        if s is None:
            s = seqs[k] = {'kind': 'stream', **base, 'original': r['orig'], 'translated': r['req'], 'lines': []}
        s['lines'].append(r['raw'])
for s in seqs.values():
    if sum(map(len, s['lines'])) + len(s['original']) <= MAX * 2:
        add(s)
# token count cases for every registration with a TokenCount fn
for e in mp:
    if e.get('tokencount'):
        for n in (0, 1, 1234):
            add({'kind': 'token_count', 'client': e['client'], 'upstream': e['upstream'], 'model': '', 'count': n})
inp = '\n'.join(json.dumps(c) for c in cases) + '\n'
out = subprocess.run(['./oracle'], input=inp, capture_output=True, text=True).stdout.splitlines()
assert len(out) == len(cases), (len(out), len(cases))
with open('/home/ishaan/box/cliproxyapirust/conformance/translator_cases.jsonl', 'w') as f:
    for c, o in zip(cases, out):
        c['expect'] = json.loads(o)
        f.write(json.dumps(c) + '\n')
import collections
cnt = collections.Counter((c['kind']) for c in cases)
pairs = collections.Counter((c['client'], c['upstream']) for c in cases)
print(len(cases), cnt); print(sorted(pairs.items()))
print('errors', sum(1 for c in cases if 'error' in c['expect']))
