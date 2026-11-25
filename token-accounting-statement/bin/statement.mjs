#!/usr/bin/env node
// Token Accounting Statement: reads Claude Code's session logs and prints a statement of token use.
// The design is docs/TOKEN-ACCOUNTING-STATEMENT.md. Plain Node, no dependencies. It reads history only, changes no
// session, and sends nothing off this machine.
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import http from 'node:http'
import readline from 'node:readline'
import crypto from 'node:crypto'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { sessionOf, rowsOf, periodOf, rowUse, amountOf, num, plural } from './account.mjs'
import { sessionHtml, periodHtml, sessionText, periodText, summaryText, sessionBrief, rangeText, usd, CSP, LIVE_CSP } from './render.mjs'

const SELF = fileURLToPath(import.meta.url)
export const RATES = JSON.parse(fs.readFileSync(path.join(path.dirname(SELF), '..', 'rates.json'), 'utf8'))
// The statement's own records: derived figures per session, and the running server's address.
const KEEP = process.env.TOKEN_ACCOUNTING_HOME || path.join(os.homedir(), '.claude', 'token-accounting')
const CACHE = path.join(KEEP, 'cache'), SERVER_FILE = path.join(KEEP, 'server.json')
const CACHE_V = 1
const ID = /^[0-9A-Za-z_-]{1,80}$/

function writePrivate(file, text) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 })
  const tmp = file + '.' + process.pid + '.tmp'
  fs.writeFileSync(tmp, text, { mode: 0o600 }) // statements and figures are as private as the logs
  fs.renameSync(tmp, file)
}

// ---- reading logs ---------------------------------------------------------------------------

// Bookkeeping records the statement never uses, skipped without parsing. They put their type first.
const SKIP = new Set(['api-request-blob', 'file-history-snapshot', 'file-history-delta', 'queue-operation', 'last-prompt', 'mode', 'permission-mode', 'session-owner', 'bridge-session', 'atis-latch', 'agent-name', 'tools-used', 'history-suppression'])
export async function readLog(file) {
  const recs = []
  let n = 0
  for await (const line of readline.createInterface({ input: fs.createReadStream(file), crlfDelay: Infinity })) {
    n++
    if (line.startsWith('{"type":"') && SKIP.has(line.slice(9, line.indexOf('"', 9)))) continue
    let r
    try { r = JSON.parse(line) } catch { continue } // a half-written last line, say
    if (r && typeof r === 'object') (r._n = n), recs.push(r)
  }
  return recs
}

function rootsOf(o) {
  if (o.root) return [o.root]
  const out = new Set()
  for (const r of [process.env.CLAUDE_CONFIG_DIR && path.join(process.env.CLAUDE_CONFIG_DIR, 'projects'), path.join(os.homedir(), '.claude', 'projects')])
    try { if (r) out.add(fs.realpathSync(r)) } catch {}
  return [...out]
}

// Every session on disk: id -> { id, path: its log, dir: the folder holding its subagent logs }.
// A session whose own log is gone but whose subagent logs remain is still listed.
function listSessions(roots) {
  const map = new Map()
  const at = id => map.get(id) || map.set(id, { id }).get(id)
  for (const root of roots) {
    let projects
    try { projects = fs.readdirSync(root) } catch { continue }
    for (const p of projects) {
      let names
      try { names = fs.readdirSync(path.join(root, p), { withFileTypes: true }) } catch { continue }
      for (const f of names) {
        if (f.isFile() && f.name.endsWith('.jsonl') && ID.test(f.name.slice(0, -6))) at(f.name.slice(0, -6)).path = path.join(root, p, f.name)
        else if (f.isDirectory() && ID.test(f.name) && fs.existsSync(path.join(root, p, f.name, 'subagents'))) at(f.name).dir = path.join(root, p, f.name)
      }
    }
  }
  return map
}

function agentFiles(s) {
  if (!s.dir) return []
  try {
    return fs.readdirSync(path.join(s.dir, 'subagents'), { recursive: true }).filter(f => /^agent-.+\.jsonl$/.test(path.basename(f))).sort().map(f => path.join(s.dir, 'subagents', f))
  } catch { return [] }
}

