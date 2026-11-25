// Pages and text for the token accounting statement. The markup follows its design canvas. Everything that
// comes from a log is escaped here before it goes on a page: log content is untrusted.
import { readFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { fileURLToPath } from 'node:url'
import { num, plural, rateFor, shortOf } from './account.mjs'

// No scripts, no outside requests. Fonts are embedded, so nothing is fetched.
export const CSP = "default-src 'none'; style-src 'unsafe-inline'; font-src data:"
// A page the server hands out follows its session while it runs. This script is the only one any
// page carries, allowed by its hash alone: it asks this same address again every two seconds,
// with the tag it has, and swaps in the new sheet when a log has changed, keeping open lines open.
const LIVE_JS = `(() => {
  let tag = document.body.dataset.tag
  const tick = async () => {
    if (!document.hidden) try {
      const r = await fetch(location.pathname, { headers: { 'If-None-Match': tag }, cache: 'no-store' })
      if (r.status === 200) {
        const next = new DOMParser().parseFromString(await r.text(), 'text/html').querySelector('.sheet')
        const open = [...document.querySelectorAll('details[open]')].map(d => d.id)
        document.querySelector('.sheet').replaceWith(next)
        for (const id of open) document.getElementById(id)?.setAttribute('open', '')
        tag = r.headers.get('ETag') || tag
      }
    } catch {}
    setTimeout(tick, 2000)
  }
  setTimeout(tick, 2000)
})()`
export const LIVE_CSP = CSP + "; script-src 'sha256-" + createHash('sha256').update(LIVE_JS).digest('base64') + "'; connect-src 'self'"
const SHOWN = 100000 // characters of one item shown on a page; the rest stays in the log

const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c => '&#' + c.charCodeAt(0) + ';')
const el = (tag, cls, inner, attrs) => '<' + tag + (cls ? ' class="' + cls + '"' : '') + (attrs || '') + '>' + inner + '</' + tag + '>'
const span = (cls, inner) => el('span', cls, inner)

export const usd = (nano, plus) =>
  nano == null ? '—' : (nano < 0 ? '−' : plus ? '+' : '') + '$' + (Math.abs(nano) / 1e9).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })
const signed = n => (n == null ? '—' : n > 0 ? '+' + num(n) : n < 0 ? '−' + num(-n) : '0')
const pct = (a, b) => {
  if (!b) return '—'
  const p = (100 * a) / b
  return a === 0 ? '0%' : p < 0.1 ? '<0.1%' : p < 10 ? p.toFixed(1) + '%' : Math.round(p) + '%'
}
const rateKnown = (rates, model) => !rates || !!rateFor(rates, model)
const dur = ms => {
  const m = Math.round(ms / 60000)
  return ms < 60000 ? Math.round(ms / 1000) + ' s' : m < 60 ? m + ' min' : Math.floor(m / 60) + ' h ' + (m % 60) + ' min'
}

