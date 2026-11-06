"""Coordination A/B: per run, the parent and its subagent(s), raw. Throwaway analysis."""
import json, glob, os, statistics as st, sys, collections

E = os.path.dirname(os.path.abspath(__file__))
EDIT = ('mt apply', 'sed -i', 'perl -pi')

def tok(u):
    return u.get('input_tokens', 0) + u.get('cache_read_input_tokens', 0) + u.get('cache_creation_input_tokens', 0), u.get('output_tokens', 0)

rows = []
RUNS = sys.argv[1] if len(sys.argv) > 1 else 'runs'
for meta in sorted(glob.glob(f'{E}/{RUNS}/*.meta.json')):
    m = json.load(open(meta)); lab = m['label']; arm, task, rep = lab.split('-')
    if rep == 'r0': continue
    ev = [json.loads(l) for l in open(f'{E}/{RUNS}/{lab}.stream.jsonl') if l.strip()]
    res = next((e for e in ev if e.get('type') == 'result'), {})
    # subagent events carry parent_tool_use_id; dedupe assistant messages by id (one per content block)
    seen = set(); sub = collections.defaultdict(lambda: {'calls': [], 'in': 0, 'out': 0})
    par = {'calls': [], 'in': 0, 'out': 0}
    for e in ev:
        if e.get('type') != 'assistant': continue
        msg = e['message']; who = sub[e['parent_tool_use_id']] if e.get('parent_tool_use_id') else par
        # one message arrives as several events (one per content block); the last carries the final usage
        who.setdefault('usage', {})[msg.get('id')] = msg.get('usage', {})
        for c in msg.get('content', []):
            if c.get('type') == 'tool_use':
                who['calls'].append((c['name'], c['input'].get('command', '') if isinstance(c['input'], dict) else ''))
    # tokens from the session transcripts (the stream's per-block usage has partial output counts)
    def tt(path):
        last = {}
        for l in open(path):
            try: d = json.loads(l)
            except Exception: continue
            if d.get('type') == 'assistant': last[d['message'].get('id')] = d['message'].get('usage', {})
        return [sum(x) for x in zip(*[tok(u) for u in last.values()])] or [0, 0]
    pt = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{m['sid']}.jsonl"))
    st_ = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{m['sid']}/subagents/*.jsonl"))
    par['in'], par['out'] = tt(pt[0]) if pt else (0, 0)
    sums = [tt(f) for f in st_]
    sub_in_t, sub_out_t = (sum(x[0] for x in sums), sum(x[1] for x in sums)) if sums else (0, 0)
    spawns = sum(1 for n, _ in par['calls'] if n in ('Agent', 'Task'))
    s_calls = [x for s in sub.values() for x in s['calls']]
    first_edit = next((i for i, (n, c) in enumerate(s_calls) if n in ('Edit', 'Write', 'MultiEdit') or any(k in c for k in EDIT)), None)
    used_brief = any('mt expand @' in c for _, c in s_calls)
    briefed = bool(glob.glob(f"{m['dir']}/.mt/coord/briefs/*.json"))
    ok = open(f'{E}/{RUNS}/{lab}.check').read().strip() == 'ok'
    if task == 'W':
        # every construction site set, with its value: 3 http, 1 websocket, 1 auth
        d = open(f'{E}/{RUNS}/{lab}.diff').read()
        added = [l for l in d.splitlines() if l.startswith('+') and 'Source' in l and ':' in l]
        got = {v: sum(f'"{v}"' in l for l in added) for v in ('http', 'websocket', 'auth')}
        ok = ok and got['http'] >= 3 and got['websocket'] >= 1 and got['auth'] >= 1
    rows.append(dict(arm=arm, task=task, rep=rep, ok=ok,
        cost=res.get('total_cost_usd', 0), turns=res.get('num_turns', 0), wall=m['wall'], spawns=spawns, briefed=briefed, used_brief=used_brief,
        sub_calls=len(s_calls), sub_orient=first_edit if first_edit is not None else len(s_calls), sub_in=sub_in_t, sub_out=sub_out_t,
        par_calls=len(par['calls']), par_in=par['in'], par_out=par['out']))

cols = ['arm', 'task', 'rep', 'ok', 'cost', 'wall', 'spawns', 'briefed', 'used_brief', 'sub_calls', 'sub_orient', 'sub_in', 'sub_out', 'par_calls', 'par_in', 'par_out']
print('\t'.join(cols))
for r in rows: print('\t'.join(f'{r[c]:.3f}' if isinstance(r[c], float) else str(r[c]) for c in cols))

def mean(xs): return st.mean(xs) if xs else 0
print('\nper task, mean A -> B (change)')
for task in sorted({r['task'] for r in rows}) + ['ALL']:
    a = [r for r in rows if r['arm'] == 'A' and task in (r['task'], 'ALL')]
    b = [r for r in rows if r['arm'] == 'B' and task in (r['task'], 'ALL')]
    if not a or not b: continue
    print(f'{task}: n={len(a)}/{len(b)} correct {sum(r["ok"] for r in a)}/{len(a)} -> {sum(r["ok"] for r in b)}/{len(b)}; B used the brief {sum(r["used_brief"] for r in b)}/{len(b)}')
    for k in ['sub_orient', 'sub_calls', 'sub_in', 'sub_out', 'par_in', 'cost', 'wall']:
        ma, mb = mean([r[k] for r in a]), mean([r[k] for r in b])
        print(f'   {k:<11} {ma:>12.1f} -> {mb:>12.1f}  ({(mb - ma) / ma * 100 if ma else 0:+.0f}%)')
