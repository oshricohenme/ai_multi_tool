// Other coding agents' logs, read as records shaped like Claude Code's, so the accounting rules in account.mjs
// apply to them unchanged. Each *Records function is pure (a parsed log in, records out); others() finds the
// sessions on disk and recordsOf() reads one. Like the rest of the statement, this only reads.
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import { createRequire } from 'node:module'

// Claude-shaped usage. Every agent's figures arrive as new input (not cached), cache read, cache write,
// output with thinking included, and thinking.
const usage = (inp, cr, cw, out, think) => ({
  input_tokens: Math.max(0, inp || 0), cache_read_input_tokens: cr || 0, cache_creation_input_tokens: cw || 0,
  output_tokens: out || 0, output_tokens_details: { thinking_tokens: think || 0 },
})
const textOf = c => (typeof c === 'string' ? c : Array.isArray(c) ? c.map(b => (typeof b === 'string' ? b : b.text || '')).filter(Boolean).join('\n') : '')
// Instructions and environment notes that agents send as a user message, not typed by the person
const META = /^\s*(<|# AGENTS\.md|\[Function Response)/
const prompt = (t, timestamp, cwd, n) => ({ type: 'user', _n: n, timestamp, cwd, isMeta: META.test(t), message: { role: 'user', content: t } })
const sent = (type, text, timestamp, n, extra) => ({ type: 'attachment', _n: n, timestamp, attachment: { type, ...extra }, rendered: [{ content: text }] })
const result = (id, content, isError) => ({ type: 'tool_result', tool_use_id: id, content, is_error: !!isError })

// ---- Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl ------------------------------------

// recs: the parsed lines of one rollout file, each with _n. OpenAI counts cached tokens inside input_tokens
// and thinking inside output_tokens.
export function codexRecords(recs) {
  const out = []
  const perResponse = recs.some(r => r.type === 'token_usage_record') // newer Codex: one record per response, before its outputs
  let model = '', cwd = '', blocks = [], lastTotal = null
  for (const r of recs) {
    const p = r.payload || {}, ts = r.timestamp, n = r._n
    if (r.type === 'session_meta') {
      cwd = p.cwd || cwd
      if (p.agent_nickname) out.push({ type: 'custom-title', customTitle: 'Subagent ' + p.agent_nickname + (p.agent_path ? ' · ' + p.agent_path : '') })
      const sys = p.base_instructions && p.base_instructions.text
      if (sys) out.push(sent('system_prompt', sys, ts, n))
      continue
    }
    if (r.type === 'turn_context') { model = p.model || model; cwd = p.cwd || cwd; continue }
    if (r.type === 'compacted') {
      out.push({ type: 'system', subtype: 'compact_boundary', _n: n, timestamp: ts, compactMetadata: { trigger: 'auto' } })
      if (p.message) out.push({ type: 'user', _n: n, timestamp: ts, isCompactSummary: true, message: { content: p.message } })
      continue
    }
    let u = null, id = null
    if (r.type === 'token_usage_record' && perResponse) (u = p.usage), (id = p.response_id)
    else if (r.type === 'event_msg' && p.type === 'token_count' && !perResponse && p.info && p.info.last_token_usage) {
      // ponytail: older Codex reports usage after the tool outputs, so a result can be named as the cause of the
      // request that asked for it. Totals are exact either way.
      const total = p.info.total_token_usage && p.info.total_token_usage.total_tokens
      if (total != null && total === lastTotal) continue // the same count, reported again
      lastTotal = total
      u = p.info.last_token_usage
    }
    if (u) {
      const cr = u.cached_input_tokens || 0, cw = u.cache_write_input_tokens || 0
      out.push({ type: 'assistant', _n: n, timestamp: ts, cwd, message: { id: id || 'codex-' + n, model, content: blocks, usage: usage((u.input_tokens || 0) - cr - cw, cr, cw, u.output_tokens, u.reasoning_output_tokens) } })
      blocks = []
      continue
    }
    if (r.type !== 'response_item') continue
    if (p.type === 'message') {
      const t = textOf(p.content)
      if (p.role === 'assistant') blocks.push({ type: 'text', text: t })
      else if (p.role === 'user') out.push(prompt(t, ts, cwd, n))
      else if (t) out.push(sent('developer_message', t, ts, n))
    } else if (p.type === 'agent_message') out.push(prompt(textOf(p.content), ts, cwd, n))
    else if (p.type === 'additional_tools') {
      const names = []
      const walk = list => (list || []).forEach(t => (t.tools ? walk(t.tools) : names.push(t.name)))
      walk(p.tools)
      out.push(sent('deferred_tools_delta', JSON.stringify(p.tools), ts, n, { addedNames: names }))
    } else if (p.type === 'reasoning') blocks.push({ type: 'thinking', thinking: (p.summary || []).map(s => s.text || '').join('\n') })
    else if (p.type === 'function_call') {
      let input
      try { input = JSON.parse(p.arguments) } catch { input = { input: p.arguments } }
      blocks.push({ type: 'tool_use', id: p.call_id, name: p.name, input })
    } else if (/_call$/.test(p.type)) blocks.push({ type: 'tool_use', id: p.call_id || p.id, name: p.name || p.type.slice(0, -5), input: p.input != null ? { input: p.input } : p.action || {} })
    else if (/_call_output$/.test(p.type)) {
      const o = p.output
      const content = Array.isArray(o) ? o.map(b => (b.type === 'input_image' ? { type: 'image', source: { media_type: 'image' } } : { type: 'text', text: b.text || '' })) : typeof o === 'string' ? o : JSON.stringify(o)
      out.push({ type: 'user', _n: n, timestamp: ts, cwd, message: { role: 'user', content: [result(p.call_id, content)] } })
    }
  }
  return out
}

// ---- opencode: ~/.local/share/opencode/opencode.db ------------------------------------------

// rows: { md: message data, pd: part data, pid: part id, t: part time } of one session, in order.
// A request is one step (step-finish part). opencode keeps cached tokens out of input and thinking out of output.
export function opencodeRecords(rows, title) {
  const out = title ? [{ type: 'custom-title', customTitle: title }] : []
  let blocks = [], results = [], n = 0
  for (const { md, pd, pid, t } of rows) {
    n++
    const ts = new Date(t || (md.time && md.time.created) || 0).toISOString(), cwd = md.path && md.path.cwd
    if (pd.type === 'compaction') out.push({ type: 'system', subtype: 'compact_boundary', _n: n, timestamp: ts, compactMetadata: { trigger: pd.auto ? 'auto' : 'manual' } })
    else if (md.role === 'user') {
      if (pd.type === 'text' && !pd.ignored) out.push({ ...prompt(pd.text || '', ts, cwd, n), isMeta: !!pd.synthetic || META.test(pd.text || '') })
    } else if (pd.type === 'text') blocks.push({ type: 'text', text: pd.text || '' })
    else if (pd.type === 'reasoning') blocks.push({ type: 'thinking', thinking: pd.text || '' })
    else if (pd.type === 'tool') {
      const s = pd.state || {}
      blocks.push({ type: 'tool_use', id: pd.callID, name: pd.tool, input: s.input || {} })
      if (s.status === 'completed' || s.status === 'error') results.push(result(pd.callID, s.output ?? s.error ?? '', s.status === 'error'))
    } else if (pd.type === 'step-finish' && pd.tokens) {
      const k = pd.tokens, c = k.cache || {}
      out.push({ type: 'assistant', _n: n, timestamp: ts, cwd, message: { id: pid, model: String(md.modelID || '').split('/').pop(), content: blocks, usage: usage(k.input, c.read, c.write, (k.output || 0) + (k.reasoning || 0), k.reasoning) } })
      blocks = []
      if (results.length) out.push({ type: 'user', _n: n, timestamp: ts, cwd, message: { role: 'user', content: results } }), (results = [])
    }
  }
  return out
}

// node:sqlite is in Node 22.13 and later. Without it, opencode's sessions are not read.
function openDb(file) {
  const warn = process.emitWarning
  process.emitWarning = () => {} // node:sqlite announces itself as experimental on every run
  try { return new (createRequire(import.meta.url)('node:sqlite').DatabaseSync)(file, { readOnly: true }) } catch { return null } finally { process.emitWarning = warn }
}
function query(file, sql, ...args) {
  const db = openDb(file)
  if (!db) return []
  try { return db.prepare(sql).all(...args) } catch { return [] } finally { db.close() }
}

// ---- Gemini CLI: ~/.gemini/tmp/<project>/chats/session-*.json -------------------------------

// chat: one parsed chat file. Gemini counts cached tokens inside input, and thinking apart from output.
export function geminiRecords(chat, cwd = '') {
  const out = []
  let n = 0
  for (const m of chat.messages || []) {
    n++
    const ts = m.timestamp
    if (m.type === 'user') {
      const t = textOf(m.content), fr = /^\[Function Response: ([^\]]+)\]/.exec(t)
      if (!fr) { out.push(prompt(t, ts, cwd, n)); continue }
      // older chats record a tool's output as a user message: name the tool for it
      out.push({ type: 'assistant', _n: n, timestamp: ts, message: { content: [{ type: 'tool_use', id: 'g' + n, name: fr[1], input: {} }] } })
      out.push({ type: 'user', _n: n, timestamp: ts, cwd, message: { role: 'user', content: [result('g' + n, t)] } })
    } else if (m.type === 'gemini' && m.tokens) {
      const k = m.tokens, calls = m.toolCalls || []
      const content = [
        ...(m.thoughts || []).map(x => ({ type: 'thinking', thinking: [x.subject, x.description].filter(Boolean).join(': ') })),
        { type: 'text', text: textOf(m.content) },
        ...calls.map(c => ({ type: 'tool_use', id: c.id, name: c.name, input: c.args || {} })),
      ]
      out.push({ type: 'assistant', _n: n, timestamp: ts, cwd, message: { id: m.id || 'gemini-' + n, model: m.model || '', content, usage: usage((k.input || 0) - (k.cached || 0), k.cached, 0, (k.output || 0) + (k.thoughts || 0), k.thoughts) } })
      if (calls.length) out.push({ type: 'user', _n: n, timestamp: ts, cwd, message: { role: 'user', content: calls.map(c => result(c.id, c.resultDisplay ?? JSON.stringify(c.result ?? ''), c.status === 'error')) } })
    }
  }
  return out
}