function clock(tz) {
  const f = new Intl.DateTimeFormat('en-GB', { timeZone: tz, day: 'numeric', month: 'short', year: 'numeric', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' })
  const long = new Intl.DateTimeFormat('en-GB', { timeZone: tz, month: 'long' })
  const p = t => Object.fromEntries(f.formatToParts(new Date(t)).map(x => [x.type, x.value]))
  const c = {
    hm: t => p(t).hour + ':' + p(t).minute,
    dm: t => p(t).day.padStart(2, '0') + ' ' + p(t).month,
    dmy: t => p(t).day + ' ' + p(t).month + ' ' + p(t).year,
    stamp: t => c.dmy(t) + ', ' + c.hm(t),
    span: (a, b) => c.stamp(a) + ' to ' + (c.dmy(a) === c.dmy(b) ? c.hm(b) : c.stamp(b)),
    // "1 to 20 November 2026"
    range: (a, b) => {
      const x = p(a), y = p(b), mx = long.format(new Date(a)), my = long.format(new Date(b))
      if (x.year !== y.year) return x.day + ' ' + mx + ' ' + x.year + ' to ' + y.day + ' ' + my + ' ' + y.year
      if (mx !== my) return x.day + ' ' + mx + ' to ' + y.day + ' ' + my + ' ' + y.year
      return (x.day === y.day ? x.day : x.day + ' to ' + y.day) + ' ' + my + ' ' + y.year
    },
  }
  return c
}

export const rangeText = (a, b, tz) => clock(tz).range(a, b)

// ---- the sheet ------------------------------------------------------------------------------

let fontCss
function fonts() {
  if (fontCss != null) return fontCss
  const dir = fileURLToPath(new URL('../fonts/', import.meta.url))
  const face = (family, weight, file) => {
    try {
      return "@font-face{font-family:'" + family + "';font-style:normal;font-weight:" + weight + ';src:url(data:font/woff2;base64,' + readFileSync(dir + file).toString('base64') + ") format('woff2')}"
    } catch {
      return '' // no font file: the page falls back to system faces
    }
  }
  fontCss = [
    face('IBM Plex Sans', '100 700', 'IBMPlexSans.woff2'),
    face('IBM Plex Mono', 400, 'IBMPlexMono-400.woff2'),
    face('IBM Plex Mono', 500, 'IBMPlexMono-500.woff2'),
    face('IBM Plex Mono', 600, 'IBMPlexMono-600.woff2'),
    face('IBM Plex Serif', 600, 'IBMPlexSerif-600.woff2'),
  ].join('\n')
  return fontCss
}

const MONO = "font-family:'IBM Plex Mono',ui-monospace,Menlo,monospace"
const SERIF = "font-family:'IBM Plex Serif',Georgia,serif"
const GRID = 'display:grid;grid-template-columns:40px 52px minmax(240px,1fr) 104px 96px 64px 92px 96px 76px 76px;column-gap:12px'
const CSS = [
  "body{margin:0;font-family:'IBM Plex Sans',system-ui,sans-serif;color:#15201B;background:#E9ECEA}",
  'a{color:#1D4F91}a:hover{color:#123766}',
  'a:focus-visible,summary:focus-visible{outline:2px solid #1D4F91;outline-offset:2px}',
  'summary{list-style:none}summary::-webkit-details-marker{display:none}',
  'details[open]>summary .chev{transform:rotate(90deg)}',
  '.desk{background-color:#E9ECEA;padding:clamp(12px,3vw,48px);box-sizing:border-box}',
  '.sheet{max-width:1240px;margin:0 auto;background-color:#FCFDFC;border:1px solid #C5CFC8;padding:clamp(20px,4.5vw,64px);box-sizing:border-box;display:flex;flex-direction:column;gap:56px}',
  '.back{margin-bottom:-32px;display:flex;flex-wrap:wrap;align-items:center;gap:0 24px}.live{font-size:14px;color:#4F5B54}.back a{display:inline-flex;align-items:center;gap:8px;min-height:44px;font-size:14px;font-weight:500;text-underline-offset:3px}',
  '.mast{display:flex;flex-wrap:wrap;justify-content:space-between;align-items:flex-end;gap:24px 48px;border-bottom:3px double #15201B;padding-bottom:24px}',
  '.mast>div{display:flex;flex-direction:column;gap:10px;min-width:0}',
  '.over{' + MONO + ';font-weight:600;font-size:12px;line-height:1;letter-spacing:.14em;text-transform:uppercase;color:#45524B}',
  'h1{margin:0;' + SERIF + ';font-weight:600;font-size:clamp(32px,5vw,48px);line-height:1.05;letter-spacing:-.01em}',
  '.lede{margin:0;font-size:17px;line-height:1.4;color:#45524B;overflow-wrap:anywhere}',
  '.meta{margin:0;display:grid;grid-template-columns:auto auto;column-gap:24px;row-gap:6px;font-size:13.5px;line-height:1.4}',
  '.meta dt{color:#4F5B54}.meta dd{margin:0;text-align:right;' + MONO + '}',
  '.figs{margin:0;display:flex;flex-wrap:wrap;gap:24px 32px}',
  '.figs>div{flex:1 1 150px;min-width:0;display:flex;flex-direction:column;gap:6px;border-top:2px solid #15201B;padding-top:10px}',
  '.figs dt{font-size:12px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:#45524B}',
  '.figs .v{margin:0;' + MONO + ';font-size:24px;line-height:1.2;font-weight:500}',
  '.note{margin:0;font-size:12.5px;line-height:1.45;color:#4F5B54}',
  '.sec{display:flex;flex-direction:column;gap:16px}.head{display:flex;flex-direction:column;gap:4px}',
  'h2{margin:0;' + SERIF + ';font-weight:600;font-size:24px;line-height:1.2}',
  '.desc{margin:0;font-size:14px;line-height:1.5;color:#45524B;max-width:68ch}',
  '.h3{margin:0;font-size:15px;font-weight:600;font-family:inherit;line-height:normal}',
  '.scroll{overflow-x:auto}',
  '.pair{display:flex;flex-wrap:wrap;gap:40px;align-items:flex-start}.pair>.wide{flex:999 1 560px;min-width:0}',
  '.cols{display:flex;flex-wrap:wrap;gap:40px}.cols>div{flex:1 1 620px;min-width:0;display:flex;flex-direction:column;gap:12px}',
  '.small{margin:0;font-size:13px;line-height:1.5;color:#45524B}',
  'table{width:100%;border-collapse:collapse}.w560{min-width:560px}.w640{min-width:640px}.w1040{min-width:1040px}',
  "th{text-align:right;font-family:'IBM Plex Sans',system-ui,sans-serif;font-weight:600;font-size:12px;line-height:1.3;letter-spacing:.06em;text-transform:uppercase;color:#45524B;padding:10px 0 10px 12px;border-top:2px solid #15201B;border-bottom:1px solid #15201B;vertical-align:bottom}",
  'th.l{text-align:left;padding:10px 12px 10px 0}th.date{width:72px}',
  'td{text-align:left;font-size:14px;line-height:1.45;font-weight:400;padding:11px 12px 11px 0;vertical-align:top;border-bottom:1px solid #C5CFC8}',
  'td.n{text-align:right;' + MONO + ';font-size:13.5px;color:#15201B;padding:11px 0 11px 12px;white-space:nowrap}',
  'td.d{' + MONO + ';font-size:13.5px;white-space:nowrap}',
  'td a{font-weight:500;text-underline-offset:3px}',
  'tr.z{background-color:#EDF4EE}tr.w{background-color:#FCFDFC}',
  'tr.sum td{border-top:1px solid #15201B;border-bottom:3px double #15201B;font-weight:600}',
  '.gl{display:block;font-size:12.5px;line-height:1.45;color:#4F5B54;font-weight:400}',
  'td.or,.n.or{color:#A84B00}td.bl,.n.bl,.ti.bl{color:#1D4F91}',
  '.keys{margin:0;flex:1 1 280px;min-width:0;border-top:2px solid #15201B}',
  '.keys>div{display:flex;justify-content:space-between;align-items:baseline;gap:16px;padding:10px 0;border-bottom:1px solid #C5CFC8}',
  '.keys dt{font-size:14px;color:#45524B}.keys dd{margin:0;text-align:right;' + MONO + ';font-size:14px;font-weight:500}',
  ".keys dd span{display:block;font-family:'IBM Plex Sans',system-ui,sans-serif;font-size:12.5px;font-weight:400;color:#4F5B54}",
  '.how{border-top:3px double #15201B;padding-top:24px;display:flex;flex-direction:column;gap:16px}',
  '.how dl{margin:0;display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:20px 40px}',
  '.how dl>div{display:flex;flex-direction:column;gap:4px}.how dt{font-size:14px;font-weight:600}.how dd{margin:0;font-size:14px;line-height:1.5;color:#45524B}',
  // the entries
  '.led{min-width:1044px}',
  '.lh{' + GRID + ';padding:10px 0;border-top:2px solid #15201B;border-bottom:1px solid #15201B}',
  '.lh span{text-align:right;font-weight:600;font-size:12px;line-height:1.3;letter-spacing:.06em;text-transform:uppercase;color:#45524B;align-self:end}',
  '.lh span:nth-child(-n+3){text-align:left}.lh span:last-child{padding-right:6px}',
  '.e{border-bottom:1px solid #C5CFC8;background-color:#FCFDFC}.e.z{background-color:#EDF4EE}.e.c{background-color:#E8EEF7}',
  '.row{' + GRID + ';align-items:start;padding:12px 0;min-height:44px;box-sizing:border-box}summary.row{cursor:pointer}',
  '.no{display:flex;align-items:center;gap:6px;padding-left:6px;' + MONO + ';font-size:13.5px;line-height:1.45;color:#45524B}',
  '.chev{flex:none;transition:transform 120ms}',
  '.t{' + MONO + ';font-size:13.5px;line-height:1.45}',
  '.en{display:flex;flex-direction:column;gap:2px;min-width:0;overflow-wrap:anywhere}',
  '.ti{font-size:14px;line-height:1.45;font-weight:600}.su{font-size:12.5px;line-height:1.45;color:#4F5B54}',
  '.fl{font-size:12.5px;line-height:1.45;font-weight:500;color:#A84B00}',
  '.n{text-align:right;' + MONO + ';font-size:13.5px;line-height:1.45;white-space:nowrap;font-weight:400;color:#15201B}',
  '.n.b{font-weight:600}.row>.n:last-child{padding-right:6px}',
  '.tot{' + GRID + ';align-items:start;padding:12px 0;border-bottom:1px solid #C5CFC8}',
  '.tot.dbl{border-top:1px solid #15201B;border-bottom:3px double #15201B}.tot>.n:last-child{padding-right:6px}',
  '.panel{padding:8px 24px 28px 116px;max-width:980px;box-sizing:border-box;display:flex;flex-direction:column;gap:24px}',
  '.blk{display:flex;flex-direction:column;gap:12px}',
  '.bh{display:flex;justify-content:space-between;align-items:baseline;gap:16px;border-bottom:1px solid #15201B;padding-bottom:6px}',
  '.bh h3,.bh h4{margin:0;font-size:12px;font-weight:600;letter-spacing:.08em;text-transform:uppercase}',
  '.bh span{' + MONO + ';font-size:13.5px;font-weight:600;white-space:nowrap}',
  '.it{display:flex;flex-direction:column;gap:8px}',
  '.ih{display:flex;flex-wrap:wrap;justify-content:space-between;align-items:baseline;gap:4px 16px}',
  '.il{font-size:14px;line-height:1.45;overflow-wrap:anywhere;min-width:0}.il b{font-weight:500}',
  '.il i{font-style:normal;' + MONO + ';font-size:13px;color:#45524B}',
  '.ir{' + MONO + ';font-size:13px;white-space:nowrap;color:#45524B}.ir b{font-weight:400;color:#15201B}',
  'pre{margin:0;padding:12px 14px;background-color:#F4F6F4;border:1px solid #C5CFC8;' + MONO + ';font-size:12.5px;line-height:1.55;white-space:pre-wrap;overflow-wrap:anywhere;max-height:220px;overflow:auto}',
  // log values shown as fields
  '.kv{margin:0;display:grid;grid-template-columns:minmax(64px,max-content) minmax(0,1fr);gap:8px 16px;padding:12px 14px;background-color:#F4F6F4;border:1px solid #C5CFC8;' + MONO + ';font-size:12.5px;line-height:1.55}',
  '.kv dt{color:#45524B;overflow-wrap:anywhere}.kv dd{margin:0;min-width:0;overflow-wrap:anywhere}',
  '.kv .kv{padding:0 0 0 12px;background:none;border:0;border-left:2px solid #C5CFC8}.kv pre{background-color:#FCFDFC;max-height:320px}',
  '.lit{color:#1D4F91}.nul{color:#4F5B54;font-style:italic}',
  '.arr{margin:0;padding-left:28px;display:flex;flex-direction:column;gap:8px;' + MONO + ';font-size:12.5px;line-height:1.55}',
  '.jt td,.jt th{padding:6px 12px 6px 0;font-size:12.5px}.jt td{' + MONO + '}',
  '.p{margin:0;font-size:14px;line-height:1.5;color:#45524B}.p b{font-weight:400;' + MONO + ';color:#15201B}',
  '.stack{display:flex;flex-direction:column;gap:12px}.fold>summary{display:flex;align-items:center;gap:8px;cursor:pointer;min-height:44px}.fold[open]>summary{margin-bottom:4px}',
  '@media print{.desk{padding:0;background:#fff}.sheet{border:0;max-width:none;padding:0}.back{display:none}.scroll{overflow:visible}pre{max-height:none;overflow:visible}.row{break-inside:avoid}}',
].join('\n')

const CHEV = '<svg class="chev" aria-hidden="true" width="10" height="10" viewBox="0 0 10 10"><path d="M3 1.5 L7 5 L3 8.5" fill="none" stroke="#45524B" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>'
const LEFT = '<svg aria-hidden="true" width="10" height="10" viewBox="0 0 10 10"><path d="M7 1.5 L3 5 L7 8.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>'

// tag: the page's live tag, when the server hands it out live. Then it carries its one script.
const page = (title, body, tag) =>
  '<!doctype html>\n<html lang="en">\n<head>\n<meta charset="utf-8">\n<meta name="viewport" content="width=device-width, initial-scale=1">\n' +
  '<meta http-equiv="Content-Security-Policy" content="' + (tag ? LIVE_CSP : CSP) + '">\n<meta name="referrer" content="no-referrer">\n' +
  '<title>' + esc(title) + '</title>\n<style>\n' + fonts() + '\n' + CSS + '\n</style>\n</head>\n<body' + (tag ? ' data-tag="' + esc(tag) + '"' : '') + '>\n<div class="desk">\n<div class="sheet">\n' + body + '\n</div>\n</div>\n' + (tag ? '<script>' + LIVE_JS + '</script>\n' : '') + '</body>\n</html>\n'

const masthead = (over, lede, rows) =>
  '<header class="mast">\n<div>\n' + el('div', 'over', esc(over)) + '\n<h1>Token Accounting Statement</h1>\n' + el('p', 'lede', esc(lede)) + '\n</div>\n' +
  el('dl', 'meta', '\n' + rows.map(([k, v]) => '<dt>' + esc(k) + '</dt><dd>' + esc(v) + '</dd>').join('\n') + '\n') + '\n</header>'

const section = (title, desc, inner) => '<section class="sec">\n<div class="head">\n<h2>' + esc(title) + '</h2>\n' + el('p', 'desc', desc) + '\n</div>\n' + inner + '\n</section>'

// heads: [label, 'l' for a text column]. rows: arrays of cells; a cell is html, or [html, class].
function table(width, heads, rows, foot) {
  const cell = (c, i) => {
    const [html, cls] = Array.isArray(c) ? c : [c, heads[i][1] === 'l' ? '' : 'n']
    return '<td' + (cls ? ' class="' + cls + '"' : '') + '>' + html + '</td>'
  }
  const tr = (cells, cls) => '<tr' + (cls ? ' class="' + cls + '"' : '') + '>' + cells.map(cell).join('') + '</tr>'
  let k = 0
  const body = rows.map(r => (r.tot ? tr(r.cells, 'sum') : tr(r.cells || r, k++ % 2 ? 'z' : 'w')))
  if (foot) body.push(tr(foot, 'sum'))
  return '<div class="scroll">\n<table class="' + width + '">\n<thead><tr>' +
    heads.map(([h, c]) => '<th scope="col"' + (c ? ' class="' + (c === 'l' ? 'l' : 'l ' + c) + '"' : '') + '>' + esc(h) + '</th>').join('') +
    '</tr></thead>\n<tbody>\n' + body.join('\n') + '\n</tbody>\n</table>\n</div>'
}
const gloss = (name, sub) => esc(name) + (sub ? span('gl', esc(sub)) : '')

const howTo = (extra, asOf) =>
  '<section class="how">\n<h2 class="h3">How to read this statement</h2>\n<dl>\n' +
  [
    ['Every request sends the whole conversation again', 'Cache read is that re-sent part. It is most of the tokens and the cheapest class.'],
    ['Context is the size of the conversation', 'It rises as results and replies are added, and falls at a compaction.'],
    ['Amounts are a yardstick, not a bill', 'They are what these tokens cost at API list prices' + (asOf ? ' (rates table of ' + asOf + ')' : '') + '. A subscription is not charged per token.'],
    ['≈ marks an estimate', 'Totals come from the API’s own counts and are exact. The split between causes is estimated.'],
    ['Lines are rounded to the cent', 'Totals are added up before rounding, so a column of lines can differ from its total by a cent or two.'],
    ...extra,
  ].map(([a, b]) => '<div><dt>' + esc(a) + '</dt><dd>' + esc(b) + '</dd></div>').join('\n') +
  '\n</dl>\n</section>'

// rule 10: our figures against Claude Code's own, difference shown as it is
function reconTable(ours, theirs) {
  const row = (name, a, b) => [esc(name), num(a), num(b), signed(b - a).replace('+', '')]
  return table('w640', [['Class', 'l'], ['Itemized here'], ['Claude Code’s own total'], ['Not itemized']],
    [row('New input', ours.in, theirs.in), row('Cache write', ours.cw, theirs.cw), row('Cache read', ours.cr, theirs.cr), row('Output', ours.out, theirs.out)],
    ['Amount', usd(ours.amt), usd(theirs.usd * 1e9), usd(Math.round(theirs.usd * 1e9) - ours.amt)])
}

// ---- session statement ----------------------------------------------------------------------

function pre(text, line) {
  if (!text) return ''
  const cut = text.length > SHOWN
  const shown = cut ? text.slice(0, SHOWN) : text
  const more = cut
    ? 'Showing the first ' + num(SHOWN) + ' of ' + num(text.length) + ' characters. The rest is in the session log' + (line ? ', line ' + num(line) : '') + '.'
    : shown.length > 800 || shown.split('\n').length > 10 ? 'Scrolls. ' + num(text.length) + ' characters in all.' : ''
  return '<pre>' + esc(shown) + '</pre>' + (more ? el('p', 'note', more) : '')
}
// A value from a log as a developer reads it, never as raw JSON: an object as named fields, a list of
// like records as a table, other lists as numbered items, a long or multi-line string as a block.
const isObj = v => v !== null && typeof v === 'object'
const asJson = s => {
  if (!/^\s*[[{]/.test(s)) return undefined
  try {
    const v = JSON.parse(s)
    return isObj(v) ? v : undefined
  } catch {
    return undefined
  }
}
const short = x => !isObj(x) && String(x).length <= 120 && !String(x).includes('\n')
function jsonView(v) {
  if (typeof v === 'string') {
    const j = asJson(v)
    if (j) return jsonView(j)
    return v === '' ? span('nul', 'empty') : short(v) ? span('sv', esc(v)) : '<pre>' + esc(v) + '</pre>'
  }
  if (!isObj(v)) return span(v === null ? 'nul' : 'lit', esc(String(v)))
  if (!Array.isArray(v)) {
    const kv = Object.entries(v)
    return kv.length ? el('dl', 'kv', kv.map(([k, x]) => '<dt>' + esc(k) + '</dt><dd>' + jsonView(x) + '</dd>').join('')) : span('nul', 'empty')
  }
  if (!v.length) return span('nul', 'empty')
  if (v.length > 1 && v.every(x => isObj(x) && !Array.isArray(x) && Object.values(x).every(short))) {
    const keys = [...new Set(v.flatMap(Object.keys))]
    if (keys.length <= 8) return table('jt', keys.map(k => [k, 'l']), v.map(x => keys.map(k => (k in x ? jsonView(x[k]) : ''))))
  }
  return el('ol', 'arr', v.map(x => '<li>' + jsonView(x) + '</li>').join(''))
}
// Text from a log: JSON shown as fields, other text as it is. A tool result can mix text with other
// content blocks, one block per line; each block line is shown as fields too.
function body(text, line) {
  if (!text || text.length > SHOWN) return pre(text, line)
  const j = asJson(text)
  if (j) return jsonView(j)
  if (!/(^|\n)\{"/.test(text)) return pre(text, line)
  const out = [], buf = []
  const flush = () => buf.length && out.push(pre(buf.splice(0).join('\n'), line))
  for (const l of text.split('\n')) {
    const x = l.startsWith('{"') ? asJson(l) : undefined
    if (x) flush(), out.push(jsonView(x))
    else buf.push(l)
  }
  flush()
  return out.join('\n')
}
const block = (title, right, inner) => '<div class="blk"><div class="bh"><h3>' + esc(title) + '</h3><span>' + esc(right) + '</span></div>\n' + inner + '</div>'
const itemHtml = (name, label, right, body) =>
  '<div class="it">\n<div class="ih">\n<span class="il"><b>' + esc(name) + '</b>' + (label ? '<i> · ' + esc(label) + '</i>' : '') + '</span>\n<span class="ir">' + right + '</span>\n</div>\n' + body + '\n</div>'
const plain = html => el('p', 'p', html)
const fig = n => '<b>' + n + '</b>'

function requestPanel(e, th, o) {
  const sent = []
  if (e.carried) sent.push(itemHtml('The model’s own previous reply', 'carried forward', fig((e.carriedEst ? '≈' : '') + num(e.carried)), ''))
  for (const x of e.items) {
    const right = (x.chars ? plural(x.chars, 'character') + ' · ' : '') + fig('≈' + num(x.tok))
    sent.push(itemHtml(x.name, x.label, right, body(x.text, x.line) + (x.note ? el('p', 'note', esc(x.note)) : '')))
  }
  if (!sent.length) sent.push(plain('Nothing new is recorded between this request and the one before.'))
  if (e.trimmed) sent.push(plain('Context fell by ' + fig(num(-e.change)) + ' tokens with no compaction recorded: older content was trimmed by context management.'))

  const from = e.no - 1 > e.since ? 'entries ' + e.since + ' to ' + (e.no - 1) : 'entry ' + e.since
  let again
  if (e.first) again = 'Nothing. This is the first request of ' + (o.sub ? 'this subagent' : 'the session') + '.' + (e.cr ? ' ' + fig(num(e.cr)) + ' tokens of it were already in the cache from earlier work, and were read from there.' : '')
  else if (e.afterComp) again = fig(num(e.prevCtx)) + ' tokens: the summary from the compaction at entry ' + e.since + ', with the system prompt and tools. ' + fig(num(e.cr)) + ' of it read from cache.'
  else if (e.flag) again = fig(num(e.prevCtx)) + ' tokens from ' + from + '. ' + (e.cr ? 'Only ' + fig(num(e.cr)) + ' of it was read from cache' : 'None of it was read from cache') + ': ' + fig(num(e.flag.again)) + ' tokens were written again.'
  else again = fig(num(e.prevCtx)) + ' tokens from ' + from + ', of which ' + fig(num(e.cr)) + ' read from cache.'

  const got = []
  const thinking = e.blocks.filter(b => b.type === 'thinking')
  let thought = false
  for (const b of e.blocks) {
    if (b.type === 'thinking') {
      if (thought) continue
      thought = true
      const texts = thinking.filter(x => x.text)
      got.push(itemHtml('Thinking', '', fig(''), texts.map(x => body(x.text, e.line)).join('\n')))
      got.push(plain(num(e.think) + ' tokens.' + (texts.length < thinking.length ? ' Text not recorded in the log.' : '')))
    } else if (b.type === 'text') got.push(itemHtml('Text', '', fig(''), body(b.text, e.line)))
    else if (b.type === 'tool') got.push(itemHtml('Tool call', b.name + (b.arg ? ' · ' + b.arg : ''), fig(''), body(b.text, e.line)))
    else got.push(itemHtml(b.name, '', fig(''), body(b.text, e.line)))
  }
  if (!got.length) got.push(plain('No content recorded.'))

  const subs = e.items.filter(x => x.sub != null).map(x => {
    const s = o.subs[x.sub], t = s.totals
    return block('The subagent’s own account', usd(t.amt),
      plain(plural(t.n, 'request') + '. New input ' + fig(num(t.in)) + ', cache write ' + fig(num(t.cw)) + ', cache read ' + fig(num(t.cr)) + ', output ' + fig(num(t.out)) +
        '. <a href="#agent' + x.sub + '">Its account is further down this page</a>; its entries open the same way as these.'))
  })

  const life = e.ttl === '1h' || (!e.ttl && e.cw1) ? '1 hour' : e.ttl === '5m' || (!e.ttl && e.cw5) ? '5 minutes' : ''
  const facts = [
    e.model, 'stop: ' + (e.stop || 'not recorded'), life && 'cache lifetime ' + life, e.effort && 'effort ' + e.effort, e.speed !== 'standard' && e.speed + ' mode',
    e.geo !== 'global' && 'region ' + e.geo, e.web && plural(e.web, 'web search', 'web searches'), e.skill && 'skill ' + e.skill, e.plugin && 'plugin ' + e.plugin,
    e.mcp && 'MCP server ' + e.mcp, e.source, e.requestId, 'log line ' + num(e.line),
  ].filter(Boolean)
  return [
    block('Sent, new in this request', (e.change < 0 ? '−' + num(-e.change) : num(e.change)) + ' tokens', sent.join('\n')),
    block('Sent again, already in context', num(e.prevCtx) + ' tokens', plain(again)),
    block('Received', num(e.out) + ' tokens', got.join('\n')),
    ...subs,
    block('As the API reported it', '', plain(esc(facts.join(' · '))) + '\n' + jsonView(e.usage || {})),
  ].join('\n')
}

function compactionPanel(e) {
  const how = e.trigger === 'manual' ? 'Started by /compact' : 'Started automatically'
  const when = e.gapMs != null ? ', ' + dur(e.gapMs) + ' after the last request' : ''
  const took = e.ms ? ' The summary took ' + dur(e.ms) + ' to write.' : ''
  return [
    block('What changed', (e.change == null ? '—' : signed(e.change)) + ' tokens',
      plain('Context before: ' + fig(e.pre == null ? 'not recorded' : num(e.pre)) + '. Context after: ' + fig(e.post == null ? 'not recorded' : num(e.post)) + '. ' + how + when + '.' + took)),
    block('Summary that replaced the history', e.post == null ? '' : num(e.post) + ' tokens with system prompt and tools',
      e.summary ? body(e.summary, e.summaryLine) : plain('Not recorded in this log.')),
    block('Cost of the compaction itself', '', plain('The request that wrote this summary is not itemized in the log. It is part of “Not itemized” in the reconciliation below.')),
  ].join('\n')
}

function flagText(e) {
  const f = e.flag
  const why = f.code === 'idle' ? 'Cache expired after ' + dur(f.gapMs) + ' idle.'
    : f.code === 'model' ? 'Model changed from ' + f.from + ', so the cache could not be used.'
    : f.code === 'shape' ? 'The system prompt or tool list changed, so the cache could not be used.'
    : f.code === 'speed' ? 'Fast mode was switched on or off, so the cache could not be used.'
    : f.code === 'effort' ? 'The effort or thinking setting changed, so the cache could not be used.'
    : 'The cache was not used; the log does not say why.'
  const cost = f.wrote != null ? ': ' + usd(f.wrote) + ', against ' + usd(f.read) + ' read from cache.' : '.'
  return why + ' ' + num(f.again) + ' tokens of existing context written again' + cost
}

const ENTRIES_HEAD = '<div class="lh">\n' + ['No.', 'Time', 'Entry', 'Context after', 'Change', 'New input', 'Cache write', 'Cache read', 'Output', 'Amount'].map(h => '<span>' + h + '</span>').join('') + '\n</div>'

// One thread's entries: a header row, one line per entry, and nothing else.
function entriesHtml(th, o) {
  const c = o.clock
  const rows = th.entries.map((e, i) => {
    const comp = e.kind === 'compaction'
    const cells = [
      span('no', (o.detail ? CHEV : '') + e.no),
      span('t', c.hm(e.t)),
      comp
        ? span('en', '\n' + span('ti bl', 'Compaction') + '\n' + span('su', (e.trigger === 'manual' ? 'Manual, /compact.' : 'Automatic.') + ' History replaced by a summary.') + '\n')
        : span('en', '\n' + span('ti', esc(e.title)) + '\n' + span('su', esc(e.sub)) + '\n' + (e.flag ? span('fl', esc(flagText(e))) + '\n' : '')),
      span('n b', e.ctx == null ? '—' : num(e.ctx)),
      span(comp ? 'n b bl' : 'n', signed(e.change)),
      span('n', comp ? '' : num(e.in)),
      span(e.flag ? 'n b or' : 'n', comp ? '' : num(e.cw)),
      span('n', comp ? '' : num(e.cr)),
      span('n', comp ? '' : num(e.out)),
      span('n', comp ? '' : usd(e.amt)),
    ].join('\n')
    const cls = 'e' + (comp ? ' c' : i % 2 ? ' z' : ''), id = ' id="' + o.prefix + e.no + '"'
    if (!o.detail) return '<div class="' + cls + '"' + id + '>\n<div class="row">\n' + cells + '\n</div>\n</div>'
    return '<details class="' + cls + '"' + id + '>\n<summary class="row">\n' + cells + '\n</summary>\n<div class="panel">\n' + (comp ? compactionPanel(e) : requestPanel(e, th, o)) + '\n</div>\n</details>'
  })
  return ENTRIES_HEAD + '\n' + rows.join('\n')
}

const totalRow = (name, sub, t, dbl) =>
  '<div class="tot' + (dbl ? ' dbl' : '') + '">\n<span></span><span></span>\n' + span('en', span('ti', esc(name)) + span('su', esc(sub))) + '\n<span></span><span></span>\n' +
  [num(t.in), num(t.cw), num(t.cr), num(t.out), usd(t.amt)].map(v => span(dbl ? 'n b' : 'n', v)).join('') + '\n</div>'

// s: a session from sessionOf(). o: { tz, detail, back: { href, label }, prepared, live: the page's tag when served live }
export function sessionHtml(s, o = {}) {
  const c = clock(o.tz), detail = o.detail !== false, m = s.main, t = s.totals
  const lo = { clock: c, detail, subs: s.subs }
  const peakNext = m.entries[m.peak.no] // entries are numbered from 1, so this is the one after the peak
  const lastIsComp = m.entries.length && m.entries[m.entries.length - 1].kind === 'compaction'
  const figs = [
    ['Opening balance', num(m.opening), 'Context at the first request'],
    ['Peak', num(m.peak.ctx), m.peak.no ? 'Entry ' + m.peak.no + (peakNext && peakNext.kind === 'compaction' ? ', before the compaction' : '') : 'No requests'],
    ['Closing balance', num(m.closing), lastIsComp ? 'Context after the last compaction' : 'Context at the last request'],
    ['Requests', num(t.n), s.subs.length ? num(m.totals.n) + ' main thread, ' + num(s.subTotals.n) + ' in subagents' : 'Main thread only'],
    ['Amount', usd(t.amt), 'API-equivalent, USD' + (t.unpriced ? '. ' + plural(t.unpriced, 'request') + ' on a model with no rate not priced' : '')],
    ['Cache hit rate', esc(pct(t.cr, t.cr + t.cw + t.in)), cacheNote(t.saved - t.premium)],
  ]
  const model = m.models.length ? m.models[0] + (m.models.length > 1 ? ' and ' + (m.models.length - 1) + ' more' : '') : '—'
  const parts = []
  if (o.back) parts.push('<nav class="back"><a href="' + esc(o.back.href) + '">' + LEFT + esc(o.back.label) + '</a>' + (o.live ? '<span class="live">Live: this page follows the session as it runs</span>' : '') + '</nav>')
  parts.push(masthead('Session statement', s.title, [
    ['Session', s.id.slice(0, 8)], ['Project', s.project || '—'],
    ['Date', s.first ? c.span(s.first, m.last || s.first) : '—'], ['Model', model],
  ]))
  parts.push('<dl class="figs">\n' + figs.map(([k, v, n]) => '<div><dt>' + k + '</dt><dd class="v">' + v + '</dd><dd class="note">' + esc(n) + '</dd></div>').join('\n') + '\n</dl>')

  parts.push(section('Entries',
    'One line per request, in order. Context after is the running balance. ' + (detail ? 'Open any line to see what was sent and what came back.' : 'This copy holds figures and labels only.'),
    '<div class="scroll">\n<div class="led">\n' + entriesHtml(m, { ...lo, prefix: 'e' }) + '\n' +
      totalRow('Main thread', plural(m.totals.n, 'request'), m.totals) + '\n' +
      (s.subs.length ? totalRow('Subagents', plural(s.subTotals.n, 'request') + ' in ' + plural(s.subs.length, 'subagent'), s.subTotals) + '\n' : '') +
      totalRow('Session total', plural(t.n, 'request'), t, true) + '\n</div>\n</div>'))

  if (s.subs.length)
    parts.push(section('Subagent accounts', 'Each subagent holds a conversation of its own, so each has its own running balance. Their totals are in the session total above.',
      s.subs.map((a, k) => {
        const p = a.parent, at = p ? (p.thread === -1 ? '<a href="#e' + p.no + '">entry ' + p.no + '</a>' : 'entry ' + p.no + ' of <a href="#agent' + p.thread + '">another subagent</a>') : ''
        return '<div class="head" id="agent' + k + '">\n<h3 class="h3">' + esc(a.type) + (a.desc ? ' · “' + esc(a.desc) + '”' : '') + '</h3>\n' +
          el('p', 'desc', plural(a.totals.n, 'request') + ', ' + usd(a.totals.amt) + (at ? '. Reported back at ' + at : '') + '.') + '\n</div>\n' +
          '<div class="scroll">\n<div class="led">\n' + entriesHtml(a, { ...lo, prefix: 'a' + k + 'e', sub: true }) + '\n' + totalRow('Subagent total', plural(a.totals.n, 'request'), a.totals, true) + '\n</div>\n</div>'
      }).join('\n')))

  const k = s.control
  const neg = k && shortOf(t, k)
  parts.push(section('Reconciliation',
    k ? 'This statement against the total Claude Code recorded for the session. “Not itemized” is spend the log does not break down: requests the engine makes for itself, such as session titles and compaction summaries.' +
        (neg ? ' Here Claude Code’s own total is lower than what this log itemizes, shown with a minus sign, so it is not a full figure for this session: its counter can miss part of a resumed session, and the work of background workflow agents. The itemized figures are the fuller count.' : '')
      : 'No control total recorded in this log, so this statement cannot be checked against Claude Code’s own figure.',
    k ? reconTable(t, k) : ''))
  parts.push(howTo(detail ? [['Content is shown as the log recorded it', 'The exact bytes sent can differ slightly, because the engine adds wrappers around some records.']] : [], o.asOf))
  return page('Token Accounting Statement: ' + s.title, parts.join('\n'), o.live)
}

// ---- period statement -----------------------------------------------------------------------

const CAUSES = [
  ['tool', 'Tool results', ''],
  ['reply', 'The model’s own replies', 'Each reply is sent back with the next request'],
  ['system', 'System prompt, tools and instructions', 'The opening balance of every session, and again after a compaction'],
  ['hook', 'Hook output', 'Added by hooks at start-up, on prompts and after tool calls'],
  ['subagent', 'Subagent reports', 'What subagents handed back'],
  ['prompt', 'Prompts', 'What was typed, and the briefs given to subagents'],
  ['other', 'Other injected context', 'Reminders, file-change notices, skill and tool listings'],
]
const EVENTS = [ // the first three always show; the rest only when they happened
  ['idle', 'Cache expired while idle'],
  ['shape', 'Tool list or system prompt changed'],
  ['model', 'Model changed mid-session'],
  ['speed', 'Fast mode switched on or off'],
  ['effort', 'Effort or thinking setting changed'],
  ['unexplained', 'Cache not used, reason not recorded'],
]
const cacheNote = net => (net >= 0 ? 'The cache saved ' + usd(net) + ', net of what writing to it cost' : 'Writing to the cache cost ' + usd(-net) + ' more than reading from it saved')
const fold = (title, hint, inner) => '<details class="fold">\n<summary>' + CHEV + '<span class="h3">' + esc(title) + '</span><span class="note">' + esc(hint) + '</span></summary>\n' + inner + '\n</details>'

// P: a period from periodOf(). o: { tz, lede, statement, prepared, link: id => href or null }
export function periodHtml(P, o = {}) {
  const c = clock(o.tz), K = P.classes
  const share = (a, b) => esc(pct(a, b))
  const parts = [masthead('Period statement', 'Statement of token use, ' + o.lede, [
    ['Statement', o.statement], ...(P.project ? [['Project', P.project]] : []), ['Account', 'Claude Code, this machine'],
    ['Prepared', c.stamp(o.prepared)], ['Amounts', 'API-equivalent, USD'],
  ])]

  // Summary
  const big = [...P.sessions].sort((a, b) => b.peak - a.peak)[0]
  const rewrites = Object.values(P.flags).reduce((a, f) => a + f.n, 0)
  const classRows = [
    ['New input', 'Sent for the first time, not stored', K.in],
    ['Cache write', 'Sent for the first time and stored for re-use', K.cw],
    ['Cache read', 'Context sent again, read back from the store', K.cr],
    ['Output', 'What the model wrote back' + (P.think ? '. ' + num(P.think) + ' of it thinking' : ''), K.out],
  ].map(([n, g, x]) => [gloss(n, g), num(x.tok), share(x.tok, P.tok), usd(x.amt), share(x.amt, P.amt)])
  if (P.web) classRows.push([gloss('Web searches', 'Charged per search, not per token'), plural(P.web, 'search', 'searches'), '', usd(P.webAmt), share(P.webAmt, P.amt)])
  const keys = [
    ['Sessions', num(P.sessions.length)], ['Requests', num(P.n)],
    ['Largest context', big ? num(big.peak) + span('', esc(c.dm(big.first).replace(/^0/, '') + ', ' + big.title)) : '—'],
    ['Compactions', num(P.comps.auto + P.comps.manual) + span('', num(P.comps.auto) + ' automatic, ' + num(P.comps.manual) + ' manual')],
    ['Cache written again', plural(rewrites, 'time')],
    ['Cache hit rate', esc(pct(K.cr.tok, K.cr.tok + K.cw.tok + K.in.tok)) + span('', esc(cacheNote(P.saved - P.premium)))],
  ]
  parts.push(section('Summary', 'Every token processed in the period, by class.' + (P.unpriced ? ' ' + plural(P.unpriced, 'request') + ' ran on a model with no rate and carry no amount.' : ''),
    '<div class="pair">\n<div class="wide">' +
      table('w560', [['Class', 'l'], ['Tokens'], ['Share'], ['Amount'], ['Share']], classRows, ['Total processed', num(P.tok), '100%', usd(P.amt), '100%']) +
      '</div>\n<dl class="keys">\n' + keys.map(([k, v]) => '<div><dt>' + k + '</dt><dd>' + v + '</dd></div>').join('\n') + '\n</dl>\n</div>'))

  // Where it went
  const byAmount = map => [...map.entries()].sort((a, b) => b[1].amt - a[1].amt || b[1].tok - a[1].tok)
  // a long list shows its 12 largest lines, then one line for the rest, so the table still adds up
  const top = (rows, what) => {
    if (rows.length <= 14) return rows
    const rest = { n: 0, tok: 0, amt: 0, sessions: new Set() }
    for (const [, x] of rows.slice(12)) (rest.n += x.n), (rest.tok += x.tok), (rest.amt += x.amt), x.sessions.forEach(s => rest.sessions.add(s))
    return [...rows.slice(0, 12), [num(rows.length - 12) + ' other ' + what, rest]]
  }
  parts.push(section('Where it went', 'By project, largest amount first.',
    table('w640', [['Project', 'l'], ['Sessions'], ['Requests'], ['Tokens processed'], ['Amount'], ['Share']],
      top(byAmount(P.projects), 'projects').map(([name, x]) => [esc(name), num(x.sessions.size), num(x.n), num(x.tok), usd(x.amt), share(x.amt, P.amt)]),
      ['All projects', num(P.sessions.length), num(P.n), num(P.tok), usd(P.amt), '100%'])))

  // Breakdown: the same spend cut other ways
  const cut = (map, first, rows = byAmount(map)) =>
    table('w640', [[first, 'l'], ['Sessions'], ['Requests'], ['Tokens processed'], ['Amount'], ['Share']],
      rows.map(([name, x]) => [esc(name), num(x.sessions.size), num(x.n), num(x.tok), usd(x.amt), share(x.amt, P.amt)]))
  const tags = byAmount(P.tags), toolRows = Object.entries(P.tools).sort((a, b) => b[1] - a[1]), allTools = toolRows.reduce((a, x) => a + x[1], 0)
  const days = [...P.days.entries()].sort((a, b) => (a[0] < b[0] ? -1 : 1))
  const perHour = w => (w.last - w.first >= 60000 ? usd(Math.round((w.amt / (w.last - w.first)) * 3600e3)) : '—')
  parts.push(section('Breakdown', 'The same spend cut other ways. Each table covers every request in the period.',
    '<div class="stack">\n<h3 class="h3">By model</h3>\n' +
      table('w640', [['Model', 'l'], ['Requests'], ['New input'], ['Cache write'], ['Cache read'], ['Output'], ['Amount'], ['Share']],
        byAmount(P.models).map(([name, x]) => [esc(name), num(x.n), num(x.in), num(x.cw), num(x.cr), num(x.out), rateKnown(o.rates, name) ? usd(x.amt) : '—', share(x.amt, P.amt)])) +
      '\n</div>\n<div class="stack">\n<h3 class="h3">By agent</h3>\n' + cut(P.agents, 'Thread', top(byAmount(P.agents), 'kinds of subagent')) +
      (tags.length ? '\n</div>\n<div class="stack">\n<h3 class="h3">By skill, plugin and MCP server</h3>\n' + cut(P.tags, 'Working for', tags.slice(0, 15)) +
        el('p', 'small', 'A request counts here when Claude Code tagged it as working for a skill, a plugin or an MCP server. One request can count toward more than one, and most count toward none, so these shares do not add up to 100%.' + (tags.length > 15 ? ' The 15 largest of ' + tags.length + ' are shown.' : '')) : '') +
      (toolRows.length ? '\n</div>\n<div class="stack">\n<h3 class="h3">Tool results, by tool</h3>\n' +
        table('w560', [['Tool', 'l'], ['Results'], ['Tokens added'], ['Share']], toolRows.slice(0, 12).map(([name, v]) => [esc(name), num(P.toolCnt[name] || 0), '≈' + num(v), share(v, allTools)])) +
        (toolRows.length > 12 ? el('p', 'small', 'The 12 largest of ' + toolRows.length + ' tools are shown.') : '') : '') +
      '\n</div>\n' +
      fold('By day', plural(days.length, 'day'), cut(P.days, 'Day', days.map(([d, x]) => [c.dmy(Date.parse(d + 'T12:00:00Z')), x]))) + '\n' +
      fold('By 5-hour usage window', plural(P.windows.length, 'window'),
        table('w640', [['Window', 'l'], ['Requests'], ['Tokens processed'], ['Amount'], ['Amount per hour']],
          P.windows.map(w => [esc(c.dm(w.start) + ', ' + c.hm(w.start) + ' to ' + c.hm(w.start + 5 * 3600e3)), num(w.n), num(w.tok), usd(w.amt), perHour(w)])) +
        el('p', 'small', 'A window opens on the hour of its first request and runs five hours; the next opens after that, or after a five-hour pause. Amount per hour is the amount over the time between the window’s first and last request. This is a reading of the local logs, not Anthropic’s own usage meter, which also counts use on other devices.'))))

  // Why
  const by = { ...P.by, system: (P.by.system || 0) + P.recomp.tok }, cnt = { ...P.cnt, system: (P.cnt.system || 0) + P.recomp.n }
  const added = CAUSES.reduce((a, [k]) => a + (by[k] || 0), 0)
  const toolTotal = Object.values(P.tools).reduce((a, b) => a + b, 0)
  const topTools = Object.entries(P.tools).sort((a, b) => b[1] - a[1])
  const toolGloss = toolTotal
    ? topTools.slice(0, 2).map(([n, v]) => n + ' ' + pct(v, toolTotal)).join(', ') + (topTools.length > 2 ? ', other tools ' + pct(topTools.slice(2).reduce((a, x) => a + x[1], 0), toolTotal) : '')
    : 'What tools returned'
  const written = Object.values(P.flags).reduce((a, f) => a + f.again, 0), extra = Object.values(P.flags).reduce((a, f) => a + f.extra, 0)
  const W = K.in.tok + K.cw.tok, rest = W - added - written
  const events = EVENTS.filter(([k], i) => i < 3 || P.flags[k]).map(([k, name]) => {
    const f = P.flags[k] || { n: 0, again: 0, extra: 0 }
    return [gloss(name, 'Whole context written to the store again'), num(f.n), [num(f.again), 'n or'], [usd(f.extra, true), 'n or']]
  })
  events.push({ tot: true, cells: ['Written again', num(rewrites), num(written), usd(extra, true)] })
  events.push([gloss('Compactions', 'History replaced by a summary; context falls'), num(P.comps.auto + P.comps.manual), [signed(-P.comps.fall), 'n bl'], ''])
  parts.push(section('Why', 'What was added to conversations, and the events that changed what they cost.',
    '<div class="cols">\n<div>\n<h3 class="h3">What filled the context</h3>\n' +
      table('w560', [['Source', 'l'], ['Times'], ['Tokens added'], ['Share']],
        CAUSES.map(([k, name, g]) => [gloss(name, k === 'tool' ? toolGloss : g), num(cnt[k] || 0), '≈' + num(by[k] || 0), share(by[k] || 0, added)]),
        ['Added to context', '', num(added), '100%']) +
      '\n</div>\n<div>\n<h3 class="h3">What cost extra, and what brought context down</h3>\n' +
      table('w560', [['Event', 'l'], ['Times'], ['Tokens'], ['Effect on amount']], events) + '\n' +
      el('p', 'small', 'Added to context ' + num(added) + ' plus written again ' + num(written) +
        (rest > 0 ? ' plus ' + num(rest) + ' of existing context sent without a cache read' : rest < 0 ? ' less ' + num(-rest) + ' that was already in the cache when a conversation opened, or was trimmed,' : '') +
        ' equals new input and cache write, ' + num(W) + '.') + '\n</div>\n</div>'))

  // Sessions
  parts.push(section('Sessions', 'One line per session. ' + (o.link ? 'Open a session to see every request and what was sent and received.' : 'Run a statement for one session to see every request.'),
    table('w1040', [['Date', 'date'], ['Session', 'l'], ['Requests'], ['Peak context'], ['New input'], ['Cache write'], ['Cache read'], ['Output'], ['Amount']],
      P.sessions.map(r => {
        const href = o.link && o.link(r.id)
        return [[c.dm(r.first), 'd'], [(href && !r.gone ? '<a href="' + esc(href) + '">' + esc(r.title) + '</a>' : esc(r.title)) + span('gl', esc(r.project + (r.gone ? ' · log removed, figures kept in the statement’s record' : ''))), ''], num(r.n), num(r.peak), num(r.in), num(r.cw), num(r.cr), num(r.out), usd(r.amt)]
      }),
      [['', ''], ['Period total', ''], num(P.n), '', num(K.in.tok), num(K.cw.tok), num(K.cr.tok), num(K.out.tok), usd(P.amt)])))

  // Reconciliation
  const R = P.recon, all = P.sessions.length
  const cover = (!R.n ? 'None of the ' + plural(all, 'session') + ' can be checked this way.' : R.n === all ? (all === 1 ? 'The session is' : 'All ' + num(all) + ' sessions are') + ' checked here.' : num(R.n) + ' of ' + plural(all, 'session') + ' are checked here.') +
    (R.missing ? ' ' + plural(R.missing, 'session') + ' carry no control total.' : '') +
    (R.short ? ' In ' + plural(R.short, 'session') + ' Claude Code’s own total is lower than what its logs itemize, so it is not a full figure for them: its counter can miss part of a resumed session, and the work of background workflow agents. They are left out of this table; the statement itemizes ' + usd(R.shortAmt) + ' for them.' : '')
  const cross = R.crossing ? ' ' + plural(R.crossing, 'session') + ' also ran outside this period; a control total covers a whole session, so those are counted whole here.' : ''
  parts.push(section('Reconciliation',
    'This statement against the totals Claude Code recorded itself. “Not itemized” is spend the logs do not break down: requests the engine makes for itself, such as session titles and compaction summaries. ' + cover + cross,
    R.n ? reconTable(R.ours, R.theirs) : ''))
  parts.push(howTo([], o.rates && o.rates.asOf))
  return page('Token Accounting Statement: period', parts.join('\n'))
}

// ---- plain text -----------------------------------------------------------------------------

const padL = (s, n) => String(s).padStart(n), padR = (s, n) => (String(s).length > n ? String(s).slice(0, n - 1) + '…' : String(s).padEnd(n))

export function sessionText(s, o = {}) {
  const c = clock(o.tz), m = s.main, t = s.totals, W = 112
  const line = (no, time, entry, ctx, change, cw, cr, out, amt) => ' ' + padL(no, 4) + '  ' + padR(time, 5) + '  ' + padR(entry, 40) + padL(ctx, 10) + padL(change, 10) + padL(cw, 10) + padL(cr, 12) + padL(out, 8) + padL(amt, 9)
  const out = [
    ' TOKEN ACCOUNTING STATEMENT · SESSION',
    ' Session  ' + s.title + ' (' + s.id.slice(0, 8) + ')    Project  ' + (s.project || '—'),
    ' Period   ' + (s.first ? c.span(s.first, m.last || s.first) : '—') + '    Model  ' + (m.models.join(', ') || '—'),
    ' ' + '═'.repeat(W),
    line('No.', 'Time', 'Entry', 'Context', 'Change', 'Cache', 'Cache', 'Output', 'Amount'),
    line('', '', '', 'after', '', 'write', 'read', '', ''),
    ' ' + '─'.repeat(W),
  ]
  for (const e of m.entries) {
    if (e.kind === 'compaction') out.push(line(e.no, c.hm(e.t), 'COMPACTION (' + (e.trigger === 'manual' ? 'manual' : 'automatic') + ')', e.ctx == null ? '—' : num(e.ctx), signed(e.change), '', '', '', ''))
    else {
      out.push(line(e.no, c.hm(e.t), e.title, num(e.ctx), signed(e.change), num(e.cw), num(e.cr), num(e.out), usd(e.amt)))
      if (e.sub) out.push(' '.repeat(14) + padR(e.sub, 96))
      if (e.flag) out.push(' '.repeat(14) + '! ' + flagText(e))
    }
  }
  out.push(' ' + '─'.repeat(W))
  const tot = (name, x) => line('', '', name + ' (' + plural(x.n, 'request') + ')', '', '', num(x.cw), num(x.cr), num(x.out), usd(x.amt))
  out.push(tot('Main thread', m.totals))
  if (s.subs.length) out.push(tot(plural(s.subs.length, 'subagent'), s.subTotals))
  out.push(' ' + '═'.repeat(W), tot('SESSION TOTAL', t), ' New input ' + num(t.in) + ' · amounts are API-equivalent USD, a yardstick and not a bill', '')
  const k = s.control
  if (!k) out.push(' RECONCILIATION  no control total recorded in this log')
  else {
    const r = (name, a, b) => ' ' + padR(name, 16) + padL(a, 16) + padL(b, 26) + padL(typeof a === 'number' ? signed(b - a).replace('+', '') : '', 16)
    out.push(' ' + padR('RECONCILIATION', 16) + padL('Itemized here', 16) + padL("Claude Code's own total", 26) + padL('Not itemized', 16))
    for (const [name, key] of [['New input', 'in'], ['Cache write', 'cw'], ['Cache read', 'cr'], ['Output', 'out']]) out.push(r(name, t[key], k[key]).replace(/(\d)(?=(\d{3})+(?!\d))/g, '$1,'))
    out.push(' ' + padR('Amount', 16) + padL(usd(t.amt), 16) + padL(usd(k.usd * 1e9), 26) + padL(usd(Math.round(k.usd * 1e9) - t.amt), 16))
  }
  return out.join('\n') + '\n'
}

export function periodText(P, o = {}) {
  const c = clock(o.tz), K = P.classes, W = 134
  const line = (d, name, n, peak, cw, cr, out, amt) => ' ' + padR(d, 7) + padR(name, 44) + padL(n, 9) + padL(peak, 11) + padL(cw, 16) + padL(cr, 17) + padL(out, 14) + padL(amt, 12)
  const out = [
    ' TOKEN ACCOUNTING STATEMENT · PERIOD',
    ' ' + o.lede + '    Statement  ' + o.statement + (P.project ? '    Project  ' + P.project : ''),
    ' ' + '═'.repeat(W),
    line('Date', 'Session', 'Reqs', 'Peak', 'Cache write', 'Cache read', 'Output', 'Amount'),
    ' ' + '─'.repeat(W),
    ...P.sessions.map(r => line(c.dm(r.first), r.title + ' · ' + r.project, num(r.n), num(r.peak), num(r.cw), num(r.cr), num(r.out), usd(r.amt))),
    ' ' + '═'.repeat(W),
    line('', 'PERIOD TOTAL (' + plural(P.sessions.length, 'session') + ')', num(P.n), '', num(K.cw.tok), num(K.cr.tok), num(K.out.tok), usd(P.amt)),
    ' New input ' + num(K.in.tok) + ' · amounts are API-equivalent USD, a yardstick and not a bill',
    '',
  ]
  return out.join('\n') + '\n'
}

// What the pane in Claude Code shows: the figures, and the latest entries newest first, as text.
export function sessionBrief(s, o = {}) {
  const c = clock(o.tz), m = s.main, t = s.totals
  return {
    id: s.id, title: s.title, amount: usd(t.amt), requests: plural(t.n, 'request'),
    context: num(m.closing), peak: num(m.peak.ctx), hit: pct(t.cr, t.cr + t.cw + t.in),
    subs: s.subs.length ? plural(s.subTotals.n, 'request') + ' in ' + plural(s.subs.length, 'subagent') + ', ' + usd(s.subTotals.amt) : '',
    rows: m.entries.slice(-40).reverse().map(e => e.kind === 'compaction'
      ? { no: e.no, time: c.hm(e.t), title: 'Compaction, ' + (e.trigger === 'manual' ? 'manual' : 'automatic'), change: signed(e.change), amount: '', flag: '' }
      : { no: e.no, time: c.hm(e.t), title: e.title, change: signed(e.change), amount: usd(e.amt), flag: e.flag ? flagText(e) : '' }),
  }
}

// The few lines /token-statement answers with in the chat. Short on purpose: it may enter the context.
export function summaryText(title, t, extra, url) {
  return [
    'Token Accounting Statement · ' + title,
    plural(t.n, 'request') + ' · ' + usd(t.amt) + ' at API list prices (a yardstick, not a bill)',
    'New input ' + num(t.in) + ' · cache write ' + num(t.cw) + ' · cache read ' + num(t.cr) + ' · output ' + num(t.out),
    ...extra,
    ...(url ? ['Statement: ' + url] : []),
  ].join('\n')
}