async function loadSession(s, detail) {
  const main = s.path ? await readLog(s.path) : []
  const subs = []
  for (const f of agentFiles(s)) {
    let meta = null
    try { meta = JSON.parse(fs.readFileSync(f.replace(/\.jsonl$/, '.meta.json'), 'utf8')) } catch {}
    subs.push({ agentId: path.basename(f).slice(6, -6), meta, recs: await readLog(f) })
  }
  const started = x => (x.recs.find(r => r.timestamp) || {}).timestamp || ''
  subs.sort((a, b) => (started(a) < started(b) ? -1 : 1))
  return sessionOf(main, subs, { id: s.id, rates: RATES, detail })
}

// The figures of one session, from the statement's record when its logs have not changed.
async function rowsFor(s, stats) {
  const sig = crypto.createHash('sha1').update(stats.map(x => x.f + ':' + x.size + ':' + x.mtimeMs).join('|')).digest('hex')
  const file = path.join(CACHE, s.id + '.json')
  try {
    const c = JSON.parse(fs.readFileSync(file, 'utf8'))
    if (c.v === CACHE_V && c.sig === sig) return c.data
  } catch {}
  const data = rowsOf(await loadSession(s, false))
  writePrivate(file, JSON.stringify({ v: CACHE_V, sig, data }))
  return data
}

// Sessions that can hold a request at or after "from". A log's modified time only rules files out;
// each request's own timestamp decides which period it belongs to.
async function sessionsSince(roots, from) {
  const index = listSessions(roots), out = []
  for (const s of index.values()) {
    const stats = [s.path, ...agentFiles(s)].filter(Boolean).map(f => ({ f, ...fs.statSync(f) }))
    if (stats.length && Math.max(...stats.map(x => x.mtimeMs)) >= from) out.push(await rowsFor(s, stats))
  }
  // Claude Code deletes old logs. What the statement already recorded about them stays on the books.
  // ponytail: reads every kept record each time; file them by month if this ever gets slow.
  let kept = []
  try { kept = fs.readdirSync(CACHE) } catch {}
  for (const f of kept) {
    if (!f.endsWith('.json') || index.has(f.slice(0, -5))) continue
    try {
      const c = JSON.parse(fs.readFileSync(path.join(CACHE, f), 'utf8'))
      if (c.v === CACHE_V) out.push({ ...c.data, gone: true })
    } catch {}
  }
  return out
}

function findSession(roots, arg) {
  if (arg.endsWith('.jsonl')) {
    if (fs.existsSync(arg)) {
      const file = path.resolve(arg), dir = file.slice(0, -6)
      return { id: path.basename(dir), path: file, dir: fs.existsSync(path.join(dir, 'subagents')) ? dir : undefined }
    }
    // Claude Code can report the log under ~/.claude while CLAUDE_CONFIG_DIR keeps it elsewhere: find it by its id
    arg = path.basename(arg, '.jsonl')
  }
  const index = listSessions(roots)
  const hits = index.has(arg) ? [index.get(arg)] : [...index.values()].filter(s => s.id.startsWith(arg))
  if (hits.length !== 1) throw new Error(hits.length ? 'More than one session starts with ' + arg : 'No session found for ' + arg)
  return hits[0]
}

// ---- periods --------------------------------------------------------------------------------

function rangeOf(o) {
  const now = Date.now()
  if (o.month) {
    const d = new Date()
    const m = o.month === 'current' ? d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') : o.month
    if (!/^\d{4}-(0[1-9]|1[0-2])$/.test(m)) throw new Error('--month takes a month like 2026-11')
    const [y, mo] = m.split('-').map(Number)
    const to = new Date(y, mo, 1).getTime()
    return { from: new Date(y, mo - 1, 1).getTime(), to, end: Math.min(to - 1, now), statement: m }
  }
  const since = o.since || '30d'
  if (!/^\d{1,4}d?$/.test(since)) throw new Error('--since takes a number of days like 7d')
  const days = parseInt(since, 10)
  return { from: now - days * 864e5, to: now + 1, end: now, statement: 'Last ' + plural(days, 'day') }
}

