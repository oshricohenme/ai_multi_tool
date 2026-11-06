#!/usr/bin/env python3
"""Per-session totals including subagents: turns, context processed, tokens, cost, wall time.
usage: cm_analyze.py <label>=<main transcript.jsonl> ...   (prints a table and JSON)"""
import glob, json, os, sys

RATES = dict(inp=4, write=8, read=0.2, out=20)  # $/Mtok, fitted exactly from 50 Opus 5.5 runs (section 16)

def usage(path):
    seen, order = {}, []
    for l in open(path, errors="replace"):
        try: m = json.loads(l)
        except Exception: continue
        if m.get("type") != "assistant": continue
        msg = m.get("message", {}); u = msg.get("usage") or {}; i = msg.get("id")
        row = dict(inp=u.get("input_tokens", 0), read=u.get("cache_read_input_tokens", 0), write=u.get("cache_creation_input_tokens", 0), out=u.get("output_tokens", 0))
        if i not in seen: seen[i] = row; order.append(i)
        else: seen[i]["out"] = max(seen[i]["out"], row["out"])
    return [seen[i] for i in order]

def wall(path):
    return sum(json.loads(l)["durationMs"] for l in open(path, errors="replace") if '"turn_duration"' in l) / 1000

def session(path):
    main = usage(path)
    subs = sorted(glob.glob(os.path.join(path[:-6], "subagents", "*.jsonl")))
    sub_turns = [t for s in subs for t in usage(s)]
    allt = main + sub_turns
    ctx = lambda t: t["inp"] + t["read"] + t["write"]
    tot = {k: sum(t[k] for t in allt) for k in ("inp", "read", "write", "out")}
    cost = sum(tot[k] * RATES[k] for k in tot) / 1e6
    tools = sum(1 for l in open(path, errors="replace") for _ in [0] if '"tool_use"' in l)
    return dict(main_turns=len(main), sub_turns=len(sub_turns), subagents=len(subs), processed=sum(ctx(t) for t in allt),
                main_processed=sum(ctx(t) for t in main), peak_main=max((ctx(t) for t in main), default=0), start=ctx(main[0]) if main else 0,
                out=tot["out"], read=tot["read"], write=tot["write"], cost=round(cost, 3), wall=round(wall(path), 1))

if __name__ == "__main__":
    rows = {}
    for a in sys.argv[1:]:
        label, path = a.split("=", 1)
        rows[label] = session(path)
    hdr = f"{'run':<26} {'turns main+sub':>14} {'agents':>6} {'context processed':>17} {'peak main':>9} {'output':>8} {'cache read':>10} {'cache write':>11} {'cost $':>7} {'wall s':>7}"
    print(hdr)
    for k, r in rows.items():
        print(f"{k:<26} {r['main_turns']:>6}+{r['sub_turns']:<7} {r['subagents']:>6} {r['processed']:>17,} {r['peak_main']:>9,} {r['out']:>8,} {r['read']:>10,} {r['write']:>11,} {r['cost']:>7.3f} {r['wall']:>7.1f}")
    print(json.dumps(rows))
