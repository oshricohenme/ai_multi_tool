# mt: measured evidence

This document collects every measurement made while building `mt`, and asks one question: does
Claude Code do the same work with fewer turns, fewer tokens, less money and the same or better
correctness when mt is present, compared with Claude Code's default workflow and with earlier mt
designs?

Every number here is copied from the developer log (`DEVLOG.md`, cited as §N) or from a summary
file under `bench/`. Nothing was re-run for this document. Where the log gives a delta, that delta
is quoted. Where it does not, the delta was computed from the log's own means and is marked
"computed".

---

## Abstract

mt does two separate jobs, and the evidence for them differs.

1. **Code reading and editing** (`mt query`, `mt context`, `mt apply`, the plugin hooks). mt
   reliably cuts **turns** and **cache reads**. It cuts **cost** only once the context is large.
   - At small starting context (about 19k tokens), mt sessions took 13% to 39% fewer turns but
     usually cost 5% to 28% *more* (§10, §13, §20), because mt's larger structured replies are
     cache writes that every later turn re-reads.
   - At a realistic 255k context, turns fell 33% to 42%. Cost fell 1% (§26, n=2) and 12% (§10,
     n=1).
   - On one large interactive task ("add Google authentication, with tests", §16 to §23), the
     final design used 13 turns against a 53-turn baseline (-75%) and cost 21% to 26% less. The
     first design cost 8.7% more. Each of these is n=1 per design.
2. **Tool and skill listing overhead** (`mt tools`). This is the strongest result. Six A/B runs
   (§19, §22, §30, §37, §41, §45; the last four with n=12 sessions per arm) show, for the mt arm:
   first request -15% to -36%, input tokens -36% to -55%, turns -21% to -30%, cost -40% to -48%.
   After a scoring fix (§45), correctness was 39/39 in both arms in every run from 3 to 6.
3. **Output compression** (`mt compress`, the PostToolUse hook). On real outputs it removed 67% to
   99% of tokens from logs and JSON, and left code and diffs untouched (§29). In one MCP session
   cost fell 49% (n=1).
4. **Speed.** With the daemon at 20,000 files, mt answered a whole read phase 6.8x faster than the
   bash chain Claude Code runs (288 ms against 1,955 ms) and at a tenth of the output bytes. bash
   still wins single cheap commands (`bench/speed_last.txt`, §38).
5. **Accuracy against graft 0.21.1.** mt finished 5 of 5 workflow tasks completely in 1 to 2
   calls. graft took 4 to 13 calls and was complete in 3 of 5 (§39).

Ranking quality for `mt tools` code cards is the weak spot. On 90 independent prompts, recall@3 is
37% with BERT embeddings, against 72% to 75% on the developer's own tuning prompts (§33). Part of
the tuning-set score is overfitting.

---

## 1. Setup

### 1.1 Environment

| Item | Value | Source |
|---|---|---|
| Machine | one developer's macOS machine, Apple Silicon, 18 cores used by mt | DEVLOG header, §28 |
| Claude Code | 2.1.318 (tool and lever probes, `mt tools` runs); 2.1.319 (`mt show` check) | §19, §30, §44 |
| Main model | Opus 5.5 for every A/B session | §6, §16, §19 |
| Other models | Haiku for probes and hook checks; Sonnet for the virtual-context live test | §19, §21, §44 |
| Cost model, interactive runs | per-token rates fitted to 50 headless Opus 5.5 runs: input $4/M, cache write $8/M, cache read $0.20/M, output $20/M (max relative error 1e-15) | §16 |
| Cost, headless runs | the `total_cost_usd` the session reports | §6, §19 |

### 1.2 The raw setup

Early runs were contaminated: Claude kept calling `graft`, a code-graph tool installed on the same
machine, through its binary on PATH, the repo's `.claude/settings.json` hooks, `AGENTS.md` and its
`graft/` card folder (§6). Every A/B after that used a raw setup:

- `graft` stubbed first on PATH;
- agent-tool config deleted from each repo copy: `.claude`, `.agents`, `.codex`, `.cursor`,
  `.gemini`, `.kiro`, `.mcp.json`, `AGENTS.md`, `GEMINI.md`, `graft/`, `opencode.json`, `.mt`;
- `--setting-sources project --strict-mcp-config` for the code workstream (§6, §16), and the graft
  MCP server denied in both arms for the `mt tools` workstream (§19);
- auto-memory off (`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`) for interactive runs, after the first
  launch loaded the user's notes about mt (§16);
- a fresh repo copy per session;
- zero graft attempts checked per run: "Zero graft attempts in all 16 final runs" (§6).

Two leaks stayed, the same in both arms of the interactive runs: `~/.claude/CLAUDE.md` and a graft
skill load as ancestor project files, because the worktrees sit under `$HOME`. Neither arm made a
graft call (§16).

### 1.3 Arms

