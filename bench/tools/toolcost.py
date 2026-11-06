import json,glob,os,collections,datetime as dt,statistics as st
root=os.path.expanduser('~/.claude/projects')
files=[f for f in glob.glob(root+'/*/*.jsonl') if 'scratchpad' not in f]
def ts(s): return dt.datetime.fromisoformat(s.replace('Z','+00:00'))
def blen(c):
    if isinstance(c,str): return len(c)
    if isinstance(c,list): return sum(len(x.get('text','')) if isinstance(x,dict) else len(str(x)) for x in c)
    return len(json.dumps(c))
S=collections.Counter(); per=[]; ts_lat=[]; ts_out=[]; errs=collections.Counter(); hookby=collections.Counter()
baselines=[]; static=collections.defaultdict(list)
for f in files:
    try: lines=[json.loads(l) for l in open(f)]
    except Exception: continue
    asst=[d for d in lines if d.get('type')=='assistant' and not d.get('isSidechain')]
    if len(asst)<2: continue
    S['sessions']+=1
    u=asst[0]['message'].get('usage',{})
    b=u.get('input_tokens',0)+u.get('cache_creation_input_tokens',0)+u.get('cache_read_input_tokens',0)
    if b: baselines.append(b)
    seen=set(); first=collections.Counter()
    tool_ids={}; prev_ts=None; session_tok=0; turns=0
    for d in lines:
        t=d.get('type')
        if t=='attachment':
            a=d['attachment']; at=a.get('type')
            if at in('skill_listing',): n=len(a.get('content',''))
            elif at=='mcp_instructions_delta': n=sum(len(x) for x in a.get('addedBlocks',[]))
            elif at=='agent_listing_delta': n=len(json.dumps(a))
            elif at=='deferred_tools_delta': n=len(json.dumps(a.get('addedNames',[])))
            elif at in('hook_additional_context','hook_success'):
                n=blen(a.get('content','')); hookby[(a.get('hookName') or at).split(':')[0]]+=n
                S['hookchars_'+((a.get('hookName') or '?').split(':')[0])]+=n
                if 'SessionStart' not in (a.get('hookName') or ''): continue
            elif at=='invoked_skills': n=sum(len(s.get('content','')) for s in a.get('skills',[])); S['skill_body_chars']+=n; continue
            else: continue
            if at not in seen or at.startswith('hook'):
                first[at]+=n; seen.add(at)
        if t=='assistant' and not d.get('isSidechain'):
            m=d['message']; u=m.get('usage',{}); turns+=1
            session_tok+=u.get('input_tokens',0)+u.get('cache_creation_input_tokens',0)+u.get('cache_read_input_tokens',0)
            for c in m.get('content',[]):
                if c.get('type')=='tool_use':
                    tool_ids[c['id']]=c['name']; S['calls']+=1
                    if c['name']=='ToolSearch':
                        S['toolsearch']+=1; ts_out.append(u.get('output_tokens',0))
                        if prev_ts: ts_lat.append((ts(d['timestamp'])-prev_ts).total_seconds())
                    if c['name']=='Skill': S['skill_calls']+=1
        if t=='user':
            c=d['message'].get('content')
            if isinstance(c,list):
                for x in c:
                    if x.get('type')=='tool_result':
                        nm=tool_ids.get(x.get('tool_use_id'),'?'); body=blen(x.get('content',''))
                        if nm=='ToolSearch': S['ts_result_chars']+=body
                        if nm=='Skill': S['skill_result_chars']+=body
                        txt=json.dumps(x.get('content',''))
                        if x.get('is_error') and ('InputValidationError' in txt or 'not loaded' in txt or 'No such tool' in txt or 'ToolSearch' in txt):
                            errs['unloaded/invalid tool call']+=1
                        if x.get('is_error') and 'No matching deferred tools' in txt: errs['toolsearch miss']+=1
        if d.get('timestamp') and t in('user','assistant'): prev_ts=ts(d['timestamp'])
    for k,v in first.items(): static[k].append(v)
    S['session_tokens']+=session_tok; S['turns']+=turns
    per.append((f,b,turns))
n=S['sessions']
print('sessions',n,'turns',S['turns'],'tool calls',S['calls'])
print('baseline first-turn tokens: median',st.median(baselines),'max',max(baselines))
print('\nper-session static injections (median chars ~ /4 tokens):')
for k,v in sorted(static.items(),key=lambda kv:-st.median(kv[1])): print(f'  {k:28} median {st.median(v):>8.0f} chars  (~{st.median(v)/4:,.0f} tok)  in {len(v)} sessions')
print('\nhook text injected (all events, chars):')
for k,v in hookby.most_common(): print(f'  {k:22} {v:>12,}  per session ~{v/n/4:,.0f} tok')
print('\nToolSearch calls',S['toolsearch'],'per session',round(S['toolsearch']/n,2))
print('  result chars total',S['ts_result_chars'],'(~%d tok)'%(S['ts_result_chars']/4))
if ts_lat: print('  model turn time before ToolSearch: median %.1fs mean %.1fs total %.1f min'%(st.median(ts_lat),st.mean(ts_lat),sum(ts_lat)/60))
if ts_out: print('  output tokens of ToolSearch turns: total',sum(ts_out))
print('Skill calls',S['skill_calls'],'skill body chars',S['skill_body_chars'],'skill tool_result chars',S['skill_result_chars'])
print('errors',dict(errs))
print('total processed input tokens',f"{S['session_tokens']:,}")
