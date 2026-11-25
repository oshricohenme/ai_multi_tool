// The accounting rules of docs/TOKEN-ACCOUNTING-STATEMENT.md. Pure: log records in, statement entries out.
// Nothing here reads a file or the clock, so the test needs no real logs.

export const CPT = 3.6 // characters per token: an estimate, used only to split a row's exact total
const CAP = 1.5 // no item is given more than one token per 1.5 characters; the rest is "not recorded"
const IMAGE_W = 5400 // an image has no characters: weigh it as about 1,500 tokens
const REWRITE_READ = 0.5 // rule 7 thresholds. Starting values: tune on real sessions.
const REWRITE_WRITE = 10000
const INJECTED = /^\s*(<(system-reminder|local-command|command-name|command-message|task-notification|bash-)|\[Request interrupted)/

export const num = x => Math.round(x).toLocaleString('en-US')
export const clip = (s, n) => (s.length > n ? s.slice(0, n - 1) + '…' : s)
export const plural = (n, word, many = word + 's') => num(n) + ' ' + (n === 1 ? word : many)
const oneLine = s => s.replace(/\s+/g, ' ').trim()
const base = p => String(p).split('/').pop()
const lower = s => (/^[A-Z][a-z]/.test(s) ? s[0].toLowerCase() + s.slice(1) : s)

// ---- rates (rule 9) -------------------------------------------------------------------------

// A model id matches a rate key exactly, or with a date or "latest" after it. A newer model whose
// name merely starts like an older one gets no rate: a dash is better than the wrong price.
export function rateFor(rates, model) {
  const m = String(model || '').replace(/\[.*\]$/, '')
  let best = null
  for (const k of Object.keys(rates.models))
    if ((m === k || /^-(\d{8}|latest)$/.test(m.slice(k.length))) && m.startsWith(k) && (!best || k.length > best.length)) best = k
  return best ? rates.models[best] : null
}

// USD per million tokens for one request, or null when the model has no rate.
export function priceOf(rates, model, promptTokens = 0, speed, geo) {
  const r = rateFor(rates, model)
  if (!r) return null
  let t = r.over && promptTokens > r.over.tokens ? { ...r, ...r.over } : r
  if (speed === 'fast') {
    if (!r.fast) return null // fast mode has its own price list: no entry, no amount
    t = { ...t, cr: t.cr != null ? (t.cr * r.fast.in) / t.in : undefined, in: r.fast.in, out: r.fast.out }
  }
  const k = (rates.geo && rates.geo[geo]) || 1 // data residency multiplier, on every token class
  const cr = t.cr != null ? t.cr : t.in * rates.cacheRead
  return { in: t.in * k, out: t.out * k, cr: cr * k, cw5: t.in * rates.cacheWrite5m * k, cw1: t.in * rates.cacheWrite1h * k }
}

// Amounts are whole nano-dollars (USD 1e-9), so that lines add up to their totals exactly.
export function amountOf(rates, u) {
  const p = priceOf(rates, u.model, u.in + u.cw5 + u.cw1 + u.cr, u.speed, u.geo)
  if (!p) return null
  const n = x => Math.round(x * 1000)
  const a = { in: n(u.in * p.in), cw: n(u.cw5 * p.cw5 + u.cw1 * p.cw1), cr: n(u.cr * p.cr), out: n(u.out * p.out) }
  a.web = n((u.web || 0) * (rates.webSearchPer1000 || 0) * 1000) // a fee per search, not per token
  a.all = a.in + a.cw + a.cr + a.out + a.web
  a.saved = n(u.cr * (p.in - p.cr)) // what reading from the cache saved against sending it as new input
  a.premium = n(u.cw5 * (p.cw5 - p.in) + u.cw1 * (p.cw1 - p.in)) // what storing it cost over new input
  return a
}

// What a re-write cost against a cache read of the same tokens, in nano-dollars.
export function rewriteCost(rates, u, again) {
  const p = priceOf(rates, u.model, u.in + u.cw5 + u.cw1 + u.cr, u.speed, u.geo)
  const cw = u.cw5 + u.cw1
  if (!p || !cw) return null
  const wrote = Math.round(((u.cw5 * p.cw5 + u.cw1 * p.cw1) / cw) * again * 1000)
  const read = Math.round(again * p.cr * 1000)
  return { wrote, read, extra: wrote - read }
}

export const usageOf = u => {
  const cw = u.cache_creation_input_tokens || 0
  const cw1 = Math.min(cw, (u.cache_creation && u.cache_creation.ephemeral_1h_input_tokens) || 0)
  return {
    in: u.input_tokens || 0, cw, cw1, cw5: cw - cw1, cr: u.cache_read_input_tokens || 0, out: u.output_tokens || 0,
    think: (u.output_tokens_details && u.output_tokens_details.thinking_tokens) || 0,
    web: (u.server_tool_use && u.server_tool_use.web_search_requests) || 0,
    speed: u.speed || 'standard', geo: u.inference_geo || 'global',
  }
}

// Integers that sum to total, in proportion to the weights (largest remainder).
export function split(total, ws) {
  const sum = ws.reduce((a, b) => a + b, 0)
  if (!sum || total <= 0) return ws.map(() => 0)
  const raw = ws.map(w => (total * w) / sum)
  const out = raw.map(Math.floor)
  const left = total - out.reduce((a, b) => a + b, 0)
  raw.map((x, i) => [x - out[i], i]).sort((a, b) => b[0] - a[0]).slice(0, left).forEach(([, i]) => out[i]++)
  return out
}

// ---- reading one record ---------------------------------------------------------------------

function contentOf(c) {
  if (typeof c === 'string') return { text: c, chars: c.length, w: c.length }
  if (!Array.isArray(c)) {
    const t = c == null ? '' : JSON.stringify(c)
    return { text: t, chars: t.length, w: t.length }
  }
  let chars = 0, w = 0
  const parts = []
  for (const b of c) {
    const t = typeof b === 'string' ? b : b.type === 'text' ? b.text || '' : null
    if (t != null) parts.push(t), (chars += t.length), (w += t.length)
    else if (b.type === 'image' || b.type === 'document') {
      const size = (b.source && b.source.data && b.source.data.length) || 0
      parts.push('[' + b.type + ' · ' + ((b.source && b.source.media_type) || 'binary') + ' · ' + num(size) + ' characters of base64, not shown]')
      w += IMAGE_W
    } else {
      const j = JSON.stringify(b)
      parts.push(j), (chars += j.length), (w += j.length)
    }
  }
  return { text: parts.join('\n'), chars, w }
}

function argOf(input, cwd) {
  if (!input || typeof input !== 'object') return ''
  const p = input.file_path ?? input.notebook_path
  if (typeof p === 'string') {
    const rel = cwd && p.startsWith(cwd + '/') ? p.slice(cwd.length + 1) : p
    return rel.length > 80 ? '…' + rel.slice(-79) : rel // the end of a path says more than its start
  }
  const v = input.command ?? input.pattern ?? input.url ?? input.query ?? input.skill ?? input.description ?? input.path ?? Object.values(input).find(x => typeof x === 'string')
  return clip(oneLine(String(v ?? '').split('\n')[0]), 80)
}

// What kind of thing an attachment is, and what to call it.
function attName(a) {
  const t = a.type || 'attachment'
  if (t.startsWith('hook_')) {
    const who = a.command ? base(a.command.split(' ')[0]) : a.hookName && a.hookName !== a.hookEvent ? a.hookName : ''
    return ['hook', 'Hook output', [a.hookEvent, who].filter(Boolean).join(' · '), { event: a.hookEvent || 'hook' }]
  }
  if (t === 'instructions' || t === 'nested_memory')
    return ['system', 'Project instructions', a.files ? plural(a.files.length, 'file') : a.path ? base(a.path) : '']
  if (t === 'queued_command') {
    const typed = !a.commandMode || a.commandMode === 'prompt'
    return typed ? ['prompt', 'Prompt', 'typed while the model was working'] : ['other', 'Task notification', a.commandMode]
  }
  const known = {
    skill_listing: ['Skill listing', a.skillCount ? plural(a.skillCount, 'skill') : ''],
    deferred_tools_delta: ['Tool listing', a.addedNames ? plural(a.addedNames.length, 'tool') + ' added' : ''],
    agent_listing_delta: ['Agent listing', a.addedTypes ? plural(a.addedTypes.length, 'agent type') : ''],
    mcp_instructions_delta: ['MCP server instructions', a.addedNames ? a.addedNames.slice(0, 4).join(', ') : ''],
    edited_text_file: ['File changed notice', a.filename ? base(a.filename) : ''],
    file: ['Attached file', a.displayPath || (a.filename ? base(a.filename) : '')],
    compact_file_reference: ['File reference', a.displayPath || ''],
  }[t]
  if (known) return ['other', known[0], known[1]]
  const name = t.replace(/_/g, ' ')
  return ['other', name[0].toUpperCase() + name.slice(1), '']
}

// Logs older than the "rendered" field: guess what was sent from the attachment itself.
function fallbackText(a) {
  const t = a.type || ''
  if (t === 'hook_additional_context') return [].concat(a.content || []).join('\n')
  if (t === 'hook_success') return /^(SessionStart|UserPromptSubmit)/.test(a.hookEvent || '') ? a.stdout || a.content || '' : ''
  if (t === 'instructions') return (a.files || []).map(f => f.content || '').join('\n')
  for (const k of ['content', 'text', 'prompt', 'snippet']) if (typeof a[k] === 'string') return a[k]
  return ''
}

// ---- one thread (rules 1 to 7) --------------------------------------------------------------

const zero = () => ({ n: 0, in: 0, cw: 0, cw5: 0, cw1: 0, cr: 0, out: 0, think: 0, web: 0, amt: 0, saved: 0, premium: 0, unpriced: 0 })
export const addTotals = (a, b) => {
  for (const k of Object.keys(b)) a[k] = (a[k] || 0) + b[k]
  return a
}

// recs: the records of one log file, in file order, each with _n (its line number).
// o.sub: a subagent log (every record belongs). o.detail === false: keep numbers and labels only.
// o.rates: the rates table; without it there are no amounts.
// o.seen: a Set of response ids already counted elsewhere in the session (rule 1).
export function accountOf(recs, o = {}) {
  const detail = o.detail !== false
  const hasRendered = recs.some(r => r.type === 'attachment' && r.rendered)
  const tools = new Map(), reqs = new Map(), shapes = new Map(), byId = new Map()
  const steps = []
  let pending = [], control = null, ai = '', custom = '', cwd = '', firstPrompt = '', comp = null, stash = null

  const item = (kind, name, label, content, r, extra) => {
    const c = contentOf(content)
    return { kind, name, label, chars: c.chars, w: c.w, tok: 0, line: r._n, ...(detail ? { text: c.text } : null), ...extra }
  }
  const prompt = (t, r) => {
    const inj = r.isMeta || INJECTED.test(t)
    const short = clip(oneLine(t), 60)
    if (inj) return item('other', 'Injected message', short, t, r)
    if (!firstPrompt) firstPrompt = clip(oneLine(t), 80)
    return item('prompt', o.sub ? 'Task brief' : 'Prompt', '“' + short + '”', t, r)
  }

  for (const r of recs) {
    if (r.type === 'api-request') { reqs.set(r.id, r); continue }
    if (r.type === 'api-request-shape') { shapes.set(r.shapeHash, r.shape); continue }
    if (r.type === 'cost-state') { control = r; continue }
    if (r.type === 'ai-title') { ai = r.aiTitle || ai; continue }
    if (r.type === 'custom-title') { custom = r.customTitle || custom; continue }
    if (!o.sub && r.isSidechain) continue
    if (r.cwd && !cwd) cwd = r.cwd

    if (r.type === 'assistant') {
      const m = r.message || {}, u = m.usage
      for (const b of m.content || [])
        if (b.type === 'tool_use') tools.set(b.id, { name: b.name, arg: argOf(b.input, cwd), agent: b.input && b.input.subagent_type })
      // rule 1: a record with no usage, or all-zero usage, is not a request
      if (!m.id || m.model === '<synthetic>' || !u || !(u.input_tokens || u.output_tokens || u.cache_read_input_tokens || u.cache_creation_input_tokens)) continue
      let s = byId.get(m.id)
      if (!s) {
        if (o.seen && o.seen.has(m.id)) continue // history replayed into this thread: counted where it first appeared
        if (o.seen) o.seen.add(m.id)
        s = { kind: 'request', id: m.id, ts: r.timestamp, line: r._n, items: pending, blocks: [], model: m.model }
        pending = []
        byId.set(m.id, s)
        steps.push(s)
      }
      // rule 1: one response, several records. Keep the most complete usage: early records can be partial.
      if (!s.usage || (u.output_tokens || 0) >= (s.usage.output_tokens || 0)) s.usage = u
      for (const k of ['attributionSkill', 'attributionPlugin', 'attributionMcpServer', 'effort']) if (r[k]) s[k] = r[k]
      s.stop = m.stop_reason ?? s.stop
      s.requestId = r.requestId || s.requestId
      s.ref = r.requestRef || s.ref
      s.ttl = r.promptCacheRequested || s.ttl
      if (detail)
        for (const b of m.content || [])
          s.blocks.push(
            b.type === 'text' ? { type: 'text', text: b.text || '' }
            : b.type === 'thinking' || b.type === 'redacted_thinking' ? { type: 'thinking', text: b.thinking || '' }
            : b.type === 'tool_use' || b.type === 'server_tool_use' ? { type: 'tool', name: b.name, arg: argOf(b.input, cwd), text: JSON.stringify(b.input, null, 2) }
            : { type: 'other', name: b.type, text: JSON.stringify(b, null, 2) },
          )
      continue
    }

    if (r.type === 'user') {
      const c = r.message && r.message.content
      if (r.isCompactSummary) {
        const sum = contentOf(c)
        const got = { summaryChars: sum.chars, summaryLine: r._n, ...(detail ? { summary: sum.text } : null) }
        if (comp && comp.summaryChars == null) Object.assign(comp, got)
        else stash = got
        continue
      }
      if (r.isVisibleInTranscriptOnly) continue
      if (typeof c === 'string') { pending.push(prompt(c, r)); continue }
      for (const b of c || []) {
        if (b.type === 'tool_result') {
          const t = tools.get(b.tool_use_id) || { name: 'tool', arg: '' }
          const res = r.toolUseResult && typeof r.toolUseResult === 'object' ? r.toolUseResult : {}
          const isAgent = t.name === 'Agent' || t.name === 'Task'
          pending.push(isAgent
            ? item('subagent', 'Subagent report', t.agent || res.agent_type || 'agent', b.content, r, { arg: t.arg, toolUseId: b.tool_use_id, agentId: res.agentId || res.agent_id })
            : item('tool', 'Tool result', t.name + (t.arg ? ' · ' + t.arg : ''), b.content, r, { tool: t.name, arg: t.arg, error: !!b.is_error }))
        } else if (b.type === 'text') pending.push(prompt(b.text || '', r))
        else pending.push(item('prompt', 'Attachment', b.type, [b], r))
      }
      continue
    }

    if (r.type === 'attachment') {
      const a = r.attachment || {}
      const sent = r.rendered ? r.rendered.map(x => x.content || '').join('\n') : hasRendered ? '' : fallbackText(a)
      if (!sent) continue // not sent to the model: most hook results, bookkeeping
      const [kind, name, label, extra] = attName(a)
      pending.push(item(kind, name, label, sent, r, extra))
      continue
    }

    if (r.type === 'system' && r.subtype === 'compact_boundary') {
      const m = r.compactMetadata || {}
      comp = { kind: 'compaction', ts: r.timestamp, line: r._n, trigger: m.trigger || 'auto', pre: m.preTokens ?? null, post: m.postTokens ?? null, ms: m.durationMs ?? null, ...stash }
      stash = null
      steps.push(comp)
      pending = [] // what came before is inside the summary now
    }
  }

  // second pass: balances, causes, flags (rules 2 to 7)
  const entries = [], totals = zero(), models = []
  let prevCtx = 0, prev = null, prevT = null, afterComp = false, any1h = false, since = 1, hasShape = false
  let peak = { ctx: 0, no: 0 }
  steps.forEach((s, i) => {
    const no = i + 1, t = Date.parse(s.ts)
    if (s.kind === 'compaction') {
      const next = steps.slice(i + 1).find(x => x.kind === 'request')
      const nu = next && usageOf(next.usage)
      const pre = s.pre ?? (prev ? prevCtx + prev.out : null)
      const post = s.post ?? (nu ? nu.in + nu.cr + nu.cw : null)
      entries.push({
        no, kind: 'compaction', ts: s.ts, t, line: s.line, trigger: s.trigger, pre, post, ctx: post,
        change: pre != null && post != null ? post - pre : null, gapMs: prevT != null ? t - prevT : null, ms: s.ms,
        summaryChars: s.summaryChars || 0, summaryLine: s.summaryLine, ...(detail ? { summary: s.summary || '' } : null),
      })
      if (post != null) prevCtx = post
      afterComp = true
      since = no
      return
    }

    const u = usageOf(s.usage), ctx = u.in + u.cr + u.cw
    const first = !prev && !afterComp
    const change = ctx - prevCtx
    const trimmed = change < 0 && !first && !afterComp // rule 6
    let items = s.items, carried = 0, carriedEst = false

    if (first) {
      const req = reqs.get(s.ref)
      const shape = req && shapes.get(req.shapeHash)
      if (shape) {
        hasShape = true
        const sys = (shape.system || []).map(b => (typeof b === 'string' ? b : b.text || '')).join('\n\n')
        const defs = (shape.tools || []).map(d => d.name + ': ' + (d.description || '') + '\n' + JSON.stringify(d.input_schema || {})).join('\n\n')
        const mk = (name, label, text) => ({ kind: 'system', name, label, chars: text.length, w: text.length, tok: 0, ...(detail ? { text } : null) })
        items = [mk('System prompt', plural((shape.system || []).length, 'block'), sys), mk('Tool definitions', plural((shape.tools || []).length, 'tool'), defs), ...items]
      }
    } else if (!afterComp && change >= 0) carried = prev.out // rule 3: the model's own reply comes back

    let rest = change - carried
    if (rest < 0 && change >= 0) {
      // less came back than the reply held (thinking is dropped, say): estimate the split
      rest = Math.min(change, Math.round(items.reduce((a, x) => a + x.w, 0) / CPT))
      carried = change - rest
      carriedEst = true
    }
    if (change > 0) {
      const unknownStart = first && !hasShape
      const shares = split(rest, items.map(x => x.w))
      let left = rest
      items.forEach((x, k) => { x.tok = Math.min(shares[k], Math.floor(x.w / (unknownStart ? CPT : CAP))); left -= x.tok })
      // ToolSearch returns references; the definitions they stand for are sent too, but are not in the log
      const loader = items.find(x => x.tool === 'ToolSearch')
      if (left > 0 && loader) (loader.tok += left), (loader.note = 'Includes the tool definitions it loaded, which the log does not hold.'), (left = 0)
      const largest = [...items].sort((a, b) => b.w - a.w)[0]
      if (left > 0 && largest && !unknownStart && left <= Math.max(50, rest * 0.05)) (largest.tok += left), (left = 0) // dense text, not a gap
      const gap = { chars: 0, w: 0, tok: left, ...(detail ? { text: '' } : null) }
      if (left > 0 && unknownStart) items = [{ kind: 'system', name: 'System prompt and tool definitions', label: 'not recorded in this log', ...gap }, ...items]
      else if (left > 0) items = [...items, { kind: 'other', name: 'Not recorded in the log', label: '', ...gap }]
    }

    // rule 7: the context was written to the cache again
    let flag = null
    if (prev && !afterComp && u.cr < REWRITE_READ * prevCtx && u.cw > REWRITE_WRITE) {
      const now = reqs.get(s.ref) || {}, was = reqs.get(prev.ref) || {}
      const knobs = x => x.params && JSON.stringify([x.params.output_config, x.params.thinking])
      const gapMs = t - prevT, again = Math.min(u.cw, Math.max(0, prevCtx - u.cr))
      const code = s.model !== prev.model ? 'model'
        : u.speed !== prev.speed ? 'speed'
        : gapMs > (any1h ? 3600e3 : 300e3) ? 'idle'
        : now.shapeHash && was.shapeHash && now.shapeHash !== was.shapeHash ? 'shape'
        : knobs(now) && knobs(was) && knobs(now) !== knobs(was) ? 'effort'
        : 'unexplained'
      flag = { code, again, gapMs, from: prev.model, ...(o.rates ? rewriteCost(o.rates, { ...u, model: s.model }, again) : null) }
    }

    const by = {}, cnt = {}, toolTok = {}, toolCnt = {}
    if (carried) (by.reply = carried), (cnt.reply = 1)
    for (const x of items) {
      by[x.kind] = (by[x.kind] || 0) + x.tok
      if (x.kind === 'system') cnt.system = 1
      else cnt[x.kind] = (cnt[x.kind] || 0) + 1
      if (x.kind === 'tool') (toolTok[x.tool] = (toolTok[x.tool] || 0) + x.tok), (toolCnt[x.tool] = (toolCnt[x.tool] || 0) + 1)
    }
    const a = o.rates ? amountOf(o.rates, { ...u, model: s.model }) : null
    const req = reqs.get(s.ref)
    const e = {
      no, kind: 'request', ts: s.ts, t, line: s.line, msgId: s.id, model: s.model, ctx, change, prevCtx, since, first, afterComp, trimmed,
      in: u.in, cw: u.cw, cw5: u.cw5, cw1: u.cw1, cr: u.cr, out: u.out, think: u.think, web: u.web, speed: u.speed, geo: u.geo,
      amt: a ? a.all : null, carried, carriedEst, items, by, cnt, tools: toolTok, toolCnt, flag,
      skill: s.attributionSkill, plugin: s.attributionPlugin, mcp: s.attributionMcpServer, effort: s.effort,
      recomp: afterComp ? Math.max(0, prevCtx - u.cr) : 0, // the summary and system prompt, sent afresh after a compaction
      source: req && req.querySource,
      ...(detail ? { blocks: s.blocks, usage: s.usage, stop: s.stop, requestId: s.requestId, ttl: s.ttl } : null),
    }
    Object.assign(e, nameOf(e))
    entries.push(e)

    addTotals(totals, { n: 1, in: u.in, cw: u.cw, cw5: u.cw5, cw1: u.cw1, cr: u.cr, out: u.out, think: u.think, web: u.web, amt: a ? a.all : 0, saved: a ? a.saved : 0, premium: a ? a.premium : 0, unpriced: a ? 0 : 1 })
    if (!models.includes(s.model)) models.push(s.model)
    if (ctx > peak.ctx) peak = { ctx, no }
    if (u.cw1 > 0) any1h = true
    prevCtx = ctx
    prev = { out: u.out, model: s.model, ref: s.ref, speed: u.speed }
    prevT = t
    afterComp = false
  })

  const reqEntries = entries.filter(e => e.kind === 'request')
  return {
    entries, totals, models, peak, cwd, hasShape, control,
    title: custom || ai || firstPrompt,
    opening: reqEntries.length ? reqEntries[0].ctx : 0,
    closing: entries.length ? entries[entries.length - 1].ctx ?? prevCtx : 0,
    first: entries.length ? entries[0].t : null,
    last: entries.length ? entries[entries.length - 1].t : null,
  }
}

// rule 4: an entry is named for its largest cause
function nameOf(e) {
  const its = e.items
  if (e.first) {
    const names = [...new Set([...its].sort((a, b) => b.tok - a.tok).map(x => x.name))].slice(0, 5)
    return { title: 'Opening balance', sub: names.map((s, i) => (i ? lower(s) : s)).join(', '), top: null }
  }
  if (e.trimmed) return { title: 'Trimmed by context management', sub: 'Context fell with no compaction recorded', top: null }
  if (!its.length) return { title: e.carried ? 'Reply carried forward' : 'No change', sub: e.carried ? 'Nothing else was added' : '', top: null }
  const sums = {}
  for (const x of its) sums[x.kind] = (sums[x.kind] || 0) + x.tok * 1e6 + x.w
  const kind = Object.keys(sums).sort((a, b) => sums[b] - sums[a])[0]
  const mine = its.filter(x => x.kind === kind)
  const top = [...mine].sort((a, b) => b.tok - a.tok || b.w - a.w)[0]
  const chars = plural(mine.reduce((a, x) => a + x.chars, 0), 'character')
  if (kind === 'tool') {
    const same = mine.every(x => x.tool === top.tool)
    const title = mine.length === 1 ? 'Tool result · ' + top.tool : same ? 'Tool results · ' + top.tool + ' × ' + mine.length : 'Tool results · ' + top.tool + ' and ' + (mine.length - 1) + ' more'
    const args = mine.map(x => x.arg).filter(Boolean)
    const what = args.length <= 1 ? args[0] || '' : args.length <= 3 ? args.map(base).join(', ') : args[0] + ' · and ' + (args.length - 1) + ' more'
    return { title, sub: (what ? what + ' · ' : '') + chars, top }
  }
  if (kind === 'subagent') return { title: 'Subagent report · ' + top.label, sub: top.arg ? '“' + top.arg + '”' : chars, top }
  if (kind === 'prompt') return { title: top.name, sub: top.label, top }
  if (kind === 'hook') return { title: 'Hook · ' + (top.event || 'hook'), sub: (top.label ? top.label + ' · ' : '') + chars, top }
  if (top.name === 'Not recorded in the log') return { title: 'Added context, not recorded', sub: 'Nothing in the log accounts for this rise', top }
  return { title: top.name, sub: (top.label ? top.label + ' · ' : '') + chars, top }
}

// ---- a session: the main thread and its subagents (rules 8 and 10) --------------------------

export function controlOf(cs) {
  if (!cs) return null
  const c = { in: 0, cw: 0, cr: 0, out: 0, web: 0, usd: cs.totalCostUSD ?? null, models: {} }
  for (const [m, x] of Object.entries(cs.modelUsage || {})) {
    const row = { in: x.inputTokens || 0, cw: x.cacheCreationInputTokens || 0, cr: x.cacheReadInputTokens || 0, out: x.outputTokens || 0, web: x.webSearchRequests || 0, usd: x.costUSD ?? null }
    c.models[m] = row
    for (const k of ['in', 'cw', 'cr', 'out', 'web']) c[k] += row[k]
  }
  return c
}

// Claude Code's own total is lower than what its logs itemize. Seen on real logs where a session was
// resumed (its counter misses a stretch) and with background workflow agents. Then it is no control.
export const shortOf = (ours, k) => ['in', 'cw', 'cr', 'out'].some(c => k[c] < ours[c] * 0.999 - 100)

// main: the records of a session log. subs: [{ agentId, meta, recs }], one per subagent log.
export function sessionOf(main, subs = [], o = {}) {
  const seen = new Set() // rule 1: a response replayed into another thread counts once
  const m = accountOf(main, { ...o, seen })
  const threads = subs.map(s => ({
    agentId: s.agentId, type: (s.meta && s.meta.agentType) || 'agent', desc: (s.meta && s.meta.description) || '', toolUseId: s.meta && s.meta.toolUseId,
    ...accountOf(s.recs, { ...o, sub: true, seen }),
  }))
  // rule 8: tie each subagent to the line that received its report
  const where = new Map()
  const index = (th, k) => {
    for (const e of th.entries)
      for (const x of e.items || [])
        if (x.kind === 'subagent') {
          if (x.toolUseId) where.set(x.toolUseId, [k, e, x])
          if (x.agentId) where.set(x.agentId, [k, e, x])
        }
  }
  index(m, -1)
  threads.forEach(index)
  threads.forEach((t, k) => {
    const hit = where.get(t.toolUseId) || where.get(t.agentId)
    if (!hit) return
    t.parent = { thread: hit[0], no: hit[1].no }
    hit[2].sub = k
    if (hit[1].top === hit[2]) hit[1].sub += ' · ' + plural(t.totals.n, 'request') + ' in its own account'
  })
  const subTotals = threads.reduce((a, t) => addTotals(a, t.totals), zero())
  const totals = addTotals(addTotals(zero(), m.totals), subTotals)
  const cwd = m.cwd || (threads[0] && threads[0].cwd) || ''
  return {
    id: o.id || '', title: m.title || o.id || 'Untitled session', project: cwd ? base(cwd) : o.project || '', cwd,
    main: m, subs: threads, totals, subTotals, control: controlOf(m.control),
    first: [m, ...threads].map(t => t.first).filter(Boolean).sort((a, b) => a - b)[0] || null,
    last: Math.max(m.last || 0, ...threads.map(t => t.last || 0)) || null,
  }
}

// ---- what the cache keeps, and the period statement -----------------------------------------

// Numbers and short labels only, never message text.
export function rowsOf(s) {
  const rows = [], comps = []
  const add = (th, k) => {
    for (const e of th.entries)
      if (e.kind === 'compaction') comps.push({ t: e.t, k, trigger: e.trigger, pre: e.pre, post: e.post })
      else
        rows.push({
          t: e.t, k, id: e.msgId, m: e.model, i: e.in, w5: e.cw5, w1: e.cw1, r: e.cr, o: e.out, th: e.think, web: e.web, sp: e.speed, geo: e.geo, c: e.ctx,
          by: e.by, cnt: e.cnt, tools: e.tools, tc: e.toolCnt, re: e.recomp, f: e.flag ? [e.flag.code, e.flag.again] : 0, sk: e.skill, pl: e.plugin, mcp: e.mcp,
        })
  }
  add(s.main, -1)
  s.subs.forEach(add)
  return { id: s.id, title: clip(s.title, 80), project: s.project, rows, comps, control: s.control, agents: s.subs.map(t => t.type) }
}

export const rowUse = r => ({ model: r.m, in: r.i, cw5: r.w5, cw1: r.w1, cr: r.r, out: r.o, web: r.web, speed: r.sp, geo: r.geo })

const WINDOW = 5 * 3600e3 // the length of a subscription usage window

// sessions: rowsOf() results. o: { from, to, project, tz }.
// A request belongs to the period its own timestamp falls in.
export function periodOf(sessions, o, rates) {
  const { from, to, project } = o
  const dayOf = new Intl.DateTimeFormat('en-CA', { timeZone: o.tz, year: 'numeric', month: '2-digit', day: '2-digit' })
  const seen = new Set(), timeline = []
  const cls = () => ({ tok: 0, amt: 0 })
  const P = {
    from, to, project, n: 0, unpriced: 0, amt: 0, think: 0, web: 0, webAmt: 0, saved: 0, premium: 0,
    classes: { in: cls(), cw: cls(), cr: cls(), out: cls() },
    sessions: [], projects: new Map(), models: new Map(), days: new Map(), agents: new Map(), tags: new Map(), windows: [],
    by: {}, cnt: {}, tools: {}, toolCnt: {}, flags: {}, recomp: { n: 0, tok: 0 }, comps: { auto: 0, manual: 0, fall: 0 },
    recon: { n: 0, missing: 0, crossing: 0, short: 0, shortAmt: 0, ours: zero(), theirs: { in: 0, cw: 0, cr: 0, out: 0, usd: 0 } },
  }
  const bump = (map, key, r, a, sid) => {
    const x = map.get(key) || map.set(key, { n: 0, tok: 0, in: 0, cw: 0, cr: 0, out: 0, amt: 0, sessions: new Set() }).get(key)
    x.n++
    x.in += r.i; x.cw += r.w5 + r.w1; x.cr += r.r; x.out += r.o
    x.tok += r.i + r.w5 + r.w1 + r.r + r.o
    x.amt += a ? a.all : 0
    x.sessions.add(sid)
  }
  const sum = (into, from) => { for (const k of Object.keys(from || {})) into[k] = (into[k] || 0) + from[k] }
  const ordered = [...sessions].filter(s => !project || s.project === project).sort((a, b) => ((a.rows[0] || {}).t || 0) - ((b.rows[0] || {}).t || 0))
  for (const s of ordered) {
    const row = { id: s.id, title: s.title, project: s.project, gone: !!s.gone, first: null, n: 0, peak: 0, in: 0, cw: 0, cr: 0, out: 0, amt: 0 }
    const whole = zero()
    let outside = 0
    for (const r of s.rows) {
      const a = amountOf(rates, rowUse(r))
      addTotals(whole, { n: 1, in: r.i, cw: r.w5 + r.w1, cr: r.r, out: r.o, amt: a ? a.all : 0 })
      if (r.t < from || r.t >= to) { outside++; continue }
      if (r.id && seen.has(r.id)) continue // rule 1: a response counts once across all files
      seen.add(r.id)
      const tok = r.i + r.w5 + r.w1 + r.r + r.o
      row.n++
      if (row.first == null || r.t < row.first) row.first = r.t
      if (r.c > row.peak && (r.k === -1 || !s.rows.some(x => x.k === -1))) row.peak = r.c
      row.in += r.i; row.cw += r.w5 + r.w1; row.cr += r.r; row.out += r.o; row.amt += a ? a.all : 0
      P.n++
      P.think += r.th || 0
      P.web += r.web || 0
      if (a) {
        P.classes.in.amt += a.in; P.classes.cw.amt += a.cw; P.classes.cr.amt += a.cr; P.classes.out.amt += a.out
        P.webAmt += a.web; P.amt += a.all; P.saved += a.saved; P.premium += a.premium
      } else P.unpriced++
      P.classes.in.tok += r.i; P.classes.cw.tok += r.w5 + r.w1; P.classes.cr.tok += r.r; P.classes.out.tok += r.o
      bump(P.projects, s.project || 'unknown', r, a, s.id)
      bump(P.models, r.m, r, a, s.id)
      bump(P.days, dayOf.format(r.t), r, a, s.id)
      bump(P.agents, r.k === -1 ? 'Main thread' : (s.agents && s.agents[r.k]) || 'agent', r, a, s.id)
      for (const [kind, v] of [['Skill', r.sk], ['Plugin', r.pl], ['MCP server', r.mcp]]) if (v) bump(P.tags, kind + ' · ' + v, r, a, s.id)
      timeline.push([r.t, tok, a ? a.all : 0])
      sum(P.by, r.by); sum(P.cnt, r.cnt); sum(P.tools, r.tools); sum(P.toolCnt, r.tc)
      if (r.re) (P.recomp.n++), (P.recomp.tok += r.re)
      if (r.f) {
        const f = P.flags[r.f[0]] || (P.flags[r.f[0]] = { n: 0, again: 0, extra: 0 })
        const c = rewriteCost(rates, rowUse(r), r.f[1])
        f.n++; f.again += r.f[1]; f.extra += c ? c.extra : 0
      }
    }
    if (!row.n) continue
    for (const c of s.comps) if (c.t >= from && c.t < to) (P.comps[c.trigger === 'manual' ? 'manual' : 'auto']++), (P.comps.fall += (c.pre || 0) - (c.post || 0))
    P.sessions.push(row)
    if (!s.control) P.recon.missing++
    else if (shortOf(whole, s.control)) (P.recon.short++), (P.recon.shortAmt += whole.amt) // not a control for this session: kept apart, never netted
    else {
      P.recon.n++
      if (outside) P.recon.crossing++
      addTotals(P.recon.ours, whole)
      for (const k of ['in', 'cw', 'cr', 'out']) P.recon.theirs[k] += s.control[k]
      P.recon.theirs.usd += s.control.usd || 0
    }
  }
  // Usage windows, read the way ccusage reads them: a window opens on the hour of its first request and
  // lasts five hours; the next opens after that, or after a five-hour pause.
  timeline.sort((a, b) => a[0] - b[0])
  let w = null
  for (const [t, tok, amt] of timeline) {
    if (!w || t - w.start > WINDOW || t - w.last > WINDOW) P.windows.push((w = { start: Math.floor(t / 3600e3) * 3600e3, first: t, last: t, n: 0, tok: 0, amt: 0 }))
    w.n++; w.tok += tok; w.amt += amt; w.last = t
  }
  P.tok = P.classes.in.tok + P.classes.cw.tok + P.classes.cr.tok + P.classes.out.tok
  return P
}
