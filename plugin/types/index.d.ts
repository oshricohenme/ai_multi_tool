export type ActivityEvent = { t: number; kind: string; label: string; detail: string; ms: number | null; ok: boolean }
export type CmdStat = { cmd: string; count: number; avg_ms: number }
export type Activity = {
  line: string
  events: number
  requests: number
  by_cmd: CmdStat[]
  compressions: number
  tokens_kept_out: number
  mcp_calls: number
  reindexes: number
  recent: ActivityEvent[]
}
export type View = { activity: Activity | null; error: string | null; scope: 'session' | 'repo' }

declare module 'claude-code' {
  interface PluginState {
    'mt': { view: Shaped<View> }
  }
}