async function periodFor(o) {
  const r = rangeOf(o)
  const P = periodOf(await sessionsSince(rootsOf(o), r.from), { from: r.from, to: r.to, project: o.project, tz: o.tz }, RATES)
  return { P, sessions: null, view: { tz: o.tz, lede: rangeText(r.from, r.end, o.tz), statement: r.statement, prepared: Date.now(), rates: RATES } }
}

// One line per request, for a spreadsheet. sessions: rowsOf() results.
function csvOf(sessions, from = -Infinity, to = Infinity) {
  const cell = v => {
    if (typeof v === 'number') return String(v)
    v = v == null ? '' : String(v)
    if (/^[=+\-@\t\r]/.test(v)) v = "'" + v // never let a spreadsheet read a label as a formula
    return /[",\n\r]/.test(v) ? '"' + v.replace(/"/g, '""') + '"' : v
  }
  const out = ['session,project,thread,time,model,new_input,cache_write_5m,cache_write_1h,cache_read,output,thinking,web_searches,context_after,amount_usd,skill,plugin,mcp_server,rewrite_reason,rewrite_tokens']
  for (const s of sessions)
    for (const r of s.rows) {
      if (r.t < from || r.t >= to) continue
      const a = amountOf(RATES, rowUse(r))
      out.push([s.id, s.project, r.k === -1 ? 'main' : (s.agents && s.agents[r.k]) || 'agent', new Date(r.t).toISOString(), r.m, r.i, r.w5, r.w1, r.r, r.o, r.th || 0, r.web || 0, r.c,
        a ? (a.all / 1e9).toFixed(6) : '', r.sk, r.pl, r.mcp, r.f ? r.f[0] : '', r.f ? r.f[1] : ''].map(cell).join(','))
    }
  return out.join('\n') + '\n'
}

// ---- the server (docs/TOKEN-ACCOUNTING-STATEMENT.md, "Safety of the server and pages") --------------------------

const TEXT = 'text/plain; charset=utf-8', HTML = 'text/html; charset=utf-8'
const send = (res, code, type, body, head = {}) => {
  res.writeHead(code, {
    'Content-Type': type, 'Content-Security-Policy': (head.csp || CSP) + "; frame-ancestors 'none'", 'Referrer-Policy': 'no-referrer',
    'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store', ...(head.tag ? { ETag: head.tag } : {}),
  })
  res.end(body)
}

// Resolves once it is listening, to { url, port, token, close }.
export function serve(o = {}) {
  const token = crypto.randomBytes(24).toString('base64url') // new at every start; no token, no page
  const idleMs = (Number(o.idle) || 30) * 60000
  const roots = rootsOf(o)
  let timer
  const server = http.createServer((req, res) => {
    clearTimeout(timer)
    timer = setTimeout(close, idleMs) // stops itself when nobody is reading
    handle(req, res).catch(err => send(res, 500, TEXT, 'The statement could not make this page: ' + err.message))
  })
  const close = () => {
    clearTimeout(timer)
    try { if (JSON.parse(fs.readFileSync(SERVER_FILE, 'utf8')).pid === process.pid) fs.unlinkSync(SERVER_FILE) } catch {}
    server.close()
    server.closeAllConnections()
  }
  async function handle(req, res) {
    const port = server.address().port
    if (req.method !== 'GET') return send(res, 405, TEXT, 'This server only answers GET.')
    // a page on another site cannot reach us under its own name
    if (req.headers.host !== '127.0.0.1:' + port && req.headers.host !== 'localhost:' + port) return send(res, 403, TEXT, 'Wrong host.')
    const url = new URL(req.url, 'http://127.0.0.1')
    const parts = url.pathname.split('/')
    const given = Buffer.from(parts[1] || ''), want = Buffer.from(token)
    if (given.length !== want.length || !crypto.timingSafeEqual(given, want)) return send(res, 404, TEXT, 'Not found.')
    const route = parts.slice(2).join('/')
    if (route === 'ping') return send(res, 200, TEXT, 'ok')
    if (route === '') {
      const q = { ...o, since: url.searchParams.get('since') || undefined, month: url.searchParams.get('month') || undefined, project: (url.searchParams.get('project') || '').slice(0, 200) || undefined }
      const { P, view } = await periodFor(q)
      return send(res, 200, HTML, periodHtml(P, { ...view, link: id => 's/' + id }))
    }
    const m = /^s\/([^/]+)$/.exec(route)
    // The id is looked up among the sessions found on disk. It is never joined into a path.
    const s = m && ID.test(m[1]) && listSessions(roots).get(m[1])
    if (!s) return send(res, 404, TEXT, m ? 'No log for this session is on disk. If it was removed, its totals still count in period statements.' : 'Not found.')
    // Live: the page asks again with the tag it holds, and gets 304 until one of the session's logs changes.
    const tag = '"' + crypto.createHash('sha1').update([s.path, ...agentFiles(s)].filter(Boolean).map(f => {
      try { const x = fs.statSync(f); return f + ':' + x.size + ':' + x.mtimeMs } catch { return f }
    }).join('|')).digest('hex').slice(0, 20) + '"'
    if (req.headers['if-none-match'] === tag) return res.writeHead(304, { ETag: tag, 'Cache-Control': 'no-store' }), res.end()
    return send(res, 200, HTML, sessionHtml(await loadSession(s, true), { tz: o.tz, asOf: RATES.asOf, live: tag, back: { href: '../', label: 'Period statement, last 30 days' } }), { csp: LIVE_CSP, tag })
  }
  return new Promise((resolve, reject) => {
    server.on('error', reject)
    server.listen(Number(o.port) || 0, '127.0.0.1', () => { // this machine only
      const port = server.address().port
      timer = setTimeout(close, idleMs)
      if (!o.quiet) writePrivate(SERVER_FILE, JSON.stringify({ port, token, pid: process.pid }))
      resolve({ url: 'http://127.0.0.1:' + port + '/' + token + '/', port, token, close })
    })
  })
}

async function running() {
  try {
    const s = JSON.parse(fs.readFileSync(SERVER_FILE, 'utf8'))
    const r = await fetch('http://127.0.0.1:' + s.port + '/' + s.token + '/ping', { signal: AbortSignal.timeout(1500) })
    if (r.ok) return 'http://127.0.0.1:' + s.port + '/' + s.token + '/'
  } catch {}
  return null
}

// The address of the running server; starts one in the background if there is none.
async function serverUrl(o) {
  let url = await running()
  if (url) return url
  spawn(process.execPath, [SELF, 'serve', ...(o.root ? ['--root', o.root] : []), ...(o.idle ? ['--idle', o.idle] : [])], { detached: true, stdio: 'ignore' }).unref()
  for (let i = 0; i < 100; i++) {
    await new Promise(r => setTimeout(r, 100))
    if ((url = await running())) return url
  }
  throw new Error('The statement server did not start.')
}

// ---- the command line -----------------------------------------------------------------------

const USAGE = [
  'Usage: statement.mjs [serve | open | index] [--session <id or log file> | --since 7d | --month 2026-11]',
  '                     [--project <name>] [--text | --html <file> | --json | --csv | --brief] [--no-detail]',
  '',
  '  (no command)  print a statement: one session with --session, otherwise a period (default: last 30 days)',
  '  open          show the statement in the browser, from a local server started if needed; prints a short summary',
  '  serve         run the local server in the foreground and print its address',
  '  index         bring the statement’s own record up to date and print nothing',
  '',
  '  --text is the default. --html <file> writes one self-contained page, readable by you only.',
  '  --no-detail leaves out message content. --csv gives one line per request. --no-open skips the browser.',
  '  --brief, with --session, prints the figures and the latest entries as JSON, for the pane in Claude Code.',
].join('\n')

function parse(argv) {
  const o = { cmd: '' }, flags = new Set(['text', 'json', 'csv', 'brief', 'no-detail', 'no-open', 'help', 'quiet'])
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    if (!a.startsWith('--')) {
      if (o.cmd || !['serve', 'open', 'index'].includes(a)) throw new Error('Unknown argument: ' + a + '\n' + USAGE)
      o.cmd = a
    } else if (flags.has(a.slice(2))) o[a.slice(2)] = true
    else if (['session', 'since', 'month', 'project', 'html', 'root', 'port', 'idle', 'tz'].includes(a.slice(2)) && i + 1 < argv.length) o[a.slice(2)] = argv[++i]
    else throw new Error('Unknown option: ' + a + '\n' + USAGE)
  }
  return o
}