| Arm | Meaning | Used in |
|---|---|---|
| A | no mt | all |
| B | mt and its skill installed, nothing else | §6 |
| C | B plus a one-line session note | §6 |
| D | mt required by the prompt ("start with mt context") | §6, §7, §9, §10, §14 |
| Q | mt required, "start with one `mt query` batch of claims" | §14 |
| O / X | mt required, claims first; X adds `mt expand` | §20 |
| E | the mt plugin only: hooks, briefing, no instruction in the prompt | §13, §26 |
| ON / OFF | interactive sessions with and without the plugin and an mt rule | §16 to §23 |
| B (tools) | `mt tools` settings layer plus the UserPromptSubmit hook | §19 to §45 |

### 1.4 Tasks

Code workstream (repo: the Relay Go and TypeScript monorepo, 289 to 551 files depending on what
was indexed):

| Task | What | Scored by |
|---|---|---|
| T1 | list every `decodeJSON` call site, what it decodes into, and its tests | 19/19 sites (§6, §13) |
| T2 | add a parameter to `decodeJSON` and update all 19 callers | build and vet (§6) |
| T3 | record the request ID on every audit row: migration pair, struct field, SQL, call sites | 8 checks in `verify_t3.sh` (§7) |
| T4 | T3 plus a new unit test | 8 checks, tests, and how many `AuditEntry` sites set `RequestID` (§9 on) |
| G1 | interactive: "I want to be able to add google authentication, please evaluate and add it along with tests" | build, vet, tests, `tsc`, reviewed by hand (§16) |

`mt tools` workstream (same repo):

