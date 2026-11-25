// One runnable check for the token accounting statement: the list in docs/TOKEN-ACCOUNTING-STATEMENT.md, "The check".
// Run: node --test token-accounting-statement/test/statement.test.mjs
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import http from 'node:http'
import crypto from 'node:crypto'
import { accountOf, sessionOf, rowsOf, periodOf, rateFor, split } from '../bin/account.mjs'
import { sessionHtml, periodHtml, sessionBrief } from '../bin/render.mjs'

const RATES = { asOf: 'test', cacheRead: 0.1, cacheWrite5m: 1.25, cacheWrite1h: 2, webSearchPer1000: 10, models: { 'claude-test-1': { in: 10, out: 50 } } }

// A made-up log: a prompt, a tool call and its result, a two-hour pause, a compaction.
function madeUpLog(prefix = 'msg') {
  let line = 0
  const at = min => new Date(Date.UTC(2026, 10, 12, 9, min)).toISOString()
  const usage = (i, cw, cr, out) => ({ input_tokens: i, cache_creation_input_tokens: cw, cache_read_input_tokens: cr, output_tokens: out, cache_creation: { ephemeral_1h_input_tokens: cw, ephemeral_5m_input_tokens: 0 } })
  const asst = (id, min, u, content) => ({ type: 'assistant', _n: ++line, timestamp: at(min), cwd: '/work/demo', message: { id: prefix + id, model: 'claude-test-1', content, usage: u, stop_reason: 'tool_use' } })
  const user = (min, content, extra) => ({ type: 'user', _n: ++line, timestamp: at(min), cwd: '/work/demo', message: { role: 'user', content }, ...extra })
  return [
    user(0, 'Find the bug in the exporter'),
    asst(1, 1, usage(2, 20000, 0, 10), [{ type: 'text', text: 'Running the tests first.' }]), // a partial record of the response...
    asst(1, 1, usage(2, 20000, 0, 100), [{ type: 'tool_use', id: 't1', name: 'Bash', input: { command: 'npm test' } }]), // ...and its last one
    user(2, [{ type: 'tool_result', tool_use_id: 't1', content: '<script>alert(1)</script> 14 passed' }]),
    asst(2, 3, usage(2, 150, 20000, 120), [{ type: 'text', text: 'Tests pass.' }]),
    user(123, 'ok fix that and run the tests'),
    asst(3, 125, usage(3, 20300, 0, 80), [{ type: 'text', text: 'Fixed.' }]), // two hours idle: nothing read from cache
    { type: 'system', subtype: 'compact_boundary', _n: ++line, timestamp: at(130), compactMetadata: { trigger: 'manual', preTokens: 20383, postTokens: 5000, durationMs: 4000 } },
    user(130, 'The user asked to fix the exporter.', { isCompactSummary: true, isVisibleInTranscriptOnly: true }),
    user(131, 'thanks, now commit'),
    asst(4, 132, usage(2, 5100, 0, 50), [{ type: 'text', text: 'Committed.' }]),
    { type: 'cost-state', _n: ++line, totalCostUSD: 1, modelUsage: { 'claude-test-1': { inputTokens: 500, outputTokens: 400, cacheReadInputTokens: 20000, cacheCreationInputTokens: 46000 } } },
  ]
}

