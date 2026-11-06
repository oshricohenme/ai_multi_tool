import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Activity, View } from '../types'

const PANE = 'mt-activity'
const view = atom({ plugin: 'mt', key: 'view' } as const, { activity: null, error: null, scope: 'session' } as View, {
  shape: 'view-1',
})

// mt keeps its own record (`.mt/events.jsonl`, per repo); `mt activity --json` sums it up, so
// this mod only asks and draws. `bin/mt` is the plugin's launcher (a local build, the release, or PATH).
async function refresh($: EngineInterface): Promise<void> {
  const { scope } = await read($, view)
  const argv = [`${$.plugin.root}/bin/mt`, 'activity', '--json', '--limit', '60']
  if (scope === 'session') argv.push('--session', await $.session.id())
  try {
    const r = await $.process.run(argv, { timeoutMs: 5000 })
    if (r.exitCode !== 0) {
      await update($, view, v => ({ ...v, error: (r.stderr || r.stdout).trim().slice(0, 200) || `mt exited ${r.exitCode}` }))
      return
    }
    const activity = JSON.parse(r.stdout) as Activity
    await update($, view, v => ({ ...v, activity, error: null }))
    // nothing done yet: no status entry at all
    $.ui.status(activity.events > 0 && (activity.requests > 0 || activity.compressions > 0 || activity.mcp_calls > 0) ? activity.line : undefined)
  } catch (err) {
    await update($, view, v => ({ ...v, error: `mt activity: ${String(err).slice(0, 200)}` }))
  }
}

const clock = (t: number): string => {
  const d = new Date(t * 1000)
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map(n => String(n).padStart(2, '0')).join(':')
}

const KIND_COLOR: Record<string, string> = { request: 'cyan', compress: 'green', mcp: 'magenta', reindex: 'gray', daemon: 'gray' }

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    await $.command.register({ name: 'mt-activity', description: "Show mt's live activity (queries, applies, compressions, MCP calls) in a pane; `repo` for the whole repo" })
    await refresh($)
    // the daemon also works between tool calls (reindexing what changed): look every 3 s
    $.clock.every(3000, () => { void refresh($) })
    return started
  })

  on('command.run', { command: 'mt-activity' }, async ($, e) => {
    const arg = e.args.trim()
    const scope: View['scope'] | null = arg === 'repo' ? 'repo' : arg === 'session' ? 'session' : null
    if (scope) await update($, view, (v): View => ({ ...v, scope }))
    await refresh($)
    await $.ui.open({ id: PANE, title: 'mt activity' })
    return { text: `mt activity pane opened (${(await read($, view)).scope}).` }
  })

  // right after any tool call: what mt just did shows at once, not on the next tick
  on('tool.call', async ($, e, next) => {
    const ran = await next(e)
    void refresh($)
    return ran
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text } = $.ui.resolve(e)
    const { activity: a, error, scope } = await read($, view)
    const rows = Math.max(3, (e.viewport?.rows ?? 24) - 7)
    if (error) return <Text color="red">{error}</Text>
    if (!a) return <Text dimColor>Waiting for mt…</Text>
    return (
      <Box flexDirection="column">
        <Text bold>{a.line}</Text>
        <Text dimColor>
          {scope === 'session' ? 'this session' : 'whole repo'} · {a.by_cmd.map(c => `${c.cmd} ${c.count}× ${c.avg_ms} ms`).join(' · ') || 'no requests yet'}
        </Text>
        <Text dimColor>
          {a.compressions} compression{a.compressions === 1 ? '' : 's'} · ~{a.tokens_kept_out.toLocaleString()} tokens kept out of context · {a.mcp_calls} MCP call{a.mcp_calls === 1 ? '' : 's'} · {a.reindexes} reindex{a.reindexes === 1 ? '' : 'es'}
        </Text>
        <Text> </Text>
        {a.recent.length === 0 && <Text dimColor>No activity yet.</Text>}
        {a.recent.slice(0, rows).map(ev => (
          <Text wrap="truncate-end">
            <Text dimColor>{clock(ev.t)} </Text>
            <Text color={ev.ok ? KIND_COLOR[ev.kind] ?? 'white' : 'red'}>{ev.label.padEnd(9)}</Text>
            <Text> {ev.detail}</Text>
            {ev.ms !== null && <Text dimColor> {ev.ms} ms</Text>}
          </Text>
        ))}
      </Box>
    )
  })
}
