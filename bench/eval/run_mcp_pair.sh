#!/bin/bash
# one paired MCP A/B: A = native MCP (mt-eval-off), B = mt plugin + skill (mt-eval-on), same task, side by side
# usage: run_mcp_pair.sh <task id>
S="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
$S/plugin-frozen/bin/mt --version >/dev/null 2>&1 || { echo "plugin-frozen/bin/mt does not run (exit $?): recopy with rm + cp"; exit 1; }
P=$(grep "^$1|" ${TASKS:-$S/tasks_mcp.txt} | cut -d'|' -f2-); [ -n "$P" ] || { echo "no task $1"; exit 1; }
STRIP='^(\.claude|\.agents|\.codex|\.cursor|\.gemini|\.kiro|\.mcp\.json|AGENTS\.md|GEMINI\.md|graft|opencode\.json)(/|$)'
COMMON="--model claude-opus-5-5 --setting-sources project --strict-mcp-config --mcp-config ${MCPCFG:-$S/mcp-ab.json} --permission-mode bypassPermissions"
launch() { # terminal worktree command-prefix extra-flags
  local T=$1 W=$2
  (cd ~/orca/workspaces/claude_coach/$W && git reset -q && { git diff --name-only | grep -Ev "$STRIP" | xargs git checkout -- 2>/dev/null; git clean -fdxq; })
  orca terminal send --terminal $T --text "/exit" --enter --json >/dev/null; sleep 5
  for k in 1 2 3; do orca terminal read --terminal $T --json | python3 -c 'import sys,json;t=json.load(sys.stdin)["result"]["terminal"]["tail"];sys.exit(0 if any("git:(" in l for l in t[-2:]) else 1)' && break; orca terminal send --terminal $T --text $'\r' --json >/dev/null; sleep 2; done
  orca terminal send --terminal $T --text "cd ~/orca/workspaces/claude_coach/$W && $3 CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 claude $COMMON $4" --enter --json >/dev/null; sleep 10
  orca terminal send --terminal $T --text $'\e[B' --json >/dev/null; sleep 0.5; orca terminal send --terminal $T --text $'\r' --json >/dev/null; sleep 8
  orca terminal read --terminal $T --json | python3 -c 'import sys,json;t=json.load(sys.stdin)["result"]["terminal"]["tail"];print("'$W' ready" if any("❯" in l for l in t) else "\n".join(t[-6:]))'
}
launch ${T_OFF:?set T_OFF to the Orca terminal id of the mt-off worktree} mt-eval-off "" "" &
launch ${T_ON:?set T_ON to the Orca terminal id of the mt-on worktree} mt-eval-on "MT_INJECT_SKILL=0 MT_MCP_CONFIG=${MCPCFG:-$S/mcp-ab.json}" "--plugin-dir $S/plugin-frozen --append-system-prompt-file $S/mt-skill-now.md" &
wait
for T in ${T_OFF:?set T_OFF to the Orca terminal id of the mt-off worktree} ${T_ON:?set T_ON to the Orca terminal id of the mt-on worktree}; do orca terminal send --terminal $T --text "$P" --enter --wait-submit 10 --json >/dev/null & done; wait; echo "$1 sent $(date +%T)"
