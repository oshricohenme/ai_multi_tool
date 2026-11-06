#!/bin/bash
# usage: run.sh <arm A|B> <task> <rep>. A: no automatic brief (MT_AUTO_BRIEF=0); B: the hook briefs the subagent.
# Both arms: the same plugin (target-coord build), so agent identity is on in both; only the brief differs.
E=$(cd "$(dirname "$0")" && pwd); arm=$1; task=$2; rep=$3; label=$arm-$task-r$rep
dir=$E/runs/$label; rm -rf "$dir"; mkdir -p "$E/runs"; cp -R "$E/repo" "$dir"
prompt=$(python3 -c "import json;print(json.load(open('$E/tasks.json'))['$task'])")
sid=$(python3 -c 'import uuid;print(uuid.uuid4())')
cd "$dir"; "$E/plugin/bin/mt" index > /dev/null 2>&1
brief=1; [ "$arm" = A ] && brief=0
t0=$(python3 -c 'import time;print(time.time())')
PATH="$E/plugin/bin:$PATH" MT_AUTO_BRIEF=$brief claude -p "$prompt" --model opus --session-id $sid --settings "$E/settings.json" --plugin-dir "$E/plugin" \
  --permission-mode bypassPermissions --output-format stream-json --verbose < /dev/null > "$E/runs/$label.stream.jsonl" 2> "$E/runs/$label.err"
t1=$(python3 -c 'import time;print(time.time())')
pkg=./...; [ "$task" = R ] && pkg=./internal/platform/ratelimit/
(PATH="$E/plugin/bin:$PATH"; go build ./... && go vet $pkg && go test $pkg) > "$E/runs/$label.check.out" 2>&1 && echo ok > "$E/runs/$label.check" || echo failed > "$E/runs/$label.check"
git status --porcelain --untracked-files=all | grep -v '^?? .mt/' > "$E/runs/$label.changes"; git diff > "$E/runs/$label.diff"
echo "{\"label\":\"$label\",\"sid\":\"$sid\",\"wall\":$(python3 -c "print(round($t1-$t0,1))"),\"dir\":\"$dir\"}" > "$E/runs/$label.meta.json"
echo "done $label wall=$(python3 -c "print(round($t1-$t0,1))")s check=$(cat $E/runs/$label.check)"
