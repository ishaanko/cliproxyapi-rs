"""Drop duplicate keys per section in crate Cargo.toml files (git can union-merge two agents'
dependency additions into duplicate keys, which cargo rejects). First occurrence wins."""
import glob
for p in glob.glob('crates/*/Cargo.toml'):
    lines = open(p).read().split('\n')
    out, seen, section = [], set(), None
    for l in lines:
        if l.startswith('['):
            section = l
            out.append(l)
            continue
        key = l.split('=')[0].strip() if '=' in l and not l.lstrip().startswith('#') else None
        if key and (section, key) in seen:
            continue
        if key:
            seen.add((section, key))
        out.append(l)
    new = '\n'.join(out)
    if new != '\n'.join(lines):
        open(p, 'w').write(new)
        print('deduped', p)
