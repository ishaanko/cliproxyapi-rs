import re, os, json, subprocess, glob
ROOT = 'capture/internal/translator'
CONST = {'OpenAI':'openai','OpenaiResponse':'openai-response','Claude':'claude','Gemini':'gemini','Codex':'codex','Antigravity':'antigravity','Interactions':'interactions'}
mapping = []
for init in sorted(glob.glob(ROOT + '/**/init.go', recursive=True)):
    d = os.path.dirname(init)
    rel = os.path.relpath(d, ROOT)
    if rel == '.': continue
    src = open(init).read()
    for m in re.finditer(r'translator\.Register\(\s*(\w+),\s*(\w+),\s*(\w+),\s*interfaces\.TranslateResponse\{(.*?)\}', src, re.S):
        client, upstream, req, body = m.groups()
        ent = {'dir': rel, 'client': CONST[client], 'upstream': CONST[upstream], 'request': req}
        for k, v in re.findall(r'(\w+):\s*(\w+)', body):
            ent[k.lower()] = v
        mapping.append(ent)
json.dump(mapping, open('translator_map.json', 'w'), indent=1)
by_dir = {}
for e in mapping:
    by_dir.setdefault(e['dir'], set()).update([e['request'], e.get('stream'), e.get('nonstream')])
for rel, names in by_dir.items():
    names = {n for n in names if n}
    for f in glob.glob(f'{ROOT}/{rel}/*.go'):
        if f.endswith('_test.go'): continue
        src = open(f).read()
        hit = [n for n in names if re.search(rf'^func {n}\(', src, re.M)]
        if hit:
            subprocess.run(['/tmp/capinject', os.path.abspath(f), rel, ','.join(hit)], cwd='capture', check=True)
print(len(mapping), 'registrations')
