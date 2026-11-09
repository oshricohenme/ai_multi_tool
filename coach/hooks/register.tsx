import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register, StarNext } from 'claude-code'

import type { DetailTab, Ev, Kind, Nudge, Tab, Turn } from '../types'

const PANE = 'coach'
const MAX_EVENTS = 1000
const MAX_TEXT = 60_000

// Shape tags: bump when Ev/Turn change, so a value an older version stored reads as empty.
const events = atom({ plugin: 'coach', key: 'events' } as const, [], { shape: 'events-2' })
const turns = atom({ plugin: 'coach', key: 'turns' } as const, [], { shape: 'turns-1' })
const tab = atom({ plugin: 'coach', key: 'tab' } as const, 'duration')
const detailTab = atom({ plugin: 'coach', key: 'detailTab' } as const, 'summary')
const query = atom({ plugin: 'coach', key: 'query' } as const, '')
const selected = atom({ plugin: 'coach', key: 'selected' } as const, null)
const windowMs = atom({ plugin: 'coach', key: 'windowMs' } as const, 0) // 0 = whole session
const nudge = atom({ plugin: 'coach', key: 'nudge' } as const, null)
const dismissed = atom({ plugin: 'coach', key: 'dismissed' } as const, [])

const COLOR: Record<Kind, string> = {
  system: '#a8a8a8',
  compact: '#6fd3b4',
  context: '#6fbf7f',
  user: '#7b9be0',
  assistant: '#9a86d0',
  tool: '#d0893c',
}
const TINT: Record<Kind, string> = {
  system: '#333333',
  compact: '#163029',
  context: '#1d3325',
  user: '#1f2b45',
  assistant: '#2d2742',
  tool: '#3b2a18',
}
const LABEL: Record<Kind, string> = {
  system: 'SYSTEM',
  compact: 'COMPACT',
  context: 'CONTEXT',
  user: 'USER',
  assistant: 'ASSISTANT',
  tool: 'TOOL',
}
const BADGE_W = 11 // widest label + padding
const GUTTER_W = 7

const oneLine = (s: string) => s.replace(/\s+/g, ' ').trim()
const clip = (s: string, n: number) => {
  const one = oneLine(s)
  return one.length > n ? `${one.slice(0, Math.max(0, n - 1))}…` : one
}
const k = (n = 0) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`)
const dur = (ms?: number) =>
  ms === undefined ? 'running' : ms < 1000 ? `${Math.round(ms)}ms` : `${(ms / 1000).toFixed(2)}s`
const money = (n?: number) => (n === undefined ? '$?' : `$${n.toFixed(4)}`)
const json = (v: unknown, pretty = false) => {
  try {
    return JSON.stringify(v, null, pretty ? 2 : undefined) ?? ''
  } catch {
    return String(v)
  }
}
const cut = (s: string) =>
  s.length > MAX_TEXT ? `${s.slice(0, MAX_TEXT)}\n… [${s.length - MAX_TEXT} chars cut]` : s
const inputOf = (x: Ev) => (x.inTok ?? 0) + (x.cacheRead ?? 0) + (x.cacheWrite ?? 0)
const span = (x: Ev) => (x.end === undefined ? undefined : x.end - x.start)
const typeOf = (v: unknown) => (v === null ? 'null' : Array.isArray(v) ? 'array' : typeof v)

// ── the local web app: a server the mod starts (server/server.mjs), fed over a Unix socket ──
// Module state, so a reload starts over: session.start runs again and the server replays its log.
type Op =
  | { op: 'put'; ev: Ev }
  | { op: 'patch'; id: string; p: Partial<Ev> }
  | { op: 'meta'; meta: Record<string, unknown> }
  | { op: 'sig'; at: number; list: Sig[]; counts: Record<string, [number, number]> }
let sock: string | undefined
let webUrl: string | undefined
let queue: Op[] = []
const send = async ($: EngineInterface, op: Op) => {
  if (!sock) {
    queue.push(op)
    return
  }
  const ops = [...queue, op]
  queue = []
  try {
    await $.http.fetch('http://coach/', { method: 'POST', body: JSON.stringify(ops), socketPath: sock })
  } catch {
    // the server is gone: keep what it missed (bounded) and start another
    queue = [...ops, ...queue].slice(-2000)
    sock = undefined
    webUrl = undefined
    void startServer($)
  }
}

// The server: started at session start, and again whenever it is found gone (at most every 10s).
// A new server retires any previous one for this session and replays the log, so nothing is lost.
const SERVER_VERSION = '5' // 5: classifier knows <agent-message> and <task-notification>
let sid = ''
let startedAt = 0
let starting = false
const startServer = async ($: EngineInterface) => {
  if (starting || sock || Date.now() - startedAt < 10_000) return
  starting = true
  startedAt = Date.now()
  try {
    // the server's own version: bump it when server/ changes so a reload replaces the running one
    const srv = $.process.spawn({ argv: ['node', `${$.plugin.root}/server/server.mjs`, sid], env: { DEVTOOLS_SERVER: SERVER_VERSION } })
    let buf = ''
    for await (const { stream, text } of srv) {
      if (stream !== 'stdout' || sock) continue
      buf += text
      const line = buf.split('\n').find(l => l.startsWith('{'))
      if (!line) continue
      const ready = JSON.parse(line) as { port: number; sock: string }
      webUrl = `http://127.0.0.1:${ready.port}`
      sock = ready.sock
      starting = false
      await $.ui.status(`coach ${webUrl}`)
      await send($, { op: 'meta', meta: {} })
    }
  } catch (err) {
    await $.ui.log(`coach server: ${String(err)}`, { to: 'debug' })
  } finally {
    // the process ended: forget it so the next send starts another
    starting = false
    sock = undefined
    webUrl = undefined
    await $.ui.status(undefined)
  }
}

// ponytail: the pane's copy clips text to 4k chars and keeps the newest events under ~2 MB (JSON escaping can nearly double it);
// the full text lives in the server's log and the web app.
const PANE_TEXT = 4000
const slim = <T extends Partial<Ev>>(x: T): T => ({
  ...x,
  ...(typeof x.payload === 'string' && x.payload.length > PANE_TEXT ? { payload: `${x.payload.slice(0, PANE_TEXT)}\n… [clipped here; full text in the web app]` } : {}),
  ...(typeof x.result === 'string' && x.result.length > PANE_TEXT ? { result: `${x.result.slice(0, PANE_TEXT)}\n… [clipped here; full text in the web app]` } : {}),
})
const fit = (l: Ev[]) => {
  let out = l.slice(-MAX_EVENTS)
  let size = out.reduce((a, x) => a + (x.payload?.length ?? 0) + (x.result?.length ?? 0) + (x.title?.length ?? 0) + 300, 0)
  while (out.length > 1 && size > 2_000_000) {
    const x = out[0]!
    size -= (x.payload?.length ?? 0) + (x.result?.length ?? 0) + (x.title?.length ?? 0) + 300
    out = out.slice(1)
  }
  return out
}
const push = async ($: EngineInterface, ev: Ev) => {
  await update($, events, l => fit([...l.map(slim), slim(ev)]))
  await send($, { op: 'put', ev })
}
const patch = async ($: EngineInterface, id: string, p: Partial<Ev>) => {
  await update($, events, l => fit(l.map(x => (x.id === id ? slim({ ...x, ...p }) : slim(x)))))
  await send($, { op: 'patch', id, p })
}

