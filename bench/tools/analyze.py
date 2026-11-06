"""Every step of every run, then per-run rows and per-arm means. usage: analyze.py [--quiet]"""
import json, glob, os, sys, re, statistics as st
E = os.path.dirname(os.path.abspath(__file__)); R = os.environ.get('MT_RUNS', E + '/runs'); quiet = '--quiet' in sys.argv
hooklog = [json.loads(l) for l in open('/tmp/mt-tools-hook.log')] if os.path.exists('/tmp/mt-tools-hook.log') else []

def score(task, answer, calls, changes, diff):
    a = answer
    if task == 'T1':
        parts = {'migration 0001_init.up.sql:575': '0001_init.up.sql' in a and re.search(r'57[45]', a) is not None,
                 'Store.WriteAudit internal/store/audit.go': 'audit.go' in a and 'WriteAudit' in a,
                 'test internal/api/routes_test.go': 'routes_test.go' in a}
    elif task == 'T2':
        used = any(('humanizer' in json.dumps(c[1])) for c in calls)
        parts = {'read the humanizer skill': used,
                 'no AI tells left': not re.search(r'testament|landscape|cutting-edge|seamless', a, re.I)}
    elif task == 'T4':
        parts = {'web: web/src/screens/Sessions.tsx': 'Sessions.tsx' in a,
                 'API: internal/api/sessions.go (listSessions)': 'api/sessions.go' in a,
                 # the store step: internal/store/sessions.go inside floorSelect (L19-53, the SQL predicate) or
                 # Store.ListSessions (L55-88), cited by line or by name; the task asks for file:line
                 'store: internal/store/sessions.go (floorSelect / ListSessions)': 'store/sessions.go' in a and (
                     'ListSessions' in a or 'floorSelect' in a or any(19 <= int(n) <= 88 for n in re.findall(r'store/sessions\.go:(\d+)', a)))}
    elif task == 'T3':
        new = [l[3:] for l in changes.splitlines() if l.startswith('??')]
        mig = [f for f in new if 'migrations/0006' in f]
        rd = lambda f: open(os.path.join(R, label, f)).read() if os.path.isfile(os.path.join(R, label, f)) else ''
        up = ''.join(rd(f) for f in mig if f.endswith('.up.sql')); down = ''.join(rd(f) for f in mig if f.endswith('.down.sql'))
        build = open(f'{R}/{label}.build').read() if os.path.exists(f'{R}/{label}.build') else 'not run'
        parts = {'0006 up migration adds archived_at': 'archived_at' in up.lower() and 'add' in up.lower(),
                 '0006 down migration drops it': 'archived_at' in down.lower() and 'drop' in down.lower(),
                 'SessionDetail.ArchivedAt with json archived_at': bool(re.search(r'^\+\s*ArchivedAt\s+\*?time\.Time.*json:"archived_at', diff, re.M)),
                 'GetSession selects and scans it': 'internal/store/sessions.go' in diff and bool(re.search(r'^\+.*archived_at', diff, re.M)) and bool(re.search(r'^\+.*ArchivedAt', diff[diff.find('internal/store/sessions.go'):], re.M)),
                 'go build ./... passes': build.strip() == 'ok'}
    else:
        new = [l[3:] for l in changes.splitlines() if l.startswith('??') or l.startswith(' A') or l.startswith('A ')]
        mig = [f for f in new if 'migrations/0006' in f]
        body = ''.join(open(os.path.join(E, 'runs', label, f)).read() for f in mig if os.path.isfile(os.path.join(E, 'runs', label, f))) if mig else ''
        parts = {'0006 up+down migrations': any(f.endswith('.up.sql') for f in mig) and any(f.endswith('.down.sql') for f in mig),
                 'migration adds cost_usd': 'cost_usd' in body.lower(),
                 'Go struct field added': bool(re.search(r'^\+.*Cost\w*\s+\*?(float64|decimal|string|sql\.Null|pgtype)', diff, re.M | re.I))}
    return sum(parts.values()), len(parts), parts

