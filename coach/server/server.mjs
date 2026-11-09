// dev-tools local server. Started by the mod, one per Claude Code session; no dependencies.
//   ingest: a Unix socket only the mod writes to (POST one op per request)
//   web:    127.0.0.1, read only: the page, a live SSE stream, and past sessions from disk
// Everything stays on this machine: ~/.claude/dev-tools/sessions/<session>.jsonl
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import * as transcripts from './transcripts.mjs'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const PAGE = path.join(HERE, '..', 'web', 'index.html')
const SID = (process.argv[2] || `s${Date.now()}`).replace(/[^\w.-]/g, '_')
const DIR = process.env.DEVTOOLS_DIR || path.join(os.homedir(), '.claude', 'dev-tools', 'sessions')
const LOG = path.join(DIR, `${SID}.jsonl`)
const SOCK = path.join(os.tmpdir(), `devtools-${process.pid}.sock`)
const FIRST_PORT = Number(process.env.DEVTOOLS_PORT || 7777)

fs.mkdirSync(DIR, { recursive: true })

// One server per session. A mod reload starts a new one; it asks the previous one (named in
// this record) to leave over that server's private socket, then takes the first free port.
const RECORD = path.join(DIR, `${SID}.server.json`)
async function retirePrevious() {
  let prev
  try { prev = JSON.parse(fs.readFileSync(RECORD, 'utf8')) } catch { return }
  if (!prev.sock || prev.pid === process.pid) return
  await new Promise(resolve => {
    const req = http.request({ socketPath: prev.sock, method: 'POST', path: '/' }, res => { res.resume(); res.on('end', resolve) })
    req.on('error', resolve) // already gone
    req.setTimeout(2000, () => { req.destroy(); resolve() })
    req.end(JSON.stringify({ op: 'bye' }))
  })
  // wait for its port to free (up to ~2s); a server too old to know 'bye' gets SIGTERM,
  // but only while its socket (named after its pid) still exists, so a reused pid is left alone
  for (let i = 0; i < 40; i++) {
    try { process.kill(prev.pid, 0) } catch { return }
    if (i === 20 && fs.existsSync(prev.sock) && prev.sock.endsWith(`-${prev.pid}.sock`)) { try { process.kill(prev.pid, 'SIGTERM') } catch {} }
    await new Promise(r => setTimeout(r, 100))
  }
}
await retirePrevious()

// ── state: replaying the log rebuilds it, so a restart (mod reload) resumes the session ──
function replay(lines) {
  const s = { meta: {}, events: [], byId: new Map(), sigs: [], series: [] }
  for (const line of lines) {
    if (!line) continue
    try { apply(s, JSON.parse(line)) } catch {}
  }
  return s
}
function apply(s, op) {
  if (op.op === 'put') {
    if (s.byId.has(op.ev.id)) Object.assign(s.byId.get(op.ev.id), op.ev)
    else { s.events.push(op.ev); s.byId.set(op.ev.id, op.ev) }
  } else if (op.op === 'patch') {
    const ev = s.byId.get(op.id)
    if (ev) Object.assign(ev, op.p)
  } else if (op.op === 'meta') Object.assign(s.meta, op.meta)
  else if (op.op === 'sig') {
    for (const g of op.list) {
      s.sigs.push(g)
      if (g.ev === 'session.measure' && g.sum) s.meta.limits = { at: g.at, rateLimits: g.sum.rateLimits, cost: g.sum.cost, context: g.sum.context }
      if (g.ev === 'classic.PreModelSwitch' && g.sum) s.meta.cacheTtl = g.sum.cache_ttl
    }
    if (s.sigs.length > 30000) s.sigs.splice(0, s.sigs.length - 20000)
    s.series.push({ at: op.at, c: op.counts })
    if (s.series.length > 7200) s.series.splice(0, s.series.length - 3600)
  }
}
const readLog = file => { try { return fs.readFileSync(file, 'utf8').split('\n') } catch { return [] } }
const live = replay(readLog(LOG))
const out = fs.createWriteStream(LOG, { flags: 'a' })
const clients = new Set()