// ---- finding and reading sessions -----------------------------------------------------------

const files = (dir, test) => {
  try { return fs.readdirSync(dir, { recursive: true }).filter(f => test(String(f))).map(f => path.join(dir, String(f))) } catch { return [] }
}
const ID = /^[0-9A-Za-z_-]{1,80}$/
const UUID = /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/

// Every session of another agent on this machine: { id, agent, path } for a log file, { id, agent, db, stamp } for
// a database row (stamp: when it last changed).
export function others(home = os.homedir()) {
  const out = []
  const codex = process.env.CODEX_HOME || path.join(home, '.codex')
  for (const dir of ['sessions', 'archived_sessions'])
    for (const f of files(path.join(codex, dir), f => UUID.test(f))) out.push({ id: UUID.exec(f)[1], agent: 'Codex', path: f })

  const data = path.join(process.env.XDG_DATA_HOME || path.join(home, '.local', 'share'), 'opencode')
  for (const db of files(data, f => /^opencode[\w.-]*\.db$/.test(f)))
    for (const s of query(db, 'select id, directory, title, time_updated from session'))
      if (ID.test(s.id)) out.push({ id: s.id, agent: 'opencode', db, stamp: s.time_updated, cwd: s.directory, title: s.title })

  for (const f of files(path.join(home, '.gemini', 'tmp'), f => /(^|\/)chats\/session-[\w-]+\.json$/.test(f))) {
    const m = /session-([\w-]+)-([0-9a-f]{8})\.json$/.exec(f) // session-<date>-<short id>: the id first, so it reads at a glance
    const id = m ? m[2] + '-' + m[1] : path.basename(f, '.json')
    if (ID.test(id)) out.push({ id, agent: 'Gemini CLI', path: f })
  }
  return out
}

// The Claude-shaped records of one session from others(). readLog: statement.mjs's JSONL reader.
export async function recordsOf(s, readLog) {
  if (s.agent === 'Codex') return codexRecords(await readLog(s.path))
  if (s.agent === 'opencode') {
    const rows = query(s.db, 'select m.data md, p.data pd, p.id pid, p.time_created t from part p join message m on m.id = p.message_id where p.session_id = ? order by m.time_created, m.id, p.time_created, p.id', s.id)
    const parse = x => { try { return JSON.parse(x) } catch { return {} } }
    return opencodeRecords(rows.map(r => ({ md: parse(r.md), pd: parse(r.pd), pid: r.pid, t: r.t })).map(r => ({ ...r, md: { path: { cwd: s.cwd }, ...r.md } })), s.title)
  }
  if (s.agent === 'Gemini CLI') {
    let root = ''
    try { root = fs.readFileSync(path.join(path.dirname(path.dirname(s.path)), '.project_root'), 'utf8').trim() } catch {}
    return geminiRecords(JSON.parse(fs.readFileSync(s.path, 'utf8')), root)
  }
  return readLog(s.path)
}
