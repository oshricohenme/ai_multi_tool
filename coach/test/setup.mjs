// End to end: two fixture repos -> real server /api/setup -> every Setup rule. Run: node coach/test/setup.mjs
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const HERE = path.dirname(new URL(import.meta.url).pathname)
const COACH = require('../web/coach.js')
require('../web/setup-checks.js')

const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'coach-setup-')))
const w = (f, t) => { fs.mkdirSync(path.dirname(f), { recursive: true }); fs.writeFileSync(f, t) }
const home = path.join(tmp, 'home'), bad = path.join(tmp, 'bad'), good = path.join(tmp, 'good'), logs = path.join(tmp, 'logs')
const munge = d => d.replace(/[^A-Za-z0-9]/g, '-')

// ── bad repo: every rule fires ──
fs.mkdirSync(path.join(bad, '.git'), { recursive: true })
w(path.join(bad, 'package.json'), '{}')
w(path.join(bad, '.env'), 'API_KEY=hunter2-not-a-real-key\n')
w(path.join(bad, 'CLAUDE.md'), ['# Project', ...['MUST be careful', 'NEVER skip this', 'CRITICAL rule', 'IMPORTANT note', 'ALWAYS ask', 'DO NOT guess'], 'Never edit migrations/ by hand', 'See `docs/gone.md` for the design.', 'Think hard before answering.'].join('\n'))
w(path.join(bad, '.claude/settings.json'), JSON.stringify({ autoMode: { allow: ['x'] }, env: { MY_TOKEN: 'sk-abcdefghijklmnop' } }))
w(path.join(bad, '.claude/settings.local.json'), JSON.stringify({ permissions: { defaultMode: 'bypassPermissions' } }))
w(path.join(bad, '.claude/agents/finder.md'), '---\nname: finder\ntools: Read, Grep\n---\nFind things.')
w(path.join(bad, '.claude/skills/ship/SKILL.md'), '---\nname: ship\ndescription: ship it\n---\nRun `git push` then deploy.')
w(path.join(home, '.claude/projects', munge(bad), 'memory/MEMORY.md'), Array.from({ length: 250 }, (_, i) => `- entry ${i}`).join('\n'))

// ── good repo: every rule passes ──
fs.mkdirSync(path.join(good, '.git'), { recursive: true })
w(path.join(good, 'package.json'), '{}')
w(path.join(good, 'src/index.js'), 'x')
w(path.join(good, '.env'), 'A=1\n')
w(path.join(good, 'CLAUDE.md'), '## Commands\n- test: `npm test`\n\nEntry is `src/index.js`.\n\n# Compact instructions\nKeep the task and failing tests.\n')
w(path.join(good, '.claude/settings.json'), JSON.stringify({ permissions: { deny: ['Read(./.env)'] } }))
w(path.join(good, '.claude/agents/finder.md'), '---\nname: finder\nmodel: haiku\ntools: Read, Grep, Glob\n---\nFind things.')
w(path.join(good, '.claude/skills/ship/SKILL.md'), '---\nname: ship\ndisable-model-invocation: true\n---\nRun git push.')
w(path.join(home, '.claude/projects', munge(good), 'memory/MEMORY.md'), '- one\n- two\n')

// session logs: one per repo; the bad one had an auto compaction
const sess = (id, cwd, extra = []) => w(path.join(logs, id + '.jsonl'), [{ op: 'meta', meta: { cwd } }, { op: 'put', ev: { id: 'u' + id, kind: 'user', turnId: 't1', start: 1, payload: 'hi' } }, ...extra].map(o => JSON.stringify(o)).join('\n') + '\n')
sess('sbad', bad, [{ op: 'put', ev: { id: 'c1', kind: 'compact', trigger: 'auto', start: 2, before: 1 } }])
sess('sgood', good, [{ op: 'put', ev: { id: 'c1', kind: 'compact', trigger: 'auto', start: 2, before: 1 } }])

const PORT = 7960 + Math.floor(Math.random() * 30)
const srv = spawn('node', [path.join(HERE, '../server/server.mjs'), 'sgood'], { env: { ...process.env, HOME: home, DEVTOOLS_DIR: logs, DEVTOOLS_PORT: String(PORT) }, stdio: ['ignore', 'pipe', 'inherit'] })
try {
  const port = await new Promise((res, rej) => { srv.stdout.once('data', d => res(JSON.parse(String(d)).port)); srv.once('exit', () => rej(new Error('server exited'))) })
  const get = async cwd => { const r = await fetch(`http://127.0.0.1:${port}/api/setup?cwd=${encodeURIComponent(cwd)}`); return { status: r.status, text: await r.text() } }
  const ids = COACH.CHECKS.filter(c => c.group === 'Setup' && (c.id.startsWith('setup-') || c.id === 'bypass-no-sandbox')).map(c => c.id)
  assert.equal(ids.length, 11, 'all 11 setup rules registered')
  const run = snap => Object.fromEntries(COACH.run({ events: [], sigs: [], meta: { setup: snap }, setup: snap }, ids).map(r => [r.id, r.status]))

  const b = await get(bad), g = await get(good)
  assert.equal(b.status, 200); assert.equal(g.status, 200)
  assert(!/hunter2|sk-abcdefghijklmnop/.test(b.text), 'no secret values in the snapshot')
  const rb = run(JSON.parse(b.text)), rg = run(JSON.parse(g.text))
  console.log('bad ', JSON.stringify(rb)); console.log('good', JSON.stringify(rg))
  for (const id of ids) { assert(['fail', 'warn'].includes(rb[id]), `bad repo: ${id} should fire, got ${rb[id]}`); assert.equal(rg[id], 'pass', `good repo: ${id}`) }
  // no snapshot -> n/a
  for (const r of COACH.run({ events: [], sigs: [], meta: {} }, ids)) assert.equal(r.status, 'na', r.id)
  // a cwd nobody has used is refused
  for (const c of ['/etc', os.homedir(), tmp, 'relative/path']) assert.equal((await get(c)).status, 403, 'refused ' + c)
  console.log('ok: 11 rules fire on the bad repo, pass on the good one, na with no snapshot, outside cwds refused')
} finally { srv.kill() }
