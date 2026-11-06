#!/bin/bash
# one mt OFF run with context management. usage: run_cm.sh <terminal> <worktree-name>
S="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; T=$1; W=$2
STRIP='^(\.claude|\.agents|\.codex|\.cursor|\.gemini|\.kiro|\.mcp\.json|AGENTS\.md|GEMINI\.md|graft|opencode\.json)(/|$)'
cd ~/orca/workspaces/claude_coach/$W && git reset -q && { git diff --name-only | grep -Ev "$STRIP" | xargs git checkout -- 2>/dev/null; git clean -fdxq; }
echo "$W: $(git status --short | grep -v '^ D' | wc -l | tr -d ' ') changes"
orca terminal send --terminal $T --text "/exit" --enter --json >/dev/null; sleep 5
for k in 1 2 3; do orca terminal read --terminal $T --json | python3 -c 'import sys,json;t=json.load(sys.stdin)["result"]["terminal"]["tail"];sys.exit(0 if any("git:(" in l for l in t[-2:]) else 1)' && break; orca terminal send --terminal $T --text $'\r' --json >/dev/null; sleep 2; done
orca terminal send --terminal $T --text "cd ~/orca/workspaces/claude_coach/$W && CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 PATH=$S/stub:\$PATH claude --model claude-opus-5-5 --setting-sources project --strict-mcp-config --permission-mode bypassPermissions --append-system-prompt-file $S/prompts/cm-system.md" --enter --json >/dev/null; sleep 9
orca terminal send --terminal $T --text $'\e[B' --json >/dev/null; sleep 0.5; orca terminal send --terminal $T --text $'\r' --json >/dev/null; sleep 8
orca terminal read --terminal $T --json | python3 -c 'import sys,json;t=json.load(sys.stdin)["result"]["terminal"]["tail"];print("ready" if any("❯" in l for l in t) else "\n".join(t[-5:]))'
orca terminal send --terminal $T --text "I want to be able to add google authentication, please evaluate and add it along with tests" --enter --wait-submit 10 --json >/dev/null; date
sleep 3; orca terminal send --terminal $T --text "No database is available on this machine: do not try to start Postgres or Docker, and do not run the database-backed tests. Write them, run everything else." --enter --json >/dev/null; date
