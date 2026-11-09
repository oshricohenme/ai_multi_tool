import { expect, test } from 'claude-code/testing'

const BAND = {
  plugin: 'coach',
  component: 'AbovePrompt',
  props: { hasSurvey: false, isWorking: false, maxRows: 10, scroll: { offset: 0, bodyRows: 10 } },
} as const
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve() }

type Fire = ($: any, ...a: any[]) => Promise<unknown>
const boot = ($: any, on: any, sandbox = false) => {
  on('clock.now', () => ({ value: 1000 }))
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.usage', () => ({ value: { context: { window: 200000 }, rateLimits: [], cost: { usd: 0 } } as never }))
  on('command.register', () => ({ value: { command: 'coach' } }))
  on('session.start', (_: any, e: any) => ({ cwd: e.cwd }))
  on('turn.start', (_: any, e: any) => ({ turnId: e.turnId }))
  on('settings.read', () => ({ value: (sandbox ? { sandbox: { enabled: true } } : {}) as never }))
  on('clock.every', () => ({ value: 1 as never }))
  on('ui.log', () => ({ value: undefined as never }))
  on('ui.status', () => ({ value: undefined as never }))
  on('session.measure', (_: any, e: any) => ({ changed: e.changed }))
  on('command.run', () => ({ text: '' }))
  for (const n of ['PostToolUse', 'PostToolUseFailure', 'PermissionDenied', 'Stop']) on(`classic.${n}` as never, () => ({}) as never)
  on('ui.render', ($: any, e: any) => { const { Box } = $.ui.resolve(e); return <Box /> })
}
const text = async ($: any) => {
  await settle()
  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' } as never)
  return ui
}
const has = async ($: any, re: RegExp) => (await (await text($)).find({ type: 'Text', text: re })) !== undefined
const start = async ($: any) => $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
const prompt = ($: any, t: string, id: string) => $.turn.start({ text: t, turnId: id })
const classic = ($: any, name: string, e: Record<string, unknown>) => ($ as any).classic[name]({ session_id: 's', transcript_path: '', cwd: '/', ...e })

// Each case drives events, checks the band, then presses Got it and replays to prove no repeat.
const cases: [string, RegExp, ($: any) => Promise<void>][] = [
  ['same-failure-loop', /same failing Bash call 3 times/, async $ => {
    for (let i = 0; i < 3; i++) await classic($, 'PostToolUseFailure', { tool_name: 'Bash', tool_input: { command: 'npm test' }, tool_use_id: `t${i}`, error: 'Exit code 1' })
  }],
  ['continue-without-goal', /times\./, async $ => {
    for (const id of ['a', 'b']) {
      await classic($, 'Stop', { stop_hook_active: false, last_assistant_message: 'Done with A. Next I will do B.' })
      await prompt($, 'continue', id)
    }
  }],
  ['undo-by-prose', /Asking Claude to undo/, async $ => { await prompt($, 'undo that please', 'u') }],
  ['fast-mode-toggle', /Fast mode/, async $ => { await $.command.run({ command: 'fast', args: 'on' }); await $.command.run({ command: 'fast', args: 'off' }); await $.command.run({ command: 'fast', args: '' }) }],
  ['bypass-no-sandbox', /Bypass mode with no sandbox/, async $ => {
    await classic($, 'PostToolUse', { permission_mode: 'bypassPermissions', tool_name: 'Read', tool_input: {}, tool_response: {}, tool_use_id: 'x' })
  }],
  ['auto-mode-block-streak', /blocked 3 in a row/, async $ => {
    for (let i = 0; i < 3; i++) await classic($, 'PermissionDenied', { tool_name: 'Bash', tool_input: {}, tool_use_id: `d${i}`, reason: 'no' })
  }],
  ['test-tampering', /changed a test right after it failed/, async $ => {
    await prompt($, 'fix the parser', 't1')
    await classic($, 'PostToolUseFailure', { tool_name: 'Bash', tool_input: { command: 'npm test' }, tool_use_id: 'f', error: 'Exit code 1' })
    await classic($, 'PostToolUse', { tool_name: 'Edit', tool_input: { file_path: '/r/a.test.ts', old_string: 'expect(1)', new_string: 'expect(2)' }, tool_response: {}, tool_use_id: 'e' })
    await classic($, 'PostToolUse', { tool_name: 'Bash', tool_input: { command: 'npm test' }, tool_response: {}, tool_use_id: 'p' })
  }],
]
for (const [id, re, drive] of cases) {
  test(`${id} raises the band once and Got it suppresses a repeat`, async ($, on) => {
    boot($, on)
    await start($)
    await drive($)
    expect(await has($, re)).toBe(true)
    await (await text($)).press({ key: 'ok' })
    await drive($)
    expect(await has($, re)).toBe(false)
  })
}

test('context-high-mid-task and rate-limit-with-background react to session.measure', async ($, on) => {
  boot($, on)
  await start($)
  await prompt($, 'refactor the parser module', 'a')
  await prompt($, 'still broken, the parser module fails', 'b')
  await $.session.measure({ context: { window: 200000, tokens: 150000, percent: 75 }, rateLimits: [], changed: ['context'] })
  expect(await has($, /Context at 75%/)).toBe(true)
  await (await text($)).press({ key: 'ok' })
  await classic($, 'Stop', { stop_hook_active: false, background_tasks: [{ id: '1', type: 'subagent', status: 'running', description: 'x' }] })
  await $.session.measure({ context: { window: 200000 }, rateLimits: [{ kind: 'five_hour', percentUsed: 85 }], changed: ['rateLimits'] })
  expect(await has($, /5-hour limit at 85%/)).toBe(true)
})

test('a low nudge does not replace an unseen high one; it waits', async ($, on) => {
  boot($, on)
  await start($)
  await classic($, 'PostToolUse', { permission_mode: 'bypassPermissions', tool_name: 'Read', tool_input: {}, tool_response: {}, tool_use_id: 'x' })
  await settle()
  await prompt($, 'undo that', 'u')
  expect(await has($, /Bypass mode/)).toBe(true)
  expect(await has($, /Asking Claude to undo/)).toBe(false)
  await (await text($)).press({ key: 'ok' })
  expect(await has($, /Asking Claude to undo/)).toBe(true)
})
