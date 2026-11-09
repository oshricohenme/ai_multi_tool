// node coach/test/checks.mjs [dir-of-jsonl-logs]
// Synthetic fire/pass cases for each rules-v2 post check, then every real log through COACH.run.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const here = path.dirname(new URL(import.meta.url).pathname);
try { globalThis.CLASSIFY = require(path.join(here, '../web/classify.js')); } catch (e) { console.log('classify.js not loaded:', e.message); }
const COACH = require(path.join(here, '../web/coach.js'));

// ── synthetic session builder ──
function S() {
  let t = 1e12, n = 0, turn = 0, tid = null;
  const events = [], sigs = [], api = { events, sigs, meta: {} };
  const id = () => 'e' + (++n);
  api.user = (text, o = {}) => { tid = 't' + (++turn); t += o.gap || 5000; events.push({ id: id(), kind: 'user', turnId: tid, payload: text, title: text, start: t }); return api; };
  api.call = (o = {}) => { t += 1000; events.push(Object.assign({ id: id(), kind: 'assistant', turnId: tid, start: t, end: t + 500, model: 'claude-opus-5-5', effort: 'medium', inTok: 100, outTok: 100, cacheRead: 10000, cacheWrite: 100, usd: 0.05, result: '' }, o)); return api; };
  api.tool = (tool, input, o = {}) => { t += 1000; events.push(Object.assign({ id: id(), kind: 'tool', turnId: tid, tool, payload: JSON.stringify(input), start: t, end: t + 100, result: '', isError: false, est: 100 }, o)); return api; };
  api.sig = (ev, sum) => { t += 10; sigs.push({ at: t, ev, sum }); return api; };
  api.compact = (o = {}) => { t += 1000; events.push(Object.assign({ id: id(), kind: 'compact', start: t, end: t + 100, trigger: 'auto', before: 150000, after: 20000 }, o)); return api; };
  api.at = ms => { t += ms; return api; };
  return api;
}
const run = (s, id) => COACH.run({ events: s.events, sigs: s.sigs, meta: s.meta }, [id])[0];
const fire = (id, s) => { const r = run(s, id); assert.ok(r.status === 'warn' || r.status === 'fail', id + ' should fire, got ' + r.status + ': ' + r.headline); };
const quiet = (id, s) => { const r = run(s, id); assert.ok(r.status === 'pass' || r.status === 'na', id + ' should not fire, got ' + r.status + ': ' + r.headline); };
const bash = (s, cmd, err) => s.tool('Bash', { command: cmd }, { isError: !!err, result: err ? 'Exit code 1\nFAIL x' : 'ok' });
const edit = (s, f, a, b) => s.tool('Edit', { file_path: f, old_string: a || 'a', new_string: b || 'b' });

const cases = {
  'test-tampering': {
    fire: () => { const s = S().user('make the build green').call(); bash(s, 'npm test', 1); edit(s, '/r/src/a.test.js', 'expect(1).toBe(2)', 'expect(1).toBe(1)'); bash(s, 'npm test'); return s; },
    pass: () => { const s = S().user('make the build green').call(); bash(s, 'npm test', 1); edit(s, '/r/src/a.js'); edit(s, '/r/src/a.test.js'); bash(s, 'npm test'); return s; },
  },
  'same-failure-loop': {
    fire: () => { const s = S().user('run it').call(); for (let i = 0; i < 3; i++) bash(s, 'node build.js', 1); return s; },
    pass: () => { const s = S().user('run it').call(); bash(s, 'node build.js', 1); bash(s, 'node build.js', 1); bash(s, 'node other.js', 1); return s; },
  },
  'no-plan-big-change': {
    fire: () => { const s = S().user('rename everything').call(); for (let i = 0; i < 6; i++) edit(s, '/r/f' + i + '.js'); s.user('no, that is wrong'); return s; },
    pass: () => { const s = S().user('rename everything').call(); s.sig('classic.Stop', { permission_mode: 'plan' }); for (let i = 0; i < 6; i++) edit(s, '/r/f' + i + '.js'); s.user('no, that is wrong'); return s; },
  },
  'continue-without-goal': {
    fire: () => { const s = S().user('port the modules').call({ result: 'Done with A. Remaining: B, C.' }).user('continue').call({ result: 'Next I will do C.' }).user('continue').call(); return s; },
    pass: () => { const s = S().user('port the modules').call({ result: 'Remaining: B' }).user('/goal tests pass'); s.sig('prompt.submit', { command: '{"name":"goal","args":"tests pass"}' }); s.user('continue').call({ result: 'Next I will do C.' }).user('continue').call(); return s; },
  },
  'undo-by-prose': {
    fire: () => S().user('add a button').call().user('undo that').call(),
    pass: () => { const s = S().user('add a button').call().user('undo that').call(); s.sig('prompt.submit', { command: '{"name":"rewind","args":""}' }); return s; },
  },
  'correction-not-saved': {
    fire: () => { const s = S().user('use tabs').call(); edit(s, '/r/a.js'); s.user('no, wrong, use tabs').call(); s.user('still wrong, use tabs').call(); s.user('again, not tabs').call(); return s; },
    pass: () => { const s = S().user('use tabs').call(); edit(s, '/r/a.js'); s.user('no, wrong, use tabs').call(); s.user('still wrong, use tabs').call(); s.user('again, not tabs').call(); edit(s, '/r/CLAUDE.md'); return s; },
  },
  'rule-lost-at-compaction': {
    fire: () => S().user('always use pnpm never use npm here please').call().compact().user('remember always use pnpm never use npm here please').call(),
    pass: () => S().user('always use pnpm never use npm here please').call().compact().user('never commit secrets to the repository ever').call(),
  },
  'fast-mode-toggle': {
    fire: () => { const s = S().user('hi').call(); s.sig('prompt.submit', { command: '{"name":"fast","args":""}' }); s.user('x').call(); s.sig('prompt.submit', { command: '{"name":"fast","args":""}' }); return s; },
    pass: () => { const s = S().user('hi').call(); s.sig('prompt.submit', { command: '{"name":"fast","args":""}' }); return s; },
  },
  'repeated-style-asks': {
    fire: () => { const s = S(); ['shorter please', 'be brief', 'no preamble', 'a b c'].forEach(p => s.user(p).call()); return s; },
    pass: () => { const s = S(); ['shorter please', 'do x', 'do y', 'do z'].forEach(p => s.user(p).call()); return s; },
  },
  'manual-formatter': {
    fire: () => { const s = S().user('go').call(); for (let i = 0; i < 3; i++) bash(s, 'npx prettier --write src'); return s; },
    pass: () => { const s = S().user('go').call(); bash(s, 'npx prettier --write src'); bash(s, 'echo black'); return s; },
  },
  'exploration-in-main': {
    fire: () => { const s = S().user('find it').call(); for (let i = 0; i < 8; i++) s.tool('Read', { file_path: '/r/f' + i }, { est: 5000 }); return s; },
    pass: () => { const s = S().user('find it').call(); for (let i = 0; i < 8; i++) s.tool('Read', { file_path: '/r/f' + i }, { est: 5000 }); s.tool('Agent', { prompt: 'x' }); return s; },
  },
  'generated-dir-reads': {
    fire: () => S().user('x').call().tool('Read', { file_path: '/r/node_modules/a/index.js' }, { est: 9000 }),
    pass: () => S().user('x').call().tool('Read', { file_path: '/r/node_modules/a/index.js' }, { est: 200 }).tool('Read', { file_path: '/r/src/a.js' }, { est: 9000 }),
  },
  'long-session-no-checkpoint': {
    fire: () => { const s = S().user('go').call().compact().user('more').call().compact().user('more').call(); return s; },
    pass: () => { const s = S().user('go').call().compact().user('more').call().compact().user('more').call(); bash(s, 'git commit -m x'); return s; },
  },
};

