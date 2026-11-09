// Coach Setup rules: read the repo/user config snapshot from /api/setup (S.setup) and register
// themselves into COACH.CHECKS. Spec: coach/RULES.md "Setup rules". Same check shape as coach.js.
(function () {
  const D = 'https://code.claude.com/docs/en/';
  const NA = h => ({ status: 'na', headline: h });
  const PASS = (h, evidence) => ({ status: 'pass', headline: h, evidence: evidence || [] });
  const clip = (s, n) => { s = String(s || '').replace(/\s+/g, ' ').trim(); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
  // the evidence label column is narrow: keep the tail of a long path
  const ev = (label, text) => ({ at: 0, label: label.length > 17 ? '…' + label.slice(-16) : label, text: clip(text, 160) });
  const pl = (n, w) => n + ' ' + w + (n === 1 ? '' : 's');

  // The snapshot arrives as S.setup (C.setup once coach.js passes it through) or S.meta.setup.
  const setupOf = C => C.setup || (C.meta && C.meta.setup) || null;
  const mds = s => s.claudeMd || [];
  const cfgs = s => [s.settings && s.settings.project, s.settings && s.settings.local, s.settings && s.settings.user].filter(Boolean);
  const denies = s => cfgs(s).flatMap(c => (c.permissions && c.permissions.deny) || []).map(String);
  const hooksOf = s => cfgs(s).map(c => c.hooks).filter(Boolean);
  const lines = md => md.text.split('\n').map((t, i) => ({ t, n: i + 1 }));
  const mdEv = (md, l) => ev((md.scope === 'user' ? '~/' : '') + 'CLAUDE.md:' + l.n, l.t);

  function rule(id, name, sev, src, why, body) {
    return { id, group: 'Setup', name, sev, src: D + src, why,
      run(C) { const s = setupOf(C); return s ? body(s, C) : NA('Repo and config files not read yet.'); } };
  }
  const hit = (sev, headline, evidence, fix) => ({ status: sev === 'high' ? 'fail' : 'warn', headline, evidence, fix });

  const CMD = /\b(npm (run )?(test|lint|build|typecheck|check)|pnpm (run )?(test|lint|build|typecheck)|yarn (test|lint|build|typecheck)|bun (run )?test|pytest|jest|vitest|mocha|cargo (test|check|clippy|build)|go (test|vet|build)|tsc\b|make (test|check|lint|build)|ruff|eslint|mypy|rspec|phpunit|node --test|node --check|swift (test|build)|xcodebuild|mt apply)/i;

  const CHECKS = [
    rule('setup-no-check-command', 'No test or check command in CLAUDE.md', 'high', 'best-practices',
      'Claude can check its own work only if it knows the command. A test, typecheck or lint line in CLAUDE.md (or a Stop/PostToolUse hook) is the single most useful line.',
      s => {
        if (!s.hasCode) return NA('No code found in this repo.');
        const hooks = hooksOf(s).some(h => ['Stop', 'PostToolUse'].some(k => h[k] && h[k].length));
        const found = mds(s).some(m => CMD.test(m.text));
        if (found || hooks) return PASS(hooks && !found ? 'A Stop/PostToolUse hook runs checks.' : 'CLAUDE.md names a check command.');
        return hit('high', mds(s).length ? 'No test, typecheck, lint or build command in CLAUDE.md, and no Stop or PostToolUse hook.' : 'No CLAUDE.md, so Claude has no check command to run.',
          mds(s).map(m => ev(m.path, pl(m.lines, 'line') + ', no command')),
          'Add a "## Commands" section to CLAUDE.md: the test, typecheck and lint commands, one per line.');
      }),

    rule('setup-secrets-not-denied', 'Secret files are readable', 'medium', 'best-practices#limit-access',
      'Claude can read .env files and keys unless a deny rule blocks them. A deny rule holds in every permission mode.',
      s => {
        const d = denies(s), z = s.sensitive || {}, miss = [];
        const has = re => d.some(x => /^Read\(/.test(x) && re.test(x));
        if ((z.env || []).length && !has(/\.env/)) miss.push(ev('.env', (z.env || []).join(', ') + ' has no Read deny'));
        if (z.secretsDir && !has(/secrets/)) miss.push(ev('secrets/', 'secrets/ has no Read deny'));
        if ((z.pem || []).length && !has(/\.pem/)) miss.push(ev('*.pem', (z.pem || []).join(', ') + ' has no Read deny'));
        if (miss.length) return hit('medium', pl(miss.length, 'kind') + ' of secret file in the repo without a Read deny rule.', miss,
          '"permissions": { "deny": ["Read(./.env)", "Read(./.env.*)", "Read(./secrets/**)", "Read(./**/*.pem)"] } in .claude/settings.json');
        return PASS((z.env || []).length || z.secretsDir || (z.pem || []).length ? 'Secret files are covered by deny rules.' : 'No secret files in the repo root.');
      }),

    rule('setup-never-rules-unenforced', 'A "never" rule nothing enforces', 'medium', 'best-practices#enforce-the-rules-that-cant-have-exceptions',
      'CLAUDE.md is advice; Claude can still break it. A hard rule needs a deny rule or a hook.',
      s => {
        const hay = (JSON.stringify(denies(s)) + JSON.stringify(hooksOf(s))).toLowerCase();
        const STOP = new Set(['to', 'the', 'a', 'an', 'any', 'files', 'file', 'in', 'on', 'of', 'directly', 'without', 'ever', 'anything', 'and', 'or', 'it', 'them', 'this', 'that', 'unless', 'asked', 'for', 'from', 'by', 'with', 'your', 'my', 'we', 'you', 'user', 'users', 'is', 'are']);
        const bad = [];
        for (const m of mds(s)) for (const l of lines(m)) {
          const r = /\b(?:never|don'?t|do not)\s+(edit|touch|push|commit|delete|read|modify|force[- ]push)\b\s*([^.\n]{0,60})/i.exec(l.t);
          if (!r) continue;
          const verb = r[1].toLowerCase().replace(/[- ]/g, ' ');
          const keys = r[2].toLowerCase().replace(/[`*"'()]/g, ' ').split(/\s+/).map(w => w.replace(/^\.?\/+|[,;:]+$/g, '')).filter(w => w.length > 2 && !STOP.has(w));
          const enforced = (/push/.test(verb) && /git push|push/.test(hay)) || (verb === 'commit' && /git commit/.test(hay)) || keys.some(k => hay.includes(k));
          if (!enforced) bad.push(mdEv(m, l));
        }
        if (bad.length) return hit('medium', pl(bad.length, 'rule') + ' in CLAUDE.md say "never" with no deny rule or hook behind them.', bad.slice(0, 6),
          'Make the hard ones a deny rule ("deny": ["Edit(./migrations/**)", "Bash(git push:*)"]) or a PreToolUse hook that exits 2.');
        return PASS('Every "never" rule has a deny rule or hook, or there are none.');
      }),

    rule('setup-automode-misconfig', 'Auto mode settings are ignored or unsafe', 'high', 'auto-mode-config',
      'autoMode is read only from user settings, and a list without "$defaults" silently drops the built-in blocks.',
      s => {
        const st = s.settings || {}, bad = [];
        for (const [k, label] of [['project', '.claude/settings.json'], ['local', '.claude/settings.local.json']]) if (st[k] && st[k].autoMode) bad.push(ev(label, 'autoMode here is ignored; it is only read from ~/.claude/settings.json'));
        const am = st.user && st.user.autoMode;
        if (am && typeof am === 'object') for (const [list, v] of Object.entries(am)) if (Array.isArray(v) && v.length && !v.includes('$defaults')) bad.push(ev('user settings', 'autoMode.' + list + ' has no "$defaults": the built-in rules are dropped'));
        if (bad.length) return hit('high', pl(bad.length, 'auto mode problem'), bad,
          'Move autoMode to ~/.claude/settings.json and start each list with "$defaults". `/auto-mode-setup` walks you through it.');
        return PASS('No autoMode problems.');
      }),

    rule('setup-claude-md-shouting', 'CLAUDE.md shouts', 'low', 'best-practices#what-you-can-stop-doing',
      'Newer models over-apply emphasis. A reason works better than capitals, and a rule that cannot bend belongs in a hook.',
      s => {
        const caps = [], magic = [];
        for (const m of mds(s)) for (const l of lines(m)) {
          if (/\b(MUST|NEVER|ALWAYS|CRITICAL|IMPORTANT|DO NOT|YOU MUST|MANDATORY)\b/.test(l.t)) caps.push(mdEv(m, l));
          if (/think (very )?hard|think carefully|be thorough|be very careful/i.test(l.t)) magic.push(mdEv(m, l));
        }
        if (caps.length > 5 || magic.length) return hit('low', (caps.length > 5 ? pl(caps.length, 'line') + ' shout in CAPITALS' : '') + (caps.length > 5 && magic.length ? '; ' : '') + (magic.length ? pl(magic.length, '"think hard" style line') : '') + '.', caps.slice(0, 4).concat(magic.slice(0, 2)),
          'State the reason instead of the volume ("Run tests before committing: CI rejects the push otherwise"), and turn hard rules into hooks.');
        return PASS('Emphasis is used sparingly.');
      }),

    rule('setup-claude-md-dead-refs', 'CLAUDE.md points at files that are gone', 'low', 'memory',
      'Dead paths send Claude hunting, and a large @import loads in full on every request.',
      s => {
        const dead = (s.refs || []).filter(r => !r.exists), big = (s.refs || []).filter(r => r.exists && r.import && r.lines > 400);
        if (dead.length || big.length) return hit('low', [dead.length ? pl(dead.length, 'missing path') : '', big.length ? pl(big.length, '@import') + ' over 400 lines' : ''].filter(Boolean).join('; ') + '.',
          dead.map(r => ev(r.from.replace(/^.*\//, ''), r.ref + ' does not exist')).concat(big.map(r => ev(r.from.replace(/^.*\//, ''), '@' + r.ref + ' is ' + r.lines + ' lines'))).slice(0, 6),
          'Remove dead references; link big docs as plain paths so they load on demand. `/doctor prompt-audit` finds more.');
        return PASS(pl((s.refs || []).length, 'path') + ' checked, all present.');
      }),

    rule('setup-memory-over-limit', 'Auto-memory index is too long', 'medium', 'memory',
      'Only the first 200 lines or 25 KB of MEMORY.md load; later lines are never seen.',
      s => {
        const m = s.memory;
        if (!m) return NA('No auto-memory MEMORY.md for this project.');
        if (m.lines > 200 || m.bytes > 25 * 1024) return hit('medium', 'MEMORY.md is ' + m.lines + ' lines, ' + Math.round(m.bytes / 1024) + ' KB: the tail is not loaded.', [ev(m.path, m.lines + ' lines · ' + Math.round(m.bytes / 1024) + ' KB')],
          'Consolidate MEMORY.md under 200 lines: merge duplicates, delete stale entries, keep one line per memory.');
        return PASS('MEMORY.md is ' + m.lines + ' lines.');
      }),

    rule('setup-compact-instructions', 'Compaction has no instructions', 'low', 'costs#manage-context-proactively',
      'Auto-compaction keeps what it guesses matters. A "Compact instructions" section tells it what to keep.',
      s => {
        const c = s.compactions || {};
        if (!c.auto) return NA('No auto-compactions in past sessions of this project.');
        if (mds(s).some(m => /compact instructions/i.test(m.text))) return PASS('CLAUDE.md has compact instructions.');
        return hit('low', c.auto + ' auto-compaction' + (c.auto === 1 ? '' : 's') + ' in past sessions and no "Compact instructions" in CLAUDE.md.', [ev('sessions', c.auto + ' auto-compactions across ' + pl(c.sessions, 'session'))],
          'Add a "# Compact instructions" section to CLAUDE.md naming what to keep: the task, the files touched, open decisions, failing tests.');
      }),

    rule('setup-agent-defs', 'Subagent definition wastes the big model', 'low', 'sub-agents',
      'A read-only agent on the inherited big model costs more than it needs to, and an agent with no tools line gets every tool.',
      s => {
        const ag = s.agents || [];
        if (!ag.length) return NA('No .claude/agents in this repo.');
        const RO = new Set(['read', 'grep', 'glob', 'ls', 'webfetch', 'websearch']), bad = [];
        for (const a of ag) {
          const tools = String(a.fm.tools || '').split(',').map(t => t.trim().toLowerCase()).filter(Boolean), model = String(a.fm.model || '');
          if (!tools.length) bad.push(ev(a.file, 'no tools: line, so it gets every tool'));
          else if (tools.every(t => RO.has(t)) && (!model || model === 'inherit')) bad.push(ev(a.file, 'read-only tools but no model:, so it inherits the big model'));
        }
        if (bad.length) return hit('low', pl(bad.length, 'agent') + ' to tighten.', bad, 'For read-only agents set `model: haiku` and `tools: Read, Grep, Glob`.');
        return PASS(pl(ag.length, 'agent') + ' set up with tools and a model.');
      }),

    rule('setup-side-effect-skills', 'A skill with side effects can run on its own', 'medium', 'skills#control-who-invokes-a-skill',
      'Without disable-model-invocation, Claude may decide to run a deploy or push skill by itself.',
      s => {
        const sk = s.skills || [];
        if (!sk.length) return NA('No .claude/skills in this repo.');
        const bad = sk.filter(k => /\b(git push|deploy|publish|migrate|rm -rf|npm publish)\b/i.test(k.body) && k.fm['disable-model-invocation'] !== true);
        if (bad.length) return hit('medium', pl(bad.length, 'skill') + ' push, deploy or delete and Claude may invoke ' + (bad.length === 1 ? 'it' : 'them') + ' unasked.', bad.map(k => ev(k.dir, (k.body.match(/\b(git push|deploy|publish|migrate|rm -rf|npm publish)\b/i) || [''])[0] + ' without disable-model-invocation')),
          'Add `disable-model-invocation: true` to the skill frontmatter so it only runs when you call it.');
        return PASS('No skill with side effects can auto-run.');
      }),

    // setup half of bypass-no-sandbox (the live half watches permission_mode in the session)
    rule('bypass-no-sandbox', 'Bypass mode with no sandbox', 'high', 'permission-modes',
      'Bypass mode approves everything. Without a sandbox, one bad command or injected instruction has no boundary.',
      s => {
        const c = cfgs(s);
        const bypass = c.some(x => (x.permissions && x.permissions.defaultMode === 'bypassPermissions'));
        if (!bypass) return PASS('Settings do not default to bypass mode.');
        if (c.some(x => x.sandbox && x.sandbox.enabled === true)) return PASS('Bypass mode runs inside a sandbox.');
        return hit('high', 'Settings default to bypass mode and no settings file enables the sandbox.', [ev('settings', 'permissions.defaultMode bypassPermissions, sandbox.enabled not set')],
          'Use auto mode (Shift+Tab), or turn on `/sandbox`, or run in a container.');
      }),
  ];

  const api = { CHECKS };
  const COACH = typeof window !== 'undefined' ? window.COACH : (typeof require !== 'undefined' ? require('./coach.js') : null);
  if (COACH && COACH.CHECKS) {
    for (const c of CHECKS) {
      const old = COACH.CHECKS.findIndex(x => x.id === c.id);
      if (old < 0) { COACH.CHECKS.push(c); continue; }
      // a live/post rule of the same id already exists: keep it, add the setup half
      const prev = COACH.CHECKS[old], mine = c.run;
      COACH.CHECKS[old] = Object.assign({}, prev, { run(C) { const r = prev.run(C); if (r.status === 'fail' || r.status === 'warn') return r; const m = mine(C); return m.status === 'na' ? r : m.status === 'pass' && r.status === 'pass' ? r : m; } });
    }
  }
  if (typeof module !== 'undefined') module.exports = api;
})();
