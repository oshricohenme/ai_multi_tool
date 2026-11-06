# mt developer log

A record of how `mt` was designed, built and measured: the trials, the results, the bugs, and
the dead ends. Every number below was measured on one developer's machine (macOS, Apple
Silicon) against real repositories and real Claude Code sessions, unless it is marked as
modelled. Commands and scripts are named so each result can be reproduced.

---

## 2026-11-05 · 1. Why: what Claude Code actually spends its tokens on

**Question.** Bash and grep are the tools Claude Code uses most. Is there a faster, more
token-efficient way?

**Method.** Parsed all 2,258 Claude Code transcripts on disk (`~/.claude/projects/**/*.jsonl`,
3,562 files, 6.9 GB, the last 90 days) and summed tool-result sizes per tool and per Bash
command class (`tooluse.py`).

**Results.**

| Source | Share of all tool output |
|---|---|
| Bash | 75.5% |
| Read tool | 17.4% |
| Everything else | 7.1% |

| Inside Bash | Share of Bash output |
|---|---|
| cat / head / tail / sed -n | 43% |
| grep / rg | 41% |
| tests, builds, runs | 9% |

- 57% of all tool output lands in subagents, not the main thread.
- Re-reads of a path already read in the same session: 2,372 calls (5% of bytes). 96% of them
  had no edit in between.
- Claude Code already caps a tool result at about 30k characters. The cost sits in the median,
  around 1k characters per call, multiplied by 76k calls.

**Why Claude makes these calls** (`why.py`, `intent.py`, `grepwindow.py`):
- In bypass-permissions mode the harness tells the model to use cat, grep and sed instead of
  Read, Grep and Edit. 51% of sessions never call the Read tool.
- 72% of greps target a single known file. Grep is mostly a line-number lookup before `sed -n`.
- 57% of grep patterns are alternations (`a\|b\|c`): the model batches questions to save turns.
- After a grep, the next call is another grep 27% of the time and a read of a file the grep hit
  only 5% of the time. Reads of import-linked files after a grep were rare, about 4%. Reads of
  the matching test file were 0.3%.
- During edits the picture changes. 46% of reads in an editing turn happen between the first and
  the last edit, and 43% of those target a file that gets edited.

**The hidden cost is turns, not bytes** (`turncost.py`):

| Turn kind (main thread) | Turns | Median output tokens | Median latency |
|---|---|---|---|
| Read or grep only | 17,005 | 403 | 3.8 s |
| Other tool | 17,375 | 526 | 4.0 s |
| Edit | 1,761 | 2,184 | 5.3 s |

The median context at a tool call is 188k tokens, and 261k at a grep. Every extra turn re-reads
all of it.

**Decision.** Build one tool that returns everything related to a symbol in one call, and applies
every edit in one call.

---

## 2026-11-05 · 2. `mt span`: one call instead of grep plus sed

**Built.** A Rust CLI. `mt span <file> <symbol|#heading|/regex/>` returns exactly the enclosing
definition, using tree-sitter, with line numbers and a content hash. A miss returns the file's
outline, so the second call lands.

**Benchmark** (`bench/bench.py`). 297 random definitions from 5 repos, in Go, Python, C#,
TypeScript and Markdown. The ground truth comes from an independent brace, indentation or
heading parser.

| Approach | Round trips | Complete | Precision | Median bytes | Total bytes | Median time |
|---|---|---|---|---|---|---|
| mt span | 1 | 100% | 95% | 831 | 511 KB | 6.7 ms |
| grep, then sed 60 lines | 2 | 88% | 35% | 2,583 | 930 KB | 54 ms |
| grep, then sed 150 lines | 2 | 92% | 31% | 4,167 | 1,398 KB | 54 ms |
| grep, then cat the file | 2 | 100% | 16% | 8,520 | 4,283 KB | 54 ms |

The bench and mt disagreed on 22 of 297 cases. In every case checked by hand, the bench's parser
was wrong: Python docstrings at column 0, and TypeScript destructured parameters.

---

## 2026-11-05 · 3. All languages: one engine over tree-sitter tags queries

**Built.** Language support in three tiers:
1. **Tree-sitter.** 35 bundled grammars. Definitions come from `@definition.*` / `@name` tags
   captures, the convention GitHub code navigation uses. Each language gets the shipped tags
   query plus mt's own patterns for what it misses: C# properties, fields, constructors and
   records, Go var/const, TS type aliases and enums, and others. Data and markup formats are
   covered too: JSON, YAML and TOML keys, HTML ids, XML elements, CSS rules and Makefile targets.
2. **Heuristic.** Any other file that looks like code: definitions found by keyword, spans by
   braces or indentation. Kinds are shown as `~kind` to mark them approximate. Covers Unity
   shaders, for example.
3. **Text.** Logs and prose. `/regex/` returns line windows.

Extra patterns can be added per repo in `.mt/queries/<lang>.scm`.

**Engineering notes.**
- One bad pattern makes tree-sitter reject a whole query, so patterns are compiled one by one
  and broken ones are dropped. `MT_DEBUG=1` prints the dropped ones, and a unit test fails if a
  language loses every pattern.
- Compiling a query costs time on every process start, so the whole query is compiled first and
  split only on failure. A one-line C# file went from 94 ms to 17 ms, and TypeScript from 74 ms
  to 29 ms.
- The C and C++ tags queries mark the declarator (the signature) as the definition, so mt climbs
  to the owning definition. Nodes that end at column 0 of the next line are trimmed back. A
  shebang line is never treated as a doc comment.
- Known gap: the bundled Kotlin grammar mis-parses some `object` declarations.

19 unit tests cover every language family.

---

## 2026-11-05 · 4. Serverless index, `context`, `apply`

**Server or not?** Measured first:

| | Time |
|---|---|
| mt process start | 4 ms |
| span on a Go file | 10 to 13 ms |
| stat every file: claude_coach, 551 files | 10 ms |
| stat every file: watchly, 5,697 files | 130 ms |
| stat every file: fps_europe, 534,853 files | 10.8 s |

A server only pays off for very large repos. Decision: serverless now, server later.

**`mt index`** writes `.mt/index.sqlite`, a self-gitignoring folder. It stores files with their
roles (source, test, integration, migration, model, query, doc, config), definitions,
identifier references per file, and table facts (CREATE TABLE and ORM mappings as definitions;
FROM, JOIN, INTO and UPDATE as uses). Every command refreshes it incrementally. Parsing runs on
all cores. Cold builds drop the secondary indexes and rebuild them afterwards.

| Repo | Files | Definitions | Cold build | Warm, no change |
|---|---|---|---|---|
| claude_coach | 486 | 6,957 | 198 ms | 6 ms |
| super_scheduler | 1,748 | 33,411 | 628 ms | 18 ms |
| watchly | 5,077 | 130,178 | 1,993 ms | 72 ms |

**`mt context <Name>`** returns:
- the target, and the callers with their exact call lines
- types in the signature, and callees named in call syntax
- unit tests, and integration tests (direct, through a caller, or through an HTTP route the caller serves)
- the data layer: CREATE TABLE, migrations, models and queries for tables the code touches,
  including through SQL in constants
- docs

Every item says why it was included and carries its hash. Bodies are inlined in priority order
until a budget is spent, and the rest become pointers. The output is capped under Claude Code's
tool-result limit by shrinking automatically. It ends by stating that the reference list is
complete.

**`mt apply`** takes a JSON batch of span replace/delete/insert, find/with, regex, create and
rename ops. Every op becomes a byte range on the original file, so ops never shift each other's
line numbers. It checks hashes, validates everything in parallel and writes nothing unless every
op is valid. It backs up the originals, writes in parallel and reindexes. It then runs the
`check` command and rolls back if that fails. `mt undo` reverts the last batch.

**Bugs found while building**, all fixed:
- Identifiers in comments produced false callees and tables. Code is now mined only outside comments.
- Same-named types and methods from unrelated packages were included. A cross-directory
  definition now counts only if the target file names its package, directory or file stem.
- Prose in markdown and YAML produced fake "table definitions". Those now come only from
  migration, model, query and source files.
- Files under a `dbq/` folder were classed as query files before their `.md` extension was checked.
- `apply` used the index without refreshing it, so renames missed files changed outside mt.
- Renames silently changed docs. They now list docs and leave them alone unless `"scope": "all"`.
- Output past 30k characters was silently truncated by Claude Code. It is now capped at 28k and
  shrinks automatically.
- Writing to a closed pipe, as in `mt ... | head`, panicked.
- `Store.X` also matched `fakeStore.X`, because "Store" is a substring of "fakeStore".
- Test fakes became targets. Real code now wins.
- Local variables and wrong `.Get` methods appeared as callees.

---

## 2026-11-05 · 5. Scripted benchmark: the traditional workflow against mt

`bench/agent_bench.py` (log: `bench/agent_bench_last.log`).

**Part A.** 30 symbols from 3 repos. The traditional side replays what the transcripts show
agents doing: grep, then a definition window, then windows on up to 5 caller files and 3 test files.

| | Calls | Command time | Modelled agent time | Modelled context cost | Whole definition | Callers found | Tests found |
|---|---|---|---|---|---|---|---|
| Traditional | 6.2 | 234 ms | 19.5 s | 167k | 50% | 83% | 92% |
| mt | 1.0 | 39 ms | 3.8 s | 53k | 100% | 98% | 100% |

Modelled values use this user's transcript medians: a 3.8 s model turn, a 261k base context and
403 output tokens per turn. Price ratios: cache read 0.1x, cache write 1.25x, output 5x.

**Part B.** 4 renames on a scratch copy of claude_coach, each followed by `go build`. The
traditional run took 8 to 14 calls, mt took 2. The diffs were identical and every build passed.
Inside `mt apply`, validation took 4 ms, writes 1 ms and reindexing 13 to 21 ms. The rest was
the Go build, about 3 s.

---

## 2026-11-05 · 6. Real Claude sessions: does Claude use mt, and does it help?

**Harness.** `run.sh` and `analyze.py` run headless `claude -p` sessions on Opus 5.5 with
project-only settings, no MCP servers, and each run in a fresh repo copy. The analyzer reports
every tool call, the context per turn, real usage and cost, and correctness checks.

**Tasks.**
- **T1 (read).** List every `decodeJSON` call site, what it decodes into, and the tests that cover it.
- **T2 (change).** Add a parameter to `decodeJSON` and update all 19 callers, with build and vet passing.

**Round 1: contaminated.** Claude kept calling `graft`. The leaks, found one at a time:
- the `graft` binary on PATH
- the repo's own `.claude/settings.json` hooks, which load even under project-only settings
- `AGENTS.md`, injected by a plugin that loads even in isolated runs
- graft's `graft/` card folder

**Raw setup.** `graft` stubbed first on PATH; agent-tool config, `AGENTS.md`, `GEMINI.md` and
`graft/` deleted from each copy. A probe confirmed user-level instructions do not load. Zero
graft attempts in all 16 final runs.

**Harness bug.** On macOS's bash 3.2, an empty array under `set -u` is an unbound variable, so
8 runs died before Claude started, with the error lost to `/dev/null`. Fixed with
`${extra[@]+"${extra[@]}"}`.

**Conditions:** A = no mt; B = mt and skill; C = B plus a one-line session note; D = mt
required by instruction. Two repetitions each, means shown. All 16 runs were correct.

| Task | Condition | mt used | Turns | Tool calls | Wall time | Cost | Output tokens |
|---|---|---|---|---|---|---|---|
| T1 | A | 0 of 2 | 11.0 | 10.0 | 79.6 s | $0.411 | 5,880 |
| T1 | B | 1 of 2 | 10.0 | 8.5 | 64.7 s | $0.422 | 5,472 |
| T1 | C | 2 of 2 | 8.5 | 6.5 | 57.5 s | $0.389 | 4,410 |
| T1 | D | 2 of 2 | 8.5 | 6.5 | 64.5 s | $0.384 | 4,404 |
| T2 | A | 0 of 2 | 6.0 | 5.0 | 37.4 s | $0.148 | 1,707 |
| T2 | B | 0 of 2 | 6.0 | 5.0 | 42.6 s | $0.149 | 1,550 |
| T2 | C | 2 of 2 | 6.0 | 4.0 | 39.6 s | $0.231 | 1,451 |
| T2 | D | 2 of 2 | 5.0 | 3.0 | 50.9 s | $0.270 | 2,524 |

**Findings.**
- **Adoption.** With only the skill installed, Claude chose mt in 1 of 4 sessions. A one-line
  session note was enough for consistent use.
- **Reading.** With mt, T1 took about 25% fewer turns, wall time and output tokens. Cost was
  flat, because these sessions start at only 19k tokens of context. At realistic context sizes
  each saved turn avoids re-reading about 240k tokens; that is still to be measured.
- **Changing.** With mt, T2 cost more. The control's single `perl` edit across 7 files is hard
  to beat. mt's full context inlined caller bodies the edit didn't need, so cache writes were
  25k against 11k. The batch had one op per call site, so output was 2.5k against 1.7k tokens.
- **Mt's output missed nothing Claude needed, but Claude didn't trust it.** It ran a confirming
  grep, and dug by hand for tests that reach handlers through HTTP routes.

**Changes made from these findings:**
- route-linked tests
- an explicit "complete" line at the end of context output
- `--edit` mode: target body plus every reference with two lines either side. 11.5k characters
  against 20k for `Store.WriteAudit`.
- a `regex` op with `refs:`, so one op covers every call site, and an `expect:` count guard
- a shorter skill

---

## 2026-11-05 · 7. A change across several files impacted by one query

**Task T3.** Record the request ID on every audit row. That means:
- a migration pair, up and down
- a field on `api.AuditEntry`
- `request_id` in both `INSERT INTO audit_log` statements in `Store.WriteAudit`
- reading it back in `Store.ListAudit`
- setting it at every request-handling call site, from the existing httpx helper

Checked by `verify_t3.sh` (8 checks, including `go build ./...` and `go vet ./...`).

**Before running**, three optimizations from section 6 were built:
- `mt context --edit`
- the `regex` op with `refs:` and an `expect:` count guard
- a shorter skill

Two ranking bugs found while preparing were also fixed: `Store.X` matched `fakeStore.X`, and
test fakes became targets.

**Runs.** Raw setup, one session each, Opus 5.5.
- A: no mt.
- D: mt stated in the base prompt.

| | A, no mt | D, mt stated | Change |
|---|---|---|---|
| Turns | 10 | 7 | -30% |
| Tool calls | 9 | 5, one of them the skill load | -44% |
| Wall time | 81.1 s | 49.4 s | -39% |
| Cost | $0.422 | $0.310 | -27% |
| Output tokens | 5,969 | 3,216 | -46% |
| Cache read | 283,725 | 142,834 | -50% |
| Cache write | 30,713 | 27,105 | -12% |
| Context per turn | 18k to 42k | 18k to 38k | |
| Checks passed | 8 of 8 | 8 of 8 | |
| AuditEntry sites setting RequestID | 6 of 6 | 4 of 6 | |

**What each run did.**
- **D** loaded the skill, then ran `mt context AuditEntry --edit` together with `ls` and a skeleton
  of the httpx package. It ran three `mt span` reads in one call, then one more read. It then made
  one `mt apply` that created both migration files and edited 4 files.
- **A** made seven grep/sed reads, then a heredoc for the migration and `sed -i` edits. It had to
  `git checkout` a file to undo a broken edit, then finished with a python edit script.

**The difference in thoroughness.** The control also set RequestID in two places that build an
audit entry while serving a request through `ctx`: the WebSocket subscribe and the login audit. mt
showed both sites, with their exact lines, in the D session's `mt context AuditEntry --edit`
output, and stated that list was complete. Claude read them and judged that they were not "built
from an HTTP request". The control read more of the code around them and concluded the `ctx`
descends from the request. The task wording was ambiguous. The verifier did not count those
sites at first.

**Caveats.** One run per condition. Earlier rounds showed run-to-run swings of about 30% in tool
calls. Sessions start at 18k context, against a 261k median in real sessions.

---

## 2026-11-05 · 8. One shot: every change, the tests, and what to run afterwards

**Why.** In every real session the loop was: edit, run tests, read the failure, re-read the
code, edit again. Each step is a turn. The developer's ask: send all changes, including test
changes, plus the operations to run afterwards, in one call. The reply should already contain
what the next fix needs, so Claude never has to ask for the code again before fixing.

**Built into `mt apply`.**
- `run: [{name, cmd, gate, timeout_s}]` executes after the edits are written. A failing gate
  restores every file and skips the remaining steps. Non-gate failures keep the edits.
  `parallel: true` runs consecutive non-gate steps at the same time. Pipes are drained on threads,
  steps have timeouts, and each full log is saved under `.mt/txn/<id>/`.
- `changed regions` in every reply: each changed area as it now reads, widened to its enclosing
  definition, with `+` on changed lines and a fresh span hash. After a gate rollback it is
  labelled `attempted change`.
- `failure context` on failure: the full body of each failing test, and the enclosing function at
  every file:line the output names, with `!` on that line. The code is read from the attempted
  text, even after a rollback, so compiler line numbers match.

**Demo** on a scratch copy of claude_coach. One batch made a code change in `decodeJSON`, added a
helper, and created a new test file. It then ran build as a gate, vet, and the unit tests in parallel.

| Case | Result | What came back |
|---|---|---|
| All good | 3 edits to 2 files; 5.8 s total, of which 5.76 s was build and tests | Both changed areas as final code |
| Wrong test expectation | Build and vet pass; the unit test fails; edits kept | The failing test's body, `!` on line 20, and a span hash for the fix |
| Compile error in the helper | The build gate fails at 1.3 s; both files rolled back; repo clean | The helper with `!` on line 337, taken from the compiler error, plus the attempted change |

mt's own overhead was 39 to 45 ms to validate, 0 to 1 ms to write and 9 to 10 ms to reindex.

---

## 2026-11-05 · 9. Real sessions on the one-shot design: change plus a new unit test

**Task T4.** T3 (request ID on audit rows), plus a new unit test in `internal/api` proving that
`auditWrite` records the request ID, with `go test ./internal/api/` passing.

Raw setup, one session each, Opus 5.5. A: no mt. D: mt stated in the base prompt.

| | A, no mt | D, mt stated | Change |
|---|---|---|---|
| Turns | 10 | 9 | -10% |
| Tool calls | 9 | 7, one of them the skill load | -22% |
| Wall time | 96.2 s | 84.9 s | -12% |
| Cost | $0.481 | $0.484 | same |
| Output tokens | 6,600 | 7,778 | +18% |
| Cache read | 302,873 | 214,904 | -29% |
| Checks passed | 8 of 8, tests pass | 8 of 8, tests pass | |
| AuditEntry sites setting RequestID | 7 | 5 | |

**What D did.** One `mt context AuditEntry --edit`, then three calls of `mt span` / `mt skeleton`
reads, then a single `mt apply` holding everything. That covered 2 new migrations, the struct
field, the SQL, the call sites and the new test file, with run steps for build (gate), vet, unit
tests and gofmt. All four passed: 17 edits to 7 files; validation 19 ms, writes 2 ms, reindex
61 ms, steps 8.3 s.

**Why D did not win this time.** Its first `mt apply` was refused. Claude used the hash
`mt context` gave for the whole `WriteAudit` function (L13-72) on an op scoped to a sub-range
(L49-71). mt checked the hash against exactly those lines. Claude then rewrote the batch with
exact-text ops and resent all of it: one extra turn, and about 4k more output tokens. That is
the whole of D's output-token overhead.

**Fix.** A hash now vouches for a span if it matches those exact lines, or an unchanged
definition that contains them. That matches how hashes are handed out. Replaying the refused op
now validates; a genuinely stale hash is still refused.

**Pattern across sections 7 and 9.** In both rounds the control set RequestID in more places:
the WebSocket subscribe and the login audit, both of which build audit rows inside a
request-derived `ctx`. mt listed those sites both times. The mt sessions read less around them,
and judged them out of scope. Fewer reads trade some depth for speed. Whether `--edit` mode
should include a little more around each site is an open question.

---

## 2026-11-05 · 10. Rerun with the hash fix: 3 repetitions, and a realistic context size

**Setup.** Task T4 (request ID on audit rows, plus a new unit test). Raw setup, Opus 5.5.
A = no mt, D = mt stated in the base prompt. Normal context: 3 runs each. Realistic context:
1 run each with about 150k tokens of the repo's own design docs appended to the system prompt
(600 KB of `Specs/*.md`, framed as documents read earlier in the session). Every run passed all
8 checks and its unit tests.

| Set | Run | Turns | Calls | Wall s | Cost $ | Output tok | Cache read | Context | mt applies | Refused | Sites |
|---|---|---|---|---|---|---|---|---|---|---|---|
| normal | A-r1 | 14 | 13 | 125.2 | 0.589 | 7,746 | 539,140 | 18k-52k | | | 7 |
| normal | A-r2 | 12 | 11 | 106.6 | 0.541 | 7,130 | 409,294 | 18k-51k | | | 7 |
| normal | A-r3 | 13 | 12 | 114.2 | 0.505 | 7,040 | 422,472 | 18k-46k | | | 7 |
| normal | D-r1 | 13 | 11 | 126.1 | 0.690 | 12,236 | 426,471 | 18k-56k | 2 | 1 | 6 |
| normal | D-r2 | 11 | 9 | 112.4 | 0.490 | 6,262 | 323,481 | 18k-49k | 3 | 0 | 5 |
| normal | D-r3 | 10 | 8 | 94.8 | 0.587 | 8,896 | 272,853 | 18k-55k | 2 | 1 | 6 |
| 255k | A-r1 | 12 | 11 | 104.2 | 2.877 | 6,609 | 2,979,026 | 255k-280k | | | 7 |
| 255k | D-r1 | 7 | 5 | 68.4 | 2.525 | 4,364 | 1,334,140 | 255k-282k | 1 | 0 | 5 |

Means at normal context:

| | Turns | Calls | Wall | Cost | Output | Cache read |
|---|---|---|---|---|---|---|
| A, no mt | 13.0 | 12.0 | 115.3 s | $0.545 | 7,305 | 456,969 |
| D, mt stated | 11.3 | 9.3 | 111.1 s | $0.589 | 9,131 | 340,935 |

**Reading the results.**
- At a realistic context size, mt wins on every measure. Turns fell 42%, calls 55%, wall time
  34%, output tokens 34%, cache reads 55%, and cost 12%: $0.35 per task. This is one run each.
- At normal context the turn savings are real: 13% fewer turns and 23% fewer calls. They are
  worth little when each turn re-reads only about 30k tokens. Retries and larger batch payloads
  then cost more than the saved turns. D cost 8% more.
- Coverage: mt runs set RequestID in 5 to 6 places, the control in 7, in all 8 runs. Same
  pattern as sections 7 and 9.

**What cost D its extra turns at normal context**, from each run's apply replies:
1. **D-r1.** Claude sent `"hash": ""`, and mt treated the empty string as a mismatching hash, so
   one batch was refused and resent. Its second batch also reported a failure: Claude added a
   repo-wide `gofmt -l internal` step, and 31 files in this repo were already unformatted before
   the change.
2. **D-r3.** The same pre-existing formatting problem sat inside the gate step, so a correct
   change was rolled back and resent.
3. **D-r2.** No refusals, but the work was split across 3 batches. The first ran only a trivial
   step, and formatting and one more edit came in later batches.

**Fixes.**
- An empty `hash` now counts as no hash.
- `{changed}` and `{changed_dirs}` placeholders let a step check only this batch's files.
- `"baseline": true` runs a step once before the edits. An identical failure is reported as
  `KNOWN`, not as yours. A silent failure can never match, because it proves nothing: the first
  attempt wrongly marked newly bad formatting as known, because `test -z "$(gofmt -l ...)"` prints
  nothing. Verified: a file that was already dirty gives KNOWN; newly bad formatting gives FAIL,
  naming the file.
- The skill now says to put every step in the first batch.


## 2026-11-05 · 11. Replaying real past sessions: original read chains vs one mt call

**Question.** Take this repo's real past sessions. Rerun their read-only investigation chains
(the greps, cats, sed ranges and Read calls Claude made while thinking between them) through mt,
batched into one call. Does mt return the same content? How do its speed and call count compare?

**Data.** 11 main-thread sessions on this repo:
- 3 in the boston worktree, 2026-10-18 to 2026-10-22
- 4 in the catfish worktree, 2026-10-22 to 2026-11-04
- 4 today

A chain is a run of 2 or more consecutive read-only calls. There were 49 chains; 13 read only
repo code, and those 13 hold 42 calls. The rest read the scratchpad, plugin docs or the Orca profile.
Scripts: `scratchpad/replay/chains.py` and `replay.py`.

**Method.**
- Every original command is re-run in its original worktree. The boston worktree has since been
  emptied, so its 5 chains ran against the main repo, whose code is 2 to 3 weeks newer.
- A **hindsight batch** puts every question of the chain into one tool call:
  - `sed -n A,Bp` on code became `mt span` on the outermost definitions covering A..B. If
    definitions covered less than 80% of the range, the raw read was kept.
  - `grep -n "func X" -A..` became `mt span X`.
  - Repo-wide greps became one `mt query` with a grep op each, keeping case-insensitivity.
  - `ls`, doc reads and greps inside named files stayed as they were, inside the same call.
- A **first-question** call asks only the chain's first question, as `mt context <symbol>` or
  `mt query grep`, with no hindsight.
- Coverage is line-level: the share of (file, line) pairs the original commands showed that the
  mt output also contains.
- mt here is the build developed in parallel by another session: `mt query` batching and a
  per-repo daemon. Its source was not modified during this test.

**Results.**

| Chain | Where | Calls | Session s | Replayed now s | One batch s | Batch coverage | First-question coverage |
|---|---|---|---|---|---|---|---|
| 0 | boston, orientation | 9 | 24.4 | 0.51 | 0.34 | 100.0% | - |
| 1 | boston, app wiring | 4 | 24.3 | 0.31 | 0.34 | 95.6% | 7% |
| 2 | catfish, invalid_envelope | 4 | 15.5 | 0.33 | 0.11 | 99.6% | 30% |
| 3 | boston, runtimes | 3 | 11.5 | 0.23 | 0.14 | 87.1% | 5% |
| 4 | catfish, installer | 3 | 4.4 | 0.13 | 0.10 | 100.0% | - |
| 5 | catfish, "degraded" | 3 | 11.3 | 1.54 | 0.10 | 35.0% | 15% |
| 6 | catfish, tail tests | 3 | 4.4 | 0.08 | 0.07 | 100.0% | 38% |
| 7 | coach, eval hooks | 3 | 7.9 | 0.12 | 0.09 | 100.0% | 2% |
| 8 | boston, adapters | 2 | 3.5 | 0.07 | 0.10 | 98.5% | 48% |
| 9 | boston, schema | 2 | 6.4 | 0.09 | 0.13 | 95.2% | 0% |
| 10 | catfish, orca config | 2 | 4.1 | 1.86 | 1.81 | 100.0% | - |
| 11 | catfish, ports | 2 | 6.8 | 0.11 | 0.09 | 100.0% | - |
| 13 | coach, relay-status | 2 | 4.0 | 0.49 | 0.04 | 100.0% | 100% |
| **All** | | **42** | **128.5** | **5.89** | **3.46** | median 100%, mean 93.2% | median 15% |

**Reading the results.**
- **The tools were never the slow part.** The 42 commands take 5.9 s to run now, about 5% of the
  128.5 s the chains took in the sessions. The other 95%, about 2.9 s per call, is model generation
  and thinking between calls, plus hook and harness overhead. In the original sessions, a call's
  measured time with hooks was 0.76 s median and 2.18 s mean; replayed now without hooks it is
  0.14 s.
- **mt returned the same content.** Median coverage was 100%, mean 93%.
  - Chain 5 is the outlier. The original grep was cut by `head -30`; mt found all 259 matching
    lines in 150 places and showed a ranked 45 within its output limit, with the count. A third of
    that chain's greps also used grep's BRE `\{0,20\}`, which mt's regex engine reads literally.
  - The other gaps are import and comment lines just outside the definitions mt returned.
- **Speed per call.** A warm `mt query` or `mt span` took 25 to 55 ms. One batch for a whole chain
  took 40 to 340 ms. Chain 10 is the exception, at 1.8 s, because it keeps a recursive
  `grep -ril orca` that walks the whole tree.
- **Hindsight is doing the work.** One batch per chain means 13 calls instead of 42, so 29 fewer
  turns. At about 2.9 s per turn that is roughly 85 s of the 128.5 s, but only if Claude knew every
  question up front. Without hindsight, `mt context` on the first question already held a median
  15% of what the chain went on to read. It held 100% only for the pure "where is X used" chain,
  `relay-status`. Usually the follow-up questions came from reading, not from the code's structure,
  which matches the transcript analysis in section 1.

**What this says about where the time goes.** Making reads faster gains almost nothing: they are
about 5% of the time. The gain comes from removing turns, and batching removes turns only when
the questions are known together. mt already makes the batch cheap: one call, a few hundred ms,
and the same content. The open lever is getting Claude to ask the next questions in the same call:
- `mt query`'s `$q1` hops, which follow a previous answer within one call
- `--edit`-style context that pre-answers the most common next question, the code around each call site

**Caveats.**
- The boston chains replayed against code 2 to 3 weeks newer than the sessions saw.
- The model and harness share of time comes from transcript timestamps, which mark when a
  message starts. Only total chain time and the replayed tool time are used as measurements.
- These are 13 chains from one repo.

---

## 2026-11-05 · 12. In-memory graph, `mt query` with claims, a live daemon

**What changed.**
- `Store` trait over the lookups (`index.rs`), implemented by the SQLite index and by an
  in-memory `Graph` (`graph.rs`). `mt context` runs on either one.
- `mt query` (`query.rs`): a JSON batch of ops (context, defined_in, exists, references, callers
  with depth, tested_by, constructs, has_field, calls, uses_table, grep). `expect` makes an op a
  claim (✓ / ✗ with the difference / ? can't check), and `exhaustive` returns every site that is
  not on the list. `$id` subjects chain queries in one call. One shared output budget; a span
  answering several queries appears once and ranks higher.
- `mt serve` (`serve.rs`): per-repo daemon on a Unix socket at `.mt/mt.sock` (0600). It starts
  automatically on the first `query`/`context` (that call answers in-process). A notify watcher
  applies changes in real time (30 ms debounce). Each request writes a cookie file and waits for
  the watcher to report it, so answers never lag the disk; if the watcher doesn't confirm within
  1 s, the request walks the tree. The daemon exits after 30 idle minutes, on `--stop`, or when
  the binary changes. `MT_NO_DAEMON=1` turns it off. Unix only; Windows runs in-process.

**Measured.**

| | claude_coach (485 files) | mixar-app (4,135 files, 1M ref rows) |
|---|---|---|
| `mt context`, in-process | 30 ms | 100-110 ms |
| `mt context`, daemon | 10 ms | 50 ms |
| 8-query batch, in-process | 90 ms | |
| 8-query batch, daemon | 16-21 ms | |
| 5-query batch incl. grep, daemon | | 127-130 ms (grep 50 ms of it) |
| daemon load / memory | 122 ms | 2.5 s / 285 MB |

- Daemon and in-process outputs are byte-identical (context for 3 names, the 8-query batch)
  once ties in ranking were broken by path and line.
- Real-time freshness test (scratch repo, 5 rounds of edit, new file, delete, rename, rename back,
  revert, query right after each): 30 of 30 correct, no walk fallbacks. Cookie sync costs 2-10 ms.
  The first version, which only waited for events already received, failed the delete case.
- grep counts match ripgrep (`TODO` 223, `^def test_` 6,801).
- On claude_coach the exhaustive claim `constructs AuditEntry, expect [auditWrite]` lists all 4
  other construction sites, including `Client.subscribe` (WebSocket) and `Store.writeAuthAudit`
  (login): the two sites the mt runs left out in sections 7 and 9.

**Open.**
- Not yet run through the headless A/B harness: the claim is that `exhaustive` claims close the
  5-vs-7 RequestID gap at lower output. That is the next measurement.
- grep with thousands of hits spends most of its time grouping by enclosing definition (364 ms
  for 6,801 hits). FTS5 trigram or file contents in memory if it matters.
- Daemon memory: identifier strings are not interned (285 MB at 1M rows).
- Requests are served one at a time.

---

---

## 2026-11-05 · 13. Session startup: a plugin that makes Claude use mt, and one mt server per session

**Goal.** Claude should use mt every time, without being told in the prompt. Each session should
start its own mt server.

**Built.** `~/orca/mt/plugin/`, a Claude Code plugin:
- `bin/mt` is a launcher. A plugin's `bin/` folder is on the Bash tool's PATH.
- `skills/mt` links to the skill.
- `hooks/hooks.json` wires up `hooks/mt_hooks.py`, which handles four events:
  - **SessionStart** starts `mt serve` for this session on its own socket, `/tmp/mt-s-<hash>.sock`.
    It writes `MT_SOCKET`, `MT_SOCKET_ROOT` and `MT_SESSION` to `CLAUDE_ENV_FILE`, so every Bash
    call in the session talks to this session's server. It injects the full skill as context,
    about 2,280 tokens, so Claude needs no skill-load call. The server was up 112 ms after
    session start.
  - **UserPromptSubmit** adds a one-line reminder.
  - **PreToolUse** on Bash, Read and Grep catches code reads done the old way: `sed -n A,Bp` on a
    code file, `cat`/`head`/`tail` of code files, repo-wide greps for identifiers including `a\|b`
    alternations, and `grep -A` on a func. In strict mode, the default, the Bash call is denied,
    and the reason holds the exact mt command that returns the same lines, so Claude switches
    straight away. Greps for literal text, logs, docs and config pass through. Read and Grep get
    a hint only, because Edit needs a prior Read. Every decision is logged to `/tmp/mt-hooks.log`
    with the rule that fired.
  - **SessionEnd** stops the session's server and removes its socket.
- **Daemon change**, agreed with the session developing the daemon (claude-coach-20):
  `serve::socket_path()` honours `MT_SOCKET`, but only inside `MT_SOCKET_ROOT`. An mt call in any
  other repo falls back to that repo's own per-repo socket.
- **Knobs:** `MT_SERVER_SCOPE=session|repo|off` and `MT_ENFORCE=strict|nudge|off`.

**Bugs found while testing the hooks offline**, with real commands from past sessions:
- The hook's `mt skeleton` helper went through the shared per-repo daemon. A differently built
  binary hits that daemon's stale-build check, which times the helper out or restarts another
  session's daemon. Hook helpers now run in-process (`MT_NO_DAEMON=1`).
- Splitting off pipes cut quoted patterns like `'decodeJSON\|WriteAudit'` in half. The split is
  now quote-aware.

**Costs to know about**, from claude-coach-20: each daemon loads the whole graph (2.5 s and 285 MB
on mixar-app, about 4k files), and several daemons write the same `.mt/index.sqlite`.
`MT_SERVER_SCOPE=repo` shares one daemon per repo instead.

**Real headless sessions.** Plugin loaded with `--plugin-dir`; nothing about mt in the prompt or
the repo. Same raw setup, tasks and model as sections 6 and 10. E = plugin only, two runs per task.
A = earlier no-mt runs of the same tasks.

| Run | Turns | Calls | mt calls | Redirected | Wall s | Cost $ | Output tok | Correct |
|---|---|---|---|---|---|---|---|---|
| A-T1 r1 / r2 | 10 / 12 | 9 / 11 | 0 / 0 | | 76.4 / 82.7 | 0.407 / 0.415 | 5,941 / 5,820 | 19/19 sites, both |
| E-T1 r1 / r2 | 7 / 9 | 6 / 8 | 4 / 6 | 1 / 1 | 56.6 / 65.0 | 0.399 / 0.548 | 4,892 / 4,966 | 19/19 sites, both |
| A-T4 r1 / r2 / r3 | 14 / 12 / 13 | 13 / 11 / 12 | 0 | | 125.2 / 106.6 / 114.2 | 0.589 / 0.541 / 0.505 | 7,746 / 7,130 / 7,040 | 8/8 and tests, all |
| E-T4 r1 / r2 | 7 / 9 | 6 / 8 | 6 / 7 | 1 / 1 | 78.0 / 121.7 | 0.499 / 0.641 | 6,078 / 8,149 | 8/8 and tests, both |

| Mean | Turns | Calls | Wall | Cost | Output |
|---|---|---|---|---|---|
| A-T1, no mt | 11.0 | 10.0 | 79.6 s | $0.411 | 5,880 |
| E-T1, plugin | 8.0 (-27%) | 7.0 | 60.8 s (-24%) | $0.474 (+15%) | 4,929 (-16%) |
| A-T4, no mt | 13.0 | 12.0 | 115.3 s | $0.545 | 7,305 |
| E-T4, plugin | 8.0 (-38%) | 7.0 | 99.9 s (-13%) | $0.570 (+5%) | 7,114 (-3%) |

**Reading the results.**
- **Adoption** went from 1 in 4 sessions with the skill alone (section 6) to 4 in 4 with the plugin.
  Every session was redirected exactly once, on its first old-style code read, and then stayed on mt.
- **Turns and wall time fell** on both tasks.
- **Cost rose slightly** at this context size. These sessions start at 18k tokens, so a saved turn
  is cheap. The briefing, about 2.3k tokens, and mt's larger structured results are cache writes
  that every later turn re-reads, and the one redirect wastes a call. Section 10 showed the balance
  tips at a realistic 255k context, where each saved turn avoids re-reading about 240k tokens.

**Next.** The one redirect per session is a wasted round trip. PreToolUse can rewrite the command
(`updatedInput`) instead of denying it, so the `sed` Claude asked for runs as the equivalent mt
command in the same turn. Then repeat the plugin runs at realistic context, 3 per task.

---

## 2026-11-05 · 14. T4 A/B with claims first: discovery is solved, the gap is interpretation

**Setup.** Task T4, raw setup (no graft), Opus 5.5, 3 runs per condition, normal context.
A = no mt. D = mt required, "start with mt context" (section 10's instruction, new binary and
skill). Q = mt required, "start with one mt query batch of claims, exhaustive for every set of
places you will change". Harness and every transcript: session scratchpad `ab/` (run.sh, t4.py,
sites.sh, r12/, r12-report.txt). Sites = how many of the 5 `AuditEntry` construction sites set
RequestID (checked within 12 lines of each literal).

| Run | Turns | Calls | Wall s | Cost $ | Output | Cache read | mt query calls | Checks | Tests | Sites |
|---|---|---|---|---|---|---|---|---|---|---|
| A-r1 | 13 | 12 | 116.5 | 0.585 | 7,424 | 498,693 | | 8/8 | ok | 5/5 |
| A-r2 | 12 | 11 | 112.4 | 0.597 | 8,055 | 445,191 | | 8/8 | ok | 5/5 |
| A-r3 | 10 | 9 | 89.2 | 0.450 | 6,575 | 307,299 | | 8/8 | ok | 5/5 |
| D-r1 | 8 | 6 | 75.6 | 0.559 | 5,906 | 249,269 | 3 | 8/8 | ok | 3/5 |
| D-r2 | 15 | 13 | 130.5 | 0.706 | 9,977 | 588,875 | 0 | 8/8 | ok | 4/5 |
| D-r3 | 8 | 6 | 80.7 | 0.526 | 6,565 | 232,155 | 1 | 8/8 | ok | 3/5 |
| Q-r1 | 9 | 7 | 90.4 | 0.509 | 5,880 | 274,803 | 2 | 8/8 | ok | 3/5 |
| Q-r2 | 15 | 13 | 159.9 | 0.736 | 10,711 | 635,657 | 2 | 8/8 | ok | 5/5 |
| Q-r3 | 10 | 8 | 113.3 | 0.593 | 7,158 | 348,499 | 2 | 8/8 | ok | 3/5 |

Means: A 11.7 turns, $0.544, 5.0/5 sites. D 10.3 turns, $0.597, 3.3/5. Q 11.3 turns, $0.613, 3.7/5.
(The analyzer's first pass counted 0 queries for Q-r1/r3 because the command began with
`ls ... &&`; counts above are from the transcripts.)

**What happened.**
- Every Q run sent the exhaustive claim `constructs AuditEntry, expect [auditWrite]` in its first
  batch and got ✗ with all 5 sites, including WebSocket `Client.subscribe` and login
  `Store.writeAuthAudit`. Discovery was 5/5 in every Q run.
- Q-r1 and Q-r3 then changed 3 of them. The prompt says "every call site that builds an AuditEntry
  from an HTTP request", and both judged the WebSocket subscribe and the store-layer login audit
  out of that scope. Q-r2 grepped the WebSocket `ctx` and included both. The control reads more
  code around each site and includes both every time.
- So the 5-vs-3 gap is now a reading of an ambiguous spec, not a missing lookup. Per the design
  rule (mt finds, the model judges), mt should not decide it. The measurable mt question is
  answered: an exhaustive claim surfaces every site.
- Cost and turns: no win at normal context in this round; Q-r2 and D-r2 each spent extra turns
  on retries (3 refused applies in D-r2).

**mt bugs found in the transcripts, fixed.**
- `grep "RequestID|request_id|X-Request"` and `grep "func .*Request"` were sent without
  `"regex": true` and found nothing. A literal that matches nothing and contains regex
  metacharacters is now retried as a regex, with a note saying so.
- `"in": "."` scanned 0 files (stored paths have no `./`). `.` and `./x` are normalized.

**Next.** Make T4's prompt unambiguous ("including WebSocket and login audits") to separate
interpretation from discovery, and rerun at the realistic 255k context, where section 10 showed
mt's turn savings pay.

---

## 2026-11-05 · 15. Parallel everywhere, and `property`: one field across every layer

**Parallel graph load.** SQLite reads split by rowid range, one read-only connection per thread
(WAL allows concurrent readers); name maps built per file slice on every core and merged.
mixar-app (4,135 files, 1,041,856 reference rows): **2,535 ms → 222-265 ms** (read 40 ms, assign
65 ms, link 73 ms, 18 threads). Daemon and in-process answers identical on 4 context queries and
two batches.

**Store is `Sync`.** The SQLite connection sits behind a mutex, so the in-process path shares
the same parallel code (lookups serialize on the lock; file reads, regexes and grouping do not).

**Parallel queries.** A batch runs in waves: every query whose `$id` subjects are answered runs
at once. 8 queries on mixar-app: 720 ms as separate calls → 360 ms as one batch → 261 ms after
the two fixes below.

**grep** groups hits by enclosing definition inside the scan threads: `^def test_` (6,801 hits)
364 ms → 95 ms. Counts still match ripgrep.

**`property`** (graph and SQLite together):
1. the normalized-name index (graph: in-memory map; SQLite: an expression index on
   `lower(replace(name,'_',''))`, schema 5) gives every spelling of the name;
2. the reference index gives the exact files and lines for each spelling;
3. those lines are read and classified in parallel by shape: access (with its receiver),
   declaration, key/tag (`"user_id"`, `json:"user_id"`), SQL column, mention.
`"user.userID"` keeps only accesses through `user`, plus every declaration, key and column.
claude_coach `property userID`: 3 spellings (user_id 186, UserID 136, userID 136), 458 lines in 68
files: 158 accesses (receivers s 37, sc 32, sd 14, ...), 47 columns, 40 declarations, 23
keys/tags, 190 mentions; 40 ms. Shape regexes compiled once per spelling (182 ms → 29-65 ms on
mixar-app). `"variants": true` gives references/callers/tested_by the same spelling expansion.
Doc hits rank below code.

Freshness loop rerun on the new binary (edit, new file, delete, rename, rename back, a new
`userID` site found by an exhaustive `property` claim): 30 of 30.

---

## 2026-11-05 · 16. Interactive A/B, test G1: "add Google authentication, with tests"

**Setup.** Two Orca worktrees from `main` at `bd5fa80`: `mt-eval-on` and `mt-eval-off`. Each runs
its own interactive Claude Code session, launched through the Orca CLI.
- **Both arms:** Opus 5.5 at medium effort; `--setting-sources project --strict-mcp-config
  --permission-mode bypassPermissions`; `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`.
  - `graft` is stubbed first on PATH. Removed from each worktree: `.claude`, `.agents`, `.codex`,
    `.cursor`, `.gemini`, `.kiro`, `.mcp.json`, `AGENTS.md`, `GEMINI.md`, `graft/`,
    `opencode.json`, `.mt`.
  - Orca adds its own `--plugin-dir orca-agent-view` to both.
- **mt ON adds:**
  - `--plugin-dir ~/orca/mt/plugin`: a session `mt serve` (up in 190 ms) and strict PreToolUse
    redirects.
  - `--append-system-prompt-file` holding an absolute rule plus the full SKILL.md (10,470 bytes).
    The rule: use mt for every code read, search and edit; pack every foreseeable question into
    as few `mt query` batches as possible, chained with `$id`; make `exhaustive` claims before
    changing every place that does something; use `mt apply` for any change that spans more than
    one spot; plain shell only for build, test and git.
- **mt OFF:** no mt anywhere.

**Leaks found and fixed before the run.**
- **Auto-memory.** Claude Code shares one memory folder across all worktrees of a repo, so the
  first launch loaded the user's mt and benchmark notes. Disabled with
  `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`; the transcripts confirm it's gone.

**Leaks still present, the same in both arms.** The worktrees sit under `$HOME`, so
`~/.claude/CLAUDE.md` (the graphify notes) and `~/.claude/skills/graft` load as ancestor *project*
files. Neither arm made a graft or graphify call.

**Prompt (identical, sent to both arms at the same time):**
> I want to be able to add google authentication, please evaluate and add it along with tests

**Interruption.** Both sessions hit "API Error: Connection lost mid-response" at the same moment
(21:22:09Z, after 732 s and 730 s), so the cause was the network, not either arm. Both got the same
`continue` 30 s later. Neither had finished:
- mt ON had written its new files and was assembling its single `mt apply` batch;
- mt OFF was mid-edit.

Times below are active time: the sum of both prompts' `turn_duration`.

**Measurement.** `live_analyze.py` reads each session's transcript
(`~/.claude/projects/<worktree>/<session>.jsonl`). It records every tool call, its time and result
bytes, the context per turn, and usage. Cost comes from per-token rates fitted exactly to 50
earlier headless Opus 5.5 runs (max relative error 1e-15): input $4/M, cache write $8/M, cache read
$0.20/M, output $20/M.

### Results

| | mt ON | mt OFF | ON vs OFF |
|---|---|---|---|
| Turns | 40 | 58 | **-31%** |
| Tool calls | 43 (34 Bash, 9 Write) | 59 (55 Bash, 4 Write) | **-27%** |
| Active time | 837.6 s (732.4 + 105.2) | 757.4 s (729.8 + 27.5) | +11% |
| Tool execution time | 59.5 s | 80.2 s | -26% |
| Cost | **$4.006** | **$3.684** | +8.7% |
| … cache read | 5,653,165 ($1.131) | 7,159,145 ($1.432) | -21% |
| … cache write | 186,216 ($1.490) | 143,869 ($1.151) | +29% |
| … output | 69,265 ($1.385) | 55,015 ($1.100) | +26% |
| Context at turn 1 | 48.2k | 40.3k | +7.9k (system prompt, plus the plugin's copy of the skill) |
| Peak context | 215.8k | 173.5k | +24% |
| Context growth per turn | ~4.2k | ~2.3k | |
| Tool result bytes | 207,634 | 169,386 | +23% |
| mt calls | 9 `mt query`, 2 `mt apply`, 54 `mt span`/`skeleton`/`context` | 0 | |
| Shell read or grep commands | 57 | 154 | -63% |
| Edits to existing files | 1 `mt apply`: 27 edits in 13 files | 9 `python3 … .replace()` scripts plus `sed -i` and `cat >>` | |
| Hook redirects | 4 denials, all followed by the suggested mt command (calls 6, 9, 11, 14) | | |
| graft or graphify calls | 0 | 0 | |

### Outcome (checked by me after both finished)

| Check | mt ON | mt OFF |
|---|---|---|
| `go build ./...` | pass | pass |
| `go vet ./...` | pass | pass |
| `go test ./...` with no database | pass (45 pass and 5 skip across api, oidc, store, config) | pass (54 pass and 8 skip) |
| DB-backed SSO tests | **not run** (5) | **not run** (8) |
| `web`: `tsc --noEmit` | pass | pass |
| Diff | 21 files, +1560 -52 | 22 files, +1532 -49 |
| New test functions | 22 (oidc 8, api 7, store 6, config 1) | 21 (oidc 8, api 5, store 5, config +3) |

The DB-backed tests are skipped without `RELAY_TEST_DB`:
- Docker isn't running.
- A scratch `initdb` fails with `shmget: Cannot allocate memory`, because `kern.sysv.shmall` is
  1024 pages and the segments are in use.
- The listener on `:5432` belongs to another VM (`gvproxy`).

Both arms hit the same wall and lost 5-6 calls trying to start Postgres: ON calls 26-31, OFF calls
47-52.

**Both designs are nearly the same.** Both found that SSO routes and a "Continue with SSO" button
already existed over a store stub, and both followed spec §10.4.5:
- `coreos/go-oidc` plus `x/oauth2`;
- state, nonce and PKCE, and a verified email;
- the Workspace `hd` claim checked for allowed domains;
- no auto-provisioning: sign-in matches an existing active user by email, and the first sign-in
  pins `sso_subject`;
- the IdP-satisfies-MFA setting;
- both fixed the same existing open redirect (`//evil.com`).

**Differences.**
- **mt OFF** also:
  - regenerated `web/src/api/types.gen.ts`;
  - passed the new env vars through `docker-compose.yml`;
  - fixed the callback's broken POST form parsing;
  - raised the MFA default as an explicit decision for the user.
- **mt ON** also:
  - built a reusable fake IdP package (`internal/auth/oidc/oidctest`);
  - wrote 2 more API tests;
  - ran `go mod tidy`, which churned 31 lines of `go.mod`.
- **Endpoint names:** ON added `/v1/auth/sso/providers`, OFF added `/v1/auth/providers`.

### What mt did, read from the transcript

- **Discovery.** Turn 4 was one `mt query` with 6 sub-queries: `context` on `Server.startSso`,
  `Server.completeSso`, `Store.SSOStart`, `Store.SSOCallback` and `Store.issueSession`, plus a regex
  grep. It returned 65 items (about 4k tokens) in 50 ms.
  - mt OFF got the same picture by `cat`-ing whole files (`auth.go` 17 KB, `store/auth.go` 21 KB,
    `router.go`, `Login.tsx` and more) over turns 2-16.
- **The rule was only partly followed.** Even under the absolute rule, ON ran 57 plain shell reads:
  - `git ls-files`, greps of the Specs markdown, and `ls` of the module cache (all allowed);
  - plus some code greps the hook caught.
  
  It never made an `exhaustive` claim. The task adds new code rather than changing every place
  that does X, so no claim really fit.
- **One apply.** ON wrote the large replacement bodies to scratch files (calls 32-34), had Python
  assemble a 27-op batch JSON, and sent one `mt apply` with 4 check steps.
  - The first attempt was refused before anything was written. Op 23's `find` text appeared 3
    times in `Login.tsx`.
  - The second attempt applied 27 edits to 13 files in 12.2 s. gofmt, build plus vet, and the unit
    tests passed. The `web` step failed only because `node_modules` was missing: TS2688, fixed by
    `npm ci` in call 42.
  - mt OFF made the same kind of edits as 9 separate Python string-replace scripts, with no
    validation and no rollback.

### Impact

- **Fewer steps.** mt cut turns by 31% and tool calls by 27%, with 21% less cache read.
  Batched discovery replaced about 15 whole-file reads, and one validated apply replaced 9
  unchecked edit scripts.
- **More cost, more time.** The run still cost 8.7% more and took 11% more active time.
  - **Where the money went:** cache writes +$0.34 and output +$0.29 outweighed cache reads -$0.30.
  - **Output:** ON wrote more output tokens because it typed every edit into JSON and scratch files
    instead of short `replace` calls. The 105 s second prompt is mostly the time spent writing that
    one big batch.
  - **Context:** each mt answer is denser and larger (+23% result bytes). Context grew about 4.2k
    per turn against 2.3k for OFF, and the run started 7.9k higher because of the rule and the
    skill in the system prompt.
- **Quality was the same.** Both arms pass every check that can run here. Each did a few things the
  other didn't, and neither result is clearly better.
- **Matches earlier findings.** As in sections 6 and 10, mt saves turns, but at this size of
  context the saving doesn't turn into lower cost. The cost of a change sits in writing the edit,
  and the mt arm's edit payload was heavier.
- **Options from this run, none implemented yet:**
  - let `mt apply` take a replacement body from a file path, so the model doesn't need the
    scratch file plus Python assembly step;
  - drop the second copy of the skill when the system prompt already holds it;
  - make `context` return pointers only by default during edit sessions.

**Caveats.**
- This is one run per arm, interrupted once in both, and the DB-backed tests are unrun in both.
- Interactive sessions are not headless: the hook output and Orca's agent-view plugin add context
  that a `claude -p` run doesn't have.

**Artifacts** (session scratchpad `d8c6cc34…/scratchpad/`):
- `live_analyze.py`
- `t-google.{on,off}.report.txt`: the full per-call reports
- `t-google.{on,off}.json` and `t-google.{on,off}.cut.json`: the metrics, and a snapshot at the
  interruption
- `mt-on-system.md`

Transcripts: `~/.claude/projects/-Users-oshricohen-orca-workspaces-claude-coach-mt-eval-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d.jsonl`
and `…-mt-eval-off/efc302f0-cf6d-4ee6-b200-58c0b1628e4e.jsonl`.

<details><summary>mt ON: all 43 tool calls</summary>

| # | Turn | Tool | Secs | Result bytes | Command or target (first 110 characters) |
|---|---|---|---|---|---|
| 1 | 1 | Bash | 0.3 | 9,713 | `git ls-files \| grep -v '^\.' \| head -200 && cat go.mod 2>/dev/null \| head -30` |
| 2 | 2 | Bash | 0.7 | 3,225 | `git ls-files \| grep -v '^\.' \| tail -n +201 \| grep -v '^web/.*\.\(png\\|svg\)$' \| head -200; grep -n 'oaut` |
| 3 | 3 | Bash | 0.4 | 22,222 | `grep -n -i -B2 -A12 'google\\|oidc\\|sso' Specs/relay-backend-spec/10-auth-rbac.md \| head -150; mt skeleton i` |
| 4 | 4 | Bash | 0.7 | 25,232 | `mt query - <<'EOF' {"queries":[ {"id":"a","context":"Server.startSso"}, {"id":"b","context":"Server.completeSs` |
| 5 | 5 | Bash | 0.7 | 26,615 | `ls $(go env GOMODCACHE)/github.com/coreos 2>/dev/null; ls $(go env GOMODCACHE)/golang.org/x \| grep oauth; ls ` |
| 6 | 6 | Bash | 0.2 | 372 | **ERR** `M=$(go env GOMODCACHE); ls $M/github.com/coreos/go-oidc $M/github.com/go-jose; ls $M/cache/download/github.com` |
| 7 | 7 | Bash | 0.4 | 29,369 | `M=$(go env GOMODCACHE); ls $M/github.com/coreos/go-oidc $M/github.com/go-jose; ls $M/cache/download/github.com` |
| 8 | 8 | Bash | 0.7 | 16,482 | `ls $(go env GOMODCACHE)/github.com/go-jose/go-jose; mt span internal/storage/migrations/0004_authn_email_looku` |
| 9 | 9 | Bash | 0.2 | 294 | **ERR** `echo "DB=$RELAY_TEST_DB"; which psql pg_ctl docker 2>/dev/null; mt skeleton internal/api/routes_test.go; mt sp` |
| 10 | 10 | Bash | 0.7 | 6,428 | `echo "DB=$RELAY_TEST_DB"; which psql pg_ctl docker 2>/dev/null; mt skeleton internal/api/routes_test.go; mt sk` |
| 11 | 11 | Bash | 0.2 | 232 | **ERR** `grep -n 'sso' api/openapi.yaml \| head; grep -rn 'RELAY_TEST_DB' --include=*.sh --include=Makefile --include=*` |
| 12 | 12 | Bash | 1.0 | 10,702 | `mt span api/openapi.yaml '/sso/' ; mt query - <<'EOF' [{"id":"db","grep":"RELAY_TEST_DB"}] EOF docker ps --for` |
| 13 | 13 | Bash | 1.2 | 162 | `GOPROXY=off go get github.com/coreos/go-oidc/v3@v3.20.0 golang.org/x/oauth2@v0.36.0 github.com/go-jose/go-jose` |
| 14 | 14 | Bash | 0.8 | 251 | **ERR** `mt span internal/storage/migrations/0001_init.up.sql app_user; mt span internal/assembler/idempotency_test.go ` |
| 15 | 15 | Bash | 0.5 | 7,673 | `mt span internal/storage/migrations/0001_init.up.sql app_user; mt span internal/assembler/idempotency_test.go ` |
| 16 | 16 | Write | 0.0 | 180 | `<wt>-on/internal/auth/oidc/oidc.go` |
| 17 | 16 | Write | 0.0 | 193 | `<wt>-on/internal/auth/oidc/oidctest/oidctest.go` |
| 18 | 16 | Write | 0.0 | 185 | `<wt>-on/internal/auth/oidc/oidc_test.go` |
| 19 | 17 | Bash | 5.9 | 221 | `go mod tidy >/dev/null 2>&1; GOPROXY=off go vet ./internal/auth/oidc/... && GOPROXY=off go test ./internal/aut` |
| 20 | 18 | Bash | 0.2 | 3,368 | `git diff go.mod` |
| 21 | 19 | Bash | 0.4 | 2,937 | `mt query - <<'EOF' [{"id":"a","grep":"auth.login.success"},{"id":"b","grep":"auth_lookup_user_by_email\|ALTER ` |
| 22 | 20 | Bash | 0.3 | 4,724 | `mt span internal/api/auth.go Server.login; mt span internal/api/auth.go Server.logout; mt span internal/api/ro` |
| 23 | 21 | Bash | 0.3 | 10,039 | **ERR** `mt span internal/api/errors.go APIError; mt span internal/api/server.go '/sessionCookie *=/'; mt span api/open` |
| 24 | 22 | Bash | 0.5 | 4,233 | `mt span internal/api/routes_test.go TestPartialSessionBlocked; mt span internal/api/fake_test.go newFakeStore ` |
| 25 | 23 | Bash | 1.0 | 2,354 | `mt span web/src/api/client.ts get; mt span web/src/components/ErrorNotice.tsx ErrorNotice \| head -30; mt quer` |
| 26 | 24 | Bash | 10.9 | 584 | `S=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/pg; mkdir -p $S && initdb -D $S/data -U postgre` |
| 27 | 25 | Bash | 0.3 | 168 | `S=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/pg; ls $S; tail -20 $S/log 2>/dev/null; ls $S/d` |
| 28 | 26 | Bash | 1.9 | 632 | `S=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/pg; initdb -D $S/data -U postgres -A trust 2>&1` |
| 29 | 27 | Bash | 1.8 | 332 | **ERR** `S=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/pg; initdb -D $S/data -U postgres -A trust >/de` |
| 30 | 28 | Bash | 1.8 | 773 | `S=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/pg; ls $S $S/data 2>&1 \| head; tail -5 $S/log ` |
| 31 | 29 | Bash | 0.5 | 2,770 | `pgrep -fl postgres \| head; for p in 5432 5433 55114 54322; do pg_isready -h localhost -p $p; done; ipcs -m \|` |
| 32 | 30 | Write | 0.1 | 244 | `<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/store_ssostart.txt` |
| 33 | 30 | Write | 0.1 | 241 | `<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/store_ssocb.txt` |
| 34 | 30 | Write | 0.0 | 245 | `<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad/api_completesso.txt` |
| 35 | 30 | Write | 0.0 | 173 | `<wt>-on/internal/app/sso.go` |
| 36 | 31 | Write | 0.0 | 178 | `<wt>-on/internal/api/sso_test.go` |
| 37 | 31 | Write | 0.0 | 180 | `<wt>-on/internal/store/sso_test.go` |
| 38 | 32 | Bash | 0.6 | 1,474 | `gofmt -l internal/ cmd/; grep -n 'AUTH\\|PUBLIC_URL' deploy/compose/.env.example; grep -n 'RELAY_AUTH_' README` |
| 39 | 35 | Bash | 0.5 | 207 | **ERR** `SP=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad && python3 - "$SP" > $SP/batch.json <<'PY' imp` |
| 40 | 36 | Bash | 13.1 | 10,040 | **ERR** `SP=<scratch>-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad && python3 - "$SP" <<'PY' import json,sys p=sy` |
| 41 | 37 | Bash | 0.6 | 474 | `cd <wt>-on/web && ls node_modules 2>/dev/null \| wc -l; git stash list >/dev/null; git -C .. status --short \|` |
| 42 | 38 | Bash | 5.5 | 11 | `npm ci --silent >/dev/null 2>&1; echo "ci=$?"; npx tsc --noEmit -p . && echo TSC_OK; npm test --silent 2>&1 \|` |
| 43 | 39 | Bash | 4.3 | 1,502 | `cd <wt>-on && go test ./internal/store/ ./internal/api/ ./internal/auth/oidc/ -count=1 -v -run 'SSO\|Sso\|Happ` |

Context per turn (k tokens): 48 53 54 64 77 90 93 108 116 120 124 125 129 130 133 137 150 151 153 157 160 166 174 177 178 178 179 179 180 182 188 196 201 0 203 208 213 214 214 215

</details>

<details><summary>mt OFF: all 59 tool calls</summary>

| # | Turn | Tool | Secs | Result bytes | Command or target (first 110 characters) |
|---|---|---|---|---|---|
| 1 | 1 | Bash | 0.3 | 11,802 | `git ls-files \| grep -v '^\.' \| head -200 && cat README* 2>/dev/null \| head -80` |
| 2 | 2 | Bash | 0.2 | 2,347 | `git ls-files \| grep -v '^\.' \| tail -n +201; cat Specs/relay-backend-spec/10-auth-rbac.md; wc -l internal/ap` |
| 3 | 3 | Bash | 0.1 | 1,711 | `git ls-files \| grep -v '^\.' \| grep -E '^web/' \| grep -v node_modules \| head -80; wc -l internal/api/*.go ` |
| 4 | 4 | Bash | 0.1 | 19,420 | `grep -rniE 'google\|oidc\|oauth\|\bsso\b' -l --exclude-dir=node_modules --exclude-dir=.git . \| head -30; echo` |
| 5 | 5 | Bash | 0.0 | 17,078 | `cat internal/api/auth.go; sed -n 2400,2530p api/openapi.yaml` |
| 6 | 6 | Bash | 0.0 | 2,160 | `grep -n 'SSO\\|sso' internal/store/*.go internal/api/store.go internal/api/fake_test.go internal/api/router.go` |
| 7 | 7 | Bash | 0.0 | 20,829 | `cat internal/store/auth.go` |
| 8 | 8 | Bash | 0.0 | 12,496 | `cat internal/api/router.go; grep -n 'type Principal\\|type Cookie\\|type Me struct' -A 22 internal/api/store.g` |
| 9 | 9 | Bash | 0.1 | 14,596 | `cat web/src/screens/Login.tsx web/src/auth.tsx \| head -300; cat internal/storage/migrations/0004_authn_email_` |
| 10 | 10 | Bash | 0.9 | 4,087 | `grep -n 'CREATE TABLE app_user' -A 25 internal/storage/migrations/0001_init.up.sql; ls $(go env GOMODCACHE)/gi` |
| 11 | 11 | Bash | 0.2 | 1,022 | `M=$(go env GOMODCACHE); ls $M/github.com/coreos/go-oidc $M/github.com/go-jose/go-jose 2>/dev/null; ls $M/cache` |
| 12 | 12 | Bash | 0.0 | 5,220 | `sed -n 1,120p internal/store/store.go; grep -n 'store.New\\|store.Options\\|RequireTOTP\\|Sealer\\|PublicURL\\` |
| 13 | 12 | Bash | 0.1 | 8,349 | `sed -n 1,80p internal/api/fake_test.go; sed -n 200,230p internal/api/fake_test.go; sed -n 1,120p internal/api/` |
| 14 | 13 | Bash | 0.1 | 5,569 | `cat web/src/auth.tsx; grep -n 'next\\|partial' web/src/App.tsx \| head; sed -n 1,40p cmd/tsgen/main.go; grep -` |
| 15 | 14 | Bash | 0.1 | 6,230 | `sed -n 170,260p internal/app/app.go; grep -n 'type Deps' -A 40 internal/api/server.go; cat deploy/compose/.env` |
| 16 | 15 | Bash | 0.2 | 1,459 | `grep -rn 'auth.login.success\\|auth.sso' --include=*.go --include=*.sql . \| grep -v _test \| head; sed -n 395` |
| 17 | 16 | Bash | 0.7 | 162 | `GOPROXY=off GOFLAGS=-mod=mod go get github.com/coreos/go-oidc/v3@v3.20.0 golang.org/x/oauth2@v0.36.0 github.co` |
| 18 | 17 | Bash | 0.2 | 1,605 | `grep -n 'oauth2' go.mod; M=$(go env GOMODCACHE); grep -n '^func ' $M/github.com/coreos/go-oidc/v3@v3.20.0/oidc` |
| 19 | 18 | Write | 0.0 | 181 | `<wt>-off/internal/auth/oidc/oidc.go` |
| 20 | 19 | Write | 0.0 | 186 | `<wt>-off/internal/auth/oidc/oidc_test.go` |
| 21 | 19 | Bash | 4.5 | 53 | `GOFLAGS=-mod=mod GOPROXY=off go test ./internal/auth/oidc/ 2>&1 \| tail -30` |
| 22 | 20 | Bash | 0.7 | 866 | `python3 - <<'EOF' import re p='internal/api/store.go' s=open(p).read() s=s.replace(""" SSOCallback(ctx context` |
| 23 | 21 | Bash | 0.2 | 1,206 | `gofmt -d internal/api/server.go \| head -40` |
| 24 | 22 | Bash | 0.5 | 303 | `sed -i '' 's/^ Role string$/ Role string/' internal/api/server.go && python3 - <<'EOF' p='internal/api/auth.go` |
| 25 | 23 | Bash | 1.8 | 31 | `python3 - <<'EOF' p='internal/api/auth.go' s=open(p).read() a=s.index("// safeReturnTo admits only") b=s.index` |
| 26 | 24 | Bash | 2.2 | 224 | `python3 - <<'EOF' p='internal/store/auth.go' s=open(p).read() a=s.index("// SSOStart / SSOCallback are stubs")` |
| 27 | 25 | Bash | 0.1 | 596 | `grep -rn 'auth.login.success\\|auth\.totp\.failure' internal web/src api/openapi.yaml \| grep -v '_test.go' \|` |
| 28 | 26 | Bash | 0.4 | 1,695 | `python3 - <<'EOF' p='internal/config/config.go' s=open(p).read() s=s.replace(''' EnrollTokenMaxTTL time.Durati` |
| 29 | 27 | Bash | 0.2 | 202 | `sed -n 60,200p internal/config/config_test.go \| grep -n 'func Test\\|Load\\|validate' \| head; grep -n '^func` |
| 30 | 28 | Bash | 0.0 | 542 | `sed -n 60,85p internal/config/config_test.go` |
| 31 | 29 | Bash | 0.5 | 31 | `cat >> internal/config/config_test.go <<'EOF' func TestValidateRequiresGoogleIDAndSecretTogether(t *testing.T)` |
| 32 | 30 | Bash | 0.0 | 190 | `grep -rn 'SignupEnabled' internal/app/ cmd/ \| head` |
| 33 | 31 | Bash | 0.0 | 1,995 | `sed -n 105,125p internal/app/roles.go; grep -n 'type Deps struct' -A 25 internal/app/app.go \| head -30` |
| 34 | 32 | Bash | 0.6 | 536 | `python3 - <<'EOF' p='internal/app/app.go' s=open(p).read() s=s.replace(''' d.Store = store.New(store.Options{'` |
| 35 | 33 | Bash | 11.6 | 31 | `sed -i '' 's#^ "github.com/relay/relay/internal/api"$# "github.com/relay/relay/internal/api"\n "github.com/rel` |
| 36 | 34 | Bash | 0.0 | 250 | `sed -n 20,26p internal/app/app.go` |
| 37 | 35 | Bash | 0.0 | 4,140 | `sed -n 120,250p internal/api/routes_test.go` |
| 38 | 36 | Write | 0.0 | 179 | `<wt>-off/internal/api/sso_test.go` |
| 39 | 36 | Bash | 5.3 | 3,669 | `go test ./internal/api/ 2>&1 \| tail -30` |
| 40 | 37 | Bash | 1.4 | 139 | `go test ./internal/api/ 2>&1 \| grep -E -- '--- FAIL\|_test.go:' \| head -20; git stash list \| head -2 >/dev/` |
| 41 | 38 | Bash | 0.1 | 90 | `grep -n ' /v1/auth/logout:' api/openapi.yaml; grep -n ' SsoCallbackForm:' api/openapi.yaml; grep -n "Unauthori` |
| 42 | 39 | Bash | 6.0 | 212 | `python3 - <<'EOF' p='api/openapi.yaml' s=open(p).read() s=s.replace(''' /v1/auth/sso/{provider}/start:''',''' ` |
| 43 | 40 | Bash | 2.7 | 3,105 | `grep -n '^func \\|^type Peppers' internal/auth/secret/token.go internal/auth/secret/seal.go \| head -20; grep ` |
| 44 | 41 | Bash | 0.1 | 1,250 | `grep -n 'func \\|SET ROLE\\|set_config' internal/storage/tenant.go \| head -20; grep -rln 'testdb.New' --inclu` |
| 45 | 42 | Write | 0.2 | 181 | `<wt>-off/internal/store/sso_test.go` |
| 46 | 42 | Bash | 6.1 | 1,410 | `go vet ./internal/store/ && go test ./internal/store/ -v 2>&1 \| grep -E '^(=== RUN\|--- \|ok\|FAIL)\|sso_test` |
| 47 | 43 | Bash | 0.1 | 151 | `which postgres initdb pg_ctl psql 2>&1; ls /opt/homebrew/opt 2>/dev/null \| grep -i postgres; ls /usr/local/op` |
| 48 | 44 | Bash | 1.6 | 210 | **ERR** `SP=<scratch>-off/efc302f0-cf6d-4ee6-b200-58c0b1628e4e/scratchpad; mkdir -p $SP && cd $SP && /usr/local/opt/pos` |
| 49 | 45 | Bash | 1.5 | 392 | `SP=<scratch>-off/efc302f0-cf6d-4ee6-b200-58c0b1628e4e/scratchpad; ls /usr/local/opt/postgresql@17/bin/ \| head` |
| 50 | 46 | Bash | 1.4 | 168 | `SP=<scratch>-off/efc302f0-cf6d-4ee6-b200-58c0b1628e4e/scratchpad; /usr/local/bin/initdb -D $SP/pg -U postgres ` |
| 51 | 47 | Bash | 1.2 | 553 | `SP=<scratch>-off/efc302f0-cf6d-4ee6-b200-58c0b1628e4e/scratchpad; /usr/local/bin/initdb -D $SP/pg -U postgres ` |
| 52 | 48 | Bash | 0.3 | 8 | `lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null \| grep -i postgres \| head; ipcs -m 2>/dev/null \| wc -l` |
| 53 | 49 | Bash | 1.4 | 523 | `go run ./cmd/tsgen internal/api/store.go web/src/api/types.gen.ts && git diff --stat web/src/api/types.gen.ts ` |
| 54 | 50 | Bash | 0.5 | 43 | `python3 - <<'EOF' p='web/src/screens/Login.tsx' s=open(p).read() s=s.replace('''import { ApiError, post } from` |
| 55 | 51 | Bash | 4.3 | 349 | `npm ci --ignore-scripts --no-audit --no-fund 2>&1 \| tail -3 && npx tsc --noEmit 2>&1 \| tail -10 && echo TSC_` |
| 56 | 52 | Bash | 11.2 | 1,365 | `cd <wt>-off && go vet ./internal/... ./cmd/relayd/ 2>&1 \| tail -5; go test ./... 2>&1 \| grep -E '^(ok\|FAIL\` |
| 57 | 53 | Bash | 0.1 | 1,609 | `git status --short \| grep -v '^ D' ; sed -n 20,45p deploy/compose/docker-compose.yml` |
| 58 | 54 | Bash | 1.0 | 55 | `python3 - <<'EOF' p='deploy/compose/docker-compose.yml' s=open(p).read() s=s.replace(''' RELAY_AUTH_REQUIRE_TO` |
| 59 | 57 | Bash | 6.9 | 4,325 | `git diff internal/api/auth.go \| head -150 && go test ./internal/api/ ./internal/auth/oidc/ ./internal/store/ ` |

Context per turn (k tokens): 40 45 47 48 56 64 65 75 83 89 91 93 100 106 109 112 113 114 119 125 127 128 130 131 135 135 137 137 138 139 139 140 141 142 142 144 150 150 151 152 154 155 161 161 162 162 163 163 164 164 167 167 168 169 171 0 171 173

</details>

### Transactions, run 1: what Claude sent and what came back

These are verbatim excerpts. The full logs, every call with up to 40 lines each way, are in
`bench/transactions/g1-run1-on.md` and `bench/transactions/g1-run1-off.md`.

**Discovery.** mt ON's first batch (#4) answered 6 questions in 50 ms. The header says
"~4042 tokens" but the reply was 25,232 B (~6.3k tokens): the bug fixed in section 17. mt OFF
built the same picture by `cat`-ing whole files (#5 `auth.go`, #7 `store/auth.go`).

<details><summary>mt ON #4 (one mt query), #6 (hook redirect)</summary>

**#4 · turn 4 · Bash · 0.7 s · reply 25,232 B (~6,308 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
{"queries":[
 {"id":"a","context":"Server.startSso"},
 {"id":"b","context":"Server.completeSso"},
 {"id":"c","context":"Store.SSOStart"},
 {"id":"d","context":"Store.SSOCallback"},
 {"id":"e","context":"Store.issueSession"},
 {"id":"f","grep":"sso_subject|oidc|google","regex":true}
],"budget":14000}
EOF
~~~~

Reply:
~~~~
# mt query: 6 queries: 0 ✓ 0 ✗ 6 answered 0 can't check; 65 items (5 inlined), ~4042 tokens, 50 ms (index: 289 files, 0 re-parsed, 9 ms)
a = context Server.startSso   [9 ms; what `mt context` gathers]
    1 target(s), 15 related
    items: [1] [3] [6] [7] [8] [9] [10] [11] [13] [14] [16] [17] [18] [19] [22] (1 not shown: output limit)
b = context Server.completeSso   [8 ms; what `mt context` gathers]
    1 target(s), 18 related
    items: [2] [4] [6] [7] [8] [10] [11] [13] [16] [17] [18] [19] [22] (6 not shown: output limit)
c = context Store.SSOStart   [7 ms; what `mt context` gathers]
    1 target(s), 9 related
    items: [1] [3] [6] [7] [8] [9] [12] [15] [21] [23]
d = context Store.SSOCallback   [8 ms; what `mt context` gathers]
    1 target(s), 11 related
    items: [1] [2] [4] [6] [7] [8] [12] [15] [20] [21] [23] (1 not shown: output limit)
e = context Store.issueSession   [8 ms; what `mt context` gathers]
    1 target(s), 10 related
    items: [5] [6] [7] [14] [24] [25] [57] (4 not shown: output limit)
f = grep sso_subject|oidc|google   [11 ms; text scan of 287 indexed files: 69 lines in 65 spans]
    items: [26] [27] [28] [29] [30] [31] [32] [33] [34] [35] [36] [37] [38] [39] [40] [41] [42] [43] [44] [45] [46] [47] [48] [49] [50] [51] [52] [53] [54] [55] +9 (26 not shown: output limit)
## code
[1] internal/store/auth.go:L560-567 method Store.SSOStart h=70ea483f  (a,c,d)  why: a; used by [1]; c; d; references [1] at L560 (name defined in 2 files: possible)
560  // SSOStart / SSOCallback are stubs in this build: OIDC and SAML need per-org
561  // provider configuration in org.settings.auth and their own metadata fetch.
562  // Returning not_implemented is honest; silently redirecting somewhere would not
563  // be. The route stays registered so the contract test passes and the client's
… (427 more lines)
~~~~

**#6 · turn 6 · Bash · 0.2 s · reply 372 B (~93 tok) · ERROR**

Claude sent:
~~~~
M=$(go env GOMODCACHE); ls $M/github.com/coreos/go-oidc $M/github.com/go-jose; ls $M/cache/download/github.com/coreos/go-oidc/v3/@v/ 2>/dev/null; cat internal/storage/migrations/0004_authn_email_lookup.up.sql; grep -n -A30 '^## 4' Specs/relay-backend-spec/api/web-client.md | head -50; mt query - <<'EOF'
{"queries":[
 {"id":"p","span":"internal/api/store.go:Principal"},
 {"id":"o","span":"internal/store/store.go:Options"},
 {"id":"new","context":"store.New","in":"internal/store/store.go"},
 {"id":"cfgauth","span":"internal/config/config.go:L126-140"},
 {"id":"login","span":"web/src/screens/Login.tsx:Login"},
 {"id":"tdb","span":"internal/storage/testdb/testdb.go:New"},
 {"id":"fk","grep":"func \\(f \\*fakeStore\\) (SSO|Login|VerifyTOTP)","regex":true,"in":"internal/api/"},
 {"id":"rt","grep":"sso|newTestServer\\(","regex":true,"in":"internal/api/routes_test.go"},
 {"id":"pep","grep":"func \\(p Peppers\\)|type Peppers","regex":true,"in":"internal/auth/secret/"},
 {"id":"cookie","context":"Server.setSessionCookie"}
],"budget":12000}
EOF
~~~~

Reply:
~~~~
PreToolUse:Bash hook error: This repo has mt. Same lines, structured, in one call: `mt skeleton internal/storage/migrations/0004_authn_email_lookup.up.sql; mt span internal/storage/migrations/0004_authn_email_lookup.up.sql auth_lookup_user_by_email`. Put any other questions in the same call (or one `mt query`). Plain grep is still fine for literal text, logs and config.
~~~~

</details>

<details><summary>mt OFF #5, #7 (whole-file reads)</summary>

**#5 · turn 5 · Bash · 0.0 s · reply 17,078 B (~4,269 tok)**

Claude sent:
~~~~
cat internal/api/auth.go; sed -n 2400,2530p api/openapi.yaml
~~~~

Reply:
~~~~
package api

import (
	"bytes"
	"io"
	"net/http"
	"strings"

	"github.com/go-chi/chi/v5"

	"github.com/relay/relay/internal/platform/httpx"
)

type loginRequest struct {
… (444 more lines)
~~~~

**#7 · turn 7 · Bash · 0.0 s · reply 20,829 B (~5,207 tok)**

Claude sent:
~~~~
cat internal/store/auth.go
~~~~

Reply:
~~~~
package store

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"sync"
	"time"

… (588 more lines)
~~~~

</details>

**Edits.** mt ON sent one 27-op batch.
- **#39:** refused before writing anything. Op 23's `find` text occurred 3 times, and the error
  didn't say where; fixed in section 17.
- **#40:** applied, with its check steps. The web step failed only because `node_modules` was
  missing; section 17 adds the ENV status for this.

mt OFF wrote the same kind of edits as unchecked Python string-replace scripts (#22, #24).

<details><summary>mt ON #39, #40 (mt apply)</summary>

**#39 · turn 35 · Bash · 0.5 s · reply 207 B (~51 tok) · ERROR**

Claude sent:
~~~~
SP=/private/tmp/claude-501/-Users-oshricohen-orca-workspaces-claude-coach-mt-eval-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad && python3 - "$SP" > $SP/batch.json <<'PY'
import json,sys
sp=sys.argv[1]
r=lambda f: open(f"{sp}/{f}").read()
OIDC='\t"github.com/relay/relay/internal/auth/oidc"\n'
ops=[
 {"file":"internal/store/store.go","find":'\t"github.com/relay/relay/internal/auth/secret"\n',"with":OIDC+'\t"github.com/relay/relay/internal/auth/secret"\n'},
 {"file":"internal/store/store.go","find":"\tSealer   secret.Sealer\n","with":"\tSealer   secret.Sealer\n\t// SSO maps a URL segment (\"google\") to its OIDC relying party. Empty\n\t// leaves /v1/auth/sso/* answering not_implemented.\n\tSSO      map[string]*oidc.Provider\n"},
 {"file":"internal/store/auth.go","find":'\t"encoding/json"\n\t"fmt"\n',"with":'\t"encoding/json"\n\t"errors"\n\t"fmt"\n'},
 {"file":"internal/store/auth.go","find":'\t"github.com/relay/relay/internal/auth/secret"\n',"with":OIDC+'\t"github.com/relay/relay/internal/auth/secret"\n'},
… (106 more lines)
~~~~

Reply:
~~~~
Exit code 1
mt apply: nothing written, 1 problem(s):
  op 23: web/src/screens/Login.tsx: "  const redirectTo = params.get(\"redirect_to\") ?? \"/\"\n" occurs 3 times; narrow it with a span or set "all": true
~~~~

**#40 · turn 36 · Bash · 13.1 s · reply 10,040 B (~2,510 tok) · ERROR**

Claude sent:
~~~~
SP=/private/tmp/claude-501/-Users-oshricohen-orca-workspaces-claude-coach-mt-eval-on/d39f1bb4-4fe3-4bf7-a4ab-2109f758c06d/scratchpad && python3 - "$SP" <<'PY'
import json,sys
p=sys.argv[1]+"/batch.json"; b=json.load(open(p))
op=b["ops"][22]
op["find"]='  const [error, setError] = useState<unknown>(null)\n  const redirectTo = params.get("redirect_to") ?? "/"\n\n  useEffect(() => {\n    if (me && !me.partial)'
op["with"]='  const [error, setError] = useState<unknown>(null)\n'+op["with"]+'\n  useEffect(() => {\n    if (me && !me.partial)'
json.dump(b,open(p,"w"))
PY
mt apply - < $SP/batch.json
~~~~

Reply:
~~~~
Exit code 1
applied 27 edits to 13 files in 12174 ms (validate 6 ms, baselines 0 ms, write 12 ms, reindex 113 ms, steps 12038 ms). undo: mt undo
steps (non-gate steps in parallel):
   1 PASS  gofmt-new  gofmt -w internal/auth/oidc internal/app/sso.go internal/api/sso_test.go internal/store/sso_test.go internal/config/sso_test.go && gofmt -l internal/auth/oidc internal/app/sso.go internal/api/sso_test.go internal/store/sso_test.go internal/config/sso_test.go  (109 ms)
   2 PASS  build      go build ./... && go vet ./internal/... ./cmd/...  (9033 ms, gate)
   3 PASS  unit       go test ./internal/auth/... ./internal/api/ ./internal/config/ ./internal/app/ ./internal/store/ -count=1  (2895 ms)
   4 FAIL  web        cd web && npx tsc --noEmit -p . && npm test --silent 2>&1 | tail -5  (927 ms)  full log: .mt/txn/1793913880772/step-4.log
       | error TS2688: Cannot find type definition file for 'vite/client'.
       |   The file is in the program because:
       |     Entry point of type library 'vite/client' specified in compilerOptions
result: some steps failed; the edits are KEPT (only gate steps roll back). Fix forward in the next batch, or run mt undo.
changed regions (code as it now is on disk, + marks changed lines; spans and hashes are ready for the next batch):
[api/openapi.yaml:L2407-2466 lines h=e8ef2a8a]
2407              X-Request-Id: { $ref: '#/components/headers/RequestId' }
2408          '401': { $ref: '#/components/responses/Unauthorized' }
2409  
2410+   /v1/auth/sso/providers:
2411+     get:
2412+       operationId: listSsoProviders
2413+       tags: [Auth]
2414+       summary: List the SSO providers configured on this deployment.
2415+       description: |
2416+         Lets the sign-in screen render only the "Continue with …" buttons that work. Public:
2417+         provider names are not secret. `google` is present when `RELAY_AUTH_GOOGLE_CLIENT_ID` is set.
… (198 more lines)
~~~~

</details>

<details><summary>mt OFF #22, #24 (python edit scripts)</summary>

**#22 · turn 20 · Bash · 0.7 s · reply 866 B (~216 tok)**

Claude sent:
~~~~
python3 - <<'EOF'
import re
p='internal/api/store.go'
s=open(p).read()
s=s.replace("""	SSOCallback(ctx context.Context, provider string, params map[string]string, body []byte, cookies map[string]string, ip, ua string) (*Principal, string, time.Time, string, error)""",
"""	SSOCallback(ctx context.Context, provider string, params map[string]string, body []byte, cookies map[string]string, ip, ua string) (*SSOResult, error)""")
s=s.replace("""type Cookie struct {""","""// SSOResult is a completed SSO callback: the session it earned and the
// destination recorded at start. ReturnTo is already validated as a same-origin
// path by the store, and validated again by the handler.
type SSOResult struct {
	Principal *Principal
	Token     string
	Expires   time.Time
	CSRF      string
	ReturnTo  string
}

// AuthProviders is the pre-authentication sign-in menu. It names which methods
// are configured on this deployment and nothing about any org or user.
type AuthProviders struct {
	Password bool     `json:"password"`
	SSO      []string `json:"sso"`
}

… (49 more lines)
~~~~

Reply:
~~~~
internal/api/errors.go
internal/api/eval.go
internal/api/page.go
internal/api/routes_test.go
internal/api/server.go
internal/app/roles.go
internal/assembler/fold.go
internal/assembler/idempotency_test.go
… (23 more lines)
~~~~

**#24 · turn 22 · Bash · 0.5 s · reply 303 B (~75 tok)**

Claude sent:
~~~~
sed -i '' 's/^	Role          string$/	Role         string/' internal/api/server.go && python3 - <<'EOF'
p='internal/api/auth.go'
s=open(p).read()
old_start=s[s.index("// completeSso is registered"):s.index("type signupRequest struct")]
new='''// completeSso is registered for BOTH GET and POST on the same path: OIDC returns
// the authorisation code on a top-level GET redirect, and SAML's assertion
// consumer service receives a form POST (00 §0.5).
//
// The flow cookie is cleared on every outcome: it is single-use, and leaving a
// spent state behind only gives a replay something to aim at. A failure is a
// redirect to the sign-in page rather than a JSON body, because the caller is a
// browser mid-navigation; the reason is uniform ("sso_failed") so the page
// cannot be used to probe which check rejected the assertion.
func (s *Server) completeSso(w http.ResponseWriter, r *http.Request) error {
	provider := chi.URLParam(r, "provider")
	params := map[string]string{}
	for k, v := range r.URL.Query() {
		if len(v) > 0 {
			params[k] = v[0]
		}
	}
	var body []byte
	if r.Method == http.MethodPost {
		b, err := io.ReadAll(io.LimitReader(r.Body, s.d.Limits.MaxJSONBody))
… (93 more lines)
~~~~

Reply:
~~~~
207:func safeReturnTo(s string) string {
208-	if s == "" || s[0] != '/' || strings.HasPrefix(s, "//") || strings.HasPrefix(s, "/\\") ||
209-		strings.ContainsAny(s, "\r\n\t") {
210-		return "/"
211-	}
[This command modified 1 file you've previously read: internal/api/auth.go. Call Read before editing.]
~~~~

</details>


---

## 2026-11-05 · 17. Fixes from section 16, then G1 run 2

### Fixes, each tied to evidence from run 1

All of them are verified on a scratch repo; `cargo test`: 21 passed.

| Fix | Run 1 evidence | Verified |
|---|---|---|
| `mt apply` bodies from files: `replace_file`, `with_file`, `text_file`, `insert_after_file` (read verbatim) | ON wrote scratch files, then had Python assemble a 27-op batch | body applied from the file; body plus `_file` is refused |
| An ambiguous `find` lists every match as a ready `"span", "hash"` pair | #39: "occurs 3 times", with no locations | pasted the second pair, which picked that match |
| `ENV` step status for exit 126/127, `command not found`, TS2688, `npm ERR! missing`; it doesn't fail the batch | the web step failed because `node_modules` was missing | an ENV-only batch exits 0; a real TS2304 error stays FAIL |
| `span` op inside `mt query` (`path:Name`, `path:L10-20`, `path:/regex/`) | #7: 6 of 11 queries came back "can't check", then were redone as 7 single calls | name, line-range and regex forms work, and a miss is reported |
| Header shows the real reply size (`reply ~N tokens`) | #4 said ~4,042 tokens; the real reply was ~6,300 | |
| `MT_INJECT_SKILL=0` plugin knob | the skill was injected twice | context drops from 9,475 to 374 characters |
| The rule, aligned with the task: `exhaustive` claims only when changing existing sites; keep the default budget; bodies from files | partial rule compliance; budget 14000 | |

### Run 2 setup

Same as section 16, with these changes:
- mt ON runs the new build with `MT_INJECT_SKILL=0` and the updated rule.
- Both worktrees were reset to `bd5fa80`, keeping only the agent-config deletions; `node_modules` and `.mt` were removed.
- The same prompt went to both. At +90 s both got the same note: "No database is available on this machine: do not try to start Postgres or Docker, and do not run the database-backed tests. Write them, run everything else."
- There were no interruptions.

### Results

| | ON run 2 | OFF run 2 | ON vs OFF | ON run 1 | OFF run 1 |
|---|---|---|---|---|---|
| Turns | **27** | 48 | **-44%** | 40 | 58 |
| Tool calls | **27** | 56 | **-52%** | 43 | 59 |
| Active time | **694 s** | 796 s | **-13%** | 838 s | 757 s |
| Cost | **$3.431** | $3.943 | **-13%** | $4.006 | $3.684 |
| Cache read | 3.58M | 6.54M | -45% | 5.65M | 7.16M |
| Cache write | 174k | 157k | +11% | 186k | 144k |
| Output tokens | 66.2k | 69.0k | -4% | 69.3k | 55.0k |
| Context at turn 1 | 45.3k | 40.3k | | 48.2k | 40.3k |
| Peak context | 203k | 188k | +8% | 216k | 173k |
| Tool result bytes | 200,126 | 168,835 | +19% | 207,634 | 169,386 |
| Hook denials | 2 (#2, #21) | | | 4 | |
| OFF shell reads / edit scripts | | 152 / 13 | | | 154 / 9 |

**Three ways to count mt use** (counted from the command text):

| mt ON | Run 1 | Run 2 |
|---|---|---|
| Bash calls that run mt (round trips) | 18 | 12 |
| mt commands run | 65 (46 `span`, 8 `skeleton`, 9 `query`, 2 `apply`) | 13 (9 `query`, 1 `span`, 1 `skeleton`, 2 `apply`) |
| Items inside query batches | 43 | 64 |
| Bash calls chaining several mt commands | 11 | 1 |

**Outcome.** Checked by me, without the DB-backed tests, as asked.
- Both arms: `go build ./...`, `go vet ./...`, `go test ./...` and `tsc --noEmit` pass.
- Skipped DB tests: ON 12, OFF 17.
- Diffs: ON 24 files, +1805 -71; OFF 24 files, +1949 -68.
- **Quality difference:** ON run 2 wrote its own Google ID-token verifier with no new dependencies (`internal/auth/google`, 378 lines: RS256 pinned, JWKS fetch and cache, iss/aud/azp/exp/iat/nonce, `email_verified`, `hd`). This was despite the spec it read naming `go-oidc` plus `x/oauth2`. OFF used `coreos/go-oidc` (as did both run-1 arms). Hand-written token verification is more security surface to review; this is a model choice, not an mt effect, but it belongs next to the cost numbers.

### Why mt ON still needed 12 round trips

1. **Reading takes 8 sequential batches (#4-#13).** Each one spans what the previous one found:
   - grep `oidc|oauth|google|sso`;
   - then spans of `auth.go`, the store and the router;
   - then `config`, migrations and the module cache;
   - then `Peppers`, OpenAPI and `.env`;
   - then `api.Deps` and `roles.go`.
   
   `$id` passes on only names, so "read what q1 found" can't go in the same batch.
2. **Every batch was near the 28 KB cap** (#4 27.7 KB, #6 27.7 KB, #7 26.4 KB). #4 was cut off:
   - its regex `span` over the auth spec inlined 12 sections, about 32k tokens;
   - requested spans were always inlined, so the shrink loop could not reduce them;
   - `skeleton` was not a query op ("can't check").
3. **Writing takes 6 calls (#14-#22).** Heredoc `cat >` for each new file, `gofmt`, then the apply.
4. **The apply's format step reformatted the repo.** `gofmt -w` on whole directories rewrote 12+ unrelated, already-unformatted files (#23). Two calls (#24 `git checkout -- …`, #25) went to undoing that.

### Bugs fixed after run 2

These are in `target-dev`, verified by replaying run 2's #4 against a clean `bd5fa80` export.

| Bug | Fix | Replay of run 2's #4 |
|---|---|---|
| Requested spans were always inlined, so the reply was cut at 28 KB | spans inline up to 70% of the cap; the rest become pointers, with a note to ask for them separately | before: claimed ~32k tokens, cut; after: no cut, 5 pointers plus the note |
| `skeleton` was not a query op | added | `authsk` gives 28 definitions instead of `?` |
| Pointers did not count against `budget` (80 per batch) | past 20, pointers come out of the budget | 128 grep hits give 20 items, not 80 |

### Transactions, run 2

Full logs: `bench/transactions/g1-run2-on.md`, `bench/transactions/g1-run2-off.md`.

<details><summary>mt ON #4 (cut-off batch), #6 (span batch)</summary>

**#4 · turn 3 · Bash · 0.8 s · reply 27,752 B (~6,938 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
[
 {"id":"sso","grep":"(?i)oidc|oauth|google|sso","regex":true},
 {"id":"authsk","skeleton":"internal/api/auth.go"},
 {"id":"spec","span":"Specs/relay-backend-spec/10-auth-rbac.md:/(?i)sso|oidc/"}
]
EOF
~~~~

Reply:
~~~~
# mt query: 3 queries: 0 ✓ 0 ✗ 2 answered 1 can't check; 12 items (12 inlined, ~31996 tokens of bodies), reply ~32436 tokens, 133 ms (index: 289 files, 0 re-parsed, 86 ms)
sso = grep (?i)oidc|oauth|google|sso   [6 ms; text scan of 287 indexed files: 169 lines in 128 spans]
    items: [1] [2] [3] [4] [5] [6] [7] [8] [9] [10] [11] [12] (116 not shown: output limit)
authsk ? {"id":"authsk","skeleton":"internal/api/auth.go"}   [0 ms]
    no known op; use one of: context, defined_in, exists, references, callers, tested_by, constructs, has_field, calls, uses_table, property, grep, span
spec = span Specs/relay-backend-spec/10-auth-rbac.md:/(?i)sso|oidc/   [2 ms; 12 span(s)]
    items: [1] [2] [3] [4] [5] [6] [7] [8] [9] [10] [11] [12]
## code
[1] Specs/relay-backend-spec/10-auth-rbac.md:L308-316 h3 10.4.6 SAML h=9c4ce192  (sso,spec)  why: sso: matches; spec: span /(?i)sso|oidc/
308  ### 10.4.6 SAML
309  
310  `github.com/crewjam/saml` and `github.com/crewjam/saml/samlsp` `[VERIFY]`. SP metadata is served per-org; the ACS is the same callback path as OIDC. Validation on every assertion: signature on the `Response` **and** on the `Assertion` (a signature on only one of the two is the classic bypass), `Destination` equal to our ACS URL, `NotBefore`/`NotOnOrAfter` with at most 60 s of clock tolerance, `AudienceRestriction` equal to our entity ID, and `InResponseTo` equal to the `AuthnRequest` ID we issued.
311  
312  SP-initiated state is carried in a `relay_saml_req` cookie holding the signed `AuthnRequest` ID, `Secure; HttpOnly; SameSite=None; Max-Age=600`. `SameSite=None` is unavoidable — the ACS POST arrives cross-site from the IdP — and is precisely why that cookie must be single-purpose, short-lived, and carry no authority whatsoever.
313  
314  **IdP-initiated SAML is rejected by default**, and it should be. With no `AuthnRequest`, `InResponseTo` is empty, so there is nothing to bind the assertion to: an attacker who obtains an assertion (or induces the IdP to issue one) and gets a victim's browser to POST it can sign that browser into an account of the attacker's choosing — login CSRF, which here means a developer's session capture flowing into an attacker-controlled org, or a manager unknowingly operating as someone else. It also discards RelayState as a trustworthy destination. `org.settings.auth.saml.allow_idp_initiated` defaults false; enabling it requires a 2-minute assertion lifetime, a *shared* assertion-ID replay cache, and a landing interstitial requiring a user gesture before the session cookie is set, and it writes `audit_log` action `org.saml_idp_initiated_enabled`. It is a documented downgrade, not a preference.
… (165 more lines)
~~~~

**#6 · turn 5 · Bash · 0.4 s · reply 27,732 B (~6,933 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
[
 {"id":"a1","span":"internal/api/auth.go:L1-48"},
 {"id":"a2","span":"internal/api/auth.go:L94-176"},
 {"id":"s1","span":"internal/store/auth.go:L1-22"},
 {"id":"s2","span":"internal/store/auth.go:L197-309"},
 {"id":"s3","span":"internal/store/auth.go:L531-602"},
 {"id":"st","span":"internal/api/store.go:L736-808"},
 {"id":"rt","span":"internal/api/router.go:L14-62"},
 {"id":"fk","span":"internal/api/fake_test.go:L1-30"},
 {"id":"fk2","span":"internal/api/fake_test.go:L199-222"},
 {"id":"cfg","span":"internal/config/config.go:L19-165"},
… (4 more lines)
~~~~

Reply:
~~~~
# mt query: 12 queries: 0 ✓ 0 ✗ 12 answered 0 can't check; 10 items (10 inlined, ~7538 tokens of bodies), reply ~7865 tokens, 67 ms (index: 289 files, 0 re-parsed, 59 ms)
a1 = span internal/api/auth.go:L1-48   [0 ms; 1 span(s)]
    items: [4]
a2 = span internal/api/auth.go:L94-176   [0 ms; 1 span(s)]
    items: [8]
s1 = span internal/store/auth.go:L1-22   [0 ms; 1 span(s)]
    items: [1]
s2 = span internal/store/auth.go:L197-309   [0 ms; 1 span(s)]
    items: [9]
s3 = span internal/store/auth.go:L531-602   [0 ms; 1 span(s)]
    items: [6]
st = span internal/api/store.go:L736-808   [0 ms; 1 span(s)]
    items: [7]
rt = span internal/api/router.go:L14-62   [0 ms; 1 span(s)]
    items: [5]
fk = span internal/api/fake_test.go:L1-30   [0 ms; 1 span(s)]
… (616 more lines)
~~~~

</details>

<details><summary>mt ON #23 (apply: gofmt -w over whole directories), #24 (undoing it)</summary>

**#23 · turn 22 · Bash · 16.6 s · reply 10,040 B (~2,510 tok) · ERROR**

Claude sent:
~~~~
mt apply - <<'EOF'
{"ops": [
 {"file":"internal/api/auth.go","find":"\t\"net/http\"\n\t\"strings\"\n","with":"\t\"net/http\"\n\t\"net/url\"\n\t\"strings\"\n"},
 {"regex":"(?s)// completeSso is registered for BOTH.*?\n\thttp.Redirect\\(w, r, dest, http.StatusFound\\)\n\treturn nil\n}\n","files":["internal/api/auth.go"],"with_file":"/private/tmp/claude-501/-Users-oshricohen-orca-workspaces-claude-coach-mt-eval-on/b2f22de6-93c1-4bfb-875c-7c7a3ce8cc70/scratchpad/completeSso.go","expect":1},
 {"file":"internal/api/store.go","find":"\tNext    string // totp | totp_enroll | done\n}\n","with":"\tNext    string // totp | totp_enroll | done\n\t// RedirectTo is set by SSOCallback: the destination the flow began with.\n\tRedirectTo string\n}\n"},
 {"file":"internal/api/server.go","find":"\tSignupEnabled bool\n\tTrustProxy    bool\n","with":"\tSignupEnabled bool\n\tTrustProxy    bool\n\t// SSOProviders are the deployment-level providers GET /v1/auth/providers lists.\n\tSSOProviders []string\n"},
 {"file":"internal/api/router.go","find":"\t\tg.Get(\"/v1/auth/sso/{provider}/start\", s.wrap(s.startSso))\n","with":"\t\tg.Get(\"/v1/auth/providers\", s.wrap(s.authProviders))\n\t\tg.Get(\"/v1/auth/sso/{provider}/start\", s.wrap(s.startSso))\n"},
 {"regex":"(?s)// SSOStart / SSOCallback are stubs.*?\nfunc \\(s \\*Store\\) SSOCallback.*?\n}\n\n","files":["internal/store/auth.go"],"with":"","expect":1},
… (22 more lines)
~~~~

Reply:
~~~~
Exit code 1
applied 21 edits to 13 files in 16111 ms (validate 11 ms, baselines 0 ms, write 3 ms, reindex 115 ms, steps 15977 ms). undo: mt undo
steps (in order):
   1 FAIL  fmt        gofmt -w internal/api internal/store internal/app internal/config internal/auth && out=$(gofmt -l internal cmd pkg); echo "$out"; test -z "$out"  (175 ms)  full log: .mt/txn/1793915402270/step-1.log
       | internal/platform/metrics/metrics.go
       | internal/realtime/client.go
       | internal/realtime/fanout.go
       | internal/realtime/hub.go
       | internal/realtime/proto.go
       | internal/storage/blob/blob.go
       | internal/storage/migrate.go
       | internal/storage/pool.go
       | cmd/relay-agent/internal/cred/cred.go
       | cmd/relay-agent/internal/cred/keyring_other.go
       | cmd/relay-agent/internal/normalize/normalize.go
       | cmd/relay-agent/internal/opencode/adapter.go
       | cmd/relay-agent/internal/redact/redact_test.go
       | cmd/relay-agent/internal/shipper/shipper.go
       | cmd/relay-hook/main.go
   2 PASS  build      go build ./...  (7212 ms, gate)
   3 PASS  vet        go vet ./...  (1516 ms)
   4 PASS  unit       go test ./internal/... ./cmd/... ./pkg/... -count=1  (6864 ms)
… (162 more lines)
~~~~

**#24 · turn 23 · Bash · 0.2 s · reply 836 B (~209 tok)**

Claude sent:
~~~~
git status --short | grep -v '^ D \.\|^D  \.\| D AGENTS\| D GEMINI\| D opencode' ; ls web/node_modules 2>&1 | head -2; ls ../*/web/node_modules/.bin/tsc 2>/dev/null | head -3
~~~~

Reply:
~~~~
 M README.md
 M api/openapi.yaml
 M deploy/compose/.env.example
 M internal/api/auth.go
 M internal/api/errors.go
 M internal/api/eval.go
 M internal/api/page.go
 M internal/api/router.go
 M internal/api/routes_test.go
 M internal/api/server.go
 M internal/api/store.go
 M internal/app/app.go
 M internal/app/roles.go
 M internal/auth/secret/seal.go
 M internal/config/config.go
 M internal/store/auth.go
 M internal/store/store.go
 M web/src/screens/Login.tsx
?? internal/api/sso_test.go
?? internal/app/sso.go
?? internal/auth/google/
?? internal/config/google_test.go
… (6 more lines)
~~~~

</details>

<details><summary>mt OFF, the first 3 reads</summary>

**#2 · turn 2 · Bash · 0.1 s · reply 2,248 B (~562 tok)**

Claude sent:
~~~~
cat Specs/relay-backend-spec/10-auth-rbac.md; wc -l internal/api/auth.go internal/api/router.go internal/api/store.go internal/api/fake_test.go internal/api/routes_test.go internal/config/config.go; git ls-files | sed -n '200,400p' | grep -v '^\.' 
~~~~

Reply:
~~~~
<persisted-output>
Output too large (74.4KB). Full output saved to: <claude-profile>/tool-results/…

Preview (first 2KB):
# 10 — Authentication, authorisation, enrolment, audit

Owns every credential in the system and every decision made about one: machine tokens and the enrolment exchange that mints them, human sessions with password/TOTP and SSO, SCIM provisioning, the `authz.Scope` that `07-query-api.md` and `09-evaluation.md` consume, the `audit_log` writer, capture-policy versioning and the killswitch, and Relay staff break-glass. Implements PRD FR-A.31/A.32/A.34, FR-E.1–E.4, FR-H.4/H.5/H.6/H.9. This is the surface an enterprise security review reads first, so it is specified to be *defensible*, not merely functional — where a control is weaker than its name suggests, this file says so.

… (24 more lines)
~~~~

**#3 · turn 3 · Bash · 0.1 s · reply 15,218 B (~3,804 tok)**

Claude sent:
~~~~
grep -n -i 'sso\|oidc\|google\|idp' Specs/relay-backend-spec/10-auth-rbac.md | head -80; wc -l internal/api/*.go internal/config/config.go; ls internal internal/auth internal/store 2>/dev/null; ls migrations db 2>/dev/null | head
~~~~

Reply:
~~~~
3:Owns every credential in the system and every decision made about one: machine tokens and the enrolment exchange that mints them, human sessions with password/TOTP and SSO, SCIM provisioning, the `authz.Scope` that `07-query-api.md` and `09-evaluation.md` consume, the `audit_log` writer, capture-policy versioning and the killswitch, and Relay staff break-glass. Implements PRD FR-A.31/A.32/A.34, FR-E.1–E.4, FR-H.4/H.5/H.6/H.9. This is the surface an enterprise security review reads first, so it is specified to be *defensible*, not merely functional — where a control is weaker than its name suggests, this file says so.
17:| Second factor | none, and none is possible | TOTP or IdP-enforced MFA |
188:| 1 | `sso` | interactive `relay login` browser flow | `high` | exact match on `app_user.sso_subject` within the org |
213:> **Where this file's state lives in 03.** Password and TOTP state are four columns on `app_user` — `password_hash` (PHC-encoded argon2id; NULL means SSO-only), `password_changed_at`, `recovery_codes` (argon2id each, single-use) and `totp_last_step` (the replay guard, §10.4.3) — carried in `03` §3.3, which owns the DDL. Break-glass grants are `staff_grant` in `03` §3.10 (§10.8). Do not invent a different location for either.
229:The operational consequence specs usually miss: at 64 MiB × 2 threads, twenty concurrent logins transiently allocate 1.3 GiB. `POST /v1/auth/login` therefore runs behind a semaphore of `RELAY_AUTH_LOGIN_CONCURRENCY` (default 8) with a 2 s queue timeout, returning `503` with `Retry-After: 2` rather than driving the api role into the OOM killer. Raising the argon2 parameters without lowering this semaphore is a memory incident waiting for a Monday morning. A user with `password_hash IS NULL` is SSO-only, and login must still spend the full argon2 cost against a fixed dummy hash before returning the uniform failure (§10.9).
240:**Why `SameSite=Lax` and not `Strict`.** `Strict` withholds the cookie on the first top-level cross-site GET navigation, which breaks the two navigations this product depends on most: the landing after an SSO redirect chain (the user arrives from the IdP's origin and lands logged-out), and emailed deep links into a session ("your manager commented on this session") arriving from a webmail origin. `Lax` sends the cookie on those and still blocks cross-site `POST`/`PUT`/`PATCH`/`DELETE`. `Strict` is also not a complete CSRF defence — it does not constrain a same-site subdomain attacker, which matters for a product handing customers vanity subdomains — so trading it away costs less than it looks. `Lax` plus the double-submit token and `Origin` check in §10.9 is strictly stronger than `Strict` alone. Lookup, through `auth_lookup_session`:
254:**Rotation on privilege change** — the token value changes, not just the row. On each of `totp_ok` transitioning false→true, a role change, a password change, TOTP enrolment or reset, and linking an SSO identity, the current row is revoked and a **new row with a new token** issued in the same transaction. Rotating across the MFA boundary is what prevents session fixation: an attacker who plants a pre-authentication cookie holds a value that dies the moment the second factor succeeds. `POST /v1/auth/logout` revokes the presented session only; revoking all of a user's sessions is a side effect of password change, TOTP reset and SCIM deprovision.
270:Blob layout: `version(1) || len(wrapped_dek)(2) || wrapped_dek || nonce(12) || AES-256-GCM ciphertext`. AAD is `org_id || 0x00 || user_id`, so a blob cannot be moved between rows even by someone with `UPDATE` on the table. *Cloud:* `kms.Sealer` generates a fresh 32-byte DEK per write and wraps it with the KMS KEK named by `RELAY_KMS_KEY_ID` (AWS KMS `GenerateDataKey`/`Decrypt`); the plaintext DEK never persists. Version byte `0x01`. *Self-hosted fallback:* `local.Sealer` derives the KEK from `RELAY_AUTH_SECRET_KEY` (32 bytes base64, required if any org has TOTP enabled) via HKDF-SHA256 with info `"relay/totp/v1"`, same blob format, version byte `0x02`. The caveat belongs in the install docs and not only here: this puts the KEK in the process environment, so a host compromise reads TOTP secrets. True of everything short of an HSM, and TOTP is a second factor rather than a first — but state it rather than implying it. The same `Sealer` protects OIDC client secrets and SAML SP keys stored in `org.settings`.
… (72 more lines)
~~~~

**#4 · turn 4 · Bash · 0.0 s · reply 19,170 B (~4,792 tok)**

Claude sent:
~~~~
cat internal/api/auth.go internal/api/router.go
~~~~

Reply:
~~~~
package api

import (
	"bytes"
	"io"
	"net/http"
	"strings"

… (552 more lines)
~~~~

</details>


---

## 2026-11-05 · 18. Two-call protocol: span chaining, `@@` bundle, session dedupe; G1 run 3

**Goal (user).** Cut the back-and-forth. The model plans the whole task and lays it out as
batches; mt does every mechanical follow-up inside a call.

### What changed

| Change | Driven by | Verified |
|---|---|---|
| `"span": "$q1"` inlines what query q1 found (grep hits, callers, ...) in the same batch | run 2: 8 read batches, most of them grep-then-read | `callers Server.login` plus `span $cb`: 18 caller bodies in one batch |
| Session dedupe in `mt query`: a body already inlined this session (same span and hash) comes back as a pointer marked unchanged; `"fresh": true` repeats it | run 2: every batch ~27 KB, with bodies sent again | repeating a batch: 20.8 KB → 11.4 KB, 14 items marked |
| Read-batch counter: from the 3rd read batch on, the reply says "put every remaining question … into ONE batch" | 8 read batches | it fired in run 3 (#7, #9, #10) |
| A span with any line over 1000 characters (generated or minified) is never inlined | `tsconfig.tsbuildinfo` ranked first | checked; that file is only 764 characters, so not caught (open: don't index build artifacts) |
| Plain-text `@@` bundle for `mt apply`: `create`, `replace`, `insert_after`, `delete`, `find`/`with`, `rename`, `run [gate] name: cmd`, `check`, `parallel`, `no-format`; `\@@` escapes a body line | run 2: 6 calls of heredoc writes plus a gofmt before the apply | scratch repo: 2 creates, a replace and a find/with applied; malformed bundles refused with nothing written |
| `apply` formats only the files it wrote (gofmt, rustfmt; `"format": false` or `@@ no-format` turns it off) | run 2 #23: `gofmt -w` over whole directories rewrote 12+ unrelated files, and 2 calls went to undoing it | `y.go` reformatted, `untouched.go` left as it was |
| Protocol text in the skill, the mt ON rule and the UserPromptSubmit hook: plan everything, then ONE read batch and ONE `@@` write batch; a third call only to fix what the steps report | | |

**Not verified:** `cargo test` doesn't compile at the moment. The failure is in another session's
in-progress `src/tools.rs` tests (`facts(&d)` has the wrong arguments), not in these files. The
release binary builds and every check above ran on it.

### Run 3 setup

- **mt ON only.** mt OFF doesn't change between runs, so it is no longer rerun (the user's call).
  The baseline is the mean of OFF runs 1 and 2. A third OFF run was stopped 3 minutes in and its
  numbers are excluded.
- Same prompt. The no-database note came 14 s in (90 s in run 2).
- The worktree was reset to `bd5fa80`, keeping only the agent-config deletions.
- The new build was in the plugin; `MT_INJECT_SKILL=0`; the updated rule.

### Results

| | ON run 3 | OFF baseline (mean of runs 1, 2) | ON 3 vs OFF | ON run 2 | ON run 1 |
|---|---|---|---|---|---|
| Turns | **13** | 53 (58, 48) | **-75%** | 27 | 40 |
| Tool calls | **13** | 57.5 (59, 56) | **-77%** | 27 | 43 |
| Active time | **612 s** | 777 s (757, 796) | **-21%** | 694 s | 838 s |
| Cost | **$2.828** | $3.81 ($3.68, $3.94) | **-26%** | $3.431 | $4.006 |
| Cache read | 1.13M ($0.23) | 6.85M | -84% | 3.58M | 5.65M |
| Cache write | 155k ($1.24) | 150k | +3% | 174k | 186k |
| Output tokens | 67.9k ($1.36) | 62.0k | +9% | 66.2k | 69.3k |
| Peak context | 185k | 180k | | 203k | 216k |
| Bash calls that run mt | 6 (5 `query` + 1 `apply`) | | | 12 | 18 |
| Query batch items | 45 | | | 64 | 43 |
| Hook denials | 0 | | | 2 | 4 |

The two OFF runs differ by 10% on turns and 7% on cost, so the cost gain (-26%) is beyond that
noise, and the turn gain (-75%) far beyond it.

**Outcome** (checked by me, without the DB tests):
- `go build`, `go vet`, `go test ./...` and `tsc --noEmit` pass; 11 DB tests skip.
- 17 files, +1535 -63. Uses `coreos/go-oidc` (run 2's ON had hand-written the verifier).
- **The single `@@` bundle (#11) applied 23 edits to 15 files.** Build, vet and unit steps passed
  on the first try, with no follow-up fix batch.

### The calls, in order

| # | What | Why it was needed |
|---|---|---|
| 1-4 | `git ls-files` ×2, grep `go.sum`, `ls` the module cache | orientation and dependency check, no mt equivalent (open: a `module` op and a repo-map op) |
| 5 | read batch 1: 15 queries, 6 of them whole-file spans (`auth.go` 327 lines, `router.go`, `Login.tsx`, `fake_test.go`, `routes_test.go`, migration 0004) | the plan, laid out up front as asked |
| 6, 7 | read batches 2, 3: **`auth.go:L1-327` asked for again** | the 6 spans total ~15k tokens, but one reply holds ~5k of code; `auth.go` lost its place each time and arrived only in #7 |
| 8 | `go get go-oidc oauth2` | dependency |
| 9, 10 | read batches 4, 5: `Principal`, `Cookie`, config, `app.go`, OpenAPI via `span $oas`, `testdb`, `org` table | facts the plan turned out to need |
| 11 | **one `@@` bundle: every file, edit and test, plus run steps** | the write batch |
| 12, 13 | `go test ./...` again, `git diff go.mod` | the model checking its own result; the apply's steps had already run the unit tests |

**Where the rest of the round trips come from**
1. **The reply size limit, not the protocol.** "Read everything in one batch" can't hold 6 whole
   files: a tool result is capped at ~28 KB, about 7k tokens. A plan needing ~15k tokens of code
   takes at least 3 replies. Here it took 3 batches for those spans, plus 2 for new facts.
2. **The span cap made it worse.** Requested spans shared a cap in rank order, so the same file
   lost its place twice.
   - **Fixed after the run:** requested spans are inlined first, smallest first, with the cap at
     85% of a reply.
   - **Also fixed:** a span that doesn't fit is named, with its size and whether it fits in a batch
     of its own ("[7] internal/api/auth.go:L1-327 ~2983 tokens (fits in a batch of its own)").
   - The replay of #5 on the fixed binary still can't fit all 6, but it says exactly what to do.
3. **Output tokens are now the biggest cost** ($1.36 of $2.83): the model writing 1,535 lines.
   - That is the work itself.
   - What isn't: the apply reply echoed changed regions (28,124 B), including whole files the model
     had just written. Open: show created files as pointers with hashes, not bodies.
4. **Cache writes** ($1.24) come mostly from the bundle turn: context went 118k → 181k on that one
   call, because the bundle text itself enters the conversation.

### Fixes after run 3

Built and verified by replay. They are not yet in the plugin build, and not yet in a run.

| Fix | Replay of run 3's #5 |
|---|---|
| Requested spans first, smallest first; cap at 85% of a reply | 5 spans inlined (was 4) |
| The note names each span left out, with its size and whether it fits alone | `## not inlined: … [6] web/src/screens/Login.tsx:L1-316 ~3553 tokens (fits in a batch of its own); [7] internal/api/auth.go:L1-327 ~2983 tokens (fits in a batch of its own). A whole file is rarely needed: a skeleton plus the spans you will edit is cheaper` |

### Transactions, run 3

Full log: `bench/transactions/g1-run3-on.md`.

<details><summary>#5: read batch 1 (15 queries, 6 whole-file spans), with the cap note</summary>

**#5 · turn 5 · Bash · 0.3 s · reply 23,473 B (~5,868 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
[
 {"id":"aa","span":"internal/api/auth.go:L1-327"},
 {"id":"rt","span":"internal/api/router.go:L1-233"},
 {"id":"sa","skeleton":"internal/store/auth.go"},
 {"id":"sso","grep":"sso_subject|SSOSubject|sso"},
 {"id":"cfg","grep":"type Auth|Auth +Auth|RequireTOTP|PublicURL|BaseURL|AllowedOrigins","in":"internal/config/"},
 {"id":"cfgs","span":"internal/config/config.go:/type Auth/"},
 {"id":"st","grep":"type Store interface|AuthLogin|CreateSession|LookupUserByEmail|IssueSession","in":"internal/"},
 {"id":"fk","span":"internal/api/fake_test.go:L1-120"},
 {"id":"rts","span":"internal/api/routes_test.go:L1-120"},
 {"id":"srv","grep":"type Server struct|func New\\(|type Deps","regex":true,"in":"internal/api/"},
 {"id":"mig","grep":"CREATE TABLE app_user","in":"internal/storage/migrations/"},
 {"id":"migs","span":"internal/storage/migrations/0001_init.up.sql:/CREATE TABLE app_user/"},
 {"id":"m4","span":"internal/storage/migrations/0004_authn_email_lookup.up.sql:L1-80"},
 {"id":"login","span":"web/src/screens/Login.tsx:L1-316"},
 {"id":"cli","grep":"login|/v1/auth","in":"web/src/api/client.ts"}
]
EOF
~~~~

Reply:
~~~~
# mt query: 15 queries: 0 ✓ 0 ✗ 14 answered 1 can't check; 24 items (4 inlined, ~3934 tokens of bodies), reply ~5876 tokens, 40 ms (index: 289 files, 0 re-parsed, 13 ms)
## 3 requested span(s) are too large to inline together and are shown as pointers: ask for them in a separate batch, or for narrower line ranges
aa = span internal/api/auth.go:L1-327   [2 ms; 1 span(s)]
    items: [7]
rt = span internal/api/router.go:L1-233   [2 ms; 1 span(s)]
    items: [5]
sa = skeleton internal/store/auth.go   [5 ms; 50 definitions, 602 lines]
    type machineCache L24-40
    type machineCacheEntry L42-45
    function newMachineCache L47-52
    method machineCache.get L54-69
    method machineCache.put L71-78
… (469 more lines)
~~~~

</details>

<details><summary>#6, #7: auth.go:L1-327 requested again; the read-batch counter fires in #7</summary>

**#6 · turn 6 · Bash · 0.3 s · reply 17,915 B (~4,478 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
{"queries":[
 {"id":"aa","span":"internal/api/auth.go:L1-327"},
 {"id":"rt","span":"internal/api/router.go:L1-233"},
… (8 more lines)
~~~~

Reply:
~~~~
# mt query: 8 queries: 0 ✓ 0 ✗ 8 answered 0 can't check; 13 items (4 inlined, ~4123 tokens of bodies), reply ~4540 tokens, 5 ms (index: 289 files, 0 re-parsed, 1 ms)
## 2 requested span(s) are too large to inline together and are shown as pointers: ask for them in a separate batch, or for narrower line ranges
aa = span internal/api/auth.go:L1-327   [0 ms; 1 span(s)]
    items: [6]
rt = span internal/api/router.go:L1-233   [0 ms; 1 span(s)]
    items: [5]
… (409 more lines)
~~~~

**#7 · turn 7 · Bash · 0.3 s · reply 17,822 B (~4,455 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
{"queries":[
 {"id":"aa","span":"internal/api/auth.go:L1-327"},
 {"id":"opt","grep":"type Options struct|RequireTOTP|SessionTTL|Peppers ","regex":true,"in":"internal/store/store.go"},
… (5 more lines)
~~~~

Reply:
~~~~
# mt query: 5 queries: 0 ✓ 0 ✗ 5 answered 0 can't check; 8 items (2 inlined, ~3627 tokens of bodies), reply ~4428 tokens, 11 ms (index: 289 files, 0 re-parsed, 10 ms)
aa = span internal/api/auth.go:L1-327   [0 ms; 1 span(s)]
    items: [2]
opt = grep type Options struct|RequireTOTP|SessionTTL|Peppers    [0 ms; text scan of 1 indexed files: 6 lines in 2 spans]
    items: [1] [5]
opts = span internal/store/store.go:/type Options struct/   [0 ms; 1 span(s)]
… (437 more lines)
~~~~

</details>

<details><summary>#11: the one @@ bundle, every file, edit, test and run step; the reply</summary>

**#11 · turn 10 · Bash · 17.9 s · reply 28,124 B (~7,031 tok)**

Claude sent:
~~~~
env -u RELAY_TEST_DB mt apply - <<'EOF'
@@ create internal/auth/sso/google/google.go
// Package google implements "Sign in with Google" as an OpenID Connect relying
// party (10 §10.4.5): the authorization-code flow with PKCE S256, state and
// nonce carried in a short-lived MAC'd cookie, and the ID token verified against
// Google's JWKS by go-oidc.
//
// It is configured per deployment (RELAY_AUTH_GOOGLE_CLIENT_ID / _SECRET), not
// per org. A Google account is matched to an EXISTING app_user by verified
// email; nothing in this package provisions users.
package google

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/coreos/go-oidc/v3/oidc"
	"golang.org/x/oauth2"
… (1611 more lines)
~~~~

Reply:
~~~~
applied 23 edits to 15 files in 11831 ms (validate 14 ms, baselines 0 ms, write 9 ms, reindex 57 ms, steps 11607 ms). undo: mt undo
format: gofmt 13 file(s), 5 reformatted (141 ms)
steps (non-gate steps in parallel):
   1 PASS  build      go build ./...  (6949 ms, gate)
   2 PASS  vet        go vet ./internal/... ./cmd/...  (1721 ms)
   3 PASS  unit       env -u RELAY_TEST_DB go test ./internal/auth/... ./internal/api/ ./internal/config/ ./internal/store/ ./internal/app/ -count=1  (4657 ms)
   4 ENV   web        cd web && if [ -d node_modules ]; then npx tsc --noEmit -p .; else echo "command not found: web/node_modules missing"; exit 127; fi  (25 ms)  could not run here (a tool or dependency is missing); not a failure of this change. full log: .mt/txn/1793916869427/step-4.log
       | command not found: web/node_modules missing
result: every step that could run passed; 1 step(s) could not run here (ENV): fix the environment and rerun them
changed regions (code as it now is on disk, + marks changed lines; spans and hashes are ready for the next batch):
[api/openapi.yaml:L2411-2443 key get h=891d1d22]
2411      get:
2412        operationId: startSso
2413        tags: [Auth]
… (603 more lines)
~~~~

</details>


---

## 2026-11-05 · 19. `mt tools`: one graph for code and tooling, replacing the always-on listings

**Why.** Across 763 past sessions (90,047 turns), searching for tools cost little: 573 ToolSearch
calls, about 3.4 s each, about 40 minutes in total. The always-on listings cost far more. The
median first request was 95k tokens before the user typed anything, and about 32k of that was
listings re-read on every turn:
- agent descriptions: 7.8k
- skill list: 7.5k
- deferred-tool names: 7.4k
- SessionStart hooks: 6k
- MCP instructions: 3.9k

Most of it had nothing to do with the repo at hand. PostHog has 161 skills (19% of the list),
Vercel is listed twice (17%), and 62 game/Unity skills were loaded in Go and web repos. Scripts:
`bench/tools/toolcost.py`, `bench/tools/roundtrip.py`.

**Goal.** mt owns that part of the context. Claude gets the code it needs and the skills, agents
and MCP servers the task needs, in as few round trips as possible.

### What Claude Code lets an outside tool change (measured, Claude Code 2.1.318)

One headless `claude -p` session per variant, haiku, "reply ok". The request each one sent was
read back from its transcript with `bench/tools/probe/shape.py`.

| Lever | Effect on the request | Measured |
|---|---|---|
| `enabledPlugins: {x: false}` | removes the plugin's skills, agents, commands, MCP servers and hooks | 57,560 → 48,290 first-turn tokens (5 plugins) |
| `--strict-mcp-config` | removes MCP tools and their instructions | → 38,350 |
| `deniedMcpServers: [{serverName}]` | removes the server's tools **and** its instructions | meshy and blender gone, 46,825 |
| `permissions.deny: ["Tool"]` | removes a built-in tool's schema or deferred name | 38.4k → 25.9k schema chars (Workflow, ScheduleWakeup, ReportFindings) |
| `permissions.deny: ["Agent(x)"]` | removes the agent from the listing | cto-orchestrator gone |
| `permissions.deny: ["mcp__x"]` | removes the tools, **keeps** the instructions | meshy instructions still sent |
| `skillOverrides: {x: "off" \| "name-only"}` | works for user skills | rpg gone; deja-history name only |
| `skillOverrides` on a plugin skill | **no effect** | superpowers:writing-skills still listed |

The skill listing has a fixed budget of about 30,000 characters (`skillListingBudgetFraction`).
With 399 skills, only 54 kept their descriptions. Turning off 233 skills let the survivors'
descriptions grow back, so the listing stayed at 30k characters. Removing skills changes *which*
descriptions Claude sees, not how big the listing is, until everything fits.

### The design

`mt tools` builds a graph (`~/.cache/mt/tools.sqlite`) from definitions:
- plugin manifests;
- `SKILL.md` files, commands (Claude Code lists them as skills) and agent files;
- MCP config, plus each stdio server's own `tools/list`. mt starts the server, sends
  `initialize` and `tools/list` (paged), caches the result per server config, and re-asks only
  when that config changes.

Nodes are plugins, skills, agents, MCP servers and MCP tools. Edges:
- `provides`: plugin → its items;
- `exposes`: server → its tools;
- `mentions`: a skill or agent body naming a server's tools.

**Project facts come from the code.** `facts(root)` walks the repo and records each fact with the
file that shows it:
- dependency manifests: package.json, composer.json, go.mod, Cargo.toml, pyproject, requirements,
  Gemfile, Podfile, csproj, gradle;
- marker files and directories near the root (vercel.json, schema.prisma, .github);
- file extensions;
- identifiers referenced in non-doc files, from mt's own code index.

530 files take 33 ms.

**Anchors.** Each item is tied to the products it is about:
- a plugin item → its plugin's product, when at least half the plugin's items name that product
  in their description (posthog, vercel, stripe; superpowers is not a product);
- an MCP server → its own name;
- a standalone skill or agent → the specific words in its name (`prisma-query-optimizer` →
  `prisma`).

An item is *grounded* when a fact matches an anchor. Plain name words like `level-design` or
`save-systems` need a dependency, file or extension. Identifier references only count for product
names, and only from 3 references up. No anchor means general-purpose.

**Code joins the same graph.** `code_cards()` reads `.mt/index.sqlite`.

*Seeds* are definitions scored by BM25 over their name words, file stem and kind:
- scaled by role (source above migration above test above doc) and kind (variables, keys and CSS
  rules down);
- an exact-name bonus (`session`, `audit_log`);
- at least two prompt words, or the exact name.

*Graph rerank* (the GraphRAG step): the top 40 seeds are connected through the code index:
- one seed's body references another seed (a shared name alone doesn't count);
- both touch the same table;
- a test, integration or migration file references several seeds;
- the same file (weak).

Then `ppr()` runs personalized PageRank from the lexical scores, and the final score is half
lexical, half graph.

*Each card* gives the definition's file:line plus:
- tests and integration tests;
- callers;
- tables used, with the migration or model that defines each one;
- docs;
- the plugin or MCP server the file's identifiers point to;
- one pointer, `mt context <name>`, for all the bodies.

A **table** card instead lists the code that reads or writes it (named by enclosing function), the
tests of that code, other migrations that touch it, and the tables it references.

**Commands.**
- `mt tools index [--reprobe]` builds the catalog and probes MCP servers.
- `mt tools facts` prints every code fact with its evidence.
- `mt tools find <task>` prints code cards, then tool cards with how to reach each item here.
- `mt tools show <name>` prints an item's neighbours and, for a skill or agent, its full body. An
  item that is off stays usable this way.
- `mt tools profile [--write|--out F|--undo] [--keep a,b] [--include-hooked]` writes the settings
  layer. Each decision is printed with its evidence.
  - Unused vendor plugins are turned off.
  - Plugins with hooks stay on unless `--include-hooked` is passed.
  - Ungrounded user skills get `skillOverrides: off`, agents `deny Agent(x)`, user MCP servers
    `deniedMcpServers`, plugin MCP servers `deny mcp__plugin_…`.
  - Built-in tools are left alone: nothing in the code argues for or against them.
- `mt tools enable <name>` takes one item back out of `.claude/settings.local.json`.
- `mt tools hook [--settings F]` is the UserPromptSubmit hook. It adds code and tool cards for the
  prompt, and every decision is logged to `/tmp/mt-tools-hook.log`.
- `mt tools stats` counts general-purpose, grounded and ungrounded items per kind.

**A dead end, corrected by the user.** The first version harvested tool schemas, MCP
instructions and per-project usage from 7.2 GB of transcripts: 5.8 GB scanned in 1.2 s, 2,768
nodes. The user asked why the graph wasn't based on the code. Usage is history, not the project:
a new repo has none, and old habits keep things alive that the code no longer needs. Transcripts
are no longer read at all. The only cost: HTTP MCP servers (posthog, stripe, vercel, sentry,
graphify, lift) have no tool list until the session connects with its own auth, so their cards
use a keyword `ToolSearch "+server words"`.

### Ranking iterations, each from a real miss

Repo copy of Relay (`claude_coach`, 319 files, Go and TypeScript).

| Miss | Cause | Fix |
|---|---|---|
| "make a 3d model of a chair" found Lift, not meshy | the tokenizer dropped words under 3 chars ("3d", "ai") | 2-letter tokens kept; 2-letter stopwords added |
| WebSearch pulled in Chrome tools | absolute co-load threshold | relative threshold (later removed with the transcripts) |
| `vercel:deploy` had no description | it is a command (`commands/deploy.md`) | commands indexed as skills |
| stripe kept on "4 identifier references" | references in Markdown counted | refs from non-doc files only |
| `level-design`, `save-systems` grounded | plain English words in identifiers | name words need strong facts |
| code cards for `live`, `usage`, `Role` | one-word hits on short variables | two prompt words or exact name; variables down-weighted |
| a table named `the` | the SQL use-regex matched English "from the" | stopword tables dropped |
| posthog skills for "sessions table for token usage" | many skills, many common words | vendor item for an ungrounded product only when the prompt names it |
| `enrollment_token` above `session` for "the sessions table" | "token" is rarer than "session" | exact-name bonus, definition kind as a word |
| CSS `table.rows` and test const `rows` boosted by PPR | two seeds sharing a name were linked | no edge for a shared name; CSS rules and keys down-weighted |
| PPR put the `session` hub table above `PublishSessionEvents` | pure graph score rewards hubs | half lexical, half graph |
| the `follow` table for "following the repo's convention" | instruction verb | stop list |

T1 prompt ("which migration creates the audit log table, which store function inserts into it,
which test covers it"):

| Ranking | Top code cards |
|---|---|
| lexical only | `AuditLog` (a TSX screen), `RoleOrgAdmin`, `Server.auditWrite` |
| + exact-name bonus | `audit_log` table, … |
| + PPR (blended) | `audit_log` table, `Store.WriteAudit`, `AuditEntry` |

The `audit_log` card answers T1 in zero calls:
- `0001_init.up.sql:575`;
- `Store.WriteAudit internal/store/audit.go:52`;
- `internal/api/routes_test.go:289`.

### Profile for the Relay repo copy

`mt tools profile --include-hooked --keep relay` (full log in `bench/tools/profile.log`):
- **Plugins off (6):** aicost, arcads, claude-seo (43 items and its PostToolUse hook), posthog (166
  items, PreToolUse and SessionEnd hooks), stripe, swift-lsp.
- **Plugins kept:**
  - grounded: vercel and vercel-plugin (`vercel.json`), typescript-lsp
    (`web/package.json: typescript`), ponytail (3 identifier references);
  - general-purpose: superpowers and others;
  - relay, by `--keep`.
- **75 user skills off; 4 agents off:** cto-orchestrator, cypress-qa-engineer,
  prisma-query-optimizer, ciso-devsecops-reviewer.
- **11 MCP servers denied:** blender, meshy, mixar, gsc, headroom, lift, sentry, graphify, graft,
  posthog, node_repl.

MCP probes, cold: blender 9 tools in 1.1 s, meshy 24 in 0.8 s, mixar 76 in 1.0 s, graft 6, headroom
3, posthog 1, gsc 8 in 14.1 s. node_repl failed (binary missing). HTTP servers aren't probed.
Cold index 14.2 s, warm 25 ms.

### Tests

11 unit tests in `src/tools.rs`; the full suite is 33/33.

| Test | Checks |
|---|---|
| `frontmatter_folded_description` | YAML `>` and indented descriptions |
| `tokens_stem_and_drop_stopwords` | stemming, 2-letter tokens ("3d", "ai") |
| `split_words_camel_and_separators` | `posthog-js`, `PostHogClient` |
| `mcp_prefix_normalises` | `claude.ai Claude Docs` → `mcp__claude_ai_Claude_Docs__` |
| `anchors_come_from_products_and_specific_name_words` | product plugin, workflow plugin, standalone agent, MCP tool |
| `profile_follows_the_code` | grounded plugin stays, general plugin stays, hooked plugin stays unless `--include-hooked`, `--keep` wins, identifier refs don't ground a standalone skill but do ground a product |
| `find_groups_server_tools_and_prefers_code_grounded` | one card per server with one `select:` line; grounded skill first |
| `off_items_point_to_mt_show_and_enable` | off skill → `mt tools show`, denied server → `mt tools enable` |
| `facts_read_manifests_and_markers` | package.json, nested go.mod, vercel.json |
| `code_cards_link_tests_migrations_docs_and_tech` | real temp repo indexed by mt: Go function → its test, its table's migration, its doc, the stripe plugin |
| `ppr_lifts_connected_seeds_over_an_isolated_one` | PPR ordering and mass conservation |

### A/B in real sessions

**Setup.**
- Opus 5.5 headless (`claude -p`, `--permission-mode bypassPermissions`), the user's real global
  setup.
- One fresh copy of the repo per run, with graft's repo wiring removed (graft/, .mcp.json,
  .cursor, .codex, AGENTS.md, opencode.json, GEMINI.md), a stub `graft` first on PATH, and the
  graft MCP server denied in **both** arms.
- A: nothing else.
- B: `--settings B.settings.json` = the profile layer plus `mt tools hook` on UserPromptSubmit.
- 3 tasks, 2 runs each, 12 sessions in parallel.
- Harness: `bench/tools/run.sh`, `analyze.py`, `tasks.json`.
- Every step of every run: `bench/tools/analysis.txt`. Raw streams: `bench/tools/runs/`.

**Tasks.**
- T1: find the migration, the store function and the test for audit log entries (code and data).
- T2: rewrite a sentence using the humanizer skill. Humanizer is **off** in B, so B has to reach it
  through mt.
- T3: add a nullable `cost_usd` column to the session table. It turned out to be a bad task: the
  column already exists (`0001_init.up.sql:198`). Every run in both arms found that and changed
  nothing, which is the correct outcome and is scored as such.

| Run | Turns | Calls | mt | First req tok | Input tok, all reqs | Output | Wall s | Cost $ | Hook chars | Correct |
|---|---|---|---|---|---|---|---|---|---|---|
| A-T1 r1 / r2 | 6 / 5 | 5 / 4 | 0 | 51,349 / 56,427 | 375,299 / 314,585 | 1,956 / 1,674 | 48.2 / 41.2 | 1.309 / 1.499 | 0 | 3/3, 3/3 |
| B-T1 r1 / r2 | 5 / 5 | 4 / 4 | 0 | 46,058 / 46,059 | 239,389 / 242,635 | 2,151 / 1,760 | 44.4 / 39.4 | 0.937 / 0.937 | 2,249 | 3/3, 3/3 |
| A-T2 r1 / r2 | 3 / 3 | 1 / 1 | 0 | 54,229 / 56,445 | 125,146 / 127,373 | 100 / 117 | 18.8 / 18.5 | 1.437 / 1.439 | 0 | 2/2, 2/2 |
| B-T2 r1 / r2 | 2 / 2 | 1 / 1 | 1 / 1 | 45,531 / 45,531 | 95,195 / 95,230 | 235 / 260 | 18.2 / 17.5 | 0.790 / 0.792 | 950 | 2/2, 2/2 |
| A-T3 r1 / r2 | 5 / 6 | 4 / 5 | 0 | 51,370 / 56,404 | 320,874 / 400,559 | 2,983 / 4,305 | 60.1 / 77.5 | 1.421 / 1.890 | 0 | 3/3, 3/3 |
| B-T3 r1 / r2 | 4 / 4 | 3 / 3 | 0 | 46,331 / 46,332 | 197,466 / 197,647 | 2,371 / 2,208 | 46.8 / 44.2 | 0.987 / 0.978 | 2,737 | 3/3, 3/3 |

| Mean (n=6 per arm) | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A, real setup | 4.7 | 3.3 | 54,371 | 277,306 | 1,856 | 44.1 | 1.499 | 16/16 |
| B, mt tools | 3.7 (-21%) | 2.7 | 45,974 (-15%) | 177,927 (-36%) | 1,498 | 35.1 (-20%) | 0.903 (-40%) | 16/16 |

**A caveat on cost.** 4 of 6 A runs started with a cold prompt cache: the parallel sessions raced
for the shared 11.5k prefix. No B run did. That inflates A's dollars. Comparing warm runs only:
- T1: $1.309 → $0.937 (-28%);
- T3: $1.421 → $0.98 (-31%).

Token counts are unaffected: B wrote 39–41k cache tokens per run, A 56–80k.

**Reading the results.**
- **The layer works.** First request 54.4k → 46.0k: 8.4k tokens off every request of every turn.
  Headless mode starts with fewer built-ins than the interactive app (57k against 95k), so the
  interactive saving should be larger. Not measured yet.
- **Turned-off skills stay reachable, more cheaply.** In B-T2 the hook card said
  `[off: skillOverrides] use: mt tools show humanizer`. Claude ran exactly that and answered one
  turn sooner. In A, the Skill tool injected the humanizer skill as a 14–17k-token cache write;
  `mt tools show` returned 10.9k chars (about 3k tokens) and the run finished in 2 turns instead
  of 3.
- **Code cards were right but not trusted.** In T1, B's card held all three answers, yet Claude
  still ran 4 verifying greps ("Verified against the working tree"), only one fewer than A. The
  calls were smaller (input -30%), but the pointers did not replace verification, and
  `mt context` was never called.
- **T3 was faster in B** (4.0 turns against 5.5, -34% wall). The session card pointed straight at
  the migration and at the store code that reads the table.

### Open problems

- **Hook latency is 1.3–1.6 s per prompt.** It walks the repo for facts and loads the graph each
  time. It belongs in `mt serve`, which already holds the code graph in memory.
- **Claude re-verifies code pointers.** The next experiment: the card carries the exact lines
  (spans and hashes, like `mt query`), or the hook calls `mt context` itself for the top card.
  Then there is nothing left to verify.
- **Noisy tool cards on prose prompts:** Vercel routing-middleware for a marketing sentence.
  Cards should need a minimum absolute score and two matched words outside the description.
- **Plugin skills can't be turned off one by one** (`skillOverrides` ignores them), only whole
  plugins. A grounded plugin keeps all 30 of its skills.
- **`profile --write` and `enable` haven't been run in a live session.** The eval used
  `--settings`. Whether a settings change takes effect without a restart (plugins, MCP) is still
  unmeasured.
- **Things outside mt's reach:** claude.ai connectors and the Chrome extension aren't in local
  config, and bundled skills (`code-review`, `simplify`) aren't on disk.
- **Next runs:** an unambiguous change task (T3 replaced), 3 runs per arm without parallel cache
  races, and the interactive app's 95k baseline.

---

## 2026-11-05 · 20. `mt expand`: pointers you can open by number

**Why.** Replies keep bodies to a budget and turn the rest into pointers (`[n] path:Ls-e kind name
h=hash`). To read one, a model had to write a new `span` query or fall back to `sed -n`. Now the
pointer is a handle.

**What was built.**
- `mt expand 3 5 8`: numbers from the last `mt query` / `mt context` reply of this session. Also
  `mt expand path:L42-55@06d9be72`, or a pasted pointer line (its number and span name one item;
  kind/name words are skipped). Runs in-process: reading the named lines takes about a millisecond.
- Hash-checked. If the span changed, the definition is found again by name (nearest to where it
  was) and comes back marked `CHANGED since the pointer (was L7-9 h=…)`, so nobody edits from
  stale lines. Output is capped like every reply; anything over the cap is listed as
  `## not expanded: mt expand …`. Expanded bodies go into the session's seen log, so later
  queries point at them instead of repeating them.
- Every reply that has pointers ends with `## N pointer(s) without bodies: [..]. … mt expand <n> <n> ...`.
  `MT_NO_EXPAND=1` turns the footer off (the A/B control).
- The pointer table: `$TMPDIR/mt-query-seen-<session>.ptrs`, one row per numbered item. Each reply
  with items replaces it; a reply with none keeps it.
- Hook: `sed -n A,Bp` over a range that a pointer covers is denied with `mt expand N`. Items
  nested inside another hit are dropped. A `Read` of such a file gets a nudge.
- Skill: one paragraph under "Keep the default budget".

**Bugs found while building, fixed.**
- The daemon and the client used different session ids. The client used MT_SESSION; the plugin's
  daemon has no MT_SESSION, so it used a hash of MT_SOCKET. So the seen-log dedup and the read-batch
  count in daemon replies were never the client's. `Req.session` now carries the client's id.
- Without the plugin there was no session at all, so numbers couldn't work. `session_id()` now
  falls back to `CLAUDE_CODE_SESSION_ID`, which Claude Code exports to every Bash call (the same
  id the plugin puts in MT_SESSION). Side effect: skill-only sessions now get the seen-log dedup
  and the read-batch nudge too.
- The analyzers counted a command as mt only when it *started* with `mt`, so `ls … && mt query`
  and `mt expand 6 7 && mt query` were miscounted (section 14 hit the same thing). `ab/t4.py` now
  counts every `mt` stage.

Test: `query::tests::expand_pointers` covers numbers, a pasted line, a handle with a hash, a file
changed underneath (relocated by name), and an unknown number. `cargo test`: 32 passed.

**A/B.** T4 with the prompt made unambiguous ("every construction site counts, including the
WebSocket subscribe audit … and the login audit"), raw setup, Opus 5.5, 3 runs each, run in 3
rounds of A/O/X in parallel. A = no mt. O = mt required, claims first, old skill, footer off.
X = the same with `mt expand` (footer and skill line). Same binary for O and X. Harness and
transcripts: session scratchpad `ab/` (run.sh, t4.py, r13/, r13-report.txt).

| Run | Turns | Calls | Wall s | Cost $ | Output | Cache read | mt stages | Checks | Tests | Sites | Expanded / offered |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A-r1 | 11 | 10 | 71.4 | 0.374 | 4,919 | 308,801 | | 8/8 | ok | 5/5 | |
| A-r2 | 10 | 9 | 90.1 | 0.433 | 6,484 | 294,110 | | 8/8 | ok | 5/5 | |
| A-r3 | 15 | 14 | 119.8 | 0.524 | 6,862 | 503,323 | | 8/8 | ok | 5/5 | |
| O-r1 | 7 | 5 | 77.8 | 0.536 | 6,074 | 178,233 | 4 | 8/8 | ok | 5/5 | |
| O-r2 | 7 | 5 | 81.0 | 0.519 | 6,067 | 172,644 | 4 | 8/8 | ok | 5/5 | |
| O-r3 | 8 | 6 | 104.3 | 0.656 | 9,494 | 235,009 | 5 | 8/8 | ok | 5/5 | |
| X-r1 | 8 | 6 | 104.7 | 0.689 | 9,265 | 256,002 | 5 | 8/8 | ok | 5/5 | 0 / 105 |
| X-r2 | 7 | 5 | 80.9 | 0.507 | 5,565 | 171,777 | 4 | 8/8 | ok | 5/5 | 7 / 48 |
| X-r3 | 7 | 5 | 63.2 | 0.504 | 5,126 | 179,334 | 4 | 8/8 | ok | 5/5 | 2 / 38 |

Means: A 12.0 turns, $0.444, cache read 369k. O 7.3 turns, $0.570, 195k. X 7.3 turns, $0.567, 202k.

**What happened.**
- With the prompt unambiguous, every run in every arm set RequestID at all 5 sites. Section 14's
  3-of-5 gap was the spec's wording, as suspected.
- Two of the three X runs used expand, and sparingly: 9 bodies from 191 pointers offered (5%).
  No over-expansion. Each expand was chained into the same Bash call as the next query
  (`mt expand 6 7 8 9 && mt query …`), so it never cost a turn. X-r1 never used it and asked for
  spans by path instead.
- No CHANGED came back: nobody expanded after editing. The relocate path is covered only by the
  unit test.
- O vs X: no measurable difference at n=3 (the same turns; cost $0.570 vs $0.567). At this
  context size, a span query and an expand cost about the same.
- mt vs no mt: −39% turns and −46% cache read, but +28% cost. Cache writes are higher (about 49k vs
  31k), because claims-first batches put big replies into context early. Same pattern as before:
  at 19k context the turn savings don't pay for the larger reply.

**Next.** The 255k-context rerun, where a turn costs enough to make fewer turns pay. An
edit-then-expand task to exercise CHANGED for real. And the earlier idea: fold a body down to the
lines that touch the query, so fewer items need expanding at all.

---

## 2026-11-05 · 21. Hooks in Rust (`mt hook`), rewrite instead of deny, no Python in the product

**Rule from here on:** everything that ships with mt is Rust. Python is only for throwaway work
(benches, transcript analysis). The goal is speed: fewer model turns, less wall time, more work
per mt call.

**What changed.**
- `plugin/hooks/mt_hooks.py` (308 lines) is gone. `mt hook <session-start|prompt|pretool|session-end>`
  (`src/hook.rs`) does the same work in-process. Definitions come from the in-process parser
  instead of a `mt skeleton` subprocess, the repo root from `index::find_root` instead of
  `git rev-parse`, and the pointer table path from `query::session_file`, so it isn't written
  twice in two languages anymore. session-end stops the server over its socket
  (`serve::request_at`), with SIGTERM to the pid as the fallback.
- `plugin/bin/mt` was a `/bin/sh` launcher that cost about 55 ms per call (sh start-up alone is
  ~19 ms on macOS, plus `$(dirname)`/`pwd` forks). That hit every hook *and* every `mt` command
  the model ran. It's now a symlink to `target-plugin/release/mt`. Nothing used its `MT_BIN`
  override.
- **Rewrite instead of deny.** A deny makes the model spend a turn rereading the suggestion and
  running it. PreToolUse can return `updatedInput`, so the hook now rewrites a lone command (one
  stage, no pipe) in place:
  - `sed -n A,Bp file` → `mt expand N` (pointer from the last reply) or `mt span file Name`
  - `grep -A30 'func X' file` → `mt span file X`
  - `grep -rn Ident .` (one identifier) → `mt context Ident --edit` (every referencing line, ±2)

  Each rewrite is `out=$(mt …) && printf '%s\n' "$out" || <original command>`: if mt has no answer
  (an unknown name, a stale number), the model gets exactly what it asked for, without mt's
  message. Piped or compound commands, and several identifiers, are still denied with the mt
  command. `MT_REWRITE=0` brings back deny-only.
- `cat`/`head` of code files with ≤ 300 lines in total now pass through. A denial costs more than
  reading a small file. Bigger files are still denied with skeleton + spans.
- A bug that was already in the Python: `first_stage("a || b")` took the second `|` for a pipe.
  Harmless there (segments split on `||` first), fixed anyway.

**Measured.** 20 runs each, median:

| | pass-through (`go build`) | fires (`sed -n` on code) |
|---|---|---|
| Python hook | 61.1 ms | 123.5 ms |
| Rust `mt hook`, binary | 6.4 ms | 9.2 ms |
| Rust through the old sh launcher | — | 64.1 ms |
| Rust through the symlink | — | 11.3 ms |

Parity: the Python and Rust hooks gave identical decisions on 14 of 14 inputs (sed ranges with and
without pointers, piped, cat/head, grep -A, repo greps, a non-code command, Read with and without
pointers, the Grep tool), with rewrite off.

**End to end (headless Claude, Haiku, `--plugin-dir`).** session-start brought the session server
up in 18 ms. The model ran `sed -n 42,55p internal/api/audit_write.go` and got the
`mt span … auditWrite` block in the same call (2 turns in total). `grep -rn "auditWrite" .` came
back as `mt context auditWrite --edit`. `grep -rn "zzqNoSuchName" .` fell back to the plain grep.
session-end stopped the server over its socket.

`cargo test`: 36 passed (new: `hook::tests::shell_words_and_stages`, `hook::tests::suggestions`).

**Not measured yet:** wall time over a whole task with the plugin (the T4 E arm), now that the
hook is ~10x cheaper and rewrites save the denied turn.

---

## 2026-11-05 · 22. `mt tools` fixes from section 19, and a clean rerun

Each fix comes from an open problem in section 19, and each is checked by a test or a measured
session.

### 1. Hook latency: it was contention, not mt

Section 19 logged 1.3–1.6 s per hook call. With per-stage timings now in `/tmp/mt-tools-hook.log`
(`stages_ms`), six sequential calls on the Relay repo copy took 112–206 ms:

| Stage | ms |
|---|---|
| facts (walk the repo, read manifests, group refs) | 70–92 |
| graph load (sqlite, ~577 nodes) | 0–1 |
| code cards (seeds, PPR, card queries) | 9–21 |
| tool cards | 5–8 |

The 1.3–1.6 s came from 12 sessions starting at once, all running every plugin's SessionStart hook
together. Table lookups changed from `lower(t.name) = lower(?)`, which bypassed the `tables_name`
index, to `t.name IN (?, lower(?), upper(?))`. Moving the hook into `mt serve` isn't needed at
these numbers.

### 2. Claude re-checked the pointers: the top card now carries the lines

In section 19, Claude ran 4 verifying greps even though the T1 card held the answer. A pointer
isn't proof. The **first** code card now includes the lines themselves, read from disk at prompt
time, numbered, under the same `h=` span hash `mt span` prints:
- a definition: its body, up to 60 lines;
- a table: its DDL (up to 40 lines) plus the body of the first function that writes it
  (insert/write/create/update/save/record first), up to 50 lines.

Longer spans end with the exact `mt span <file> Lx-y` for the rest. The hook header tells Claude
these lines are current and needn't be read again. For T1 that is the `audit_log` DDL
(`0001_init.up.sql:575-587`) and `Store.WriteAudit` (`internal/store/audit.go:13-62`, then
`mt span internal/store/audit.go L63-72`).

### 3. Noisy cards on prose prompts

Section 19's T2 prompt (a marketing sentence) drew Vercel routing-middleware skills from the words
"edge", "guidance" and "platform". New rules:
- **Tool card:** the item must be named in the prompt, or match at least two prompt words, at least
  one of them in its own name. For an MCP server, its tools' names count as its name.
- **"Named in the prompt":** needs the full name, a multi-word name (`creating-surveys`) or a
  distinctive one (8+ characters, `humanizer`). The skill `posthog:flags` no longer counts as
  named by the word "flags".
- **Code card:** variables, constants, config keys and CSS rules appear only on an exact name
  match. A CSS rule `table.rows` had matched "inserts rows into the table".
- **Stemmer:** now strips 4-letter plurals (`tabs` → `tab`), so the Chrome server's
  `tabs_context` matches "tab".

After the fixes, T2's hook context is one card (humanizer); it was three.
New test: `prose_with_incidental_words_gets_no_tool_card`.

### 4. Plugin skills one by one: no lever exists

Measured: `permissions.deny: ["Skill(superpowers:writing-skills)", "Skill(posthog:creating-surveys)", "Skill(rpg)"]`
leaves all three in the listing. Together with section 19's result for `skillOverrides`, Claude
Code 2.1.318 has no per-skill control for plugin skills. A plugin is on or off as a whole.

What does work is the listing budget:

| `skillListingBudgetFraction` | listing chars |
|---|---|
| default | 30,001 |
| 0.005 | 18,015 |
| 0.002 | 18,015 (the names-only floor: every name stays, every description goes) |

`profile` now sets `skillListingBudgetFraction: 0.002` (opt out with `--keep-descriptions`). The
listing becomes names only, and the prompt hook supplies descriptions for the skills a prompt
needs. That is the design anyway: names for everything, details on demand.

### 5. `profile --write` and `enable` in live sessions

Headless haiku sessions in a repo copy, with nothing but the graft deny passed on the command line:

| Check | Result |
|---|---|
| `profile --write` merges into an existing `.claude/settings.local.json` | merged; the file's own `allow: ["Bash(ls:*)"]` kept |
| a new session loads it, before the listing budget | first request 44,650 tokens (headless baseline 57,560) |
| with the names-only listing | **37,914 tokens (-34%)**, skill listing 9,750 chars (was 30,001) |
| `Skill("humanizer")` while it's off | `Skill humanizer is disabled for model invocation in skillOverrides settings` |
| `mt tools enable humanizer`, then `Skill("humanizer")` **in the same session** | `Launching skill: humanizer`: no restart |
| `mt tools enable headroom` (MCP), then ToolSearch in the same session | not found: MCP servers connect at startup |
| a new session after that | headroom's 3 tools listed |
| `mt tools profile --undo` | file restored byte for byte (`diff` clean) |

So skills come back mid-session and MCP servers in the next session, which is what the cards
already say.

### 6. The interactive app (modelled from this session's own request)

The interactive session that built this started at 79,728 tokens. Applied item by item to that
request, the Relay profile would remove:
- deferred tool names: 179 → 58 (-3,751 chars);
- MCP instructions: 9,612 of 12,527 chars;
- agents: 44 → 20, -10,583 chars of agent descriptions;
- skill listing: 405 → 132 entries and, with the names-only budget, about 30k → under 10k chars.

That is about 40k characters, roughly 10k tokens, off every request. It's a model, not a run: the
interactive app can't be driven headless.

**Left:** 24.5k characters of SessionStart hook text from the global setup (graphify, graft,
Vercel, superpowers, Relay). It comes from the user's own hook configuration, which the profile
doesn't touch.

### 7. A real change task, and a clean rerun

Section 19's T3 asked for a column that already existed. The new T3 is a real change:
- add a nullable `archived_at timestamptz` column to `session` with a new up and down migration;
- add a nullable `ArchivedAt` (JSON `archived_at`) to the type the session-detail endpoint returns;
- read the column in the store query that loads one session.

It is scored on five checks: up migration, down migration, struct field with tag, `GetSession`
select and scan, and **`go build ./...` passing**, which the harness runs itself after each
session.

**Setup.** Same raw setup as section 19, with B using the new layer (names-only listing included)
and the fixed hook. Runs are **sequential**, A and B alternating per task and rep, with one
unscored warm-up per arm: all 18 first requests hit a warm cache, so there is no race this time.
3 runs per task per arm. Harness: `bench/tools/run2/seq.sh`, `run.sh`, `analyze.py`. Every step of
every run: `bench/tools/run2/analysis2.txt`.

| Run | Turns | Calls | mt | First req tok | Input tok, all reqs | Output | Wall s | Cost $ | Hook chars | Correct |
|---|---|---|---|---|---|---|---|---|---|---|
| A-T1 r1/r2/r3 | 5/6/6 | 4/5/5 | 0 | 58,226/58,222/58,222 | 319,728/375,442/370,931 | 1,918/2,182/2,060 | 37.8/41.6/39.3 | 1.245/1.127/1.184 | 0 | 3/3 ×3 |
| B-T1 r1/r2/r3 | 4/4/5 | 3/3/4 | 0 | 40,860 ×3 | 167,502/167,250/211,785 | 1,510/1,334/1,733 | 26.0/26.0/30.8 | 0.647/0.636/0.790 | 5,690 | 3/3 ×3 |
| A-T2 r1/r2/r3 | 3/3/3 | 1/1/1 | 0 | 58,240/58,240/58,485 | 129,387/129,390/128,818 | 101/99/114 | 9.6/9.4/10.5 | 1.063/1.063/1.145 | 0 | 2/2 ×3 |
| B-T2 r1/r2/r3 | 2/2/2 | 1/1/1 | 1/1/1 | 38,611 ×3 | 81,358/80,512/81,416 | 233/231/285 | 8.9/9.2/9.1 | 0.552/0.535/0.653 | 312 | 2/2 ×3 |
| A-T3 r1/r2/r3 | 5/7/7 | 4/6/6 | 0 | 58,669 ×3 | 329,794/480,246/457,952 | 2,274/2,860/3,299 | 39.6/48.3/92.9 | 1.339/1.430/1.436 | 0 | 5/5 ×3 |
| B-T3 r1/r2/r3 | 4/4/6 | 3/3/5 | 0 | 40,442/40,442/40,446 | 179,129/173,776/264,168 | 1,753/1,884/2,175 | 29.8/41.2/69.1 | 0.803/0.759/0.876 | 3,423 | 5/5 ×3 |

| Mean (n=9 per arm) | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A, real setup | 5.0 | 3.7 | 58,405 | 302,410 | 1,656 | 36.6 | 1.226 | 30/30 |
| B, mt tools | 3.7 (-26%) | 2.7 (-27%) | 39,971 (-32%) | 156,322 (-48%) | 1,238 (-25%) | 27.8 (-24%) | 0.695 (-43%) | 30/30 |

Per task, B against A:

| Task | Turns | Input tok | Wall | Cost |
|---|---|---|---|---|
| T1 (find code and data) | 5.7 → 4.3 | -49% | -30% | -42% |
| T2 (skill that is off in B) | 3.0 → 2.0 | -37% | -7% | -47% |
| T3 (migration plus Go change) | 6.3 → 4.7 | -51% | -23% | -42% |

**Reading the results.**
- **Correct in both arms, every time.** All 9 T3 runs in both arms produced migrations and code that
  build.
- **The first request fell 18.4k tokens** (58.4k → 40.0k). That is section 19's layer plus the
  names-only listing. Each saved token is saved again on every later request of the session,
  which is why total input fell 48%.
- **Inlined lines cut the verification work but didn't remove it.** On T1, B made 3 calls (was 4
  in section 19, 4.7 in A). The calls were confirmations with tiny results (116, 798 and 857
  chars) rather than searches. On T3, the hook's `session` cards let B go straight to the
  migration directory and the store query.
- **Hook latency in real sessions** was 128–506 ms per prompt, against 1.3–1.6 s in section 19's
  parallel runs.
- **Noise left in the run:** T3's hook still carried a `key type` card (a JSON fixture key
  matching "the Go type"). It's fixed since, with a test (low-value kinds need a 6+ character
  exact name). The T3 hook now gives `SessionDetail`, `Store.GetSession` and `Migration`. Not
  rerun after that fix.

### Tests added

36 in the full suite:
- `prose_with_incidental_words_gets_no_tool_card`;
- `tokens_stem_and_drop_stopwords` extended (`tabs` → `tab`);
- `profile_follows_the_code` extended (names-only listing; `--keep-descriptions`);
- `code_cards_link_tests_migrations_docs_and_tech` extended: the first card carries
  `lines api/billing.go:5-8 h=…`, and a JSON key `type` isn't a code card for "the Go type".

### Still open

- **SessionStart hook text** from the global setup (24.5k chars) is now the biggest fixed cost
  left. It comes from the user's own hooks, not anything the profile can turn off.
- **Plugin skills can't be turned off one by one** in Claude Code 2.1.318: neither
  `skillOverrides` nor `Skill()` deny affects them.
- **HTTP MCP servers** (posthog, stripe, vercel, sentry) have no tool list until the session
  connects.
- **No headless numbers for the interactive app.** Section 6 above is a model built from this
  session's own request.

---

## 2026-11-05 · 23. Orientation in the batch (`map`, `module`), outlines, slim apply reply; G1 runs 4 (invalid) and 5

### What changed

All of these were verified by replaying run 3's real calls against a clean `bd5fa80` export.

| Change | Driven by (run 3) | Replay or check |
|---|---|---|
| Requested spans inlined first, smallest first; span cap at 85% of a reply | `auth.go:L1-327` asked for 3 times (#5-#7) | #5: 5 spans inlined (was 4) |
| A span that doesn't fit comes back with its **outline** (every definition inside it, with span and hash) plus its size and whether it fits on its own | the same | `Login.tsx` gives 6 definitions, ready to read in pieces |
| `map` query op: directories with file counts by role (`"depth"`, default 2) | #1-#2: `git ls-files` ×2, 25 KB | 292 files grouped by directory, ~2.5k tokens |
| `module` query op: `go.mod` and `go.sum` state, versions in the module cache, packages, and the exported API of one package (`"package"`); also `package.json` | #3-#4: grep `go.sum`, `ls` the module cache | `go-oidc`: not required, v3.20.0 cached, 35 exported items of `oidc` (methods on unexported types filtered out) |
| Slim apply reply: `files:` (+/- per file, new files marked), dependency-manifest changes in the repo (also those from `go get`), "already ran: don't rerun them", changed regions as span + hash only (`"echo": true` or `@@ echo` shows the bodies; a rollback always shows them) | #11: a 28,124 B reply echoing what was just written; #12-#13: the model reran the tests and `git diff go.mod` | **run 3's bundle replayed: 28,150 B → 3,381 B**, the same 23 edits and steps, and the `go.mod` changes listed |

The skill and the mt ON rule now say to put `map` and `module` in the read batch, to read large
files in parts, and not to re-check after a passing apply.

### Run 4: invalid

At 17:40, five minutes into run 4, the other session working in `~/orca/mt` replaced the plugin's
Python hooks with Rust `mt hook <event>` (section 21) and deleted `plugin/hooks/mt_hooks.py`. The
running session still had the old `hooks.json` loaded, so every Bash call from #11 on failed with
`can't open file … mt_hooks.py`. #11 was the bundle; nothing was written.

Its read phase is still a data point: 7 `mt query` batches (#1-#5, #9, #10) plus 3 shell reads of
`openapi.yaml`, $2.87 spent at the cutoff.

The full log is `bench/transactions/g1-run4-on-invalid.md`.

**Fix:** runs now use a frozen copy of the plugin (binary, hooks and skill) in the session
scratchpad. The first relaunch also failed: the old SessionEnd hook waited for "any key" and
used up the launch command. The run script now presses Enter until the shell prompt is back.

### Run 5

The same setup as run 3, on the frozen plugin. The binary (sha 8c0cdb0e…) also carries the other
session's `mt expand` and hook rewrites. The no-database note came 12 s in.

| | ON run 5 | ON run 3 | OFF baseline (mean of runs 1, 2) | ON 5 vs OFF |
|---|---|---|---|---|
| Turns | 13 | 13 | 53 | **-75%** |
| Tool calls | 12 | 13 | 57.5 | **-79%** |
| Active time | 637 s | 612 s | 777 s | -18% |
| Cost | $3.013 | $2.828 | $3.81 | **-21%** |
| Cache read | 1.30M | 1.13M | 6.85M | -81% |
| Cache write | 148k | 155k | 150k | |
| Output tokens | 78.6k | 67.9k | 62.0k | +27% |
| Peak context | 185k | 185k | 180k | |
| Read batches (`mt query`) | 6 (2 with `mt expand`) | 5 | | |
| Orientation shell calls | 0 (used `map`, `module`) | 4 | | |
| Self-checks after the apply | 0 | 2 | | |
| Apply reply | 3,583 B | 28,124 B | | |

**Outcome** (checked by me, without the DB tests):
- `go build`, `go vet`, `go test ./...` and `tsc --noEmit` pass; 8 DB tests skip.
- 16 files, +1244 -20. Uses `coreos/go-oidc`.

### What the new parts did, and what went wrong

- **Worked.**
  - Orientation moved into the read batch: #1 was `map` plus greps, and there were no `ls` or `git ls-files` calls.
  - There were no re-checks after the apply.
  - The apply reply was 3.6 KB.
  - Run 3's repeated whole-file span didn't happen: it used `skeleton` plus named spans, and `mt expand` for pointers.
- **The gate failure cost the whole bundle twice.**
  - #9's first build failed on `missing go.sum entry for module providing package github.com/go-jose/go-jose/v4`: `go get` in #8 had added the module, but not the package's transitive sums. That is a dependency step, not a code fault.
  - The gate rolled back all 20 edits to 14 files.
  - #10 fixed the dependencies; #11 was `mt apply --help | grep -iE 'retry|re…'`, so the model went looking for a retry option.
  - #12 resent the **identical 44,259-byte bundle**: 21.6k output tokens (~$0.43) plus the context. Turn output was 45.1k for #9 and 21.6k for #12.
  - Without the resend, run 5 would have cost about $2.55 (estimated).
- **Output is the cost now.**
  - 78.6k output tokens ($1.57 of $3.01). Most of it is the bundle turn (45.1k, of which the bundle itself is ~11k tokens; the rest is reasoning) plus the resend.
  - Cache reads are only $0.26.

### Next fixes (from run 5)

1. **`mt apply --retry`.** On a gate failure, keep the attempted batch in `.mt/txn/<id>/`, which is already saved, and accept a small corrections bundle against it: "the last attempt, plus these changes". A failed gate then costs only the fix, not the whole bundle.
2. **A setup section that runs after writing, before the gate** (`@@ setup: go mod tidy`), so dependency resolution sees the new imports and can't throw the batch away.
3. Outstanding from section 18: don't index `.tsbuildinfo` and similar build artifacts.

### Transactions

Full log: `bench/transactions/g1-run5-on.md`.

<details><summary>#1: read batch 1 with `map`, `module` and greps (no shell orientation)</summary>

**#1 · turn 1 · Bash · 0.7 s · reply 24,566 B (~6,141 tok)**

Claude sent:
~~~~
mt query - <<'EOF'
[
 {"id":"map","map":".","depth":3},
 {"id":"auth","grep":"oauth|oidc|google|sso|login|password|session_token|Bearer","regex":true},
 {"id":"router","grep":"r\\.(Get|Post|Handle|Route)\\(|HandleFunc\\(","regex":true},
 {"id":"gomod","grep":"^module |^go |^\\t[a-z]","regex":true,"in":"go.mod"}
]
EOF
~~~~

Reply:
~~~~
# mt query: 4 queries: 0 ✓ 0 ✗ 4 answered 0 can't check; 53 items (0 inlined, ~2525 tokens of bodies), reply ~6144 tokens, 27 ms (index: 289 files, 0 re-parsed, 17 ms)
map = map .   [0 ms; 289 indexed files under .]
    (files here)                                    8  config 3, doc 1, source 4
    Specs/claude-design-interactive-setup/          1  doc 1
    Specs/claude-design-interactive-setup/project/   24  config 1, doc 1, source 22
    Specs/relay-backend-spec/                      21  doc 21
    Specs/relay-backend-spec/api/                   2  config 1, doc 1
    Specs/relay-backend-spec/packs/                 2  config 2
    api/                                            1  config 1
    api/packs/                                      2  config 2
    cmd/relay-agent/                                1  source 1
    cmd/relay-agent/internal/                      33  source 27, test 6
    cmd/relay-hook/                                 5  source 5
    cmd/relayd/                                     3  source 2, test 1
… (377 more lines)
~~~~

</details>

<details><summary>#9: the bundle's gate fails on a dependency, and the whole batch is rolled back</summary>

**#9 · turn 9 · Bash · 0.8 s · reply 10,040 B (~2,510 tok) · ERROR**

Claude sent:
~~~~
mt apply - <<'EOF'
@@ create internal/auth/google/google.go
// Package google signs users in with Google: OpenID Connect, authorization code
// flow, PKCE S256 (10 §10.4.5). It is deliberately independent of the store and
// the HTTP layer. It turns one browser round-trip into a verified Identity, and
// the caller decides which account, if any, that identity may open.
package google

… (1309 more lines)
~~~~

Reply:
~~~~
Exit code 1
gate FAILED after writing 20 edits to 14 files. Rolled back 14 file(s); nothing changed on disk. (502 ms total, steps 361 ms)
steps (non-gate steps in parallel, stopped at a failed gate):
   1 FAIL  build      go build ./...  (299 ms, gate)  full log: .mt/txn/1793919466926/step-1.log
       | ../../../../go/pkg/mod/github.com/coreos/go-oidc/v3@v3.20.0/oidc/jose.go:3:8: missing go.sum entry for module providing package github.com/go-jose/go-jose/v4 (imported by github.com/coreos/go-oidc/v3/oidc); to add:
       | 	go get github.com/coreos/go-oidc/v3/oidc@v3.20.0
   2 SKIP  vet        go vet ./internal/auth/... ./internal/api/ ./internal/store/ ./internal/config/ ./internal/app/
   3 SKIP  unit       env -u RELAY_TEST_DB go test ./internal/auth/... ./internal/api/ ./internal/config/ -count=1
   4 SKIP  store-db-skips env -u RELAY_TEST_DB go test ./internal/store/ -run TestGoogleSignIn -count=1 -v
   5 SKIP  web-types  cd web && npx --no-install tsc --noEmit -p .
attempted change (NOT on disk now; resend a corrected batch against the original spans):
[internal/api/auth.go:L4-15 lines h=ceab917d]
 4  	"bytes"
 5  	"io"
 6  	"net/http"
 7+ 	"net/url"
… (231 more lines)
~~~~

</details>

<details><summary>#11, #12: looking for a retry, then the identical bundle resent; the slim reply</summary>

**#11 · turn 11 · Bash · 0.2 s · reply 97 B (~24 tok)**

Claude sent:
~~~~
ls .mt/txn/1793919466926/ 2>/dev/null; mt apply --help 2>&1 | grep -iE 'retry|redo|again|txn' | head
~~~~

Reply:
~~~~
0.orig
1.orig
10.orig
12.orig
13.orig
3.orig
4.orig
8.orig
9.orig
manifest.json
step-1.log
undone
~~~~

**#12 · turn 12 · Bash · 9.6 s · reply 3,583 B (~895 tok)**

Claude sent:
~~~~
mt apply - <<'EOF'
@@ create internal/auth/google/google.go
// Package google signs users in with Google: OpenID Connect, authorization code
// flow, PKCE S256 (10 §10.4.5). It is deliberately independent of the store and
// the HTTP layer. It turns one browser round-trip into a verified Identity, and
// the caller decides which account, if any, that identity may open.
… (1311 more lines)
~~~~

Reply:
~~~~
applied 20 edits to 14 files in 8930 ms (validate 11 ms, baselines 0 ms, write 4 ms, reindex 33 ms, steps 8808 ms). undo: mt undo
format: gofmt 13 file(s), 4 reformatted (70 ms)
files:
  internal/api/auth.go +21 -3
  internal/api/fake_test.go +9 -1
  internal/api/sso_test.go +79 -0 (new)
  internal/api/store.go +3 -0
  internal/app/app.go +14 -1
  internal/auth/google/google.go +359 -0 (new)
  internal/auth/google/google_test.go +296 -0 (new)
  internal/auth/google/googletest/googletest.go +137 -0 (new)
  internal/config/config.go +13 -0
  internal/config/config_test.go +21 -0
  internal/store/auth.go +113 -7
  internal/store/sso_google_test.go +147 -0 (new)
  internal/store/store.go +10 -8
  web/src/screens/Login.tsx +13 -0
dependency manifests (git diff, including changes made outside this batch):
  +	github.com/coreos/go-oidc/v3 v3.20.0 // indirect
  +	github.com/go-jose/go-jose/v4 v4.1.4 // indirect
  +	golang.org/x/oauth2 v0.36.0 // indirect
steps (non-gate steps in parallel):
   1 PASS  build      go build ./...  (6572 ms, gate)
   2 PASS  vet        go vet ./internal/auth/... ./internal/api/ ./internal/store/ ./internal/config/ ./internal/app/  (1068 ms)
   3 PASS  unit       env -u RELAY_TEST_DB go test ./internal/auth/... ./internal/api/ ./internal/config/ -count=1  (2235 ms)
   4 PASS  store-db-skips env -u RELAY_TEST_DB go test ./internal/store/ -run TestGoogleSignIn -count=1 -v  (2233 ms)
   5 ENV   web-types  cd web && npx --no-install tsc --noEmit -p .  (815 ms)  could not run here (a tool or dependency is missing); not a failure of this change. full log: .mt/txn/1793919617304/step-5.log
       | error TS2688: Cannot find type definition file for 'vite/client'.
       |   The file is in the program because:
       |     Entry point of type library 'vite/client' specified in compilerOptions
… (22 more lines)
~~~~

</details>


---

## 2026-11-05 · 24. mt knows no language: removing the Go-specific parts of sections 18 and 23

**Rule (user):** mt is language-agnostic; nothing specific to Go codebases. An audit of `src/`
found Go- or tool-specific code only in what sections 18 and 23 had added. The rest of mt was
already generic: `role_of`, the hook's extension list and the grammars are multi-language data.

| Was | Now | Verified |
|---|---|---|
| `apply` formatted written files with a built-in table (gofmt for `.go`, rustfmt for `.rs`) | no built-in formatter. `"format": "<cmd>"` or `@@ format: <cmd>` runs the repo's own formatter on exactly the files the batch wrote (`{changed}`, or the files appended), before the steps; mt rereads them | a Python repo with a user formatter: it ran on 1 written file and left the others alone |
| `module` parsed `go.mod`/`go.sum`, walked `GOMODCACHE` with Go's path escaping, kept only capitalised (Go-exported) names; plus a `package.json` lookup | `module <name>`: lines naming it, grouped by the file's role: **declared** (config files and files with no code: manifests, lockfiles), **used in** (code), **mentioned in docs**. With `"path"` (the dependency's source on disk, wherever its package manager keeps it; the model gets that from the toolchain), that source's layout and top-level outline from the same grammars as the repo; `"package"` picks a directory. No export rules | Go: pgx declared in `go.mod:37` and `go.sum`, used in 31 code files; go-oidc with a module-cache path gives 7 directories and an `oidc` outline. Python: `requirements.txt:1` declared, `app/net.py` uses it, `venv/requests` outline. npm: `package.json` declared, `src/s.ts` uses it, `node_modules/zod/lib` outline |
| ENV status matched TypeScript's TS2688, "Cannot find type definition file" and `npm ERR! missing` | only exit 126/127 or the shell's own "command not found"/"not found". A step that already failed before the change is what `"baseline": true` (KNOWN) is for | `definitely-missing-tool` gives ENV, and a step that exits 3 gives FAIL |
| The apply reply's dependency summary diffed a fixed list (`go.mod`, `package.json`, `Cargo.toml`, `pyproject.toml`, `requirements.txt`) | "other uncommitted changes in the tree, not made by this batch", from `git status`: modified and untracked files with +/- counts, deletions only counted. That covers any package manager's lockfile, or a generated file | the Python repo listed its 4 untracked files |
| Skill and rule mentioned gofmt, `go get`, `go.mod`, `go build`/`go test`, `npm ci`, `node_modules` | placeholders (`<the repo's build command>`, `<its test command> {changed_dirs}`, `@@ format: <the repo's formatter> {changed}`) and generic wording | `grep` finds no Go- or npm-specific wording in either |

**Bug found and fixed in the new `module`:** "declared vs used" first went by whether the grammar
found definitions in the file. JSON keys and Markdown headings parse as definitions, so
`package.json` and the auth spec counted as code. It now goes by the file's role.

**Tests:** `cargo test`: 36 passed. The other session's `tools.rs` tests compile again, so this is
the first full test run on sections 18, 23 and this one. Built into `target-dev` and copied into
the frozen plugin used for runs (sha 375713bb…), not into `target-plugin`.

**Still open (run 5, section 23):** `mt apply --retry` with a corrections bundle after a failed
gate, and a `@@ setup` step that runs before the gate. Both are designed to stay language-agnostic:
the commands are the repo's own.

---

## 2026-11-05 · 25. Check, prepare, change, prove: one `mt apply` call

**Ask (user):** Claude should be able to say "do all of these; if these are correct, make these
changes, then run the formatter, then the build, and so on", and have it happen in one call with
less thinking and back-and-forth. Most of it existed: run steps, gates, the formatter, span
hashes. Two pieces were missing:
- a way to say "if these are correct" inside the batch;
- commands that run before anything is written.

The skill also had to say how to use it.

**Added to `mt apply` (both the bundle and the JSON form):**

| Section / field | Runs | On failure |
|---|---|---|
| `@@ expect` + JSON array of `mt query` claims (`"expect"`) | first, against the index | any ✗ or "can't check": nothing is written; the reply is the claims report |
| `@@ before [name]: <cmd>` (`"before": [steps]`) | second, in order, before any file is written; the index is refreshed after | any non-PASS: nothing is written; the reply has the excerpt |
| edits | third | (validation as before) |
| `@@ format: <cmd>` | fourth, on exactly the written files | reported; the files are kept |
| `@@ run [gate] …` | last | a gate rolls back; the rest keep the edits |

- The claim check runs `mt query` with a new `"session": false`: no read-batch count, no "already
  shown" marks, and `mt expand`'s numbered items are not overwritten.
- The reply now opens with `expect: N claim(s) hold` and `before k: PASS …` lines.

**Verified** (scratch Go repo, real identifiers):
- **All pass:** `defined_in` plus an exhaustive `callers` claim, a before-step that generates a
  file, a replace, gofmt on that file only, then build and vet (both PASS). Exit 0, session state
  untouched.
- **A claim fails** (`defined_in Nope`): exit 1, nothing written, the claims report returned.
- **A before-step fails:** exit 1, nothing written, the stderr excerpt returned.
- **Found while testing:** identifiers of one or two letters are not indexed, so `callers A`
  finds 0 sites. This is not new.

**The skill, the mt ON rule and the per-prompt hook text** (`src/hook.rs`, a one-line string in
the other session's file) now say:
- The write batch "checks, prepares, changes and proves in one call": `@@ expect`, `@@ before`,
  every file and edit and test, `@@ format`, `@@ run`.
- No separate calls before or after the bundle: adding a dependency goes in `@@ before`.

**This targets run 5's double bundle (section 23).** That gate failed on a dependency the model had
added with `go get` *before* its new imports existed. `@@ before: <add the dependency>` in the
same bundle, ordered after the claims, does that step inside the call.
`mt apply --retry` (resend only corrections after a failed gate) is still open.

**Tests:** `cargo test`: 38 passed, 1 failed. The failure is
`tools::tests::routes_cross_subprojects_and_change_plan`, in the other session's in-progress
`mt tools`. Built into `target-dev` and the frozen run plugin (sha c6130c5f…), not into
`target-plugin`.

---

## 2026-11-05 · 26. Folding, the 10k briefing limit, and never denying useful work; T4 with the plugin at normal and 255k context

**Built.**
- **Folding** (`context::fold_plan`, `scope_lines`). A body shown for its references (a caller,
  a test; never a target or a requested span) of 20+ lines, where folding hides at least 30%, shows:
  its signature, each referencing line (`>`) with one line either side, the lines above it that
  open its enclosing blocks, the last line, and `... L20-34 folded (15 lines)` for the rest.
  Blocks are found by indentation only, so it works in any language (the test covers brace-and-tab
  and indentation-only code). Budgeting counts the folded size, so more items fit. A folded body
  is not written to the seen log, so it never comes back later as "already shown". `--json` folds
  the same way (`folded: [[a, b]]`). `MT_NO_FOLD=1` turns it off.
  Edit-mode pointers use the same scope lines: each reference ±2 plus the signature and enclosing
  block lines, so a pointer reads without being expanded.

  | `mt context X` (study mode) | no fold | fold |
  |---|---|---|
  | WriteAudit | 17 inlined, 4 pointers, ~4,913 tok | 17 inlined, 4 pointers, ~3,065 tok |
  | AuditEntry | 20 inlined, 11 pointers, ~4,968 tok (budget lowered) | 24 inlined, 7 pointers, ~2,814 tok |
  | decodeJSON | 18 inlined, 19 pointers, ~5,200 tok (budget lowered) | 35 inlined, 2 pointers, ~3,475 tok |
  | requireMachine | 21 inlined, 3 pointers, ~3,438 tok | 21 inlined, 3 pointers, ~3,239 tok |

- **MOVED vs CHANGED in `mt expand`.** An edit above a block shifted it. Before, the block came
  back as CHANGED although its hash was identical. Now: `MOVED since the pointer (was L74-83), text
  unchanged`. Scripted check on a scratch copy of the repo: read, an edit that shifts the code,
  then `mt expand 12` gives the new span. `mt apply` accepts that span and hash, and refuses the old.
- **The briefing no longer costs a turn.** Claude Code moves hook `additionalContext` over 10,000
  characters into a file. Measured with a word at the end of the context: 9,900 inline, 10,100
  not. Every plugin run in section 21's build spent turn 1 `cat`-ing the 14.5k briefing (one run
  then read it in pieces with `sed`). Each hook event has its own limit, so the skill now goes in
  `##`-section parts counted in UTF-16 units: SessionStart 9,003, first UserPromptSubmit 7,339,
  nothing lost or doubled. Real session (Haiku): asked about `@@ run` (part 2) and `mt expand`
  (part 1), answered both, 1 turn, 0 tool calls.
- **Hook, language-agnostic (section 24).** "Code" = a file one of mt's grammars parses, minus
  config and docs (`sed` on a manifest or README passes). `grep -A 'pattern' file` is matched
  against that file's own definitions, not a `func|class|def` keyword regex. The tool-name skip
  list is gone. Up to 4 identifiers in a repo grep are rewritten (`mt context A --edit && …`,
  all-or-nothing, so any unknown name falls back to the original grep).
- **Never deny useful work.** The 255k runs below each lost a turn to a denial: a compound
  command was denied for one grep in it, throwing away the `mt expand` in the same call (E-r1),
  and a 20-line `head` got a whole `mt apply` call denied (E-r2). A denial now happens only when
  every part of the command is an old-style read. Anything else is allowed with the hint.
  `head -N`/`tail -N` with N ≤ 300 passes. Replaying both commands through the new hook: allowed.

`cargo test`: 42 passed.

**T4, plugin arm (E): the Rust hooks, rewrites, expand, fold, no instruction.** Same unambiguous
prompt as section 20, Opus 5.5, raw setup. The normal-context E runs used the build *before*
the briefing split and the deny fix; the 255k runs had the briefing split but not the deny fix.

| Run | Turns | Calls | Wall s | Cost $ | Output | Cache read | Checks | Tests | Sites | Expanded / offered |
|---|---|---|---|---|---|---|---|---|---|---|
| normal E-r1 | 7 | 6 | 91.9 | 0.541 | 6,584 | 247,960 | 8/8 | ok | 5/5 | 0 / 53 |
| normal E-r2 | 10 | 9 | 113.0 | 0.647 | 8,143 | 393,372 | 8/8 | ok | 5/5 | 6 / 25 |
| normal E-r3 | 7 | 6 | 92.1 | 0.584 | 6,837 | 260,520 | 8/8 | ok | 5/5 | 6 / 67 |
| 255k A-r1 | 10 | 9 | 87.9 | 2.682 | 5,494 | 2,388,059 | 8/8 | ok | 5/5 | |
| 255k A-r2 | 11 | 10 | 91.2 | 2.749 | 5,208 | 2,669,947 | 8/8 | ok | 5/5 | |
| 255k E-r1 | 7 | 6 | 80.9 | 2.660 | 5,195 | 1,678,971 | 8/8 | ok | 5/5 | 6 / 53 |
| 255k E-r2 | 7 | 6 | 102.4 | 2.718 | 8,986 | 1,667,707 | 8/8 | ok | 5/5 | 6 / 31 |

Round 3 of the 255k set never ran: both arms hit the account's 5-hour usage limit in their first
second. Not counted.

**Reading it.**
- Normal context: E averaged 8.0 turns, $0.591, 99.0 s, against A's 12.0 turns, $0.444, 93.8 s
  (section 20). Turn 1 of every E run was the `cat` of the spilled briefing; fixed since.
- 255k, 2 runs each: E used 7.0 turns against A's 10.5 (−33%), 34% less cache read, the same cost
  ($2.689 vs $2.716, −1%) and the same wall time (91.7 vs 89.6 s). Each E run had one denied turn
  (fixed since) and started 6k tokens higher (the skill). The hook never rewrote anything in
  these runs: the models called mt themselves.
- Turns fall a third, but cost doesn't follow: at 255k a turn's cache read is cheap next to the
  cache writes of large early replies. The next lever is reply size per turn, not turn count.

**Next.** Rerun 255k A vs E (3 each) on this build when the usage limit allows. One denied turn per
run is gone, so E should land at about 6 turns.

---

## 2026-11-05 · 27. Speed: the bash chains Claude writes, against mt, at 500 / 5,000 / 20,000 files

**Ask (user):** before the next integration run on a real codebase, show as unit tests that mt is
faster than the bash and grep scripts it replaces, given that everything is in memory.

**The test:** `tests/speed.rs` (`cargo test --release --test speed -- --ignored --nocapture`;
knobs `MT_SPEED_SIZES`, `MT_SPEED_RUNS`).
- It runs the built `mt` (`CARGO_BIN_EXE_mt`) against real `sh`, so no mt module is linked in.
- **Fixture:** a seeded generated repo with Go, Python and TypeScript in turn, 40 packages, ~60
  lines per file, plus `go.mod`, `package.json` and `requirements.txt`. The generator records
  the ground truth: call sites, constructions, property lines, declarations, files.
- **Scenarios:** the 11 bash chains from this session's transcripts, each against the mt call
  answering the same question.
- **Modes:** bash; mt in-process (`MT_NO_DAEMON=1`, warm on-disk index); mt through a warm
  daemon (`mt serve`, the in-memory graph).
- **Timing:** median of 5 runs after one warm-up. Session state is off (`"session": false`,
  `"fresh": true`) so repeated runs don't get cheaper from dedupe.
- **Correctness is asserted for every row:** both sides must match the ground truth. Speed is
  asserted only for the claim: S11 through the daemon beats bash at 5,000 files and up. Result:
  **1 passed**, 188 s.

**Index:**
- Cold build: 1,211 ms (500 files), 305 ms (5,000), 1,263 ms (20,000). The 500-file figure
  includes first-run setup.
- Warm refresh: 14, 43 and 161 ms.

### Results: ms (bash / mt in-process / mt daemon); x = bash ÷ daemon

| Scenario | bash processes at 20k | 500 files | 5,000 files | 20,000 files | x at 20k |
|---|---|---|---|---|---|
| S1 definition + body | 4 | 26.5 / 11.2 / 12.3 | 105.5 / 39.3 / 8.6 | 378.3 / 142.3 / **14.8** | **25.5x** |
| S2 every caller + enclosing function | 954 | 60.8 / 11.9 / 12.2 | 410.6 / 47.7 / 12.8 | 1,499.1 / 173.6 / **46.4** | **32.3x** |
| S3 eight known reads | 8 | 16.4 / 11.3 / 12.3 | 15.0 / 42.2 / 12.7 | 14.9 / 142.9 / 14.7 | 1.0x |
| S4 grep, then read every hit | 608 | 51.4 / 15.4 / 12.3 | 306.2 / 145.4 / 126.4 | 1,427.8 / 390.3 / **270.5** | **5.3x** |
| S5 repo layout | 3 | 20.0 / 11.0 / 12.3 | 22.2 / 40.5 / 11.9 | 31.5 / 132.3 / 14.6 | 2.2x |
| S6 one file's outline | 1 | 4.7 / 12.4 / 11.1 | 3.9 / 41.9 / 12.6 | **4.3** / 128.0 / 14.5 | 0.3x |
| S7 every construction of a type | 2 | 24.2 / 12.0 / 11.8 | 103.1 / 46.7 / 12.6 | 254.3 / 168.6 / **31.8** | **8.0x** |
| S8 one property, all spellings | 2 | 42.6 / 34.8 / 30.6 | 292.6 / 306.8 / 142.0 | 1,085.8 / 1,080.6 / **610.3** | 1.8x |
| S9 a dependency's declarations | 1 | 3.9 / 16.0 / 23.3 | 4.1 / 111.4 / 85.3 | **5.2** / 446.0 / 353.2 | **0.01x** |
| S10 rename across files + format | 3 | 80.8 / 124.0 / 145.1 | 217.9 / 340.2 / 333.6 | **597.8** / 1,020.4 / 2,460.8 | **0.2x** |
| S11 whole read phase (S1, S2, S4, S5, S7, S8, S9) | 1,568 | 195.0 / 38.3 / 41.2 | 1,285.3 / 358.6 / 313.2 | 4,524.3 / 1,443.9 / **1,140.3** | **4.0x** |

**Output size, S11 at 20k:** bash 282,008 B; mt 27,883 B (capped to one tool result; the counts in
its headers stay exact).

### What it shows

- **In memory pays off as the repo grows.**
  - The daemon's floor is ~12-15 ms per call at every size.
  - In-process, every call opens and reads the on-disk index, so its floor grows: ~11 ms at 500,
    ~40 ms at 5k, ~130-140 ms at 20k.
  - At 20k the daemon is **10x faster than in-process** on lookups (S1 14.8 vs 142.3 ms, S5 14.6
    vs 132.3 ms).
- **Graph lookups that replace a chain win big.**
  - S2 is a grep plus 953 `awk`s walking back to the enclosing function: 1.5 s in bash, 46 ms in
    mt (32x).
  - S1 is 25x, S7 8x, S4 5x.
  - The gap grows with size, because bash rescans every file on each command and mt doesn't.
- **The whole read phase (S11) is 4x faster** (4.5 s against 1.1 s at 20k).
  - It is one mt call against what is, step by step, 1,568 bash processes and 7 or more tool
    calls.
  - Wall time is not the main effect: each of those tool calls would also be a model round trip,
    which costs far more than 4.5 s.
  - It also returns a tenth of the bytes.
- **bash wins on single cheap commands.**
  - S6 (one grep on one file) and S3 (8 `sed`s of known ranges): 4-15 ms either way. mt's floor
    is process start plus a socket round trip.
- **Two mt weak spots, both fixable:**
  1. **S9 `module` is O(repo):** 353 ms at 20k against 5 ms for a grep of the three manifests.
     It reads every indexed file's text from disk to find the name. Fix: look for declarations
     only in non-code files (data, heuristic and text tiers, a handful per repo), and take uses
     from the reference index instead of a scan.
  2. **S10 rename + format is slower than `perl -pi`:** 1.0 s in-process, 2.5 s with the daemon,
     against 0.6 s in bash at 955 files. mt validates every hash, backs up 955 originals for
     `mt undo`, writes in parallel and reindexes; that safety is the cost. The daemon figure is
     inflated by its file watcher re-indexing after the untimed `git checkout` reset between runs,
     so in this test the watcher competes with the timed apply. To check: drain the watcher before
     each timed run.

### Found and fixed on the way

- **mt bug, `module` declared vs used:** it went by "has definitions", so `go.mod` (the
  heuristic tier finds "definitions" in `require` lines) counted as code, and `requirements.txt`
  (`.txt` is the doc role) as a doc. It found 1 of 3 declarations. It now goes by the language
  table, which stays language-agnostic: a programming-language grammar is code (data and markup
  grammars carry `max_depth`), Markdown is docs, and everything else declares. 3 of 3.
- **Test bugs:**
  - the expected S10 count missed the definition's doc comment, a whole-word match bash and mt
    both count;
  - S7: bash counts lines and mt counts sites (one per enclosing function), so both counts are
    now kept;
  - the `count()` parser matched the word "lines" in "lines from the reference index".

**Artifacts:** `bench/speed_last.txt` (the tables), `bench/speed_last.json` (every row).

---

## 2026-11-05 · 28. No undo, no backups; `module` on every core; speed rerun

**User:** "I don't care about mt undo, it should scan the entire repo using as many threads as
there are available cpus", then "remove the undo function entirely".

**Changes:**
- **`mt apply` writes no backups.** It used to copy every original to `.mt/txn/<id>/N.orig` plus
  a manifest before writing.
  - A failing gate (or a failed write) now restores from the originals already held in memory,
    in parallel: it rewrites what existed and deletes what the batch created.
  - `.mt/txn/<id>/` now holds only step logs, and is created only when there are steps.
- **`mt undo` is gone.** The subcommand, `apply::undo`, the "or run mt undo" hint, and the skill
  row ("Preview a change: `mt apply --dry-run`; to revert a finished change, use git") are all
  removed. `mt undo` now gives "unrecognized subcommand".
  - `mt tools profile --undo` (the other session's settings layer) is a different feature and is
    unchanged.
- **`module`:** the whole-repo scan (read, match, classify by the language table) now runs on
  every core (`par_files`, `available_parallelism`: 18 here). Every other repo-wide pass
  already did: indexing, the graph, context, apply's writes, `property`.
- **Speed test, S10:** after each untimed `git checkout` reset, a tiny query runs first. The
  daemon answers only once its watcher has caught up with the disk, so the timed apply doesn't
  overlap that reindex.

**Checked:**
- A failing gate removes the created file and leaves the edited file as it was, and the txn
  directory holds only `step-1.log`.
- `cargo test`: 42 passed.
- The speed suite passed again: every row correct at every size, and the S11 assertion holds.

### Rerun, ms (bash / mt in-process / mt daemon), 20,000 files, against section 27

| Scenario | Section 27 | Now | Change |
|---|---|---|---|
| S9 dependency (`module`) | 5.2 / 446.0 / 353.2 | 4.3 / 321.2 / **196.8** | daemon **-44%**, in-process -28% |
| S10 rename + format (955 files) | 597.8 / 1,020.4 / 2,460.8 | 605.5 / 843.0 / 1,989.8 | in-process **-17%** (no backups), daemon -19% |
| S1 definition + body | 378.3 / 142.3 / 14.8 | 447.4 / 133.7 / 13.5 | 33x faster than bash |
| S2 callers + enclosing function | 1,499.1 / 173.6 / 46.4 | 1,734.1 / 171.6 / 39.8 | **43.5x** faster than bash |
| S7 constructions | 254.3 / 168.6 / 31.8 | 353.6 / 157.3 / 32.4 | 10.9x faster than bash |
| S11 whole read phase | 4,524.3 / 1,443.9 / 1,140.3 | 4,838.1 / 1,568.6 / **1,021.2** | **4.7x** faster than bash |

The bash timings vary 10-20% between runs (S2 1,499 → 1,734 ms), so treat single-row differences
under ~20% as noise. Full tables: `bench/speed_last.txt`.

### What is still slow, and why

1. **S10 through the daemon is 2.4x slower than in-process** (1,990 against 843 ms).
   - `mt apply` never goes through the daemon: it always opens the on-disk index in the client
     process, reindexes the written files itself, and meanwhile the daemon's watcher reindexes
     the same 955 files. Two indexers do the same work at the same time.
   - **Fix:** route `apply` through the daemon when one is running, so the in-memory graph is
     updated once and persisted once. Or, as a smaller step, skip the client's own reindex when a
     daemon owns the repo; the daemon's sync barrier keeps later answers fresh.
2. **Text is not in memory.** S9 (197 ms) is still ~45x slower than `grep` on the three known
   manifests (4 ms).
   - The daemon holds the graph (definitions, references, names) in memory, but the text scans
     (`grep`, `module`, `property`'s line reads, S4's spans) still read every file from disk on
     each call, now on 18 cores.
   - **Fix:** the daemon keeps every indexed file's text in memory, updated by the same watcher.
     Text scans then never touch the disk.
3. **bash still wins single cheap commands** (S6, S3, S9's three-file grep): mt's floor is
   process start plus a socket round trip, ~12-15 ms.

---

## 2026-11-05 · 29. Output compression: logs and JSON stream through mt, the model queries for what it needs

**Rule (user):** every compression algorithm that can be implemented deterministically goes into mt
as a function. Large output, JSON above all, goes through mt. The model gets a **graph** of it and
pulls only what it needs, with JSON queries or plain-word questions. The JSON content itself never
reaches the model.

**Built** (`src/compress.rs`, `mt hook posttool`, `tests/compress.rs`):
- `mt compress` (stdin), `mt run -- <cmd>` (combined output, exit code kept),
  `mt out <id> "<question>" | --json PATH | --grep RE [-C n] | --lines A-B | --level error|warn`,
  `mt out` (list). The original is kept per session in `$TMPDIR/mt-out-<session>/<id>.txt`; the
  id is a hash of the original.
- **Lossless, always:** ANSI escapes stripped, `\r` redraws resolved, identical consecutive
  lines collapsed to `(xN)`, JSON ≤ 1,500 chars minified. Nothing dropped means no header: on tiny
  output the header cost more than it saved (`go vet` was +80% before).
- **JSON > 1,500 chars → graph, no data:** every path, its types, in how many of its parents
  it occurs, number ranges, string lengths, and the values of small sets (≤ 8 short values, e.g.
  `.state string (values {failed:6, passed:294})`), which are what a `[?key=value]` needs.
  Unbounded strings show only "50+ distinct, up to N chars". NDJSON is read as an array.
- **JSON queries:** `.a.b[3]`, `[a:b]`, `[*]`, `[?key=value]`, chained. An answer is minified,
  or shown as a table (keys once, constant columns hoisted, rows sampled with error-looking rows
  kept) when it's big.
- **Plain-word questions:** BM25 over records (each element of every array of objects; leaves
  inside a record are not records of their own, or they outrank it) or over lines, with a light
  stemmer (failed/failing/failure → fail). Deterministic.
- **Logs past the budget (120 lines and 14,400 chars):** a graph (lines, shapes, how many lines
  matter, the commonest shapes with their first line), then runs of same-shape lines (digits and
  hex ignored) folded, passing-line runs folded, head + tail + every error/warning/failure/
  `file:line`/stack frame with context, and lines over 300 chars cut (600 for important ones).
  Everything folded names its `L a-b` range for `mt out --lines`.
- **Verbatim** for code and diffs (`cat`/`sed`/`head`/`git diff|show|blame`): lossless steps
  only, so an `Edit` can still match the text. JSON printed by `cat` is a graph only above 8k chars.
- **Session dedup:** a run of ≥ 6 lines (each ≥ 20 chars) already shown this session becomes one
  line. A whole-file Read of unchanged content becomes a one-line note. Both are forgotten on
  SessionStart `compact`/`clear`, and so is `mt query`'s seen log, which had never been reset.
  After a compaction it pointed at text the model no longer had.
- **PostToolUse hook** on `Bash|Read|mcp__.*`. It keeps each tool's structure (verified to be
  required, see memory). It never touches mt's own output, a Read with offset/limit, or small
  output. A whole-file Read of a code file over 800 lines becomes its outline (first 200 and last
  50 definitions). MCP: every large text block anywhere in the response is compressed, and the
  answer goes out as `updatedMCPToolOutput` and `updatedToolOutput`.
- **Claude Code's own large-output handling, found in the real runs.** A Bash output too big for
  it arrives at the hook with `stdout` truncated to ~30k and the whole text at
  `persistedOutputPath`, and the model would then see a 2 KB preview. The hook reads the file,
  compresses all of it, and drops the path. The `<persisted-output>…saved to: <path>` text form is
  handled too.
- Skill: two Commands rows and a section "Large output and JSON: stream it through mt, then ask
  for only what you need". The skill is 17.8k now and still arrives inline: 9,273 + 9,155 units.

**Measured on real output from these repos** (`mt compress`, tokens ≈ chars/4):

| Output | Tokens in | Tokens out | Saved |
|---|---|---|---|
| `cargo metadata` (JSON) | 113,174 | 2,049 | 99% |
| `git log --stat -n 60` | 8,230 | 1,222 | 86% |
| `cargo build -v` (400 lines) | 3,483 | 815 | 77% |
| `go test -v ./internal/...` | 4,098 | 1,359 | 67% |
| `/tmp/mt-hooks.log` | 6,682 | 4,692 | 30% |
| `git diff` (verbatim) | 2,560 | 2,559 | 0% (exact on purpose) |
| `go vet` (tiny) | 25 | 25 | 0% |

**Real sessions** (Opus 5.5, the plugin, `MT_COMPRESS=0` as control; the task: which of 400
CI checks failed, answer 8 numbers):

| Task | Arm | Correct | Turns | Cost | What the model got / did |
|---|---|---|---|---|---|
| MCP tool returning 44.5k chars of JSON | on | yes | 4 | $0.128 | 605-char graph, then `mt out 5753cb2a --json '.checks[?state=failed]'` |
| | off | yes | 3 | $0.251 | the 44,490 chars |
| `cat data.json` (75.7k chars) | on | yes | 3 | $0.117 | 702-char graph, then `mt out bc4fb99e --json '.checks[?state=failed]'` |
| | off | yes | 3 | $0.122 | Claude Code's 2 KB preview, then `jq` on the file |

MCP: −49% cost, the JSON never entered context. Bash: about the same cost, because Claude
Code's own preview already guards that path, but the model works from a map instead of a cut-off
preview. One run each; the mechanism is verified, the size of the gain is not yet a mean.

`cargo test`: 52 unit + 4 integration (`tests/compress.rs` drives the real binary: JSON graph
with no data leaking, path and plain-word queries, `mt run` exit code and `--lines`/`--level`/
`--grep`, the hook on Bash with its structure kept, mt left alone, cat kept exact, both persisted
forms, Read dedup and its reset on compaction, the large-Read outline, MCP blocks).

**Next.** T4 at 255k with this build (output compression plus the deny fix), and an MCP-heavy
task (e.g. many `gh`/issue calls) with 3 runs each.

---

## 2026-11-05 · 30. `mt tools`: the thirteen improvements, the code graph across sub-projects, and a gold set

The user asked for thirteen improvements (listed at the end of section 22's follow-up) and then
"Implement everything". Two rules from the user shaped them along the way:
- "Facts can be per subproject but must also cross subprojects since an internal can be used by an
  api and the api can be called by ajax."
- Section 24's rule: mt knows no language.

Code: `src/tools.rs` (graph, ranking, cards, profile) and the new `src/tools_ext.rs` (slim plugins,
gate, refresh, capture, explain, agents). 42 tests. Evidence: `bench/tools/run3/`, the gold set
`bench/tools/gold/relay.json`.

### What Claude Code allows, measured first

Each answer came from a headless probe session, and each one changed the design.

| Question | Answer (Claude Code 2.1.318) |
|---|---|
| Can a project switch hooks off and run them through mt instead (`disableAllHooks`)? | It switches off **every** hook, mt's own and the global ones included: 16.5k chars of SessionStart text gone, and nothing left to run mt. So mt wraps each hook instead (#1). |
| Does `permissions.deny: ["Skill(x)"]` drop one plugin skill? | No. All three probe skills stayed listed. With section 19's `skillOverrides` result: no per-skill switch for plugin skills. |
| Does a project-declared directory marketplace load a plugin? | Given with `--settings`, yes, and it **auto-installs at user scope** (a global change; I caused one in a probe and removed it with `claude plugin uninstall` / `marketplace remove`, comparing `installed_plugins.json` before and after). Declared in project settings, a headless session does not install it. |
| Does it serve the directory live? | No. It caches a copy and serves it until the plugin version changes. So each trimmed copy gets its own version. |
| Can a plugin copy be all symlinks? | Not for `.claude-plugin/`, `skills/` or `commands/`: with those linked, no skills listed. Copied, with everything else linked, it loads; its hooks run through the linked `hooks/`. |
| Does `skillListingBudgetFraction` shrink the listing? | Yes: 30,001 → 18,015 chars at 0.005 and 0.002, the names-only floor (section 22). |

### The code graph now crosses sub-projects

- **HTTP routes.** A route literal on a line that registers a route (`.Get(`, `.post(`,
  `.HandleFunc(`, `@app.route(`, …) names its handler: the rightmost identifier on the line that is
  a defined function, or the definition after a decorator. Every other line naming the same route
  is a call site, in any file: frontend ajax, another service, a test or an OpenAPI spec. Path
  parameters (`{id}`, `:id`, `${id}`) are wildcards.
  - Relay: 63 routes, 181 call sites. `/v1/enroll` links `enroll` (Go) to the agent's `control.go:83`
    and the React `Account.tsx:137`.
  - `mt tools routes` prints the whole map.
- **Edges, both ways.** Each call site's enclosing definition and the handler share a route node in
  the PageRank graph.
  - A chosen card that calls a route brings in its handler.
  - A chosen handler brings in its callers.
  - A handler's own calls into other files (the API calling `internal/`) are one more hop.
  - Cards show `calls route /v1/invoices/* → served by getInvoice api/invoice.go:3-7` on frontend code, and
    `serves route … — called from: web/src/InvoicePage.tsx:2 (InvoicePage)` on the handler.
- **Sub-projects only rank.** Facts stay repo-wide, so the profile keeps what any sub-project uses;
  definitions under the session's directory get ×1.25. Nothing is cut at a sub-project boundary.

### Ranking: a gold set instead of impressions

`mt tools bench <gold.json>` scores prompts against the definitions or items they must surface. It
reports recall@3, recall@k (cards shown), MRR, and whether the name appears anywhere in the code
cards. The Relay set has 21 prompts and 32 expected names, covering code and data, frontend → API →
store chains, a skill, a deployment command and an MCP server.

| Step | recall@3 | recall@k | MRR | Fix (from the misses the bench printed) |
|---|---|---|---|---|
| first run | 44% | 66% | 0.389 | |
| de-duplicate by kind + name; doc words count a third toward the multi-word bonus; question words ignored | 53% | 72% | 0.398 | `AuditLog` (a React screen) and `audit_log` (the table) both normalised to `auditlog`, so the table was silently dropped |
| close word forms (revok/revoke), code abbreviations (seq/sequence), exact-name bonus scaled by rarity, endpoint/screen intent, handler → callee hop | 59% | 81% | 0.500 | `session` (in 167 names) won every prompt on its exact-name bonus |
| doc bridges only for code-like names | 72% | 88% | 0.546 | a spec naming `` `state` `` near two prompt words lifted every `state` method |
| instruction words ignored, at most 2 doc-comment words per definition | 72% | 84% | 0.558 | long prompts matched long doc comments word by word |
| directory words (web/src/screens → screen) | 69% | 81% | 0.571 | kept for MRR; costs one item at @3 |

Bugs found on the way: the stemmer turned `invoices` into `invoic`, and the exact-name check used
qualified names (`server.getinvoice`), so a method never matched exactly. Both have tests now.

Misses left: `Sessions` (the screen) for "the sessions list screen", `GetSession` via the
session-detail endpoint, `EnrollTOTPConfirm`, `UpdatePolicy`. Each needs a weak seed to start the
handler chain.

### Each improvement and its evidence

| # | What | Evidence |
|---|---|---|
| 1 | `mt tools gate --event E --cmd-b64 …` runs a hook unchanged: same stdin and exit code, decisions (`decision`, `continue`, `permissionDecision`, `systemMessage`) untouched. Only context text is filtered: a repeat in the session, or text about a product the code doesn't use (a product word named 3+ times and most often, with no fact). Every call is logged to `/tmp/mt-gate.log`. `mt tools hooks` lists every hook (user settings and enabled plugins) with what the gate measured; `--wrap FILE` gates a settings file, keeping `FILE.mt-backup` | CLI: first context kept, repeat dropped, PostHog text dropped (named 4 times, not in this code), a block decision passed through, exit 2 passed through. **In a real session**, the PostHog SessionStart text was dropped. Wrapping a copy of the user's settings gated 41 commands with every other key unchanged. The real global settings were **not** wrapped; that is the user's call |
| 2 | `profile --slim [--install]`: trimmed copies of kept **product** plugins in `.mt/marketplace`, one per project (`mt-<hash>`), version `<orig>-mt.<hash of kept skills>`, hooks gated. A skill is dropped when its own name has specific words and none is a strong fact. Workflow plugins (superpowers) are never trimmed; that rule was added after the first run dropped `brainstorming`. `--install` runs `claude plugin marketplace add` + `install --scope local`; `--undo` uninstalls, removes the marketplace and deletes the plugin cache | Vercel: 11 of 32 skills kept. Live session: 15 `vercel:` entries (4 commands, 11 skills), first request **31,529** tokens. After `--undo`: no `mt-` entry in `installed_plugins.json`, `known_marketplaces.json` or user `settings.json` (`marketplace add` writes there), no cache directory |
| 2b | duplicate plugins: two enabled plugins whose skill names overlap ≥60% → the smaller is turned off | `vercel-plugin` off, 28 of 28 names shared with `vercel`. SessionStart hook text 20,055 → 11,998 chars |
| 3 | completeness claims: a table card lists **all** writers and readers by regex over every indexed code file (`writes, all of them (regex over 217 code files ✓ exhaustive)`), and callers and tests come from the refs index, marked exhaustive. Names too short or common to link (`state`, `live`) say so instead of listing every file that mentions them | T1: `audit_log` writers `Maintenance.runRetention maintenance.go:481,488; Store.WriteAudit audit.go:52,65` |
| 4 | change plans: one `mt apply` bundle with the next migration's files (numbering, width and up/down suffixes read from the existing files), `@@ replace path:Lx-y <hash>` for each target, `@@ check <the repo's build or test command>`, tests that reference the targets, and files usually committed alongside | test `routes_cross_subprojects_and_change_plan`: `0002_invoices_paid_at.up.sql` + `.down.sql`, `@@ replace api/invoice.go:L…`, `api/invoice_test.go` |
| 5 | git co-change: files committed together in the last 400 commits (sweeping commits of more than 30 files skipped) link the seeds in the PageRank graph, cached per HEAD | in the graph and in plans ("usually committed together with…") |
| 6 | the gold set and `mt tools bench` | the table above |
| 7 | doc bridges: a Markdown line naming `` `a_code_like_name` `` and two prompt words links the phrase to that definition; doc-comment words are a third field (weight 0.6, at most 2 per definition) | bench steps 3 to 5 |
| 8 | task kinds (change, debug, question) shape the cards: a change gets a plan and a looser cutoff, debugging gets the top file's last 3 commits. Cards are packed into a character budget, and identifiers the prompt writes like code are always shown | tests `task_kind_and_route_shape` and `routes_cross…`; the hook takes 130–460 ms |
| 9 | `mt tools hook-start` (SessionStart): recompute the profile with its saved options (`.mt/tools-profile.json`), undo what mt turned off that the code now uses, apply what is newly unused, and tell the session | added `posthog-js` to `web/package.json`; the next session's context said `back on: plugin posthog@claude-plugins-official — posthog in code: web/package.json: posthog-js`, and the settings file had PostHog on |
| 10 | sub-projects rank only (see above) | |
| 11 | `mt tools hook-post` (PostToolUse on `mcp__.*\|ToolSearch`): records MCP tool names a session calls or finds; the catalog attaches them to servers mt can't probe | PostHog's HTTP server: 0 → 3 tool nodes |
| 12 | `mt tools explain [--session F]`: a session's first request taken apart (each listing's size) and every listed skill, agent and MCP server with mt's decision and its reason. The transcript is read only to show what was sent | this interactive session: 79,731 tokens; 86k chars of tool schemas, 31k of SessionStart hook text; 299 of 405 listed skills and 27 of 44 agents off for this repo |
| 13 | `mt tools agents`: `.codex/hooks.json` (UserPromptSubmit → `mt tools hook`) and `.cursor/rules/mt-tools.mdc` (an always-on rule: run `mt tools find` first) | **Not verified end to end.** Codex ran 12 user-level UserPromptSubmit hooks but not the project's; it loads a project's `.codex/` only for trusted projects (`trust_level` in `~/.codex/config.toml`), and the ChatGPT usage limit blocked the model call. The hook now logs payload keys to `/tmp/mt-tools-hook-in.log` for that test |

### mt knows no language (section 24), applied to this work

Audit of `tools.rs` and `tools_ext.rs`; manifest names now appear only in test fixtures.

| Was | Now |
|---|---|
| facts parsed package.json, go.mod, Cargo.toml, pyproject, requirements, Gemfile, Podfile, csproj, gradle | **declaration files**: near a project root, not code by mt's own detection (config role, data grammars json/yaml/toml/xml, or plain text), ignore lists and lockfiles skipped. Inside them, a dependency is the name written right before a version number. Facts read like `chi ← go.mod:34: github.com/go-chi/chi/v5`, `react ← web/package.json:15` |
| change plans ran `go build` / `cargo check` / `tsc` by marker file | `@@ check <the repo's build or test command>` |
| the callee hop matched `.Capitalised(` (Go's exported names) | any identifier before `(` that is defined in another file |
| the UI boost listed `.tsx/.jsx/.vue/.svelte` | the clients are files that call routes and register none, from the route map |
| migrations assumed `.up.sql`/`.down.sql` | numbering, width and suffixes read from the latest migration in the directory |

The first version of declaration facts took every identifier, which grounded `graft` from
`.ignore`, `ponytail` from a shell script, and 23 skills from plain YAML words (`scenario`, `level`,
`image`). The version-adjacent rule fixed all of them. The resulting profile for the Relay copy is
identical, item for item, to the one the A/B below ran with.

### A/B, run 3: four tasks, sequential, warm cache

Same raw setup as section 22. B uses the profile above (duplicate plugin off, names-only listing)
and the hook with code cards, routes, claims and plans; **no embeddings** yet (they came after).
New task **T4** crosses sub-projects: find where the Sessions screen sends its state filter
(`web/src/screens/Sessions.tsx:22`), where the API reads it (`internal/api/sessions.go:32`) and
where the store applies it (`internal/store/sessions.go`, `ListSessions`).

12 runs per arm: 4 tasks, 3 reps, A and B alternating, one warm-up per arm. Every step:
`bench/tools/run3/analysis3.txt`; raw streams in `bench/tools/run3/runs/`.

| Mean | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A-T1 / B-T1 | 5.3 / 4.0 | 4.3 / 3.0 | 58,222 / 38,755 | 334,044 / 160,283 | 2,125 / 1,514 | 61.0 / 43.3 | 1.220 / 0.707 | 9/9, 9/9 |
| A-T2 / B-T2 | 3.0 / 2.0 | 1.0 / 1.0 | 58,240 / 38,173 | 129,393 / 78,901 | 106 / 400 | 14.9 / 14.3 | 1.161 / 0.617 | 6/6, 6/6 |
| A-T3 / B-T3 | 6.3 / 5.0 | 5.3 / 4.0 | 58,751 / 38,365 | 415,708 / 187,164 | 2,602 / 1,834 | 61.3 / 39.5 | 1.377 / 0.837 | 15/15, 15/15 |
| A-T4 / B-T4 | 6.7 / 6.0 | 5.7 / 5.0 | 58,319 / 36,417 | 444,348 / 269,745 | 3,549 / 3,096 | 87.2 / 72.1 | 1.495 / 0.974 | 6/9, 8/9 |
| **A / B, all** | **5.3 / 4.2 (-21%)** | 4.1 / 3.2 | **58,383 / 37,927 (-35%)** | **330,873 / 174,023 (-47%)** | 2,096 / 1,711 | **56.1 / 42.3 (-25%)** | **1.313 / 0.784 (-40%)** | **36/39, 38/39** |

- T4's misses, in both arms, are all the store step (`ListSessions`): the frontend → API hop is
  found, and the API → store hop is the hard one, in the bench too.
- Before the profile tightening, T4 in B took 7.0 turns against 6.5. The run-3 hook's T4 cards
  were weak (`SessionFilter`, `loadSession`); the ranking fixes after it (directory words, the
  exact-name rule) are in the bench, not in this run.

### Embeddings, pure Rust, and the vector graph

The user asked whether embeddings would improve accuracy and reduce context, "use only an
embedding that is rust native", and whether a vector-graph structure could give the best of both.

- **Rust-native.** The official `model2vec-rs` crate pulls in Oniguruma (C) by default, and the
  pure-regex feature still builds a C++ suffix-array library. So `src/embed.rs` implements Model2Vec
  directly, with no new dependency:
  - BERT-style normalisation, then greedy longest-match WordPiece from `tokenizer.json`;
  - the `model.safetensors` header and f32 rows read by hand (8-byte length, JSON header, raw
    little-endian data);
  - mean of the token vectors, L2-normalised.
  
  Model: `minishlab/potion-base-8M` (29,528 tokens × 256, 30 MB) in `~/.cache/mt/models/`.
- **The vector graph.** HNSW is a graph over similarity, not code relations, and brute force over
  6.7k × 256 takes a few milliseconds, so it isn't needed. Instead, each definition's text vector
  (name words, file, directory, doc comment) is **propagated** twice over the code graph's typed
  edges: calls to distinctive names, route caller ↔ handler (weight 1.5), table use ↔ table
  definition. Each round is `self_w · own + (1 − self_w) · weighted mean of neighbours`. A store
  function takes on words of the handler and screen that reach it. Relay: 6,757 definitions,
  26,696 edges, 73 ms.
- **Use.** Cosine above the repo's median similarity adds to each seed's word score, scaled to the
  best word score, and the 15 closest vector-only definitions join as seeds. PageRank then ranks
  as before. `MT_EMBED=off|text|graph`, `MT_EMBED_W`, `MT_EMBED_SELF`.

**Bench fix first.** Code and tool cards are separate lists in the hook, so each expected item is
now ranked in its own list. The combined ranking had blamed embeddings for tool items pushed down
by extra code cards.

| Seeds | Tuning set (32 items): recall@3 / @k / MRR | Held-out set (12 new prompts): recall@3 / @k / MRR | Both (44) |
|---|---|---|---|
| words only | 75% / 81% / 0.616 | 42% / 50% / 0.433 | 66% / 73% / 0.566 |
| + text vectors (w 0.6) | 81% / 81% / 0.661 | 42% / 42% / 0.417 | |
| + graph-propagated vectors (w 0.6, self 0.5) | **81% / 88% / 0.647** | 42% / 42% / 0.417 | **70% / 75% / 0.584** |

**Reading this honestly.**
- Embeddings fixed real vocabulary misses in the tuning set (`EnrollTOTPConfirm`, `search`) and
  raised six ranks.
- On the held-out prompts they changed nothing at @3 and lost one item at @k.
- The bigger finding is the held-out score itself: **42% @3 against 75%**. A good part of this
  section's ranking gains is overfitting to the 21 tuning prompts.
- The held-out misses are semantic ("valid session cookie rejected" → `AuthenticateSession`, "OS
  keychain" → `keyringGet`, "moved out of the event" → `oversizedField`), which the 8M static
  model doesn't bridge.
- **Context:** embeddings don't shrink the listings; at best better seeds mean fewer wrong cards
  and fewer verifying calls.
- **Latency:** the hook goes from 162 to 282 ms (model load plus 73 ms of embedding and
  propagation).
- Default: graph mode, w 0.6, self 0.5. A small pooled gain (2–4 points, within one or two items)
  at a cost that fits the hook budget.

### Open

- **A larger gold set, written by someone other than the person tuning.** 44 items cannot tell
  2 points from noise. This is the next thing to build.
- **A stronger Rust-native embedding:** candle with a small BERT (MiniLM, bge-small), to see whether
  the semantic held-out misses move. Vectors cached per definition hash, so the hook doesn't
  re-embed 6.7k definitions on every prompt.
- **The API → store hop** (T4's `ListSessions`, the bench's `GetSession`/`UpdatePolicy`) is the
  weakest link in the cross-sub-project chain.
- **Codex:** verify the hook end to end once the project is trusted in `~/.codex/config.toml` and
  the usage limit resets (Nov 10).
- **The user's global hooks** can be gated with `mt tools hooks --wrap ~/.claude/settings.json`;
  not applied, since that is the user's call.

---

## 2026-11-05 · 31. Named stores: each line of work keeps its own outputs, and any agent can read any of them

**Rule (user):** an agent debugging Vercel logs and another studying docker compose logs must not
get each other's data, but each must be able to pull from the other's.

**Built.**
- Kept outputs moved from a per-session temp dir to **named stores** in the repo:
  `.mt/out/<name>/<id>.txt` plus `<id>.meta` (label, store, owner session, time). So every
  agent and session in the repo can read every store. Outside a repo they go to the temp dir;
  `MT_OUT_DIR` overrides. The store writes `.mt/out/.gitignore` (`*`) itself: kept outputs
  are raw logs and API responses and may hold tokens, so they stay out of git whatever the
  repo's own ignores say (checked in a fresh repo). Outputs older than 7 days are pruned when a new
  one is kept.
- **Naming:** `--name` on `mt compress` / `mt run`. Otherwise `MT_OUT_NAME` (an agent's own),
  otherwise the command: program plus first plain-word argument, wrappers skipped (`vercel logs
  …` → `vercel-logs`, `docker compose logs` → `docker-compose`, `sudo FOO=1 go test` → `go-test`,
  `npx jest` → `jest`). MCP output is named after server and tool. Reads go to `read`. So by
  default, outputs of different tools never share a store.
- **Queries:** `mt out <id>` finds the id in whichever store holds it.
  `mt out --name <store> <query>` searches that store, newest first, each answer under its id.
  `mt out <query>` with neither searches every store. `mt out` lists the stores and their
  outputs with who kept each. `mt out --name vercel-logs "question"` parses the first word as
  part of the question, not an id, unless it is a kept id.
- The session dedup (what this context was shown) stays per session in the temp dir: it is about
  one context, not shared data.

**A ranking flaw the new test exposed, fixed.** "why did it time out" over compose logs returned
30 `checkpoint starting: time N` lines and missed `ETIMEDOUT … (timeout waiting for db)`. Now:
adjacent query words are also tried joined ("time out" → `timeout`); in logs, important lines
(errors, failures) score 2.5x; hits below 25% of the best score are dropped.

**Test** `named_stores_keep_work_apart_and_any_store_can_be_read`. Two stores both containing a
timeout. A question to each returns only its own line. An agent with another name reads the other
store's output by id. A grep over every store returns both, each under its store. The listing
groups by store. Unnamed hook output from `vercel logs` and `docker compose logs` lands in
`vercel-logs` and `docker-compose`. `cargo test`: 54 unit, 5 integration.

**Known limit.** The skill is 18.4k now. Its second part is 9,745 of the 10,000 units a hook can
inline, so the next addition will make a part 3, which arrives only with a second prompt (a
one-prompt headless run never gets it). Trim the skill, or move reference detail behind
`mt help`, before adding more.

---

## 2026-11-05 · 32. In memory for real: apply through the daemon, file text in memory, a lower floor, reindexing only what changed

**User:** "implement all three" (from section 28), then "we need to make sure the reindexing is
efficient and does not touch or reindex things that didn't change".

### 1. `mt apply` goes through the daemon

- **Before:** `mt apply` always ran in the client process against the on-disk index. It
  re-indexed what it wrote, then sent the daemon a `touch`, which walks the whole tree. Meanwhile
  the daemon's watcher re-indexed the same files: two indexers on the same work.
- **Now:**
  - `mt apply` sends the batch to the repo's daemon when one is running
    (`apply::run_with(idx, Some(graph), …)`).
  - The daemon skips the up-front tree walk (its graph is fresh: the request passed the cookie
    barrier). Afterwards it re-indexes exactly the written files, into both the index and the
    graph, once (`resync(Some(written))`).
  - The claims check (`@@ expect`) runs against the graph.
- **Caller fidelity:**
  - The client sends its working directory and environment (`apply::Client`). Steps and the
    formatter run with that environment: `env -u SECRET_VAR mt apply` leaves `SECRET_VAR` unset
    in the steps, checked by a step.
  - Relative body files resolve from the caller's directory. This was a bug: the path was checked
    against the daemon's directory; fixed and checked from a subdirectory.
- **Never applied twice:** `serve::request_apply` falls back to in-process only if no daemon took
  the batch (none running, or another build). Once the daemon has it, any failure comes back as an
  error ("check `git status` before resending"), and there is no read timeout, because steps can
  take minutes. The old 120 s timeout plus fallback would have applied a long batch twice.
- **In-process (no daemon):** after writing, it refreshes only the written files, against their
  own index rows (`Index::known_for`). Before, it walked the tree: 0-6 ms now at 20,000 files.
  The up-front freshness walk (stat only, ~125 ms at 20k) stays, because without a watcher it is
  the only way to know.
- **Checked through the daemon:** claims, steps, a gate rollback (the created file removed), a dry
  run (nothing written), env fidelity, relative paths.

### 2. File text in memory (`src/texts.rs`)

- The daemon loads every indexed file's text at start, on every core: 20,003 files, **14 MB,
  313 ms**. `Graph::apply` keeps it current, and the cookie barrier means a request never sees
  text older than the disk.
- Every repo-file read in `query` and `context` goes through `texts::get`: grep, `module`,
  `property`, `span`, `skeleton`, outlines and context bodies. Without the daemon (the CLI,
  tests) it reads the disk as before. Keys are absolute paths, so two repos in one process never
  share an entry.

### 3. The per-call floor

Measured per request with `MT_TIMING=1` in the daemon: read 0.01 ms, **barrier 4-5 ms**, work
**0.07 ms**, reply 0.01 ms.
- **Fixed:** the watcher reacted to mt's own files (cookies, `index.sqlite-wal`), so every request
  triggered a refresh of the daemon's own index write. It also cloned all 20,000 paths
  (`known()`) on every burst, while holding the lock that requests wait on.
- **Floor now, without the measuring shell:** `mt --version` 6.1 ms; `mt query` through the daemon
  **11.2 ms**; `grep` on one file 10.0 ms. That is process start plus the 4-5 ms barrier, which is
  macOS FSEvents with latency 0 and NoDefer already set. It is kept: it is what guarantees an answer
  never lags the disk.

### 4. Reindexing only what changed

Measured with a per-batch watcher log (`MT_TIMING=1`: paths in, re-parsed, restamped, removed,
whether it walked, ms), on the 20,000-file fixture:

| Change | Before | Now |
|---|---|---|
| `touch` (same bytes), twice | 1 re-parsed each time | **0 re-parsed, 1 restamped**, 0.13 ms |
| a new file | **walked all 20,000 files**, 156 ms | 1 parsed, no walk, 0.91 ms |
| a deleted file | 1 removed | 1 removed, 0.61 ms |
| `git checkout` of 2 changed files | 1 re-parsed; `.git/index` in the batch forced the full known-files map | 2 re-parsed (both changed), `.git/` dropped, 1.12 ms |
| `mt apply` through the daemon, 1 file | apply indexed in-process, then the daemon walked (`touch`) | apply indexed it once; the watcher's event then re-parsed **0** |
| a new directory with 2 files | **walked all 20,000 files**, 130 ms | 2 parsed, only that directory walked, 1.96 ms |
| that directory deleted | | 2 removed, no walk, 0.23 ms |
| `git checkout` of the whole tree (1 file differed) | | only that file re-parsed |

**How:**
- **A content hash per file**, `files.hash`, fnv1a. It was added with `ALTER TABLE` inside schema
  5, so this build and the other session's still share one index; rows without a hash are parsed
  once and get one.
  - When a known file's stamp changed, its bytes are hashed first.
  - Same bytes: only the stamp is updated, in the index (`UPDATE files SET mtime, size`) and the
    graph (`Graph::restamp`). There is no re-parse and no rewrite of its definitions and
    references.
- **New files** are parsed directly: no walk.
- **Directories:** a directory in a batch is walked alone; files under a vanished directory are
  removed.
- **Known-files map:** `known_for` builds it for just the batch's paths plus files under any
  directory among them (`Graph::known_for`, `Index::known_for`).
- **Watcher filter:** `index::watch_relevant` drops `.git/`, `.mt/`, every skipped directory
  (`node_modules`, `target`, ...) and mt's `.mt-tmp-` files.

**Regression test:** `index::reindex_tests::reindex_touches_only_what_changed` asserts re-parsed,
restamped, removed and `walked == false` for each case above (touch, an edit, a new file, a new
directory, a deleted directory, the watcher filter), plus 0 left over afterwards. Passing.

**Tests:** 54 unit tests pass. `tests/compress.rs` has 3 failures; it is the other session's
in-progress output-compression feature (`src/compress.rs`), not this work.

### Speed suite, clean rerun

- Binary sha `92aadea56e49` before and after: nothing rebuilt during the run.
- 1 passed, 202 s; every row correct at every size.
- The run in section 28's follow-up was discarded: target-dev was rebuilt during it.

ms at 20,000 files (bash / mt in-process / **mt daemon**), and against section 28:

| Scenario | Section 28 (daemon) | Now | bash ÷ daemon |
|---|---|---|---|
| S1 definition + body | 13.5 | 382.2 / 138.8 / **11.2** | **34x** |
| S2 every caller + enclosing function | 39.8 | 1,965.6 / 166.7 / **27.5** | **71x** |
| S4 grep, then read every hit | 268.7 | 1,462.2 / 429.4 / **98.2** | **15x** |
| S7 constructions | 32.4 | 331.8 / 195.4 / **26.4** | 13x |
| S8 one property, all spellings | 565.7 | 1,025.2 / 1,335.5 / **119.2** | 8.6x |
| S9 dependency (`module`) | 196.8 | 7.0 / 351.2 / **24.4** | 0.3x (bash greps 3 known files) |
| S10 rename + format, 955 files | 1,989.8 | 831.6 / 872.5 / **742.6** | **1.1x**: mt now beats perl + gofmt |
| S11 whole read phase | 1,021.2 | 7,190.0 / 1,961.9 / **227.5** | **31.6x** |

- Bash timings moved 15-50% between runs (S11 4.8 → 7.2 s), so treat the bash column as noisy.
  The daemon column is stable.
- At 5,000 files: S2 43x, S11 12x. At 500 files every daemon row is ~11 ms, the floor.
- **Still slower than bash:** single cheap commands (S6 one-file outline 4.8 against 11.4 ms; S3
  eight known reads 15 against 11 ms, about even; S9). That is mt's 11 ms floor against a 5 ms
  grep.

**Artifacts:**
- `bench/speed_last.txt` and `bench/speed_last.json`
- this build copied into the frozen run plugin (not `target-plugin`)

---

## 2026-11-05 · 33. An independent gold set, and a Rust-native BERT

Section 30 ended with two open items: a gold set written by someone other than whoever tunes the
ranking, and a stronger Rust-native embedding model. Both are done here.

### The gold set: 90 prompts nobody tuned for

Three fresh agents each wrote 30 prompts for one area of the Relay repo copy. They had no access to
mt, graft or any ranking output (plain grep and read only), and their briefs required:
- prompts typed the way a developer would: bug reports, change requests, questions;
- paraphrase: no prompt may spell out an expected identifier;
- a mix of single-definition, cross-layer (handler and store; agent and server; React component
  and Go handler) and data prompts;
- every expected definition verified with grep, with its file:line;
- none of the targets already used in mt's own gold sets.

| Area | Prompts | Kinds |
|---|---|---|
| `internal/api`, `internal/store`, migrations | 30 | 10 single, 10 cross, 10 data |
| `cmd/relay-agent` and the ingest/assembler/realtime pipeline | 30 | 12 single, 10 cross, 8 data |
| `web/src` and the Go handlers it calls | 30 | 12 frontend, 12 cross, 6 types |

I checked the result: all 147 expected names exist in mt's index, and no prompt contains its own
answer. Files: `bench/tools/gold/relay-independent.json` (merged), the three per-area files, and the
split used below (`gold-tune.json` = the api-store area, `gold-test.json` = the other 60 prompts).

**First result: the word ranking was overfit.** On the 90 independent prompts it scores 24% @3,
against 75% on my own 21 tuning prompts.

### BERT in pure Rust: candle and bge-small

- **Crates.** `candle-core`, `candle-nn`, `candle-transformers` 0.11.1 with default features, which
  are empty: CPU only, no CUDA, Metal, MKL, Accelerate or ONNX, no C or C++. They are optional
  dependencies behind the `bert` feature, so builds without it are unchanged:
  `cargo build --release --features bert`. The build takes 44 s.
- **Model.** `BAAI/bge-small-en-v1.5`: 12 layers, 384 dimensions, 133 MB, in
  `~/.cache/mt/models/bge-small-en-v1.5`.
- **Encoding.** mt's own WordPiece tokenizer from `embed.rs` (bge uses the same format) with
  `[CLS]`/`[SEP]`, at most 64 tokens, batches of 32 padded to the longest; the `[CLS]` vector,
  L2-normalised (how bge is trained). Prompts get bge's retrieval instruction.
- **`Embedder`** is either the static Model2Vec table or BERT, picked by the model directory's
  `config.json`. Default: bge when built with `bert` and on disk, else potion-base-8M.
  `MT_EMBED_MODEL` overrides.
- **Cache.** Definition vectors are cached in `tools.sqlite` (`embcache`, keyed by model and text
  hash). `mt tools embed` fills it: Relay's 6,757 definitions (3,606 distinct texts) took **47 s**,
  once. The hook uses BERT only when the cache already covers every definition; otherwise it falls
  back to the static model for that prompt, so a first prompt never waits on a full encoding pass.
- **Hook latency** with BERT and a warm cache: 215–233 ms (model2vec: 183–270 ms).

### Results, with the tuning kept honest

The embedding weight was first tuned on the 21 overfit prompts (w 0.6). To re-tune without touching
the test data, I swept only on the api-store area (30 prompts), chose a configuration there, and
only then scored the other 60.

Sweep on the tuning split, bge (recall@3 / recall@k / MRR):

| | w 0.6 | w 1.5 | w 3 | w 6 |
|---|---|---|---|---|
| text | 53 / 57 / 0.400 | 55 / 57 / 0.439 | 51 / 61 / 0.449 | 53 / 63 / 0.447 |
| graph, self 0.5 | 51 / 61 / 0.403 | 53 / 67 / 0.419 | 53 / 71 / 0.439 | 53 / 75 / 0.420 |

Chosen there: graph, w 3, self 0.5.

Test split, 60 prompts, 96 items, never used for tuning:

| Seeds | recall@3 | recall@k | MRR |
|---|---|---|---|
| words only | 19% | 22% | 0.149 |
| model2vec, text, w 1.5 | 26% | 32% | 0.194 |
| model2vec, graph, w 3 | 23% | 30% | 0.178 |
| bge, text, w 0.6 (old default) | 25% | 32% | 0.190 |
| **bge, text, w 3** | **29%** | **39%** | **0.233** |
| bge, graph, w 3, self 0.5 (the configuration chosen on the tuning split) | 29% | 39% | 0.205 |

**Reading this.**
- BERT at weight 3 adds **+10 points @3, +17 @k, +0.084 MRR** over words alone, on prompts nobody
  tuned for. That is about 10 to 16 of 96 items: real, not noise.
- Graph propagation, the configuration chosen on the tuning split, ties plain text vectors on
  recall and loses on MRR. On held-out prompts it doesn't pay, for either model.
- New defaults: text mode; weight 3 for BERT, 1.5 for model2vec. Propagation stays available as
  `MT_EMBED=graph`.

Final scores with those defaults (recall@3 / recall@k / MRR; full output in
`bench/tools/embeddings-final.txt`):

| Gold set | words only | model2vec | bge |
|---|---|---|---|
| independent, 90 prompts / 147 items (the api-store third was used for the weight) | 24 / 31 / 0.205 | 33 / 39 / 0.261 | **37 / 46 / 0.307** |
| section 30's held-out, 12 items | 42 / 50 / 0.433 | 33 / 33 / 0.236 | **50 / 58 / 0.319** |
| my tuning set, 32 items | 75 / 81 / 0.616 | 75 / 78 / 0.626 | 72 / 88 / 0.605 |

**Where it still fails.** Independent set, recall@3 by area and kind, words only → bge:

| Area / kind | words only | bge |
|---|---|---|
| api-store, cross | 6/21 | 11/21 |
| api-store, data | 9/20 | 11/20 |
| agent-pipeline, cross | 4/25 | 7/25 |
| agent-pipeline, **data** | 1/13 | **0/13** |
| web-api, cross | 5/24 | 7/24 |
| web-api, frontend | 3/14 | 5/14 |
| web-api, types | 1/8 | 4/8 |

The agent's data-flow prompts (spool, outbox, batch retries) and the cross-component chains (agent →
server, web → API) are the weakest. These chains go through queues and wire formats, not the HTTP
routes and direct calls mt links today.

### Open

- **Data-flow edges:** the agent's outbox → shipper → ingest batch → assembler path, linked by the
  event types in `pkg/schema` and the payloads that cross the wire. That is the edge type missing for
  the weakest category, and the same three agents' 90 prompts can measure it.
- **Cache freshness:** the BERT cache is keyed by definition text, so edits only re-encode changed
  definitions. `mt tools embed` should run at SessionStart (`hook-start`) in the background, and
  the hook falls back to model2vec until the cache is full.
- **The end-to-end A/B (section 30) predates embeddings.** It should be rerun with the bge build and
  the independent prompts as tasks.

---

## 2026-11-05 · 34. Standard query languages for kept JSON: JSONPath (RFC 9535) and jq

**Why.** mt's `--json` was a home-made dialect (`.items[?state=failed]`) the model could only
copy from the header. The industry formats are JSONPath (IETF RFC 9535, 2024; also Postgres
`jsonpath`, `kubectl -o jsonpath`), jq (the de facto command-line language), and JMESPath
(AWS/Azure CLI). In every control run Opus wrote jq unprompted, so jq is what it already reaches for.

**Built.**
- `--json` is **RFC 9535 JSONPath** via `serde_json_path`: `@` filters, `&&`/`||`,
  comparisons, `..` recursive descent, `length()`, `match()`, `search()`, slices. Each match
  comes back with its normalized location (`$['items'][7]['number']  7`) when the results are few
  and small, else as one table. The earlier shorthand still works: `to_jsonpath` turns
  `.items[?state=failed].title` into `$.items[?@.state=='failed'].title`, so every query
  already in transcripts and skills stays valid. A bad path says "not a JSONPath (…)" with the parser's error.
- `--jq '<filter>'` runs **jaq** (jq in Rust: `jaq-core`, `jaq-std`, `jaq-json`) in process,
  with no external jq. NDJSON is given to it as one array. It is bounded: at most 10,000
  results, and a filter running past `MT_JQ_SECONDS` (default 10) is stopped with "stopped after
  N s". A filter that recurses without end (`def f: f; f`) could overflow the stack and abort the
  process, so it is not tested; that limit is jaq's.
- The graph header now shows one JSONPath and one jq example. Its old dialect examples are gone.
- Skill: the output section was rewritten with JSONPath and jq **and shorter** (skill 18,376 →
  17,950 chars; part 2 of the briefing 9,745 → 9,319 units, no part 3).

**Tests.** Unit: shorthand translation; RFC features (`&&` with `>`, `..`, `match()`,
`length()`, a parse error); jq `select`, `group_by`/`from_entries`, a stream of results, a
syntax error. Integration (real binary): shorthand → RFC with locations; `&&`+`>`; `$..number`
(300); `match()` (10); a bad path (exit 1); jq filter, jq counting (`{"failed":6,"passed":294}`),
a jq syntax error (exit 1), `last(repeat(1))` stopped at 1 s; jq over NDJSON. `cargo test`: 54
unit + 5 integration, all pass.

**Real session** (Opus 5.5, plugin, the 44.5k-char MCP response; asked which checks failed and how
many each service has): the model read the 763-char graph, ran
`mt out 5753cb2a --jq '[.checks[] | select(.state=="failed") | {number, service, name}]'` (452
chars back), and took the per-service totals (133/134/133) from the graph's value counts. All
correct, 4 turns, $0.148; the JSON never entered context.

---

## 2026-11-05 · 35. Fair bash baselines, many agents on one daemon, and why the daemon queued them

**User:** are the scenarios executed in parallel or serially, and would parallel help since it is a
lot of IO? Then: "also simulate multiple agents using multiple mt clients against a single
daemon", and "it shouldn't slow down with multiple agents, why did it slow down?"

### What runs in parallel

- **The test harness: serial, on purpose.** Each timed run gets the machine to itself.
- **Inside mt: already parallel.**
  - A query batch runs in waves: every query whose `$id` inputs are ready runs at once, one
    thread each.
  - Every repo-wide scan (grep, property, module, constructs, indexing, apply's writes and
    restore) is split across all cores (18 here).
  - Since section 32 the daemon does no file IO for reads: the text is in memory.
- **Bash in the test was serial and single-threaded** (`/usr/bin/grep`), which flatters mt. A
  Claude Code session's `grep` is a shell function calling the **ugrep** bundled in the Claude
  Code binary, and `rg` its ripgrep. Both are multi-threaded. Measured on S8 at 20k:
  `/usr/bin/grep` 1,110 ms, ugrep 296 ms (3.7x), ripgrep 369 ms, plus ugrep's ~21 ms start-up
  per call.

### Added to `tests/speed.rs`

- **Mode "bash (Claude Code)":**
  - `grep` is a shim calling the bundled ugrep (a symlink named `ugrep` to the Claude Code
    binary, which dispatches on its program name) with the session wrapper's exact flags.
  - S2's per-hit enclosing-function lookups and S4's per-file reads fan out with
    `xargs -P <cores>`.
  - Skipped, with a note, when no Claude Code binary is found.
- **Agents:** 1, 2, 4 and 8 agents start at the same instant (a barrier) on the largest repo. Each
  runs an agent's read sequence: definition, callers, constructions, the whole read phase.
  - Modes: all agents against **one daemon**, each in-process, each in Claude Code's bash.
  - Reported: wall time, per-request median and p95, requests per second.

### Why the daemon slowed down with more agents

Three things serialized every agent behind every other:
1. **One thread** accepted and answered connections: `for conn in listener.incoming()` read,
   answered, then took the next.
2. **Each request held that thread through its own 4-5 ms freshness barrier** (the cookie round
   trip through FSEvents), with the daemon idle meanwhile. That is half of a light request, so
   barriers ran back to back instead of side by side.
3. **The graph was behind an exclusive `Mutex`,** although queries only read it.

The single-threaded run shows it: at 20,000 files, 1 → 2 → 4 → 8 agents took 324 → 562 → 1,103 →
2,185 ms of wall time at a flat **~14.5 requests/s**.

### Fix (`src/serve.rs`)

- **One thread per connection** (`handle()`).
- **`RwLock<(Index, Graph)>`:** queries, `context` and `status` share the read lock. The watcher,
  `touch`, an unsynced refresh and `apply` take the write lock.
- **Overlapping barriers:** each request still writes its own cookie (an atomic counter) and waits
  for the watcher to report it, so every change made before the request is in the graph. Concurrent
  requests now wait at the same time.
- **`apply` stays exclusive:** it changes files and the graph, and the caller environment it runs
  steps in is process-wide.
- ponytail: unbounded threads; a pool if hundreds of agents ever share one daemon.

### Results, 20,000 files

**Agents, all against one daemon** (single-threaded → threaded):

| Agents | Wall | Median request | p95 | Requests/s |
|---|---|---|---|---|
| 1 | 324 → **250** ms | 25.1 → 18.8 ms | 261 → 204 ms | 12.3 → 16.0 |
| 2 | 562 → **330** ms | 29.6 → 24.3 ms | 479 → 270 ms | 14.2 → 24.2 |
| 4 | 1,103 → **407** ms | 53.0 → 26.8 ms | 687 → 321 ms | 14.5 → 39.3 |
| 8 | 2,185 → **525** ms | 101.5 → **24.6** ms | 1,439 → **402** ms | 14.6 → **61.0** |

- The median request now stays flat (~19-27 ms) whatever the number of agents.
- The p95 still grows a little: those are the whole-read-phase requests, and 8 of them at once
  share the same 18 cores. The work is CPU-bound now; nothing queues.

**The same 8 agents, other modes:**
- in-process: 6,407 ms, 5.0 requests/s (each process opens and reads the on-disk index; ~140 ms
  per call);
- Claude Code's bash: 15,525 ms, 2.1 requests/s.

**Scenarios, against the fair baseline,** daemon vs Claude Code's bash (ugrep, `xargs -P`), at
20k, median of 5:

| Scenario | bash (Claude Code) | mt daemon | Speed-up |
|---|---|---|---|
| S1 definition + body | 71.7 ms | 12.2 ms | 5.9x |
| S2 callers + enclosing function | 957.5 ms | 12.2 ms | **78x** |
| S4 grep, then read every hit | 393.6 ms | 75.6 ms | 5.2x |
| S7 constructions | 77.6 ms | 11.6 ms | 6.7x |
| S8 property | 233.3 ms | 93.2 ms | 2.5x |
| S11 whole read phase | 1,742.1 ms | 223.0 ms | **7.8x** |
| S3, S5, S6, S9 | 11-30 ms | 11-12 ms | 1.0-2.8x (ugrep's ~21 ms start-up meets mt's ~11 ms floor) |
| S10 rename + format | 388.9 ms | 599.2 ms | **0.6x: bash wins** (ugrep finds the files fast; mt validates, writes and re-indexes 955 files) |

- Against plain `/usr/bin/grep` the same S11 is 20x. The fair number is 7.8x.
- Correct in every mode at every size; 1 passed.
- Unit tests: 55 passed, plus the other session's `tests/compress.rs`: 5 passed.

**Artifacts:**
- `bench/speed_last.txt` and `bench/speed_last.json` (with an `agents` section)
- this build copied into the frozen run plugin

---

## 2026-11-05 · 36. mt vs graft, side by side on the same queries

**Setup.** Relay repo (Go + TS). graft 0.21.1, its graph checked in sync (`graft check`: OK);
mt index current. Each tool's best command per query, median of 5 runs, a fresh mt session per
run (no "already shown" dedup across repeats). Tokens = chars/4 of exactly what the model would
read, including graft's per-call notice. Answer key: an exhaustive regex scan of all 211 tracked
code files (graft/, Specs/, node_modules excluded). References are scored on use sites, not
definitions (definitions are what `defined_in` answers). Depth 2 is scored against the Go
source: call sites mapped to their enclosing function, with Go's package visibility for
lowercase names. Scripts: session scratchpad `mt_vs_graft.py`, `depth_truth.py`, outputs in `mvg/`.

| Query | mt ms | mt tokens | mt recall | graft ms | graft tokens | graft recall |
|---|---|---|---|---|---|---|
| all references to AuditEntry | 48 | 2,766 | 21/21 | 402 | 873 | 19/21 (missed 2 TS imports) |
| all references to WriteAudit | 21 | 1,633 | 6/6 | 262 | 517 | 6/6 |
| callers of decodeJSON | 18 | 1,845 | 19/19 | 202 | 772 | 19/19 |
| callers of requireMachine (passed as a value: `g.Use(s.requireMachine)`) | 18 | 268 | 2/2 | 189 | 206 | 0/2 |
| blast radius of WriteAudit, depth 2 | 19 | 5,035 | hop 1: 5/5, hop 2: 23/23 + 13 false | 185 | 316 | hop 1: 1/5, hop 2: 2/23, 0 false |
| literal text `request_id` | 20 | 2,520 | 9/9 | 196 | 432 | 8/9 (missed a .tsx) |
| outline of internal/api/server.go | 9 | 287 | all 33 top-level | 197 | 778 | all 33 |
| requireMachine with its body | 10 | 507 | the body, nothing else | 203 | 1,380 | the body + 7 other symbols |
| "how is an audit row written" | 17 (`mt context WriteAudit`) | 3,595 | names all 5 writers | 215 (`graft ask`, plain words) | 1,116 | names 1 of 5 |

**Reading it.**
- mt is about 10x faster on every query (9-48 ms vs 185-402 ms). graft is a Node CLI that
  loads its graph on every call.
- graft's answers are 1.5-16x smaller. It prints only the hit line; mt prints each hit with its
  surrounding lines and enclosing blocks and a span hash, so the result can be edited without
  another read. Smaller output costs graft in recall: it drops what it can't resolve. Every call
  through the store interface (`s.d.Store.WriteAudit`) is dropped because "2 definitions share
  the name" (the real one and a test fake), so its blast radius finds 1 of 5 direct and 2 of 23
  indirect callers. It also misses references that aren't calls (a function passed as a value)
  and some TS files (it indexes 191 files to mt's 485).
- mt finds everything, and is noisy where names collide. At hop 2 it adds 13 functions that
  only share a name (`subscribe` in the TS client and in the bus tests). Its header says so
  ("by name … a match may refer to another one"). And it indexes graft's own generated cards
  (`graft/**/*.md`, committed in this repo) as docs: 9 of the items above are cards.
- graft answers plain-word questions about code (`graft ask`); mt needs a name.

**mt issues this exposed (open).** 1) Committed generated mirrors (`graft/`) are indexed as
docs: duplicates and wasted tokens. 2) Hop 2 by name pulls in same-named functions from
unrelated directories; a language-agnostic tie-break (prefer the definition nearest the caller,
the same directory first) would cut most of the 13. 3) No plain-word search over code.

---

## 2026-11-05 · 37. Wire keys between components, and the A/B with BERT

### Wire keys

Section 33's weakest categories were the agent's data flow (0/13 at @3) and cross-component chains
(agent → server 7/25, web → API 7/24). Those components agree on things without calling each
other: status codes, header names, queue and channel names, event kinds, JSON keys.

A **wire key** is a string literal, or an identifier the repo doesn't define (`StatusConflict`, a
header name), that appears in 2 to 8 files across at least two sub-projects (first two path
segments). Routes are left out (they have their own edges), and so are docs, config, migrations and
generic words. Each key links the definitions that contain it:
- seeds sharing a key get a PageRank edge (0.7);
- a chosen card brings in up to 2 definitions in other sub-projects that share a key with it;
- the card says so: ``wire (shared with other components): `turn.ended` also in FoldEvent server/fold/apply.go:3``.

`MT_WIRE=off` turns them off. Parameters were fixed before measuring. Test:
`wire_keys_link_components_that_never_call_each_other`.

Independent gold set, bge defaults (recall@3 / recall@k / MRR / named anywhere in the code cards):

| | All 90 prompts | Test split (60) |
|---|---|---|
| wire keys off | 38 / 46 / 0.306 / 56% | 30 / 40 / 0.230 / 44% |
| wire keys on | 37 / 46 / 0.309 / 59% | 29 / 39 / 0.231 / **50%** |

**Reading this.**
- Wire keys don't move the right definition into a card heading: within one item either way.
- They do put it into the context: named somewhere in the code cards on 50% of test items instead
  of 44%. Claude reads all of it, so the keys stay on.
- The agent data-flow prompts stay at 0/13 @3. Their seeds land in the wrong component (tests and
  web handlers come first), so the edges never get a chance.

**Bug found:** the embedding bump wasn't weighted by role and kind, so long, descriptive test names
(`TestSeqGapIsCountedOnce`) won on meaning alone. The bump now uses the same `role_w × kind_w` as
the word score. The measured effect is small (+1 item @3).

**Cache read:** one query for the whole model instead of one per definition (40 → 33 ms warm). The
565 ms seen once was a cold first read.

### The A/B, rerun with BERT

B = section 30's profile plus the hook from the `bert` build: bge-small text vectors (w 3, cache
filled by `mt tools embed`), wire keys, route links, claims and plans. Same 4 tasks, 3 reps,
sequential with warm-ups, 24 sessions. All hook calls in B took 401–475 ms. Every step:
`bench/tools/run4/analysis4.txt`.

| Mean | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A-T1 / B-T1 | 5.7 / 3.7 | 4.7 / 2.7 | 58,222 / 38,093 | 358,750 / 144,067 | 2,129 / 1,480 | 63.8 / 42.3 | 1.150 / 0.590 | 9/9, 9/9 |
| A-T2 / B-T2 | 3.0 / 2.0 | 1.0 / 1.0 | 58,240 / 38,086 | 129,399 / 78,986 | 111 / 384 | 15.7 / 15.3 | 1.064 / 0.523 | 6/6, 6/6 |
| A-T3 / B-T3 | 7.3 / 4.3 | 6.3 / 3.3 | 58,669 / 39,125 | 526,648 / 184,153 | 2,702 / 1,840 | 82.9 / 47.2 | 1.484 / 0.701 | 15/15, 15/15 |
| A-T4 / B-T4 | 6.3 / 5.7 | 5.3 / 4.7 | 58,319 / 37,245 | 439,640 / 247,397 | 2,874 / 3,003 | 76.8 / 71.5 | 1.406 / 0.838 | 6/9, 6/9 |
| **A / B, all** | **5.6 / 3.9 (-30%)** | 4.3 / 2.9 | **58,362 / 38,137 (-35%)** | **363,609 / 163,651 (-55%)** | 1,954 / 1,677 | **59.8 / 44.1 (-26%)** | **1.276 / 0.663 (-48%)** | 36/39, 36/39 |

Against run 3 (section 30, no embeddings), B against A:

| | Run 3 | Run 4 |
|---|---|---|
| Turns | -21% | -30% |
| Input tokens | -47% | -55% |
| Cost | -40% | -48% |

Arm A also varies between runs (5.3 → 5.6 turns), so the run-to-run difference is partly noise. T3
gains the most (4.3 turns against 7.3).

**T4 is unchanged in correctness:** 6/9 in both arms. The misses are again the store step
(`ListSessions`): the API → store hop under a cross-component prompt, matching the bench.

### Open

- **Seeding for data-flow prompts.** The fix is about where the search starts, not about edges:
  prompts that describe a pipeline step ("written to the agent's queue", "redelivered batch") need
  seeds in the right component before any graph step can help.
- **Paired A/B mode** (warm-up, then each A/B pair run together) to bring 40 minutes down to about
  15–20 without the cache race of run 1.

---

## 2026-11-05 · 38. Reindexing in the background, never holding anything up; a race it exposed; rename speed

**User:** "The reindexing should be a background thread and not hold up anything." Also: "how
much faster can a rename be?"

### What held things up before

1. **The watcher parsed changed files while holding the graph's write lock,** and wrote SQLite
   under it too, so every query waited for both.
2. **The daemon's `apply` held the write lock for the whole batch,** including its build and
   test steps: a 10-second `go test` blocked every agent's queries for 10 seconds. It also
   re-indexed the written files inline before replying.
3. **A query's freshness wait included the SQLite write,** not just the parse it needs.

### Now

- **The index is outside the graph's lock:** `Index` methods take `&self`, since SQLite has its
  own mutex. `refresh` split into `scan` (stat, content-hash check, parse on every core, no
  writes) and `persist` (the SQLite writes).
- **One background reindexer** (the watcher thread):
  1. `scan` with no lock held;
  2. the graph's write lock only to `restamp`/`apply` (microseconds);
  3. acknowledge waiting requests;
  4. hand the SQLite write to a **persister thread** (a channel, so the order is kept).
- **"Same content?" now checks the graph's hash** (`FileData.hash`, `Graph::hashes`), the
  in-memory truth, not SQLite's, which can lag behind the persister. The graph loads the hash from
  the index at start.
- **`apply` in the daemon** (`apply::Ctx::Daemon`) holds no graph lock across its writes or
  steps, and does not re-index: the watcher sees the files and the reindexer picks them up. Claims
  and rename lookups take a brief read lock. Batches are serialized among themselves
  (`APPLY_LOCK`), because the caller environment the steps run in is process-wide.
- **Requests wait only for what freshness needs:** the parse of files changed before they were
  asked. They never wait on SQLite, other requests or an apply's steps.

**Checked:**
- a query during an `apply` running a 3-second step: **18.8 ms**, with the reindexer indexing
  that apply's new file (0.2 ms) while the step still ran;
- a query sent the instant `apply` returned found the new function;
- unit tests 55 + 5 pass.

### A race this exposed, and fixed

- **The symptom.** A stress run on the 20,000-file fixture logged "954 **restamped** (same
  content)" for files `perl -pi` had really changed. An in-process rename then found no
  `ResolveAccount` in SQLite: references that didn't match the disk, with stamps that did.
- **Cause: two reindexers.** When a request's barrier timed out (1 s), the request thread ran its
  own full scan and wrote SQLite directly, while the watcher scanned and queued writes to the
  persister. An older scan could land after a newer one.
- **Why barriers timed out.** `perl -pi` makes a temp file per edited file (`pkg0/XX…`, 954 per
  burst). For each path the graph did not know, `known_for` scanned all 20,000 entries in case it
  was a directory: ~19 million comparisons per burst. `scan`'s removed-directory check did the same.
- **Fixes:**
  - **One reindexer.** A request needing a full walk (`touch`, a timed-out barrier) asks the
    reindexer on its channel (`.mt/walk-N`) and waits. Walks are numbered and acknowledged apart
    from cookies: a walk sent down the channel can overtake a cookie still in FSEvents, and a
    shared counter would have released that cookie's request early.
  - **One pass for unknown paths,** in `Graph::known_for` and `scan`: collect them, then check
    each entry's parent directories against the set.
- **Stress rerun on a freshly built index** (`perl -pi` V1→V2 and `git checkout` back, twice;
  then an mt rename plus gofmt; then a reset). After every step the graph and SQLite agree with
  each other and the disk (953 references flip each time). **0 barrier timeouts, 0 full walks;**
  each burst re-parsed exactly the 954 changed files.

### Rename: how fast can it be (955 files, 20,000-file repo)

**Breakdown:**
- `grep -rlw` to find the files: 360 ms;
- writing 954 files: ~110 ms;
- gofmt on 954 files: ~100 ms;
- validation: 8-21 ms;
- the reply's tail (954 line diffs for the region list and the +/- counts, plus `git status`):
  **~160 ms, now ~40 ms**. The diffs run on every core, in order (`par_in_order`; `par` deals
  items out round-robin), and `git status` starts on its own thread the moment the files are
  written, overlapping the formatter and steps.

| Rename + gofmt, 955 files | bash (`/usr/bin/grep`) | bash (Claude Code: ugrep) | mt daemon |
|---|---|---|---|
| section 32 | 831.6 ms | | 742.6 ms |
| section 35 (threaded) | 665.3 ms | 388.9 ms | 599.2 ms (**0.6x** of Claude Code's bash) |
| **now** | 656.4 ms | 431.3 ms | **293.1 ms (1.5x faster than Claude Code's bash, 2.2x than plain)** |

What is left is the work itself: writing 954 files, plus gofmt, plus ~15 ms of client round trip
and barrier. About 300 ms is the floor with a formatter.

### Final speed run on this build

- Binary sha `9a88bd42a487` before and after.
- 1 passed, 305 s; correct in every mode at every size.

20,000 files, mt daemon against Claude Code's bash:
- S1 9.9x, S2 41.9x, S4 5.0x, S7 3.0x, S8 2.2x, **S10 1.5x**, **S11 6.8x** (287.7 against
  1,955 ms).
- 8 agents on one daemon: 608 ms wall, median 32.3 ms, 52.6 requests/s. In-process: 6,454 ms.
  Claude Code's bash: 17,280 ms.
- **S11 moved 223 → 288 ms against section 35, and 8 agents 525 → 608 ms.** An A/B run of both
  daemon builds, alternating at the same moment, gives the same S11 (median 290.0 and 288.6 ms for
  the previous build, 283.5 and 291.5 ms for this one). It is machine load (bash moved by up to
  40% too), not this change.

---

## 2026-11-06 · 39. Accuracy first: definition-following resolution (from reading graft's resolver), mirror cards out, nothing silently missing

**From graft's source** (MIT; `graph/resolve.js`, `bindings.js`): edges carry a confidence tier
(`extracted`: same file, import or containment; `inferred`: a bare name unique across files). A
name defined in several files is **dropped**, never guessed. `reachable()` rejects a different
language family. Member calls need a receiver type from per-language bindings. Imports are resolved
per language (go.mod packages, TS relative paths, Java FQN suffixes, Rust `crate::`, PHP PSR-4,
C includes). Precise, but it loses everything ambiguous: 1 of 5 direct callers of `WriteAudit`,
because a test fake shares the name.

**Built** (`src/resolve.rs`, wired into `callers`/`references`/`tested_by`, language-agnostic):
- A call site is code: comments and string literals don't count (`in_code`).
- Language family from the grammar table (TS/TSX/JS one family, C/C++/ObjC one). An unknown family
  never filters.
- Shipped code never reaches a test's definition, so a fake can't steal the real callers.
- Of the definitions sharing a name, each site is attributed to the nearest: its own file, then a
  file that names the definition's directory or module (`context::reachable`, already used for
  types), then the same directory, then the longest shared path. A tie is **kept**, labelled
  ambiguous, and not expanded further.
- **Hops follow definitions, not names**: hop 2 asks who reaches *this* `Client.subscribe`, not
  every word `subscribe`.
- Every site is labelled (`resolved` / `nearest definition` / `ambiguous`). The header counts
  them, and what was left out and why ("3 only in comments/strings, 10 reaching another definition").
- Generated per-file mirrors are not indexed (`index::mirrors`): a top-level dir where ≥ 20 markdown
  files, and ≥ 80% of its markdown, are named after a source file at the same relative path.
  This repo's 191 graft cards dropped out of the index (485 → 294 files). A real docs folder stays.
- **Nothing silently missing**: items over the output limit used to be counted ("not shown: 4
  items"). They are now named as `path:Ls-e Name` handles for one `mt expand`.

**Single-query accuracy after** (same answer keys as section 36): references/callers recall
unchanged (100% everywhere). Blast radius depth 2: **5/5 direct, 23/23 indirect, 0 false**, down
from 13 false, and its output fell 5,035 → 3,544 tokens. graft: 1/5, 2/23.

**Whole workflows** (the user's test). graft's lookup, then one Bash `sed -n` per file for every range
it reported (merged per file, the fewest calls possible), against one mt call, plus one
`mt expand` when the reply named pointers or unshown items. Completeness = whether the code of
every function the task needs (answer key from the source) is in what the model got. Wall time =
tool time only; every extra call is also a model round trip (about 8-10 s each in the T4 runs, not
counted here).

| Task | graft calls | graft tokens | graft ms | graft code found | mt calls | mt tokens | mt ms | mt code found |
|---|---|---|---|---|---|---|---|---|
| callers of decodeJSON, with code | 7 | 4,798 | 316 | 19/19 | 1 | 6,045 | 19 | 19/19 |
| blast radius of WriteAudit (depth 2), with code | 4 | 2,530 | 282 | 3/28 | 2 | 11,438 | 162 | 28/28 |
| every use of AuditEntry, with code | 13 | 6,263 | 441 | 7/7 | 2 | 9,091 | 27 | 7/7 |
| read requireMachine | 9 | 1,250 | 378 | 1/1 | 1 | 507 | 9 | 1/1 |
| how is an audit row written | 6 | 993 | 305 | 2/6 | 2 | 5,170 | 24 | 6/6 |

- mt: 1-2 calls per task and complete in every task. graft: 4-13 calls, and complete in 3 of 5.
  On the two that need resolution it reached 3 of 28 and 2 of 6 functions.
- Tokens: mt is equal or smaller where both are complete and the task is precise (requireMachine
  507 vs 1,250; AuditEntry 6,223 in one call vs 6,263 in 13). It is larger where it returns what graft
  never found (blast radius: 28 bodies vs 3). The `mt expand` step here expands *every* pointer, an
  upper bound: models expanded about 5% of pointers in sections 20 and 26.
- `graft ask requireMachine` led to 8 reads of unrelated files; mt's span read 1.

`cargo test`: 57 unit + 5 integration. New tests: `resolve::tests` (comments, strings, block
comments, family, nearest, the test-fake rule, ties kept, import evidence) and `index` mirrors (cards
dropped, a docs folder kept). Scripts: session scratchpad `mt_vs_graft.py`, `depth_truth.py`, `workflow.py`.

**Still open:** plain-word search over code (graft's `ask`), so a "how does X work" question needs
no name.

---

## 2026-11-06 · 40. No one-off scripts: `@@ regex` over a directory, and the skill says so

**User:** the skill should say that instead of Python loops over files and other one-off scripts,
the better approach is `mt apply`.

**Evidence:** in the last 14 days of transcripts (6,892 Bash calls, 200 sessions), Python/Node
one-off scripts were 5.4% of Bash calls and shell `for` loops 3.8%. This period is weighted by the
mt benchmark sessions themselves.

**The gap first.** The commonest script, "the same edit in every `*.ts` under `src/`", had no
`mt apply` form:
- the JSON `regex` op took only `files` or `refs`;
- the `@@` bundle had no regex section at all.

**Added:**
- **`"in": "<dir prefix>"` and `"ext": [...]` on the regex op.** It targets every indexed text
  file under the prefix (`.` is the whole repo) with those extensions that the pattern actually
  matches. The pre-filter reads the in-memory text in the daemon and runs on every core, so a
  pattern over a large directory does not plan an edit per file.
- **`@@ regex [in <dir>] [ext a,b] [refs <Name>] [files a,b] [expect N]`.** The pattern is the
  section's body, and the replacement the body of the `@@ with` that follows (`$1` captures
  work).

**Checked, in-process and through the daemon:**
- `in src ext ts,tsx expect 6` changed exactly the 3 matching files, with `$1` applied;
  `other/` and a `.md` were untouched.
- A wrong `expect 5`: "regex matches 6 times across 3 files, expected 5; nothing written".
- Through the daemon, the next grep saw all 6 new lines.
- No match: 0 edits.

**The skill, the mt ON rule and the per-prompt hook:**
- **New skill section, "No one-off scripts: mt is the loop".** A table maps each script pattern to
  its mt call: a reading loop to one query (`grep` plus `span $id`); `text.replace` in several
  files to `@@ find`/`with` or `@@ rename`; a regex over a directory to `@@ regex in … ext …`;
  `sed -i`/`perl -pi` to the same; `cat >`/heredocs to `@@ create`; the formatter, build and
  tests afterwards to `@@ format`/`@@ run`.
- **It says why:** validation before writing, all-or-nothing, `expect` counts, gate rollback, and
  a reply listing every file. "A script that silently changes 3 files instead of 30 is the failure
  this prevents."
- **The rule and the per-prompt hook line** add: "No Python/shell scripts or loops over files: the
  same edit in many files is one `@@ regex in <dir> ext <exts> expect N`."

**Tests:** 57 unit + 5 (`tests/compress.rs`) pass. Built into `target-mt2` and the frozen run
plugin.

---

## 2026-11-06 · 41. Component-first seeding (rejected), a regression the A/B caught, and paired A/B runs

### Component first: tried and removed

Hypothesis: prompts that describe a pipeline step ("written to the agent's queue", "a redelivered
batch") name a component before they name a function, so mt should rank directories first and
search inside the top ones. The design was fixed before measuring:
- a directory's vector is the mean of its definitions' vectors (tests excluded), plus 0.05 per
  prompt word in its path;
- the top 3 directories' definitions get ×1.5;
- each top directory adds its 5 closest definitions as seeds.

Measured with bge defaults (recall@3 / recall@k / MRR):

| Gold set | off | on |
|---|---|---|
| tuning split | 53 / 61 / 0.458 | 49 / 59 / 0.413 |
| test split | 29 / 39 / 0.231 | 26 / 33 / 0.184 |
| independent, all 90 | 37 / 46 / 0.309 | 34 / 42 / 0.263 |
| agent data-flow prompts only (@3) | 1/13 | 0/13 |

The component ranking itself was reasonable: for the outbox prompt it ranked
`cmd/relay-agent/internal/agent` first, where `Daemon.enqueue` lives. But `DB.Enqueue` sits in the
outbox package, and favouring three directories took weight from the right places everywhere else.
The code is deleted, not left behind a switch.

### A regression the A/B caught and the bench couldn't

The first paired run (below) scored B-T2 5/6, the first T2 miss in any run. T2's hook now carried
4.9k chars of code cards (`Client.Complete` in the Anthropic provider, matched on the meaning of
"sounds like AI") for a request to rewrite a marketing sentence; in one of three runs Claude
answered without reading the skill. Run 3, without embeddings, had injected 312 chars (the
humanizer card only).

Two causes:
- vector-only seeds skip the two-word rule;
- the word side also matched prose ("reply", "only") through doc words and a design doc's mention
  of `eval_response`.

First fix tried: gate every prompt on "two prompt words in some definition's name, an exact name,
or a code-like identifier". That fixed T2 but cost the independent set 37% → 32% @3, because
paraphrased bug reports often name no definition.

Kept: **a prompt that names a skill, agent or MCP server** (a distinctive name: a hyphen or 8+
characters) **and matches no definition by name or by a code-like word is a tool request and gets
no code cards.** T2 is back to the single humanizer card. The independent set is unchanged (37 / 46
/ 0.309), and the older sets slightly improved (tuning 78 / 88 / 0.668, held-out 50 / 67 / 0.311).
Test: an assertion in `code_cards_link_tests_migrations_docs_and_tech`.

**Lesson:** the bench scores whether the right code appears; only the end-to-end run shows code
appearing where it shouldn't.

### Paired A/B mode

`bench/tools/run5/pairs.sh`: one warm-up per arm, run together, then each task and rep runs its A
and B sessions side by side.
- Before the fix: 14.3 min. After: 10.1 min. The sequential run took about 40.
- **No cold-cache first requests** in either paired run (checked per session), so the warm-up does
  what the sequential order was for.

Paired run with the fix (B = bge, wire keys, the tool-request rule; 12 sessions per arm; every
step in `bench/tools/run5/analysis6.txt`):

| Mean | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A-T1 / B-T1 | 6.0 / 3.7 | 5.0 / 2.7 | 58,222 / 37,538 | 378,441 / 145,465 | 2,266 / 1,471 | 53.2 / 29.3 | 1.162 / 0.606 | 9/9, 9/9 |
| A-T2 / B-T2 | 3.0 / 2.0 | 1.0 / 1.0 | 58,240 / 36,134 | 129,404 / 76,424 | 120 / 249 | 14.2 / 12.1 | 1.064 / 0.503 | 6/6, 6/6 |
| A-T3 / B-T3 | 8.3 / 6.3 | 7.3 / 5.3 | 58,670 / 39,124 | 583,767 / 243,283 | 3,269 / 2,267 | 70.2 / 46.8 | 1.480 / 0.756 | 15/15, 15/15 |
| A-T4 / B-T4 | 5.7 / 5.7 | 4.7 / 4.7 | 58,237 / 37,254 | 385,121 / 249,811 | 2,570 / 2,366 | 45.3 / 40.5 | 1.362 / 0.817 | 6/9, 6/9 |
| **A / B, all** | **5.8 / 4.4 (-24%)** | 4.5 / 3.4 | **58,342 / 37,512 (-36%)** | **369,183 / 178,746 (-52%)** | 2,056 / 1,588 | **45.7 / 32.2 (-30%)** | **1.267 / 0.670 (-47%)** | 36/39, 36/39 |

Across runs 3 to 5 (B against A), what holds steady:

| | Turns | Input tokens | Cost |
|---|---|---|---|
| Range | -21% to -30% | -47% to -55% | -40% to -48% |

Correctness is equal or better in every run. T4 (screen → API → store) is unchanged in correctness
and turns, and it misses the same store step every time.

---

## 2026-11-06 · 42. Making Claude batch edits across code files; mt only does what it is told

**User:** "we need to make sure that Claude really understands batch operations against code
files. Also a failed build does not revert, mt only complies with what it is told to do."

### Correction

Sections 39-40, and replies in this session, said "the gate rolls everything back on a failed
build" and "a failed build rolls everything back". That is wrong as stated: **mt never reverts on
its own.**
- It restores the files only when a step the batch itself marks as a gate fails (`@@ run gate …`,
  or `check:`, which is shorthand for one gate).
- A failing step without `gate` keeps the edits.

The skill's wording is fixed throughout:
- the `mt apply` heading is now "restored only if a step you marked as a gate fails";
- the no-scripts list says "a step you mark as a gate restores the files if it fails; mt never
  reverts on its own, it does exactly what the batch says";
- the bundle order line says the same.

### Evidence for "really understands"

From the last 30 days of transcripts (603 sessions):
- ~5,200 file-rewrite commands (Python replace-and-write scripts, `sed -i`, `perl -pi`), about
  8.4M characters of script;
- 50% of them changed at most one file with at most two replacements, each its own tool call;
- 356 bursts of 5 or more in a row, covering 3,910 commands. The longest: **104 rewrite scripts in
  a row** in fps-europe `826aead1`, 210 replacements across 79 files in 1.4 h;
- 70% never checked that a replacement matched;
- one script deleted functions by hard-coded line numbers across 13 files (gym-software); after
  the first cut, every later cut in the same file is off.

### The skill teaches batching

New section, "Batch edits across code files: one bundle, every file, every edit":
- **The rule:** "If you are about to make a second edit call (another `mt apply`, an Edit, a
  script), stop: it belongs in the same bundle."
- **How a bundle behaves:**
  - every op addresses the file as it was read, so many edits to one file and edits to many files
    never shift each other's lines;
  - nothing is written until every op is valid (no match, several matches, a changed hash, or a
    wrong `expect` count refuses the batch and says which op);
  - mt does exactly what the batch says, and only a failing `gate` step restores the files.
- **Addressing a site, best first:** `replace <span> <hash>`, then `find`/`with`, then
  `rename`, then `regex … expect N`.
- **A worked example:** the 104-script burst as one bundle (claims; rename; span replace; a regex
  across `src` with `expect`; find/with; two creates; the formatter; build as gate; unit tests).

The mt ON rule carries both points: "About to make a second edit call? It belongs in the same
bundle", and "mt restores the files only when a step you marked `gate` fails."

### The hook enforces it (`src/hook.rs`)

- **`edit_script(cmd)`.** A Bash command that rewrites **code** files is refused in strict mode,
  or advised in nudge mode. That means: a Python/Node/Perl/Ruby script that replaces and writes,
  `sed -i`, `perl -pi`, or either through `find -exec`/`xargs`.
  - **Code** is what `is_code` says: a programming-language grammar, not config or doc.
  - Scripts writing data, docs or generated assets, or only `/tmp` and the scratchpad, pass.
  - The refusal shows the bundle form. A simple `s/old/new/` becomes the exact
    `@@ regex files … expect <N>` / `@@ with`, via a hand-written parser, because Rust's regex
    crate has no backreferences.
- **Edit runs.** A per-session counter of consecutive `Edit`/`Write`/`MultiEdit` calls on code
  files. From the second one in a row, the hook adds the reminder: "This is edit call N in a row.
  Put every remaining edit … in ONE `mt apply` bundle". Edit itself stays allowed. The plugin's
  PreToolUse matcher gained `Edit|Write|MultiEdit`.

**Checked:**
- **Unit test** `edit_scripts_are_redirected_and_others_are_not`:
  - redirected: `sed -i` on two `.go` files (with the exact `@@ regex files a.go,b.go` hint),
    `xargs perl -pi`, `find -exec sed -i`, a Python replace-and-write on `.ts`, a Node
    `writeFileSync`;
  - left alone: `sed -n` reads, a Python JSON generator writing `art/out.json`, `sed -i` on
    `.md`, a script on `/tmp/x.go`, and `mt apply` itself.
- **Replay of 400 randomly sampled real rewrite commands from the transcripts** through the built
  hook: **323 redirected (81%)**. The 77 passed are mostly edits of Markdown specs and data files,
  as intended.
- `cargo test`: 59 + 5 pass.
- The frozen run plugin has the binary, the skill and the new matcher.

---

## 2026-11-06 · 43. Map, then expand once: IDs instead of contents, expansions kept in memory, the whole reference inline

**Rule (user):** mt returns expand IDs, not contents, so the model asks for every body it needs in one
`mt expand` call, never by writing a script. The daemon keeps what those IDs point at in memory, so
the expansion is near instant. Relations ("tested here, here, there") are part of the answer.

**Built.**
- **Map view:** `mt context X --map` and `{"view": "map", ...}` in `mt query`. Each item is one line:
  where, size in lines, hash, why it is here (`called by`, `calls route … served by …`, `named in the
  signature of [1]`). A `## relations of [1]` line groups the numbers: called by / unit tests /
  integration tests / types it uses / calls / data / docs. No bodies, except spans asked for.
  `mt context WriteAudit`: 3,454 chars as a map vs 13,424 in full (-74%).
- **`src/precache.rs`:** when a reply hands out numbers, the daemon keeps each item's lines in memory
  per session. `mt expand` asks the daemon first; an item is served from memory only while its
  file's (mtime, size) is unchanged, else read from disk and hash-checked (MOVED / CHANGED). Each
  file's identifier set (resolution evidence) is cached per file version too.
- **`expand` skips the daemon's sync barrier** (it needs no graph). The first version then read the
  daemon's in-memory texts (the other session's `texts.rs`), which the watcher had not yet
  updated: a **stale read**, caught by the new test. `expand` now reads the disk directly on a
  memory miss, so it never depends on the watcher.
- **Latency, measured:** `mt expand 1 3 14` 5.7 ms through the daemon from memory, 5.6 ms in-process
  from disk, 5.7 ms for `mt --version` alone. Process start is the floor; the map itself is 11 ms. The
  win is tokens and calls, not milliseconds: the model's own turn is seconds.
- **Edges:** relations are derived per query, with the resolution of section 39, and each file's
  evidence is cached in the daemon. A persisted raw-edge table waits on an index schema change
  shared with the other session.
- **The reference reaches the model whole.** The skill grew to 23k (other sessions), which is 3 parts,
  and a one-prompt run never got part 3 (`mt apply`, large output and JSON, Errors). Now: part 1 at
  session start, part 2 with the prompt plus a map of the rest (`mt ref <section>`, new: one section of
  the reference), and the rest **with the first tool results** (PostToolUse `additionalContext`,
  verified: 9,000 chars inline). Simulated 3x: 315/315 skill lines, every answer under 10,000 units
  (9,082 / 6,215 / 9,149). Real session (Haiku): after one `ls` it answered a part-3-only question
  correctly.

**Workflow benchmark, a third arm** (map, then one `mt expand` of exactly the items the task needs):

| Task | graft: calls · tokens · complete | mt inline: calls · tokens · complete | mt map+expand: calls · tokens · complete |
|---|---|---|---|
| callers of decodeJSON, with code | 7 · 4,798 · 19/19 | 1 · 6,045 · 19/19 | 2 · 6,422 · 19/19 |
| blast radius of WriteAudit (depth 2), with code | 4 · 2,530 · 3/28 | 2 · 11,438 · 28/28 | 2 · 8,156 · 24/28 (+1 expand for the last 4) |
| every use of AuditEntry, with code | 13 · 6,263 · 7/7 | 2 · 9,091 · 7/7 | 2 · **3,634** · 7/7 |
| read requireMachine | 9 · 1,250 · 1/1 | 1 · 507 · 1/1 | 1 · 507 · 1/1 |
| how is an audit row written | 6 · 993 · 2/6 | 2 · 5,170 · 6/6 | 2 · **3,571** · 6/6 |

- The map is cheapest when a task needs some of what is found (AuditEntry -60%, "how" -31% vs
  inline). When it needs every body, inline is cheaper by the map's own lines (decodeJSON +6%).
- The blast-radius map lists all 28 in 1,124 tokens. Expanding all 28 bodies (~9k tokens) is more than
  one tool result carries (~7k tokens of code), so the last 4 are named for one more expand.

**Tests:** `tests/map.rs` (a real daemon, 3 rounds: map without bodies, expand from memory, an edit
gives MOVED from disk, a body edit gives CHANGED; query map keeps requested spans inline);
`tests/briefing.rs` (every skill line delivered, every answer under 10k); `precache::tests`.
`cargo test`: 62 unit + 1 + 5 + 2 integration.

A test-harness trap (not mt): zsh's `echo` turns `\n` inside a JSON string into a real newline, so
hand-fed hook input was invalid JSON and the hook (correctly) did nothing. Use `printf '%s'`.

---

## 2026-11-06 · 44. The virtual context: codes instead of code, `mt show` for the human, the working set after a compaction

**User:** mt is the model's *virtual context*, like React's virtual DOM: the context window is the
expensive render target. Can the model feed short codes back to mt, so mt does the large printing?

**Evidence first** (34 recent real sessions, 2,119M input tokens; each call's context growth split
across what was added, carried until compaction): prefix 24%, the model's **own** output re-read
~34% (text 15.6%, tool-call inputs 12.5%, Write 4%, Agent prompts 2.3%), injected text ~22%,
**all tool results ~15%** (Read 2.4%). The biggest slice is what the model writes, not what it reads.

**Built** (`src/vctx.rs`; small hooks in `apply.rs`, `hook.rs`, `main.rs`, `compress.rs`):
- **Codes in `mt apply`.** `#3` (or `3`) in any span field is item [3] of the session's last
  query/context reply, with its hash: `@@ replace #3`, `@@ delete #3`, `@@ find f in #3`. The
  session comes from the client's environment in the daemon (`client_session`).
- **`@@ copy|move <span|#n> to <file|span|#n>`** (JSON `{"copy"|"move": ..., "to": ...}`): mt writes
  the code; a file destination appends (or creates), a span inserts after it; move deletes the
  source. Hash-checked, all-or-nothing like every op.
- **`mt show <file>|- [--title T]`**: the PostToolUse hook sends the text to the user as a
  `systemMessage` and gives the model a one-line receipt. Verified on Claude Code 2.1.319 (headless,
  stream-json): the message is shown to the user and never reaches the model (a secret word in it
  was unknown to the model); it arrives whole up to ~9k characters, and past the 10k hook limit it
  becomes a 2 KB preview, so mt sends the first ~8.5k and names the file with the rest.
  The Bash tool display is *not* a channel: the transcript's `toolUseResult` holds the replaced
  output too.
- **Working set after a compaction**: SessionStart `compact` lists, before the seen reset, the files
  `mt apply` changed (new `.written` log, kept across compactions), the spans mt showed, and whole
  files read, newest first, as handles for `mt expand` (≤ 2,500 chars). It also deletes the `.ptrs`
  table: `#n`/`mt expand n` must not resolve to items the model can no longer see.
- **Skill**: "Write less: refer by code, let mt print" (codes, `mt show`, no narration, working set).
  Still 4 parts, each under the 10k limit (5,647 / 8,445 / 1,678 after part 1 at 9,072).

**Checked:** `vctx::tests` (codes, show round trip and cut, working set order). End to end in a
scratch repo, in process and through `mt serve`: copy appends, move creates the file and deletes the
source, a stale `#1` is refused ("past the end of the file"), fake hashes are refused with nothing
written, an unknown `#9` names the problem. Headless Haiku: `mt show` of a 40-row table, the user's
message had all 40 rows, the model got the receipt.

**Config (the user's, not mt):** the graft hooks were registered at user level and in this repo, so
each fired twice, and two Vercel plugins (0.30.0 and 0.50.0) each injected their context and skill
set. Removed the user-level graft hooks and disabled `vercel-plugin@vercel-vercel-plugin`. First
request 60,698 → 57,223 tokens (−3,475, re-read every turn).

**Not done:** an A/B of whether models actually use codes and `mt show` unprompted, and the cost of
the model's narration. (`tests/compress.rs::hook_reads_and_mcp` failed on the tree while section 43 was in progress; it passes since.)

**Section 44, follow-up: the test.** `tests/vctx.rs` (through the real binary and hooks, scratch
repo): numbered items → `@@ move #1 to c.go` → `@@ create e.go` (header) plus two moves into it, one
spelled `@@ move <span> <hash> to <file>` → a stale `#1` refused with nothing written → `mt show`
split (the user's message has row 30, the model's receipt has no data) → SessionStart(compact) lists
changed files, spans and the last reply's pointers, and `#1` no longer resolves. Passes; full suite
green. Two fixes it forced: `move` into a file the same batch creates appends to it (a new Go file
needs its package line first), and `@@ move|copy <span> [hash] to <dest>` (the form a model writes).
The working set now also lists the last reply's pointers (they are not in the seen log).

**Live** (`bench/vctx_live.sh`, Sonnet, 6 runs, a move task that never mentions codes): all 6 moved
the code correctly and passed vet and tests. The working set came back after `/compact` and the
model answered "what did you change" with one mt call. **Adoption of codes: 1 of 6** (run 2 tried
`@@ move <span> <hash>`, hit the parser gap since fixed, then retyped). The "Write less" section was
in context before the first call (UserPromptSubmit), so the gap is behaviour, not delivery, the
same pattern as section 13 (skill alone 1 in 4; a hook 4 in 4). `mt show` was not used: the table
had 5 rows, cheaper to type than a call. Retyped apply inputs were 704-860 chars; the move form is
~320.

**Section 44, lever 1: one clause in the per-prompt line** ("Never retype code that exists: `@@ move
<span> <hash> to <file>` / `@@ copy`, `#n`; into a new file, `@@ create` its header, then the
moves"). Same task, 6 Sonnet runs: **codes used in 6 of 6** (was 1 of 6), no run retyped the code,
6 of 6 correct with vet and tests passing. Apply input 321-436 chars (was 704-860), output tokens
mean 1,430 (was 1,688, −15%), task cost mean $0.1119 (was $0.1149, −3%; a small task, where the
saving has few turns left to compound). In one run the model guessed the package name (`util`), the vet
gate rolled the batch back, and its corrected batch used moves again.

**Released** (2026-11-06): `target-plugin` rebuilt from the tree (68 unit + every integration suite green), so `mt` on PATH (`~/.local/bin/mt` → `target-plugin/release/mt`) has codes, `copy`/`move`, `mt show` and the working set. The hook-side parts (the per-prompt clause, the user-only `mt show` channel, the working set at SessionStart) run only where the plugin is loaded (`--plugin-dir ~/orca/mt/plugin`); it is not installed in Claude Code itself. Housekeeping: `tests/compress.rs::hook_reads_and_mcp` calls `session-start` without `MT_SERVER_SCOPE=off`, so each run leaves an `mt serve` in a temp dir (two found and stopped).

---

## 2026-11-06 · 45. T4 was a scoring bug; the real gap was speed, fixed with a chain card

### The "unsolved" store step was never missed

T4 (find where the Sessions screen sends the state filter, where the API reads it, and where the
store applies it) had scored 6/9 in both arms of every run, always failing the store step. Reading
the 18 answers instead of the scores: every one cites the store correctly, as
`internal/store/sessions.go:42` (the SQL predicate in `floorSelect`), `:55` (`Store.ListSessions`)
or `:67` (where the states are bound).

The task asks for "file:line for each". The scorer also demanded the word `ListSessions`, which no
answer had a reason to write. The scorer now accepts `internal/store/sessions.go` at a line inside
`floorSelect` (L19-53) or `Store.ListSessions` (L55-88), or either name.

Re-scored against each run's own repo copies (`MT_RUNS` points `analyze.py` at a run directory; T3
checks the migration files each run created, which live only in those copies):

| Run | A correct | B correct |
|---|---|---|
| 3 | 39/39 | 39/39 |
| 4 | 39/39 | 39/39 |
| 5 | 39/39 | 39/39 |

So the open problem was speed. On T4, B saved tokens but no turns (run 5: 5.7 against 5.7).

### Two fixes tried and removed

- **One card per file** (so six `SessionState` methods can't take every slot): the bench fell on
  every set (independent 37 → 36 @3, test split 39 → 34 @k, held-out 50 → 42 @3), and T4 picked
  `Store.GetSession`. Deleted.
- **Component-first seeding** (section 41): deleted earlier.

### The chain card

T4's seeds were already good (`Server.listSessions` #2, the `Sessions` screen #7). Selection was the
problem: a cluster of `SessionState` methods reinforced itself in PageRank and took the three code
slots.

Everything the task needs was already in the graph, so mt now hands it over as one card. **When a
prompt spans layers** (it mentions at least two of: screen/UI words, API/endpoint words,
store/database words), mt takes the best route-linked definition among the top 10 ranked, whether
or not it won a card, and prints the path one hop per line, each with file:line:
- the client call sites (frontend, other services; tests and config left out);
- the route registration and the handler with its span;
- the handler's calls into other files: other directories first, at most 3 same-package helpers,
  with the tables each callee touches and the constants it uses (its SQL).

Callees are matched by **how the call is written**, which is the same in most languages:
- `s.d.Store.ListSessions(` only matches a method whose receiver type is `Store`;
- `r.Context()` matches neither `SessionState.Context` nor a plain function `Context`;
- a plain function matches an unqualified call, or one qualified by its own directory's name;
- calls stay within the caller's language (file extension), and a name defined in more than two
  places is skipped.

T4's chain:
```
web/src/screens/Floor.tsx:52  Floor  calls /v1/sessions
web/src/screens/Sessions.tsx:22  Sessions  calls /v1/sessions
internal/api/router.go:133  registers /v1/sessions → Server.listSessions internal/api/sessions.go:15-78
internal/api/sessions.go:56  Server.listSessions calls Store.ListSessions internal/store/sessions.go:55-88 (uses floorSelect internal/store/sessions.go:19-53)
internal/api/sessions.go:23  Server.listSessions calls mustScope internal/api/scope.go:145-151
internal/api/sessions.go:27  Server.listSessions calls parsePage internal/api/page.go:100-119
internal/api/sessions.go:32  Server.listSessions calls queryCSV internal/api/page.go:127-140
```

The same card reaches a bench miss I had written off: "the org policy settings panel saves the
wrong retention value in the database" gives `PolicyPanel` → `/v1/org/policy` →
`Server.updateOrgPolicy` → `Store.UpdatePolicy` (table `capture_policy`).

`MT_CHAIN=off` turns it off. Test: in `routes_cross_subprojects_and_change_plan`.

On the bench the chain changes nothing at @3 or @k: it carries no card heading, and few bench
prompts name two layers. Named anywhere in the code cards rises by 1 point.

### Paired A/B with the chain card

Run 6: 12 sessions per arm, 8.8 min, no cold starts. Every step:
`bench/tools/run6/analysis7.txt`.

| Mean | Turns | Calls | First req tok | Input tok | Output | Wall s | Cost $ | Correct |
|---|---|---|---|---|---|---|---|---|
| A-T1 / B-T1 | 5.3 / 4.0 | 4.3 / 3.0 | 58,222 / 38,098 | 332,430 / 160,090 | 1,930 / 1,576 | 34.5 / 26.4 | 1.109 / 0.616 | 9/9, 9/9 |
| A-T2 / B-T2 | 3.0 / 2.0 | 1.0 / 1.0 | 58,240 / 36,134 | 129,401 / 75,891 | 113 / 281 | 10.5 / 9.5 | 1.064 / 0.494 | 6/6, 6/6 |
| A-T3 / B-T3 | 7.3 / 4.7 | 6.3 / 3.7 | 57,776 / 39,114 | 443,997 / 200,511 | 3,145 / 1,887 | 50.1 / 30.8 | 1.335 / 0.720 | 15/15, 15/15 |
| **A-T4 / B-T4** | **6.0 / 4.7** | 5.0 / 3.7 | 56,041 / 37,531 | 395,828 / 197,610 | 2,770 / 2,053 | 60.2 / 43.3 | 1.313 / 0.724 | 9/9, 9/9 |
| **A / B, all** | **5.4 / 3.8 (-30%)** | 4.2 / 2.8 | **57,570 / 37,720 (-34%)** | **325,414 / 158,525 (-51%)** | 1,989 / 1,449 | **38.8 / 27.5 (-29%)** | **1.205 / 0.638 (-47%)** | **39/39, 39/39** |

T4, B against A, before and after the chain card:

| | Turns | Input tokens | Cost |
|---|---|---|---|
| Run 5 (no chain) | 5.7 against 5.7 | -35% | -40% |
| **Run 6** | **4.7 against 6.0** | -50% | -45% |

With the chain, B-T4's first call went straight to the file the chain named.

### Where this leaves the A/B

Five runs, 120 sessions. B is at least as correct as A in every run, 39/39 against 39/39 in the
latest. Turns -21% to -30%, input tokens -47% to -55%, cost -40% to -48%. Every task is now faster
with mt.

---

## 2026-11-06 · 46. Every agent is told: mt, and only mt, for code

**Rule (user):** Claude and every other model use mt for code (reading, searching, editing) and long
output, ahead of graft, graphify and grep/sed/cat/Read.

- **`mt init [dir] [--dry-run] [--remove]`** (`src/init.rs`): one block between `<!-- mt:start -->`
  and `<!-- mt:end -->` in CLAUDE.md (Claude Code), AGENTS.md (Codex, opencode, most others),
  GEMINI.md, and `.cursor/rules/mt.mdc` (always-apply, only when the repo uses Cursor). Written if
  missing, replaced in place if present, removed with --remove. Text outside the markers is left byte
  for byte. When `mt` is not on PATH, the block names the binary's absolute path. Tests: add, replace,
  remove, idempotence, other content kept.
- The skill's description and opening, the plugin's session-start briefing and `plugin.json` now
  say it: mt only, and where other instructions say another code tool comes first, mt takes precedence.
- `~/.local/bin/mt` → `target-plugin/release/mt`, so agents outside the plugin find `mt`. It is the
  same binary as the plugin's, since the daemon refuses clients from another build.
- Applied: `mt init` in this repo (CLAUDE.md, AGENTS.md, GEMINI.md created) and in claude_coach
  (CLAUDE.md and .cursor/rules/mt.mdc created; the block added after graft's own block in AGENTS.md
  and GEMINI.md). A second run: all unchanged. `~/.claude/CLAUDE.md` got an "mt: the code tool"
  section above the graphify notes.

**Still conflicting, left to the user:** the graphify-first hooks (`~/.claude/hooks/graphify-*`) and
graft's hooks and blocks still tell agents to use those first. The mt text says mt takes precedence,
but two "first" instructions in one context cost attention; turning those off is the user's call.

---

## 2026-11-06 · 47. Line-level dedupe, and an A/B that found two other problems

**User:** compress what mt returns without making the model think more. Then: what is the
difference between resending duplicates and referring back by ID, and does that cost more
thinking or cache hits?

### Measured first

21 real `mt query` replies from mt ON runs 2, 3 and 5: 354,334 characters, ~88.6k tokens.
- **What they held:**
  - code 70.0%;
  - notes, outlines and skeletons 7.3%;
  - line-number gutters 6.0%;
  - "why" text 3.9%;
  - result lines 3.6%;
  - other header text 3.1%;
  - paths 2.3%;
  - batch headers 2.0%;
  - hashes 1.0%;
  - item lists 0.9%.
- **Inside the code:** comments 20.2%, indentation 4.1%, blank lines 0.2%.
- **37.1% of the code lines had already been sent earlier in the same session.** The existing
  dedupe only catches an exact repeat of a span (path, range, hash), not overlaps such as
  `auth.go:L1-327` and then `L94-176`.

### Resending versus referring back

Each turn resends the whole context. New tokens are a cache write (/M, once); tokens already in
context are a cache read (/bin/zsh.20/M) on every later turn; output is /M (rates fitted in
section 16). A 4k-token duplicate with 20 turns left costs ~/bin/zsh.032 to write plus ~/bin/zsh.016 in reads,
grows the context for the rest of the session and brings compaction closer. A back-reference costs
~15 tokens. The original is in the cache either way.

A back-reference needs no extra thinking, with two exceptions, both handled:
- the earlier reply was compacted away: the record is reset on compaction;
- the file changed: the record is keyed by the file's content hash.

### Built

**Line-level dedupe** (`query.rs`: `Shown`, `load_shown`, `record_shown`, `file_hash`):
- **The record:** the session's `.ranges` file holds every line range shown whole, per file
  version (path + content hash). `mt query` inline bodies and `mt expand` both write to it.
- **Fully shown items:** one line: "(unchanged, already shown earlier in this session: it is in
  your context above; `mt expand N` repeats it)".
- **Partly shown items:** only the new lines, each already-shown stretch as one line ("… L100-120
  already shown earlier in this session (unchanged)").
- **Fresh again:** a changed file is a new version, so everything in it is sent again.
  `"fresh": true` sends everything. `MT_LINE_DEDUPE=0` turns it off (the A/B control); the
  older exact-span dedupe stays on.
- **Compaction:** the SessionStart hook deletes `.ranges` on compact or /clear, next to the
  other session's reset of `.ptrs` and the seen log.

**Item-number edits:** already built by the other session (`vctx::code` / `vctx::pointer`:
`#3` in an `mt apply` span, `@@ copy`/`@@ move`). Not duplicated; `@@ replace #1` checked.

**Checked:** a sub-span after a wider one gives one line; an overlap sends only its new lines;
`fresh` and a compaction reset send everything. Tests: 62 + 1 + 5 pass.

### A/B on G1 (mt ON, one run each)

| | Control (`MT_LINE_DEDUPE=0`) | Treatment (line dedupe on) |
|---|---|---|
| Turns / calls | 11 / 10 | 9 / 8 |
| Cost | **$2.574** | **$3.596** |
| Output tokens | 63.8k | 102.1k |
| Read replies | 9, 110,856 B | 5, 123,984 B |
| Line dedupe fired | (off) | **once** (one stretch, L764-777) |
| Result | 17 files; build, vet, tests, tsc pass | 12 files after the resend; build, vet, unit tests pass |

**The A/B does not measure the dedupe:** it fired once. This run read each region once (5 large
batches), so there was almost nothing to dedupe. The 37% repeat came from earlier runs that re-read
whole files, which the read guidance has since largely stopped.

**What actually made the treatment cost more: two other problems.**
1. **The skill handed the model a hash.**
   - The bundle (#6, 59,316 B) was refused: "op 7: internal/store/auth.go:L560-572 changed since
     it was read (hash 70ea483f …)".
   - `70ea483f` is the hash of **L560-567** of that file, the real `SSOStart` stub, and it was
     printed verbatim in the skill's own examples (`@@ replace …:L560-567 70ea483f`, and the
     `replace_file` example). The model widened the range and reused the example's hash; mt
     correctly refused.
   - **Fixed:** every concrete hash in the skill is now `<hash>` (including `4c2ed24b`,
     `06d9be72` and `9f2c41aa`). The skill states: "A hash belongs to exactly one line range:
     copy both from the same item of mt's reply (or write `#3`). If you widen or narrow the range,
     the old hash no longer applies … Never reuse a hash from another range or from an example."
2. **One wrong hash cost the whole bundle again.** The resent bundle (#8) differs from the refused
   one by **one line** (the hash), yet all 59,316 bytes went out again: ~29k output tokens,
   ~$0.58. That is the `mt apply --retry` gap from section 23.

### Next

- **`mt apply --retry`:** keep the last refused or failed bundle, and accept only the corrected
  sections (e.g. `@@ op 7 replace <path:Ls-e> <hash>`), so a one-line fix costs one line. Two runs
  now show the full resend; it is the biggest avoidable output cost left.
- **Re-measure the dedupe on a task that re-reads:** a long multi-step session, or one past a
  compaction.

---

## 2026-11-06 · 48. `mt build`, `mt rebuild`, `mt logs`

Asked by the user alongside `mt init`. Checked first with the three active sessions
(claude-coach-95, -98, -e6): none was building them. New `src/ops.rs`, with anchored lines in main.rs.

- **`mt build [dir]`:** the index up to date (incremental), then the daemon running and warm (it
  starts one and waits until it answers, up to 20 s). Prints the index stats and the daemon status.
- **`mt rebuild [dir]`:** stops the daemon (and waits for its socket to go), drops
  `.mt/index.sqlite` (+ `-wal`, `-shm`), indexes from scratch, starts a daemon on the new graph.
  `mt index --rebuild` alone left a running daemon serving the old graph. Kept outputs (`.mt/out`)
  and settings stay.
- **`mt logs [-n N] [--source events|serve|hooks|apply] [--session ID] [-f]`:** one view of what mt
  records: the daemon's events (`.mt/events.jsonl`, from claude-coach-95's events.rs, rendered
  one line each: when, kind, command, ok, ms, tokens, subject → header), the daemon logs
  (`.mt/serve.log` and the session daemons' `/tmp/mt-serve-*.log` that name this repo), the hook
  decisions (`/tmp/mt-hooks.log`), and the newest `mt apply` runs (`.mt/txn/*`, last lines of each
  step log). Times are relative ("12s ago"). It reads only the last 256 KB of each file, so it stays
  instant. `-f` follows events and hooks as they happen.
- **Fix (found by claude-coach-e6):** `tests/compress.rs` started a real `mt serve` in a temp dir on
  every run; its helper now sets `MT_SERVER_SCOPE=off`. Checked: 4 daemons before the suite, 4 after.

Test `tests/ops.rs` (a real daemon on a private socket): build indexes and starts the daemon; a second
build re-parses nothing and finds the same daemon; a request appears in `mt logs`; all four
sections show; rebuild stops, drops, re-indexes, restarts; the new daemon sees a new file.
All 7 suites pass. target-plugin rebuilt (`~/.local/bin/mt` links to it).

**Follow-up (user):** `mt logs` must be bound to the repo it runs in. The hook decisions were the one
section that wasn't: `/tmp/mt-hooks.log` is shared by every repo, and its lines name no repo. Each
hook event carries its `cwd`, so the hook now resolves that to the repo root and also appends to
`<repo>/.mt/hooks.log`, and `mt logs` reads only that file. The shared log stays, because `mt config
doctor` and `mt visualize` read it; claude-coach-95 was told. An event with no `cwd` (only test
fixtures) writes no repo log. Test: an event from this repo is listed, one from another repo is not.

**Follow-up:** claude-coach-95 switched `mt visualize` and `mt config doctor`/`path` to the per-repo
`<repo>/.mt/hooks.log` (`crate::hook::repo_log`), so the shared `/tmp/mt-hooks.log` is no longer
written at all: every mt log is now bound to the repo it belongs to. All suites pass; target-plugin
rebuilt.

## 2026-11-06 · 49. Does mt still win against proper context management? Four arms, and a post-compaction preload

**Question.** With good context-management habits (delegate exploration, read only what an edit
needs, keep big output out, compact early), is mt still worth it? And can mt help after a
compaction by putting back the code the model was working on?

**Task.** G1, the same prompt and DB note as every earlier G1 run: "I want to be able to add google
authentication, please evaluate and add it along with tests". Opus 5.5, fresh worktree each run,
project settings only, auto-memory off, `bypassPermissions`. Totals include subagent transcripts
(`cm_analyze.py`), at the fitted rates from section 16.

**Arms.**
- **OFF**: plain Claude Code (runs 1 and 2 from section 16's set).
- **OFF + CM**: plain Claude Code plus `cm-system.md` (delegate exploration to Explore/general
  agents with short summaries; read only the lines to edit; keep large output out; batch calls).
- **ON**: mt plugin (frozen copy), the four G1 runs from earlier sections (runs 3 and 5, A/B control and treatment).
- **ON + CM + preload**: mt plus the same `cm-system.md`, plus early auto-compaction
  (`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=10`: about 100k of the 1M window), plus the new preload.

**The preload (new, `hook.rs` `session_start`, `vctx::recent_spans`).** On `source: compact`, the
hook already listed the working-set handles. It now also runs `query::expand` on the 6 spans the
session read most recently with mt (`path:Ls-e@hash`, hash-checked), capped at `MT_MAX_OUTPUT=8000`
chars (about 2k tokens), under "## Preloaded after the compaction: the code you were working on,
as it is on disk now". It runs after the reset of the seen/ranges/pointer state, so what it shows is
recorded as shown and a later query returns pointers for it. `MT_PRELOAD=0` turns it off. Checked
on a fake compaction: the preloaded span came back, and the next `span` query for it returned a pointer.

**Results (mean per run).**

| arm | runs | turns (main+sub) | context processed | peak context | output | cost $ | wall s |
|---|---|---|---|---|---|---|---|
| OFF | 2 | 53 + 0 | 7.00M | 181k | 62k | 3.81 | 777 |
| OFF + CM | 2 | 32 + 9.5 | 3.90M | 184k | 85k | 3.90 | 860 |
| ON | 4 | 11.5 + 0 | 1.21M | 188k | 78k | 3.00 | 668 |
| ON + CM + preload | 2 | 16 + 0 | 1.29M | **96k** | 65k | 2.96 + compaction | 687 |

Per run:

| run | turns main+sub | processed | peak | cost $ | wall s | notes |
|---|---|---|---|---|---|---|
| OFF 1 (efc302f0) | 58+0 | 7,303,130 | 173,457 | 3.684 | 757 | |
| OFF 2 (8fb62418) | 48+0 | 6,697,013 | 187,689 | 3.943 | 796 | |
| OFF+CM 1 (08ea063e) | 42+19 | 5,196,473 | 156,393 | 3.805 | 817 | 1 subagent |
| OFF+CM 2 (da545a12) | 22+0 | 2,605,724 | 211,608 | 4.000 | 904 | no delegation, no compaction |
| ON 3 (a4dec39a) | 13 | 1,285,747 | 185,049 | 2.828 | 612 | |
| ON 5 (cd868398) | 13 | 1,444,488 | 184,781 | 3.013 | 637 | |
| ON A/B control (538d370e) | 11 | 1,027,896 | 169,090 | 2.574 | 606 | |
| ON A/B treatment (7b5ca891) | 9 | 1,079,088 | 212,275 | 3.596 | 816 | |
| ON+CM+preload 1 (371e8f9d) | 12+0 | 968,419 | 93,540 | 2.728 | 679 | 2 compactions (96.5k→14.3k, 153.7k→28.3k) |
| ON+CM+preload 2 (d79b775b) | 20+0 | 1,609,906 | 97,574 | 3.193 | 695 | 3 compactions (104k→14k, 129k→21k, 112k→22k) |

All four new runs are correct: `go build`, `go vet` and all 21 test packages pass. tsc is clean in
mt-eval-off; in mt-eval-on it reports the known TS2688 (`vite/client`, no node_modules in that worktree).
ON+CM+preload run 2 wrote four test files (`internal/api/sso_test.go`, `internal/config/google_test.go`,
`internal/store/google_sso_test.go`, `internal/store/google_unit_test.go`).

**Cost caveat.** The transcript does not record the compaction calls' usage (no usage on the
`isCompactSummary` message, no subagent file). They summarized 250k (run 1) and 345k (run 2) tokens
into summaries of about 3.5-4.3k tokens each. If Claude Code's prompt cache covers them, that is about
$0.2-0.3 per run; uncached it is about $1.0-1.4. So ON + CM + preload costs **about the same as ON,
not less**.

**What the data says.**
1. **mt still wins against context management alone.** OFF + CM against ON: context processed
   3.90M → 1.21M (−69%), turns 41.5 → 11.5 (−72%), cost $3.90 → $3.00 (−23%), wall 860 → 668 s (−22%).
   CM halved OFF's context processed (7.0M → 3.9M) but did not lower cost: output rose (62k → 85k)
   and the delegated subagent added 19 turns of its own.
2. **CM on top of mt buys a smaller window, not a cheaper run.** Peak context halved (188k → 96k),
   which matters on a 200k-window model or a long session. Context processed (1.21M → 1.29M) and
   cost did not improve; each compaction added about 50 s and an uncounted model call, and run 2
   needed 20 turns against ON's 11.5.
3. **The preload's hit rate was low on this task.** Looking at run 2's reads after each compaction:
   - after compaction 1, the 13 spans read were all code it had not read before (or other ranges
     of files it had read). No preload from history could have covered them;
   - after compaction 2, it re-read 2 spans it had seen. One (`internal/app/app.go:L186-206`, the
     block it was about to edit) was 7th by recency, just past the cap of 6.

   I suspected expansions by pointer number were missing from the seen log. A test showed they are
   logged, so that change was reverted.

**Odd behaviour.** ON+CM+preload run 2 ended with a `SendMessage` to another local session
("claude-coach-6c") reporting that it was done. Nothing in the prompt asked for it. It is
harmless here but would leak into other sessions on a shared machine. `--strict-mcp-config` does
not remove the built-in SendMessage tool.

**Next.** To raise the preload's hit rate: rank spans in files the session wrote (or applied to)
above plain reads, and raise the cap for small spans; then re-measure the post-compaction re-read
count. The bigger lever is still the one mt already pulls: fewer turns.

## 2026-11-06 · 50. `mt mcp`: MCP tool calls through mt, in batches, results kept for `mt out`

**Why.** On the code side, mt's gains came from batching (fewer turns) and from keeping large
output out of context. MCP calls had neither: one native call per turn, and the whole result
(analytics rows, error lists, API JSON) in context, re-read on every later turn.

**Constraint from the user: don't interfere with MCP permissions.** Rejected design: mt as the
only MCP server, with a generic `call` tool. Claude Code's per-tool rules would then see only
`mt_call`, and mt would have to re-implement the gate. Also rejected: running only allow-listed
tools. This user has no MCP allow rules and runs in `defaultMode: auto`, so nothing would run.

**Chosen design.**
- The call is a visible Bash command (`mt mcp call <server> <tool> '{args}'`, or a batch on stdin).
  Claude Code judges it like any command: the auto-mode classifier or a prompt sees the server,
  tool and arguments.
- mt loosens nothing. A tool covered by a `permissions.deny` or `permissions.ask` rule
  (`mcp__s__t`, `mcp__s`, `mcp__s__*`), or a server in `deniedMcpServers`, is refused with
  "call it natively". The native call then meets the user's rule exactly as written.
- The settings read are user, project, local and managed.

**What it does (`src/mcp.rs`, about 330 lines; `mt mcp list | tools | call`).**
- **Servers:** the same set Claude Code starts here:
  - local and user scope from `.claude.json`;
  - project `.mcp.json`, only if approved (`enabledMcpjsonServers`, `enableAllProjectMcpServers`,
    not in `disabledMcpjsonServers`);
  - enabled plugins (`.mcp.json` and the manifest's `mcpServers`).

  Configs get `${CLAUDE_PLUGIN_ROOT}` and `${VAR:-default}` expanded. stdio servers only:
  HTTP/SSE servers use the session's OAuth tokens, so they are listed as "native only".
- **`mt mcp tools <server>`:** each tool in one line, `name(req: type, opt?: type) — first sentence`,
  marked when a rule makes it native only. Naming tools gives their full schema. This stands in
  for ToolSearch, which loads whole schemas.
- **`mt mcp call`:** a batch in order, one server process per server for the whole batch.
  - `"$a"` / `"$a.items[0].id"` puts an earlier call's result (a JSONPath match) into a later call's
    arguments. Only ids of calls in the batch count as references; `"$5 off"` and `"$HOME"` pass as written.
  - A failed call fails only the calls that reference it.
  - Each result is JSON when it is JSON (`structuredContent`, or a text item that parses). It goes
    through `compress_and_store`, so a large one comes back reduced, with an id for
    `mt out <id> --json/--jq/"question"`.
  - Per call: `name` (store), `lines` (budget), `timeout`.

**Checks (live servers).**
- `mt mcp list` here: 17 servers, 8 callable (stdio), 9 native only.
- `mt mcp tools graft`: 6 tools in 206 ms, one line each. `headroom`: 3 tools in 642 ms.
- One batch of 5 calls on 2 servers took 4.5 s, of which headroom's start was most:
  - `graft_find_all "func "` returned 46,180 chars (~11.3k tokens) and was shown as ~1.9k tokens,
    kept as `d4c04cf2`. `mt out d4c04cf2 "outbox open sql"` found `outbox.go:L96 open`;
  - `headroom_compress`, then `headroom_retrieve` with `"$a.hash"`: the chain worked;
  - an unknown server failed, and the call referencing it failed with "no earlier call `x` succeeded".
- With a project rule `deny: mcp__headroom__headroom_stats` and `ask: mcp__graft`:
  - both calls were refused with the rule named; the unruled call in the same batch ran;
  - `mt mcp tools headroom` marked `headroom_stats` "native only".
- Unit test `references_resolve_and_rules_refuse` covers references, literals, deny, ask on a
  plugin wildcard, and `deniedMcpServers`. Full suite passes.
- Skill: a Commands row and a short "MCP tools: through mt, in one batch" section, in both copies.
  All hook parts stay under 10,000 chars (8.7k, 8.4k, 9.3k, 5.2k); the section arrives in part 4.

**Not done yet.**
- Measuring it: a paired A/B on MCP-heavy tasks (turns, input tokens, cost against native calls).
- Warm server processes in the daemon (npx servers take 1-3 s to start on each `mt mcp call`).
- Replacing large results of *native* MCP calls in `PostToolUse`: `updatedToolOutput` is verified
  only for Bash and Read on 2.1.318, and is untested on MCP tools.
- `posthog` (`npx mcp-remote`) was not exercised: without a cached token it may open a browser
  login.

## 2026-11-06 · 51. Logs and large output always through mt; big diffs outlined; an MCP nudge; `mt activity`, a status line and a live pane

**Skill (user request: every log and large output goes through mt).**
- A top-level rule, delivered at SessionStart: builds, tests, installs, docker/kubectl/vercel/gh logs,
  log files, `git log`, API/curl responses and MCP results go through `mt run` /
  `| mt compress` / `mt compress < file`, never `cat`/`tail`/`head`/Read, then `mt out`.
- An "Instead of | Use" table in the large-output section.
- Hook parts: 9.3k, 8.4k, 7.3k, 8.1k, 0.8k chars, all under the 10,000 limit.
- Note: `skills/mt/SKILL.md` and `plugin/skills/mt/SKILL.md` are one file (hard link, same inode).
  Edit one, never both in a loop.

**Two gaps the rule exposed, fixed in `compress.rs` and `hook.rs`.**
1. **Diffs passed through verbatim at any size.** `git log -p -50 | mt compress` returned all
   6,809,337 chars, and nothing was stored.
   - Fix: code and diffs stay exact up to `EXACT_MAX_CHARS` = 40,000 (about 10k tokens).
   - A larger diff becomes a **diff outline**: per commit (short sha, subject, `L` line), per file
     (`path +A -D (n hunks) L<line>`), totals, capped at the budget, kept for `mt out --lines/--grep`.
   - The same input now: 97,911 lines, ~1.7M tokens → 122 lines, ~1.8k tokens, kept as `3a47284d`.
     A small `git diff` still comes back exact.
2. **Log files printed with `cat`/`tail`/`head` counted as "exact" (code) in the Bash hook.**
   A path matching `.log/.out/.err/.jsonl/.ndjson` or a `logs/` directory is now compressed like a log.

**MCP adoption.** In the first MCP A/B pair (T1), arm B made the same native calls as arm A. The
skill alone did not steer Claude to `mt mcp call`. Added a PostToolUse nudge:
- after the first native call to a server mt can run, once per session, it names the server,
  gives a ready batch template, and gives the result size when over 8k chars;
- never for OAuth/HTTP servers or for tools the user's deny/ask rules cover.

The same branch already compressed large *native* MCP results through `updatedMCPToolOutput`.
The hook log shows it working in real sessions: `mcp__gh__list_checks` went from 44,521 to 758 chars.

**An invalid A/B, and why.** T1 and T2's arm B ran with every mt hook failing ("non-blocking
status code, no stderr"). `plugin-frozen/bin/mt` exited 137 (SIGKILL). Copying a new build *over*
a binary that had already run invalidates its code signature, and macOS kills it at launch.
- Fix: `rm -f` then `cp`, so the copy gets a new inode.
- `run_mcp_pair.sh` now refuses to launch when the frozen binary does not run.
- Not affected: the section 49 runs (run 1 shows the preload, and nothing was copied before run 2).
- Rerunning T1-T3.

**Activity: events, `mt activity`, a status line, a pane.**
- **Events.** `.mt/events.jsonl` held only daemon requests, reindexes and daemon starts. It now also
  records:
  - every lossy compression (`kind: compress`: what, store, id, chars in/out, session);
  - every `mt mcp` call (`kind: mcp`: server, tool, ok, ms, chars, session).

  Hook processes take the session id from their input (`MT_SESSION`).
- **`mt activity [--session ID] [--json | --line | --statusline] [--limit N]`.** It returns:
  - totals: requests by command with average ms, compressions, tokens kept out of context, MCP
    calls, reindexes;
  - the latest events, one line each, plus a ready status line.

  `--session` filters to one session; repo-wide events (reindex, daemon) show in every session's view.
- **`--statusline`** is Claude Code's `statusLine` command. It reads `session_id` and
  `workspace.current_dir` from stdin and prints nothing until mt has done something in that session.
  Settings: `{"statusLine": {"type": "command", "command": "mt activity --statusline"}}`.
- **The mod `mods/mt-activity`.** A hooks module, also in the session's dev-mods folder.
  - Its status-line entry is `$.ui.status(line)`, empty until there is activity.
  - The "mt activity" pane shows totals, per-command counts and the latest 60 events, coloured by kind,
    with time and ms.
  - It refreshes after every tool call and every 3 s, so reindexes between calls show too.
  - `/mt-activity [session|repo]` opens the pane and switches the scope.
  - It runs `mt activity --json`, so the logic stays in Rust and the mod only draws.
- **Checks.**
  - `claude plugin validate` passes (only directory-listing warnings); `tsc` is clean against this
    build's types.
  - `claude plugin test`: 2 tests on terminal, desktop, vscode and mobile (the status line and pane
    show seeded activity; a failing mt shows its error).
  - Rust unit test `activity_counts_one_session`.

---

## 2026-11-06 · 52. Plain-word search over code: `mt search`, and `{"search": ...}` in a batch

The last capability graft had and mt didn't (`graft ask`): find code by a question, no name known.

- **`src/search.rs`:** every definition is a document. Its name is split into words
  (`WriteAudit` → write, audit; `user_id` → user, id; `HTTPServer` → http, serv), weighted 4; its scope 1.5;
  its path 1; every word of its span (code, comments, strings) 1, up to 600 lines. A container (a class
  or impl holding other definitions) is its header only, since its members are documents of their
  own. A function's own locals are not documents. BM25, times the share of the question's words
  matched, times 1 + 0.4 per question word in the name, times 1 + 0.12·ln(1 + files mentioning it)
  (coupling, on the top candidates). Tests ×0.5 and docs ×0.35 unless the question is about them.
  Adjacent question words are also tried joined ("kill switch" → `Killswitch`). A small table
  covers irregular verbs (written → write) and the abbreviations code uses (organization → org,
  configuration → config, repository → repo, database → db …). Deterministic. Each file's documents
  are cached per file version in the daemon.
- `mt search "<question>" [--limit N] [--map]`, and `{"search": "…", "limit": N}` as a `mt query`
  op: the best three come with their code, the rest as numbered pointers for one `mt expand`.
- On this repo (no names given): "how is an audit row written" → writeAuthAudit, WriteAudit,
  auditWrite, auditRead (4 of the 5 writers, in the top 4). "authenticate the machine headers on
  ingest" → requireMachine. "list audit entries with pagination" → ListAudit, listAudit. "enroll
  a TOTP second factor" → the 5 enrollment functions in the top 5. "decode the JSON body of a
  request" → decodeJSON. Weak: "kill switch for an organization" finds the org-killswitch code, but
  `Server.setKillswitch` is #9, under types and tables that mention both words.
- Time: ~130 ms in-process on 295 files (1.4 s cold, first read of every file); cached in the daemon.

Tests: `search::tests::code_words`, `tests/search.rs` (5 questions on a fixture repo each find their
function first; locals never come back; a batch `search` op returns the best bodies). The skill's
Commands table and the `mt init` block mention it; both repos re-initialized.

## 2026-11-06 · 52. MCP A/B, valid runs (stopped early at the user's request)

**Setup.** Paired runs, side by side: A is native MCP; B is the mt plugin (frozen copy, current
skill as its system prompt, `MT_MCP_CONFIG` naming the same `--mcp-config`). Opus 5.5, read-only tasks.

- `mt mcp` gained `MT_MCP_CONFIG=<file>[:<file>]`, for servers a session got with `--mcp-config`.
- An MCP server that exits during the handshake is now reported with its exit status and the last
  lines of its stderr, instead of a 60 s "no answer".
- Fixture: `uvx mcp-server-sqlite` crashes on its current `mcp` library, and pinning older ones
  broke pydantic. The test uses a stdlib-only, read-only SQLite MCP server instead (scratchpad
  `fixture/sqlite_mcp.py`, the same three tools) over a generated shop database: 3,000 customers,
  200 products, 30,000 orders, 62,939 items, 2,368 refunds.

**graft tasks (T1-T3).** Hooks ran and the nudge was delivered, but B made only native calls. The
prompts say "use only the graft MCP server's tools", which B evidently read as ruling out
`mt mcp` via Bash.

| task | turns A/B | cost A/B $ | wall A/B s | tool-result chars A/B |
|---|---|---|---|---|
| T1 | 5 / 4 | 0.228 / 0.199 | 23 / 20 | 14,086 / 6,437 |
| T2 | 7 / 5 | 0.258 / 0.238 | 29 / 24 | 16,475 / 12,433 |
| T3 | 8 / 8 | 0.316 / 0.314 | 35 / 43 | 26,166 / 18,916 |

mt's hook compressed one native graft result (T3, 6,491 → 3,955 chars), and `mt activity` in that
worktree recorded it. The other differences come from different call choices, at noise level.

**Data task D1 (shop DB).** Both arms were correct: DE at 96/456 = 21.05%, the top products, the
customers.

| arm | turns | processed | cost $ | wall s |
|---|---|---|---|---|
| A | 8 | 345,530 | 0.287 | 52.9 |
| B | 8 | 439,030 | 0.396 | 47.6 |

- A made 9 native `read_query` calls.
- B made 4 native calls. mt's hook compressed a large one, and B queried it twice with
  `mt out <id> --jq`.
- B still never used `mt mcp call`, and cost more: +12k cache write, from the 26k-char skill
  system prompt and compression headers.

D2 and D3 were stopped before finishing.

**Reading so far.** The pieces work (compressing native results, `mt out` queries, nudges,
activity events), but **batching through `mt mcp call` is not being adopted**, so no saving is
shown yet. Two levers to test next:
1. A PreToolUse suggestion *before* the second native call to the same server, not after the first.
2. Measuring without the 26k skill system prompt, which costs more than these tasks can save.

## 2026-11-06 · 53. Indexing progress; lookup tables built in the background

**Progress.** Every indexing phase redraws one stderr line (only on a terminal): walking (files
found), parsing (done/total on N cores), writing (done/total), and single SQLite steps with a
ticking seconds counter. On watchly (5,073 files) the line never sat still for more than 0.36 s.

**Where a first index spent its time** (watchly copy, 5,073 files, 130,178 definitions, 1,319,011
reference rows, 10,906 table facts; Apple M-series, 18 cores): walk 0.1 s, parse 0.5 s, write
0.6 s, lookup tables 1.6-2.5 s. `write_ms` was reported as 0: it was never measured; it is now.

**Lookup tables, per index** (warm Python sqlite3 3.51, same DB, 7 `CREATE INDEX` in one tx):

| variant | total | refs_name | refs_norm | refs_file |
|---|---|---|---|---|
| as mt did it | 3.00 s | 1.10 | 1.43 | 0.31 |
| + cache_size 256 MB | 3.11 s | 1.18 | 1.41 | 0.31 |
| + PRAGMA threads=8 | 3.00 s | 1.17 | 1.33 | 0.29 |
| + mmap 1 GB | 2.81 s | 1.08 | 1.24 | 0.26 |
| + a stored `norm` column (not the expression) | 2.50 s | 1.07 | 0.97 | 0.26 |

Tuning buys at most 17%; the cost is sorting 1.3M reference rows. Not taken.

**Taken: the daemon builds them, after it answers.** The daemon answers from its in-memory graph
and loads the tables by rowid, so it needs none of the 7 lookup tables; only one-shot commands,
`mt tools` and the visualizer query through them. `Index::update_serving` indexes without
building them; the daemon's writer thread builds what is missing (`ensure_lookups`) once it is
serving, in line with its other writes. `Index::update` (every one-shot path, `mt index`) still
builds them, one by one with progress; an interrupted build is redone by whichever runs next.

| watchly, fresh repo | before | after |
|---|---|---|
| `mt build` wall time (2 runs each) | 4.75 s, 4.15 s | 1.82 s, 1.85 s |
| daemon's background build of the 7 tables | (in the foreground) | 2,338 ms |
| `mt context <fn> --map` during that build | | 129, 59, 58 ms |

94 Rust tests pass; `reindex_touches_only_what_changed` now also checks that `update_serving`
leaves 7 missing, `update` builds them, and `ensure_lookups` then has nothing to do.

## 2026-11-06 · 54. One server per worktree, bound to its folder; two builds no longer fight

**What broke** (fps_europe, 206,600 files, a 12.6 GB index): `mt query` failed with `database is
locked`. Evidence from `.mt/serve.log` and `events.jsonl`:
- three servers started in 2.5 min (47-50 s load each): one per Claude session (the default
  `MT_SERVER_SCOPE=session`, sockets in `/tmp/mt-s-<session>.sock`) plus the folder's own;
- two mt builds on the machine: Claude's own `mt` calls ran `~/.local/bin/mt` (`~/orca/mt`), the
  hooks the plugin's binary. A server answered any other build with "stale" and exited, so the
  two builds took turns stopping each other's server;
- each refused caller ran in-process, whose `Index::update` writes; with a server persisting (and
  building lookup tables on a 12.6 GB DB) the 5 s busy timeout ran out: `database is locked`;
- lookup tables were left 4 of 7, every server being stopped part way through building them.

**Changed:**
- one server per worktree, bound to its folder (`.mt/mt.sock`; a deep path gets a socket named
  by a hash of the folder, still that folder's alone). SessionStart only makes sure it runs and
  waits up to 3 s; SessionEnd no longer stops it (other sessions there use it; it exits when
  idle). `MT_SOCKET`/`MT_SOCKET_ROOT` and per-session servers are gone; `MT_SERVER_SCOPE=off`
  remains for tests;
- builds compare by size and mtime, not path (the same binary in two plugin version folders is
  one build). Another build still gets "stale" (every mt build then answers in-process), but the
  server exits only for a newer build: an older one can never stop it;
- `Index::update`, when the server holds the write lock past the timeout, answers from the index
  as it stands (a note on stderr) instead of failing.

**Checked** (watchly copy): two SessionStarts, one server; five `mt context` calls from the old
`~/.local/bin/mt` build answered, the server untouched; a new-build call and a SessionEnd, same
server; `serve --stop` stopped it. 95 Rust tests pass (new: `one_build_in_two_places_is_one_build_and_newer_wins`;
`tests/map.rs` and `tests/ops.rs` now use the folder's socket).

**Next: memory.** A server holds every file's text and the whole graph (defs with a path string
each, refs as a string map per file, name maps): ~993 MB RSS on gym_software (9,721 files, 133 MB
of source), 7.5 GB of texts alone on fps_europe. The goal is a few hundred MB on any repo.

## 2026-11-06 · 55. The server's memory: from 1.2 GB to 36 MB, answering from SQLite

**Goal** (the owner's): a few hundred MB at most, on any repo.

**Where the memory was** (gym_software copy: 9,721 files, 133 MB of source, 417k defs, 2.68M refs;
`footprint`, physical): the server held every file's text (`texts`), the whole graph (defs with a
path string each, refs as a string map per file, name maps), and plain-word search documents for
every definition once anything was searched (`search::cache`). Attribution after phase 2a, by step:
started 7.5 MB; after 20 `context --map` 33 MB; after a repo-wide grep 33 MB; after the first
`mt search` **438 MB**.

**Changed:**
- 2a: the server answers through `Index` (SQLite, the lookup tables) instead of `graph::Graph`;
  `query::run`/`context::run`/`apply` were already generic over `Store`. The reindexer writes each
  change before it acknowledges waiting requests. File text is read from disk on demand (the OS
  caches it, outside the process). `graph.rs` and the in-memory texts are deleted. The lookup
  tables are built before the server answers again (it now needs them).
- 2b: plain-word search keeps each definition's weighted words on disk (`search_post`: term,
  def, tf, len, with a covering index; `search_doc`: def, len), computed while parsing by the
  same code as before (`search::terms`, `postings_for`). A question reads only its own words'
  postings; BM25, the matched-share and name/role factors and the coupling boost are unchanged.
  The name/role factors apply to the best 500 by text score (`ponytail:` note in `search.rs`).
  Schema 6.
- 2c: `context::gather` preloaded every test file as lines and, per route, cloned each file's
  lines and built three strings per line (93% of a 3 s call in `sample`). Now one parallel pass
  over the test files' text; only files naming a route are split into lines.

**Measured** (`bench.sh`: build, 20 `context --map`, 10 query batches incl. a repo-wide grep, 4
searches, 8 parallel `context`; then the server's physical footprint):

| gym_software | before | 2a | 2b | 2c |
|---|---|---|---|---|
| physical footprint | 1,212 MB | 508 MB | 142 MB | **36 MB** |
| `context --map` p50 / max | 27 / 3,956 ms | 28 / 2,514 | 35 / 6,498 | **30 / 122** |
| query batch p50 | 22 ms | 62 | 66 | 64 |
| search p50 / max | 122 / 5,279 ms | 95 / 4,666 | 27 / 36 | **27 / 34** |
| 8 parallel `context` | 3,483 ms | 2,609 | 1,257 | **76** |
| fresh `mt build` | 4.8 s | 8.6 | 14.2 | 13.8 |
| index on disk | 297 MB | | 528 MB | |

watchly (5,073 files): 495 MB → 39 MB; 8 parallel 1,801 → 48 ms; `mt build` 2.6 → 4.6 s.

**Worse, and next:** a fresh index is ~3x slower and the index ~1.8x larger (4.5M postings rows,
the lookup tables now built up front); a query batch with a repo-wide grep is ~3x slower (it reads
files from disk). **Found, not fixed:** `context --map` output is not deterministic run to run
(the order of table items, from a HashMap): 24 lines differed between two runs of the old build.
95 Rust tests pass (twice). One run of the suite had `hook_reads_and_mcp` fail; it passed alone
3/3 and in two later full runs: a flake under parallel tests, not yet explained.

## 2026-11-06 · 56. Indexing in batches; fewer lookup tables; faster word splitting

**Indexing memory.** A full index held every parsed file until it wrote (`mt index` on
gym_software peaked at 732 MB physical). It now parses and writes `BATCH` = 2,000 files at a time
(`scan_batched` with a sink; `refresh` of the whole tree returns no parsed files). Peak: gym_software
732 → **378-389 MB**, watchly 340 → **179-181 MB**. It no longer grows with the repo, only with the
largest 2,000 files.

**Lookup tables** (gym_software, each `CREATE INDEX` timed): search_term 3.81 s, refs_norm 2.76,
refs_name 2.39, search_def 1.12, refs_file 0.71, the rest 0.44; 11.2 s in all.
- `refs_norm` (an expression over 2.4M rows) is replaced by a `names(name, norm)` table, one row
  per distinct identifier, written with the refs; `spellings` reads it. Its norm is `index::norm`
  on both sides (the old SQL dropped `_` only, `norm` also `-`, so a dashed name never matched).
- `search_def` is gone: a file's postings are one block of rows, recorded as `files.post_lo..
  post_hi` and deleted by rowid range (the range is cleared on delete, so reused rowids are never
  taken for it). Schema 7.

**Parse time.** `search::postings_for` compared every pair of definitions (nesting) and `words`
allocated per character and scanned ~90 stop words linearly. Nesting is now one sort and two passes
(`nesting`, checked against every pair on 400 random layouts); `words` splits ASCII on bytes and
looks stop words up in a set (checked equal to the char path). Parse on gym_software 3.3-3.8 s →
2.4-2.7 s. Overlap of tokenized lines is only 1.23x, so a per-line cache was not worth it.

**Not better yet:** a fresh `mt index` of gym_software is still 14-16 s (machine load average 14 on
18 cores during these runs: the walk alone varied 0.15-1.3 s, so these numbers are noisy). Index on
disk 528 → 448 MB. The writes of 4.5M postings and the covering search index remain the cost.
97 Rust tests pass.

## 2026-11-06 · 57. Everything git keeps is indexed, at launch; git alone decides

**The owner's rule:** index everything that is not in `.gitignore`, at launch. Before, the walk also
skipped hidden files and folders (`.github/workflows` among them), 21 folder names whether ignored or
not (`node_modules`, `vendor`, `dist`, `build`, `target`, `bin`, `out`, `obj`, `Library`, `Temp`,
`Logs`, ...), 80 extensions (`.lock`, `.csv`, `.log`, Unity `.meta`/`.prefab`, ...), files over
1.5 MB (not even listed), and honoured ripgrep's `.ignore` files.

**Now:** one walker (`index::walk`) follows git only: `.gitignore` at every level (and in parents of
a walked folder), `.git/info/exclude`, the global excludes; hidden files included; never `.git`,
`.hg`, `.svn`, `.jj`, `.mt`. Every file it yields is listed: media, archives and compiled files
(`BINARY_EXT`) and files over 1.5 MB are recorded without being read; text types are parsed like
any other. The server checks each changed path with `Index::ignored` (each folder's `.gitignore`,
deepest first, first decision wins, then `info/exclude` and the global excludes; rules cached per
folder) and walks again when a `.gitignore` changes. The SessionStart hook already started the
worktree's server, which indexes before it answers: that is "at launch".

**Checked:** `git_decides_what_is_indexed` (nested `.gitignore`, `!` re-include, an anchored
`/secret.txt`, `info/exclude`, `node_modules/` and `bin/` not ignored, a binary listed); live: a
fresh repo's SessionStart indexed `.github/`, `bin/`, `src/` and not `build/`; a new file in `bin/`
was picked up and one in `build/` was not; adding `bin/` to `.gitignore` removed its files.
98 Rust tests pass.

**Cost to expect:** repos that commit dependencies or generated folders (not gitignored) now index
them too; that is the rule, and it makes the size and speed work (dedupe by content next) matter more.

## 2026-11-06 · 58. A `[mt] work:` line on every answer; a status line like graft's

**Asked:** a status line like graft's, and every answer saying how much work mt did, as graft's
packs end with `[graft] tokens saved ≈ N (P%); this pack ≈ X tok vs reading the K file(s) whole ≈ Y tok`.

**The line** (`work.rs`): every `query`, `context`, `expand` and `search` answer, through the server
or in-process, ends with `[mt] work: <index lookups> (<rows>) · <files read> (<KB>) · <ms> · this
reply ≈ R tok vs ≈ W tok to read the K files it covers whole: ≈ S tok saved (P%, estimate)`.
Counters are per thread (a request is one thread) and worker threads hand theirs back (`snapshot`/
`absorb`), so parallel parts are counted and concurrent requests never mix. JSON output gets no
line. Each line also records a `saved` event, which `mt activity` sums (`tokens_saved`).

**The status line:** `mt statusline` reads Claude Code's JSON and prints two lines:
`◤ mt · 6,974 files · ✓ server up · 4 calls · ~436.0k tok saved` / `▸ ctx 41% · last: query search
"close the account" (322 ms)` (a batch's JSON is shown as its operations). It opens the index read
only and never creates one. The plugin's status entry (`◤ mt · …`) now shows from the session's start.

**What the line exposed at once:** `context` read 1,766 files (35 MB) per call on gym_software. Two
places: route detection read every file referencing each of up to 40 caller names (now each file
read once, on every core, looking only at the reference lines; same routes, same order), and the
route-in-tests match rebuilt a substring searcher per line per route per test file (now the whole
text is checked once per route first; test files are also narrowed by a route's inner path word in
the refs index). `context listValueAccounts`: 1,289 → ~390 ms. It still reads ~1,800 files, honestly
counted: the next thing to cut. Outputs compared on 13 names against the build before: identical
but for database-table items, which differ between two runs of the old build too (known
nondeterminism, section 55). 100 Rust tests pass; the plugin's 2 tests pass.

## 2026-11-06 · 59. Routes from the index: `context` reads only what it shows; answers deterministic

**Routes without reading files.** `context` found route registrations by reading every file that
references each of up to 40 caller names, and route tests by reading test files: ~1,800 files
(36 MB) per call on gym_software. The parser now records, per line, in `lits(file, line, kind,
text)`: `route` (what `route_rx` captures, as `context` reads it) and `path` (each quote followed by
`/`, with the longest run of path characters after it). A test line holds `"<route>` (or `'`, `` ` ``)
exactly when one of its runs starts with the route, since a route is path characters only: a
prefix range on `lits_text`. `Store::routes_at`, `Store::quoted_paths`. Schema 8. gym_software has
13,346 route rows and 35,247 path rows.

`context listValueAccounts`: 53 lookups, 1,800 files read, ~1,060 ms → 257 lookups, **20 files read
(the ones it shows), 10 ms**.

**Determinism.** Comparing builds first failed: neither build answered the same twice (3 and 4
distinct outputs in 4 runs). Two HashMap iterations: the callers' `groups` (pushed in random order,
so ties broke differently) and `touched.iter().take(6)` (which 6 tables were shown). Both ordered
now (`BTreeMap`, `BTreeSet`); 1 distinct output in 4 runs on 5 names.

**Checked:** `47f8cce` with the same two ordering fixes against this build, 13 names on gym_software:
13 of 13 answers byte-identical. 99 Rust tests pass (section 58 said 100: a miscount, it was 99).

## 2026-11-06 · 60. Indexing: every step shown at once; parse and write overlap; fresh builds journal-free

**Asked:** indexing far faster, with every step and its progress shown at once.

**Shown:** one board on stderr (a terminal only), a line per step with a bar and its time, redrawn in
place at most 10 times a second; finished steps stay:
```
✓ walk     6974 files, 6974 to parse  0.0 s
✓ parse    ████████████████████████ 6974/6974 files · 18 cores  2.2 s
✓ write    ████████████████████████ 6974/6974 files  4.0 s
✓ lookups  ████████████████████████ 9/9 lookup tables  5.6 s
```
**Overlap:** a full index parses batch k+1 on every core while a writer thread writes batch k (a
channel of one batch, so memory stays ~2 batches).

**Fresh builds:** an empty index (first run, new schema) is built in `index.sqlite.new-<pid>` with no
journal, `synchronous=OFF` and an exclusive lock, then the old file's `-wal`/`-shm` are removed and
the new one is renamed into place and reopened in WAL mode. An interrupted build is only that file
(removed by a later build): never a half-written index. `mt build` now always builds the lookup
tables before the server starts (it answers through them).

**Measured** (gym_software, 6,974 files; the machine at load 8-15 from other sessions, so wall
times swing; CPU time is steadier): before, three runs 17.0 / 20.6 / 28.6 s wall (CPU ~24-26 s);
after, **9.3 / 9.3 / 9.4 s** wall (CPU ~22-23 s). The saving is waiting on the write-ahead log, not
computation. What remains is SQLite on one thread: write 4.0 s, lookup tables 5.6 s.
99 Rust tests pass.

## 2026-11-06 · 61. Search postings packed per word and file; references read by row range

**Rows.** gym_software: 4,505,476 search postings (one per word and definition) but only 1,106,582
(word, file) pairs, and 2,449,249 reference rows. `search_post` is now (term, file, posts): one row
per word and file, its postings packed as (def i64, tf f32, len f32). Its lookup is on `term` alone.
A file's references, like its postings, are one block of rows (`files.refs_lo..refs_hi`), so the
`refs_file` lookup table is gone; `tools.rs` reads a file's references by that range. Schema 9.

**Checked** against `07ac27f` (gym_software, a fresh session each run, timings stripped): 8 of 8
searches and 6 of 6 `context` answers identical. 99 Rust tests pass.

**Measured:** index 448 → 336 MB; write 4.0 → 3.0 s; a fresh index 6.2 s in one run, 9.5 s in
another (load 13). The lookup tables (~6 s) are the largest step left.

## 2026-11-06 · 62. References store their name as a number

`refs_name` sorted 2.45M name strings (2.1-2.3 s, the largest lookup table left). A reference now
stores `names.id` (`refs_ids(file, name_id, lines)`, an integer index); a `refs` view joins the
names back, so the SQL in `tools.rs` and `viz.rs` is unchanged, and SQLite answers `name = ?`
through the unique name index and the integer one. Names get their number in the same write
(one upsert `RETURNING id`, cached per batch). Schema 10.

**Checked** against `07ac27f`: 6 of 6 searches, 6 of 6 `context` answers and a `mt tools find`
identical. 99 Rust tests pass. gym_software fresh index **5.4 s** (from 17-29 s this morning,
9.3 s after section 60, 6.2 s after section 61); index 304 MB (from 448).

## 2026-11-06 · 63. Fix: an index from before schema 10 would not open; `mt index add/remove/list`

**Bug** (reported from `install.sh` on gym_software): `cannot open the index: use DROP TABLE to delete
table refs`. Schema 10 made `refs` a view; the upgrade ran `DROP VIEW IF EXISTS refs` first, and
SQLite refuses that when `refs` is a table, IF EXISTS or not. Every index made before schema 10 failed
to open. The tests only ever built fresh indexes. Now `refs` is dropped as what `sqlite_master` says
it is. Reproduced with a copy of the real gym_software index (schema 5): `70c586e` fails with that
error, the fix rebuilds it (8.5 s) and starts the server. New test:
`an_index_of_an_older_schema_opens_and_is_rebuilt` (a schema-9 index with `refs` a table opens, is
rebuilt from the tree, and opens again).

**`mt index add <dir>...` / `remove <dir>...` / `list`:** folders indexed although git ignores them,
kept in `.mt/include` (per worktree). The full walk adds every file in them (ignore rules off);
`Index::ignored` says no for anything under one, so the server's per-path checks agree; the server
re-reads the list on every walk, and `add`/`remove` ask it to walk (`touch`), or index in-process
when no server runs. Checked live, with and without a server: a gitignored `build/` comes in and goes
out, and `context` finds a function in it. New test: `an_added_folder_is_indexed_although_git_ignores_it`.
101 Rust tests pass.

## 2026-11-06 · 64. `MT_DEBUG=1`: a trace of everything indexing does, for analysis

**Asked:** a debug argument that dumps everything mt does to a file, to send back for analysis.

`MT_DEBUG=1` on `mt build`, `mt index`, `mt rebuild` or `install.sh` (or `mt index --debug`) writes
`.mt/debug/<command>-<pid>.jsonl`, and the server it starts writes its own (`serve-<pid>.jsonl`; it
inherits the variable). One JSON event per line (`trace.rs`): `start` (cores, load average, mt build,
OS), `span` (walk; each parse batch; each write batch with its parts in ms: files, defs+docs,
postings, names, refs, tables+lits, commit, and row counts; each lookup table; the fresh build; the
server's start), `note` (rows per table, index file sizes), `summary` (files parsed, the 40 slowest
files with language, bytes, ms and definitions, totals per language). Off, each call is one relaxed
load. 101 Rust tests pass.

**What the first trace shows** (gym2, 6,974 files, 6.35 s fresh index): write batches 0.25-1.57 s,
of which refs (with names) 0.17-1.02 s and new names alone 0.08-0.50 s; lookup tables 2.15 s, of
which `refs_name` 1.54 s (2.45M rows, integers now) and `search_term` 0.40 s; one file,
`leads_scripts/raw/crossfit_ca.html`, takes 700 ms to parse and holds its batch to 1.2 s.
