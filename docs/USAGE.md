# Using mt

This guide covers installing mt, connecting it to Claude Code, every command, every setting, the
live visualizer and troubleshooting. The model-facing reference (what the agent reads) is
[skills/mt/SKILL.md](../skills/mt/SKILL.md); `mt ref` prints it section by section.

- [1. Install](#1-install)
- [2. Connect it to your agent](#2-connect-it-to-your-agent)
- [3. Configure: `mt config`](#3-configure-mt-config)
- [4. Commands](#4-commands)
- [5. `mt query`: many questions in one call](#5-mt-query-many-questions-in-one-call)
- [6. `mt apply`: one checked change across many files](#6-mt-apply-one-checked-change-across-many-files)
- [7. Large output: `mt run`, `mt compress`, `mt out`](#7-large-output-mt-run-mt-compress-mt-out)
- [8. Watching it work: `mt visualize` and `mt logs`](#8-watching-it-work-mt-visualize-and-mt-logs)
- [9. Tool and skill listings: `mt tools`](#9-tool-and-skill-listings-mt-tools)
- [10. Files mt writes](#10-files-mt-writes)
- [11. Troubleshooting](#11-troubleshooting)

---

## 1. Install

Requirements: Rust 1.85 or newer (edition 2024) and a C compiler (the tree-sitter grammars are C).
macOS and Linux are fully supported. On Windows every command runs in-process, because the daemon
needs Unix sockets.

```bash
git clone <this repo> mt && cd mt
cargo build --release
install -m 755 target/release/mt ~/.local/bin/mt   # any directory on PATH
mt --version
mt config doctor                                   # checks the binary, repo, index, daemon, hooks
```

Optional builds:

| Feature | Build | What it adds |
|---|---|---|
| BERT embeddings | `cargo build --release --features bert` | bge-small vectors for `mt tools` ranking (pure Rust, CPU, via candle). Download the model into `~/.cache/mt/models/` and run `mt tools embed`. |
| Model2Vec (default) | nothing to build | static embeddings; put `minishlab/potion-base-8M` in `~/.cache/mt/models/potion-base-8M` to enable them. Without a model, ranking uses words only. |

## 2. Connect it to your agent

### Claude Code (recommended: the plugin)

The plugin in `plugin/` does four things:

- **SessionStart** starts this session's `mt serve` and exports `MT_SESSION` / `MT_SOCKET` to every
  Bash call. It also briefs the model with the reference.
- **PreToolUse** catches old-style code reads (`sed -n` ranges, `cat`/`head` of code,
  identifier greps). In `strict` mode it rewrites a lone read into the matching mt call in
  place, so no turn is lost; anything else is denied, with the exact mt command in the reason.
- **PostToolUse** shrinks large Bash, Read and MCP results.
- **SessionEnd** stops the daemon.

```bash
ln -sf "$(command -v mt)" plugin/bin/mt     # the plugin runs bin/mt; point it at your build
claude --plugin-dir /path/to/mt/plugin      # one session
```

For every session, add the plugin through your Claude Code plugin marketplace setup, or put
`--plugin-dir` in your shell alias. Hooks take about 6 to 11 ms each.

### Any agent (Codex, opencode, Gemini, Cursor, Claude Code without the plugin)

```bash
mt init            # writes a marked "use mt for code" block into CLAUDE.md, AGENTS.md, GEMINI.md
                   # and .cursor/rules/mt.mdc (when the repo uses Cursor); idempotent
mt init --dry-run  # show what it would write
mt init --remove   # take the block out again; everything outside the markers is untouched
```

Without the plugin's hooks, models choose mt less often: 1 session in 4 with the skill alone,
against 4 in 4 with the hooks ([RESEARCH.md](RESEARCH.md)).

## 3. Configure: `mt config`

Every setting mt has is an `MT_*` environment variable. `mt config` puts them in one place and
saves them in a file, so you never have to export anything.

```bash
mt config                  # guided setup on a terminal: pick a preset, then go through each group
mt config list             # every setting, its value, and where it came from
mt config get enforce      # one setting: value, source, default, allowed values, what it does
mt config set enforce nudge
mt config set compress_lines 200 --global   # --global writes your user file instead of the repo's
mt config unset enforce
mt config preset lockdown  # lockdown | balanced | nudge | off
mt config env              # the effective settings as export lines (CI, shells)
mt config doctor           # what works and what doesn't, each with its fix
mt config path             # where the files are
```

**Where settings come from**, highest first:

1. the project file when it is **locked** (`"locked": true`, which `preset lockdown` sets);
2. the environment (`MT_ENFORCE=off mt ...`);
3. the project file, `<repo>/.mt/config.json`;
4. the user file, `$XDG_CONFIG_HOME/mt/config.json`, else `~/.config/mt/config.json`;
5. the built-in default.

New mt calls see a change at once. A running daemon keeps its settings until it restarts
(`mt serve --stop`, or `mt rebuild`).

**Presets**

| Preset | For | What it sets |
|---|---|---|
| `lockdown` | teams that want the measured savings on every session | strict hooks with in-place rewrites, skill briefing, compression, daemon, folding, expand hints, line dedupe, and `locked`, so neither env vars nor user config can turn them off for this repo |
| `balanced` | the default | the defaults below |
| `nudge` | trying mt out | hooks allow old-style reads and add a hint instead of blocking |
| `off` | opting out per repo | hooks don't interfere, no daemon is started, no compression; the CLI still works when called |

**Every setting**

| Key | Variable | Default | Values | What it does |
|---|---|---|---|---|
| **Agent hooks** |||||
| `enforce` | `MT_ENFORCE` | strict | strict, nudge, off | what PreToolUse does with old-style code reads |
| `rewrite` | `MT_REWRITE` | on | on, off | in strict mode, rewrite a lone read into mt in place (off: deny instead) |
| `inject_skill` | `MT_INJECT_SKILL` | on | on, off | brief the model with the reference at session start |
| `server_scope` | `MT_SERVER_SCOPE` | session | session, repo, off | one daemon per session, one per repo, or none from the hook |
| `compress` | `MT_COMPRESS` | on | on, off | PostToolUse compression of large Bash, Read and MCP results |
| `compress_lines` | `MT_COMPRESS_LINES` | 120 | number | line budget of a compressed result |
| **Daemon** |||||
| `daemon` | `MT_NO_DAEMON` | on | on, off | answer from the in-memory daemon (off: every call opens the index) |
| `idle_minutes` | `MT_IDLE_MIN` | 30 | number | the daemon exits after this many idle minutes |
| **Replies** |||||
| `max_output` | `MT_MAX_OUTPUT` | 28000 | number | hard cap on one reply in characters (Claude Code spills over ~30k to a file) |
| `fold` | `MT_NO_FOLD` | on | on, off | fold long caller and test bodies to the lines that matter |
| `expand_hints` | `MT_NO_EXPAND` | on | on, off | numbered pointers and the `mt expand` footer |
| `line_dedupe` | `MT_LINE_DEDUPE` | on | on, off | don't resend lines this session has already seen unchanged |
| **Kept output** |||||
| `out_dir` | `MT_OUT_DIR` | `<repo>/.mt/out` | path | where kept outputs live |
| `out_name` | `MT_OUT_NAME` | from the command | text | store name for unnamed output (one per agent keeps data apart) |
| `jq_seconds` | `MT_JQ_SECONDS` | 10 | number | time limit for one `mt out --jq` filter |
| **Tools graph** |||||
| `tools_db` | `MT_TOOLS_DB` | `~/.cache/mt/tools.sqlite` | path | the skills/agents/MCP catalog |
| `embed` | `MT_EMBED` | text | text, graph, off | embeddings in tool ranking |
| `embed_model` | `MT_EMBED_MODEL` | potion-base-8M | path | embedding model directory |
| `chain_cards` | `MT_CHAIN` | on | on, off | client call → route → handler → store chain cards for cross-layer prompts |
| `wire_keys` | `MT_WIRE` | on | on, off | link sub-projects by shared literals |
| **Diagnostics** |||||
| `timing` | `MT_TIMING` | off | on, off | per-stage timings |
| `debug` | `MT_DEBUG` | off | on, off | parser and query diagnostics |
| `hook_debug` | `MT_HOOK_DEBUG` | off | on, off | log every PostToolUse payload |

`MT_EVENTS=0` turns off the event log that `mt visualize` and `mt logs` read.

## 4. Commands

| Command | What it does |
|---|---|
| `mt context <Name> [--map] [--edit]` | a symbol and everything related to it: callers with call sites, types, callees, unit and integration tests (including tests that reach it through an HTTP route), tables and migrations, docs. `--map` gives one line per item and no bodies; `--edit` gives the target plus every referencing line |
| `mt expand <n> ...` | open numbered pointers from the last reply (or `path:Ls-e@hash`); hash-checked, and a moved block is found again by name |
| `mt query [file\|-]` | a batch of questions and claims in one call (section 5) |
| `mt span <file> <query>` | one block: a name, `Type.method`, a key path (`scripts.build`), a markdown heading, or `/regex/` |
| `mt skeleton <file>` | every definition in a file with line ranges, no bodies |
| `mt apply [file\|-] [--dry-run]` | one checked change across many files (section 6) |
| `mt run -- <cmd>` | run a command and compress its output, keeping the exit code |
| `mt compress [--label L] [--name S]` | compress stdin (logs, test output, JSON, an MCP response) |
| `mt out [id] [question] [--json P] [--jq F] [--grep RE] [--lines A-B] [--level error]` | query a kept output |
| `mt show <file\|->` | show text to the user, not the model (the model gets a one-line receipt) |
| `mt index [--rebuild]` | build or refresh `.mt/index.sqlite` (incremental) |
| `mt build [dir]` | make a repo ready: index up to date and daemon warm |
| `mt rebuild [dir]` | from scratch: stop the daemon, drop the index, index again, restart. Kept outputs and settings stay |
| `mt serve [--status\|--stop]` | the per-repo daemon (normally started for you) |
| `mt logs [-n N] [--source events\|serve\|hooks\|apply] [--session ID] [-f]` | one view of the event log, daemon logs, hook decisions and apply runs |
| `mt visualize [--mode M] [--port P] [--no-open]` | the live view in the browser (section 8) |
| `mt config ...` | settings (section 3) |
| `mt init [dir] [--dry-run] [--remove]` | tell every agent in a repo to use mt for code |
| `mt tools ...` | the skills/agents/MCP graph (section 9) |
| `mt ref [section]` | the model-facing reference |
| `mt langs` | supported languages and how each file type is handled |
| `mt tree <file>` | the syntax tree, for writing your own `.mt/queries/<lang>.scm` |

## 5. `mt query`: many questions in one call

State what you believe and let mt check it. A claim that holds costs one line; a claim that fails
comes back with the difference.

```bash
mt query - <<'EOF'
[
 {"id": "def",   "defined_in": "WriteAudit", "expect": "internal/store/audit.go"},
 {"id": "shape", "has_field": "AuditEntry", "expect": ["ActorID", "Action", "RequestID"]},
 {"id": "sites", "constructs": "AuditEntry", "expect": ["auditWrite", "auditRead"], "exhaustive": true},
 {"id": "up",    "callers": "$def", "depth": 2},
 {"id": "tests", "tested_by": "WriteAudit"},
 {"id": "code",  "span": "internal/store/audit.go:WriteAudit"}
]
EOF
```

Ops: `context`, `defined_in`, `exists`, `references`, `callers` (`depth` up to 4), `tested_by`,
`constructs`, `has_field`, `calls`, `uses_table`, `property` (one field across every spelling
and layer, from SQL column to UI), `map`, `module`, `grep`, `span`. `"$id"` as a subject chains
on an earlier query within the same call. Independent queries run in parallel. Every result says
how it was matched (`exact`, `by name`, `heuristic`).

## 6. `mt apply`: one checked change across many files

One bundle holds every edit in every file, the new files, and the steps that prove the change:

```
@@ expect
[{"id": "c", "callers": "loadUser", "expect": ["showUser", "editUser"], "exhaustive": true}]
@@ replace internal/user/load.go:L10-24 0db616c6
func loadUser(ctx context.Context, id string) (*User, error) {
...
@@ rename loadUser LoadUser
@@ regex in internal/ ext go expect 12
oldLogger\.(\w+)
@@ with
log.$1
@@ format: gofmt -w {changed}
@@ run gate build: go build ./...
@@ run unit: go test {changed_dirs}
```

- Every op addresses the file as it was read, so ten edits to one file never shift each other.
- Nothing is written unless every op is valid. A changed hash, a `find` that matches zero times
  or twice, or a regex count different from `expect` refuses the whole batch and names the op.
- A failing step marked `gate` restores the files. Other failing steps keep the edits, so you can
  fix forward. mt never decides on its own to revert.
- `#3` refers to item 3 of the last reply; `@@ move #3 to pkg/b.go` moves code without retyping it.

## 7. Large output: `mt run`, `mt compress`, `mt out`

```bash
mt run -- go test ./...                       # errors, failures and file:line kept; passing runs folded
gh api repos/o/r/issues | mt compress --label "issues"
mt out 3 --json '$.items[?@.state=="open"].title'   # RFC 9535 JSONPath
mt out 3 --jq '[.items[] | select(.comments > 5) | .number]'
mt out 3 "which tests failed and why"         # plain words, BM25-ranked
mt out                                        # list kept outputs
```

JSON over about 1,500 characters never enters the context whole. You get its graph (paths, types,
counts, small value sets) and query only what you need. Outputs are kept in named stores under
`.mt/out/` for 7 days.

## 8. Watching it work: `mt visualize` and `mt logs`

```bash
mt visualize                  # opens http://127.0.0.1:<port>/ in your browser
mt visualize --mode spectrum  # open on a given scene
mt visualize --port 7777 --no-open
```

The page shows the repo as a graph and lights up whatever the daemon and the Claude Code hooks
are doing, as it happens. Every request, reindex, daemon start and hook decision streams in from
`.mt/events.jsonl` and this repo's hook log, `.mt/hooks.log`. The activity feed
lists each call with its time, latency, size and the files it touched. The header keeps running
totals, and the footer has a 2-minute timeline.

**Scenes.** Pick one with the buttons in the footer, keys `1`-`0` and `-`, `m` / `←` `→` to step,
or `a` to change scene every 20 s. The choice is kept in the URL (`#spectrum`) and remembered by
the browser. Every scene is driven by the same signal: the repo is cut into 32 bands (neighbouring
bands are neighbouring code), each call adds energy to the bands of the files it touched, and
every call is a beat.

| Key | Scene | What you see |
|---|---|---|
| 1 | `graph` | directory islands of files linked by references; files flare and comets run along the path a call walked. Click a file for its definitions, `/` to search, `f` to fit, drag to pan, scroll to zoom, click a legend entry to isolate a directory |
| 2 | `spectrum` | the classic LED bar analyzer, one bar per slice of the repo, with falling peaks |
| 3 | `scope` | a phosphor oscilloscope: the bands summed into one waveform |
| 4 | `radial` | a circular spectrum around a core that beats with every call |
| 5 | `starfield` | warp speed that follows the call rate; touched files fly past |
| 6 | `tunnel` | MilkDrop-style feedback vortex |
| 7 | `matrix` | code rain spelled from the names being read |
| 8 | `waterfall` | a spectrogram of the last 30 seconds |
| 9 | `fireworks` | one rocket per call; the burst grows with the files touched |
| 0 | `plasma` | demoscene plasma whose speed and tint follow the activity |
| - | `leds` | every indexed file as one LED, lit in the colour of what mt did to it |

Colours: teal for reads, gold for writes (`mt apply`), amber for reindexing, violet for hooks,
red for failures.

The server listens on 127.0.0.1 only and answers only `Host: localhost` / `127.0.0.1`, so other
sites can't read it. Writing an event costs a few microseconds, after the reply has already been
sent.

`mt logs` is the terminal view of the same stream: `mt logs -f` follows it, and
`--source hooks` shows only hook decisions.

## 9. Tool and skill listings: `mt tools`

Claude Code sends every skill, agent and MCP description with every request. `mt tools` builds a
graph of them, plus your code, and turns off what the project never uses:

```bash
mt tools index               # catalog from disk and each MCP server's tools/list
mt tools stats               # what the always-on listings cost here
mt tools profile             # the settings layer it would write, with evidence per item
mt tools profile --write     # merge it into .claude/settings.local.json (backup kept)
mt tools profile --undo      # byte-exact restore
mt tools enable <name>       # bring one item back
mt tools find "add a stripe webhook"   # rank skills/agents/tools for a task
mt tools explain             # take a session's first request apart, item by item
```

## 10. Files mt writes

| Path | What | Safe to delete |
|---|---|---|
| `<repo>/.mt/index.sqlite` | the index | yes; rebuilt on the next call |
| `<repo>/.mt/config.json` | project settings | yes |
| `<repo>/.mt/events.jsonl` | the event log (rolls over at 4 MB) | yes |
| `<repo>/.mt/out/` | kept outputs (7-day prune) | yes |
| `<repo>/.mt/.gitignore` | `*`, so nothing in `.mt/` is committed | |
| `~/.config/mt/config.json` | user settings | yes |
| `~/.cache/mt/` | tools catalog, embedding models and vectors | yes |
| `$TMPDIR/mt-*.sock`, `/tmp/mt-s-*.sock` | daemon sockets | stop the daemon first |
| `<repo>/.mt/hooks.log` | this repo's hook decisions, each with the rule that fired | yes |

## 11. Troubleshooting

Start with `mt config doctor`. It checks each item below and prints the fix.

| Symptom | Cause and fix |
|---|---|
| The agent keeps using grep/sed | the plugin isn't loaded (`.mt/hooks.log` has no recent lines), or `enforce` is `off`/`nudge`. Load the plugin, or `mt config preset lockdown` |
| `changed since it was read` from `mt apply` | the file changed after the read; read the span again and resend the batch |
| Answers look stale | they can't be: every request waits for the watcher to confirm the disk. If the watcher is stuck, `mt rebuild` |
| `stale` / the daemon restarts | the mt binary changed; the old daemon exits by design and the next call starts the new one |
| A setting has no effect | the daemon was started before the change: `mt serve --stop`. Or the project config is `locked` |
| `mt visualize` shows "no indexed files" | run `mt index` or `mt build` in the repo first |
| `bind ...: path must be shorter than SUN_LEN` | `MT_SOCKET` points at a path that is too long for a Unix socket; use a short one under `/tmp` |
| Hook output too large / spilled to a file | `max_output` above ~30,000 characters makes Claude Code write results to a file; keep the default |
