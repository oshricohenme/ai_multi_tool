#!/usr/bin/env python3
"""Traditional agent workflow vs mt, on real symbols from real repos.

Part A, gather context for a change to symbol X:
  traditional  what transcripts show agents doing: grep -rn for the name, read a window around
               the definition, then a window around each caller file and each test file,
               one call each (capped at 5 callers and 3 tests, as agents usually stop there)
  mt           one call: mt context X

Part B, rename X everywhere and prove it builds (scratch copy of a Go repo):
  traditional  grep -rlw, then per file: read the hit region, sed -i the file; then go build
  mt           mt context X, then one mt apply with a rename op and "check": "go build ./..."

Measured: wall time of every command, number of calls, bytes returned, coverage.
Modelled (labelled as such): end-to-end agent time and context cost, using medians from this
user's own transcripts:
  MODEL_TURN_S   3.8 s  median model latency for a read/grep turn
  OUT_TOKENS     403    median output tokens (incl. thinking) for a read/grep turn
  BASE_CONTEXT   261k   median context size at the moment of a grep
  LATER_CALLS    53     median further calls in the session after a grep, before compaction
Cost is in input-token equivalents with assumed price ratios: cache read 0.1x,
cache write 1.25x, output 5x. Each call re-reads the whole context (cache read), writes its
own result and output into the cache, and that new content is then re-read by every later call.
"""
import json, os, random, re, sqlite3, statistics as S, subprocess, sys, time, shutil

random.seed(11)
MT = os.path.expanduser("~/orca/mt/target/release/mt")
HOME = os.path.expanduser("~")
REPOS = [f"{HOME}/orca/claude_coach", f"{HOME}/orca/super_scheduler", f"{HOME}/orca/watchly"]
PER_REPO = int(sys.argv[1]) if len(sys.argv) > 1 else 10
CAP = 30000  # Claude Code truncates a tool result at ~30k chars
MODEL_TURN_S, OUT_TOKENS, BASE_CONTEXT, LATER_CALLS = 3.8, 403, 261_000, 53
R_READ, R_WRITE, R_OUT = 0.1, 1.25, 5.0
EXCLUDE = "--exclude-dir=node_modules --exclude-dir=.git --exclude-dir=vendor --exclude-dir=dist --exclude-dir=build --exclude-dir=.next --exclude-dir=.mt --exclude-dir=graft"
CODE_INC = " ".join(f"--include=*.{e}" for e in ["go", "ts", "tsx", "js", "py", "cs", "sql", "rs", "java", "swift"])

def sh(cmd, cwd):
    t = time.perf_counter()
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, errors="replace")
    out = (r.stdout + r.stderr)[:CAP]
    return out, (time.perf_counter() - t) * 1000

def tokens(s):
    return len(s) / 4

def cost_of(calls):
    """calls: list of result sizes in tokens. Returns (per-call rows, total)."""
    ctx, rows, total = BASE_CONTEXT, [], 0.0
    for i, r in enumerate(calls):
        read = ctx * R_READ
        write = (r + OUT_TOKENS) * R_WRITE
        out = OUT_TOKENS * R_OUT
        carry = (r + OUT_TOKENS) * R_READ * LATER_CALLS
        rows.append(dict(call=i + 1, result=r, ctx=ctx, read=read, write=write, out=out, carry=carry, sum=read + write + out + carry))
        total += read + write + out + carry
        ctx += r + OUT_TOKENS
    return rows, total

def sample(repo):
    db = sqlite3.connect(os.path.join(repo, ".mt/index.sqlite"))
    rows = db.execute("""
      SELECT d.last, f.path, d.start, d.end, d.head FROM defs d JOIN files f ON f.id=d.file
      WHERE f.role='source' AND d.kind IN ('function','method','func')
        AND d.end - d.start BETWEEN 6 AND 150 AND length(d.last) >= 6""").fetchall()
    random.shuffle(rows)
    out = []
    for last, path, s, e, h in rows:
        if db.execute("SELECT COUNT(DISTINCT file) FROM defs WHERE last=?", (last,)).fetchone()[0] != 1:
            continue
        files = {p for (p,) in db.execute("SELECT f.path FROM refs r JOIN files f ON f.id=r.file WHERE r.name=? AND f.role IN ('source','test','integration','model','query')", (last,))}
        if len(files - {path}) < 2:
            continue
        out.append(dict(name=last, path=path, start=s, end=e, head=h))
        if len(out) >= PER_REPO:
            break
    return out

