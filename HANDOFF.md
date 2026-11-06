# Handoff: ai_multi_tool (mt, its Claude Code plugin, and two mods)

Written 2026-11-06, when this repo was assembled from its working copies. Read this first, then
`README.md` (install, a first session), `docs/USAGE.md` (every command and setting) and the end of
`DEVLOG.md` (sections 47-52 are the most recent work).

## What is here

**mt** is a code multitool for coding agents, written in Rust. It answers from a live, in-memory
index of the whole repo (a per-repo daemon), so one call returns what a model would otherwise
gather in many: a symbol with its callers, tests, types and tables (`mt context --map`, then
`mt expand`); a batch of questions and claims (`mt query`); a batch of edits across many files,
validated before writing, with format/build/test steps (`mt apply`). Long output (builds, tests,
logs, JSON, MCP results) is compressed and kept out of context, then queried (`mt run`,
`mt compress`, `mt out`). The goal is fewer model round trips: less thinking, fewer tokens, less wall time.

| path | what |
|---|---|
| `src/` | the binary (about 30 modules; see "Code map" below) |
| `tests/` | integration tests: speed, compression, map, briefing, virtual context, coord, ops, search |
| `plugin/` | the Claude Code plugin: `hooks/hooks.json` (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, SessionEnd → `mt hook ...`), `skills/mt` (a symlink to `../../skills/mt`), `bin/mt` (you provide it; see Build) |
| `skills/mt/SKILL.md` | the reference the model reads. Delivered in parts by the hooks (see Gotchas) |
| `.claude-plugin/marketplace.json` | the marketplace: two plugins, `mt` (`./plugin`) and `coach` (`./mods/coach`). `claude-coach` is a reserved name (no `claude-` prefix for third parties) |
| `plugin/hooks/register.tsx` | the former mt-activity mod, folded into the mt plugin: status line and `/mt-activity` pane, from `mt activity --json`; tests in `plugin/tests` |
| `mods/coach/` | Claude coach (was `dev-tools`; command `/coach`, was `/devtools`): inspects every model call and tool call (reasoning, actions, tokens, cost, timing, context, cache, habits, coach nudges) in a pane and in a local web app (`server/server.mjs`, first free port from 7777, data in `~/.claude/dev-tools/sessions/`). Fully local, separate from Relay by design |
| `bench/eval/` | the A/B harness behind the DEVLOG numbers: run scripts, analyzers, task lists, a SQLite MCP fixture. Its own README |
| `bench/tools/`, `bench/coord/` | the tool-graph bench (gold sets) and the multi-agent coordination bench |
| `docs/` | `USAGE.md`, `RESEARCH.md` (evidence and methods), `HISTORY.md` (index of the DEVLOG) |
| `DEVLOG.md` | the full development log: 52 dated sections, raw numbers, transactions |

## State at handoff

- **Nothing is committed.** This folder's `.git` is yours; I did not touch it.
- **Copied, not moved.** The originals are unchanged:
  - mt: `~/orca/mt`. Another Claude session was editing it at the same time (coordination leases
    in `apply.rs`/`coord.rs`, DEVLOG section 48 `mt build/rebuild/logs`), so it may hold newer work
    than this copy.
  - `mt-activity`: `~/orca/mt/mods/mt-activity`, and this session's dev-mods folder.
  - `dev-tools`: `~/Library/Application Support/orca/claude-profiles/<profile>/home/dev-mods/0c226e3e-.../dev-tools`.
  - Compare before deleting anything: `diff -rq ~/orca/mt ai_multi_tool -x 'target*' -x .mt -x .git`.
