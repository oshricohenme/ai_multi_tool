import { expect, test } from 'claude-code/testing'

const PANE = {
  plugin: 'mt',
  component: 'Pane',
  requestId: 'mt-activity',
  props: { title: 'mt activity', isFocused: false, bodyColumns: 100, placement: 'dock', scroll: { offset: 0, bodyRows: 20 }, view: {} },
} as const

const ACTIVITY = {
  line: 'mt ▸ 3 calls · 1 mcp · ~9.5k tok kept out · last query 4 ms',
  events: 5, requests: 3, compressions: 1, tokens_kept_out: 9500, mcp_calls: 1, reindexes: 1,
  by_cmd: [{ cmd: 'query', count: 2, avg_ms: 4 }, { cmd: 'apply', count: 1, avg_ms: 120 }],
  recent: [
    { t: 1793980990, kind: 'compress', label: 'compress', detail: '`go test ./...` output 40000→2000 chars, kept as x1', ms: null, ok: true },
    { t: 1793980980, kind: 'request', label: 'query', detail: 'Server.auditWrite', ms: 4, ok: true },
  ],
}

test('the status line and the pane show what mt did, on every surface', async ($, on) => {
  const statuses: (string | undefined)[] = []
  const asked: string[][] = []
  on('command.register', () => ({ value: { command: 'mt-activity' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.id', () => ({ value: 'abcdef12-3456' }))
  on('clock.every', () => ({ value: { cancel: () => {} } }))
  on('ui.status', (_, e) => { statuses.push((e as { text?: string }).text); return { value: undefined } })
  on('process.run', (_, e) => { asked.push([...(e as { argv: string[] }).argv]); return { value: { exitCode: 0, stdout: JSON.stringify(ACTIVITY), stderr: '' } } })

  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  expect(asked[0]).toEqual(['mt', 'activity', '--json', '--limit', '60', '--session', 'abcdef12-3456'])
  expect(statuses.at(-1)).toBe(ACTIVITY.line)

  for (const surface of ['terminal', 'desktop', 'vscode', 'mobile'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect((await ui.find({ type: 'Text', text: /9,500 tokens kept out/ }))?.text).toContain('1 MCP call')
    expect(await ui.find({ type: 'Text', text: /Server\.auditWrite/ })).toBeDefined()
    await ui.unmount()
  }
})

test('a failing mt shows its error instead of stale numbers', async ($, on) => {
  on('command.register', () => ({ value: { command: 'mt-activity' } }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.id', () => ({ value: 's1' }))
  on('clock.every', () => ({ value: { cancel: () => {} } }))
  on('ui.status', () => ({ value: undefined }))
  on('process.run', () => ({ value: { exitCode: 2, stdout: '', stderr: 'mt: not a repo' } }))
  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect((await ui.find({ type: 'Text', text: /not a repo/ }))?.text).toBe('mt: not a repo')
  await ui.unmount()
})
