# Claude coach rules, v2 (2026-11-08)

The best-practice catalog the coach detects. It comes from three research passes:
- the official Claude Code docs and changelog up to v2.1.319;
- Anthropic engineering posts and staff (Boris Cherny, Thariq Shihipar, the code review post);
- community practitioners, kept only where the advice is widely repeated or agrees with the docs.

It extends the 36 checks already in `web/coach.js`.

Where each rule shows up:
- **live**: the terminal nudge band, the moment it happens. One nudge at a time, dismissible, remembered.
- **post**: a finding on the web Coach page, computed over the whole session.
- **setup**: a check of the repo and user config files (CLAUDE.md, settings, agents, skills, hooks), shown in the Coach "Setup" group.

Severity: high / medium / low. "Saving" applies only to cost rules.

Sources: B = https://code.claude.com/docs/en. "Boris" = Boris Cherny's tips thread, Jan 2 2026. "Thariq" = Thariq Shihipar, Mar and Jul 2026.

## New rules

| id | where | sev | detect | nudge / fix | source |
|---|---|---|---|---|---|
| `test-tampering` | live + post | high | In one turn: a test/build command fails, then a file matching `test\|spec\|__tests__\|_test\.\|\.test\.\|snap` is edited, then the same command passes, with no non-test source edit in between. Also: an edit that adds `skip\|xit\|\.only\|@Ignore\|pytest.mark.skip` or removes more `assert\|expect` lines than it adds. Skip when that turn's prompt asks for tests, or the file is new. | "Claude changed a test right after it failed. Check: was the test wrong, or the code?" Fix: say "Don't change tests; fix the implementation." | community (Dolthub 2025-06; widely reported); B/best-practices (avoid hardcoding to pass tests) |
| `same-failure-loop` | live + post | medium | The same tool and normalised input fails 3 or more times in one turn. | "Claude ran the same failing command 3 times. Esc and say what to try differently." | community (Ralph loops); B/best-practices (course-correct early) |
| `no-plan-big-change` | post | medium | A turn edits 6 or more distinct files, its first assistant step is an edit, permission_mode was never `plan`, there was no ExitPlanMode or `/plan`, and a correction followed within 3 turns. | "That was a 7-file change with no plan. Next time: `/plan <task>` (or Shift+Tab) and approve the file list first." | Boris; B/common-workflows#plan-before-editing |
| `continue-without-goal` | live + post | medium | The user types "continue / keep going / go on / next" 2 or more times after turns that ended with "next I will…" or "remaining:", and no `/goal` is active. | "Saying 'continue' again? `/goal <check> passes, or stop after 20 turns` keeps it going on its own." | B/goal; Opus 5.5 prompting guide (unattended runs) |
| `undo-by-prose` | live + post | low | A prompt matching `^(undo\|revert\|go back\|put it back\|restore)` with no `/rewind` in the session. | "Esc Esc (or `/rewind`) restores code and conversation exactly; it's cheaper than asking Claude to undo." | B/checkpointing; Cat Wu and Boris interview, Oct 2025 |
| `correction-not-saved` | post | medium | `correction-loop` fired, and no Edit/Write to a CLAUDE.md or memory file happened later in the session. | "You corrected this twice. Ask Claude: 'add a rule about this to CLAUDE.md' so it sticks." | Boris; B/memory#when-to-add-to-claudemd |
| `rule-lost-at-compaction` | post | medium | After a `compact`, the user re-types a rule-like sentence (always/never/don't/use X) that was also said before the compact (token overlap ≥ 0.8). | "That instruction was lost at compaction. Put it in the root CLAUDE.md so it reloads." | B/context-window#what-survives-compaction |
| `context-high-mid-task` | live | low | session.measure context ≥ 70% of the window while the last 2 turns are the same task. Once per session; suppressed after a compact. | "Context at 70%. At the next break: `/compact keep <what matters>`, or write PROGRESS.md and `/clear`." | B/best-practices (compact at a natural break); community (document and clear) |
| `fast-mode-toggle` | live + post | medium | Fast mode turned on when context > 30k, or toggled 2 or more times in a session. | "Fast mode has its own cache: turning it on now re-reads {k} at fast prices. Enable it at session start." | B/fast-mode; B/prompt-caching#turning-on-fast-mode |
| `bypass-no-sandbox` | live + setup | high | permission_mode `bypassPermissions` with no `sandbox.enabled` in any settings file. | "Bypass mode with no sandbox. Use auto mode (Shift+Tab), or turn on `/sandbox`, or run in a container." | B/permission-modes; Anthropic sandboxing post (Oct 2025); Willison, "lethal trifecta" |
| `auto-mode-block-streak` | live | medium | 3 consecutive classifier PermissionDenied (auto mode pauses at 3 in a row or 20 total). | "Auto mode blocked 3 in a row and will pause. `/auto-mode-setup` tells it what you trust." | B/permission-modes#when-auto-mode-falls-back |
| `repeated-style-asks` | post | low | 3 or more prompts with the same style request ("shorter", "be brief", "no preamble", "explain each change"). | "`/output-style concise` (or explanatory) instead of asking every time." | B/output-styles |
| `manual-formatter` | post | low | Bash runs prettier/biome/black/ruff format/gofmt/rustfmt 3 or more times, with no PostToolUse Edit\|Write hook. | "Add a PostToolUse hook on Edit\|Write that runs your formatter." | Boris; B/hooks-guide |
| `exploration-in-main` | post | low | 8 or more consecutive Read/Grep/Glob calls in the main thread, totalling over 30k tokens, with no agent spawn in the turn. | "Claude read {n} files here (~{k}). Next time: 'use a subagent to find X and report in 10 lines.'" | B/costs#delegate-verbose-operations; community (Shrivu) |
| `generated-dir-reads` | post | low | Read/Grep results from `node_modules/`, `dist/`, `build/`, `vendor/`, `*.min.js` or lockfiles over 5k tokens. | Add `"deny": ["Read(./node_modules/**)", "Read(./dist/**)"]`. | B/large-codebases |
| `long-session-no-checkpoint` | post | low | 2 or more compactions or 3h or more of wall time, with no git commit and no PROGRESS/NOTES/PLAN file written. | "Long run with no commits or notes. Commit per step and keep PROGRESS.md so a compact can't lose state." | Anthropic, "effective harnesses for long-running agents" (Nov 2025) |
| `rate-limit-with-background` | live | medium | 5-hour limit ≥ 80% while a /loop, background agent or 3 or more subagents are running. | "5-hour limit at {p}%. Background work is still spending it: stop the /loop, or lower /effort." | B/costs |

## Setup rules (read files: repo CLAUDE.md files, `.claude/settings*.json`, `~/.claude/settings.json`, `.claude/agents`, `.claude/skills`, the auto-memory MEMORY.md)

| id | sev | detect | fix | source |
|---|---|---|---|---|
| `setup-no-check-command` | high | No test/typecheck/lint/build command in any loaded CLAUDE.md and no Stop or PostToolUse hook, in a repo with code. | "Add a `## Commands` section (test, typecheck, lint). It's the single most useful line in CLAUDE.md." | B/best-practices; Boris (verification is his top tip) |
| `setup-secrets-not-denied` | medium | `.env*`, `secrets/` or `*.pem` exists in the repo and no matching `Read(...)` deny. | `"deny": ["Read(./.env)", "Read(./secrets/**)"]` holds in every mode. | B/best-practices#limit-access |
| `setup-never-rules-unenforced` | medium | A CLAUDE.md line like `(never\|don't\|do not) (edit\|touch\|push\|commit\|delete\|read) <path\|command>` with no matching deny rule or hook. | "CLAUDE.md says never X, but nothing enforces it. Make it a deny rule or a hook." | B/best-practices#enforce-the-rules-that-cant-have-exceptions |
| `setup-automode-misconfig` | high | An `autoMode` block in project settings (ignored there), or an autoMode list without `"$defaults"` (silently drops the built-in blocks). | "Move autoMode to ~/.claude/settings.json and keep \"$defaults\" in each list; `/auto-mode-setup`." | B/auto-mode-config |
| `setup-claude-md-shouting` | low | More than 5 ALL-CAPS emphasis lines (MUST/NEVER/CRITICAL/IMPORTANT) in CLAUDE.md, or "think hard"/"be thorough" lines. | "Newer models over-apply emphasis. State the reason instead; make hard rules hooks." | B/best-practices#what-you-can-stop-doing; Thariq (Jul 2026) |
| `setup-claude-md-dead-refs` | low | Paths mentioned in CLAUDE.md that don't exist, or `@` imports of files over 400 lines. | "Remove dead references; link big docs as plain paths so they load on demand. `/doctor prompt-audit` finds more." | B/memory; community |
| `setup-memory-over-limit` | medium | Auto-memory MEMORY.md over 200 lines or 25 KB: later lines aren't loaded. | "Consolidate MEMORY.md under 200 lines." | B/memory |
| `setup-compact-instructions` | low | Auto-compactions in past sessions and no "Compact instructions" heading in CLAUDE.md. | Add `# Compact instructions` naming what to keep. | B/costs#manage-context-proactively |
| `setup-agent-defs` | low | A `.claude/agents/*.md` with only read-only tools but no `model:` (inherits the big model), or no `tools:` line. | "Set `model: haiku` and `tools: Read, Grep, Glob` for read-only agents." | B/sub-agents |
| `setup-side-effect-skills` | medium | A skill whose body runs push/deploy/publish/migrate/`rm -rf` without `disable-model-invocation: true`. | "Add `disable-model-invocation: true` so it only runs when you call it." | B/skills#control-who-invokes-a-skill |

## Refinements to existing checks
- `obsolete-incantations`:
  - Allowlist `ultrathink`, `ultracode` and "be ambitious"; these still work.
  - Add "keep going until done", "don't be lazy", "don't stop", and lines that are entirely ALL CAPS.
  - `budget_tokens` is gone; effort is the control.
- `model-switch`:
  - Also count an effort change, but only on models where it still breaks the cache. On Opus/Sonnet/Haiku 5.5 and Fable 5.1, with an API key or subscription, effort changes are cache-safe.
  - Count `opusplan` toggles.
- `destructive-commands`: also `git commit --no-verify` and `git push --force`. Escalate when the tree was dirty before.
- `costly-compact`: a manual `/compact` with no focus text on a large context ("say what to keep").
- `overthinking`: read the model's default effort from the model. The 5.5 models default to medium; don't hard-code a level.
- `idle-mcp`: tool search defers schemas now, so the per-server cost is names plus instructions. Lower the weight.
- `repeated-approval`: name the exact allow rule (e.g. `Bash(npm test:*)`). Mention `/fewer-permission-prompts` when in Manual mode.

## Considered and not built
- **Undetectable:** IDE/desktop/terminal ergonomics, branch protection, whether the user read the diff, spec quality, admin and managed settings.
- **Low confidence, single source, or advice that changes too fast:** queueing messages instead of interrupting, "describe the outcome, not the steps", worktree overlap across sessions, idle-thinking bug versions, model-downgrade nagging.
  - Boris runs the biggest model for everything, so the coach doesn't push downgrades.
- **Numbers to re-check in product copy before quoting:** "84% fewer prompts" (sandbox), "~7x tokens" (agent teams), "~15x" (multi-agent).