def truth(repo, sym):
    out, _ = sh(f"grep -rnw {EXCLUDE} {CODE_INC} {sym['name']} .", repo)
    hits = {}
    for m in re.finditer(r"(?m)^\./([^:]+):(\d+):", out):
        p, l = m.group(1), int(m.group(2)) - 1
        if p == sym["path"] and sym["start"] <= l <= sym["end"]:
            continue
        hits.setdefault(p, []).append(l)
    tests = {p for p in hits if re.search(r"(_test\.go|\.test\.|\.spec\.|/tests?/|__tests__|e2e|test_)", p)}
    return hits, tests

LOG = []
def step(steps, cmd, out, t):
    steps.append((cmd, t, len(out)))

def traditional(repo, sym):
    calls, ms, seen, steps = [], 0.0, {}, []
    c = f"grep -rn <excludes> <code includes> {sym['name']} ."
    out, t = sh(f"grep -rn {EXCLUDE} {CODE_INC} {sym['name']} .", repo)
    calls.append(out); ms += t; step(steps, c, out, t)
    hits = {}
    for m in re.finditer(r"(?m)^\./([^:]+):(\d+):", out):
        hits.setdefault(m.group(1), []).append(int(m.group(2)) - 1)
    # read the definition: a 60-line window from the declaration, the usual agent habit
    s, e = max(0, sym["head"] - 2), sym["head"] + 60
    c = f"sed -n '{s+1},{e+1}p' {sym['path']}"
    out, t = sh(c, repo); calls.append(out); ms += t; step(steps, c, out, t)
    seen[sym["path"]] = [(s, e)]
    others = [p for p in hits if p != sym["path"]]
    tests = [p for p in others if re.search(r"(_test\.go|\.test\.|\.spec\.|/tests?/|__tests__|e2e|test_)", p)]
    callers = [p for p in others if p not in tests]
    for p in callers[:5]:
        l = hits[p][0]
        c = f"sed -n '{max(1, l-14)},{l+16}p' {p}"
        out, t = sh(c, repo); calls.append(out); ms += t; step(steps, c, out, t)
        seen.setdefault(p, []).append((l - 15, l + 15))
    for p in tests[:3]:
        l = hits[p][0]
        c = f"sed -n '{max(1, l-39)},{l+41}p' {p}"
        out, t = sh(c, repo); calls.append(out); ms += t; step(steps, c, out, t)
        seen.setdefault(p, []).append((l - 40, l + 40))
    return calls, ms, seen, steps

def mt_context(repo, sym):
    c = f"mt context {sym['name']} --budget 6000"
    out, t = sh(f"{MT} context {sym['name']} --budget 6000", repo)
    steps = [(c, t, len(out))]
    head = out.splitlines()[0] if out else ""
    seen = {}
    for m in re.finditer(r"(?m)^\[\d+\] (\S+):L(\d+)-(\d+) ", out):
        seen.setdefault(m.group(1), []).append((int(m.group(2)) - 1, int(m.group(3)) - 1))
    return [out], t, seen, steps, head

def coverage(sym, seen, hits, tests):
    def_ok = any(a <= sym["start"] and b >= sym["end"] for a, b in seen.get(sym["path"], []))
    callers = [p for p in hits if p not in tests and p != sym["path"]]
    c_cov = sum(1 for p in callers if p in seen) / len(callers) if callers else 1.0
    t_cov = sum(1 for p in tests if p in seen) / len(tests) if tests else None
    missed = [p for p in callers if p not in seen] + [p for p in tests if p not in seen]
    return def_ok, c_cov, t_cov, len(callers), len(tests), missed

