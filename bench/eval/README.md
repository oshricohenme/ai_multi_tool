# bench/eval: the A/B harness behind DEVLOG sections 16-52

Real Claude Code sessions, side by side in two worktrees of a target repo: one with mt (the plugin,
frozen), one without. Each script resets its worktree, restarts the session in an Orca terminal,
and sends the task prompt. The analyzers read the session transcripts (`~/.claude/projects/...`).

## Setup

1. Two worktrees of the repo under test, by default `~/orca/workspaces/claude_coach/mt-eval-off` and
   `.../mt-eval-on`, each open in an Orca terminal.
2. `export T_OFF=<terminal id> T_ON=<terminal id>` (`orca terminal list --json`).
3. `cargo build --release` at the repo root, then `bench/eval/setup.sh`. It writes the frozen plugin
   copy (`plugin-frozen/`), the shop database, `mcp-ab.json` and `mcp-shop.json` with this machine's
   paths. Rerun it after every mt build you want to test: it copies the binary with `rm` + `cp`,
   because macOS kills a binary whose signature changed under it.

## Runs

| script | what |
|---|---|
| `run_on.sh` | one mt ON run of the Google-sign-in task (G1), in `$T_ON` |
| `run_cm.sh <terminal> <worktree>` | one mt OFF run with context-management instructions (`prompts/cm-system.md`) |
| `run_on_cm.sh` | mt ON plus context management; with `EXTRA="CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=10"` it compacts early (section 49) |
| `run_mcp_pair.sh <task>` | one paired MCP A/B: A native MCP, B mt (`TASKS=`, `MCPCFG=` choose the task list and server config) |
| `run_mcp_all.sh <task>...` | pairs in sequence, waiting for both sessions each time |

Task lists: `tasks_mcp.txt` (graft, T1-T3), `tasks_shop.txt` (the shop database, D1-D3, with
`MCPCFG=mcp-shop.json`). `results/shop-gold.txt` holds D1-D3's reference answers.

## Analysis

| script | what |
|---|---|
| `cm_analyze.py label=<transcript.jsonl> ...` | turns (main + subagents), context processed, peak, output, cache, cost, wall time |
| `live_analyze.py <transcript.jsonl> <label>` | every tool call with time, result size and context per turn |
| `transactions.py <transcript.jsonl> <title> full <out.md>` | what Claude sent and got back, as markdown (the DEVLOG transaction logs) |

Cost uses the rates fitted in DEVLOG section 16 (input $4/M, cache write $8/M, cache read
$0.20/M, output $20/M). Compaction calls are not in the transcript, so they are not counted.

## Fixtures

- `fixture/sqlite_mcp.py`: a read-only SQLite MCP server, stdlib only (`list_tables`,
  `describe_table`, `read_query`). The reference `mcp-server-sqlite` does not start with its current
  `mcp` library.
- `gen_shop_db.py`: the seeded shop database (3,000 customers, 30,000 orders, 2,368 refunds).
- `stub/graft`: put first on `PATH` to keep the graft CLI out of a run.
- `prompts/`: the system prompts each arm gets.
