#!/usr/bin/env python3
"""Render a session's tool transactions (what Claude sent, what came back) as markdown.
usage: transactions.py <transcript.jsonl> <title> full <out.md>
       transactions.py <transcript.jsonl> <title> pick 4,39,40 [cmd_lines] [reply_lines]"""
import sys
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from live_analyze import analyze

def clip(text, n):
    lines = text.splitlines()
    more = len(lines) - n
    return "\n".join(lines[:n]) + (f"\n… ({more} more lines)" if more > 0 else "")

def block(i, c, cmd_n, rep_n):
    dt = f"{c['t1'] - c['t0']:.1f} s" if c["t1"] and c["t0"] else "-"
    res = c.get("result", "")
    fence = "~~~~"
    return (f"**#{i} · turn {c['turn']} · {c['tool']} · {dt} · reply {len(res):,} B (~{len(res) // 4:,} tok){' · ERROR' if c['err'] else ''}**\n\n"
            f"Claude sent:\n{fence}\n{clip(c['full'], cmd_n)}\n{fence}\n\nReply:\n{fence}\n{clip(res, rep_n)}\n{fence}\n")

if __name__ == "__main__":
    path, title, mode = sys.argv[1:4]
    calls, *_ = analyze(path)
    if mode == "full":
        out = [f"# {title}: every transaction\n\nSource: `{path}`\n"]
        out += [block(i, c, 40, 40) for i, c in enumerate(calls, 1)]
        open(sys.argv[4], "w").write("\n".join(out))
        print(f"wrote {len(calls)} transactions to {sys.argv[4]}")
    else:
        cmd_n = int(sys.argv[5]) if len(sys.argv) > 5 else 14
        rep_n = int(sys.argv[6]) if len(sys.argv) > 6 else 14
        print("\n".join(block(int(i), calls[int(i) - 1], cmd_n, rep_n) for i in sys.argv[4].split(",")))