def part_a():
    rows = []
    example = None
    for repo in REPOS:
        if not os.path.exists(os.path.join(repo, ".mt/index.sqlite")):
            subprocess.run([MT, "index"], cwd=repo, capture_output=True)
        subprocess.run([MT, "index"], cwd=repo, capture_output=True)  # warm
        for sym in sample(repo):
            hits, tests = truth(repo, sym)
            ncall = len([p for p in hits if p not in tests and p != sym["path"]])
            print(f"\n--- [{os.path.basename(repo)}] {sym['name']}  defined {sym['path']}:L{sym['start']+1}-{sym['end']+1} "
                  f"({sym['end']-sym['start']+1} lines); ground truth: {ncall} caller files, {len(tests)} test files", flush=True)
            res = {}
            for approach, fn in (("traditional", traditional), ("mt", mt_context)):
                r = fn(repo, sym)
                calls, ms, seen, steps = r[0], r[1], r[2], r[3]
                sizes = [tokens(c) for c in calls]
                _, cost = cost_of(sizes)
                d, cc, tc, nc, nt, missed = coverage(sym, seen, hits, tests)
                print(f"  {approach}:")
                for i, (c, t, b) in enumerate(steps, 1):
                    print(f"    {i:>2}. {c[:92]:<92} {t:7.1f} ms {b:>7,} B {b/4:>6,.0f} tok")
                if approach == "mt" and len(r) > 4:
                    print(f"        mt says: {r[4][2:150]}")
                print(f"      => {len(calls)} calls, {ms:.1f} ms of commands, {sum(sizes):,.0f} tokens into context | "
                      f"definition whole: {'yes' if d else 'NO'} | callers {cc*nc:.0f}/{nc} | tests {('-' if tc is None else f'{tc*nt:.0f}/{nt}')}"
                      + (f" | missed: {', '.join(missed[:4])}{' ...' if len(missed) > 4 else ''}" if missed else ""))
                print(f"      => modelled: {ms/1000 + len(calls)*MODEL_TURN_S:.1f} s agent time, {cost/1000:,.1f}k token-equivalents", flush=True)
                res[approach] = (ms, len(calls), cost)
                rows.append(dict(repo=os.path.basename(repo), sym=sym["name"], approach=approach, calls=len(calls), cmd_ms=ms,
                                 agent_s=ms / 1000 + len(calls) * MODEL_TURN_S, tokens=sum(sizes), cost=cost,
                                 def_ok=d, callers=cc, tests=tc, n_callers=nc, n_tests=nt))
                if example is None and approach == "traditional" and len(calls) >= 6:
                    example = (sym, repo, sizes)
            (tm, tc_, tco), (mm, mc, mco) = res["traditional"], res["mt"]
            print(f"  verdict: mt {tm/max(mm,0.1):.1f}x faster on commands, {tc_-mc} fewer calls, {tco/mco:.1f}x cheaper in context")
    return rows, example

def summarize(rows, title):
    print(f"\n== {title} ==")
    print(f"{'approach':<12} {'n':>3} {'calls':>6} {'cmd ms':>8} {'agent s*':>9} {'tokens in':>10} {'cost* (k)':>10} {'def whole':>9} {'callers':>8} {'tests':>7}")
    for a in ("traditional", "mt"):
        sel = [r for r in rows if r["approach"] == a]
        if not sel:
            continue
        tc = [r["tests"] for r in sel if r["tests"] is not None]
        print(f"{a:<12} {len(sel):>3} {S.mean(r['calls'] for r in sel):>6.1f} {S.median(r['cmd_ms'] for r in sel):>8.1f} "
              f"{S.median(r['agent_s'] for r in sel):>9.1f} {S.median(r['tokens'] for r in sel):>10.0f} {S.median(r['cost'] for r in sel)/1000:>10.1f} "
              f"{sum(r['def_ok'] for r in sel)/len(sel)*100:>8.0f}% {S.mean(r['callers'] for r in sel)*100:>7.0f}% {(S.mean(tc)*100 if tc else float('nan')):>6.0f}%")

