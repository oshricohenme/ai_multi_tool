import { expect, test } from 'claude-code/testing'

const BAND = {
  plugin: 'coach',
  component: 'AbovePrompt',
  props: { hasSurvey: false, isWorking: false, maxRows: 10, scroll: { offset: 0, bodyRows: 10 } },
} as const
// the band's detector runs beside the event, not inside it: let it finish
const settle = async () => { for (let i = 0; i < 50; i++) await Promise.resolve() }

// Docs: after two failed corrections, /clear instead of a third. The band says so, once.
test('two corrections in a row raise the coach band, and Got it dismisses it', async ($, on) => {
  on('clock.now', () => ({ value: 1000 }))
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.usage', () => ({ value: { context: { window: 200000 }, rateLimits: [], cost: { usd: 0 } } as never }))
  on('command.register', () => ({ value: { command: 'coach' } }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('ui.render', ($, e) => { const { Box } = $.ui.resolve(e); return <Box /> }) // the engine's own (empty) band

  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'add a retry to fetchUser', turnId: 't1' })
  await $.turn.start({ text: "no, that's not what I meant", turnId: 't2' })
  await settle()
  let ui = await $.ui.mount({ ...BAND, surface: 'terminal' } as never)
  expect(await ui.find({ type: 'Text', text: /Second correction/ })).toBeUndefined()

  await $.turn.start({ text: 'still broken, it retries forever', turnId: 't3' })
  await settle()
  ui = await $.ui.mount({ ...BAND, surface: 'terminal' } as never)
  expect(await ui.find({ type: 'Text', text: /Second correction/ })).toBeDefined()

  await ui.press({ key: 'ok' })
  await settle()
  ui = await $.ui.mount({ ...BAND, surface: 'terminal' } as never)
  expect(await ui.find({ type: 'Text', text: /Second correction/ })).toBeUndefined()
})
