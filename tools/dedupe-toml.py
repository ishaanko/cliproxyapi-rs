"""Drop duplicate keys per section in crate Cargo.toml files (git can union-merge two agents'
dependency additions into duplicate keys, which cargo rejects). First occurrence wins."""
import glob
import re
for p in glob.glob('crates/*/Cargo.toml'):
    text = open(p).read()
    # Union-resolve conflict markers: keep both sides, duplicates are dropped below.
    text = re.sub(r'<<<<<<< [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n', lambda m: m.group(1) + m.group(2), text, flags=re.S)
    open(p, 'w').write(text)
    lines = text.split('\n')
    out, seen, section = [], set(), None
    for l in lines:
        if l.startswith('['):
            # Each [[array]] table entry is its own section; only [table] headers repeat by name.
            section = (l, len(out)) if l.startswith('[[') else l
            out.append(l)
            continue
        key = l.split('=')[0].strip().split('.')[0] if '=' in l and not l.lstrip().startswith('#') else None
        if key and (section, key) in seen:
            continue
        if key:
            seen.add((section, key))
        out.append(l)
    new = '\n'.join(out)
    if new != '\n'.join(lines):
        open(p, 'w').write(new)
        print('deduped', p)
