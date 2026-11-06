"""Coordination tax in real Claude Code transcripts: parent sessions and their subagents."""
import json, glob, os, re, collections, statistics as st

ROOT = os.path.expanduser('~/.claude/projects')
PATHRE = re.compile(r'(?:[\w.\-]+/)+[\w.\-]+\.(?:rs|go|py|ts|tsx|js|jsx|cs|swift|kt|java|rb|md|json|toml|yaml|yml|sql|sh|c|h|cpp|hpp|vue|svelte|shader|hlsl|html|css)\b')

def files_of(tu):
    n, i = tu.get('name'), tu.get('input') or {}
    out = set()
    for k in ('file_path', 'path', 'notebook_path'):
        v = i.get(k)
        if isinstance(v, str) and '.' in os.path.basename(v): out.add(os.path.basename(v))
    if n == 'Bash':
        for m in PATHRE.findall(i.get('command', '')): out.add(os.path.basename(m))
    return out

def is_edit(tu):
    n = tu.get('name'); c = (tu.get('input') or {}).get('command', '')
    return n in ('Edit', 'Write', 'MultiEdit', 'NotebookEdit') or (n == 'Bash' and re.search(r'\bmt apply\b|sed -i|perl -pi|> *\S+\.\w+\b|tee ', c))

def toks(u):
    return (u.get('input_tokens', 0) + u.get('cache_read_input_tokens', 0) + u.get('cache_creation_input_tokens', 0)), u.get('output_tokens', 0)

def load(path):
    ev = []
    for l in open(path, errors='ignore'):
        try: ev.append(json.loads(l))
        except Exception: pass
    return ev

S = collections.Counter(); L = collections.defaultdict(list)
prompts = []
for parent in glob.glob(f'{ROOT}/*/*.jsonl'):
    sid = os.path.basename(parent)[:-6]
    sdir = os.path.join(os.path.dirname(parent), sid, 'subagents')
    if not os.path.isdir(sdir): continue
    metas = {}
    for m in glob.glob(f'{sdir}/*.meta.json'):
        try: metas[json.load(open(m))['toolUseId']] = m[:-10] + '.jsonl'
        except Exception: pass
    if not metas: continue
    S['parents'] += 1
    ev = load(parent)
    # parent timeline of tool uses
    timeline = []  # (idx, kind, data)
    ptok_in = ptok_out = 0
    for idx, e in enumerate(ev):
        if e.get('type') == 'assistant':
            msg = e.get('message', {}); a, b = toks(msg.get('usage', {})); ptok_in += a; ptok_out += b
            tus = [c for c in msg.get('content', []) if isinstance(c, dict) and c.get('type') == 'tool_use']
            for c in tus: timeline.append((idx, c, msg.get('id')))
    S['parent_in'] += ptok_in
    spawns = [(i, c, mid) for i, c, mid in timeline if c['name'] in ('Agent', 'Task') and c['id'] in metas]
    # results of spawns
    results = {}
    for e in ev:
        if e.get('type') == 'user':
            for c in (e.get('message', {}).get('content') or []):
                if isinstance(c, dict) and c.get('type') == 'tool_result':
                    results[c.get('tool_use_id')] = len(json.dumps(c.get('content')))
    by_msg = collections.defaultdict(list)
    for i, c, mid in spawns: by_msg[mid].append(c)
    for i, c, mid in spawns:
        sub = load(metas[c['id']])
        S['spawns'] += 1
        prompt = (c['input'] or {}).get('prompt', '')
        L['prompt_chars'].append(len(prompt)); L['result_chars'].append(results.get(c['id'], 0))
        prompts.append(prompt)
        sfiles, sedit, calls, sin, sout = set(), False, 0, 0, 0
        first_edit_call, first_tool_before = None, 0
        for e in sub:
            if e.get('type') != 'assistant': continue
            msg = e.get('message', {}); a, b = toks(msg.get('usage', {})); sin += a; sout += b
            for t in msg.get('content', []):
                if isinstance(t, dict) and t.get('type') == 'tool_use':
                    calls += 1; sfiles |= files_of(t)
                    if is_edit(t) and first_edit_call is None: first_edit_call = calls
        S['sub_in'] += sin; S['sub_out'] += sout
        L['sub_calls'].append(calls)
        if first_edit_call: L['calls_before_first_edit'].append(first_edit_call - 1)
        pre = set(); post = set()
        for j, t, _ in timeline:
            if j < i: pre |= files_of(t)
        nxt = [j for j, t, _ in spawns if j > i]
        stop = min(nxt) if nxt else 10**9
        for j, t, _ in timeline:
            if i < j < stop and t['name'] not in ('Agent', 'Task'): post |= files_of(t)
        if sfiles:
            S['spawns_with_files'] += 1
            S['sub_files'] += len(sfiles)
            S['sub_files_parent_already_read'] += len(sfiles & pre)
            S['sub_files_parent_rereads_after'] += len(sfiles & post)
            if sfiles & pre: S['spawns_rediscover'] += 1
            if sfiles & post: S['spawns_parent_reverify'] += 1
        # did the prompt name file paths?
        if PATHRE.search(prompt): S['prompts_with_paths'] += 1
    # sibling overlap: spawns in the same assistant message
    for key, cs in by_msg.items():
        if len(cs) < 2: continue
        S['parallel_groups'] += 1
        sets = []
        for c in cs:
            fs = set()
            for e in load(metas[c['id']]):
                if e.get('type') == 'assistant':
                    for t in e.get('message', {}).get('content', []):
                        if isinstance(t, dict) and t.get('type') == 'tool_use': fs |= files_of(t)
            sets.append(fs)
        cnt = collections.Counter(f for s in sets for f in s)
        S['sib_files_total'] += sum(len(s) for s in sets)
        S['sib_files_dup_reads'] += sum(v - 1 for v in cnt.values() if v > 1)

def med(x): return st.median(x) if x else 0
print(json.dumps(S, indent=1))
for k, v in L.items(): print(k, 'n', len(v), 'median', med(v), 'p90', sorted(v)[int(len(v) * .9)] if v else 0, 'sum', sum(v))
tot = S['parent_in'] + S['sub_in']
print(f"subagent share of input tokens: {S['sub_in']/tot:.1%}")
print(f"sub files parent had already read: {S['sub_files_parent_already_read']/max(1,S['sub_files']):.1%} of sub file-touches; spawns affected {S['spawns_rediscover']/max(1,S['spawns_with_files']):.1%}")
print(f"parent re-touches sub's files after result: {S['sub_files_parent_rereads_after']/max(1,S['sub_files']):.1%}; spawns affected {S['spawns_parent_reverify']/max(1,S['spawns_with_files']):.1%}")
print(f"sibling duplicate file reads: {S['sib_files_dup_reads']/max(1,S['sib_files_total']):.1%} of sibling file-touches over {S['parallel_groups']} parallel groups")
print(f"prompts naming file paths: {S['prompts_with_paths']/max(1,S['spawns']):.1%}")