test('the accounting rules, on a made-up log', () => {
  const L = accountOf(madeUpLog(), { rates: RATES })
  const reqs = L.entries.filter(e => e.kind === 'request')

  // two records with one message id count once, and the fuller usage wins
  assert.equal(reqs.length, 4)
  assert.equal(reqs[0].out, 100)
  assert.equal(L.totals.out, 100 + 120 + 80 + 50)

  // context after is input + cache read + cache write; the change is exact
  assert.deepEqual(reqs.map(e => e.ctx), [20002, 20152, 20303, 5102])
  assert.equal(reqs[1].change, 150)
  assert.equal(reqs[1].carried, 100) // the model's own reply, carried forward
  assert.equal(reqs[1].carried + reqs[1].items.reduce((a, x) => a + x.tok, 0), reqs[1].change)

  // an entry's detail holds the tool result's text, and is named for its largest cause
  assert.equal(reqs[1].title, 'Tool result · Bash')
  assert.match(reqs[1].items[0].text, /14 passed/)

  // a gap longer than the cache lifetime gives a re-write entry with the idle reason
  assert.equal(reqs[2].flag.code, 'idle')
  assert.equal(reqs[2].flag.again, 20152)
  assert.ok(reqs[2].flag.extra > 0)
  assert.equal(reqs[1].flag, null)

  // a compaction is one entry with the right fall, and holds the summary
  const comps = L.entries.filter(e => e.kind === 'compaction')
  assert.equal(comps.length, 1)
  assert.equal(comps[0].change, 5000 - 20383)
  assert.match(comps[0].summary, /fix the exporter/)
  assert.equal(reqs[3].flag, null) // writing the new context after a compaction is not a re-write
  assert.equal(reqs[3].change, 102)

  // entries sum to the session total, tokens and amount
  for (const k of ['in', 'cw', 'cr', 'out', 'amt']) assert.equal(reqs.reduce((a, e) => a + e[k], 0), L.totals[k], k)
  // amount: 2 input at $10, 20,000 one-hour cache write at $20, 100 output at $50, per million, in nano-dollars
  assert.equal(reqs[0].amt, (2 * 10 + 20000 * 20 + 100 * 50) * 1000)
})

test('rates: a newer model never borrows an older model’s price', () => {
  assert.ok(rateFor(RATES, 'claude-test-1'))
  assert.ok(rateFor(RATES, 'claude-test-1-20260101'))
  assert.equal(rateFor(RATES, 'claude-test-1-5'), null)
  assert.equal(accountOf(madeUpLog().map(r => (r.message && r.message.model ? { ...r, message: { ...r.message, model: 'claude-unknown-9' } } : r)), { rates: RATES }).totals.unpriced, 4)
  assert.deepEqual(split(10, [1, 1, 1]).reduce((a, b) => a + b, 0), 10)
})

test('a session reconciles, and a response in two files counts once in a period', () => {
  const s = sessionOf(madeUpLog(), [], { id: 'one', rates: RATES })
  assert.equal(s.control.out - s.totals.out, 50) // "Not itemized", shown as it is
  assert.equal(s.project, 'demo')
  const copy = rowsOf(sessionOf(madeUpLog(), [], { id: 'two', rates: RATES })) // a resumed session that copied its history
  const P = periodOf([rowsOf(s), copy], { from: 0, to: Date.UTC(2027, 0, 1), tz: 'UTC' }, RATES)
  assert.equal(P.n, 4)
  assert.equal(P.classes.out.tok, s.totals.out)
  assert.equal(P.flags.idle.n, 1)
  assert.equal(P.comps.manual, 1)
  assert.match(periodHtml(P, { tz: 'UTC', lede: 'test', statement: 'test', prepared: 0, rates: RATES }), /Period statement/)
})

test('pages escape log content and forbid scripts', () => {
  const html = sessionHtml(sessionOf(madeUpLog(), [], { id: 'one', rates: RATES }), { tz: 'UTC' })
  assert.ok(!html.includes('<script'))
  assert.ok(html.includes('&#60;script&#62;alert(1)'))
  assert.match(html, /default-src 'none'/)
  assert.ok(!sessionHtml(sessionOf(madeUpLog(), [], { id: 'one', rates: RATES, detail: false }), { tz: 'UTC', detail: false }).includes('alert(1)'))
})

