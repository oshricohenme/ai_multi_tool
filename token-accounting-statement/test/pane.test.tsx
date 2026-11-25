import { expect, mock, test } from 'claude-code/testing'

const BRIEF = {
  id: 'abc',
  title: 'Find the bug',
  amount: '$1.23',
  requests: '3 requests',
  context: '12,000',
  peak: '15,000',
  hit: '90%',
  subs: '',
  rows: [{ no: 3, time: '10:02', title: 'Read · notes.md', change: '+1,200', amount: '$0.41', flag: 'Cache expired after 6 min idle.' }],
}

const PANE = {
  plugin: 'token-accounting-statement',
  component: 'Pane',
  requestId: 'token-statement',
  props: { title: 'Token Accounting Statement', isFocused: false, bodyColumns: 72, placement: 'dock', scroll: { offset: 0, bodyRows: 20 }, view: {} },
} as const

test('the pane shows what the script printed, on every surface', async ($, on) => {
  // Nothing sits beneath the plugin: each hook here stands for the engine.
  on('session.info', () => ({ value: { transcriptPath: '/logs/abc.jsonl', sysname: 'Darwin', release: '25' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('ui.status', () => ({ value: undefined }))
  on('process.run', (_, e) => ({ value: { exitCode: 0, stdout: e.argv.includes('--brief') ? JSON.stringify(BRIEF) : 'opened', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }))

  const { text } = await $.command.run({ command: 'token-statement', args: '', origin: { kind: 'composer' }, presentation: { isFullscreen: false, columns: 120 } })
  expect(text).toBe('opened')

  for (const surface of ['terminal', 'desktop', 'vscode', 'mobile'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect((await ui.find({ type: 'Text', text: /API-equivalent/ }))?.text).toBe('$1.23 API-equivalent · 3 requests')
    expect((await ui.find({ type: 'Text', text: /notes\.md/ }))?.text).toBe('Read · notes.md')
    expect((await ui.find({ type: 'Text', text: /Cache expired/ }))?.text).toBe('Cache expired after 6 min idle.')
    await ui.unmount()
  }
})

test('a new response refreshes the pane without anyone asking', async ($, on) => {
  on('session.info', () => ({ value: { transcriptPath: '/logs/abc.jsonl', sysname: 'Darwin', release: '25' } }))
  on('ui.status', () => ({ value: undefined }))
  on('process.run', () => ({ value: { exitCode: 0, stdout: JSON.stringify({ ...BRIEF, amount: '$2.50' }), stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }))
  on('session.measure', (_, e) => ({ changed: e.changed }))
  const clock = mock.clock(on)

  await $.session.measure({ context: { window: 200000, tokens: 12000, percent: 6 }, rateLimits: [], changed: ['cost'] })

  // The refresh runs on after the hook returns, so it never holds the session up: let it finish.
  await clock.settle()
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect((await ui.find({ type: 'Text', text: /API-equivalent/ }))?.text).toBe('$2.50 API-equivalent · 3 requests')
  await ui.unmount()
})