const jsonOf = x => JSON.stringify(x, (k, v) => (v instanceof Map ? Object.fromEntries(v) : v instanceof Set ? [...v] : k === 'top' ? undefined : v), 1)
const say = s => process.stdout.write(s.endsWith('\n') ? s : s + '\n')

export async function main(argv) {
  const o = parse(argv)
  if (o.help) return say(USAGE)
  const roots = rootsOf(o)
  if (o.cmd === 'serve') {
    const s = await serve(o)
    return say(JSON.stringify({ url: s.url, pid: process.pid }))
  }
  if (o.cmd === 'index') return void (await sessionsSince(roots, rangeOf(o).from))
  const show = url => {
    if (!o['no-open'] && process.platform === 'darwin') spawn('open', [url], { detached: true, stdio: 'ignore' }).unref()
  }

  if (o.session) {
    const found = findSession(roots, o.session)
    const s = await loadSession(found, !(o['no-detail'] || o.text || o.csv || o.cmd === 'open' || !(o.html || o.json)))
    if (o.cmd === 'open') {
      const url = (await serverUrl(o)) + 's/' + found.id
      show(url)
      const m = s.main
      return say(summaryText('session ' + s.id.slice(0, 8) + ' · ' + s.title, s.totals, [
        'Context: opened at ' + num(m.opening) + ', peak ' + num(m.peak.ctx) + ', now ' + num(m.closing) + (s.subs.length ? ' · ' + plural(s.subTotals.n, 'request') + ' in ' + plural(s.subs.length, 'subagent') : ''),
      ], url))
    }
    if (o.html) return writePrivate(path.resolve(o.html), sessionHtml(s, { tz: o.tz, detail: !o['no-detail'], asOf: RATES.asOf })), say('Wrote ' + path.resolve(o.html))
    if (o.brief) return say(JSON.stringify(sessionBrief(s, { tz: o.tz })))
    if (o.json) return say(jsonOf(s))
    if (o.csv) return say(csvOf([rowsOf(s)]))
    return say(sessionText(s, { tz: o.tz }))
  }

  const r = rangeOf(o)
  if (o.csv) return say(csvOf((await sessionsSince(roots, r.from)).filter(s => !o.project || s.project === o.project), r.from, r.to))
  const { P, view } = await periodFor(o)
  if (o.cmd === 'open') {
    const q = new URLSearchParams(Object.entries({ since: o.since, month: o.month && r.statement, project: o.project }).filter(x => x[1]))
    const url = (await serverUrl(o)) + (String(q) ? '?' + q : '')
    show(url)
    const t = { n: P.n, amt: P.amt, in: P.classes.in.tok, cw: P.classes.cw.tok, cr: P.classes.cr.tok, out: P.classes.out.tok }
    return say(summaryText(view.lede + ' · ' + plural(P.sessions.length, 'session'), t, [], url))
  }
  if (o.html) return writePrivate(path.resolve(o.html), periodHtml(P, view)), say('Wrote ' + path.resolve(o.html))
  if (o.json) return say(jsonOf(P))
  return say(periodText(P, view))
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === SELF)
  main(process.argv.slice(2)).catch(err => {
    process.stderr.write(err.message + '\n')
    process.exit(1)
  })