test('log values show as fields, never as raw JSON', () => {
  const log = madeUpLog()
  log[2].message.content[0].input = { code: 'const a = 1\nawait a', description: 'Run <b>it</b>', timeout: 600000, opts: { all: true }, list: [{ k: 1 }, { k: 2 }] }
  const html = sessionHtml(sessionOf(log, [], { id: 'one', rates: RATES }), { tz: 'UTC' })
  for (const key of ['code', 'description', 'timeout', 'all', 'input_tokens']) assert.ok(html.includes('<dt>' + key + '</dt>'), key)
  assert.ok(!/&#34;(code|input_tokens)&#34;:/.test(html)) // no quoted JSON keys anywhere
  assert.ok(html.includes('<pre>const a = 1\nawait a</pre>')) // a multi-line string is a block
  assert.ok(html.includes('Run &#60;b&#62;it&#60;/b&#62;')) // still escaped
  assert.match(html, /<table class="jt">/) // a list of like records is a table
})

test('the pane gets the figures and the latest entries, newest first', () => {
  const b = sessionBrief(sessionOf(madeUpLog(), [], { id: 'one', rates: RATES }), { tz: 'UTC' })
  assert.deepEqual(b.rows.map(r => r.no), [...b.rows.map(r => r.no)].sort((x, y) => y - x))
  assert.match(b.amount, /^\$\d/)
  assert.ok(b.rows.some(r => r.flag.startsWith('Cache expired')))
})

test('the server answers only this machine, only GET, only with its token', async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'statement-test-'))
  process.env.TOKEN_ACCOUNTING_HOME = path.join(tmp, 'keep')
  fs.mkdirSync(path.join(tmp, 'logs', 'demo'), { recursive: true })
  fs.writeFileSync(path.join(tmp, 'logs', 'demo', 'abc123.jsonl'), madeUpLog().map(r => JSON.stringify(r)).join('\n') + '\n')
  const { serve } = await import('../bin/statement.mjs')
  const s = await serve({ root: path.join(tmp, 'logs'), quiet: true, tz: 'UTC' })
  const get = (p, opts = {}) => new Promise((resolve, reject) => {
    http.request({ host: '127.0.0.1', port: s.port, path: p, method: 'GET', ...opts }, res => {
      let body = ''
      res.on('data', d => (body += d)).on('end', () => resolve({ code: res.statusCode, headers: res.headers, body }))
    }).on('error', reject).end()
  })
  try {
    assert.equal((await get('/')).code, 404) // no token
    assert.equal((await get('/not-the-token/')).code, 404)
    assert.equal((await get('/' + s.token + '/', { headers: { host: 'evil.example' } })).code, 403)
    assert.equal((await get('/' + s.token + '/', { method: 'POST' })).code, 405)
    assert.equal((await get('/' + s.token + '/s/..%2F..%2Fetc')).code, 404) // an id is looked up, never joined into a path
    assert.equal((await get('/' + s.token + '/s/nope')).code, 404)
    const page = await get('/' + s.token + '/s/abc123')
    assert.equal(page.code, 200)
    assert.match(page.headers['content-security-policy'], /default-src 'none'/)
    assert.ok(page.body.includes('Find the bug in the exporter') && page.body.includes('&#60;script&#62;alert(1)'))
    // a served page carries one script, the live reload, allowed by its hash and nothing else
    const scripts = page.body.match(/<script>[\s\S]*?<\/script>/g) || []
    assert.equal(scripts.length, 1)
    const hash = crypto.createHash('sha256').update(scripts[0].slice(8, -9)).digest('base64')
    assert.ok(page.headers['content-security-policy'].includes("script-src 'sha256-" + hash + "'"))
    const again = { headers: { 'if-none-match': page.headers.etag } }
    assert.equal((await get('/' + s.token + '/s/abc123', again)).code, 304) // nothing new in the log
    fs.appendFileSync(path.join(tmp, 'logs', 'demo', 'abc123.jsonl'), '\n')
    assert.equal((await get('/' + s.token + '/s/abc123', again)).code, 200) // the log moved on
    const period = await get('/' + s.token + '/?since=4000d')
    assert.equal(period.code, 200)
    assert.ok(period.body.includes('href="s/abc123"'))
  } finally {
    s.close()
    fs.rmSync(tmp, { recursive: true, force: true })
  }
})

test('a session log path that does not exist is found by its id in the roots', async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'statement-test-'))
  process.env.TOKEN_ACCOUNTING_HOME = path.join(tmp, 'keep')
  fs.mkdirSync(path.join(tmp, 'logs', 'demo'), { recursive: true })
  fs.writeFileSync(path.join(tmp, 'logs', 'demo', 'abc123.jsonl'), madeUpLog().map(r => JSON.stringify(r)).join('\n') + '\n')
  const { main } = await import('../bin/statement.mjs')
  try {
    await main(['--session', path.join(tmp, 'elsewhere', 'abc123.jsonl'), '--root', path.join(tmp, 'logs'), '--brief', '--tz', 'UTC'])
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true })
  }
})
