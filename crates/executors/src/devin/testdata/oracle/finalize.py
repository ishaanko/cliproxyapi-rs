# Trims golden.json (Go oracle output) into the committed ../golden.json; see gen_cases.py.
import json
g = json.load(open('golden.json'))
req = g['streams'][0]['requests']
for s in g['streams']:
    if s['requests'] == req:
        del s['requests']
    if s['name'] == 'many_tools_130':
        s['formats'] = [f for f in s['formats'] if f['format'] == 'interactions']
        for f in s['formats']:
            f['non_stream'] = ''
g['requests'] = req
json.dump(g, open('../golden.json', 'w'), separators=(',', ':'), ensure_ascii=True)