def part_b():
    """Rename on a scratch copy of a Go repo; verify it builds both ways."""
    src = f"{HOME}/orca/claude_coach"
    scratch = os.environ.get("MT_SCRATCH", "/tmp/mt-bench-b")
    shutil.rmtree(scratch, ignore_errors=True)
    os.makedirs(scratch)
    files = subprocess.run("git ls-files", shell=True, cwd=src, capture_output=True, text=True).stdout.split("\n")
    for f in files:
        if not f or f.startswith("web/") or not os.path.isfile(os.path.join(src, f)):
            continue
        d = os.path.join(scratch, os.path.dirname(f)); os.makedirs(d, exist_ok=True)
        shutil.copy2(os.path.join(src, f), os.path.join(scratch, f))
    for c in ("git init -q", "git add -A", "git -c user.email=b@b -c user.name=b commit -qm base", "go build ./..."):
        subprocess.run(c, shell=True, cwd=scratch, capture_output=True)
    subprocess.run([MT, "index"], cwd=scratch, capture_output=True)
    db = sqlite3.connect(os.path.join(scratch, ".mt/index.sqlite"))
    cands = db.execute("""SELECT d.last FROM defs d JOIN files f ON f.id=d.file WHERE f.path LIKE '%.go' AND f.role='source'
        AND d.kind IN ('function','method') AND length(d.last)>=7 GROUP BY d.last HAVING COUNT(*)=1""").fetchall()
    random.shuffle(cands)
    picked = []
    for (n,) in cands:
        out, _ = sh(f"grep -rlw --include=*.go {n} .", scratch)
        nf = len([l for l in out.split("\n") if l.strip()])
        if 3 <= nf <= 8:
            picked.append((n, nf))
        if len(picked) >= 4:
            break
    rows = []
    for name, nf in picked:
        new = name + "Renamed"
        # traditional
        subprocess.run("git checkout -q -- . && git clean -qfd -e .mt", shell=True, cwd=scratch)
        calls, ms = [], 0.0
        print(f"\n--- rename {name} -> {new} ({nf} files)\n  traditional:", flush=True)
        out, t = sh(f"grep -rnw --include=*.go {name} .", scratch); calls.append(out); ms += t
        print(f"    {len(calls):>2}. grep -rnw --include=*.go {name} .{'':<40} {t:7.1f} ms {len(out):>7,} B")
        files = sorted({m.group(1) for m in re.finditer(r"(?m)^\./([^:]+):\d+:", out)})
        for f in files:
            first = int(re.search(rf"(?m)^\./{re.escape(f)}:(\d+):", out).group(1))
            o, t = sh(f"sed -n '{max(1, first-10)},{first+10}p' {f}", scratch); calls.append(o); ms += t
            print(f"    {len(calls):>2}. sed -n '{max(1, first-10)},{first+10}p' {f:<52} {t:7.1f} ms {len(o):>7,} B")
            o, t = sh(f"sed -i '' 's/\\b{name}\\b/{new}/g' {f} 2>/dev/null || sed -i 's/\\b{name}\\b/{new}/g' {f}", scratch)
            # BSD sed has no \\b: fall back to perl, which every macOS has
            o, t2 = sh(f"perl -pi -e 's/\\b{name}\\b/{new}/g' {f}", scratch); calls.append(o); ms += t + t2
            print(f"    {len(calls):>2}. edit {f} (word-boundary replace){'':<36} {t+t2:7.1f} ms")
        o, t = sh("go build ./... && echo BUILD_OK", scratch); calls.append(o); ms += t
        trad_ok = "BUILD_OK" in o
        print(f"    {len(calls):>2}. go build ./...{'':<62} {t:7.1f} ms  {'BUILD OK' if trad_ok else 'BUILD FAILED'}")
        trad_diff = subprocess.run("git diff --stat | tail -1", shell=True, cwd=scratch, capture_output=True, text=True).stdout.strip()
        _, cost = cost_of([tokens(c) for c in calls])
        rows.append(dict(sym=name, files=nf, approach="traditional", calls=len(calls), cmd_ms=ms, agent_s=ms / 1000 + len(calls) * MODEL_TURN_S, cost=cost, ok=trad_ok, diff=trad_diff))
        # mt
        subprocess.run("git checkout -q -- . && git clean -qfd -e .mt", shell=True, cwd=scratch)
        calls, ms = [], 0.0
        o, t = sh(f"{MT} context {name} --budget 6000", scratch); calls.append(o); ms += t
        print(f"  mt:\n    {len(calls):>2}. mt context {name} --budget 6000{'':<40} {t:7.1f} ms {len(o):>7,} B")
        batch = json.dumps({"ops": [{"rename": name, "to": new}], "check": "go build ./..."})
        p = os.path.join(scratch, ".mt/ops.json"); open(p, "w").write(batch)
        o, t = sh(f"{MT} apply .mt/ops.json", scratch); calls.append(o); ms += t
        print(f"    {len(calls):>2}. mt apply {{rename {name}->{new}, check: go build ./...}}{'':<18} {t:7.1f} ms")
        for line in o.splitlines()[:4]:
            print(f"        | {line[:130]}")
        mt_ok = o.startswith("applied") and "check passed" in o
        mt_diff = subprocess.run("git diff --stat | tail -1", shell=True, cwd=scratch, capture_output=True, text=True).stdout.strip()
        timing = re.search(r"in (\d+) ms \(validate (\d+) ms, write (\d+) ms, reindex (\d+) ms, check (\d+) ms\)", o)
        _, cost = cost_of([tokens(c) for c in calls])
        rows.append(dict(sym=name, files=nf, approach="mt", calls=len(calls), cmd_ms=ms, agent_s=ms / 1000 + len(calls) * MODEL_TURN_S, cost=cost, ok=mt_ok, diff=mt_diff,
                         breakdown=timing.groups() if timing else None))
    subprocess.run("git checkout -q -- . && git clean -qfd -e .mt", shell=True, cwd=scratch)
    return rows

