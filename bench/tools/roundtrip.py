import json,glob,os,collections,datetime as dt,statistics as st
root=os.path.expanduser('~/.claude/projects')
files=[f for f in glob.glob(root+'/*/*.jsonl') if 'scratchpad' not in f]
ts=lambda s: dt.datetime.fromisoformat(s.replace('Z','+00:00'))
solo=0; mixed=0; reread=[]; gaps=[]; errgaps=[]; errreread=[]; names=collections.Counter(); qs=collections.Counter()
for f in files:
    try: L=[json.loads(l) for l in open(f)]
    except Exception: continue
    # group assistant blocks by message id, in order
    msgs=[]; idx={}
    for d in L:
        if d.get('type')=='assistant' and not d.get('isSidechain'):
            mid=d['message'].get('id')
            if mid not in idx: idx[mid]=len(msgs); msgs.append({'ts':d['timestamp'],'usage':d['message'].get('usage',{}),'tools':[]})
            for c in d['message'].get('content',[]):
                if c.get('type')=='tool_use': msgs[idx[mid]]['tools'].append((c['name'],c.get('input',{}),c['id']))
    errids=set()
    for d in L:
        if d.get('type')=='user' and isinstance(d['message'].get('content'),list):
            for x in d['message']['content']:
                if x.get('type')=='tool_result' and x.get('is_error') and 'InputValidationError' in json.dumps(x.get('content','')): errids.add(x['tool_use_id'])
    for i,m in enumerate(msgs[:-1]):
        nm=[t[0] for t in m['tools']]
        nxt=msgs[i+1]; u=nxt['usage']; tok=u.get('input_tokens',0)+u.get('cache_read_input_tokens',0)+u.get('cache_creation_input_tokens',0)
        gap=(ts(nxt['ts'])-ts(m['ts'])).total_seconds()
        if 'ToolSearch' in nm:
            for t in m['tools']:
                if t[0]=='ToolSearch': qs[t[1].get('query','')[:70]]+=1
            if all(n=='ToolSearch' for n in nm):
                solo+=1; reread.append(tok)
                if 0<gap<600: gaps.append(gap)
            else: mixed+=1
        if any(t[2] in errids for t in m['tools']):
            errreread.append(tok)
            if 0<gap<600: errgaps.append(gap)
print('ToolSearch-only turns',solo,'mixed with other tools',mixed)
print('  context re-read by the follow-up request: total %s tok, median %s'%(f'{sum(reread):,}',st.median(reread)))
print('  wall time ToolSearch turn -> next turn: median %.1fs, total %.1f min'%(st.median(gaps),sum(gaps)/60))
print('unloaded-tool error turns',len(errreread),'re-read %s tok, %.1f min'%(f'{sum(errreread):,}',sum(errgaps)/60))
print('top queries:'); [print(f'  {v:4} {k}') for k,v in qs.most_common(15)]