function ingest(op) {
  apply(live, op)
  out.write(`${JSON.stringify(op)}\n`)
  const msg = `data: ${JSON.stringify(op)}\n\n`
  for (const c of clients) c.write(msg)
}

// ── past sessions: a small summary each, cached by mtime ──
const summaries = new Map()
// context window of a model: what the data reports wins; else 1M for Opus/Sonnet/Fable (and any [1m] id), 200k for Haiku/unknown
const windowOf = (model, meta) => meta.window || (meta.context && meta.context.max) || (/haiku/i.test(model || '') ? 2e5 : /\[1m\]|opus|sonnet|fable/i.test(model || '') ? 1e6 : 2e5)
const alive = pid => { try { process.kill(pid, 0); return true } catch (e) { return e.code === 'EPERM' } }
function summary(file) {
  const st = fs.statSync(file)
  const hit = summaries.get(file)
  if (hit && hit.mtime === st.mtimeMs) return hit.v
  const s = replay(readLog(file))
  const calls = s.events.filter(e => e.kind === 'assistant')
  const tok = k => calls.reduce((a, e) => a + (e[k] || 0), 0)
  const input = tok('inTok') + tok('cacheRead') + tok('cacheWrite')
  const main = calls.filter(e => !e.agentId && e.inTok !== undefined), lastMain = main.at(-1) // last call that has usage (an in-flight one has none yet)
  const act = ['classic.PostToolUse', 'classic.PermissionDenied', 'classic.UserPromptSubmit', 'classic.Stop']
  const askAt = Math.max(0, ...s.sigs.filter(g => g.ev === 'classic.PermissionRequest' || (g.ev === 'classic.Notification' && g.sum && g.sum.notification_type !== 'idle_prompt')).map(g => g.at))
  const stopped = new Set(s.sigs.filter(g => g.ev === 'classic.SubagentStop' && g.sum).map(g => g.sum.agent_id))
  const agentLast = new Map()
  for (const e of s.events) if (e.agentId) agentLast.set(e.agentId, Math.max(agentLast.get(e.agentId) || 0, e.end || e.start || 0))
  const firstPrompt = s.events.find(e => e.kind === 'user' && !e.agentId && !/^\s*</.test(e.payload || e.title || '')) || s.events.find(e => e.kind === 'user' && !e.agentId)
  const v = {
    // fresh-per-request fields (live/port/agents.running/status) are added in sessionOut
    lastAt: Math.max(0, ...s.events.map(e => e.end || e.start || 0), ...s.sigs.filter(g => act.includes(g.ev)).map(g => g.at)) || null,
    title: firstPrompt ? (firstPrompt.payload || firstPrompt.title || '').replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 80) : '',
    ctxPct: lastMain ? Math.round((lastMain.inTok + lastMain.cacheRead + lastMain.cacheWrite || 0) / windowOf(lastMain.model, s.meta) * 100) : null,
    _askAt: askAt, _agents: [...agentLast].map(([id, last]) => ({ id, last, stopped: stopped.has(id) })),
    id: path.basename(file, '.jsonl'),
    cwd: s.meta.cwd, model: s.meta.model || lastMain?.model || [...s.events].reverse().find(e => e.kind === 'assistant' && e.model)?.model, start: s.events[0]?.start, end: s.events.at(-1)?.end ?? s.events.at(-1)?.start,
    turns: new Set(s.events.filter(e => e.kind === 'user' && !e.agentId).map(e => e.turnId)).size,
    calls: calls.length, input, cacheRead: tok('cacheRead'), output: tok('outTok'),
    usd: calls.reduce((a, e) => a + (e.usd || 0), 0) + s.events.filter(e => e.kind === 'compact').reduce((a, e) => a + (e.usd || 0), 0),
    subagentInput: calls.filter(e => e.agentId).reduce((a, e) => a + (e.inTok || 0) + (e.cacheRead || 0) + (e.cacheWrite || 0), 0),
    compactions: s.events.filter(e => e.kind === 'compact').map(e => ({ trigger: e.trigger, before: e.before, window: s.meta.window })),
    toolResults: s.events.filter(e => e.kind === 'tool').map(e => e.est || 0),
    turnCosts: Object.values(calls.reduce((m, e) => ((m[e.turnId] = (m[e.turnId] || 0) + (e.usd || 0)), m), {})),
    peakPercent: Math.max(0, ...calls.filter(e => !e.agentId).map(e => ((e.inTok || 0) + (e.cacheRead || 0) + (e.cacheWrite || 0)) / (s.meta.window || 200000) * 100)),
    ctxPercents: calls.filter(e => !e.agentId).map(e => Math.round(((e.inTok || 0) + (e.cacheRead || 0) + (e.cacheWrite || 0)) / (s.meta.window || 200000) * 100)),
    permissionAsks: s.sigs.filter(g => g.ev === 'classic.PermissionRequest').length,
    toolFailures: s.sigs.filter(g => g.ev === 'classic.PostToolUseFailure').length,
    modelSwitches: calls.filter((e, i) => i && !e.agentId && e.model && calls[i - 1].model && e.model !== calls[i - 1].model).length,
  }
  summaries.set(file, { mtime: st.mtimeMs, v })
  return v
}