if __name__ == "__main__":
    t0 = time.time()
    rows, example = part_a()
    summarize(rows, f"PART A: gather context for a change ({PER_REPO} symbols x {len(REPOS)} repos)")
    for repo in REPOS:
        summarize([r for r in rows if r["repo"] == os.path.basename(repo)], os.path.basename(repo))
    if example:
        sym, repo, sizes = example
        print(f"\n== CONTEXT COST CALL BY CALL, traditional, {sym['name']} in {os.path.basename(repo)} (input-token equivalents) ==")
        print(f"{'call':>4} {'result tok':>10} {'context':>9} {'re-read':>8} {'write':>7} {'output':>7} {'carried':>8} {'call total':>10}")
        trows, ttot = cost_of(sizes)
        for r in trows:
            print(f"{r['call']:>4} {r['result']:>10.0f} {r['ctx']:>9.0f} {r['read']:>8.0f} {r['write']:>7.0f} {r['out']:>7.0f} {r['carry']:>8.0f} {r['sum']:>10.0f}")
        print(f"{'':>4} {'TOTAL':>10} {'':>9} {'':>8} {'':>7} {'':>7} {'':>8} {ttot:>10.0f}")
        out, _ = sh(f"{MT} context {sym['name']} --budget 6000", repo)
        mrows, mtot = cost_of([tokens(out)])
        print(f"== same symbol, mt ==")
        for r in mrows:
            print(f"{r['call']:>4} {r['result']:>10.0f} {r['ctx']:>9.0f} {r['read']:>8.0f} {r['write']:>7.0f} {r['out']:>7.0f} {r['carry']:>8.0f} {r['sum']:>10.0f}")
    b = part_b()
    print("\n== PART B: rename across files + go build, scratch copy of claude_coach ==")
    print(f"{'symbol':<28} {'files':>5} {'approach':<12} {'calls':>5} {'cmd ms':>8} {'agent s*':>8} {'cost* (k)':>9} {'builds':>6}  diff")
    for r in b:
        print(f"{r['sym']:<28} {r['files']:>5} {r['approach']:<12} {r['calls']:>5} {r['cmd_ms']:>8.0f} {r['agent_s']:>8.1f} {r['cost']/1000:>9.1f} {str(r['ok']):>6}  {r['diff']}")
        if r.get("breakdown"):
            tot, v, w, ri, c = r["breakdown"]
            print(f"{'':<28} {'':>5} {'  mt apply':<12} total {tot} ms = validate {v} + write {w} + reindex {ri} + go build {c}")
    print(f"\n* modelled: agent s = command time + calls x {MODEL_TURN_S}s model turn; cost = input-token equivalents "
          f"(base context {BASE_CONTEXT//1000}k, cache read {R_READ}x, write {R_WRITE}x, output {R_OUT}x, {LATER_CALLS} later calls re-read new content)")
    json.dump({"a": rows, "b": b}, open(os.path.join(os.path.dirname(__file__), "agent_bench_last.json"), "w"), default=str)
    print(f"bench wall time {time.time()-t0:.0f}s")