rows = []
for meta in sorted(glob.glob(R + '/*.meta.json')):
    m = json.load(open(meta)); label = m['label']; arm, task, rep = label.split('-')
    msgs, order, calls, results, answer, res = {}, [], [], {}, '', {}
    for l in open(f'{R}/{label}.stream.jsonl'):
        d = json.loads(l)
        if d.get('type') == 'assistant' and not d.get('parent_tool_use_id'):
            mm = d['message']; mid = mm['id']
            if mid not in msgs: order.append(mid); msgs[mid] = {'usage': mm.get('usage', {}), 'calls': [], 'text': ''}
            msgs[mid]['usage'] = mm.get('usage', msgs[mid]['usage'])
            for c in mm['content']:
                if c['type'] == 'tool_use': msgs[mid]['calls'].append((c['name'], c['input'], c['id'])); calls.append((c['name'], c['input']))
                if c['type'] == 'text': msgs[mid]['text'] += c['text']
        if d.get('type') == 'user':
            for c in (d['message'].get('content') or []):
                if isinstance(c, dict) and c.get('type') == 'tool_result':
                    body = c.get('content'); results[c['tool_use_id']] = len(body) if isinstance(body, str) else len(json.dumps(body))
        if d.get('type') == 'result': res = d; answer = d.get('result') or ''
    tot = lambda u: u.get('input_tokens', 0) + u.get('cache_creation_input_tokens', 0) + u.get('cache_read_input_tokens', 0)
    first = tot(msgs[order[0]]['usage']) if order else 0
    hook = [h for h in hooklog if h['project'].endswith('/' + label)]
    hook_chars = sum(len('\n'.join(h['cards'])) for h in hook)
    changes = open(f'{R}/{label}.changes').read(); diff = open(f'{R}/{label}.diff').read()
    ok, n, parts = score(task, answer, calls, changes, diff)
    u = res.get('usage', {})
    row = dict(label=label, arm=arm, task=task, turns=res.get('num_turns'), calls=len(calls), mt=sum(1 for c in calls if c[0] == 'Bash' and re.search(r'(^|[ /])mt ', c[1].get('command', ''))),
               toolsearch=sum(1 for c in calls if c[0] == 'ToolSearch'), first=first, input=sum(tot(msgs[k]['usage']) for k in order),
               out=u.get('output_tokens', 0), cost=res.get('total_cost_usd', 0), wall=m['wall'], hook_chars=hook_chars, ok=ok, n=n)
    rows.append(row)
    if not quiet:
        print(f"\n===== {label}  ({task}) wall {m['wall']}s  cost ${row['cost']:.3f}  turns {row['turns']}  first request {first:,} tok")
        for h in hook: print(f"  hook ({h['ms']} ms) injected {len(chr(10).join(h['cards'])):,} chars: " + ' | '.join(c.split(chr(10))[0][:90] for c in h['cards']))
        for i, k in enumerate(order):
            mm = msgs[k]; us = mm['usage']
            print(f"  req {i+1:2}: ctx {tot(us):>8,} (read {us.get('cache_read_input_tokens',0):,} write {us.get('cache_creation_input_tokens',0):,}) out {us.get('output_tokens',0):>5,}")
            for name, inp, cid in mm['calls']:
                s = inp.get('command') or inp.get('file_path') or inp.get('query') or inp.get('skill') or inp.get('pattern') or json.dumps(inp)[:120]
                print(f"         {name:10} {str(s)[:150]!s:150}  → {results.get(cid, 0):,} chars")
        print(f"  answer: {answer[:400]!r}")
        for k, v in parts.items(): print(f"  {'✓' if v else '✗'} {k}")
print('\n| run | turns | calls | mt | ToolSearch | first req tok | input tok (all reqs) | output | wall s | cost $ | hook chars | correct |')
print('|---|---|---|---|---|---|---|---|---|---|---|---|')
for r in rows: print(f"| {r['label']} | {r['turns']} | {r['calls']} | {r['mt']} | {r['toolsearch']} | {r['first']:,} | {r['input']:,} | {r['out']:,} | {r['wall']} | {r['cost']:.3f} | {r['hook_chars']:,} | {r['ok']}/{r['n']} |")
print('\n| mean | turns | calls | first req tok | input tok | output | wall s | cost $ | correct |')
print('|---|---|---|---|---|---|---|---|---|')
for task in ['T1', 'T2', 'T3', 'T4', 'all']:
    for arm in ['A', 'B']:
        g = [r for r in rows if r['arm'] == arm and (task == 'all' or r['task'] == task)]
        if not g: continue
        mean = lambda k: st.mean(r[k] for r in g)
        print(f"| {arm}-{task} (n={len(g)}) | {mean('turns'):.1f} | {mean('calls'):.1f} | {mean('first'):,.0f} | {mean('input'):,.0f} | {mean('out'):,.0f} | {mean('wall'):.1f} | {mean('cost'):.3f} | {sum(r['ok'] for r in g)}/{sum(r['n'] for r in g)} |")
json.dump(rows, open(E + '/results.json', 'w'), indent=1)
