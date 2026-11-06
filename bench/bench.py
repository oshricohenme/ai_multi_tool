#!/usr/bin/env python3
"""Traditional (grep -n + sed -n range) vs `mt span`, on real repos.

For each sampled definition we measure, per approach:
  calls      shell round trips the agent would make
  bytes      what lands in the agent's context
  complete   the returned text contains the whole definition (oracle span)
  precision  oracle lines / returned lines
  ms         wall time of the subprocesses

The oracle is independent of tree-sitter: a declaration-line regex plus brace
matching (Go, TS, C#), indentation (Python) or heading levels (Markdown).
Disagreements between mt and the oracle are listed, not hidden.
"""
import os, random, re, statistics, subprocess, sys, time, json, collections

random.seed(7)
MT = os.path.expanduser("~/orca/mt/target/release/mt")
HOME = os.path.expanduser("~")
CORPUS = {
    "go": [f"{HOME}/orca/claude_coach", f"{HOME}/orca/super_scheduler"],
    "py": [f"{HOME}/orca/mixar-app"],
    "cs": [f"{HOME}/orca/freindly_shooter", f"{HOME}/orca/fps_europe"],
    "ts": [f"{HOME}/orca/watchly", f"{HOME}/orca/claude_coach/web"],
    "md": [f"{HOME}/orca/claude_coach", f"{HOME}/orca/fps_europe", f"{HOME}/orca/gym_software"],
}
EXT = {"go": (".go",), "py": (".py",), "cs": (".cs",), "ts": (".ts", ".tsx"), "md": (".md",)}
PER_LANG = int(sys.argv[1]) if len(sys.argv) > 1 else 60
SKIP_DIRS = {"node_modules", ".git", "vendor", "dist", "build", "target", "Library", ".next", "__pycache__", "graft", "Specs"}

def files_for(lang):
    out = []
    for root in CORPUS[lang]:
        for dp, dns, fns in os.walk(root):
            dns[:] = [d for d in dns if d not in SKIP_DIRS and not d.startswith(".")]
            for fn in fns:
                if fn.endswith(EXT[lang]) and not fn.endswith((".d.ts", ".min.js", ".g.cs", ".Designer.cs")):
                    p = os.path.join(dp, fn)
                    try:
                        n = sum(1 for _ in open(p, errors="replace"))
                    except OSError:
                        continue
                    if 40 <= n <= 4000:
                        out.append(p)
    return out

# ---------------------------------------------------------------- oracle

COMMENT = re.compile(r"^\s*(//|/\*|\*|#|@|\[.*\]\s*$)")

def decl_regex(lang, name):
    n = re.escape(name)
    if lang == "go":
        return re.compile(rf"^(func(\s*\([^)]*\))?\s+{n}\s*[\(\[]|type\s+{n}\b|(const|var)\s+{n}\b|\s+{n}\s+=)")
    if lang == "py":
        return re.compile(rf"^\s*(async\s+)?(def|class)\s+{n}\b")
    if lang == "ts":
        return re.compile(rf"(^|\s)(function\*?\s+{n}\b|(class|interface|type|enum)\s+{n}\b|(const|let|var)\s+{n}\s*[=:<]|{n}\s*[(<]|(get|set)\s+{n}\s*\(|{n}\s*=\s*(async\s*)?\(|{n}\s*:\s*(async\s*)?\()")
    if lang == "cs":
        return re.compile(rf"(\b(class|struct|interface|enum|record|delegate)\s+{n}\b|\b{n}\s*\(|\b{n}\s*(\{{|=>|;|=))")
    return re.compile(rf"^#{{1,6}}\s+{n}\s*#*\s*$")

