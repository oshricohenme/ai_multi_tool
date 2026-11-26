# Changelog

## Unreleased

- **Every coding agent.** The statement also reads Codex, opencode and Gemini CLI sessions. Period
  statements gain a "By coding agent" table, and session statements name their agent.
  `--session latest` picks the session written to last.
- **One command to install and run.** `install.sh` installs a `token-statement` command, the
  Claude Code plugin when Claude Code is there, a skill for agents that read `~/.agents/skills`,
  and `/token-statement` for opencode and Gemini CLI. Then it opens the last 7 days. `--no-claude`
  skips the plugin.
- The browser opens on Linux too (`xdg-open`).
- Session pages never show raw JSON. Tool call inputs, JSON tool results and the API's usage
  show as named fields, tables of like records, numbered lists and code blocks.

## 0.2.0 (2026-11-24)

- **Live.** A status line entry and a pane in Claude Code that update after every response,
  a subagent's included, and after each compaction. The browser's session page follows the
  session as it runs and keeps open lines open.
- **Renamed** from Ledger to Token Accounting Statement. The command is `/token-statement`.
- `--brief` prints the figures and latest entries as JSON, for the pane.

## 0.1.0 (2026-11-21)

- Session statement: one line per request, with the context balance, the change, the reason,
  the tokens by class and the amount. Open any line to see what was sent and received.
- Period statement for a week, a month or any number of days: summary by token class, spend by
  project, a breakdown by model, agent, skill, plugin, MCP server, tool, day and five-hour
  window, and the reasons behind it.
- Reconciliation against Claude Code's own totals, with a "Not itemized" line.
- Subagents itemized in their own accounts.
- Cache rewrites flagged with their cause and cost (an idle gap, a tool list or model change).
- Prices in `rates.json`: fast mode, long-prompt tiers, web search, data residency.
- `--csv`, `--json` and `--html` output, `--no-detail` for sharing.
- Its own record of each session's figures, so statements outlive Claude Code's log cleanup.
- A local server bound to 127.0.0.1, with a random token in its address and a strict
  Content Security Policy.
