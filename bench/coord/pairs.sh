#!/bin/bash
# paired: each task/rep runs its A and B side by side; one warm-up per arm first
E=$(cd "$(dirname "$0")" && pwd); rm -rf $E/runs; mkdir -p $E/runs
for arm in A B; do (cd $E/repo && claude -p "Reply with the single word ok." --model opus --settings "$E/settings.json" --plugin-dir "$E/plugin" --output-format json < /dev/null > /dev/null 2>&1) & done; wait; echo "warm-ups done $(date +%T)"
for task in ${TASKS:-R T3}; do for rep in $(seq 1 ${REPS:-3}); do $E/run.sh A $task $rep & $E/run.sh B $task $rep & wait; done; done
echo "ALL DONE $(date +%T)"
