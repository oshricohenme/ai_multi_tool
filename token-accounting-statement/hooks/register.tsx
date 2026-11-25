import { atom, read, update } from 'claude-code'
import type { EngineInterface, ProcessRunResult, Register } from 'claude-code'

import type { Brief } from '../types'

// /token-statement: the token accounting statement, live for this session in a pane and in the
// browser, or for a period. bin/statement.mjs does the work and prints the text. Nothing here calls
// a model: a command's output may be carried in the conversation, and the statement must not spend
// what it counts.
const PANE = 'token-statement'
const TITLE = 'Token Accounting Statement'
// Change the tag when Brief's fields change: a value an earlier version stored outlives a reload.
const brief = atom({ plugin: 'token-accounting-statement', key: 'brief' } as const, null, { shape: 'brief-1' })

let busy = false
let again = false
// Reads this session's logs again, then redraws the pane and the status line. One run at a time:
// what lands during a run is picked up by one more run after it.
async function refresh($: EngineInterface) {
  if (busy) {
    again = true
    return
  }
  busy = true
  try {
    do {
      again = false
      const { transcriptPath } = await $.session.info()
      const ran = await $.process.run(['node', `${$.plugin.root}/bin/statement.mjs`, '--session', transcriptPath, '--brief'], { timeoutMs: 120_000 })
      if (ran.exitCode !== 0) break // no log written yet: the next response tries again
      const b: Brief = JSON.parse(ran.stdout)
      await update($, brief, () => b)
      $.ui.status(`Token statement ${b.amount} · context ${b.context} · ${b.requests}`)
    } while (again)
  } catch {
    // a run cut short: the next response tries again
  } finally {
    busy = false
  }
}

const said = (ran: ProcessRunResult) =>
  ran.exitCode === 0 ? ran.stdout.trim() : `The statement could not run: ${(ran.stderr || ran.stdout).trim()}`

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'token-statement',
      description: 'Token accounting statement: what this session spends, live, how the context moves, and why',
      argumentHint: '[week | month | text]',
    })
    // Keep the statement's own record current, so statements outlive Claude Code's cleanup of old logs.
    void $.process.run(['node', `${$.plugin.root}/bin/statement.mjs`, 'index'], { timeoutMs: 600_000 }).catch(() => {})
    void refresh($)
    return next(e)
  })

  // Raised when the session's cost or context moves: after each priced response, the main thread's
  // and every subagent's, and after a compaction.
  on('session.measure', async ($, e, next) => {
    if (e.changed.includes('cost') || e.changed.includes('context')) void refresh($)
    return next(e)
  })

  on('command.run', { command: 'token-statement' }, async ($, e) => {
    const want = e.args.trim().toLowerCase()
    const { transcriptPath } = await $.session.info()
    const script = `${$.plugin.root}/bin/statement.mjs`
    if (want === 'week' || want === 'month' || want === 'text') {
      const args =
        want === 'week' ? ['open', '--since', '7d']
        : want === 'month' ? ['open', '--month', 'current']
        : ['--session', transcriptPath, '--text']
      return { text: said(await $.process.run(['node', script, ...args], { timeoutMs: 600_000 })) }
    }
    // The live view: this pane in Claude Code, and the same statement in the browser.
    await $.ui.open({ id: PANE, title: TITLE })
    await refresh($)
    return { text: said(await $.process.run(['node', script, 'open', '--session', transcriptPath], { timeoutMs: 600_000 })) }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text } = $.ui.resolve(e)
    const b = await read($, brief)
    if (!b) return <Text dimColor>Reading this session’s log…</Text>

    return (
      <Box flexDirection="column">
        <Text bold>
          {b.amount} API-equivalent · {b.requests}
        </Text>
        <Text>
          Context now {b.context} · peak {b.peak} · cache hit {b.hit}
        </Text>
        {b.subs !== '' && <Text dimColor>Subagents: {b.subs}</Text>}
        <Text dimColor>Every request in full, live, in the browser: /token-statement</Text>
        <Box flexDirection="row" gap={1} marginTop={1}>
          <Text dimColor>{'No.'.padStart(4) + ' ' + 'Time'.padEnd(5)}</Text>
          <Box flexGrow={1} flexShrink={1}>
            <Text dimColor>Entry, newest first</Text>
          </Box>
          <Text dimColor>{'Change'.padStart(9) + ' ' + 'Amount'.padStart(8)}</Text>
        </Box>
        {b.rows.map(r => (
          <Box flexDirection="column">
            <Box flexDirection="row" gap={1}>
              <Text dimColor>{String(r.no).padStart(4) + ' ' + r.time.padEnd(5)}</Text>
              <Box flexGrow={1} flexShrink={1}>
                <Text wrap="truncate">{r.title}</Text>
              </Box>
              <Text>{r.change.padStart(9) + ' ' + r.amount.padStart(8)}</Text>
            </Box>
            {r.flag !== '' && (
              <Box paddingLeft={11}>
                <Text color="warning" wrap="truncate">
                  {r.flag}
                </Text>
              </Box>
            )}
          </Box>
        ))}
      </Box>
    )
  })
}
