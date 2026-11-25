// What the pane and the status line show: bin/statement.mjs --brief, as it prints it.
export type Row = { no: number; time: string; title: string; change: string; amount: string; flag: string }
export type Brief = {
  id: string
  title: string
  amount: string
  requests: string
  context: string
  peak: string
  hit: string
  subs: string
  rows: Row[]
}

declare module 'claude-code' {
  interface PluginState {
    'token-accounting-statement': { brief: Shaped<Brief | null> }
  }
}
