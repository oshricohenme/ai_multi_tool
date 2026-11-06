# mt

**A context engine for coding agents.** mt replaces the `grep` / `sed -n` / `cat` / `perl -i`
chains a coding agent writes to read and change code. You make one call to read everything
related to a symbol, and one checked call to change it. The same binary also shrinks large tool
output and the always-on skill, agent and MCP listings Claude Code sends with every request.

The model's context window is the expensive part: every turn re-reads all of it. Disk, tool
output and listings are only sources. mt works out the smallest update that has to reach the
context and sends only that.

Status: **early, single-author, measured on Claude Code 2.1.318 and 2.1.319 only.** It works
day to day, and the evidence behind it is in [docs/RESEARCH.md](docs/RESEARCH.md). The
interfaces may still change.

## What it does

| Area | Without mt | With mt |
|---|---|---|
| Reading code | 4 to 13 calls of grep, then `sed -n` ranges, then more greps | `mt query` (a batch of questions and claims) or `mt context Name --map`, then one `mt expand` |
| Changing code | one Edit or script per file, line numbers shifting under each other | one `mt apply` bundle: hash-checked, all-or-nothing, runs the formatter and tests, restores files when a gate step fails |
| Large output | full build logs and JSON in context | `mt run -- cmd`: errors and `file:line`s kept; JSON becomes a schema graph you query with JSONPath or jq |
| Tool listings | every skill, agent and MCP description in every request | `mt tools profile`: a per-project settings layer from what the code actually uses, with cards added per prompt |
| Settings | environment variables scattered through the code | `mt config`: guided setup, presets (`lockdown`, `balanced`, `nudge`, `off`), `list/get/set`, `doctor` |
| Seeing it | logs | `mt visualize`: the repo graph lit up live by what the daemon and hooks are doing, plus ten Winamp-style scenes (spectrum, scope, starfield, tunnel, matrix, …) driven by the same activity |

How it works, briefly:

- **Index.** tree-sitter tags queries for 35 languages, plus heuristic and plain-text tiers.
  The index is kept in `.mt/index.sqlite` and updated incrementally.
- **Daemon.** One `mt serve` per session (or per repo) holds the graph in memory. A file
  watcher applies changes as they happen, and a sync barrier makes sure answers never lag the
  disk.
- **Resolution.** Calls are followed to their definitions (code only, same language family,
  nearest definition first), and every hit says how it was matched: `exact`, `by name` or
  `heuristic`.
- **Claude Code plugin.** It starts the session daemon, briefs the model, rewrites old-style
  reads into mt calls in place (no denied turn), and compresses large Bash, Read and MCP
  results. All hooks are in the Rust binary (`mt hook <event>`) and take about 6 to 11 ms.

mt knows no language or ecosystem in particular. There is no toolchain table and nothing
specific to Go or npm: everything comes from grammars, manifests on disk and the code itself.

## Results

From [docs/RESEARCH.md](docs/RESEARCH.md), which has the setup, every run and the threats to
validity. These are headless Claude Code A/B runs with graft, MCP and other helpers stripped;
n is small (2 to 3 repetitions per task).

| Measurement | Result with mt |
|---|---|
| Tool and skill listings, 6 A/B runs (n=12 sessions per arm in runs 3 to 6) | input tokens −36% to −55%, turns −21% to −30%, cost −40% to −48%; 39/39 correct in both arms |
| Code tasks at a realistic 255k context | turns −33% to −42%, cost −1% to −12% |
| Large interactive task ("add Google auth, with tests"), final design, n=1 | 13 turns against 53, cost −21% to −26% |
| Output compression on real logs and JSON | 67% to 99% of tokens removed, code and diffs untouched |
| Read phase at 20,000 files, daemon against the bash chain Claude writes | 288 ms against 1,955 ms (6.8x), a tenth of the bytes |
| Find-everything workflow against graft 0.21.1, 5 tasks | complete in 5/5 with 1 to 2 calls; graft 3/5 with 4 to 13 calls |

Where mt does **not** win, also measured:

- **Small contexts.** At about 19k tokens of context, mt sessions took 13% to 39% fewer turns but
  usually cost 5% to 28% *more*: its structured replies are cache writes that every later turn
  re-reads.
- **Adoption.** With only the skill installed, Claude chose mt in 1 session of 4. The plugin's
  hooks fixed that (4 in 4).
- **Ranking.** `mt tools` ranking is overfit: recall@3 is 37% on 90 independent prompts, against
  72% to 75% on the tuning prompts.

All of this is in the research doc, with n, sources and the threats to validity.

## Install

You need Rust 1.85 or newer (edition 2024) and a C compiler for the tree-sitter grammars.

```bash
git clone <this repo> mt && cd mt
cargo build --release              # add --features bert for the optional BERT embeddings
cp target/release/mt ~/.local/bin/ # or anywhere on PATH
mt --version
```

To use it with Claude Code, point the plugin's `bin/mt` at your build and load the plugin:

```bash
ln -sf "$PWD/target/release/mt" plugin/bin/mt
claude --plugin-dir "$PWD/plugin"
```

This repo is also a Claude Code marketplace with two plugins: `mt` (the hooks, the skill, a status
line and an `/mt-activity` pane) and `coach` (Claude coach: every model and tool call in a pane and a
local web app, `/coach`). Install either or both; `mt` finds the binary at `plugin/bin/mt`, else on PATH:

```
/plugin install mt --marketplace oshricohenme/ai_multi_tool
/plugin install coach --marketplace oshricohenme/ai_multi_tool
```

[docs/USAGE.md](docs/USAGE.md) covers permanent install, every command, the configuration
variables and troubleshooting.

## A first session

```bash
cd your-repo
mt config                                  # guided setup: pick a preset, tune what you like
mt build                                   # index up to date, daemon warm (optional; first use does it)
mt visualize &                             # watch it work in the browser
mt context handleLogin --map               # one line per related item, numbered
mt expand 3 7                              # the bodies of items 3 and 7
mt query - <<'EOF'
[{"id": "def", "defined_in": "WriteAudit", "expect": "internal/store/audit.go"},
 {"id": "up",  "callers": "$def", "depth": 2},
 {"id": "t",   "tested_by": "WriteAudit"}]
EOF
mt run -- go test ./...                    # long output, compressed; errors kept
```

With the plugin loaded you don't type any of this. Claude does, because the session briefing
and hooks steer it there.

## Documentation

- [docs/USAGE.md](docs/USAGE.md): install, connecting agents, every setting, every command,
  `mt query` ops, the `mt apply` bundle, output stores, the visualizer, `mt tools`,
  troubleshooting.
- [docs/RESEARCH.md](docs/RESEARCH.md): the evidence, with methods, every A/B, rejected designs
  and threats to validity.
- [docs/HISTORY.md](docs/HISTORY.md): a readable index of the development log, one entry per
  section with its outcome.
- [DEVLOG.md](DEVLOG.md): the full development log, 45+ dated sections with raw numbers and
  transcripts.
- [skills/mt/SKILL.md](skills/mt/SKILL.md): the reference the model reads.

## Repository layout

```
src/        the binary: index, graph, resolve, query, context, apply, serve, hook, compress, tools,
            config, events, viz (+ viz.html, compiled in)
tests/      integration tests (speed, compression, map, briefing, virtual context)
plugin/     Claude Code plugin: hooks.json, bin/mt, skills/mt
skills/mt/  the model-facing reference
bench/      benchmark harnesses, task definitions, gold sets, summarised results
```

## License

MIT, see [LICENSE](LICENSE).
