#!/bin/bash
S="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for t in "$@"; do
  bash $S/run_mcp_pair.sh $t || exit 1
  sleep 45
  for T in ${T_OFF:?set T_OFF to the Orca terminal id of the mt-off worktree} ${T_ON:?set T_ON to the Orca terminal id of the mt-on worktree}; do orca terminal wait --terminal $T --for tui-idle --timeout-ms 1800000 --json >/dev/null; done
  for W in off on; do echo "$t $W $(ls -t ~/.claude/projects/-Users-oshricohen-orca-workspaces-claude-coach-mt-eval-$W/*.jsonl | head -1)"; done
done