def brace_end(lines, start):
    depth = 0; seen = False
    in_block = False
    if lines[start].count("`") % 2 == 1:  # Go raw string literal
        for i in range(start + 1, len(lines)):
            if "`" in lines[i]: return i
    for i in range(start, len(lines)):
        l = lines[i]
        # strip strings and line comments crudely
        l = re.sub(r'"(\\.|[^"\\])*"|\'(\\.|[^\'\\])*\'|`[^`]*`', '""', l)
        l = re.sub(r"//.*", "", l)
        if not seen:
            code = l.strip()
            if code.endswith(";"):
                return i  # one-liner: field, type alias, abstract method, arrow body
            if not code.endswith("{") and not code.endswith("{}") and "{" in code and "}" in code:
                continue  # braces inside the signature (destructuring, generics) are not the body
        j = 0
        while j < len(l):
            if in_block:
                k = l.find("*/", j)
                if k < 0: break
                in_block = False; j = k + 2; continue
            c = l[j]
            if l.startswith("/*", j):
                in_block = True; j += 2; continue
            if c == "{": depth += 1; seen = True
            elif c == "}":
                depth -= 1
                if seen and depth == 0: return i
            j += 1
        if not seen and (l.rstrip().endswith(";") or (l.strip() and i > start and not l.rstrip().endswith((",", "(", "=", "&&", "||", "+", "?", ":")))):
            return i  # one-liner (field, type alias, abstract method)
    return len(lines) - 1

def py_end(lines, start):
    indent = len(lines[start]) - len(lines[start].lstrip())
    end = start
    for i in range(start + 1, len(lines)):
        l = lines[i]
        if not l.strip(): continue
        if len(l) - len(l.lstrip()) <= indent and not l.lstrip().startswith((")", "]", "}")): break
        end = i
    return end

def oracle(lang, lines, name, hint_line):
    """Declaration line nearest to hint_line (mt's start, so the oracle resolves the same overload) → span."""
    rx = decl_regex(lang, name)
    cands = [i for i, l in enumerate(lines) if rx.search(l)]
    if not cands: return None
    s = min(cands, key=lambda i: abs(i - hint_line))
    if lang == "md":
        level = len(lines[s]) - len(lines[s].lstrip("#"))
        e = len(lines) - 1
        fence = False
        for i in range(s + 1, len(lines)):
            t = lines[i].lstrip()
            if t.startswith(("```", "~~~")): fence = not fence; continue
            if fence: continue
            m = re.match(r"^(#{1,6})\s", t)
            if m and len(m.group(1)) <= level: e = i - 1; break
        while e > s and not lines[e].strip(): e -= 1
        return s, e
    e = py_end(lines, s) if lang == "py" else brace_end(lines, s)
    while s > 0 and COMMENT.match(lines[s - 1]): s -= 1
    return s, e

# ---------------------------------------------------------------- runners

def sh(cmd):
    t = time.perf_counter()
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, errors="replace")
    return r.stdout + r.stderr, (time.perf_counter() - t) * 1000

def run_mt(path, name, hint):
    t = time.perf_counter()
    r = subprocess.run([MT, "span", path, name], capture_output=True, text=True, errors="replace")
    ms = (time.perf_counter() - t) * 1000
    spans = [(int(m.group(1)) - 1, int(m.group(2)) - 1) for m in re.finditer(r"(?m)^" + re.escape(path) + r" L(\d+)-(\d+) \S+ ", r.stdout)]
    best = min(spans, key=lambda s: abs(s[0] - hint)) if spans else None
    return r.stdout, ms, best

def pick_line(lang, grep_out, name, smart):
    """What line the agent would choose from the grep listing."""
    rx = decl_regex(lang, name)
    hits = [(int(m.group(1)), m.group(2)) for m in re.finditer(r"(?m)^(\d+):(.*)$", grep_out)]
    if not hits: return None
    if smart:
        for ln, txt in hits:
            if rx.search(txt): return ln - 1
    return hits[0][0] - 1

def run_trad(path, lines, lang, name, window, smart=True):
    q = name if lang != "md" else name
    g_out, g_ms = sh(f"grep -n {json.dumps(q)} {json.dumps(path)}")
    ln = pick_line(lang, g_out, name, smart)
    if ln is None:
        return g_out, g_ms, None, 1
    if window == "cat":
        s_out, s_ms = sh(f"cat -n {json.dumps(path)}")
        return g_out + s_out, g_ms + s_ms, (0, len(lines) - 1), 2
    s, e = max(0, ln - 2), min(len(lines) - 1, ln + window)
    s_out, s_ms = sh(f"sed -n '{s+1},{e+1}p' {json.dumps(path)}")
    return g_out + s_out, g_ms + s_ms, (s, e), 2

# ---------------------------------------------------------------- main