// per-request view of a cached summary: the live/status fields change without the log changing
function sessionOut(v) {
  const { _askAt, _agents, ...o } = v
  let rec = null
  try { rec = JSON.parse(fs.readFileSync(path.join(DIR, `${v.id}.server.json`), 'utf8')) } catch {}
  o.live = !!(rec && rec.pid && alive(rec.pid)) // its <id>.server.json exists and the pid answers
  if (o.live) o.port = rec.port
  const now = Date.now()
  // running agent: no SubagentStop for its id, session live, and some activity in the last 30 min
  o.agents = { total: _agents.length, running: o.live ? _agents.filter(a => !a.stopped && now - a.last < 30 * 60e3).length : 0 }
  // needs: live and the latest permission request / non-idle notification is newer than any other activity; running: live and active < 2 min ago; idle: live, quiet; done: not live
  o.status = !o.live ? 'done' : _askAt && _askAt > (o.lastAt || 0) ? 'needs' : now - (o.lastAt || 0) < 120e3 ? 'running' : 'idle'
  return o
}


// ── setup snapshot: what the Coach "Setup" rules read ──
// Only known paths are read (CLAUDE.md files, settings, agents, skills, MEMORY.md). Secret files are listed by name, never read.
const HOME = os.homedir()
const SECRETISH = /token|secret|passw(or)?d|passwd|api[_-]?key|credential|authorization|private/i
const SECRET_VAL = /sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9]{16,}|xox[a-z]-[\w-]+|AKIA[0-9A-Z]{12,}|eyJ[\w-]{10,}\.[\w-]{10,}|Bearer\s+\S+/g
function scrub(v, key = '', depth = 0) {
  if (depth > 8) return null
  if (key === 'env' || key === 'headers') return v && typeof v === 'object' ? Object.fromEntries(Object.keys(v).map(k => [k, '[redacted]'])) : '[redacted]'
  if (SECRETISH.test(key) && v !== null && typeof v !== 'boolean') return '[redacted]'
  if (typeof v === 'string') return v.replace(SECRET_VAL, '[redacted]').slice(0, 2000)
  if (Array.isArray(v)) return v.slice(0, 200).map(x => scrub(x, '', depth + 1))
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, scrub(x, k, depth + 1)]))
  return v
}
const readCapped = (file, max) => { try { const st = fs.statSync(file); if (!st.isFile()) return null; const fd = fs.openSync(file, 'r'); const b = Buffer.alloc(Math.min(st.size, max)); fs.readSync(fd, b, 0, b.length, 0); fs.closeSync(fd); return { text: b.toString('utf8'), size: st.size } } catch { return null } }
const readJson = file => { const r = readCapped(file, 200e3); if (!r) return null; try { return scrub(JSON.parse(r.text)) } catch { return { _unreadable: true } } }
function frontmatter(text) {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/.exec(text)
  if (!m) return { fm: {}, body: text }
  const fm = {}
  for (const l of m[1].split('\n')) {
    const kv = /^([\w-]+):\s*(.*)$/.exec(l)
    if (kv) { let v = kv[2].trim().replace(/^["']|["']$/g, ''); fm[kv[1]] = v === 'true' ? true : v === 'false' ? false : v.slice(0, 500) }
  }
  return { fm, body: m[2] }
}
const tilde = p => (p.startsWith(HOME) ? '~' + p.slice(HOME.length) : p)
// a cwd may be read only if it is the live session's cwd or one a session log has recorded
function cwdAllowed(cwd) {
  if (!cwd || !path.isAbsolute(cwd)) return null
  let real; try { real = fs.realpathSync(cwd) } catch { return null }
  const seen = new Set()
  const add = c => { if (c) try { seen.add(fs.realpathSync(c)) } catch {} }
  add(live.meta.cwd)
  for (const f of fs.readdirSync(DIR).filter(f => f.endsWith('.jsonl'))) { try { add(summary(path.join(DIR, f)).cwd) } catch {} }
  return seen.has(real) ? real : null
}
function setupSnapshot(cwd) {
  let root = cwd
  for (let d = cwd; ; d = path.dirname(d)) { if (fs.existsSync(path.join(d, '.git'))) { root = d; break } if (d === path.dirname(d) || d === HOME) break }
  const chain = []
  for (let d = cwd; ; d = path.dirname(d)) { chain.push(d); if (d === root || d === path.dirname(d)) break }
  let budget = 200e3
  const claudeMd = []
  const addMd = (file, scope) => {
    const r = readCapped(file, 1e6); if (!r) return
    const text = r.text.slice(0, Math.min(100e3, Math.max(0, budget))); budget -= text.length
    claudeMd.push({ path: tilde(file), scope, lines: r.text.split('\n').length, bytes: r.size, text })
  }
  for (const d of chain) for (const f of [path.join(d, 'CLAUDE.md'), path.join(d, '.claude', 'CLAUDE.md')]) addMd(f, 'project')
  addMd(path.join(HOME, '.claude', 'CLAUDE.md'), 'user')
  // paths mentioned in CLAUDE.md files: only existence (and an @import's line count) is returned
  const refs = []
  for (const md of claudeMd) {
    const base = md.scope === 'user' ? path.join(HOME, '.claude') : root
    const seenRef = new Set()
    const toks = [...md.text.matchAll(/`([^`\s]+)`|(?:^|\s)(@[\w.\/~-]+)/g)].map(m => m[1] || m[2])
    for (const raw of toks) {
      const imp = raw.startsWith('@'), t = raw.replace(/^@/, '').replace(/[.,;:)]+$/, '')
      if (!t || seenRef.has(t) || /[*<>{}$()|=\s]|:\/\//.test(t)) continue
      const slash = t.includes('/'), ext = /\.\w{1,5}$/.test(t)
      if (!(imp ? true : slash ? ext || t.endsWith('/') : /\.(md|json|toml|ya?ml|sh|py|mjs|tsx?|cfg|ini)$/.test(t))) continue
      if (t.startsWith('/') && !imp) continue
      seenRef.add(t)
      const abs = path.resolve(t.startsWith('~/') ? path.join(HOME, t.slice(2)) : t.startsWith('/') ? t : path.join(base, t))
      if (!(abs === root || abs.startsWith(root + path.sep) || abs.startsWith(path.join(HOME, '.claude') + path.sep))) continue
      let exists = false, lines = null
      try { const st = fs.statSync(abs); exists = true; if (imp && st.isFile() && st.size < 2e6) lines = fs.readFileSync(abs, 'utf8').split('\n').length } catch {}
      refs.push({ from: md.path, ref: t, exists, import: imp, lines })
      if (refs.length >= 200) break
    }
  }
  const settings = {
    project: readJson(path.join(root, '.claude', 'settings.json')),
    local: readJson(path.join(root, '.claude', 'settings.local.json')),
    user: readJson(path.join(HOME, '.claude', 'settings.json')),
  }
  const mdDir = (dir, pick) => { try { return fs.readdirSync(dir).sort().slice(0, 50).map(pick).filter(Boolean) } catch { return [] } }
  const agents = mdDir(path.join(root, '.claude', 'agents'), f => {
    if (!f.endsWith('.md')) return null
    const r = readCapped(path.join(root, '.claude', 'agents', f), 20e3); return r && { file: f, fm: frontmatter(r.text).fm }
  })
  const skills = mdDir(path.join(root, '.claude', 'skills'), d => {
    const r = readCapped(path.join(root, '.claude', 'skills', d, 'SKILL.md'), 40e3); if (!r) return null
    const { fm, body } = frontmatter(r.text); return { dir: d, fm, body: body.slice(0, 3000) }
  })
  let rootNames = []; try { rootNames = fs.readdirSync(root) } catch {}
  const sensitive = {
    env: rootNames.filter(n => /^\.env(\.|$)/.test(n) && !/\.(example|sample|template|dist)$/.test(n)),
    secretsDir: rootNames.includes('secrets'),
    pem: rootNames.filter(n => n.endsWith('.pem')),
  }
  const CODE = /\.(m?[jt]sx?|py|go|rs|rb|java|kt|swift|c|cc|cpp|h|cs|php|sh|lua|dart|ex|exs)$/
  const MANIFESTS = ['package.json', 'pyproject.toml', 'go.mod', 'Cargo.toml', 'pom.xml', 'build.gradle', 'Gemfile', 'composer.json', 'Package.swift', 'Makefile', 'requirements.txt']
  const sub = ['src', 'lib', 'app', 'server', 'web', 'cmd'].flatMap(d => { try { return fs.readdirSync(path.join(root, d)) } catch { return [] } })
  const hasCode = rootNames.some(n => MANIFESTS.includes(n) || CODE.test(n)) || sub.some(n => CODE.test(n))
  // auto-memory index for this project
  let memory = null
  for (const d of [cwd, root]) {
    const f = path.join(HOME, '.claude', 'projects', d.replace(/[^A-Za-z0-9]/g, '-'), 'memory', 'MEMORY.md')
    const r = readCapped(f, 2e6)
    if (r) { memory = { path: tilde(f), bytes: r.size, lines: r.text.split('\n').length }; break }
  }
  // auto compactions in past sessions of this project
  let auto = 0, sessions = 0
  for (const f of fs.readdirSync(DIR).filter(f => f.endsWith('.jsonl'))) {
    try {
      const v = summary(path.join(DIR, f)); if (!v.cwd) continue
      let c; try { c = fs.realpathSync(v.cwd) } catch { continue }
      if (c !== cwd && c !== root) continue
      sessions++; auto += v.compactions.filter(x => /auto/i.test(x.trigger || '')).length
    } catch {}
  }
  return { cwd: tilde(cwd), repoRoot: tilde(root), hasCode, claudeMd, refs, settings, agents, skills, sensitive, memory, compactions: { auto, sessions } }
}

// ── ingest socket ──
try { fs.unlinkSync(SOCK) } catch {}
const ingestServer = http.createServer((req, res) => {
  let body = ''
  req.on('data', c => (body += c))
  req.on('end', () => {
    try {
      const ops = JSON.parse(body)
      if (!Array.isArray(ops) && ops.op === 'bye') { res.end('bye'); return setTimeout(bye, 50) }
      for (const op of Array.isArray(ops) ? ops : [ops]) ingest(op)
      res.end('ok')
    } catch (err) { res.statusCode = 400; res.end(String(err)) }
  })
})
ingestServer.listen(SOCK, () => fs.chmodSync(SOCK, 0o600))

// ── web, read only ──
const json = (res, v) => { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(v)) }
const webServer = http.createServer((req, res) => {
  // DNS-rebinding guard: only answer to a loopback Host
  const host = (req.headers.host || '').replace(/:\d+$/, '')
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(host)) { res.statusCode = 403; return res.end('forbidden') }
  if (req.method !== 'GET') { res.statusCode = 405; return res.end() }
  const url = new URL(req.url, 'http://x')
  if (url.pathname === '/') {
    res.setHeader('content-type', 'text/html; charset=utf-8')
    res.setHeader('cache-control', 'no-store')
    return fs.createReadStream(PAGE).on('error', () => { res.statusCode = 500; res.end('web/index.html missing') }).pipe(res)
  }
  if (url.pathname === '/api/stream') {
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' })
    res.write(`event: snapshot\ndata: ${JSON.stringify({ session: SID, meta: live.meta, events: live.events, sigs: live.sigs.slice(-8000), series: live.series.slice(-3600) })}\n\n`)
    clients.add(res)
    const ping = setInterval(() => res.write(': ping\n\n'), 15000)
    req.on('close', () => { clients.delete(res); clearInterval(ping) })
    return
  }
  if (url.pathname === '/api/sessions') {
    const files = fs.readdirSync(DIR).filter(f => f.endsWith('.jsonl')).map(f => path.join(DIR, f))
    const list = files.map(f => { try { return sessionOut(summary(f)) } catch { return null } }).filter(Boolean)
    list.sort((a, b) => (b.start || 0) - (a.start || 0))
    return json(res, { current: SID, sessions: list })
  }
  if (url.pathname === '/api/setup') {
    const want = url.searchParams.get('cwd') || live.meta.cwd
    const cwd = cwdAllowed(want)
    if (!cwd) { res.statusCode = 403; return json(res, { error: 'cwd is not the session cwd or one seen in a session log' }) }
    try { return json(res, setupSnapshot(cwd)) } catch (err) { res.statusCode = 500; return json(res, { error: String(err.message || err) }) }
  }
  const script = url.pathname.match(/^\/(coach|classify|setup-checks)\.js$/)
  if (script) {
    res.setHeader('content-type', 'text/javascript; charset=utf-8')
    res.setHeader('cache-control', 'no-store')
    return fs.createReadStream(path.join(HERE, '..', 'web', `${script[1]}.js`)).on('error', () => { res.statusCode = 404; res.end() }).pipe(res)
  }
  // past sessions from Claude Code's transcripts: indexed in the background on first ask
  if (url.pathname === '/api/turns') {
    if (url.searchParams.get('refresh') === '1' || transcripts.status().status === 'idle') void transcripts.build()
    return json(res, transcripts.list())
  }
  const turn = url.pathname.match(/^\/api\/turn\/(\d+)$/)
  if (turn) { const d = transcripts.detail(Number(turn[1])); if (!d) { res.statusCode = 404; return res.end() } return json(res, d) }
  const view = url.pathname.match(/^\/views\/(\w+)\.dc\.html$/)
  if (view) {
    res.setHeader('content-type', 'text/html; charset=utf-8')
    res.setHeader('cache-control', 'no-store')
    return fs.createReadStream(path.join(HERE, '..', 'web', 'views', `${view[1]}.dc.html`)).on('error', () => { res.statusCode = 404; res.end() }).pipe(res)
  }
  const m = url.pathname.match(/^\/api\/session\/([\w.-]+)$/)
  if (m) {
    const s = replay(readLog(path.join(DIR, `${m[1]}.jsonl`)))
    return json(res, { session: m[1], meta: s.meta, events: s.events, sigs: s.sigs.slice(-8000), series: s.series.slice(-3600) })
  }
  res.statusCode = 404; res.end()
})

function listen(port) {
  webServer.once('error', err => {
    if (err.code === 'EADDRINUSE' && port < FIRST_PORT + 50) listen(port + 1)
    else { console.error(String(err)); process.exit(1) }
  })
  webServer.listen(port, '127.0.0.1', () => {
    // the mod reads this one line
    process.stdout.write(`${JSON.stringify({ ready: true, port, sock: SOCK, log: LOG })}\n`)
    try { fs.writeFileSync(RECORD, JSON.stringify({ pid: process.pid, sock: SOCK, port })) } catch {}
  })
}
listen(FIRST_PORT)

const bye = () => {
  try { fs.unlinkSync(SOCK) } catch {}
  try { if (JSON.parse(fs.readFileSync(RECORD, 'utf8')).pid === process.pid) fs.unlinkSync(RECORD) } catch {}
  process.exit(0)
}
process.on('SIGTERM', bye)
process.on('SIGINT', bye)
// the mod kills us on unload; if Claude Code itself dies we are re-parented, so leave too
const parent = process.ppid
setInterval(() => { if (process.ppid !== parent) bye() }, 5000).unref()