| Task | What |
|---|---|
| T1 | find the migration, store function and test for audit log entries |
| T2 | rewrite a sentence with the humanizer skill, which arm B has turned off |
| T3 | add a nullable `archived_at` column with migrations, a struct field and a store read (from §22; §19's T3 asked for a column that already existed) |
| T4 | trace the Sessions screen's state filter through the API to the store (from §30) |

Prompts: `bench/tools/run6/tasks.json`.

### 1.5 Repetitions and context sizes

- Code workstream: 1 to 3 runs per arm. Sessions start at about 18k to 19k tokens, against a
  median of 188k at a tool call in real sessions (§1). Two sets used about 255k tokens of
  context, made by appending about 150k tokens of the repo's own design docs to the system prompt
  (§10, §26).
- `mt tools` workstream: 2 runs per task in §19 (in parallel), 3 per task from §22 on (sequential
  with warm-ups, then paired from §41).
- Interactive G1: one run per arm per design.

### 1.6 What is and is not published

The test repositories are private, so the raw session transcripts (`*.stream.jsonl`) are not
published. What is published: the harness scripts, task prompts, settings layers, gold sets,
per-step analysis files (`bench/tools/run*/analysis*.txt`), the speed suite and its results, and
edited transaction logs (`bench/transactions/`).

---

## 2. Why: where Claude Code spends tokens and turns (§1, §19, §44)

| Measurement | Value | Source |
|---|---|---|
| Transcripts parsed | 2,258 sessions, 6.9 GB, 90 days | §1 |
| Bash share of all tool output | 75.5% (Read 17.4%) | §1 |
| Inside Bash: cat/head/tail/sed -n | 43% | §1 |
| Inside Bash: grep/rg | 41% | §1 |
| Greps that target a single known file | 72% | §1 |
| Median output tokens and latency, read-or-grep turn | 403 tokens, 3.8 s (17,005 turns) | §1 |
| Median context at a tool call / at a grep | 188k / 261k tokens | §1 |
| Median first request before the user types | 95k tokens, about 32k of it listings (763 sessions) | §19 |
| ToolSearch total cost | 573 calls, about 40 minutes over 763 sessions | §19 |
| Context growth in 34 recent sessions | prefix 24%, model's own output re-read ~34%, injected text ~22%, all tool results ~15% | §44 |

Two conclusions drove the design. Turns cost more than bytes, because every turn re-reads the whole
context. And the always-on listings cost more than searching for tools does.

---

## 3. Results: code reading and editing

### 3.1 Scripted benchmark (§2, §5)

`mt span` against grep-then-sed, 297 random definitions in 5 repos and 5 languages
(`bench/bench.py`, §2):

| Approach | Round trips | Complete | Precision | Median bytes | Median time |
|---|---|---|---|---|---|
| mt span | 1 | 100% | 95% | 831 | 6.7 ms |
| grep, then sed 60 lines | 2 | 88% | 35% | 2,583 | 54 ms |
| grep, then sed 150 lines | 2 | 92% | 31% | 4,167 | 54 ms |
| grep, then cat the file | 2 | 100% | 16% | 8,520 | 54 ms |

The bench and mt disagreed on 22 of 297 cases. In every case checked by hand, the bench's parser
was wrong.

Replay of the traditional read workflow against one `mt context` call, 30 symbols, 3 repos
(`bench/agent_bench.py`, `bench/agent_bench_last.log`, §5):

| | Calls | Command time | Whole definition | Callers found | Tests found |
|---|---|---|---|---|---|
| Traditional | 6.2 | 234 ms | 50% | 83% | 92% |
| mt | 1.0 | 39 ms | 100% | 98% | 100% |

The same section reports modelled agent time (19.5 s against 3.8 s) and modelled context cost
(167k against 53k). These are computed from transcript medians, not measured.

4 renames on a scratch repo: traditional 8 to 14 calls, mt 2, identical diffs, every build passed
(§5).

### 3.2 Real headless sessions, small context (~19k tokens)

**Adoption and first A/B (§6),** 2 runs per arm, all 16 correct:

| Task | Arm | mt used | Turns | Wall s | Cost $ | Output tok |
|---|---|---|---|---|---|---|
| T1 | A | 0 of 2 | 11.0 | 79.6 | 0.411 | 5,880 |
| T1 | B (skill only) | 1 of 2 | 10.0 | 64.7 | 0.422 | 5,472 |
| T1 | C (session note) | 2 of 2 | 8.5 | 57.5 | 0.389 | 4,410 |
| T1 | D (required) | 2 of 2 | 8.5 | 64.5 | 0.384 | 4,404 |
| T2 | A | 0 of 2 | 6.0 | 37.4 | 0.148 | 1,707 |
| T2 | B | 0 of 2 | 6.0 | 42.6 | 0.149 | 1,550 |
| T2 | C | 2 of 2 | 6.0 | 39.6 | 0.231 | 1,451 |
| T2 | D | 2 of 2 | 5.0 | 50.9 | 0.270 | 2,524 |

Reading task: about 25% fewer turns, wall time and output tokens. Change task: mt cost 56% to 82%
more (computed), because the control made a single `perl` edit across 7 files.

**T3 and T4, one run each (§7, §9):**

| | T3 A | T3 D | Change | T4 A | T4 D | Change |
|---|---|---|---|---|---|---|
| Turns | 10 | 7 | -30% | 10 | 9 | -10% |
| Wall s | 81.1 | 49.4 | -39% | 96.2 | 84.9 | -12% |
| Cost $ | 0.422 | 0.310 | -27% | 0.481 | 0.484 | same |
| Output tok | 5,969 | 3,216 | -46% | 6,600 | 7,778 | +18% |
| Checks | 8/8 | 8/8 | | 8/8 | 8/8 | |
| AuditEntry sites set | 6 of 6 | 4 of 6 | | 7 | 5 | |

**T4, 3 runs per arm (§10),** normal context, all runs passed 8/8 and their tests:

| Mean | Turns | Calls | Wall s | Cost $ | Output | Cache read |
|---|---|---|---|---|---|---|
| A, no mt | 13.0 | 12.0 | 115.3 | 0.545 | 7,305 | 456,969 |
| D, mt stated | 11.3 | 9.3 | 111.1 | 0.589 | 9,131 | 340,935 |
| Change | -13% | -23% | -4% (computed) | +8% | +25% (computed) | -25% (computed) |

**T1 and T4 with the plugin and no instruction (§13),** E = plugin only, 2 runs; A from §6 and §10:

| Mean | Turns | Wall s | Cost $ | Output |
|---|---|---|---|---|
| A-T1 | 11.0 | 79.6 | 0.411 | 5,880 |
| E-T1 | 8.0 (-27%) | 60.8 (-24%) | 0.474 (+15%) | 4,929 (-16%) |
| A-T4 | 13.0 | 115.3 | 0.545 | 7,305 |
| E-T4 | 8.0 (-38%) | 99.9 (-13%) | 0.570 (+5%) | 7,114 (-3%) |

**T4 with exhaustive claims (§14),** 3 runs per arm, ambiguous prompt:

| Mean | Turns | Cost $ | Sites set (of 5) |
|---|---|---|---|
| A | 11.7 | 0.544 | 5.0 |
| D (context first) | 10.3 | 0.597 | 3.3 |
| Q (claims first) | 11.3 | 0.613 | 3.7 |

Every Q run's first batch surfaced all 5 construction sites. Two of three Q runs then judged two of
them out of the prompt's "from an HTTP request" scope. The gap was in reading the spec, not in
finding the sites.

**T4 with an unambiguous prompt, plus `mt expand` (§20),** 3 runs per arm:

| Mean | Turns | Cost $ | Cache read | Sites set |
|---|---|---|---|---|
| A, no mt | 12.0 | 0.444 | 369k | 5/5 in all runs |
| O, mt, no expand | 7.3 | 0.570 | 195k | 5/5 in all runs |
| X, mt with expand | 7.3 | 0.567 | 202k | 5/5 in all runs |

mt against no mt: -39% turns, -46% cache read, +28% cost. With the wording fixed, every run in
every arm found all 5 sites. X expanded 9 of 191 pointers offered (5%).

**Plugin arm E at normal context (§26),** 3 runs: 8.0 turns, $0.591, 99.0 s, against A's 12.0
turns, $0.444, 93.8 s from §20. Turn 1 of every E run was spent reading a spilled 14.5k briefing,
since fixed.

### 3.3 Realistic context (~255k tokens)

| Source | Arm | n | Turns | Wall s | Cost $ | Cache read |
|---|---|---|---|---|---|---|
| §10 | A | 1 | 12 | 104.2 | 2.877 | 2,979,026 |
| §10 | D | 1 | 7 (-42%) | 68.4 (-34%) | 2.525 (-12%) | 1,334,140 (-55%) |
| §26 | A | 2 | 10.5 | 89.6 | 2.716 | |
| §26 | E (plugin) | 2 | 7.0 (-33%) | 91.7 | 2.689 (-1%) | -34% |

All 255k runs passed 8/8 and their tests. In §10 the mt run set 5 sites against the control's 7
(the ambiguous prompt). In §26, with the unambiguous prompt, every run set 5/5. Each §26 E run lost
one turn to a hook denial (fixed after), and started 6k tokens higher because of the skill. The
log's conclusion: "Turns fall a third, but cost doesn't follow ... The next lever is reply size per
turn, not turn count."

### 3.4 Interactive task G1, "add Google authentication, with tests" (§16 to §23)

One interactive session per arm per design. The OFF arm does not change between designs, so from
run 3 on the baseline is the mean of OFF runs 1 and 2.

| | OFF baseline | ON run 1 | ON run 2 | ON run 3 | ON run 5 |
|---|---|---|---|---|---|
| Turns | 53 (58, 48) | 40 | 27 | 13 | 13 |
| Tool calls | 57.5 (59, 56) | 43 | 27 | 13 | 12 |
| Active time s | 777 (757, 796) | 838 | 694 | 612 | 637 |
| Cost $ | 3.81 (3.684, 3.943) | 4.006 | 3.431 | 2.828 | 3.013 |
| Cache read | 6.85M | 5.65M | 3.58M | 1.13M | 1.30M |
| Output tokens | 62.0k | 69.3k | 66.2k | 67.9k | 78.6k |
| ON vs its paired OFF | | turns -31%, cost +8.7% (vs OFF run 1) | turns -44%, cost -13% (vs OFF run 2) | turns -75%, cost -26% | turns -75%, cost -21% |

What changed between designs: file-path bodies and the `span` op in a query batch (run 2, §17);
span chaining, session dedupe and the plain-text `@@` bundle (run 3, §18); `map` and `module` ops
and a slim apply reply (run 5, §23). Run 4 is invalid: another session replaced the hooks mid-run
(§23).

Outcome: every run in both arms passed build, vet, the non-database tests and `tsc`. Database-backed
tests could not run on the machine in any arm. The designs were similar. One difference matters:
ON run 2 hand-wrote a Google ID-token verifier instead of using `coreos/go-oidc`, which is more
security surface (§17). Runs 3 and 5 used `go-oidc`.

Run 5's cost includes an identical 44,259-byte bundle resent after a dependency gate failure,
about 21.6k output tokens. The log estimates run 5 at about $2.55 without it (§23). That figure is
an estimate.

### 3.5 Replayed real sessions (§11)

13 read-only chains (42 calls) from 11 real sessions, re-run as one mt batch per chain:

| Measure | Value |
|---|---|
| Original chain time in sessions | 128.5 s |
| The same 42 commands replayed now | 5.89 s |
| One mt batch per chain, all chains | 3.46 s |
| Coverage of the original lines by the batch | median 100%, mean 93.2% |
| Coverage by `mt context` on only the first question | median 15% |

The tools were about 5% of chain time; the rest was model time between calls. One batch per chain
removes 29 turns, but only if Claude knows every question up front. Without hindsight, the first
call held a median 15% of what the chain read. This is why later designs focused on batching and
chaining (`$id`, `span $q`) rather than raw speed.

### 3.6 Adoption: does Claude use mt?

| Condition | Sessions that used mt | Source |
|---|---|---|
| Skill installed only | 1 of 4 | §6 |
| Plugin (hooks, briefing, redirects) | 4 of 4 | §13 |
| Virtual-context codes (`#n`, `@@ move #n`) with the skill text in context | 1 of 6 | §44 |
| Rewrite-command hook on 400 sampled real rewrite commands | 323 redirected (81%) | §42 |

---

## 4. Results: tool and skill listing overhead (`mt tools`)

### 4.1 Levers Claude Code exposes (§19, §30; Claude Code 2.1.318)

| Lever | Measured effect |
|---|---|
| `enabledPlugins: {x: false}` | 57,560 → 48,290 first-turn tokens (5 plugins) |
| `--strict-mcp-config` | → 38,350 |
| `deniedMcpServers` | removes tools and instructions |
| `permissions.deny: ["mcp__x"]` | removes tools, keeps instructions |
| `skillOverrides` | works on user skills; no effect on plugin skills |
| `skillListingBudgetFraction` | 30,001 → 18,015 chars at 0.005 and 0.002 |
| `disableAllHooks` | turns off mt's own hooks too, so it is unusable |

### 4.2 A/B runs

Every B arm adds the settings layer and the hook. Runs 1 and 2 have 3 tasks; runs 3 to 6 have 4.

| Run | Section | n per arm | Ordering | Turns | First request | Input tokens | Wall | Cost | Correct A / B |
|---|---|---|---|---|---|---|---|---|---|
| 1 | §19 | 6 | parallel | 4.7 → 3.7 (-21%) | 54,371 → 45,974 (-15%) | 277,306 → 177,927 (-36%) | 44.1 → 35.1 s (-20%) | 1.499 → 0.903 (-40%) | 16/16, 16/16 |
| 2 | §22 | 9 | sequential, warm | 5.0 → 3.7 (-26%) | 58,405 → 39,971 (-32%) | 302,410 → 156,322 (-48%) | 36.6 → 27.8 s (-24%) | 1.226 → 0.695 (-43%) | 30/30, 30/30 |
| 3 | §30 | 12 | sequential, warm | 5.3 → 4.2 (-21%) | 58,383 → 37,927 (-35%) | 330,873 → 174,023 (-47%) | 56.1 → 42.3 s (-25%) | 1.313 → 0.784 (-40%) | 36/39, 38/39 |
| 4 | §37 | 12 | sequential, warm, BERT | 5.6 → 3.9 (-30%) | 58,362 → 38,137 (-35%) | 363,609 → 163,651 (-55%) | 59.8 → 44.1 s (-26%) | 1.276 → 0.663 (-48%) | 36/39, 36/39 |
| 5 | §41 | 12 | paired | 5.8 → 4.4 (-24%) | 58,342 → 37,512 (-36%) | 369,183 → 178,746 (-52%) | 45.7 → 32.2 s (-30%) | 1.267 → 0.670 (-47%) | 36/39, 36/39 |
| 6 | §45 | 12 | paired, chain card | 5.4 → 3.8 (-30%) | 57,570 → 37,720 (-34%) | 325,414 → 158,525 (-51%) | 38.8 → 27.5 s (-29%) | 1.205 → 0.638 (-47%) | 39/39, 39/39 |

Notes:
- Run 1's cost is inflated for A: 4 of 6 A runs started with a cold prompt cache. Warm runs only:
  T1 -28%, T3 -31% (§19).
- The T4 misses in runs 3 to 5 were a scoring bug. Re-scored against each run's own repo copies,
  runs 3, 4 and 5 are 39/39 in both arms (§45, `bench/tools/run5/rescored.txt`).
- T4 in run 5 saved no turns (5.7 against 5.7). The chain card in run 6 made it 4.7 against 6.0
  (§45).

Per-task, run 6 (`bench/tools/run6/analysis7.txt`, §45):

| Task | Turns A / B | Input tok A / B | Cost $ A / B |
|---|---|---|---|
| T1 find code and data | 5.3 / 4.0 | 332,430 / 160,090 | 1.109 / 0.616 |
| T2 skill that is off in B | 3.0 / 2.0 | 129,401 / 75,891 | 1.064 / 0.494 |
| T3 migration plus Go change | 7.3 / 4.7 | 443,997 / 200,511 | 1.335 / 0.720 |
| T4 screen → API → store | 6.0 / 4.7 | 395,828 / 197,610 | 1.313 / 0.724 |

Most of the saving comes from the smaller first request, re-read on every turn. The rest comes
from fewer turns.

### 4.3 Ranking quality: gold sets

`mt tools bench <gold.json>` scores whether code cards surface the expected definitions.

| Gold set | Written by | Prompts / items |
|---|---|---|
| `relay.json` | the developer, used for tuning | 21 / 32 |
| `relay-heldout.json` | the developer, held out | 12 / 12 |
| `relay-independent.json` | three agents with no access to mt, paraphrase required | 90 / 147 |
| `gold-tune.json` / `gold-test.json` | the api-store third / the other 60 prompts of the independent set | 30 / 60 prompts |

Final defaults (`bench/tools/embeddings-final.txt`, §33), recall@3 / recall@k / MRR:

| Gold set | words only | model2vec | bge-small (BERT) |
|---|---|---|---|
| tuning, 32 items | 75 / 81 / 0.616 | 75 / 78 / 0.626 | 72 / 88 / 0.605 |
| held-out, 12 items | 42 / 50 / 0.433 | 33 / 33 / 0.236 | 50 / 58 / 0.319 |
| independent, 147 items | 24 / 31 / 0.205 | 33 / 39 / 0.261 | 37 / 46 / 0.307 |

On the 60-prompt test split, never used for tuning, BERT at weight 3 adds +10 points @3, +17 @k
and +0.084 MRR over words alone (19% → 29% @3) (§33).

The gap between 75% on the tuning set and 24% on independent prompts (words only) is the
overfitting result. The ranking was tuned against misses in its own prompts.

Weakest areas, independent set, recall@3 words only → BERT (§33):

| Area / kind | words only | bge |
|---|---|---|
| api-store, cross-layer | 6/21 | 11/21 |
| agent-pipeline, cross-layer | 4/25 | 7/25 |
| agent-pipeline, data flow | 1/13 | 0/13 |
| web-api, cross-layer | 5/24 | 7/24 |

Wire keys (§37) raised "named anywhere in the code cards" on the test split from 44% to 50%, with
recall@3 within one item. The chain card (§45) changed nothing at @3 or @k on the bench and raised
"named anywhere" by 1 point, while it cut T4 turns in the end-to-end run.

---

## 5. Results: output compression (§29, §34)

Real output from these repos through `mt compress` (tokens ≈ chars/4):

| Output | Tokens in | Tokens out | Saved |
|---|---|---|---|
| `cargo metadata` (JSON) | 113,174 | 2,049 | 99% |
| `git log --stat -n 60` | 8,230 | 1,222 | 86% |
| `cargo build -v` (400 lines) | 3,483 | 815 | 77% |
| `go test -v ./internal/...` | 4,098 | 1,359 | 67% |
| a hook log | 6,682 | 4,692 | 30% |
| `git diff` (kept verbatim on purpose) | 2,560 | 2,559 | 0% |
| `go vet` (tiny) | 25 | 25 | 0% |

Real sessions (Opus 5.5, plugin, control `MT_COMPRESS=0`; task: which of 400 CI checks failed),
one run each:

| Source | Arm | Correct | Turns | Cost $ | What the model got |
|---|---|---|---|---|---|
| MCP tool, 44.5k chars of JSON | on | yes | 4 | 0.128 | a 605-char graph, then one JSONPath query |
| | off | yes | 3 | 0.251 | the 44,490 chars |
| `cat data.json`, 75.7k chars | on | yes | 3 | 0.117 | a 702-char graph, then one query |
| | off | yes | 3 | 0.122 | Claude Code's own 2 KB preview, then `jq` |

MCP: -49% cost. Bash: about even, because Claude Code already previews large Bash output. After
the switch to standard JSONPath and jq (§34), a repeat of the MCP task with a per-service question
was correct in 4 turns at $0.148; the model wrote a jq filter straight from the graph header.
These are single runs: the mechanism is verified, the size of the gain is not a mean.

---

## 6. Results: speed (§27, §28, §32, §35, §38)

`tests/speed.rs` runs the built binary against real `sh` on a seeded generated repo (Go, Python and
TypeScript). It asserts correctness for every row in every mode, median of 5 runs after a warm-up.
"bash (CC)" is the chain as Claude Code runs it (ugrep, `xargs -P`), the fair baseline from §35.

Final build (`bench/speed_last.txt`, §38), 20,000 files, ms:

| Scenario | bash processes | plain bash | bash (CC) | mt in-process | mt daemon | bash (CC) ÷ daemon |
|---|---|---|---|---|---|---|
| S1 definition + body | 4 | 489.0 | 111.3 | 140.7 | 11.3 | 9.9x |
| S2 every caller + enclosing function | 954 | 1,995.9 | 1,094.9 | 182.0 | 26.1 | 41.9x |
| S3 eight known reads | 8 | 18.4 | 18.7 | 144.4 | 10.7 | 1.8x |
| S4 grep, then read every hit | 608 | 1,472.1 | 418.0 | 401.3 | 83.3 | 5.0x |
| S5 repo layout | 3 | 34.2 | 33.8 | 160.1 | 21.0 | 1.6x |
| S6 one file's outline | 1 | 5.2 | 13.5 | 142.1 | 11.2 | 1.2x (0.5x against plain bash) |
| S7 every construction of a type | 2 | 366.9 | 72.9 | 168.9 | 24.7 | 3.0x |
| S8 one property, all spellings | 2 | 1,153.7 | 234.8 | 1,181.2 | 104.9 | 2.2x |
| S9 a dependency's declarations | 1 | 5.4 | 15.6 | 404.2 | 21.4 | 0.7x |
| S10 rename across 955 files + format | 3 | 656.4 | 431.3 | 641.1 | 293.1 | 1.5x |
| S11 whole read phase | 1,568 | 6,178.5 | 1,955.1 | 1,637.1 | 287.7 | 6.8x |

S11 output: bash 282,008 bytes, mt 27,883 bytes. S11 against bash (CC) at the other sizes: 5.2x at
500 files, 4.0x at 5,000.

Where bash wins: S9 (a grep of three known manifests) and S6 against plain bash. mt's floor is
process start plus a 4 to 5 ms freshness barrier, about 11 ms per call (§32).

How S10 moved across builds against Claude Code's bash: 0.6x in §35 (bash wins), 1.5x in §38
after parallel diffs and an overlapped `git status`.

Several agents on one daemon, 20,000 files, 4 requests each (`bench/speed_last.txt`):

| Agents | mt daemon wall / median / req/s | mt in-process wall | bash (CC) wall |
|---|---|---|---|
| 1 | 327 ms / 23.6 ms / 12.2 | 2,158 ms | 3,321 ms |
| 8 | 608 ms / 32.3 ms / 52.6 | 6,454 ms | 17,280 ms |

Before §35's threading fix, 8 agents took 2,185 ms with a 101.5 ms median: the daemon served one
request at a time.

Hook overhead (§21), median of 20:

| | pass-through | fires |
|---|---|---|
| Python hook | 61.1 ms | 123.5 ms |
| Rust `mt hook` | 6.4 ms | 9.2 ms |

Bash timings varied 10% to 50% between runs (§28, §32). The daemon column was stable.

---

## 7. Results: accuracy against graft 0.21.1 (§36, §39, §43)

Same Relay repo, graft's graph checked in sync, answer keys from an exhaustive regex scan of the
tracked code files.

Single queries after §39's resolver (recall):

| Query | mt | graft |
|---|---|---|
| references to AuditEntry | 21/21 | 19/21 |
| callers of decodeJSON | 19/19 | 19/19 |
| callers of requireMachine (passed as a value) | 2/2 | 0/2 |
| blast radius of WriteAudit, depth 2 | 5/5 direct, 23/23 indirect, 0 false | 1/5, 2/23, 0 false |
| literal `request_id` | 9/9 | 8/9 |
| latency, every query | 9 to 48 ms | 185 to 402 ms |

Before §39, mt's depth-2 blast radius also returned 13 false positives that only shared a name
(§36). graft's answers are 1.5x to 16x smaller in tokens (§36).

Whole workflows: graft lookup then the fewest `sed -n` reads, against mt (§39, §43):

| Task | graft calls · tokens · complete | mt inline calls · tokens · complete | mt map + expand calls · tokens · complete |
|---|---|---|---|
| callers of decodeJSON, with code | 7 · 4,798 · 19/19 | 1 · 6,045 · 19/19 | 2 · 6,422 · 19/19 |
| blast radius of WriteAudit, with code | 4 · 2,530 · 3/28 | 2 · 11,438 · 28/28 | 2 · 8,156 · 24/28 |
| every use of AuditEntry, with code | 13 · 6,263 · 7/7 | 2 · 9,091 · 7/7 | 2 · 3,634 · 7/7 |
| read requireMachine | 9 · 1,250 · 1/1 | 1 · 507 · 1/1 | 1 · 507 · 1/1 |
| how is an audit row written | 6 · 993 · 2/6 | 2 · 5,170 · 6/6 | 2 · 3,571 · 6/6 |

The `mt expand` step here expands every pointer, which is an upper bound. In §20 and §26, models
expanded about 5% of pointers offered. graft answers plain-word questions about code; mt needs a
name. That gap is still open.

---

## 8. What was tried and did not work

These changes were measured and then reversed, rejected, or found to be wrong.

| Change or claim | What the measurement showed | Outcome | Source |
|---|---|---|---|
| Skill alone for adoption | Claude chose mt in 1 of 4 sessions | plugin with hooks: 4 of 4 | §6, §13 |
| Full context for edits | T2 change task cost 56% to 82% more than a one-line `perl` edit (computed) | `--edit` mode, `regex` op with `expect`, later `@@ regex` | §6, §40 |
| Hash scoped to the whole definition | a correct sub-range op was refused, costing a turn and ~4k output tokens | hash vouches for any unchanged sub-range | §9 |
| Empty `hash` treated as a mismatch, repo-wide `gofmt` gate | correct batches refused or rolled back | empty hash = none; `{changed}` placeholders; `baseline` steps | §10 |
| Python hooks | 61 to 124 ms per call, plus a 55 ms sh launcher | Rust `mt hook`, 6 to 11 ms | §21 |
| Denying old-style reads | each denial costs a turn | rewrite in place via `updatedInput` | §21 |
| Denying compound commands | threw away mt calls in the same command, one turn per 255k run | deny only commands that are nothing but old-style reads | §26 |
| One briefing over 10,000 chars | Claude Code spilled it to a file; every plugin run spent turn 1 reading it | skill split across hook events | §26 |
| Built-in formatter table, Go-specific `module` and ENV rules | broke the language-agnostic rule | removed; repo's own commands | §24 |
| `mt undo` with backups | backups cost time on 955-file renames | removed; gate restores from memory | §28 |
| Tool graph built from transcripts | usage history, not the project; a new repo has none | built from code and definitions only | §19 |
| Word ranking tuned on own prompts | 75% @3 on tuning, 24% on independent prompts | independent gold set; BERT embeddings | §30, §33 |
| Graph-propagated embeddings | tied plain text vectors on recall, lost on MRR on held-out prompts | text mode is the default | §33 |
| Component-first seeding | independent set 37 → 34 @3; data-flow prompts 1/13 → 0/13 | code deleted | §41 |
| Gate on "two prompt words in a name" | fixed T2 but cost the independent set 37% → 32% @3 | narrower tool-request rule kept | §41 |
| Embedding seeds without the two-word rule | B-T2 got 4.9k chars of irrelevant code cards; first T2 miss | tool-request prompts get no code cards | §41 |
| One card per file | bench fell on every set; T4 picked the wrong store function | deleted | §45 |
| "T4 store step unsolved" | a scoring bug: every answer cited the store by file:line | scorer fixed; runs 3 to 5 re-scored to 39/39 | §45 |
| "The gate rolls everything back on a failed build" | wrong as stated; mt restores only when a step marked `gate` fails | wording corrected in the skill | §42 |
| `expand` reading the daemon's in-memory text | a stale read, caught by a test | reads disk on a memory miss | §43 |
| Two reindexers | a race wrote stale references | one background reindexer | §38 |
| Daemon serving one request at a time | 8 agents: 101.5 ms median, 2,185 ms wall | threaded: 24.6 ms, 525 ms | §35 |
| Unprompted use of `#n` codes | 1 of 6 runs | open | §44 |

---

## 9. Threats to validity

- **Small n.** Most code-workstream rows are n=1 to 3 per arm. G1 is n=1 per design. The log notes
  run-to-run swings of about 30% in tool calls (§7). The two OFF G1 runs differ by 10% on turns and
  7% on cost (§18).
- **One developer's repositories.** Every task runs on one Go and TypeScript monorepo, and the
  replays use that developer's own sessions. Tasks and scorers were written by the person building
  mt. A scorer bug hid a correct result for three runs (§45).
- **Prompt wording.** T4's ambiguous wording made mt arms look less thorough (§7, §9, §14). Changing
  one clause removed the gap (§20).
- **Context size.** Most code A/B sessions start at about 19k tokens, far below the 188k median in
  real sessions. The large-context result rests on 3 sessions per arm in total (§10, §26).
- **Warm cache.** The `mt tools` runs from §22 on used warm-up sessions so both arms hit a warm
  prompt cache. Real first sessions of the day may differ. §19's parallel run showed how much a
  cold cache moves cost.
- **One model.** All A/B sessions used Opus 5.5. Haiku and Sonnet were used only for probes and one
  live test.
- **A cut run.** Round 3 of the 255k set in §26 never ran: both arms hit the account's 5-hour usage
  limit in their first second.
- **Ranking overfit.** The `mt tools` ranking was tuned on 21 prompts and scores 24% to 37% @3 on
  independent prompts (§33).
- **Moving builds.** Another session edited mt during some runs. G1 run 4 is invalid for that
  reason (§23). Later runs used a frozen plugin copy.
- **Fitted cost.** Interactive costs come from fitted per-token rates, not billed amounts (§16).
- **Timing noise.** Bash timings vary 10% to 50% between runs (§28, §32).
- **Partial reproducibility.** The test repos are private. Several harnesses lived in session
  scratchpads and are not in `bench/` (see section 10).

---

## 10. Reproducing it

| What | Command or file | Notes |
|---|---|---|
| Unit and integration tests | `cargo test --release` | |
| Speed suite | `cargo test --release --test speed -- --ignored --nocapture` | knobs `MT_SPEED_SIZES`, `MT_SPEED_RUNS`; generated fixture, no private repo needed |
| `mt span` accuracy | `python3 bench/bench.py` | repo paths are hard-coded to the developer's checkouts; edit them |
| Scripted traditional vs mt | `python3 bench/agent_bench.py` | same; output in `bench/agent_bench_last.*` |
| `mt tools` A/B, paired | `bench/tools/run6/pairs.sh`, `run.sh`, `analyze.py`, `tasks.json`, `A.settings.json`, `B.settings.json` | expects a repo copy at `run6/repo` and a stub-`graft` `bin/` |
| Ranking | `mt tools bench bench/tools/gold/relay-independent.json` (and the other gold files) | names refer to the Relay repo |
| Lever probes | `bench/tools/probe/run.sh`, `shape.py` | |
| Live virtual-context test | `bench/vctx_live.sh` | |
| G1 transactions | `bench/transactions/g1-run*.md` | edited excerpts, not full transcripts |

Not in `bench/`: the transcript analysis scripts of §1 (`tooluse.py`, `why.py`, `turncost.py`),
the code-workstream A/B harness (`run.sh`, `analyze.py`, `ab/t4.py`, `sites.sh`, `verify_t3.sh`),
the interactive analyzer `live_analyze.py` (§16), the replay scripts of §11, and the graft
comparison scripts (`mt_vs_graft.py`, `depth_truth.py`, `workflow.py`, §36, §39). Those results can
be checked against the tables in `DEVLOG.md` but not re-run from this repository.

To run an A/B on your own code: copy the repository, strip agent-tool config from the copy as in
section 1.2, put a stub for any competing tool first on PATH, and run the same prompt headless with
and without the mt plugin (`claude -p ... --plugin-dir plugin`). Report every run before the means.
