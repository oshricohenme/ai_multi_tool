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
  const script = url.pathname.match(/^\/(coach|classify)\.js$/)
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