let fails = 0;
for (const [id, c] of Object.entries(cases)) {
  try { fire(id, c.fire()); quiet(id, c.pass()); console.log('ok   ', id); } catch (e) { fails++; console.log('FAIL ', id, '-', e.message); }
}
// every new check exists
for (const id of Object.keys(cases)) assert.ok(COACH.CHECKS.some(c => c.id === id), 'missing ' + id);

// refinements
try {
  const s = S().user('please ultrathink about this one, be ambitious').call().user('x').call().user('y').call();
  assert.equal(run(s, 'obsolete-incantations').status, 'pass', 'ultrathink/be ambitious are allowlisted');
  const s2 = S().user('KEEP GOING UNTIL EVERYTHING IS DONE').call().user('x').call();
  assert.equal(run(s2, 'obsolete-incantations').status, 'warn', 'all-caps line');
  const s3 = S().user('do it').call(); s3.sig('prompt.submit', { command: '{"name":"compact","args":""}' }); s3.compact({ trigger: 'manual', before: 180000 });
  assert.equal(run(s3, 'costly-compact').status, 'warn', 'bare /compact');
  console.log('ok    refinements');
} catch (e) { fails++; console.log('FAIL  refinements -', e.message); }

// ── real logs: replay the jsonl op stream into {events, sigs, meta} ──
function load(f) {
  const ev = new Map(), sigs = [], meta = {};
  for (const l of fs.readFileSync(f, 'utf8').split('\n')) {
    if (!l.trim()) continue; let o; try { o = JSON.parse(l); } catch { continue; }
    if (o.op === 'put') ev.set(o.ev.id, Object.assign({}, o.ev));
    else if (o.op === 'patch') { const e = ev.get(o.id); if (e) Object.assign(e, o.p); }
    else if (o.op === 'sig') sigs.push(...o.list);
    else if (o.op === 'meta') Object.assign(meta, o.meta);
  }
  return { events: [...ev.values()], sigs, meta };
}
const dir = process.argv[2] || (fs.existsSync('/tmp/rp') ? '/tmp/rp' : path.join(os.homedir(), '.claude/dev-tools/sessions'));
const files = fs.readdirSync(dir).filter(f => f.endsWith('.jsonl'));
const NEW = Object.keys(cases).concat(['obsolete-incantations', 'model-switch', 'destructive-commands', 'costly-compact', 'overthinking', 'idle-mcp', 'repeated-approval']);
const table = [];
for (const f of files) {
  const S0 = load(path.join(dir, f));
  const res = COACH.run(S0);
  const bad = res.filter(r => /^Check failed/.test(r.headline || ''));
  try { assert.equal(bad.length, 0, bad.map(b => b.id + ': ' + b.headline).join('; ')); } catch (e) { fails++; console.log('FAIL real', f, e.message); }
  table.push({ s: f.slice(0, 8), ev: S0.events.length, r: Object.fromEntries(res.map(r => [r.id, r.status])) });
}
console.log('\nreal sessions (F=fail W=warn .=pass -=na)');
console.log('check'.padEnd(30) + table.map(t => t.s).join(' '));
for (const id of NEW) console.log(id.padEnd(30) + table.map(t => ({ fail: 'F       ', warn: 'W       ', pass: '.       ', na: '-       ' }[t.r[id]] || '?       ')).join(' '));
console.log('events'.padEnd(30) + table.map(t => String(t.ev).padEnd(8)).join(' '));
console.log(fails ? '\n' + fails + ' FAILED' : '\nall passed');
process.exit(fails ? 1 : 0);
