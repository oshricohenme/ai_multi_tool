import { expect, test } from 'claude-code/testing'

// The firehose must never change what it observes: streams, results, errors.
test('the all-events observer passes streams, results and errors through untouched', async ($, on) => {
  let clock = 1000
  on('clock.now', () => ({ value: (clock += 250) }))
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.usage', () => ({ value: { context: { window: 200000 }, rateLimits: [], cost: { usd: 0 } } as never }))
  on('command.register', () => ({ value: { command: 'coach' } }))
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))
  on('tool.call', (_, e) => {
    if ((e as { command?: string }).command === 'boom') throw new Error('tool failed')
    return { result: {}, text: 'OUT' }
  })
  on('turn.step', async function* () {
    yield { kind: 'text', index: 0, text: 'Hel' } as never
    yield { kind: 'text', index: 0, text: 'lo' } as never
    return { turnId: 't1', index: 0, answer: 'Hello', toolUses: [], stopReason: 'end_turn', usage: { input_tokens: 5, output_tokens: 2, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, model: 'm' } } as never
  })

  await $.session.start({ cwd: '/', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'hi', turnId: 't1' })

  const stream = $.turn.step({ turnId: 't1', index: 0, model: 'm', messageCount: 1 } as never)
  const chunks: string[] = []
  for await (const c of stream as AsyncIterable<{ text?: string }>) chunks.push(c.text ?? '')
  expect(chunks.join('')).toBe('Hello')

  const ok = await $.tool.call({ tool: 'Bash', tool_use_id: 'tu1', command: 'echo' } as never)
  expect((ok as { text: string }).text).toBe('OUT')

  let threw = ''
  try {
    await $.tool.call({ tool: 'Bash', tool_use_id: 'tu2', command: 'boom' } as never)
  } catch (err) {
    threw = String(err)
  }
  expect(threw.length > 0).toBe(true) // the rejection reaches the caller
})
