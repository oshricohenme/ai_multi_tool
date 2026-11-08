// Coach: best-practice checks run against one recorded session.
// A check reads a session ({events, sigs, meta}) and returns
//   { status: 'fail' | 'warn' | 'pass' | 'na', headline, evidence: [{id?, at, label, text}], fix? }
// Checks are pure, so the same code runs on the live session and on any past one.
// Catalog and thresholds: the research pass of 2026-11-05 against code.claude.com/docs.
(function () {
  const D = 'https://code.claude.com/docs/en/';
  const BP = D + 'best-practices', COST = D + 'costs', PC = D + 'prompt-caching', CW = D + 'context-window', MEM = D + 'memory',
    SA = D + 'sub-agents', SK = D + 'skills', PERM = D + 'permissions', MC = D + 'model-config';
  const parse = s => { try { return typeof s === 'string' ? JSON.parse(s.replace(/…$/, '')) : s || {}; } catch { return {}; } };
  const clip = (s, n) => { s = String(s || '').replace(/\s+/g, ' ').trim(); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
  const k = n => n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : n >= 1000 ? Math.round(n / 1000) + 'k' : String(Math.round(n || 0));
  const usd = n => '$' + (n < 0.1 ? n.toFixed(3) : n.toFixed(2));
  const ctx = e => (e.inTok || 0) + (e.cacheRead || 0) + (e.cacheWrite || 0);
  const med = a => { const x = a.slice().sort((p, q) => p - q); return x.length ? x[Math.floor(x.length / 2)] : 0; };
  const words = s => String(s || '').trim().split(/\s+/).filter(Boolean).length;
  const EDIT = /^(Edit|Write|MultiEdit|NotebookEdit)$/;
  const SEARCH = /^(Read|Grep|Glob|LS)$/;
  const AGENT = /^(Agent|Task)$/;
  const TESTCMD = /\b(test|tests|pytest|jest|vitest|mocha|playwright|cypress|go (test|build|vet)|cargo (test|check|build)|tsc|typecheck|type-check|lint|eslint|ruff|mypy|make( \w+)?|build|npm run \w+|pnpm \w+|yarn \w+|rspec|phpunit|xcodebuild|swift (build|test)|claude plugin (test|validate)|regress)\b/i;
  const CORR = /^\s*(no\b|nope|wrong|incorrect|that'?s not|not what i|this is not|still (broken|failing|wrong|not)|again\b|you didn'?t|you (forgot|missed|broke|ignored)|didn'?t work|doesn'?t work|does not work|it (still|doesn'?t)|undo|revert|why did you)/i;
  const DONE = /\b(done|fixed|implemented|should (now )?work|completed?|all set)\b/i;
  const PASSCLAIM = /\b(all )?tests? (now )?pass(es|ing)?\b|\bbuild (succeeds|passes|is green)\b|\bno (type )?errors\b/i;
  const FAILOUT = /\b(FAIL|failed|failing|Error:|✗)\b|\b[1-9]\d* (failing|failed)\b/;
  const MUTATE = /\bsed -i|\bperl -pi|>\s*[\w.\/-]+\.\w+\b|\btee\b|\bmv\b|\brm\b|git (checkout|reset|restore)\b/;

  function fmtT(ms) { ms = Math.max(0, ms); return ms < 60000 ? Math.round(ms / 1000) + 's' : ms < 3600e3 ? Math.floor(ms / 60000) + 'm' + String(Math.round(ms % 60000 / 1000)).padStart(2, '0') : (ms / 3600e3).toFixed(1) + 'h'; }

  // One pass over the session, shared by every check.
  function context(S) {
    const ev = (S.events || []).slice().sort((a, b) => a.start - b.start);
    const meta = S.meta || {}, sigs = S.sigs || [];
    const users = ev.filter(e => e.kind === 'user' && !e.agentId);
    const turnNo = new Map(); users.forEach(u => { if (!turnNo.has(u.turnId)) turnNo.set(u.turnId, turnNo.size + 1); });
    const calls = ev.filter(e => e.kind === 'assistant'), main = calls.filter(e => !e.agentId);
    const tools = ev.filter(e => e.kind === 'tool'), mainTools = tools.filter(e => !e.agentId);
    let num = 0, den = 0;
    calls.forEach(e => { if (e.usd > 0) { num += e.usd; den += (e.inTok || 0) + 0.1 * (e.cacheRead || 0) + 1.25 * (e.cacheWrite || 0) + 5 * (e.outTok || 0); } });
    const p = den > 0 ? num / den : 5e-6; // $ per uncached input token, from the session's own ledger
    const costOf = e => e.usd != null ? e.usd : p * ((e.inTok || 0) + 0.1 * (e.cacheRead || 0) + 1.25 * (e.cacheWrite || 0) + 5 * (e.outTok || 0));
    const window = meta.window || (meta.context && meta.context.max) || 200000;
    const ttl = String(meta.cacheTtl || '');
    const ttlMs = /5m|300/.test(ttl) ? 300e3 : 3600e3; // unknown: the subscription main thread's 1h
    const t0 = ev.length ? ev[0].start : 0;
    const turnOf = e => turnNo.get(e.turnId);
    const label = e => (turnOf(e) ? 'Turn ' + turnOf(e) : e.agentId ? 'Subagent' : '·') + ' · +' + fmtT(e.start - t0);
    const byTurn = new Map(); ev.forEach(e => { if (!e.agentId) { if (!byTurn.has(e.turnId)) byTurn.set(e.turnId, []); byTurn.get(e.turnId).push(e); } });
    const turns = [...byTurn.entries()].map(([id, list]) => ({ id, list, user: list.find(e => e.kind === 'user'), tools: list.filter(e => e.kind === 'tool'), calls: list.filter(e => e.kind === 'assistant') }));
    const agents = new Map(); ev.forEach(e => { if (e.agentId) { if (!agents.has(e.agentId)) agents.set(e.agentId, []); agents.get(e.agentId).push(e); } });
    const ctxAt = at => { let c = 0; main.forEach(e => { if (e.start <= at) c = ctx(e); }); return c; };
    // the Turns classifier's view of the same session (browser only; skipped where it isn't loaded)
    const CL = globalThis.CLASSIFY;
    const tturns = CL ? CL.fromEvents(ev, '', '') : [], tcls = CL ? CL.classifyAll(tturns) : [];
    return { ev, meta, sigs, users, turnNo, calls, main, tools, mainTools, p, costOf, window, ttlMs, t0, turnOf, label, byTurn, turns, agents, ctxAt, tturns, tcls, hasCL: !!CL };
  }
  const evd = (C, e, text) => ({ id: e.id, at: e.start, label: C.label(e), text: clip(text, 160) });
  const sigEvd = (C, g, text) => ({ at: g.at, label: g.at >= C.t0 ? '+' + fmtT(g.at - C.t0) : new Date(g.at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }), text: clip(text, 160) });
  const inputText = v => { const a = parse(v); return a.command || a.file_path || a.url || a.pattern || (typeof v === 'string' ? v : JSON.stringify(v || '')); };
  const cmdOf = e => String(parse(e.payload).command || '');
  const fileOf = e => { const a = parse(e.payload); return String(a.file_path || a.path || a.notebook_path || ''); };
  const say = e => e.payload || e.title || '';
  const pl = (n, w, ws) => n + ' ' + (n === 1 ? w : ws || w + 's');
  const NA = h => ({ status: 'na', headline: h });
  const PASS = (h, evidence) => ({ status: 'pass', headline: h, evidence: evidence || [] });
  const permKey = (tool, input) => {
    const a = parse(input);
    if (tool === 'Bash' && a.command) { const t = String(a.command).replace(/^\s*((timeout|nice)\s+\S+\s+|\w+=\S+\s+)*/, '').trim().split(/\s+/); return 'Bash(' + t.slice(0, 2).join(' ') + ' *)'; }
    if (/^mcp__/.test(tool)) return 'mcp__' + tool.split('__')[1];
    if (a.url) { try { return tool + '(domain:' + new URL(a.url).host + ')'; } catch {} }
    return tool;
  };

  const CHECKS = [
    // ── A. Context & cost ────────────────────────────────────────────────────
    { id: 'kitchen-sink', group: 'Context & cost', name: 'New task on an old context', sev: 'medium', src: BP,
      why: 'Every request re-sends the whole conversation, so an unrelated earlier task is paid for on every turn. /clear is free.',
      run(C) {
        const hits = [];
        C.turns.forEach((t, i) => {
          if (!t.user || i < 2) return;
          const c = C.ctxAt(t.user.start); if (c < 150000) return;
          // only a person's own new ask counts: not a subagent report, not "did you…", "2", "where are my tokens going?"
          const cl = C.hasCL ? C.tcls[C.tturns.findIndex(x => x.id === t.user.id)] : null;
          if (cl && (cl.source !== 'human' || globalThis.CLASSIFY.groupOf(cl.intent) !== 'ask')) return;
          if (say(t.user).replace(/\s+/g, ' ').trim().length < 60) return;
          if (/\b(it|this|that|above|same|also)\b/i.test(say(t.user).split(/\s+/).slice(0, 8).join(' '))) return;
          const before = new Set(), after = new Set();
          C.turns.slice(0, i).forEach(x => x.tools.forEach(e => { const f = fileOf(e); if (f) before.add(f); }));
          C.turns.slice(i, i + 5).forEach(x => x.tools.forEach(e => { const f = fileOf(e); if (f) after.add(f); }));
          if (after.size < 3 || !before.size) return;
          const inter = [...after].filter(f => before.has(f)).length, jac = inter / (before.size + after.size - inter);
          if (jac < 0.1) hits.push({ u: t.user, c, carried: C.main.filter(e => e.start > t.user.start).reduce((a, e) => a + (e.cacheRead || 0), 0) });
        });
        if (C.turns.length < 4) return NA('Too few turns to judge.');
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'prompt') + ' started unrelated work on a ' + k(hits[0].c) + '-token context.', evidence: hits.map(h => evd(C, h.u, k(h.c) + ' ctx · ' + say(h.u))),
          fix: '/clear before switching tasks (or /rename this one first and come back with /resume <name>).' };
        return PASS('No unrelated task started on a large context.');
      } },
    { id: 'cold-cache-resume', group: 'Context & cost', name: 'Came back after the cache expired', sev: 'medium', src: PC,
      why: 'The first message after a break longer than the cache lifetime re-processes the whole context at write price.',
      run(C) {
        if (C.main.length < 2) return NA('Too few model calls.');
        const turnUsd = C.turns.map(t => t.calls.reduce((a, e) => a + C.costOf(e), 0)).filter(x => x > 0), m = med(turnUsd);
        const comps = C.ev.filter(e => e.kind === 'compact'), hits = [];
        for (let i = 1; i < C.main.length; i++) {
          const a = C.main[i - 1], b = C.main[i], from = a.end || a.start, gap = b.start - from;
          if (gap > C.ttlMs && ctx(b) >= 100000 && (b.cacheWrite || 0) >= 0.5 * ctx(b) && !comps.some(x => x.start > from && x.start < b.start)) hits.push({ b, gap, cost: C.costOf(b) });
        }
        const tot = hits.reduce((a, h) => a + h.cost, 0);
        if (hits.length) return { status: hits.some(h => m && h.cost > 5 * m) ? 'fail' : 'warn', headline: pl(hits.length, 'cold restart') + ' after pauses over ' + fmtT(C.ttlMs) + ' on 100k+ contexts, ≈' + usd(tot) + '.', evidence: hits.map(h => evd(C, h.b, 'paused ' + fmtT(h.gap) + ' · re-wrote ' + k(h.b.cacheWrite) + ' tokens · ' + usd(h.cost) + (m ? ' (median turn ' + usd(m) + ')' : ''))),
          fix: 'Done with the task? /clear before stepping away. Need the history? /compact <focus> while the cache is still warm.\nOn an API key you can opt into the 1-hour cache: "promptCacheTtl": "1h" in settings.' };
        return PASS('No cold restarts on a large context.');
      } },
    { id: 'model-switch', group: 'Context & cost', name: 'Switched model on a large context', sev: 'medium', src: PC,
      why: 'Each model has its own cache, so a switch re-reads the whole history uncached. Opusplan toggles and a skill with its own model do the same.',
      run(C) {
        if (C.main.length < 2) return NA('Too few model calls.');
        const hits = [];
        C.sigs.filter(g => g.ev === 'classic.PreModelSwitch' && g.sum && (g.sum.context_tokens || 0) >= 50000).forEach(g => hits.push(sigEvd(C, g, g.sum.from_model + ' → ' + g.sum.to_model + ' · ' + k(g.sum.context_tokens) + ' ctx · ≈$' + (+g.sum.estimated_cache_write_usd || 0).toFixed(2))));
        if (!hits.length) for (let i = 1; i < C.main.length; i++) { const a = C.main[i - 1], b = C.main[i]; if (a.model && b.model && a.model !== b.model && b.start - (a.end || a.start) < C.ttlMs && (b.cacheWrite || 0) >= 0.5 * ctx(b) && ctx(b) >= 50000) hits.push(evd(C, b, a.model + ' → ' + b.model + ' · re-wrote ' + k(b.cacheWrite) + ' · ' + usd(C.costOf(b)))); }
        if (hits.length) return { status: hits.length >= 3 ? 'fail' : 'warn', headline: pl(hits.length, 'switch', 'switches') + ' with 50k+ tokens in context' + (hits.length >= 3 ? ' (toggling)' : '') + '.', evidence: hits,
          fix: 'Pick model and effort at the start of the session. For cheap side work, delegate to a subagent with model: haiku instead of /model. Toggling plan mode often? Drop opusplan.' };
        return PASS('No costly model switches.');
      } },
    { id: 'unexplained-cache-miss', group: 'Context & cost', name: 'Cache rebuilt for no visible reason', sev: 'low', src: PC,
      why: 'Changing effort, fast mode, MCP servers or plugins (with tool search off) also invalidates the cached prefix.',
      run(C) {
        if (C.main.length < 3) return NA('Too few model calls.');
        const comps = C.ev.filter(e => e.kind === 'compact'), hits = [];
        for (let i = 1; i < C.main.length; i++) {
          const a = C.main[i - 1], b = C.main[i], c = ctx(b);
          if (b.start - (a.end || a.start) >= C.ttlMs || a.model !== b.model || c < 50000) continue;
          if (comps.some(x => x.start > a.start && x.start < b.start)) continue;
          if ((b.cacheWrite || 0) / Math.max(1, (b.cacheRead || 0) + (b.cacheWrite || 0)) > 0.3) {
            const near = C.sigs.filter(g => Math.abs(g.at - b.start) < 60000 && /^(mcp\.|plugin\.|config\.|settings\.|command\.run)/.test(g.ev)).map(g => g.ev);
            hits.push(evd(C, b, 're-wrote ' + k(b.cacheWrite) + ' of ' + k(c) + (near.length ? ' · near ' + [...new Set(near)].slice(0, 3).join(', ') : '')));
          }
        }
        if (hits.length >= 2) return { status: 'warn', headline: pl(hits.length, 'cache rebuild') + ' with the same model and no long pause.', evidence: hits,
          fix: 'Run /usage and read the "Prompt cache (main) … likely cause" line. Settle effort, fast mode and MCP servers at the top of the session.' };
        return PASS(hits.length ? 'One unexplained rebuild (not a pattern).' : 'The cache held between calls.', hits);
      } },
    { id: 'costly-compact', group: 'Context & cost', name: 'Compacted a cold, large context', sev: 'low', src: PC,
      why: '/compact re-reads everything it summarises: cheap while the cache is warm, expensive when cold. /clear is free and /rewind returns to a cached prefix.',
      run(C) {
        const cs = C.ev.filter(e => e.kind === 'compact');
        if (!cs.length) return NA('No compactions.');
        const hits = cs.filter(e => {
          if ((e.before || 0) < 200000) return false;
          const prev = C.main.filter(x => (x.end || x.start) < e.start).pop();
          return e.trigger !== 'auto' && prev && e.start - (prev.end || prev.start) > C.ttlMs || /idle/.test(e.trigger || '');
        });
        const manual = cs.filter(e => e.trigger === 'manual');
        if (hits.length || manual.length >= 3) return { status: 'warn', headline: hits.length ? pl(hits.length, 'compaction') + ' of 200k+ tokens on a cold cache.' : manual.length + ' manual compactions in one session.', evidence: (hits.length ? hits : manual).map(e => evd(C, e, (e.trigger || '?') + ' · ' + k(e.before || 0) + ' → ' + k(e.after || 0))),
          fix: 'New work: /clear. Abandoning a path: /rewind. Otherwise compact at a break while the cache is warm: /compact keep the API changes and test commands.\nTo stop idle compaction: "idleCompaction": false.' };
        return PASS('Compactions ran on a warm cache.');
      } },
    { id: 'auto-compact-mid-task', group: 'Context & cost', name: 'Auto-compacted in the middle of work', sev: 'medium', src: CW,
      why: 'Automatic compaction drops what the summariser judges unimportant. Compacting yourself at a task boundary keeps what you choose. On 1M-token models this means the session is enormous.',
      run(C) {
        const auto = C.ev.filter(e => e.kind === 'compact' && e.trigger === 'auto');
        if (!C.ev.some(e => e.kind === 'compact')) return PASS('No compaction · peak context ' + Math.round(Math.max(0, ...C.main.map(ctx)) / C.window * 100) + '% of ' + k(C.window) + '.');
        const mid = auto.filter(e => {
          const t = C.byTurn.get(e.turnId) || [];
          const inTurn = t.some(x => x.kind === 'tool' && x.start < e.start) && t.some(x => x.kind === 'tool' && x.start > e.start);
          const next = C.users.find(u => u.start > e.start);
          return inTurn || (next && (CORR.test(say(next)) || /\b(forgot|lost|you were|earlier)\b/i.test(say(next))));
        });
        if (mid.length) return { status: 'warn', headline: pl(mid.length, 'automatic compaction') + ' landed mid-task (' + k(mid[0].before || 0) + ' → ' + k(mid[0].after || 0) + ').', evidence: mid.map(e => evd(C, e, 'auto-compact ' + k(e.before || 0) + ' → ' + k(e.after || 0))),
          fix: '/compact <focus> between tasks, /autocompact 400k to compact earlier, and a "# Compact instructions" section in CLAUDE.md saying what to keep.' };
        return PASS(auto.length ? 'Auto-compaction ran between tasks.' : 'All compactions were yours.');
      } },
    { id: 'huge-tool-result', group: 'Context & cost', name: 'Oversized tool output', sev: 'medium', src: COST,
      why: 'Large raw output rides along in context for the rest of the session. Filter it or isolate it in a subagent.',
      run(C) {
        if (!C.mainTools.length) return NA('No tool calls.');
        const big = C.mainTools.filter(e => (e.est || 0) >= 20000);
        const testTot = C.mainTools.filter(e => TESTCMD.test(cmdOf(e))).reduce((a, e) => a + (e.est || 0), 0);
        const rep = {}; C.mainTools.filter(e => (e.est || 0) >= 5000 && cmdOf(e)).forEach(e => (rep[cmdOf(e)] = rep[cmdOf(e)] || []).push(e));
        const repeated = Object.values(rep).filter(l => l.length >= 3).flat();
        const hits = [...new Set(big.concat(repeated))].sort((a, b) => b.est - a.est);
        if (hits.length || testTot > 50000) return { status: big.length >= 3 || testTot > 50000 ? 'fail' : 'warn', headline: [big.length && pl(big.length, 'result') + ' over 20k tokens', testTot > 50000 && k(testTot) + ' tokens of test output', repeated.length && 'a ' + k(repeated[0].est) + ' output repeated'].filter(Boolean).join(' · ') + '.', evidence: hits.slice(0, 8).map(e => evd(C, e, (e.tool || '') + ' · ~' + k(e.est) + ' · ' + (cmdOf(e) || fileOf(e) || e.title))),
          fix: 'Pipe test runs through `| tail -50` or `grep -E "FAIL|ERROR"`; Read with offset/limit; Grep with head_limit.\nOr: "run the suite in a subagent and report only the failures".' };
        return PASS('No oversized tool output.');
      } },
    { id: 'idle-background-spend', group: 'Context & cost', name: 'Turns you did not type', sev: 'medium', src: COST,
      why: '/loop, goal check-ins, scheduled tasks and messages from other sessions each send the full context while you are away.',
      run(C) {
        const bg = C.turns.filter(t => !t.user && t.calls.length);
        if (!C.turns.length) return NA('No turns.');
        const bgUsd = bg.reduce((a, t) => a + t.calls.reduce((s, e) => s + C.costOf(e), 0), 0), all = C.calls.reduce((a, e) => a + C.costOf(e), 0);
        const bigBg = bg.filter(t => Math.max(0, ...t.calls.map(ctx)) >= 100000);
        if ((all && bgUsd > 0.2 * all && bg.length >= 2) || bigBg.length >= 10) return { status: 'warn', headline: pl(bg.length, 'turn') + ' with no prompt from you, ' + usd(bgUsd) + ' (' + Math.round(bgUsd / Math.max(1e-9, all) * 100) + '% of the session).', evidence: bg.slice(0, 8).map(t => evd(C, t.calls[0], k(ctx(t.calls[0])) + ' ctx · ' + usd(t.calls.reduce((s, e) => s + C.costOf(e), 0)))),
          fix: 'Stop the /loop, or run it in a fresh session or as a /schedule. Turn off goal check-ins with CLAUDE_CODE_GOAL_CHECKIN_MINUTES=0.' };
        return PASS(bg.length ? pl(bg.length, 'background turn') + ', a small share of spend.' : 'Every turn started from your prompt.');
      } },
    { id: 'overthinking', group: 'Context & cost', name: 'Heavy thinking on small turns', sev: 'low', src: BP,
      why: 'Thinking is billed as output. Effort is the control: low for quick back-and-forth, max for hard unattended problems.',
      run(C) {
        const hits = C.turns.filter(t => t.user && t.calls.reduce((a, e) => a + (e.thinkingChars || 0), 0) >= 20000 && t.tools.filter(e => EDIT.test(e.tool || '')).length <= 1 && t.calls.reduce((a, e) => a + (e.outTok || 0), 0) < 800 && !t.tools.some(e => TESTCMD.test(cmdOf(e))));
        if (!C.calls.some(e => e.thinkingChars)) return NA('No thinking recorded.');
        if (hits.length >= 5) return { status: 'warn', headline: pl(hits.length, 'small turn') + ' with 20k+ characters of thinking each.', evidence: hits.slice(0, 8).map(t => evd(C, t.user, k(t.calls.reduce((a, e) => a + (e.thinkingChars || 0), 0)) + ' chars thinking · ' + say(t.user))),
          fix: '/effort low for sketching and renames, /effort medium as the default; one hard question: write "ultrathink" in that prompt instead of raising effort for the session.' };
        return PASS('Thinking matched the size of the work.');
      } },

    // ── B. Prompting & steering ──────────────────────────────────────────────
    { id: 'correction-loop', group: 'Steering', name: 'Correcting the same thing again', sev: 'high', src: BP,
      why: 'After two failed corrections, a third mostly adds failed attempts for Claude to re-read. /clear and a better prompt works better "almost every time".',
      run(C) {
        if (C.users.length < 3) return NA('Too few prompts to judge.');
        const filesOf = u => new Set(((C.byTurn.get(u.turnId) || []).filter(e => e.kind === 'tool' && EDIT.test(e.tool || ''))).map(fileOf));
        let best = [];
        C.users.forEach((u, i) => {
          const win = C.users.slice(i, i + 4).filter(x => CORR.test(say(x)));
          if (win.length < 2 || !CORR.test(say(u))) return;
          const prev = C.users[i - 1], f0 = prev ? filesOf(prev) : new Set();
          const same = win.every(x => { const f = filesOf(x); return !f.size || !f0.size || [...f].some(p => f0.has(p)); });
          if (same && win.length > best.length) best = win;
        });
        if (best.length >= 2) return { status: best.length >= 3 ? 'fail' : 'warn', headline: best.length + ' corrections within 4 turns on the same work, from turn ' + C.turnOf(best[0]) + '.', evidence: best.map(u => evd(C, u, say(u))),
          fix: '/rewind (Esc Esc) to before the first wrong attempt, or /clear, then one prompt that folds in what you learned:\n\n"X fails when Y. Look at <file>. Constraint: Z. Done = <test> passes."' };
        return PASS('No stacked corrections.');
      } },
    { id: 'vague-prompt-thrash', group: 'Steering', name: 'Underspecified task prompt', sev: 'medium', src: BP,
      why: 'Naming the file, the symptom and what "fixed" looks like saves a round of searching and correcting. Vague is fine for exploring, so this only fires when corrections followed.',
      run(C) {
        const ID = /[\w\/.-]+\.\w{1,5}\b|@\S+|`[^`]+`|\w+\(\)/;
        const hits = [];
        C.turns.forEach((t, i) => {
          if (!t.user || words(say(t.user)) >= 20 || ID.test(say(t.user))) return;
          const fe = t.tools.findIndex(e => EDIT.test(e.tool || '')); if (fe < 0) return;
          const reads = t.tools.slice(0, fe).filter(e => SEARCH.test(e.tool || '') || AGENT.test(e.tool || '')).length;
          const corrected = C.turns.slice(i + 1, i + 4).some(x => x.user && CORR.test(say(x.user)));
          if (reads >= 15 && corrected) hits.push({ u: t.user, reads });
        });
        if (!C.users.length) return NA('No prompts.');
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'short prompt') + ' with no file or symbol, ' + hits[0].reads + ' searches before the first edit, then corrections.', evidence: hits.map(h => evd(C, h.u, h.reads + ' searches · ' + say(h.u))),
          fix: 'Say where and how you\'ll judge it: "users report login fails after session timeout. Look at token refresh in src/auth/, write a failing test that reproduces it, then fix it."\nBigger feature? "Interview me until you could write the spec, then write SPEC.md", and build it in a fresh session.' };
        return PASS('Task prompts were specific enough.');
      } },
    { id: 'obsolete-incantations', group: 'Steering', name: 'Magic words that do nothing', sev: 'low', src: BP,
      why: '"think hard" and "be thorough" are plain text; only ultrathink or /effort change reasoning. "Keep going / don\'t be lazy" tends to cause over-work.',
      run(C) {
        if (!C.users.length) return NA('No prompts.');
        const eff = C.users.filter(u => /\b(think (hard|harder|more|deeply)|think step by step|be thorough|megathink)\b/i.test(say(u)) && !/ultrathink/i.test(say(u)));
        const per = C.users.filter(u => /\b(keep going|don'?t stop|don'?t be lazy|loop until|never give up)\b/i.test(say(u)));
        if (eff.length || per.length) return { status: 'warn', headline: [eff.length && pl(eff.length, 'effort phrase'), per.length && pl(per.length, 'persistence phrase')].filter(Boolean).join(' and ') + ' Claude treats as plain text.', evidence: eff.concat(per).map(u => evd(C, u, say(u))),
          fix: (eff.length ? 'Effort: /effort high for the session, or "ultrathink" in one prompt.\n' : '') + (per.length ? 'Persistence: give a check instead, "run the auth tests and keep going until they pass", or /goal <condition>.' : '') };
        return PASS('No placebo phrases.');
      } },
    { id: 'late-interrupt', group: 'Steering', name: 'Drift caught late', sev: 'low', src: BP,
      why: 'Watch the first minute and press Esc the moment Claude drifts; an early correction avoids expensive rework.',
      run(C) {
        const aborts = C.sigs.filter(g => g.ev === 'turn.abort');
        if (!aborts.length) return NA('No interruptions.');
        const hits = [];
        aborts.forEach(g => {
          const t = C.turns.find(x => x.user && x.user.start <= g.at && (x.list[x.list.length - 1].end || x.list[x.list.length - 1].start) >= g.at - 5000) || C.turns.filter(x => x.user && x.user.start <= g.at).pop();
          if (!t) return;
          const before = t.tools.filter(e => e.start <= g.at), edits = before.filter(e => EDIT.test(e.tool || '')).length;
          const next = C.users.find(u => u.start > g.at);
          if ((before.length >= 10 || edits >= 3 || g.at - t.user.start > 180000) && next && CORR.test(say(next))) hits.push({ g, t, n: before.length, edits });
        });
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'interruption') + ' after ' + hits[0].n + ' tool calls (' + hits[0].edits + ' edits), followed by a correction.', evidence: hits.map(h => sigEvd(C, h.g, 'Esc after ' + h.n + ' tools · ' + say(h.t.user))),
          fix: 'Interrupt on the first off-track tool call. Esc Esc (/rewind) rolls the code back. For risky changes start in plan mode (Shift+Tab).' };
        return PASS('Interruptions came early.');
      } },
    { id: 'side-questions', group: 'Steering', name: 'Quick questions in a long session', sev: 'low', src: BP,
      why: '/btw answers a quick question without adding it to the conversation every later request re-reads.',
      run(C) {
        const hits = C.turns.filter(t => t.user && /\?\s*$/.test(say(t.user)) && words(say(t.user)) < 25 && !t.tools.length && t.calls.reduce((a, e) => a + (e.outTok || 0), 0) < 600 && C.ctxAt(t.user.start) >= 150000);
        if (!C.users.length) return NA('No prompts.');
        if (hits.length >= 3) return { status: 'warn', headline: pl(hits.length, 'quick question') + ' asked on a 150k+ context.', evidence: hits.map(t => evd(C, t.user, k(C.ctxAt(t.user.start)) + ' ctx · ' + say(t.user))), fix: '/btw <question>' };
        return PASS('No side questions bloating the history.');
      } },
    { id: 'repeated-procedure', group: 'Steering', name: 'Same instructions pasted again', sev: 'low', src: SK,
      why: 'When you keep pasting the same checklist or procedure, make it a skill: its body loads only when used.',
      run(C) {
        const norm = s => String(s || '').toLowerCase().replace(/\s+/g, ' ').trim();
        const seen = {}, hits = [];
        // subagent reports and notifications share boilerplate by design; only what a person pasted counts
        const human = u => { if (!C.hasCL) return !/^\s*</.test(say(u)); const cl = C.tcls[C.tturns.findIndex(x => x.id === u.id)]; return !cl || cl.source === 'human' || cl.source === 'pasted'; };
        C.users.filter(human).forEach(u => { const n = norm(say(u)); if (n.length < 300) return; for (let i = 0; i + 300 <= n.length; i += 150) { const key = n.slice(i, i + 300); if (seen[key] && seen[key] !== u) { hits.push([seen[key], u]); return; } seen[key] = seen[key] || u; } });
        if (!C.users.length) return NA('No prompts.');
        if (hits.length) return { status: 'warn', headline: 'A 300+ character block was pasted ' + (hits.length + 1) + ' times.', evidence: [hits[0][0]].concat(hits.map(h => h[1])).map(u => evd(C, u, say(u))),
          fix: 'Save it as .claude/skills/<name>/SKILL.md with a one-line description; add disable-model-invocation: true if it has side effects, then run it as /<name>.' };
        return PASS('No repeated pasted procedures.');
      } },

    // ── C. Verification & quality ────────────────────────────────────────────
    { id: 'edit-without-check', group: 'Verification', name: '"Done" with no check run', sev: 'high', src: BP,
      why: 'Without something pass/fail to run, "looks done" is all Claude has, and you become the test suite.',
      run(C) {
        const hits = [], all = [];
        C.turns.forEach(t => {
          let last = -1; t.tools.forEach((e, i) => { if (EDIT.test(e.tool || '') && !/\.(md|txt|mdx)$/i.test(fileOf(e))) last = i; });
          if (last < 0) return; all.push(t);
          const checked = t.tools.slice(last + 1).some(e => TESTCMD.test(cmdOf(e)) || /^mcp__.*(browser|playwright|test|screenshot)/i.test(e.tool || ''));
          const answer = (t.calls[t.calls.length - 1] || {}).result || '';
          if (!checked && DONE.test(answer)) hits.push({ t, e: t.tools[last], answer });
        });
        if (!all.length) return NA('No code edits.');
        // the checks this session actually ran are the ones worth writing down
        const seen = [];
        // a check is a short command that starts with a runner, not a script that happens to contain the word "test"
        const RUNNER = /^(?:npx\s+\S+|npm\s+(?:run\s+)?\S+|pnpm\s+\S+|yarn\s+\S+|bunx?\s+\S+|cargo\s+\S+|go\s+(?:test|build|vet)|make(?:\s+\S+)?|pytest\b|tsc\b|eslint\b|ruff\b|mypy\b|jest\b|vitest\b|mocha\b|claude\s+plugin\s+(?:test|validate)|node\s+\S*(?:test|regress|check)\S*)/;
        C.mainTools.forEach(e => { const c = cmdOf(e); if (!TESTCMD.test(c)) return; c.split(/\s*(?:&&|\|\||;|\|)\s*/).forEach(p => { p = p.trim(); if (!RUNNER.test(p) || !TESTCMD.test(p) || p.length > 80 || /["'{]/.test(p)) return; const s = p.replace(/\s+2>&1.*$/, ''); if (!seen.includes(s)) seen.push(s); }); });
        const cmds = seen.slice(0, 2);
        if (hits.length) return { status: hits.length >= 2 ? 'fail' : 'warn', headline: hits.length + ' of ' + pl(all.length, 'editing turn') + ' ended "done" with no test, build or lint after the last edit.', evidence: hits.slice(0, 8).map(h => evd(C, h.e, fileOf(h.e).split('/').slice(-2).join('/') + ' · "' + clip((h.answer.match(DONE) || [''])[0], 20) + '" with nothing run')),
          fix: cmds.length
            ? 'Write down the checks this session already used, in CLAUDE.md:\n\n# Commands\n- After changing code: ' + cmds.map(c => '`' + c + '`').join(' and ') + '\n\nOr enforce the first with a Stop hook in .claude/settings.json:\n{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"' + cmds[0].replace(/"/g, '\\"') + ' 1>&2 || exit 2"}]}]}}'
            : 'Put the exact check in CLAUDE.md:\n\n# Commands\n- After changing code: `<your test command>` and `<your typecheck or lint>`\n\nOr enforce it with a Stop hook in .claude/settings.json:\n{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"<check> 1>&2 || exit 2"}]}]}}' };
        return PASS('Every "done" had a check after the last edit.');
      } },
    { id: 'unbacked-pass-claim', group: 'Verification', name: '"Tests pass" without proof', sev: 'high', src: BP,
      why: 'Ask for the command and its output; "all tests pass" on its own tells you very little.',
      run(C) {
        const hits = [];
        C.turns.forEach(t => {
          const last = t.calls[t.calls.length - 1]; if (!last || !PASSCLAIM.test(last.result || '')) return;
          const runs = t.tools.filter(e => TESTCMD.test(cmdOf(e))), r = runs[runs.length - 1];
          if (!r || r.isError || FAILOUT.test(r.result || '')) hits.push({ last, r });
        });
        if (!C.calls.some(e => PASSCLAIM.test(e.result || ''))) return NA('No pass claims.');
        if (hits.length) return { status: 'fail', headline: pl(hits.length, 'claim') + ' of passing checks that the last run in that turn does not back up.', evidence: hits.map(h => evd(C, h.last, '"' + clip((h.last.result.match(PASSCLAIM) || [''])[0], 30) + '" · ' + (h.r ? 'last run: ' + clip(h.r.result, 90) : 'no check ran'))),
          fix: 'Reply: "show the command and its output." Add a Stop hook so a failing check blocks the end of the turn.' };
        return PASS('Pass claims matched the last run.');
      } },
    { id: 'failure-left-standing', group: 'Verification', name: 'Failing check left standing', sev: 'medium', src: BP,
      why: 'Close the loop: change, run the check, read the output, fix, run again.',
      run(C) {
        const hits = [];
        C.turns.forEach(t => {
          const runs = t.tools.filter(e => TESTCMD.test(cmdOf(e))), r = runs[runs.length - 1];
          if (r && (r.isError || FAILOUT.test(r.result || ''))) hits.push(r);
        });
        if (!C.tools.some(e => TESTCMD.test(cmdOf(e)))) return NA('No checks ran.');
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'turn') + ' ended with the last check still failing.', evidence: hits.map(e => evd(C, e, cmdOf(e) + ' · ' + clip(e.result, 100))),
          fix: '"Rerun `<cmd>` and fix until it\'s green", or /goal <cmd> passes for an unattended run.' };
        return PASS('Every turn ended on a passing check.');
      } },
    { id: 'no-independent-review', group: 'Verification', name: 'Big diff, no fresh-eyes review', sev: 'low', src: BP,
      why: 'A reviewer that has not seen the reasoning catches what the author misses: /code-review, or a read-only reviewer subagent.',
      run(C) {
        const files = new Set(C.tools.filter(e => EDIT.test(e.tool || '')).map(fileOf).filter(Boolean));
        const lines = C.tools.filter(e => EDIT.test(e.tool || '')).reduce((a, e) => { const p = parse(e.payload); return a + String(p.new_string || p.content || '').split('\n').length; }, 0);
        if (!files.size) return NA('No edits.');
        const reviewed = C.sigs.some(g => (g.ev === 'skill.prompt' && /review|verify/i.test((g.sum || {}).skill || '')) || (g.ev === 'agent.spawn' && /review/i.test(JSON.stringify(g.sum || {})))) || C.users.some(u => /\/(code-)?review|\/verify/.test(say(u))) || C.tools.some(e => AGENT.test(e.tool || '') && /review/i.test(e.payload || ''));
        if ((files.size >= 6 || lines >= 300) && !reviewed) return { status: 'warn', headline: files.size + ' files and ~' + lines + ' lines changed with no review step.', evidence: [...files].slice(0, 8).map(f => ({ at: C.t0, label: 'edited', text: f.replace(/^\/Users\/[^/]+/, '~') })),
          fix: 'Run /code-review, or define .claude/agents/reviewer.md with tools: Read, Grep, Glob and "compare against SPEC.md, skip style".' };
        return PASS(reviewed ? 'The change was reviewed.' : 'Changes were small.');
      } },
    { id: 'bash-file-mutation', group: 'Verification', name: 'Edits through Bash (no checkpoint)', sev: 'low', src: BP,
      why: 'Checkpoints cover Claude\'s file tools only. A file changed only by Bash is not restored by /rewind.',
      run(C) {
        const hits = C.tools.filter(e => e.tool === 'Bash' && MUTATE.test(cmdOf(e)));
        if (!C.tools.length) return NA('No tool calls.');
        const rewound = C.users.some(u => /\/rewind|\b(revert|undo|roll ?back)\b/i.test(say(u)));
        if (hits.length >= 3) return { status: rewound ? 'warn' : 'pass', headline: hits.length + ' Bash commands changed files' + (rewound ? ', and you later asked to undo.' : '; no undo was needed.'), evidence: hits.slice(0, 8).map(e => evd(C, e, cmdOf(e))),
          fix: 'Commit before risky steps, or ask Claude to use Edit for source changes so /rewind can restore them.' };
        return PASS('File changes went through file tools.');
      } },

    // ── D. Permissions & flow ────────────────────────────────────────────────
    { id: 'repeated-approval', group: 'Permissions & flow', name: 'Approving the same command again', sev: 'medium', src: PERM,
      why: 'After the tenth prompt you approve without reading. Auto mode is the default; in Manual mode use allow rules or /fewer-permission-prompts.',
      run(C) {
        const asks = C.sigs.filter(g => g.ev === 'classic.PermissionRequest' && g.sum);
        if (!asks.length) return NA('No permission prompts recorded.');
        const per = {}; asks.forEach(a => (per[permKey(a.sum.tool_name || '?', a.sum.input)] = per[permKey(a.sum.tool_name || '?', a.sum.input)] || []).push(a));
        const rep = Object.entries(per).filter(([, l]) => l.length >= 3).sort((a, b) => b[1].length - a[1].length);
        if (rep.length) return { status: 'fail', headline: rep.slice(0, 3).map(([r, l]) => r + ' ×' + l.length).join(', ') + '.', evidence: rep[0][1].slice(0, 6).map(g => sigEvd(C, g, g.sum.tool_name + ' · ' + clip(inputText(g.sum.input), 120))),
          fix: 'In .claude/settings.json:\n{ "permissions": { "allow": [' + rep.slice(0, 4).map(([r]) => JSON.stringify(r)).join(', ') + '] } }\n\nOr Shift+Tab into auto mode, or run /fewer-permission-prompts.' };
        return PASS(pl(asks.length, 'prompt') + ', none repeated three times.');
      } },
    { id: 'permission-idle', group: 'Permissions & flow', name: 'Claude sat waiting for approval', sev: 'low', src: BP,
      why: 'In Manual mode nothing moves while you are away from the keyboard.',
      run(C) {
        const asks = C.sigs.filter(g => g.ev === 'classic.PermissionRequest' || (g.ev === 'classic.Notification' && g.sum && g.sum.notification_type === 'permission_prompt'));
        if (!asks.length) return NA('No permission prompts recorded.');
        const waits = asks.map(a => { const n = C.sigs.find(g => g.at > a.at && /^(classic\.PostToolUse|classic\.PermissionDenied|tool\.call)$/.test(g.ev)); return { a, w: n ? n.at - a.at - ((n.sum && n.sum.duration_ms) || 0) : 0 }; }).filter(x => x.w > 60000).sort((p, q) => q.w - p.w);
        const tot = waits.reduce((s, x) => s + x.w, 0);
        if (tot > 600000) return { status: 'warn', headline: fmtT(tot) + ' waiting on approvals (' + pl(waits.length, 'wait') + ' over a minute).', evidence: waits.slice(0, 6).map(x => sigEvd(C, x.a, fmtT(x.w) + ' · ' + ((x.a.sum && (x.a.sum.tool_name || x.a.sum.message)) || ''))),
          fix: 'Auto mode for interactive work (Shift+Tab). Unattended runs: --permission-mode dontAsk --allowedTools "…".' };
        return PASS(tot ? fmtT(tot) + ' of long waits in all.' : 'Approvals came quickly.');
      } },
    { id: 'classifier-blocks', group: 'Permissions & flow', name: 'Auto mode keeps blocking trusted work', sev: 'medium', src: BP,
      why: 'Instead of approving by hand, describe trusted infrastructure in autoMode.environment or add exceptions to autoMode.allow. 3 blocks in a row or 20 in all pause auto mode.',
      run(C) {
        const den = C.sigs.filter(g => g.ev === 'classic.PermissionDenied' || (g.ev === 'tool.check' && /decision: deny/.test(g.out || '') && !/rule /.test(g.out || '')));
        if (!den.length) return NA('No denials.');
        const per = {}; den.forEach(g => { const s = g.sum || {}; (per[permKey(s.tool_name || s.tool || '?', s.input)] = per[permKey(s.tool_name || s.tool || '?', s.input)] || []).push(g); });
        const rep = Object.entries(per).filter(([, l]) => l.length >= 2);
        if (rep.length || den.length >= 3) return { status: 'warn', headline: den.length + ' blocked calls' + (rep.length ? ', ' + rep[0][0] + ' ×' + rep[0][1].length : '') + '.', evidence: den.slice(0, 6).map(g => sigEvd(C, g, ((g.sum || {}).tool_name || (g.sum || {}).tool || '') + ' · ' + clip(inputText((g.sum || {}).input), 90) + ' · ' + (g.out || ''))),
          fix: 'In ~/.claude/settings.json (keep "$defaults"):\n{"autoMode":{"environment":["$defaults","Trusted internal domains: *.internal.example.com"],"allow":["$defaults","<plain-English exception>"]}}\n\nOr run /auto-mode-setup.' };
        return PASS('Denials were one-offs.');
      } },
    { id: 'rule-should-be-enforced', group: 'Permissions & flow', name: 'A standing rule Claude broke', sev: 'medium', src: BP,
      why: 'CLAUDE.md is guidance. A rule with zero exceptions belongs in a deny rule or a hook, not more emphasis.',
      run(C) {
        const hits = C.users.filter(u => /(i (told|asked) you|as (instructed|i said)|claude\.md says|you('re| are) not (allowed|supposed)|never (touch|edit|read|change))/i.test(say(u)));
        if (!C.users.length) return NA('No prompts.');
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'prompt') + ' say Claude broke a rule you had already given.', evidence: hits.map(u => evd(C, u, say(u))),
          fix: 'Make it impossible instead of asked: "permissions": { "deny": ["Read(./.env)", "Edit(src/gen/**)"] }, or ask Claude to "write a PreToolUse hook that blocks edits to migrations/".' };
        return PASS('No broken standing rules mentioned.');
      } },

    // ── E. Setup ─────────────────────────────────────────────────────────────
    { id: 'claude-md-bloat', group: 'Setup', name: 'CLAUDE.md too big', sev: 'medium', src: MEM,
      why: 'Aim for under 200 lines per file. Longer files cost context on every request and lower adherence; imports do not reduce the cost.',
      run(C) {
        const mf = (C.meta.context && C.meta.context.memoryFiles) || [];
        if (!mf.length) return NA('No memory files reported yet.');
        const tot = mf.reduce((a, f) => a + (f.tokens || 0), 0), big = mf.filter(f => (f.tokens || 0) > 3000);
        const ev = mf.slice().sort((a, b) => b.tokens - a.tokens).slice(0, 6).map(f => ({ at: C.t0, label: f.type || 'memory', text: String(f.path || '').replace(/^\/Users\/[^/]+/, '~') + ' · ' + k(f.tokens) + ' tokens × ' + C.main.length + ' requests' }));
        if (big.length || tot > 8000) return { status: 'warn', headline: (big.length ? pl(big.length, 'file') + ' over ~3k tokens (≈200 lines); ' : '') + k(tot) + ' tokens of instructions on every request.', evidence: ev,
          fix: 'Run /doctor prompt-audit. Move procedures to .claude/skills/, area rules to .claude/rules/*.md with paths:, and delete architecture notes Claude can read from the repo.' };
        return PASS(k(tot) + ' tokens across ' + pl(mf.length, 'file') + '.', ev);
      } },
    { id: 'idle-mcp', group: 'Setup', name: 'MCP servers loaded but unused', sev: 'low', src: COST,
      why: 'Connected servers add tool names and instructions even when unused. Definitions are deferred by default, so a large loaded count means tool search is off.',
      run(C) {
        const tools = (C.meta.context && C.meta.context.mcpTools) || [];
        if (!tools.length) return NA('No MCP tools reported for this session.');
        const used = new Set(C.tools.map(e => String(e.tool || '').split('__')[1]).filter(Boolean).concat(C.sigs.filter(g => g.ev === 'mcp.call').map(g => (g.sum || {}).server)));
        const per = {}; tools.forEach(t => { const s = t.server || String(t.name || '').split('__')[1] || '?'; const x = per[s] = per[s] || { tok: 0, loaded: 0 }; x.tok += t.tokens || 0; if (t.isLoaded !== false) x.loaded += t.tokens || 0; });
        const idle = Object.entries(per).filter(([s]) => !used.has(s)).sort((a, b) => b[1].tok - a[1].tok);
        const loaded = idle.reduce((a, [, x]) => a + x.loaded, 0);
        if (idle.length) return { status: loaded > 5000 ? 'warn' : 'pass', headline: pl(idle.length, 'server') + ' never called' + (loaded > 5000 ? ', ' + k(loaded) + ' tokens of definitions loaded on every request (tool search looks off).' : ' (definitions deferred, small cost).'), evidence: idle.slice(0, 8).map(([s, x]) => ({ at: C.t0, label: 'MCP', text: s + ' · ' + k(x.tok) + ' tokens' + (x.loaded ? ' loaded' : ' deferred') })),
          fix: 'Disable them in /mcp, or scope each server to the project that needs it (.mcp.json) instead of user scope.' };
        return PASS('Every connected server was used.');
      } },
    { id: 'skill-bloat', group: 'Setup', name: 'Skill descriptions crowding context', sev: 'low', src: SK,
      why: 'Skill descriptions sit in every session. Manual-only skills can drop out entirely with disable-model-invocation.',
      run(C) {
        const s = C.meta.context && C.meta.context.skills;
        if (!s || !s.tokens) return NA('No skill listing reported.');
        const used = new Set(C.sigs.filter(g => g.ev === 'skill.prompt').map(g => (g.sum || {}).skill));
        if (s.tokens > 5000) return { status: 'warn', headline: k(s.tokens) + ' tokens of descriptions for ' + (s.total || '?') + ' skills; ' + used.size + ' used this session.', evidence: [],
          fix: 'Shorten descriptions (use case first), set disable-model-invocation: true on manual-only skills, and uninstall plugins you do not use.' };
        return PASS(k(s.tokens) + ' tokens for ' + (s.total || '?') + ' skills.');
      } },

    // ── F. Delegation ────────────────────────────────────────────────────────
    { id: 'subagent-not-worth-it', group: 'Delegation', name: 'Subagent for a tiny job', sev: 'low', src: BP,
      why: 'Subagents pay off for parallel work, noisy output, fresh-eyes review and tool restriction. A fresh one re-gathers context and is not smarter.',
      run(C) {
        if (!C.agents.size) return NA('No subagents.');
        const m = med(C.turns.map(t => t.calls.reduce((a, e) => a + C.costOf(e), 0)).filter(x => x > 0));
        const hits = [];
        C.agents.forEach((list, id) => {
          const tl = list.filter(e => e.kind === 'tool'), cost = list.filter(e => e.kind === 'assistant').reduce((a, e) => a + C.costOf(e), 0);
          if (tl.length <= 3 && tl.reduce((a, e) => a + (e.est || 0), 0) < 5000 && m && cost > 2 * m) hits.push({ e: list[0], n: tl.length, cost });
        });
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'subagent') + ' did ≤3 tool calls but cost more than 2× a typical turn.', evidence: hits.map(h => evd(C, h.e, h.n + ' tools · ' + usd(h.cost) + ' · ' + (h.e.title || ''))),
          fix: 'Claude spawns these itself, so steer it: "do this inline", or use /btw for questions about what is already in context.' };
        return PASS('Subagents were sized for their jobs.');
      } },
    { id: 'subagent-model', group: 'Delegation', name: 'Read-only subagent on a big model', sev: 'low', src: SA,
      why: 'For simple jobs (finding callers, checking links) a subagent on Haiku is enough; inheriting Opus or Fable costs more for the same answer.',
      run(C) {
        if (!C.agents.size) return NA('No subagents.');
        const hits = [];
        C.agents.forEach(list => {
          const calls = list.filter(e => e.kind === 'assistant'), tl = list.filter(e => e.kind === 'tool');
          const model = (calls[0] || {}).model || '', cost = calls.reduce((a, e) => a + C.costOf(e), 0);
          if (/opus|fable/i.test(model) && !tl.some(e => EDIT.test(e.tool || '')) && tl.length && tl.filter(e => SEARCH.test(e.tool || '')).length / tl.length >= 0.8 && cost > 0.5) hits.push({ e: list[0], model, cost });
        });
        if (hits.length) return { status: 'warn', headline: pl(hits.length, 'read-only subagent') + ' ran on ' + hits[0].model + ', ' + usd(hits.reduce((a, h) => a + h.cost, 0)) + ' in all.', evidence: hits.map(h => evd(C, h.e, h.model + ' · ' + usd(h.cost) + ' · ' + (h.e.title || ''))),
          fix: 'In .claude/agents/<name>.md frontmatter: model: haiku and tools: Read, Grep, Glob. Or set CLAUDE_CODE_SUBAGENT_MODEL=haiku.' };
        return PASS('Subagent models fit their work.');
      } },
    { id: 'fat-subagent-return', group: 'Delegation', name: 'Subagent results flooding the parent', sev: 'low', src: SA,
      why: 'The point of a subagent is that only a summary comes back; condensed 1–2k-token reports keep the parent lean.',
      run(C) {
        const at = C.mainTools.filter(e => AGENT.test(e.tool || ''));
        if (!at.length) return NA('No subagent calls.');
        const big = at.filter(e => (e.est || 0) > 6000);
        const turnTot = C.turns.map(t => ({ t, n: t.tools.filter(e => AGENT.test(e.tool || '')).reduce((a, e) => a + (e.est || 0), 0) })).filter(x => x.n > 20000);
        if (big.length || turnTot.length) return { status: 'warn', headline: (big.length ? pl(big.length, 'subagent') + ' returned 6k+ tokens' : '') + (turnTot.length ? (big.length ? '; ' : '') + pl(turnTot.length, 'turn') + ' took 20k+ from subagents' : '') + '.', evidence: big.map(e => evd(C, e, '~' + k(e.est) + ' returned · ' + clip(parse(e.payload).description || e.title, 90))),
          fix: 'Add to the delegation prompt (or the agent\'s system prompt): "Report back in ≤15 bullet lines, file:line references only."' };
        return PASS('Subagents returned short summaries.');
      } },

    // ── Habits, from the Turns classifier ────────────────────────────────────
    { id: 'status-polling', group: 'Habits', name: 'Checking status instead of waiting', sev: 'low', src: BP,
      why: 'Each "are you done?" is a full model call that re-sends the whole context. Background work reports back when it finishes, and a notification or a Monitor wakes you.',
      run(C) {
        if (!C.hasCL) return NA('Needs the Turns classifier.');
        const idx = C.tcls.map((c, i) => i).filter(i => C.tcls[i].source === 'human' && C.tcls[i].intent === 'status');
        if (!C.tturns.length) return NA('No turns.');
        const quick = idx.filter(i => i > 0 && C.tturns[i - 1].end && C.tturns[i].ts - C.tturns[i - 1].end < 120000);
        const ev = idx.slice(0, 8).map(i => ({ id: C.tturns[i].id, at: C.tturns[i].ts, label: '+' + fmtT(C.tturns[i].ts - C.t0), text: '"' + clip(C.tturns[i].prompt, 80) + '" · ' + (C.tcls[i].intents[0].ev ? '«' + C.tcls[i].intents[0].ev + '»' : '') }));
        if (idx.length >= 5) return { status: 'warn', headline: idx.length + ' status checks' + (quick.length ? ', ' + quick.length + ' within 2 minutes of the previous reply' : '') + '.', evidence: ev,
          fix: 'Let long work report back: ask Claude to "run it in the background and tell me when it finishes", or use /loop or a Monitor instead of polling.' };
        return PASS(idx.length ? idx.length + ' status check' + (idx.length > 1 ? 's' : '') + '.' : 'No status polling.', ev);
      } },
    { id: 'unverified-claims', group: 'Habits', name: 'Replies that admit work was not verified', sev: 'medium', src: BP,
      why: 'When Claude says something was not tested or checked, the turn is not done yet. Following up now is cheaper than finding it later.',
      run(C) {
        if (!C.hasCL) return NA('Needs the Turns classifier.');
        const hits = C.tcls.map((c, i) => ({ c, t: C.tturns[i] })).filter(x => x.c.flags.unverified && (x.c.facts.files.length || x.c.roll.edit));
        if (!C.tturns.length) return NA('No turns.');
        if (hits.length) return { status: hits.length >= 3 ? 'fail' : 'warn', headline: hits.length + ' turn' + (hits.length > 1 ? 's' : '') + ' changed files and said ' + (hits.length > 1 ? 'they were' : 'it was') + ' not verified.', evidence: hits.slice(0, 8).map(x => ({ id: x.t.id, at: x.t.ts, label: '+' + fmtT(x.t.ts - C.t0), text: '«' + x.c.flags.unverified + '» · ' + x.c.facts.files.slice(0, 2).join(', ') })),
          fix: 'Reply "verify it now: run the tests / open it and check", or make it a rule in CLAUDE.md: "never report done without running <check>".' };
        return PASS('No unverified changes reported.');
      } },
    { id: 'destructive-commands', group: 'Habits', name: 'Destructive commands', sev: 'medium', src: PERM,
      why: 'rm -rf, git reset --hard, force pushes and pkill cannot be undone by /rewind. Worth a deny rule or an ask rule if they surprise you.',
      run(C) {
        if (!C.hasCL) return NA('Needs the Turns classifier.');
        const hits = [];
        C.tcls.forEach((c, i) => c.actions.forEach(a => { const m = String(a.arg || '').match(/\brm -rf\b[^;&|]*|\bgit (reset --hard|push (-f|--force)|clean -fd)[^;&|]*|\bpkill\b[^;&|]*|\bkillall\b[^;&|]*/); if (m) hits.push({ t: C.tturns[i], cmd: m[0] }); }));
        if (!C.tturns.length) return NA('No turns.');
        if (hits.length) return { status: 'warn', headline: hits.length + ' destructive command' + (hits.length > 1 ? 's' : '') + ' ran.', evidence: hits.slice(0, 8).map(x => ({ id: x.t.id, at: x.t.ts, label: '+' + fmtT(x.t.ts - C.t0), text: x.cmd })),
          fix: 'If any surprised you, add them to permissions.ask (or deny) in .claude/settings.json:\n{ "permissions": { "ask": ["Bash(rm -rf:*)", "Bash(git reset --hard:*)", "Bash(git push --force:*)"] } }' };
        return PASS('No destructive commands.');
      } },
    { id: 'secret-in-prompt', group: 'Habits', name: 'A key or token pasted into a prompt', sev: 'high', src: D + 'security',
      why: 'Anything in a prompt is stored in the transcript on disk and sent to the model. Keys belong in the environment or a secrets manager.',
      run(C) {
        if (!C.hasCL) return NA('Needs the Turns classifier.');
        const hits = C.tcls.map((c, i) => ({ c, t: C.tturns[i] })).filter(x => x.c.facts.secret);
        if (!C.tturns.length) return NA('No turns.');
        if (hits.length) return { status: 'fail', headline: hits.length + ' prompt' + (hits.length > 1 ? 's contain' : ' contains') + ' what looks like a live key.', evidence: hits.map(x => ({ id: x.t.id, at: x.t.ts, label: '+' + fmtT(x.t.ts - C.t0), text: clip(x.t.prompt.replace(/([A-Za-z0-9_-]{6})[A-Za-z0-9_-]{10,}/g, '$1…'), 120) })),
          fix: 'Rotate the key now. Then put it in an environment variable (or your shell profile) and tell Claude its name, not its value.' };
        return PASS('No keys in prompts.');
      } },
    { id: 'repeated-prompts', group: 'Habits', name: 'The same prompt sent again', sev: 'low', src: BP,
      why: 'Re-sending an identical prompt usually means the first attempt did not land. A rewind plus a sharper prompt beats repetition.',
      run(C) {
        if (!C.hasCL) return NA('Needs the Turns classifier.');
        const hits = C.tcls.map((c, i) => ({ c, t: C.tturns[i] })).filter(x => x.c.source === 'human' && x.c.facts.repeat);
        if (!C.tturns.length) return NA('No turns.');
        if (hits.length >= 2) return { status: 'warn', headline: hits.length + ' prompts repeated the one before word for word.', evidence: hits.slice(0, 8).map(x => ({ id: x.t.id, at: x.t.ts, label: '+' + fmtT(x.t.ts - C.t0), text: '"' + clip(x.t.prompt, 100) + '"' })),
          fix: 'Esc Esc (/rewind) to before the first try and add what was missing: the file, the symptom, how you will check it.' };
        return PASS(hits.length ? 'One repeat.' : 'No repeated prompts.');
      } },

    // ── LLM judge (opt-in, next) ─────────────────────────────────────────────
    { id: 'judge-prompts', group: 'LLM judge', name: 'Prompt quality', sev: 'medium', llm: true, src: BP,
      why: 'Rates each task prompt for location, symptom, constraints and a done-criterion.', run() { return NA('Needs the opt-in LLM judge.'); } },
    { id: 'judge-corrections', group: 'LLM judge', name: 'Corrections on the same issue', sev: 'high', llm: true, src: BP,
      why: 'Reads the corrections semantically, beyond the keyword match.', run() { return NA('Needs the opt-in LLM judge.'); } },
    { id: 'judge-rules', group: 'LLM judge', name: 'Broken standing rules', sev: 'medium', llm: true, src: BP,
      why: 'Finds the rule you restated and the call that broke it.', run() { return NA('Needs the opt-in LLM judge.'); } },
  ];

  function run(S, ids) {
    const C = context(S), want = ids ? new Set(ids) : null;
    return CHECKS.filter(c => !want || want.has(c.id)).map(c => {
      let r; try { r = c.run(C); } catch (err) { r = NA('Check failed: ' + String(err && err.message || err)); }
      return Object.assign({ id: c.id, group: c.group, name: c.name, sev: c.sev, why: c.why, src: c.src, llm: !!c.llm, evidence: [] }, r);
    });
  }
  function score(results) {
    const w = { high: 3, medium: 2, low: 1 }, live = results.filter(r => r.status !== 'na');
    if (!live.length) return null;
    const max = live.reduce((a, r) => a + w[r.sev], 0);
    const got = live.reduce((a, r) => a + w[r.sev] * (r.status === 'pass' ? 1 : r.status === 'warn' ? 0.5 : 0), 0);
    return Math.round(got / max * 100);
  }
  const api = { CHECKS, run, score, context };
  if (typeof window !== 'undefined') window.COACH = api;
  if (typeof module !== 'undefined') module.exports = api;
})();
