import { expect, test } from 'claude-code/testing'

const PANE = {
  plugin: 'coach',
  component: 'Pane',
  requestId: 'coach',
  props: {
    title: 'Claude coach',
    isFocused: false,
    bodyColumns: 100,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 30 },
    view: {},
  },
} as const

test('a turn and its tool call show in the trace, and the tabs switch', async ($, on) => {
  let clock = 1000
  on('clock.now', () => ({ value: (clock += 250) }))
  on('command.register', () => ({ value: { command: 'coach' } }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('tool.call', () => ({ result: {}, text: 'NAVIGATION_OK' }))
  on('tool.list', () => ({ value: [{ name: 'Bash', description: 'Run a shell command', mcp: false }] }))

  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'print NAVIGATION_OK', turnId: 't1' })
  await $.tool.call({ tool: 'Bash', tool_use_id: 'tu1', command: 'echo NAVIGATION_OK' })

  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ type: 'Text', text: /print NAVIGATION_OK/ })).toBeDefined()
    expect(await ui.find({ type: "Text", text: /^bash .*echo NAVIGATION_OK/ })).toBeDefined()
    expect(await ui.find({ type: "Text", text: /→ NAVIGATION_OK/ })).toBeDefined()

    expect(await ui.find({ type: 'Text', text: /Turn 1/ })).toBeDefined()

    await ui.press({ key: 'tool:tu1' })
    await ui.press({ key: 'dt-result' })
    expect(await ui.find({ type: 'Text', text: /^NAVIGATION_OK$/ })).toBeDefined()
    await ui.press({ key: 'dt-schema' })
    expect(await ui.find({ type: 'Text', text: /command: string/ })).toBeDefined()
    await ui.press({ key: 'close' })

    await ui.press({ key: 'tab-turns' })
    expect(await ui.find({ type: 'Text', text: /1 tools/ })).toBeDefined()

    await ui.input({ key: 'search', text: 'nomatch' })
    expect(await ui.find({ type: 'Text', text: /0 match/ })).toBeDefined()
    await ui.input({ key: 'search', text: '' })
    await ui.press({ key: 'tab-duration' })
    await ui.unmount()
  }
})
