#!/usr/bin/env python3
"""Verbose report for one interactive session transcript: every tool call (time, result bytes, ~tokens),
context per turn, usage, mt calls. usage: live_analyze.py <transcript.jsonl> <label> [--json]"""
import json, re, sys
from datetime import datetime

def ts(m):
    t = m.get("timestamp")
    return datetime.fromisoformat(t.replace("Z", "+00:00")).timestamp() if t else None

def text_of(c):
    if isinstance(c, str):
        return c
    return "".join(b.get("text", "") for b in c or [] if isinstance(b, dict) and b.get("type") == "text")

def analyze(path):
    msgs = [json.loads(l) for l in open(path) if l.strip()]
    calls, pending, turns, seen = [], {}, [], {}
    start = end = None
    for m in msgs:
        t = ts(m)
        if m.get("type") == "user" and start is None and isinstance(m.get("message", {}).get("content"), str):
            start = t
        if m.get("type") == "assistant":
            msg = m.get("message", {}); u = msg.get("usage", {}) or {}
            mid = msg.get("id")
            if mid not in seen:
                seen[mid] = dict(ctx=u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0),
                                 inp=u.get("input_tokens", 0), read=u.get("cache_read_input_tokens", 0),
                                 write=u.get("cache_creation_input_tokens", 0), out=u.get("output_tokens", 0), tools=0)
                turns.append(seen[mid])
            else:  # streamed chunks repeat the id; keep the largest output count
                seen[mid]["out"] = max(seen[mid]["out"], u.get("output_tokens", 0))
            for b in msg.get("content", []) or []:
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    inp = b.get("input", {}) or {}
                    desc = inp.get("command") or inp.get("file_path") or inp.get("pattern") or inp.get("skill") or json.dumps(inp)
                    c = dict(turn=len(turns), tool=b.get("name"), desc=" ".join(str(desc).split()), full=str(desc), t0=t, t1=None, out=0, err=False)
                    pending[b.get("id")] = c; calls.append(c); seen[mid]["tools"] += 1
            end = t or end
        elif m.get("type") == "user":
            for b in (m.get("message", {}) or {}).get("content", []) or []:
                if isinstance(b, dict) and b.get("type") == "tool_result" and b.get("tool_use_id") in pending:
                    c = pending[b["tool_use_id"]]; c["out"] = len(text_of(b.get("content"))); c["t1"] = t; c["err"] = bool(b.get("is_error"))
                    c["result"] = text_of(b.get("content"))
    final = next((text_of(m["message"]["content"]) for m in reversed(msgs) if m.get("type") == "assistant" and text_of(m["message"].get("content"))), "")
    return calls, turns, start, end, final

if __name__ == "__main__":
    path, label = sys.argv[1], sys.argv[2]
    calls, turns, start, end, final = analyze(path)
    mt = [c for c in calls if c["tool"] == "Bash" and re.search(r"(^|[;&|(]\s*|\s)mt\s+(query|apply|context|span|skeleton|grep|undo|index)", c["full"])]
    batches = [c for c in mt if re.search(r"mt\s+(query|apply)", c["full"])]
    denied = [c for c in calls if c["err"] and "mt " in c.get("result", "")]
    S = dict(label=label, turns=len(turns), calls=len(calls), wall=(end - start) if start and end else 0,
             inp=sum(t["inp"] for t in turns), read=sum(t["read"] for t in turns), write=sum(t["write"] for t in turns),
             out=sum(t["out"] for t in turns), peak=max((t["ctx"] for t in turns), default=0), first=turns[0]["ctx"] if turns else 0,
             mt=len(mt), batches=len(batches), denied=len(denied), result_bytes=sum(c["out"] for c in calls),
             by_tool={k: sum(1 for c in calls if c["tool"] == k) for k in sorted({c["tool"] for c in calls})},
             graft=sum(1 for c in calls if "graft" in c["full"]))
    # rates fitted exactly (50 headless Opus 5.5 runs, max rel err 1e-15): $/Mtok in 4, cache write 8, cache read 0.2, out 20
    S["cost"] = round((S["inp"] * 4 + S["write"] * 8 + S["read"] * 0.2 + S["out"] * 20) / 1e6, 3)
    if "--json" in sys.argv:
        print(json.dumps(S)); sys.exit()
    print(f"{'=' * 120}\n{label}   {path}\n{'=' * 120}")
    print(f"{'#':>3} {'turn':>4} {'tool':<9} {'secs':>6} {'result B':>9} {'~tok':>6}  command / target")
    for i, c in enumerate(calls, 1):
        dt = f"{c['t1'] - c['t0']:.1f}" if c["t1"] and c["t0"] else "-"
        print(f"{i:>3} {c['turn']:>4} {c['tool']:<9} {dt:>6} {c['out']:>9,} {c['out'] // 4:>6}  {'[ERR] ' if c['err'] else ''}{c['desc'][:160]}")
    print(f"\ncontext per turn (k tokens): " + " ".join(str(t['ctx'] // 1000) for t in turns))
    for k, v in S.items():
        if k != "label":
            print(f"  {k}: {v:,}" if isinstance(v, int) else f"  {k}: {v}")
    print(f"\nFINAL ANSWER ({len(final)} chars):\n{final[:4000]}")
