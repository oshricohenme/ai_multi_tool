#!/bin/bash
# exits when mt's code and the other sessions have been quiet for QUIET seconds (checks every minute, up to 3 h)
QUIET=${QUIET:-600}; ME=d8c6cc34-3ec3-4e65-a074-8faf18b37a91
for i in $(seq 1 180); do
  now=$(date +%s)
  code=$(find ~/orca/mt/src ~/orca/mt/plugin ~/orca/mt/skills ~/orca/mt/Cargo.toml -type f -exec stat -f %m {} + 2>/dev/null | sort -n | tail -1)
  # other Claude sessions working in this project (any transcript but mine and the eval worktrees)
  sess=$(find ~/.claude/projects/-Users-oshricohen-orca-claude-coach ~/.claude/projects/-Users-oshricohen-orca-mt -maxdepth 1 -name '*.jsonl' ! -name "$ME.jsonl" -exec stat -f %m {} + 2>/dev/null | sort -n | tail -1)
  last=$(( code > ${sess:-0} ? code : ${sess:-0} ))
  idle=$(( now - last ))
  echo "$(date +%H:%M:%S) code idle $((now-code))s, other sessions idle $((now-${sess:-0}))s"
  [ $idle -ge $QUIET ] && { echo "QUIET for ${idle}s"; exit 0; }
  sleep 60
done
echo "still active after 3h"; exit 1
