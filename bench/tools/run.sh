#!/bin/bash
# usage: run.sh <arm A|B> <task T1|T2|T3> <rep>   one headless session in a fresh repo copy
E=${MT_EVAL_DIR:-$(cd "$(dirname "$0")" && pwd)}; arm=$1; task=$2; rep=$3; label=$arm-$task-r$rep
dir=$E/runs/$label; rm -rf "$dir"; mkdir -p "$E/runs"; cp -R "$E/repo" "$dir"
prompt=$(python3 -c "import json;print(json.load(open('$E/tasks.json'))['$task'])")
sid=$(python3 -c 'import uuid;print(uuid.uuid4())')
cd "$dir"
t0=$(python3 -c 'import time;print(time.time())')
PATH="$E/bin:$HOME/orca/mt/target/release:$PATH" claude -p "$prompt" --session-id $sid --settings "$E/$arm.settings.json" \
  --permission-mode bypassPermissions --output-format stream-json --verbose > "$E/runs/$label.stream.jsonl" 2> "$E/runs/$label.err"
t1=$(python3 -c 'import time;print(time.time())')
git -C "$dir" status --porcelain > "$E/runs/$label.changes"; git -C "$dir" diff > "$E/runs/$label.diff"
echo "{\"label\":\"$label\",\"sid\":\"$sid\",\"wall\":$(python3 -c "print(round($t1-$t0,1))"),\"dir\":\"$dir\"}" > "$E/runs/$label.meta.json"
echo "done $label wall=$(python3 -c "print(round($t1-$t0,1))")s"