// ── the firehose: every event the engine raises, passed through untouched ──
// Each dispatch becomes one signal: event, who raised it, how long each link of
// the chain took (next.trace), the outcome, and a small, redacted summary of the
// input. Per-frame noise (renders, clocks, state) is only counted, never listed.
type Sig = {
  at: number
  ev: string
  by: string
  tier: string
  ms: number
  chain: [string, string, number][]
  sum?: Record<string, unknown>
  out?: string
  err?: string
}
const NOISY = /^(ui\.(render|resolve|scroll|focus|blit|invalidate|view|selection|panes)|prompt\.(paint|read|change|autocomplete|suggest|fill)|clock\.|state\.|store\.|session\.(id|cwd|root|surface|surfaces|version|identity))$/
const STREAMING = new Set(['turn.step', 'process.spawn'])
const KEEP = ['tool', 'tool_name', 'path', 'file_path', 'url', 'method', 'argv', 'command', 'server', 'key', 'model', 'trigger', 'type', 'origin', 'surface', 'skill', 'name', 'kind', 'agentId', 'agent_type', 'reason', 'message', 'title', 'notification_type', 'permission_mode', 'source', 'tier', 'door', 'event', 'to', 'cwd', 'old_cwd', 'new_cwd', 'file', 'instructions', 'subagent_type', 'description', 'id', 'status', 'decision', 'percentUsed', 'requestId', 'component', 'trigger_kind', 'memory_type', 'load_reason']
const SECRET = /(token|secret|password|authorization|cookie|api[_-]?key|credential)/i
let sigs: Sig[] = []
let counts: Record<string, [number, number]> = {} // event → [dispatches, ms]
const brief = (v: unknown): unknown => {
  if (typeof v === 'string') return v.length > 240 ? `${v.slice(0, 239)}…` : v
  if (Array.isArray(v)) return v.slice(0, 12).map(brief)
  if (v && typeof v === 'object') {
    try {
      const j = JSON.stringify(v)
      return j.length > 300 ? `${j.slice(0, 299)}…` : j
    } catch {
      return '{…}'
    }
  }
  return v
}
const summarize = (ev: string, e: unknown): Record<string, unknown> | undefined => {
  if (!e || typeof e !== 'object') return undefined
  const o = e as Record<string, unknown>
  const out: Record<string, unknown> = {}
  for (const k of KEEP) if (k in o && o[k] !== undefined) out[k] = SECRET.test(k) ? '•••' : brief(o[k])
  if (ev === 'env.get' || ev === 'env.set') delete out.value
  if (ev === 'session.append') {
    const m = o.message as Record<string, unknown> | undefined
    out.role = m?.role
    try {
      out.chars = JSON.stringify(m?.content ?? '').length
    } catch {}
    delete out.message
  }
  if (ev === 'tool.check' || ev === 'classic.PermissionRequest' || ev === 'classic.PostToolUseFailure' || ev === 'classic.PermissionDenied') out.input = brief(o.input ?? o.tool_input)
  if (ev === 'classic.PostToolUse') out.duration_ms = o.duration_ms
  if (ev === 'session.measure') {
    out.rateLimits = o.rateLimits
    out.cost = o.cost
    out.context = o.context && { tokens: (o.context as Record<string, unknown>).tokens, percent: (o.context as Record<string, unknown>).percent }
    out.changed = o.changed
  }
  // everything else on a classic event or a telemetry record, keys only past the whitelist
  const BULKY = /^(tool_input|tool_response|transcript_path|agent_transcript_path|session_id|requested_schema|background_tasks|session_crons|last_assistant_message)$/
  if (ev.startsWith('classic.') || ev.startsWith('telemetry.')) for (const k of Object.keys(o)) if (!(k in out) && !BULKY.test(k)) out[k] = SECRET.test(k) ? '•••' : brief(o[k])
  return Object.keys(out).length ? out : undefined
}
const outcomeOf = (r: unknown): string | undefined => {
  if (!r || typeof r !== 'object') return undefined
  const o = r as Record<string, unknown>
  if (o.decision !== undefined) return ['decision: ' + String(brief(o.decision)), o.rule && `rule ${String(o.rule)}`, o.setting && `from ${String(o.setting)}`, o.plugin && `by ${String(o.plugin)}`, o.hook && `hook ${String(o.hook)}`, o.reason && String(brief(o.reason))].filter(Boolean).join(' · ')
  for (const k of ['deny', 'skip', 'refuse', 'consumed', 'block', 'exitCode', 'status', 'isError', 'isConnected']) if (o[k] !== undefined && o[k] !== false) return `${k}: ${String(brief(o[k]))}`
  if (typeof o.text === 'string') return `${o.text.length} chars`
  return undefined
}
// ── Coach, live: only the time-sensitive checks; the rest run in the web Coach view. ──
// Thresholds follow code.claude.com/docs/en/best-practices (two failed corrections → /clear).
const CORR = /^\s*(no\b|nope|wrong|incorrect|that'?s not|not what i|this is not|still (broken|failing|wrong|not)|again\b|you didn'?t|you (forgot|missed|broke|ignored)|didn'?t work|doesn'?t work|does not work|it (still|doesn'?t)|undo|revert|why did you)/i
const live = {
  prompts: [] as boolean[], asks: {} as Record<string, number>, lastEnd: 0, lastCtx: 0, ttlMs: 3600e3,
  // per main-agent turn
  turnText: '', fails: {} as Record<string, number>, cmdFailed: new Set<string>(), testEdited: false, srcEdited: false,
  // per session
  contN: 0, wrapped: false, goal: false, rewound: false, compacted: false, ctxTurns: [] as string[],
  denied: 0, fastToggles: 0, bypassChecked: false, bg: 0, loops: 0, subagents: 0,
}
// Which nudge holds the band: high > medium > low, newer first within a level. Losers wait in `held`.
const RANK = { high: 3, medium: 2, low: 1 } as const
const rank = (n: Nudge) => RANK[n.sev ?? 'low']
let held: Nudge[] = []
const TEST_FILE = /(^|[/._-])(tests?|spec|__tests__)([/._-]|$)|_test\.|\.test\.|\.spec\.|snap/i
const TEST_CMD = /\b(test|jest|vitest|pytest|mocha|tsc|build|lint|cargo|go test|make|check)\b/
const SKIP_ADDED = /\b(xit|xdescribe|it\.skip|test\.skip|describe\.skip|\.only|@Ignore|pytest\.mark\.skip|t\.Skip)\b|\bskip\(/
const ASSERTS = /\b(assert\w*|expect)\b/g
const asserts = (t: unknown) => (typeof t === 'string' ? (t.match(ASSERTS) ?? []).length : 0)
const words = (t: string) => new Set(t.toLowerCase().match(/[a-z]{4,}/g) ?? [])
const permKey = (tool: string, input: unknown) => {
  let a: Record<string, unknown> = {}
  try { a = typeof input === 'string' ? JSON.parse(input) : (input as Record<string, unknown>) ?? {} } catch {}
  if (tool === 'Bash' && typeof a.command === 'string') {
    const t = a.command.replace(/^\s*((timeout|nice)\s+\S+\s+|\w+=\S+\s+)*/, '').trim().split(/\s+/)
    return `Bash(${t.slice(0, 2).join(' ')} *)`
  }
  return tool
}
const say = async ($: EngineInterface, n: Nudge) => {
  if ((await read($, dismissed)).includes(n.id)) return
  const cur = await read($, nudge)
  if (cur?.id === n.id || held.some(h => h.id === n.id)) return
  if (!cur) return void (await update($, nudge, () => n))
  if (rank(n) >= rank(cur)) {
    held = [...held, cur]
    await update($, nudge, () => n)
  } else held = [...held, n]
}
// "Got it": clear the band and promote the best waiting nudge.
const dismiss = async ($: EngineInterface, id: string) => {
  await update($, dismissed, l => [...l, id].slice(-50))
  const seen = await read($, dismissed)
  held = held.filter(h => !seen.includes(h.id))
  held.sort((a, b) => rank(a) - rank(b)) // last = best, newest among equals
  const next = held.pop() ?? null
  await update($, nudge, () => next)
}
const norm = (tool: string, input: unknown) => `${tool} ${json(input).replace(/\s+/g, ' ').slice(0, 300)}`
const str = (v: unknown) => (typeof v === 'string' ? v : '')
const coachSee = async ($: EngineInterface, ev: string, e: unknown) => {
  const x = (e ?? {}) as Record<string, unknown>
  const input = (x.tool_input ?? {}) as Record<string, unknown>
  const main = !x.agent_id && !x.agentId
  if (ev === 'turn.start' && !x.agentId && typeof x.text === 'string') {
    const text = x.text
    live.turnText = text
    live.fails = {}; live.cmdFailed = new Set(); live.testEdited = false; live.srcEdited = false
    live.prompts = [...live.prompts, CORR.test(text)].slice(-4)
    live.ctxTurns = [...live.ctxTurns, text].slice(-2)
    if (/^\s*\/goal\b/.test(text)) live.goal = true
    if (/^\s*\/rewind\b/.test(text)) live.rewound = true
    if (/^\s*(continue|keep going|go on|next)\b[\s.!]*$/i.test(text)) {
      if (live.wrapped && !live.goal && ++live.contN === 2)
        await say($, { id: 'continue-without-goal', sev: 'medium', text: `You typed "${text.trim().slice(0, 20)}" ${live.contN} times.`, fix: '/goal <check> passes, or stop after 20 turns, keeps it going on its own.' })
    } else if (/^\s*(undo|revert|go back|put it back|restore)\b/i.test(text) && !live.rewound)
      await say($, { id: 'undo-by-prose', sev: 'low', text: 'Asking Claude to undo.', fix: 'Esc Esc (or /rewind) restores code and conversation exactly; it is cheaper than asking Claude to undo.' })
    if (live.prompts.filter(Boolean).length >= 2 && live.prompts[live.prompts.length - 1])
      await say($, { id: 'correction-loop', sev: 'medium', text: 'Second correction in a row. Another one mostly adds failed attempts to the context.', fix: 'Esc Esc (/rewind) to before the first wrong try, or /clear and restate with what you learned.' })
  } else if (ev === 'classic.Stop') {
    if (main) live.wrapped = /(next,? i('| wi)ll|remaining:)/i.test(str(x.last_assistant_message))
    const bg = (x.background_tasks as { type?: string }[] | undefined) ?? []
    live.bg = bg.length
    live.subagents = bg.filter(t => t.type === 'subagent').length
    live.loops = ((x.session_crons as unknown[] | undefined) ?? []).length
  } else if (ev === 'classic.PermissionRequest') {
    const key = permKey(String(x.tool_name ?? '?'), x.tool_input ?? x.input)
    const n = (live.asks[key] = (live.asks[key] ?? 0) + 1)
    if (n === 3) await say($, { id: `perm:${key}`, sev: 'low', text: `You approved ${key} 3 times.`, fix: `Add "${key}" to permissions.allow in .claude/settings.json, or Shift+Tab into auto mode.` })
  } else if (ev === 'classic.PostToolUseFailure') {
    const tool = str(x.tool_name)
    const key = norm(tool, input)
    const n = (live.fails[key] = (live.fails[key] ?? 0) + 1)
    if (n === 3) await say($, { id: `same-failure-loop:${key.slice(0, 80)}`, sev: 'medium', text: `Claude ran the same failing ${tool} call ${n} times.`, fix: 'Esc and say what to try differently.' })
    if (tool === 'Bash' && main) live.cmdFailed.add(str(input.command))
  } else if (ev === 'classic.PostToolUse' || ev === 'classic.PermissionDenied') {
    if (ev === 'classic.PermissionDenied') {
      if (++live.denied === 3) await say($, { id: 'auto-mode-block-streak', sev: 'medium', text: `Auto mode blocked ${live.denied} in a row and will pause.`, fix: '/auto-mode-setup tells it what you trust.' })
      return
    }
    live.denied = 0
    const tool = str(x.tool_name)
    if (!main) return
    const file = str(input.file_path)
    if (tool === 'Bash') {
      const cmd = str(input.command)
      if (live.cmdFailed.has(cmd) && TEST_CMD.test(cmd) && live.testEdited && !live.srcEdited && !/\b(tests?|spec)\b/i.test(live.turnText))
        await say($, { id: 'test-tampering', sev: 'high', text: 'Claude changed a test right after it failed, then the same command passed.', fix: 'Check: was the test wrong, or the code? Say "Don\'t change tests; fix the implementation."' })
    } else if (/^(Edit|Write|MultiEdit)$/.test(tool) && file) {
      if (!TEST_FILE.test(file)) live.srcEdited = true
      else if (!(tool === 'Write' && (x.tool_response as { type?: string } | undefined)?.type === 'create') && !/\b(tests?|spec)\b/i.test(live.turnText)) {
        const edits = tool === 'MultiEdit' ? (input.edits as { old_string?: string; new_string?: string }[] ?? []) : [{ old_string: str(input.old_string), new_string: str(input.new_string ?? input.content) }]
        const add = edits.map(d => str(d.new_string)).join('\n'), del = edits.map(d => str(d.old_string)).join('\n')
        live.testEdited = live.cmdFailed.size > 0 || live.testEdited
        if (tool !== 'Write' && (SKIP_ADDED.test(add) && !SKIP_ADDED.test(del) || asserts(del) > asserts(add)))
          await say($, { id: 'test-tampering', sev: 'high', text: `Claude weakened a test in ${file.split('/').pop()} (${SKIP_ADDED.test(add) ? 'added a skip' : `${asserts(del)} asserts -> ${asserts(add)}`}).`, fix: 'Check: was the test wrong, or the code? Say "Don\'t change tests; fix the implementation."' })
      }
    }
  } else if (ev === 'session.measure') {
    const ctx = x.context as { percent?: number; tokens?: number } | undefined
    if ((ctx?.percent ?? 0) >= 70 && !live.compacted && live.ctxTurns.length === 2) {
      const [a, b] = live.ctxTurns.map(t => words(t!)) as [Set<string>, Set<string>]
      const shared = [...a].filter(w => b.has(w)).length / Math.max(1, Math.min(a.size, b.size))
      if (shared >= 0.3 || CORR.test(live.ctxTurns[1]!) || /^\s*(continue|keep going|go on|next)\b/i.test(live.ctxTurns[1]!))
        await say($, { id: 'context-high-mid-task', sev: 'low', text: `Context at ${Math.round(ctx!.percent!)}%.`, fix: 'At the next break: /compact keep <what matters>, or write PROGRESS.md and /clear.' })
    }
    const five = ((x.rateLimits as { kind: string; percentUsed: number }[] | undefined) ?? []).find(r => r.kind === 'five_hour')
    if (five && five.percentUsed >= 80 && (live.loops > 0 || live.bg > 0))
      await say($, { id: 'rate-limit-with-background', sev: 'medium', text: `5-hour limit at ${Math.round(five.percentUsed)}%. Background work is still spending it (${live.bg} running${live.loops ? `, ${live.loops} scheduled` : ''}).`, fix: 'Stop the /loop, or lower /effort.' })
  } else if (ev === 'session.compact') {
    live.compacted = true
  } else if (ev === 'command.run' || ev === 'config.set') {
    const isFast = ev === 'command.run' ? x.command === 'fast' : /fast/i.test(str(x.key))
    const off = ev === 'command.run' ? /^\s*off\b/i.test(str(x.args)) : x.value === false
    if (x.command === 'rewind') live.rewound = true
    if (isFast && !off) {
      live.fastToggles++
      if (live.lastCtx > 30000 || live.fastToggles >= 2)
        await say($, { id: 'fast-mode-toggle', sev: 'medium', text: live.fastToggles >= 2 ? `Fast mode toggled ${live.fastToggles} times.` : `Fast mode on at ${k(live.lastCtx)} context.`, fix: `Fast mode has its own cache: turning it on now re-reads ${k(live.lastCtx)} at fast prices. Enable it at session start.` })
    }
  }
  if (!live.bypassChecked && x.permission_mode === 'bypassPermissions' && ev.startsWith('classic.')) {
    live.bypassChecked = true
    const sb = (await $.settings.read({ key: 'sandbox' }).catch(() => undefined)) as { enabled?: boolean } | undefined
    if (!sb?.enabled) await say($, { id: 'bypass-no-sandbox', sev: 'high', text: 'Bypass mode with no sandbox.', fix: 'Use auto mode (Shift+Tab), or turn on /sandbox, or run in a container.' })
  }
  if (ev === 'classic.PreModelSwitch') {
    if (typeof x.cache_ttl === 'string') live.ttlMs = /5m/.test(x.cache_ttl) ? 300e3 : 3600e3
    const ctx = Number(x.context_tokens) || 0
    if (ctx >= 50000) await $.ui.toast(`Model switch re-sends ${Math.round(ctx / 1000)}k tokens uncached (≈$${(Number(x.estimated_cache_write_usd) || 0).toFixed(2)}).`)
  }
}

const WATCH = new Set(['turn.start', 'session.measure', 'session.compact', 'command.run', 'config.set'])
const observe = ($: EngineInterface, e: unknown, next: StarNext) => {
  const ev = next.event as string
  // our own calls would observe themselves forever
  if (next.origin.plugin === 'coach' || ev === 'engine.create') return next(e)
  if (WATCH.has(ev) || (ev.startsWith('classic.') && next.event !== 'classic.PreToolUse')) void coachSee($, ev, e).catch(() => {})
  if (STREAMING.has(ev)) {
    if (!NOISY.test(ev)) sigs.push({ at: Date.now(), ev, by: next.origin.plugin, tier: next.origin.tier, ms: 0, chain: [], sum: summarize(ev, e), out: 'stream' })
    return next(e)
  }
  const at = Date.now()
  const done = (r: unknown, err?: unknown) => {
    const trace = next.trace
    const ms = trace.reduce((a, t) => a + t.ms, 0)
    const c = (counts[ev] ??= [0, 0])
    c[0]++
    c[1] += ms
    if (NOISY.test(ev)) return
    sigs.push({
      at, ev, by: next.origin.plugin, tier: next.origin.tier, ms: Math.round(ms * 10) / 10,
      chain: trace.map(t => [t.plugin, t.outcome, Math.round(t.ms * 10) / 10] as [string, string, number]),
      sum: summarize(ev, e), out: err === undefined ? outcomeOf(r) : undefined,
      err: err === undefined ? undefined : brief(String(err)) as string,
    })
    if (sigs.length > 4000) sigs = sigs.slice(-2000) // server down: keep the latest
  }
  return next(e).then(
    r => (done(r), r),
    err => {
      done(undefined, err)
      throw err
    },
  )
}
const flush = async ($: EngineInterface) => {
  if (!sock || (!sigs.length && !Object.keys(counts).length)) return
  const batch = sigs
  const agg = counts
  sigs = []
  counts = {}
  await send($, { op: 'sig', at: Date.now(), list: batch, counts: agg })
}

// What the web app's Context view draws: /context's breakdown, trimmed to plain numbers.
const snapshotContext = async ($: EngineInterface) => {
  try {
    const { context } = await $.session.usage({ breakdown: 'full' })
    const b = context.breakdown
    await send($, {
      op: 'meta',
      meta: {
        window: context.window,
        context: b && {
          at: await $.clock.now(),
          total: b.totalTokens,
          max: b.rawMaxTokens,
          percent: b.percentage,
          autoCompactAt: b.autoCompactThreshold,
          model: b.model,
          categories: b.categories.map(c => ({ name: c.name, tokens: c.tokens, kind: c.kind, isDeferred: c.isDeferred })),
          memoryFiles: b.memoryFiles.map(f => ({ path: f.path, type: f.type, tokens: f.tokens })),
          mcpTools: b.mcpTools.map(t => ({ name: t.name, server: t.serverName, tokens: t.tokens, isLoaded: t.isLoaded })),
          skills: b.skills && { total: b.skills.totalSkills, tokens: b.skills.tokens },
        },
      },
    })
  } catch {
    // no session bound yet
  }
}

export const register: Register = on => {
  // Per loop: the turn it is in and the model call it is on, to tie tool calls to both.
  // Lost on reload; harmless.
  const turnOf = new Map<string, string>()
  const stepOf = new Map<string, number>()
  let lastSystem = ''
  let lastContext = ''

  on('*', observe)
  // telemetry: only the user's own OpenTelemetry collector stream (Anthropic's is for built-ins)
  on('telemetry.*', { to: 'collector' }, observe as never)

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'coach',
      description: 'Open Claude coach in the browser (localhost); `/coach pane` for the terminal pane',
    })
    const started = await next(e)
    sid = await $.session.id()
    sock = undefined
    webUrl = undefined
    startedAt = 0
    queue = [{ op: 'meta', meta: { cwd: e.cwd, session: sid } }]
    $.clock.every(1000, () => void flush($))
    void startServer($)
    return started
  })

  on('command.run', { command: 'coach' }, async ($, e) => {
    if (!webUrl && e.args.trim() !== 'pane') {
      startedAt = 0
      void startServer($)
      return { text: 'Claude coach: starting the web server. Run /coach again in a second.' }
    }
    if (e.args.trim() === 'pane' || !webUrl) {
      await $.ui.open({ id: PANE, title: 'Claude coach' })
      return { text: webUrl ? 'Dev tools pane opened.' : 'Dev tools pane opened (the web server is not up: is `node` on PATH?).' }
    }
    const opener = (await $.process.run(['uname'])).stdout.trim() === 'Darwin' ? 'open' : 'xdg-open'
    await $.process.run([opener, webUrl])
    return { text: `Dev tools: ${webUrl}` }
  })

  on('session.compact', async ($, e, next) => {
    const start = await $.clock.now()
    const before = (await $.session.usage()).cost?.usd
    const r = await next(e)
    if (r.skip === undefined && e.trigger !== 'precompute') {
      const after = (await $.session.usage()).cost?.usd
      await push($, {
        id: `compact:${start}`,
        kind: 'compact',
        agentId: e.agentId,
        trigger: e.trigger,
        title: `Compaction · ${e.trigger}${e.instructions ? ` · ${clip(e.instructions, 60)}` : ''}`,
        payload: e.instructions ?? '',
        result: `tokensBefore ${r.tokensBefore ?? '?'} → tokensAfter ${r.tokensAfter ?? '?'}`,
        start,
        end: await $.clock.now(),
        before: r.tokensBefore,
        after: r.tokensAfter,
        inTok: r.usage?.input_tokens,
        outTok: r.usage?.output_tokens,
        usd: before !== undefined && after !== undefined ? after - before : undefined,
      })
      await snapshotContext($)
    }
    return r
  })

  on('prompt.compose', async ($, e, next) => {
    const r = await next(e)
    const text = r.sections.map(s => `## ${s.id} (${s.scope})\n${s.text}`).join('\n\n')
    if (text !== lastSystem) {
      const isFirst = lastSystem === ''
      lastSystem = text
      const now = await $.clock.now()
      await push($, {
        id: `system:${now}`,
        kind: 'system',
        title: `${isFirst ? 'Initial System Prompt' : 'System prompt changed'} · ${r.sections.length} sections · ~${k(Math.round(text.length / 4))} tok`,
        payload: cut(text),
        result: '',
        start: now,
      })
    }
    return r
  })

  on('prompt.context', async ($, e, next) => {
    const r = await next(e)
    const text = r.blocks.map(b => `## ${b.name}\n${b.text}`).join('\n\n')
    if (text && text !== lastContext) {
      lastContext = text
      const now = await $.clock.now()
      await push($, {
        id: `context:${now}`,
        kind: 'context',
        title: `Runtime context: ${r.blocks.map(b => b.name).join(', ')}`,
        payload: cut(text),
        result: '',
        start: now,
      })
    }
    return r
  })

  on('turn.start', async ($, e, next) => {
    const r = await next(e)
    const now = await $.clock.now()
    turnOf.set(e.agentId ?? 'main', e.turnId)
    const turn: Turn = { id: e.turnId, agentId: e.agentId, text: e.text, start: now }
    await update($, turns, l => [...l, turn].slice(-300))
    if (e.text) {
      await push($, {
        id: `user:${e.turnId}`,
        kind: 'user',
        turnId: e.turnId,
        agentId: e.agentId,
        title: e.text,
        payload: cut(e.text),
        result: '',
        start: now,
      })
    }
    return r
  })

  on('turn.complete', async ($, e, next) => {
    const r = await next(e)
    const now = await $.clock.now()
    await update($, turns, l => l.map(t => (t.id === e.turnId ? { ...t, end: now, reason: e.reason } : t)))
    await patch($, `user:${e.turnId}`, { end: now })
    if (!e.agentId) await snapshotContext($)
    return r
  })

  // One model request: stream through untouched, collect reasoning, record usage, cost and timing.
  on('turn.step', async function* ($, e, next) {
    const id = `call:${e.turnId}:${e.index}`
    const start = await $.clock.now()
    stepOf.set(e.agentId ?? 'main', e.index)
    // ponytail: cost is the session ledger's delta across the call, so parallel subagent
    // calls can bleed into each other; price per model from usage if that matters.
    const before = (await $.session.usage()).cost?.usd
    await push($, {
      id,
      kind: 'assistant',
      turnId: e.turnId,
      agentId: e.agentId,
      step: e.index,
      title: '…',
      payload: json(
        { model: e.model, effort: e.effort, messageCount: e.messageCount, agentId: e.agentId, turnId: e.turnId, step: e.index },
        true,
      ),
      result: '',
      start,
      model: e.model,
      effort: e.effort === undefined ? undefined : String(e.effort),
    })
    let thinking = ''
    let firstAt: number | undefined
    let isDone = false
    try {
      const stream = next(e)
      for await (const c of stream) {
        if (firstAt === undefined && c.kind !== 'engine') firstAt = await $.clock.now()
        if (c.kind === 'thinking') thinking += c.text
        yield c
      }
      const r = await stream.result
      const after = (await $.session.usage()).cost?.usd
      const u = r.usage
      const tools = r.toolUses.map(t => t.name)
      const title =
        clip(r.answer, 300) || (tools.length ? `→ ${tools.join(', ')}` : `(${r.stopReason ?? 'no response'})`)
      const result = [
        thinking && `REASONING\n${thinking}`,
        r.answer && `ANSWER\n${r.answer}`,
        tools.length && `TOOL USES\n${r.toolUses.map(t => `${t.name} ${json(t.input, true)}`).join('\n')}`,
        r.serverToolUses?.length && `SERVER TOOLS\n${r.serverToolUses.map(t => json(t)).join('\n')}`,
      ]
        .filter(Boolean)
        .join('\n\n')
      await patch($, id, {
        end: await $.clock.now(),
        firstAt,
        title,
        result: cut(result),
        model: u?.model ?? e.model,
        inTok: u?.input_tokens,
        outTok: u?.output_tokens,
        cacheRead: u?.cache_read_input_tokens,
        cacheWrite: u?.cache_creation_input_tokens,
        usd: before !== undefined && after !== undefined ? after - before : undefined,
        stop: r.stopReason,
        thinkingChars: thinking.length,
        isError: r.stopReason === null,
      })
      if (!e.agentId && u) {
        live.lastEnd = Date.now()
        live.lastCtx = (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0)
      }
      isDone = true
      return r
    } finally {
      if (!isDone) {
        await patch($, id, {
          end: await $.clock.now(),
          firstAt,
          isError: true,
          title: '(interrupted)',
          result: thinking && `REASONING\n${thinking}`,
        })
      }
    }
  })

  on('tool.call', async ($, e, next) => {
    const { tool, tool_use_id, agentId, requestMeta, ...args } = e as typeof e & {
      agentId?: string
      requestMeta?: unknown
    }
    const id = `tool:${tool_use_id}`
    const loop = agentId ?? 'main'
    const head = `${tool.toLowerCase()} ${json(args)}`
    await push($, {
      id,
      kind: 'tool',
      tool,
      turnId: turnOf.get(loop),
      step: stepOf.get(loop),
      agentId,
      title: head,
      payload: cut(json(args, true)),
      result: '',
      start: await $.clock.now(),
    })
    try {
      const r = await next(e)
      const out = r.deny !== undefined ? `DENIED: ${r.deny}` : (r.text ?? json(r.result, true))
      await patch($, id, {
        end: await $.clock.now(),
        isError: r.deny !== undefined || Boolean(r.isError),
        title: `${head} → ${clip(out, 120)}`,
        result: cut(out),
        est: Math.round(out.length / 4),
      })
      return r
    } catch (err) {
      await patch($, id, {
        end: await $.clock.now(),
        isError: true,
        title: `${head} → threw`,
        result: String(err),
      })
      throw err
    }
  })

  // The coach's band: one nudge at a time, or the cache-expiry warning while you are away.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (e.props.hasSurvey) return next(e)
    let n = await read($, nudge)
    const away = Date.now() - live.lastEnd
    if (!n && live.lastEnd && away > live.ttlMs && live.lastCtx >= 100000 && !(await read($, dismissed)).includes('cold-cache')) {
      n = { id: 'cold-cache', text: `Cache expired ${Math.round(away / 60000)} min ago: your next message re-sends ${Math.round(live.lastCtx / 1000)}k tokens at full price.`, fix: 'Same task: go ahead, or /compact <focus> first. New task: /clear (free).' }
    }
    if (!n) return next(e)
    const shown = n
    const { Box, Button, Text } = $.ui.resolve(e)
    return (
      <Box flexDirection="column">
        <Text>
          <Text color="#e2a557">coach </Text>
          {shown.text}
        </Text>
        <Box>
          <Text dimColor>{shown.fix} </Text>
          <Button key="ok" label="Got it" onPress={() => dismiss($, shown.id)} />
        </Box>
      </Box>
    )
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button, Input } = $.ui.resolve(e)
    const cols = Math.max(50, e.props.bodyColumns ?? e.viewport?.columns ?? 80)
    const height = Math.max(16, e.props.scroll?.bodyRows ?? e.viewport?.rows ?? 30)
    const all = await read($, events)
    const allTurns = await read($, turns)
    const t = await read($, tab)
    const dt = await read($, detailTab)
    const q = await read($, query)
    const sel = await read($, selected)
    const win = await read($, windowMs)
    const now = await $.clock.now()

    const ql = q.toLowerCase()
    const shown = ql ? all.filter(x => `${x.title}\n${x.payload}\n${x.result}`.toLowerCase().includes(ql)) : all
    const calls = all.filter(x => x.kind === 'assistant')
    const turnNo = new Map(allTurns.map((tr, i) => [tr.id, i + 1]))
    const sessionStart = all[0]?.start ?? now
    const toggle = (id: string) => () => update($, selected, s => (s === id ? null : id))

    const badge = (kind: Kind, isActive = false) => (
      <Text color={COLOR[kind]} backgroundColor={TINT[kind]} bold inverse={isActive}>
        {` ${LABEL[kind]} `}
      </Text>
    )

    // ── header: tabs left, search right ──
    const tabButton = (id: Tab, label: string) => (
      <Button key={`tab-${id}`} plain onPress={() => update($, tab, () => id)}>
        <Text backgroundColor={t === id ? '#3a3a3a' : undefined} bold={t === id} dimColor={t !== id}>
          {` ${label} `}
        </Text>
      </Button>
    )
    const header = (
      <Box flexDirection="row" justifyContent="space-between">
        <Box flexDirection="row" gap={1}>
          {tabButton('duration', '◷ Duration')}
          {tabButton('turns', `Turns ${allTurns.length}`)}
          {tabButton('calls', `Calls ${calls.length}`)}
        </Box>
        <Box width={Math.min(32, Math.floor(cols / 3))} borderStyle="round" borderDimColor>
          <Input
            key="search"
            placeholder="⌕ Search"
            value={q}
            onInput={v => update($, query, () => v)}
            onSubmit={v => update($, query, () => v)}
          />
        </Box>
      </Box>
    )

    // ── swimlanes: one block per event, a gap kept between neighbours ──
    const W = cols - GUTTER_W - 1
    const firstShown = shown[0]?.start ?? now
    const t0 = win ? Math.max(firstShown, now - win) : firstShown
    const range = Math.max(1, now - t0)
    const lane = (pred: (x: Ev) => boolean) => {
      const cells: (Kind | null)[] = Array(W).fill(null)
      for (const x of shown) {
        if (!pred(x) || (x.end ?? now) < t0) continue
        const a = Math.max(0, Math.round(((x.start - t0) / range) * (W - 2)))
        const b = Math.min(W - 1, Math.max(a + 1, Math.round((((x.end ?? now) - t0) / range) * (W - 2))))
        if (cells[a] !== null) continue // keep the gap rather than smear two events together
        for (let i = a; i < b; i++) cells[i] = x.kind
      }
      // Runs of one kind become one Text, so each block is coloured by what it is.
      const runs: { kind: Kind | null; n: number }[] = []
      for (const c of cells) {
        const lastRun = runs[runs.length - 1]
        if (lastRun && lastRun.kind === c) lastRun.n++
        else runs.push({ kind: c, n: 1 })
      }
      return runs.map(r =>
        r.kind ? <Text color={COLOR[r.kind]}>{'█'.repeat(r.n)}</Text> : ' '.repeat(r.n),
      )
    }
    const laneRow = (name: string, pred: (x: Ev) => boolean) => (
      <Text>
        <Text dimColor>{name.padStart(GUTTER_W - 1)} </Text>
        {lane(pred)}
      </Text>
    )
    const winButton = (ms: number, label: string) => (
      <Button key={`win-${ms}`} plain onPress={() => update($, windowMs, () => ms)}>
        <Text bold={win === ms} dimColor={win !== ms}>
          {label}
        </Text>
      </Button>
    )
    const timeline = (
      <Box flexDirection="column">
        {laneRow('Input', x => x.kind === 'user' || x.kind === 'context' || x.kind === 'system')}
        {laneRow('Model', x => x.kind === 'assistant')}
        {laneRow('Tools', x => x.kind === 'tool')}
        <Box flexDirection="row" gap={1} justifyContent="flex-end">
          <Text dimColor>{dur(range)} ·</Text>
          {winButton(0, 'all')}
          {winButton(600_000, '10m')}
          {winButton(60_000, '1m')}
          {winButton(10_000, '10s')}
        </Box>
      </Box>
    )

    // ── detail panel ──
    const detail = sel ? all.find(x => x.id === sel) : undefined
    const panelW = detail ? Math.max(30, Math.floor(cols * 0.48)) : 0
    const listW = cols - panelW
    const used = 3 + (t === 'duration' ? 5 : 0)
    const room = Math.max(4, height - used)

    const schemaOf = async (x: Ev) => {
      if (x.kind === 'tool') {
        const info = (await $.tool.list()).find(tool => tool.name === x.tool)
        let args: Record<string, unknown> = {}
        try {
          args = JSON.parse(x.payload) as Record<string, unknown>
        } catch {}
        return [
          `${x.tool}${info?.mcp ? ' (MCP)' : ''}`,
          info?.description ? `\n${info.description}` : '',
          '\nARGUMENTS (as called)',
          ...Object.entries(args).map(([name, v]) => `  ${name}: ${typeOf(v)}`),
        ].join('\n')
      }
      if (x.kind === 'assistant') {
        return `TOOLS AVAILABLE\n${(await $.tool.list()).map(tool => `  ${tool.name}${tool.mcp ? ' (MCP)' : ''}`).join('\n')}`
      }
      return '(no schema for this row)'
    }
    const summaryOf = (x: Ev) =>
      [
        ['kind', LABEL[x.kind]],
        ['turn', x.turnId ? `${turnNo.get(x.turnId) ?? '?'}` : '—'],
        ['step', x.step === undefined ? '—' : `${x.step + 1}`],
        ['agent', x.agentId ?? 'main'],
        ...(x.kind === 'assistant'
          ? [
              ['model', x.model ?? '?'],
              ['effort', x.effort ?? '—'],
              ['stop', `${x.stop ?? '—'}`],
              ['input', `${x.inTok ?? 0} uncached + ${x.cacheRead ?? 0} cache read + ${x.cacheWrite ?? 0} cache write`],
              ['output', `${x.outTok ?? 0}`],
              ['reasoning', `${k(x.thinkingChars)} chars`],
              ['cost', money(x.usd)],
            ]
          : []),
        ...(x.kind === 'tool' ? [['tool', x.tool ?? '?']] : []),
        ['duration', dur(span(x))],
        ['status', x.isError ? 'error' : x.end === undefined && (x.kind === 'tool' || x.kind === 'assistant') ? 'running' : 'ok'],
        ['title', x.title],
      ]
        .map(([a, b]) => `${(a ?? '').padEnd(10)} ${b}`)
        .join('\n')
    const timingOf = (x: Ev) => {
      const d = span(x)
      const lines = [
        `started    +${dur(x.start - sessionStart)} from first event`,
        `duration   ${dur(d)}`,
      ]
      if (x.kind === 'assistant') {
        if (x.firstAt !== undefined) lines.push(`first byte ${dur(x.firstAt - x.start)}`)
        if (x.firstAt !== undefined && x.end !== undefined) lines.push(`streaming  ${dur(x.end - x.firstAt)}`)
        if (d && x.outTok) lines.push(`throughput ${((x.outTok / d) * 1000).toFixed(1)} out tok/s`)
      }
      if (x.kind === 'tool' && x.turnId) {
        const call = all.find(c => c.kind === 'assistant' && c.turnId === x.turnId && c.step === x.step)
        if (call?.end !== undefined) lines.push(`after call +${dur(x.start - call.end)} from model call end`)
      }
      return lines.join('\n')
    }

    let panel = null
    if (detail) {
      const body =
        dt === 'summary'
          ? summaryOf(detail)
          : dt === 'payload'
            ? detail.payload
            : dt === 'result'
              ? detail.result || (detail.end === undefined ? '(running…)' : '(empty)')
              : dt === 'schema'
                ? await schemaOf(detail)
                : timingOf(detail)
      const bodyLines = body.split('\n').slice(0, Math.max(3, height - 6))
      const dtButton = (id: DetailTab, label: string) => (
        <Button key={`dt-${id}`} plain onPress={() => update($, detailTab, () => id)}>
          <Text bold={dt === id} underline={dt === id} dimColor={dt !== id}>
            {label}
          </Text>
        </Button>
      )
      const where = [
        detail.turnId && `Turn ${turnNo.get(detail.turnId) ?? '?'}`,
        detail.step !== undefined && `Step ${detail.step + 1}`,
      ]
        .filter(Boolean)
        .join(' · ')
      panel = (
        <Box flexDirection="column" width={panelW} borderStyle="single" borderDimColor paddingX={1}>
          <Box flexDirection="row" justifyContent="space-between">
            <Box flexDirection="row" gap={1}>
              {badge(detail.kind)}
              <Text dimColor>{where}</Text>
            </Box>
            <Box flexDirection="row" gap={1}>
              <Button key="copy" plain onPress={press => $.ui.copy({ text: body, surface: press.surface })}>
                <Text dimColor>⧉</Text>
              </Button>
              <Button key="close" plain role="dismiss" onPress={() => update($, selected, () => null)}>
                <Text dimColor>×</Text>
              </Button>
            </Box>
          </Box>
          <Box flexDirection="row" gap={1}>
            {dtButton('summary', 'Summary')}
            {dtButton('payload', 'Payload')}
            {dtButton('result', 'Result')}
            {dtButton('schema', 'Schema')}
            {dtButton('timing', 'Timing')}
          </Box>
          <Text> </Text>
          <Text wrap="wrap">{bodyLines.join('\n')}</Text>
        </Box>
      )
    }

    // ── rows ──
    const metaOf = (x: Ev) =>
      x.kind === 'assistant'
        ? `${k(inputOf(x))}↓ ${k(x.outTok)}↑ ${money(x.usd)} ${dur(span(x))}`
        : x.kind === 'tool'
          ? dur(span(x))
          : ''
    const showMeta = !detail
    let prevTurn: string | undefined
    const row = (x: Ev) => {
      const isNewTurn = x.turnId !== undefined && x.turnId !== prevTurn
      if (x.turnId) prevTurn = x.turnId
      const gutter = isNewTurn ? `Turn ${turnNo.get(x.turnId ?? '') ?? ''}` : ''
      const meta = showMeta ? metaOf(x) : ''
      const textW = Math.max(8, listW - GUTTER_W - BADGE_W - 2 - (meta ? meta.length + 1 : 0))
      const agent = x.agentId ? `[${x.agentId.slice(0, 6)}] ` : ''
      const arrow = x.title.indexOf(' → ')
      const title = clip(`${agent}${x.title}`, textW)
      const at = arrow < 0 ? -1 : title.indexOf(' → ')
      return (
        <Button key={x.id} plain onPress={toggle(x.id)}>
          <Text dimColor>{gutter.padEnd(GUTTER_W).slice(0, GUTTER_W)}</Text>
          {' '.repeat(Math.max(0, BADGE_W - LABEL[x.kind].length - 2))}
          {badge(x.kind, sel === x.id)}{' '}
          <Text color={x.isError ? 'error' : undefined}>{at < 0 ? title : title.slice(0, at)}</Text>
          {at >= 0 && <Text dimColor>{title.slice(at)}</Text>}
          {meta && <Text dimColor> {meta}</Text>}
        </Button>
      )
    }

    let list
    if (t === 'duration') {
      list = shown.slice(-room).map(row)
    } else if (t === 'calls') {
      const rows = shown.filter(x => x.kind === 'assistant').slice(-(room - 1))
      list = [
        <Text dimColor>
          {'turn'.padEnd(GUTTER_W)}
          {'model'.padEnd(20)}
          {'in'.padStart(7)}
          {'cache r'.padStart(9)}
          {'out'.padStart(7)}
          {'cost'.padStart(9)}
          {'time'.padStart(8)}
        </Text>,
        ...rows.map(x => (
          <Button key={x.id} plain onPress={toggle(x.id)}>
            <Text dimColor>{`T${turnNo.get(x.turnId ?? '') ?? '?'}.${(x.step ?? 0) + 1}`.padEnd(GUTTER_W)}</Text>
            <Text color={COLOR.assistant} inverse={sel === x.id}>
              {clip(x.model ?? '?', 19).padEnd(20)}
            </Text>
            {k(x.inTok).padStart(7)}
            {k(x.cacheRead).padStart(9)}
            {k(x.outTok).padStart(7)}
            {money(x.usd).padStart(9)}
            <Text color={x.isError ? 'error' : undefined}>{dur(span(x)).padStart(8)}</Text>
          </Button>
        )),
      ]
    } else {
      list = allTurns.slice(-room).map(tr => {
        const mine = all.filter(x => x.turnId === tr.id)
        const cs = mine.filter(x => x.kind === 'assistant')
        const stats = `${cs.length} calls · ${mine.filter(x => x.kind === 'tool').length} tools · ${k(cs.reduce((s, x) => s + inputOf(x), 0))}↓ ${k(cs.reduce((s, x) => s + (x.outTok ?? 0), 0))}↑ · ${money(cs.reduce((s, x) => s + (x.usd ?? 0), 0))} · ${dur(tr.end === undefined ? undefined : tr.end - tr.start)}`
        return (
          <Button key={`turn-${tr.id}`} plain onPress={toggle(`user:${tr.id}`)}>
            <Text dimColor>{`Turn ${turnNo.get(tr.id)}`.padEnd(GUTTER_W + 1)}</Text>
            {badge('user', sel === `user:${tr.id}`)}{' '}
            {clip(`${tr.agentId ? `[agent ${tr.agentId.slice(0, 6)}] ` : ''}${tr.text || '(continuation)'}`, Math.max(10, listW - stats.length - 22))}
            <Text dimColor> {stats}</Text>
            <Text color={tr.reason && tr.reason !== 'answer' ? 'error' : undefined}> {tr.reason ?? ''}</Text>
          </Button>
        )
      })
    }

    const totals = `${k(calls.reduce((s, x) => s + inputOf(x), 0))}↓ ${k(calls.reduce((s, x) => s + (x.outTok ?? 0), 0))}↑ · ${money(calls.reduce((s, x) => s + (x.usd ?? 0), 0))}${ql ? ` · ${shown.length} match` : ''}`

    return (
      <Box flexDirection="column">
        {header}
        {t === 'duration' && timeline}
        <Box flexDirection="row" justifyContent="space-between">
          <Text dimColor>{'─'.repeat(Math.max(0, cols - totals.length - 18))}</Text>
          <Text dimColor> {totals} </Text>
          <Button key="clear" plain onPress={async () => {
            await update($, events, () => [])
            await update($, turns, () => [])
            await update($, selected, () => null)
          }}>
            <Text dimColor>clear</Text>
          </Button>
        </Box>
        <Box flexDirection="row">
          <Box flexDirection="column" width={listW}>
            {shown.length === 0 && <Text dimColor>Nothing recorded yet. Send a prompt.</Text>}
            {list}
          </Box>
          {panel}
        </Box>
      </Box>
    )
  })
}
