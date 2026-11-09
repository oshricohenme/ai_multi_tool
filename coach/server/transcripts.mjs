// Past sessions from Claude Code's own transcripts (~/.claude/projects/*/*.jsonl), read only.
// One turn per prompt: the prompt, every tool call (and whether it failed), the final reply.
// Extraction is cached per file (by mtime) under ~/.claude/dev-tools, so only new or changed
// transcripts are read again. Nothing leaves the machine.
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import readline from 'node:readline'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)
const CLASSIFY = require(path.join(HERE, '..', 'web', 'classify.js'))
const ROOT = process.env.DEVTOOLS_TRANSCRIPTS || path.join(os.homedir(), '.claude', 'projects')
const CACHE = path.join(process.env.DEVTOOLS_DIR ? path.dirname(process.env.DEVTOOLS_DIR) : path.join(os.homedir(), '.claude', 'dev-tools'), 'transcripts-cache.json')
const NOISE = /^\s*<(command-name|command-message|local-command-stdout|local-command-caveat|system-reminder|bash-input|bash-stdout|task-notification|user-prompt-submit-hook)/

const textOf = c => typeof c === 'string' ? c : Array.isArray(c) ? c.filter(b => b && b.type === 'text').map(b => b.text || '').join('\n') : ''
const argOf = inp => {
  if (!inp || typeof inp !== 'object') return ''
  for (const k of ['command', 'file_path', 'path', 'pattern', 'url', 'query', 'description', 'prompt', 'skill', 'notebook_path']) if (typeof inp[k] === 'string' && inp[k]) return inp[k].slice(0, 300)
  return JSON.stringify(inp).slice(0, 200)
}

async function extract(file) {
  const project = path.basename(path.dirname(file))
  const rows = []
  let cur = null
  const pending = new Map()
  const close = () => { if (cur && (cur.prompt || cur.actions.length)) rows.push(cur) }
  const start = (o, prompt, slash) => { close(); cur = { sid: o.sessionId, project, ts: Date.parse(o.timestamp) || 0, prompt: prompt.slice(0, 4000), slash, actions: [], reply: '', calls: 0, inTok: 0, outTok: 0, cwd: o.cwd } }
  const rl = readline.createInterface({ input: fs.createReadStream(file, { encoding: 'utf8' }), crlfDelay: Infinity })
  for await (const line of rl) {
    let o; try { o = JSON.parse(line) } catch { continue }
    if (o.isSidechain) continue // subagent internals: counted through the Agent call in the parent
    const m = o.message || {}
    if (o.type === 'user') {
      const c = m.content
      if (Array.isArray(c) && c.some(b => b && b.type === 'tool_result')) {
        for (const b of c) if (b && b.type === 'tool_result') {
          const a = pending.get(b.tool_use_id); if (!a) continue
          pending.delete(b.tool_use_id)
          const rt = typeof b.content === 'string' ? b.content : textOf(b.content)
          a.err = !!b.is_error; a.out = (rt || '').slice(0, 240); a.outLen = (rt || '').length
        }
        continue
      }
      const txt = textOf(c)
      if (o.isMeta || !txt.trim() || NOISE.test(txt)) {
        const mm = /<command-name>(\/[^<]+)<\/command-name>/.exec(txt || '')
        if (mm) { const args = /<command-args>([\s\S]*?)<\/command-args>/.exec(txt); start(o, mm[1] + (args && args[1].trim() ? ' ' + args[1].trim() : ''), true) }
        continue
      }
      start(o, txt, false)
    } else if (o.type === 'assistant' && cur) {
      const u = m.usage || {}
      cur.calls++; cur.outTok += u.output_tokens || 0
      cur.inTok += (u.input_tokens || 0) + (u.cache_read_input_tokens || 0) + (u.cache_creation_input_tokens || 0)
      for (const b of m.content || []) {
        if (!b || typeof b !== 'object') continue
        if (b.type === 'text' && (b.text || '').trim()) cur.reply = b.text.slice(-3000)
        else if (b.type === 'tool_use') { const a = { tool: b.name, arg: argOf(b.input) }; cur.actions.push(a); pending.set(b.id, a) }
      }
    }
  }
  close()
  return rows
}

// ── the index: built once in the background, refreshed when asked ──
const state = { status: 'idle', done: 0, total: 0, files: {}, rows: [], classified: null, at: 0, error: null }
let cache = {}
try { cache = JSON.parse(fs.readFileSync(CACHE, 'utf8')) } catch {}

export async function build() {
  if (state.status === 'indexing') return
  state.status = 'indexing'; state.error = null
  try {
    const dirs = fs.existsSync(ROOT) ? fs.readdirSync(ROOT) : []
    const files = []
    for (const d of dirs) { let ls = []; try { ls = fs.readdirSync(path.join(ROOT, d)) } catch {} for (const f of ls) if (f.endsWith('.jsonl')) files.push(path.join(ROOT, d, f)) }
    state.total = files.length; state.done = 0
    const next = {}
    for (const f of files) {
      let st; try { st = fs.statSync(f) } catch { state.done++; continue }
      const hit = cache[f]
      next[f] = hit && hit.mtime === st.mtimeMs ? hit : { mtime: st.mtimeMs, turns: await extract(f).catch(() => []) }
      state.done++
    }
    cache = next
    try { fs.mkdirSync(path.dirname(CACHE), { recursive: true }); fs.writeFileSync(CACHE, JSON.stringify(cache)) } catch {}
    const rows = []
    Object.entries(cache).forEach(([file, v]) => v.turns.forEach((t, i) => rows.push({ ...t, file, i })))
    rows.sort((a, b) => a.ts - b.ts)
    state.rows = rows
    state.classified = CLASSIFY.classifyAll(rows)
    state.status = 'ready'; state.at = Date.now()
  } catch (err) { state.status = 'error'; state.error = String(err) }
}

const clip = (s, n) => { s = String(s || '').replace(/\s+/g, ' ').trim(); return s.length > n ? s.slice(0, n - 1) + '…' : s }
// a compact row per turn: enough to list, filter and chart; the detail endpoint has the rest
export function list() {
  if (state.status !== 'ready') return { status: state.status, done: state.done, total: state.total, error: state.error }
  const rows = state.rows.map((t, k) => {
    const c = state.classified[k]
    return {
      k, sid: t.sid, project: c.facts.project, ts: t.ts, prompt: clip(t.prompt, 220), reply: clip(String(t.reply).slice(-400), 180),
      source: c.source, sub: c.sub, failed: c.failed, intent: c.intent, intents: c.intents.map(x => [x.k, x.ev]),
      outcome: c.outcome, outcomeEv: c.outcomeEv, roll: c.roll, dominant: c.dominant, n: c.n, errs: c.errs,
      flags: Object.fromEntries(Object.entries(c.flags).filter(([, v]) => v)),
      facts: Object.fromEntries(Object.entries(c.facts).filter(([k, v]) => v && (!Array.isArray(v) || v.length) && !['read', 'ids'].includes(k))),
      inTok: t.inTok, outTok: t.outTok
    }
  })
  return { status: 'ready', at: state.at, total: state.total, rows }
}
export function detail(k) {
  const t = state.rows[k], c = state.classified && state.classified[k]
  if (!t || !c) return null
  return { turn: { prompt: t.prompt, reply: t.reply, actions: t.actions, sid: t.sid, project: t.project, ts: t.ts, cwd: t.cwd, file: t.file }, c }
}
export function status() { return { status: state.status, done: state.done, total: state.total, at: state.at } }
