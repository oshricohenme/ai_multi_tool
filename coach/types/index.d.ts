export type Kind = 'system' | 'context' | 'user' | 'assistant' | 'tool' | 'compact'
export type Tab = 'duration' | 'turns' | 'calls'
export type DetailTab = 'summary' | 'payload' | 'result' | 'schema' | 'timing'

/** One row of the trace. `assistant` rows are model calls and carry usage. */
export type Ev = {
  id: string
  kind: Kind
  turnId?: string
  agentId?: string
  /** The model call this row is (assistant) or came from (tool). */
  step?: number
  tool?: string
  title: string
  /** What went in: prompt text, request, tool arguments. */
  payload: string
  /** What came out: reasoning and answer, tool output. */
  result: string
  start: number
  /** First streamed chunk of a model call. */
  firstAt?: number
  end?: number
  isError?: boolean
  model?: string
  effort?: string
  inTok?: number
  outTok?: number
  cacheRead?: number
  cacheWrite?: number
  usd?: number
  stop?: string | null
  thinkingChars?: number
  /** Tool result size in tokens (chars / 4). */
  est?: number
  /** compact rows: who compacted, context tokens before and after. */
  trigger?: string
  before?: number
  after?: number
}

/** A live coach nudge shown above the prompt. */
export type Nudge = { id: string; text: string; fix: string; sev?: 'high' | 'medium' | 'low' }

export type Turn = {
  id: string
  agentId?: string
  text: string
  start: number
  end?: number
  reason?: string
}

declare module 'claude-code' {
  interface PluginState {
    'coach': {
      events: Shaped<Ev[]>
      turns: Shaped<Turn[]>
      tab: Tab
      detailTab: DetailTab
      query: string
      selected: string | null
      windowMs: number
      nudge: Nudge | null
      dismissed: string[]
    }
  }
}