- **Left out, as build, test or runtime artifacts:**
  - every `target*` dir (about 9.6 GB) and `plugin/bin/mt`;
  - `.mt/` (the index);
  - `bench/coord/repo/` (a copy of the Relay repo used as a fixture: private data);
  - eval results and the generated shop DB;
  - the mods' generated `.claude-plugin/types/`;
  - `~/.claude/dev-tools/` (the coach's logs and a 31 MB transcript cache).

  `.gitignore` covers all of them.
- **Verified in this folder:**
  - `cargo build --release` passes, and all 94 Rust tests pass;
  - `mt-activity`: 2 mod tests pass; `dev-tools`: 3 pass; both pass `claude plugin validate`;
  - the secret scan found no key or token formats.
- **On this machine, still pointing at the old copy:** `~/.local/bin/mt` is a symlink to
  `~/orca/mt/target-plugin/release/mt`. Repoint it after you build here.

## Build and run

```bash
cargo build --release                          # Rust 1.85+, edition 2024; --features bert for BERT embeddings
ln -sf "$PWD/target/release/mt" plugin/bin/mt  # a symlink, or rm + cp (never cp over an existing binary: see Gotchas)
ln -sf "$PWD/target/release/mt" ~/.local/bin/mt
claude --plugin-dir "$PWD/plugin"              # mt plugin
claude --plugin-dir "$PWD/plugin" --plugin-dir "$PWD/mods/coach"
```

- Status line without the mod: `{"statusLine": {"type": "command", "command": "mt activity --statusline"}}`.
- Mod checks: `claude plugin validate mods/<mod>` and `claude plugin test mods/<mod>`. Run `tsc -p mods/<mod>`
  after Claude Code has loaded the mod once (it lays the types).
- Eval: see `bench/eval/README.md`. Set `T_OFF`/`T_ON` to two Orca terminals, then run `bench/eval/setup.sh`.

## Code map (src/)

| module | role |
|---|---|
| `index.rs`, `languages.rs`, `graph.rs`, `resolve.rs` | tree-sitter parsing (language agnostic, queries per language), the SQLite index, the in-memory graph, name resolution |
| `serve.rs`, `texts.rs`, `precache.rs` | the per-repo daemon (Unix socket `.mt/mt.sock`, cookie barrier so replies never lag the disk, background reindexer that touches only changed files, all CPUs), file texts held in memory |
| `query.rs`, `context.rs`, `search.rs`, `vctx.rs` | `mt query` ops, `mt context`, plain-words search, the virtual context (numbered pointers, `mt expand`, dedupe of what the session already saw, working set and preload after a compaction) |
| `apply.rs`, `ops.rs`, `coord.rs` | `mt apply` bundles (`@@` sections: expect, create, replace, find/with, rename, regex, run [gate], format, ...), multi-agent briefs, claims and changes |
| `compress.rs` | log and JSON compression, the JSON graph, the diff outline, the output store, `mt out` (JSONPath, jq via jaq, BM25 questions) |
| `hook.rs` | every Claude Code hook: skill delivery in parts, nudges and redirects (edit scripts, grep on identifiers, MCP batching), output compression of Bash/Read/MCP results |
| `mcp.rs` | `mt mcp list/tools/call`: MCP calls through mt, batched, chained with `$id.path`, results kept |
| `events.rs` | `.mt/events.jsonl` (requests, compressions, MCP calls, reindexes) and `mt activity` |
| `tools.rs`, `tools_ext.rs`, `embed.rs` | `mt tools`: the graph of skills/agents/MCP servers linked to the code, the settings profile, embeddings (Model2Vec, optional BERT) |
| `config.rs`, `init.rs`, `viz.rs` (+ `viz.html`) | settings and presets, `mt init` (writes the "use mt" block into CLAUDE.md/AGENTS.md/GEMINI.md/Cursor), the live browser visualizer |

## What has been measured (details in DEVLOG and docs/RESEARCH.md)

- **G1, a real feature task** ("add Google authentication with tests", Opus 5.5, a Go + TS repo):
  - mt ON took 9-13 turns against 48-58;
  - context processed fell 7.0M → 1.21M tokens (−83%);
  - cost −21 to −26%, wall time about −20%;
  - all runs correct.
- **Context management** (section 49): mt still wins against good context-management habits
  without mt. Context processed 3.90M → 1.21M, turns 41.5 → 11.5, cost $3.90 → $3.00.
  - Adding early compaction on top of mt halves the peak context (188k → 96k) but does not lower cost.
  - The post-compaction preload rarely hit what was read next.
- **Speed, unit benches at 20k files against Claude Code's own bash/ugrep chains:**
  - the whole read phase is 7.8x faster, callers with enclosing functions 42-78x;
  - 8 agents on one daemon: 525 ms against 15.5 s.
- **Tool graph** (`mt tools`): first request −35%, input −47 to −55%, cost −40 to −48%, with equal correctness.
- **MCP through mt** (sections 50-52): the mechanics work.
  - A 3.1 MB SQLite result becomes a JSON graph; a 46k-char graft result becomes ~1.9k tokens;
    `$a.path` chaining works; deny/ask rules are honoured.
  - **Adoption failed:** in paired runs Claude never chose `mt mcp call`, and no saving is shown yet.
- **Logs and diffs** (section 51): a 6.8 MB `git log -p` is now a 1.8k-token outline, where before
  it passed through whole.

## Open problems, in the order I would take them

1. **MCP batching is not adopted.** The skill section and the once-per-session PostToolUse nudge
   did not move Claude off native calls. Try, then measure with `bench/eval`
   (`TASKS=tasks_shop.txt MCPCFG=mcp-shop.json run_mcp_all.sh D1 D2 D3`):
   - a PreToolUse suggestion *before the second native call* to the same server;
   - measuring without the 26k-char skill system prompt, which costs more than small tasks save;
   - task prompts that do not say "only the X MCP tools", which Claude reads as forbidding Bash.

   D2/D3 have never completed. `bench/eval/results/` is not shipped: the reference answers come
   from re-running the SQL in DEVLOG section 52, or regenerate them.
2. **The post-compaction preload** (`vctx::recent_spans`, `hook.rs` `session_start`) picks by
   recency, which missed. Rank spans in files the session wrote (or `mt apply`'d) above plain
   reads, allow more small spans, and re-measure the re-read count after a compaction.
3. **`mt apply --retry`** (proposed, not built): resend only the corrected sections of a failed
   bundle, instead of the whole bundle (one run re-sent 59 KB over one bad hash).
4. **Warm MCP server processes** in the daemon (`mt mcp call` spawns each server per call; npx
   servers take 1-3 s).
5. **Line-level dedupe** (section 47) was inconclusive (it fired once). It needs a task that re-reads.
6. **The tool-graph ranking is overfit:** 24% @3 on an independent gold set against 75% on the
   tuning set. Weakest areas: agent data flow and cross-component chains.

## Gotchas (each cost real time)

- **macOS kills a copied-over binary.** `cp new old`, where `old` is a binary that already ran,
  invalidates its code signature and the next launch dies with exit 137 (SIGKILL), with no stderr.
  Use `rm -f old && cp new old`, or a symlink.
  - This silently made every mt hook fail in two A/B runs (DEVLOG section 51).
  - `bench/eval/run_mcp_pair.sh` now refuses to launch if the frozen binary does not run.
- **`plugin/skills/mt` is a symlink to `skills/mt`.** Edit the skill once; a loop over "both
  copies" edits it twice.
- **Hook context over 10,000 chars spills to a file and costs a turn**, so the hooks deliver the
  skill in parts (SessionStart, then the first prompts and tool results).
  - Anything placed late in SKILL.md arrives late: the MCP section arrives with part 4.
  - Check the part sizes after every skill edit.
- **Output replacement.** `updatedToolOutput` replaces Bash/Read output only in the tool's own
  structured shape; MCP results use `updatedMCPToolOutput`. Say in the replacement that it was
  reduced.
- **Compaction calls are not in the transcript**, so the cost analyzers cannot count them.
  Estimate them separately.
- **`claude --mcp-config` servers are invisible to `mt mcp`.** Set `MT_MCP_CONFIG=<file>` to the
  same file.
- **Profiles:** this machine runs Claude Code under Orca profiles; `CLAUDE_CONFIG_DIR` points
  into `~/Library/Application Support/orca/claude-profiles/<id>/home`, not `~/.claude`.
- **Eval hygiene:**
  - give every run a frozen plugin copy (`setup.sh`), because a rebuild mid-run changes the arm;
  - turn auto-memory off (`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`);
  - use `--setting-sources project --strict-mcp-config`;
  - reset the worktrees between runs;
  - read the answers before trusting a score (a scorer bug once looked like a model gap).
- **`uvx mcp-server-sqlite` does not start** with its current `mcp` library. Use
  `bench/eval/fixture/sqlite_mcp.py`.

## How the owner wants this built (standing decisions)

- Rust only for what ships; Python only for throwaway analysis. Speed first: turns and wall time
  are the cost.
- Language agnostic: nothing specific to Go or any toolchain; formatters and checks are whatever
  the bundle names (`@@ format: <cmd>`, `@@ run`).
- No `mt undo`, no backups. mt only does what it is told: only a step marked `gate` restores files.
  A failed build does not revert anything on its own.
- In-memory everything; scan with all CPUs; background reindexing that holds nothing up and touches
  only what changed; many agents on one daemon without slowing down.
- Claude must batch: one read batch, one write batch. No one-off scripts or loops over files for
  edits (`mt apply` instead).
- Logs and large output always go through mt, then get queried: never `cat`/`tail`/`head` them
  into context.
- Do not interfere with MCP permissions: mt refuses what deny/ask rules cover and never widens them.
- Visibility first: show everything with evidence, no hidden "other" buckets. The dev-tools mod
  stays fully local and separate from Relay.
- Benchmarks are raw and verbose: print every step, no graft in evals, log every test (query,
  results, transactions from Claude and from mt) in DEVLOG.

## Where to look when something breaks

- `mt logs [-f]`: daemon events, daemon logs, hook decisions, `mt apply` step logs, in one view.
- `mt activity`: what mt did per session. `mt visualize`: a live graph in the browser.
- `/tmp/mt-hooks.log` and `<repo>/.mt/hooks.log`: hook decisions. `<repo>/.mt/events.jsonl`: daemon events.
- Hooks failing with "non-blocking status code, no stderr": run the plugin's binary by hand
  (`plugin/bin/mt --version`). Exit 137 is the signature problem above.