def sample_defs(lang, files):
    """Candidates come from mt's skeleton, but the oracle re-derives every span independently."""
    picks = []
    random.shuffle(files)
    for p in files:
        out = subprocess.run([MT, "skeleton", p], capture_output=True, text=True, errors="replace").stdout
        defs, stack = [], []
        for m in re.finditer(r"(?m)^( *)(\S+) (\S+) L(\d+)-(\d+)$", out):
            depth = len(m.group(1)) // 2
            stack = stack[:depth] + [m.group(3)]
            qn = m.group(3) if "." in m.group(3) or lang == "md" else ".".join(stack[-2:])
            defs.append((m.group(2), qn, int(m.group(4)) - 1, int(m.group(5)) - 1))
        defs = [d for d in defs if d[3] - d[2] >= 2]
        if not defs: continue
        picks.append((p, random.choice(defs)))
        if len(picks) >= PER_LANG: break
    return picks

APPROACHES = ["mt", "trad+60", "trad+150", "trad-naive+60", "cat"]
rows = []
disagree = []
for lang in CORPUS:
    files = files_for(lang)
    if not files:
        print(f"[{lang}] no files"); continue
    for path, (kind, qname, s0, e0) in sample_defs(lang, files):
        lines = open(path, errors="replace").read().split("\n")
        name = qname.split(".")[-1]
        truth = oracle(lang, lines, name, s0)
        if truth is None:
            disagree.append((lang, path, qname, None, (s0, e0), "oracle found no declaration line"))
            continue
        ts, te = truth
        truth_lines = te - ts + 1
        def score(span, out, ms, calls, approach):
            if span is None:
                rows.append(dict(lang=lang, approach=approach, calls=calls, bytes=len(out), complete=0, precision=0.0, ms=ms)); return
            a, b = span
            complete = int(a <= ts and b >= te)
            overlap = max(0, min(b, te) - max(a, ts) + 1)
            rows.append(dict(lang=lang, approach=approach, calls=calls, bytes=len(out), complete=complete,
                             precision=truth_lines / (b - a + 1) if complete else overlap / (b - a + 1), ms=ms))
        out, ms, span = run_mt(path, qname, s0)
        score(span, out, ms, 1, "mt")
        if span != truth:
            disagree.append((lang, path, qname, truth, span, "mt vs oracle"))
        for w, smart, label in ((60, True, "trad+60"), (150, True, "trad+150"), (60, False, "trad-naive+60"), ("cat", True, "cat")):
            out, ms, span, calls = run_trad(path, lines, lang, name, w, smart)
            score(span, out, ms, calls, label)

def agg(sel):
    if not sel: return "-"
    med = lambda k: statistics.median(r[k] for r in sel)
    return (f"n={len(sel):>3} calls={med('calls'):.0f} complete={sum(r['complete'] for r in sel)/len(sel)*100:5.1f}% "
            f"precision={statistics.mean(r['precision'] for r in sel)*100:5.1f}% "
            f"bytes(med)={med('bytes'):>6.0f} bytes(p90)={sorted(r['bytes'] for r in sel)[int(len(sel)*.9)]:>6} "
            f"ms(med)={med('ms'):5.1f}")

print(f"\n== OVERALL ({len({(r['lang']) for r in rows})} languages) ==")
for a in APPROACHES:
    print(f"  {a:<14} {agg([r for r in rows if r['approach']==a])}")
for lang in CORPUS:
    print(f"\n== {lang} ==")
    for a in APPROACHES:
        print(f"  {a:<14} {agg([r for r in rows if r['approach']==a and r['lang']==lang])}")

tot = collections.defaultdict(int)
for r in rows: tot[r["approach"]] += r["bytes"]
print("\n== TOTAL BYTES INTO CONTEXT, same queries ==")
for a in APPROACHES:
    print(f"  {a:<14} {tot[a]/1000:8.0f} KB  ({tot[a]/tot['mt']:.1f}x mt)")

print(f"\n== mt vs oracle disagreements: {len(disagree)} of {len(rows)//len(APPROACHES)} (listed, review by hand) ==")
for lang, path, name, truth, got, why in disagree[:40]:
    fmt = lambda s: f"L{s[0]+1}-{s[1]+1}" if s else "none"
    print(f"  [{lang}] {name:<32} oracle={fmt(truth):<12} mt={fmt(got):<12} {why}  {path.replace(HOME, '~')}")
json.dump({"rows": rows, "disagree": disagree}, open(os.path.join(os.path.dirname(__file__), "last_run.json"), "w"))
