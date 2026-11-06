# mt history: the development log, condensed

`DEVLOG.md` is the full record of how mt was designed, built and measured: every trial, number, bug and dead end, with raw transcripts and command output. It is long because it is the evidence. This file is the readable companion: one row per section with its outcome, then two or three lines per section with the key numbers, each linking to the full section in [`../DEVLOG.md`](../DEVLOG.md). Numbers were measured on one machine (macOS, Apple Silicon) against private test repositories and real Claude Code sessions unless marked as modelled. **Status** says whether the work was kept, reversed, or is a measurement.

## Contents

| # | Date | Section | Status | Outcome |
|---|---|---|---|---|
| 1 | 2026-11-05 | [Why: what Claude Code actually spends its tokens on](../DEVLOG.md#2026-11-05--1-why-what-claude-code-actually-spends-its-tokens-on) | measured | 57% of tool output is in subagents; median context 188k at a tool call; the cost is turns, not bytes |
| 2 | 2026-11-05 | [`mt span`: one call instead of grep plus sed](../DEVLOG.md#2026-11-05--2-mt-span-one-call-instead-of-grep-plus-sed) | kept | `mt span`: exact enclosing definition in one call |
| 3 | 2026-11-05 | [All languages: one engine over tree-sitter tags queries](../DEVLOG.md#2026-11-05--3-all-languages-one-engine-over-tree-sitter-tags-queries) | kept | 35 tree-sitter grammars via tags queries, plus heuristic and text tiers |
| 4 | 2026-11-05 | [Serverless index, `context`, `apply`](../DEVLOG.md#2026-11-05--4-serverless-index-context-apply) | kept | serverless SQLite index, `context` and `apply`; a server deferred |
| 5 | 2026-11-05 | [Scripted benchmark: the traditional workflow against mt](../DEVLOG.md#2026-11-05--5-scripted-benchmark-the-traditional-workflow-against-mt) | measured | scripted bench: renames in 2 calls vs 8-14, identical diffs |
| 6 | 2026-11-05 | [Real Claude sessions: does Claude use mt, and does it help?](../DEVLOG.md#2026-11-05--6-real-claude-sessions-does-claude-use-mt-and-does-it-help) | measured | real headless sessions: Claude rarely chose mt from the skill alone; harness leaks of graft found and closed |
| 7 | 2026-11-05 | [A change across several files impacted by one query](../DEVLOG.md#2026-11-05--7-a-change-across-several-files-impacted-by-one-query) | measured | T3 multi-file change: mt found every site; the task wording was ambiguous |
| 8 | 2026-11-05 | [One shot: every change, the tests, and what to run afterwards](../DEVLOG.md#2026-11-05--8-one-shot-every-change-the-tests-and-what-to-run-afterwards) | kept | `mt apply` runs gates, tests and formatters after writing; replies carry what the next fix needs |
| 9 | 2026-11-05 | [Real sessions on the one-shot design: change plus a new unit test](../DEVLOG.md#2026-11-05--9-real-sessions-on-the-one-shot-design-change-plus-a-new-unit-test) | measured | T4 one-shot: 17 edits to 7 files in one apply, but a hash mistake cost the win |
| 10 | 2026-11-05 | [Rerun with the hash fix: 3 repetitions, and a realistic context size](../DEVLOG.md#2026-11-05--10-rerun-with-the-hash-fix-3-repetitions-and-a-realistic-context-size) | measured | at ~150k context mt cut turns 42%, calls 55%, wall 34%, cost 12% |
| 11 | 2026-11-05 | [Replaying real past sessions: original read chains vs one mt call](../DEVLOG.md#2026-11-05--11-replaying-real-past-sessions-original-read-chains-vs-one-mt-call) | measured | replayed 13 real read chains (42 calls) as single mt calls |
| 12 | 2026-11-05 | [In-memory graph, `mt query` with claims, a live daemon](../DEVLOG.md#2026-11-05--12-in-memory-graph-mt-query-with-claims-a-live-daemon) | kept | in-memory graph, `mt query` batches with claims, per-repo daemon |
| 13 | 2026-11-05 | [Session startup: a plugin that makes Claude use mt, and one mt server per session](../DEVLOG.md#2026-11-05--13-session-startup-a-plugin-that-makes-claude-use-mt-and-one-mt-server-per-session) | kept | plugin: adoption went from 1 in 4 sessions (skill alone) to 4 in 4 |
| 14 | 2026-11-05 | [T4 A/B with claims first: discovery is solved, the gap is interpretation](../DEVLOG.md#2026-11-05--14-t4-ab-with-claims-first-discovery-is-solved-the-gap-is-interpretation) | measured | claims-first T4: exhaustive claims surfaced all 5 sites every run; the gap is interpretation |
| 15 | 2026-11-05 | [Parallel everywhere, and `property`: one field across every layer](../DEVLOG.md#2026-11-05--15-parallel-everywhere-and-property-one-field-across-every-layer) | kept | parallel graph load 2,535 ms -> ~250 ms; `property` follows one field across spellings |
| 16 | 2026-11-05 | [Interactive A/B, test G1: "add Google authentication, with tests"](../DEVLOG.md#2026-11-05--16-interactive-ab-test-g1-add-google-authentication-with-tests) | measured | interactive G1 run 1 (Google auth): turns -31%, calls -27%, but cost +8.7% |
| 17 | 2026-11-05 | [Fixes from section 16, then G1 run 2](../DEVLOG.md#2026-11-05--17-fixes-from-section-16-then-g1-run-2) | measured | G1 run 2 after fixes: turns -44%, calls -52%, cost -13% |
| 18 | 2026-11-05 | [Two-call protocol: span chaining, `@@` bundle, session dedupe; G1 run 3](../DEVLOG.md#2026-11-05--18-two-call-protocol-span-chaining--bundle-session-dedupe-g1-run-3) | measured | two-call protocol: G1 in 13 turns vs 53 (-75%), cost -26% |
| 19 | 2026-11-05 | [`mt tools`: one graph for code and tooling, replacing the always-on listings](../DEVLOG.md#2026-11-05--19-mt-tools-one-graph-for-code-and-tooling-replacing-the-always-on-listings) | kept | `mt tools`: one graph for code and tooling to replace ~32k tokens of always-on listings |
| 20 | 2026-11-05 | [`mt expand`: pointers you can open by number](../DEVLOG.md#2026-11-05--20-mt-expand-pointers-you-can-open-by-number) | kept | `mt expand`: numbered pointers open in one hash-checked call |
| 21 | 2026-11-05 | [Hooks in Rust (`mt hook`), rewrite instead of deny, no Python in the product](../DEVLOG.md#2026-11-05--21-hooks-in-rust-mt-hook-rewrite-instead-of-deny-no-python-in-the-product) | kept | hooks rewritten in Rust: 6-11 ms instead of 61-124 ms; rewrite instead of deny |
| 22 | 2026-11-05 | [`mt tools` fixes from section 19, and a clean rerun](../DEVLOG.md#2026-11-05--22-mt-tools-fixes-from-section-19-and-a-clean-rerun) | measured | `mt tools` rerun: first request -32%, input -48%, turns -26%, cost -43%, 30/30 correct |
| 23 | 2026-11-05 | [Orientation in the batch (`map`, `module`), outlines, slim apply reply; G1 runs 4 (invalid) and 5](../DEVLOG.md#2026-11-05--23-orientation-in-the-batch-map-module-outlines-slim-apply-reply-g1-runs-4-invalid-and-5) | measured | `map`/`module` in the batch: G1 run 5 at 13 turns (-75%), cost -21% |
| 24 | 2026-11-05 | [mt knows no language: removing the Go-specific parts of sections 18 and 23](../DEVLOG.md#2026-11-05--24-mt-knows-no-language-removing-the-go-specific-parts-of-sections-18-and-23) | reversed | Go-specific parts of sections 18 and 23 removed: mt knows no language |
| 25 | 2026-11-05 | [Check, prepare, change, prove: one `mt apply` call](../DEVLOG.md#2026-11-05--25-check-prepare-change-prove-one-mt-apply-call) | kept | one `mt apply`: check, prepare, change, prove |
| 26 | 2026-11-05 | [Folding, the 10k briefing limit, and never denying useful work; T4 with the plugin at normal and 255k context](../DEVLOG.md#2026-11-05--26-folding-the-10k-briefing-limit-and-never-denying-useful-work-t4-with-the-plugin-at-normal-and-255k-context) | measured | folding and no denial of useful work; T4 at 255k: turns -33%, cache read -34%, cost flat |
| 27 | 2026-11-05 | [Speed: the bash chains Claude writes, against mt, at 500 / 5,000 / 20,000 files](../DEVLOG.md#2026-11-05--27-speed-the-bash-chains-claude-writes-against-mt-at-500--5000--20000-files) | measured | speed suite: whole read phase 4.0x faster than bash at 20k files; bash still wins single cheap reads |
| 28 | 2026-11-05 | [No undo, no backups; `module` on every core; speed rerun](../DEVLOG.md#2026-11-05--28-no-undo-no-backups-module-on-every-core-speed-rerun) | reversed | undo and backups removed; `module` on every core |
| 29 | 2026-11-05 | [Output compression: logs and JSON stream through mt, the model queries for what it needs](../DEVLOG.md#2026-11-05--29-output-compression-logs-and-json-stream-through-mt-the-model-queries-for-what-it-needs) | kept | output compression: large JSON becomes a graph; an MCP reply 44.5k -> 605 chars, cost -49% |
| 30 | 2026-11-05 | [`mt tools`: the thirteen improvements, the code graph across sub-projects, and a gold set](../DEVLOG.md#2026-11-05--30-mt-tools-the-thirteen-improvements-the-code-graph-across-sub-projects-and-a-gold-set) | measured | `mt tools` run 3: first request -35%, input -47%, cost -40%, 38/39 vs 36/39 correct; ranking overfit |
| 31 | 2026-11-05 | [Named stores: each line of work keeps its own outputs, and any agent can read any of them](../DEVLOG.md#2026-11-05--31-named-stores-each-line-of-work-keeps-its-own-outputs-and-any-agent-can-read-any-of-them) | kept | named output stores: agents' data kept apart, readable by any agent |
| 32 | 2026-11-05 | [In memory for real: apply through the daemon, file text in memory, a lower floor, reindexing only what changed](../DEVLOG.md#2026-11-05--32-in-memory-for-real-apply-through-the-daemon-file-text-in-memory-a-lower-floor-reindexing-only-what-changed) | kept | apply through the daemon, file text in memory, reindex only what changed |
| 33 | 2026-11-05 | [An independent gold set, and a Rust-native BERT](../DEVLOG.md#2026-11-05--33-an-independent-gold-set-and-a-rust-native-bert) | measured | independent 90-prompt gold set: word ranking 24% @3; Rust BERT +10 @3 |
| 34 | 2026-11-05 | [Standard query languages for kept JSON: JSONPath (RFC 9535) and jq](../DEVLOG.md#2026-11-05--34-standard-query-languages-for-kept-json-jsonpath-rfc-9535-and-jq) | kept | kept JSON queried with RFC 9535 JSONPath and jq |
| 35 | 2026-11-05 | [Fair bash baselines, many agents on one daemon, and why the daemon queued them](../DEVLOG.md#2026-11-05--35-fair-bash-baselines-many-agents-on-one-daemon-and-why-the-daemon-queued-them) | measured | 8 agents on one daemon: wall 2,185 -> 525 ms, median request flat ~25 ms |
| 36 | 2026-11-05 | [mt vs graft, side by side on the same queries](../DEVLOG.md#2026-11-05--36-mt-vs-graft-side-by-side-on-the-same-queries) | measured | mt vs graft: mt ~10x faster with full recall, graft 1.5-16x smaller with gaps |
| 37 | 2026-11-05 | [Wire keys between components, and the A/B with BERT](../DEVLOG.md#2026-11-05--37-wire-keys-between-components-and-the-ab-with-bert) | measured | wire keys across components; `mt tools` A/B with BERT: turns -30%, input -55%, cost -48% |
| 38 | 2026-11-05 | [Reindexing in the background, never holding anything up; a race it exposed; rename speed](../DEVLOG.md#2026-11-05--38-reindexing-in-the-background-never-holding-anything-up-a-race-it-exposed-rename-speed) | kept | reindexing moved to a background thread; a race fixed; rename speed measured |
| 39 | 2026-11-06 | [Accuracy first: definition-following resolution (from reading graft's resolver), mirror cards out, nothing silently missing](../DEVLOG.md#2026-11-06--39-accuracy-first-definition-following-resolution-from-reading-grafts-resolver-mirror-cards-out-nothing-silently-missing) | kept | definition-following resolution: blast radius 13 false positives -> 0 at 23/23 |
| 40 | 2026-11-06 | [No one-off scripts: `@@ regex` over a directory, and the skill says so](../DEVLOG.md#2026-11-06--40-no-one-off-scripts--regex-over-a-directory-and-the-skill-says-so) | kept | `@@ regex` over a directory replaces one-off Python/shell loops |
| 41 | 2026-11-06 | [Component-first seeding (rejected), a regression the A/B caught, and paired A/B runs](../DEVLOG.md#2026-11-06--41-component-first-seeding-rejected-a-regression-the-ab-caught-and-paired-ab-runs) | reversed | component-first seeding hurt everywhere and was deleted; paired A/B mode added |
| 42 | 2026-11-06 | [Making Claude batch edits across code files; mt only does what it is told](../DEVLOG.md#2026-11-06--42-making-claude-batch-edits-across-code-files-mt-only-does-what-it-is-told) | kept | correction: mt never reverts on its own; the hook redirects 81% of real rewrite commands |
| 43 | 2026-11-06 | [Map, then expand once: IDs instead of contents, expansions kept in memory, the whole reference inline](../DEVLOG.md#2026-11-06--43-map-then-expand-once-ids-instead-of-contents-expansions-kept-in-memory-the-whole-reference-inline) | kept | map view, then one `mt expand`: IDs instead of contents, served from memory |
| 44 | 2026-11-06 | [The virtual context: codes instead of code, `mt show` for the human, the working set after a compaction](../DEVLOG.md#2026-11-06--44-the-virtual-context-codes-instead-of-code-mt-show-for-the-human-the-working-set-after-a-compaction) | kept | virtual context: codes in `mt apply`, `mt show` for the human, the working set after compaction |
| 45 | 2026-11-06 | [T4 was a scoring bug; the real gap was speed, fixed with a chain card](../DEVLOG.md#2026-11-06--45-t4-was-a-scoring-bug-the-real-gap-was-speed-fixed-with-a-chain-card) | measured | T4 'miss' was a scoring bug; chain card: overall turns -30%, input -51%, cost -47%, 39/39 |

## Sections

### 1. Why: what Claude Code actually spends its tokens on

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--1-why-what-claude-code-actually-spends-its-tokens-on)

**Outcome:** Parsed 2,258 transcripts (6.9 GB, 90 days). 57% of tool output lands in subagents; 2,372 re-reads (5% of bytes), 96% with no edit between. Median context is 188k tokens at a tool call and 261k at a grep, so every extra turn re-reads all of it. Decision: one call to read everything related, one call to change it.

### 2. `mt span`: one call instead of grep plus sed

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--2-mt-span-one-call-instead-of-grep-plus-sed)

**Outcome:** `mt span` returns the enclosing definition via tree-sitter, with line numbers and a hash; a miss returns the outline. On 297 random definitions in 5 repos, all 22 disagreements with the reference parser were the reference parser's errors.

### 3. All languages: one engine over tree-sitter tags queries

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--3-all-languages-one-engine-over-tree-sitter-tags-queries)

**Outcome:** Three tiers: 35 bundled grammars (tags queries plus mt's own patterns), a heuristic tier for other code-like files (`~kind`), and a text tier for logs and prose. 19 unit tests cover every family. Known gap: some Kotlin `object` declarations.

### 4. Serverless index, `context`, `apply`

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--4-serverless-index-context-apply)

**Outcome:** Measured first: a server only paid off on very large repos, so the index went serverless (`.mt/index.sqlite`, incremental, all cores). `mt context` returns target, callers with call lines, types, callees, tests, tables and docs to a budget; `mt apply` is a hash-checked all-or-nothing batch. Several false-match bugs fixed (e.g. `Store.X` matching `fakeStore.X`).

### 5. Scripted benchmark: the traditional workflow against mt

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--5-scripted-benchmark-the-traditional-workflow-against-mt)

**Outcome:** Part A replayed the transcript workflow on 30 symbols (modelled at the user's medians: 3.8 s turn, 261k context). Part B: 4 renames, 8-14 calls traditional vs 2 with mt, identical diffs, every build passing; mt's own share was 4 ms validate, 1 ms write, 13-21 ms reindex.

### 6. Real Claude sessions: does Claude use mt, and does it help?

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--6-real-claude-sessions-does-claude-use-mt-and-does-it-help)

**Outcome:** First real A/B on Opus 5.5 (T1 read, T2 change). Round 1 was contaminated by graft through PATH, repo hooks and AGENTS.md; each leak was removed. With only the skill Claude used mt in 1 of 4 runs. Read task with mt: ~25% fewer turns, wall time and output tokens at similar cost. Added `--edit` mode, a `regex` op with `refs:` and an `expect:` guard.

### 7. A change across several files impacted by one query

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--7-a-change-across-several-files-impacted-by-one-query)

**Outcome:** T3 (request ID on every audit row: migration, struct field, SQL, call sites; 8 checks). mt listed every site, but the mt run judged two request-derived sites out of scope; the control did not. One run per condition, 18k context, run-to-run swing ~30%.

### 8. One shot: every change, the tests, and what to run afterwards

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--8-one-shot-every-change-the-tests-and-what-to-run-afterwards)

**Outcome:** `run:` steps with gates (a failing gate restores every file), parallel non-gate steps, timeouts and saved logs; each reply shows the changed regions and the code at every file:line a failure names. Demo: mt overhead 39-45 ms validate, 0-1 ms write, 9-10 ms reindex.

### 9. Real sessions on the one-shot design: change plus a new unit test

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--9-real-sessions-on-the-one-shot-design-change-plus-a-new-unit-test)

**Outcome:** T4 = T3 plus a new unit test. The mt arm did everything in one `mt apply` (17 edits, 7 files, all four steps passing) but its first apply was refused: it used a whole-function hash on a sub-range op. Fixed in section 10.

### 10. Rerun with the hash fix: 3 repetitions, and a realistic context size

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--10-rerun-with-the-hash-fix-3-repetitions-and-a-realistic-context-size)

**Outcome:** T4 rerun with the hash fix, 3 runs each at normal context and 1 each at ~150k context. At realistic context mt won on every measure: turns -42%, calls -55%, wall time -34%, output -34%, cache reads -55%, cost -12% ($0.35 per task). At normal context: turns -13%, calls -23%. Added `baseline` steps so pre-existing failures report as KNOWN.

### 11. Replaying real past sessions: original read chains vs one mt call

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--11-replaying-real-past-sessions-original-read-chains-vs-one-mt-call)

**Outcome:** 13 read-only chains (42 calls) from 11 real sessions replayed through one batched mt call each, comparing content, speed and call count. Caveats: one repo; some chains replayed against code 2-3 weeks newer.

### 12. In-memory graph, `mt query` with claims, a live daemon

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--12-in-memory-graph-mt-query-with-claims-a-live-daemon)

**Outcome:** `Store` trait over SQLite and an in-memory `Graph`; `mt query` runs JSON batches with `expect` claims, `exhaustive` lists and `$id` chaining under one budget; `mt serve` is a per-repo Unix-socket daemon with a file watcher and a sync barrier so answers never lag disk.

### 13. Session startup: a plugin that makes Claude use mt, and one mt server per session

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--13-session-startup-a-plugin-that-makes-claude-use-mt-and-one-mt-server-per-session)

**Outcome:** A Claude Code plugin starts one `mt serve` per session, injects the skill, and redirects old-style code reads to the matching mt command. Claude used mt in 4 of 4 sessions, against 1 of 4 with the skill alone. The one denied turn per session became the next target.

### 14. T4 A/B with claims first: discovery is solved, the gap is interpretation

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--14-t4-ab-with-claims-first-discovery-is-solved-the-gap-is-interpretation)

**Outcome:** 3 runs per arm. Means: no mt 11.7 turns, $0.544, 5.0/5 sites; mt context-first 10.3 turns, $0.597, 3.3/5; claims-first 11.3 turns, $0.613, 3.7/5. mt showed all 5 sites every time; models judged WebSocket/login sites out of the prompt's scope.

### 15. Parallel everywhere, and `property`: one field across every layer

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--15-parallel-everywhere-and-property-one-field-across-every-layer)

**Outcome:** Graph load on a 4,135-file repo (1M reference rows): 2,535 ms -> 222-265 ms. Query batches run in parallel waves: 8 queries 720 ms separately -> 261 ms as one batch. `property` follows a field across spellings and layers; freshness loop 30/30.

### 16. Interactive A/B, test G1: "add Google authentication, with tests"

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--16-interactive-ab-test-g1-add-google-authentication-with-tests)

**Outcome:** Two interactive worktrees, same prompt. mt ON vs OFF: 40 vs 58 turns, 43 vs 59 calls, shell reads 57 vs 154 (-63%), cache read -21%, but cost $4.006 vs $3.684 (+8.7%) because replies were larger (peak context +24%). Both builds, tests and typecheck pass.

### 17. Fixes from section 16, then G1 run 2

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--17-fixes-from-section-16-then-g1-run-2)

**Outcome:** Fixes tied to run 1's evidence (reply header undercounting tokens, hook redirects, skill size). Run 2: 27 vs 48 turns, 27 vs 56 calls, active time 694 vs 796 s, cost $3.431 vs $3.943 (-13%), cache read -45%. Both arms pass build, vet, tests and tsc.

### 18. Two-call protocol: span chaining, `@@` bundle, session dedupe; G1 run 3

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--18-two-call-protocol-span-chaining--bundle-session-dedupe-g1-run-3)

**Outcome:** Span chaining, the `@@` bundle and session dedupe. Run 3: 13 turns vs a 53-turn OFF baseline (-75%), 13 vs 57.5 calls, active time -21%, cost $2.828 vs $3.81 (-26%), cache read -84%. One bundle applied 23 edits to 15 files and every step passed first try.

### 19. `mt tools`: one graph for code and tooling, replacing the always-on listings

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--19-mt-tools-one-graph-for-code-and-tooling-replacing-the-always-on-listings)

**Outcome:** Across 763 sessions the median first request was 95k tokens, ~32k of it listings (agents 7.8k, skills 7.5k, deferred names 7.4k, hooks 6k, MCP 3.9k). `mt tools` builds a catalog plus code cards from definitions on disk and writes a settings profile that trims listings to what the repo needs.

### 20. `mt expand`: pointers you can open by number

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--20-mt-expand-pointers-you-can-open-by-number)

**Outcome:** Pointers in replies open by number (`mt expand 3 5`), hash-checked and relocated by name if changed. In the T4 A/B every arm got 5/5 sites; expand was used in 2 of 3 runs, 9 of 191 pointers. No cost difference at 19k context.

### 21. Hooks in Rust (`mt hook`), rewrite instead of deny, no Python in the product

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--21-hooks-in-rust-mt-hook-rewrite-instead-of-deny-no-python-in-the-product)

**Outcome:** The Python hook (308 lines) became `mt hook <event>`. Hook latency fell from 61-124 ms to 6-11 ms. A lone sed range or single-identifier grep is rewritten in place to the mt command, so no denied turn. Rule from here: everything shipped is Rust.

### 22. `mt tools` fixes from section 19, and a clean rerun

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--22-mt-tools-fixes-from-section-19-and-a-clean-rerun)

**Outcome:** Hook latency was startup contention, not mt (112-206 ms sequential). Top card carries lines with hashes; names-only listings; prose-noise rules. Rerun, 3 tasks x 3 runs: first request -32%, input tokens -48%, turns -26%, cost -43%, 30/30 correct in both arms. No lever exists for single plugin skills.

### 23. Orientation in the batch (`map`, `module`), outlines, slim apply reply; G1 runs 4 (invalid) and 5

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--23-orientation-in-the-batch-map-module-outlines-slim-apply-reply-g1-runs-4-invalid-and-5)

**Outcome:** Run 4 invalid (the plugin's hook file was replaced mid-run by another session). Run 5: 13 turns vs 53 (-75%), 12 vs 57.5 calls, cost $3.013 vs $3.81 (-21%), cache read -81%, 0 orientation shell calls, apply reply 3,583 B vs 28,124 B.

### 24. mt knows no language: removing the Go-specific parts of sections 18 and 23

2026-11-05 · reversed · [full section](../DEVLOG.md#2026-11-05--24-mt-knows-no-language-removing-the-go-specific-parts-of-sections-18-and-23)

**Outcome:** User rule: mt is language-agnostic. An audit found Go-specific code only in sections 18 and 23's additions; removed. Fixed `module` counting JSON/Markdown as code by using file roles. 36 tests pass.

### 25. Check, prepare, change, prove: one `mt apply` call

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--25-check-prepare-change-prove-one-mt-apply-call)

**Outcome:** Added in-batch checks ("if these are correct") and steps that run before anything is written, so one call can verify, change, format, build and test. `--retry` with corrections stays open.

### 26. Folding, the 10k briefing limit, and never denying useful work; T4 with the plugin at normal and 255k context

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--26-folding-the-10k-briefing-limit-and-never-denying-useful-work-t4-with-the-plugin-at-normal-and-255k-context)

**Outcome:** Indentation-only folding of reference bodies (any language); the skill split across hooks to fit the 10k injection limit; the hook only denies commands that are nothing but old-style reads. T4 at 255k, 2 runs each: 7 vs 10.5 turns (-33%), cache read -34%, cost and wall time the same. Next lever: reply size.

### 27. Speed: the bash chains Claude writes, against mt, at 500 / 5,000 / 20,000 files

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--27-speed-the-bash-chains-claude-writes-against-mt-at-500--5000--20000-files)

**Outcome:** `tests/speed.rs` against real `sh` on generated repos (500/5,000/20,000 files). At 20k via the daemon: definition 25.5x, every caller 32.3x, constructions 8.0x, whole read phase 4.0x (1,140 vs 4,524 ms), output 27,883 vs 282,008 B. Bash wins a single outline (0.3x), dependency declarations (0.01x) and rename (0.2x).

### 28. No undo, no backups; `module` on every core; speed rerun

2026-11-05 · reversed · [full section](../DEVLOG.md#2026-11-05--28-no-undo-no-backups-module-on-every-core-speed-rerun)

**Outcome:** At the user's request `mt undo` and on-disk backups were removed; a failing gate restores from originals held in memory. `module` uses every core. Bash still wins single cheap commands; mt's floor is ~12-15 ms.

### 29. Output compression: logs and JSON stream through mt, the model queries for what it needs

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--29-output-compression-logs-and-json-stream-through-mt-the-model-queries-for-what-it-needs)

**Outcome:** `mt compress`, `mt run --`, `mt out` and a PostToolUse hook turn large Bash/Read/MCP output into a structure the model queries. A 44.5k-char MCP response became a 605-char graph plus one query, at -49% cost.

### 30. `mt tools`: the thirteen improvements, the code graph across sub-projects, and a gold set

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--30-mt-tools-the-thirteen-improvements-the-code-graph-across-sub-projects-and-a-gold-set)

**Outcome:** Thirteen improvements (hook gate, slim plugins, cross-sub-project routes, embeddings). 4 tasks x 3 runs: first request -35%, input -47%, turns -21%, cost -40%, 38/39 vs 36/39 correct. Held-out gold set showed overfitting: 42% @3 vs 75% on the tuning set.

### 31. Named stores: each line of work keeps its own outputs, and any agent can read any of them

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--31-named-stores-each-line-of-work-keeps-its-own-outputs-and-any-agent-can-read-any-of-them)

**Outcome:** Kept outputs live in `.mt/out/<name>/` (self-gitignored, 7-day prune); any agent can query any store, none mixes into another. Skill part 2 reached 9,745 of 10,000 units: trim before adding.

### 32. In memory for real: apply through the daemon, file text in memory, a lower floor, reindexing only what changed

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--32-in-memory-for-real-apply-through-the-daemon-file-text-in-memory-a-lower-floor-reindexing-only-what-changed)

**Outcome:** `mt apply` goes through the daemon instead of a second indexer; the daemon holds file text in memory; reindexing touches only changed files. mt's floor is ~11 ms against a 5 ms grep.

### 33. An independent gold set, and a Rust-native BERT

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--33-an-independent-gold-set-and-a-rust-native-bert)

**Outcome:** Three blind agents wrote 90 prompts (147 items). The word ranking scored 24% @3 against 75% on the tuning prompts. Pure-Rust BERT (candle, bge-small) added +10 @3 / +17 @k on the untouched split; graph-propagated vectors did not beat plain text vectors.

### 34. Standard query languages for kept JSON: JSONPath (RFC 9535) and jq

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--34-standard-query-languages-for-kept-json-jsonpath-rfc-9535-and-jq)

**Outcome:** `--json` is RFC 9535 JSONPath; `--jq` runs jaq in process with time and result bounds. In a real session Opus used `--jq` straight from the header: 4 turns, $0.148, all correct, and the JSON never entered context.

### 35. Fair bash baselines, many agents on one daemon, and why the daemon queued them

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--35-fair-bash-baselines-many-agents-on-one-daemon-and-why-the-daemon-queued-them)

**Outcome:** The daemon served requests one at a time; it is now threaded. 8 agents: wall 2,185 -> 525 ms, median request 101.5 -> 24.6 ms, 14.6 -> 61.0 requests/s. In-process mode for the same 8 agents: 6,407 ms.

### 36. mt vs graft, side by side on the same queries

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--36-mt-vs-graft-side-by-side-on-the-same-queries)

**Outcome:** 9 queries on the same repo. mt 9-48 ms vs graft 185-402 ms; mt had full recall, graft missed callers passed as values (0/2) and most of a blast radius (hop 1 1/5, hop 2 2/23). mt replies were 1.5-16x bigger and had 13 by-name false positives at hop 2.

### 37. Wire keys between components, and the A/B with BERT

2026-11-05 · measured · [full section](../DEVLOG.md#2026-11-05--37-wire-keys-between-components-and-the-ab-with-bert)

**Outcome:** Shared literals and undefined identifiers link components that never call each other (named anywhere 44% -> 50%). A/B with BERT: turns -30%, input -55%, cost -48%, 36/39 in both arms.

### 38. Reindexing in the background, never holding anything up; a race it exposed; rename speed

2026-11-05 · kept · [full section](../DEVLOG.md#2026-11-05--38-reindexing-in-the-background-never-holding-anything-up-a-race-it-exposed-rename-speed)

**Outcome:** The watcher no longer parses or writes SQLite under the graph's write lock, and `apply` no longer holds it through build and test steps. A race this exposed was fixed. Speed changes on S11 traced to machine load, not the build.

### 39. Accuracy first: definition-following resolution (from reading graft's resolver), mirror cards out, nothing silently missing

2026-11-06 · kept · [full section](../DEVLOG.md#2026-11-06--39-accuracy-first-definition-following-resolution-from-reading-grafts-resolver-mirror-cards-out-nothing-silently-missing)

**Outcome:** Learning from graft's resolver: code-only sites, same language family, nearest definition, hops follow definitions, every site labelled; committed mirror cards dropped from the index. Blast radius went from 13 false to 0 at 23/23. Workflow test: mt 1-2 calls, complete 5/5; graft lookup + reads 4-13 calls, complete 3/5.

### 40. No one-off scripts: `@@ regex` over a directory, and the skill says so

2026-11-06 · kept · [full section](../DEVLOG.md#2026-11-06--40-no-one-off-scripts--regex-over-a-directory-and-the-skill-says-so)

**Outcome:** One-off scripts were 5.4% of Bash calls and `for` loops 3.8% over 14 days. Added `@@ regex in <dir> ext <exts> expect N`; the skill and the per-prompt hook line say to use it instead of scripts.

### 41. Component-first seeding (rejected), a regression the A/B caught, and paired A/B runs

2026-11-06 · reversed · [full section](../DEVLOG.md#2026-11-06--41-component-first-seeding-rejected-a-regression-the-ab-caught-and-paired-ab-runs)

**Outcome:** Component-first ranking, designed before measuring, lowered scores in every category and was removed. An A/B caught embeddings creating code cards for a skill request (now gated). Paired A/B (warm-up, then A+B side by side) takes ~10 min with no cold starts. Across runs 3-5: turns -21 to -30%, input -47 to -55%, cost -40 to -48%.

### 42. Making Claude batch edits across code files; mt only does what it is told

2026-11-06 · kept · [full section](../DEVLOG.md#2026-11-06--42-making-claude-batch-edits-across-code-files-mt-only-does-what-it-is-told)

**Outcome:** Earlier text said a failed build rolls everything back; wrong: only a step the batch marks as a gate restores files. The skill teaches batching; the hook redirected 323 of 400 sampled real rewrite commands (81%).

### 43. Map, then expand once: IDs instead of contents, expansions kept in memory, the whole reference inline

2026-11-06 · kept · [full section](../DEVLOG.md#2026-11-06--43-map-then-expand-once-ids-instead-of-contents-expansions-kept-in-memory-the-whole-reference-inline)

**Outcome:** `--map` returns one line per item with its relations and no bodies; one `mt expand` fetches every body needed from the daemon's memory. Latency floor ~5.7 ms (process start). The whole reference is delivered across SessionStart, the prompt and the first tool results.

### 44. The virtual context: codes instead of code, `mt show` for the human, the working set after a compaction

2026-11-06 · kept · [full section](../DEVLOG.md#2026-11-06--44-the-virtual-context-codes-instead-of-code-mt-show-for-the-human-the-working-set-after-a-compaction)

**Outcome:** 34 real sessions: prefix 24%, the model's own output re-read ~34%, injections ~22%, all tool results ~15%. So apply accepts `#n` codes and `@@ copy|move`; `mt show` sends text to the user without it entering context; the working set is restored after compaction. Not yet A/B tested.

### 45. T4 was a scoring bug; the real gap was speed, fixed with a chain card

2026-11-06 · measured · [full section](../DEVLOG.md#2026-11-06--45-t4-was-a-scoring-bug-the-real-gap-was-speed-fixed-with-a-chain-card)

**Outcome:** Every answer had cited the store correctly; the scorer wanted a name. A chain card (client call -> route -> handler -> store) fixed the real gap, speed. Run 6, 12 sessions per arm: turns 5.4 vs 3.8 (-30%), first request -34%, input -51%, wall -29%, cost $1.205 vs $0.638 (-47%), 39/39 in both arms.
