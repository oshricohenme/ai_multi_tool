#!/bin/bash
# Live test of the virtual context (DEVLOG 44): a real headless Claude session with the mt hooks,
# in a scratch Go repo. The task never mentions codes or `mt show`; the transcript shows whether
# the model uses them. Then the session is compacted and resumed, to show the working set.
#   bench/vctx_live.sh [model]   (default sonnet). MT=<binary> overrides the mt build.
set -u
MODEL=${1:-sonnet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
MT=${MT:-$HERE/target-vctx/release/mt}
W=$(mktemp -d /tmp/mt-vctx-live.XXXX)
mkdir -p "$W/repo" && cd "$W/repo" && git init -q
cat > go.mod <<'EOF'
module example.com/text

go 1.21
EOF
cat > util.go <<'EOF'
package text

import "strings"

// Slugify lowercases s and joins its words with dashes.
func Slugify(s string) string {
	var b strings.Builder
	dash := false
	for _, r := range strings.ToLower(s) {
		if isSep(r) {
			dash = b.Len() > 0
			continue
		}
		if dash {
			b.WriteRune('-')
			dash = false
		}
		b.WriteRune(r)
	}
	return b.String()
}

func isSep(r rune) bool {
	return r == ' ' || r == '_' || r == '-' || r == '.'
}

// Title upper-cases the first letter of s.
func Title(s string) string {
	if s == "" {
		return s
	}
	return strings.ToUpper(s[:1]) + s[1:]
}

// Words splits s on separators.
func Words(s string) []string {
	return strings.FieldsFunc(s, isSep)
}
EOF
cat > util_test.go <<'EOF'
package text

import "testing"

func TestSlugify(t *testing.T) {
	if got := Slugify("Hello World_again"); got != "hello-world-again" {
		t.Fatalf("got %q", got)
	}
}
EOF
git add -A && git commit -qm init

cat > "$W/settings.json" <<EOF
{"hooks": {
 "SessionStart": [{"matcher": "startup|resume|clear|compact", "hooks": [{"type": "command", "command": "$MT hook session-start", "timeout": 20}]}],
 "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "$MT hook prompt", "timeout": 5}]}],
 "PreToolUse": [{"matcher": "Bash|Read|Grep|Edit|Write|MultiEdit", "hooks": [{"type": "command", "command": "$MT hook pretool", "timeout": 10}]}],
 "PostToolUse": [{"matcher": "Bash|Read|mcp__.*", "hooks": [{"type": "command", "command": "$MT hook posttool", "timeout": 10}]}],
 "SessionEnd": [{"hooks": [{"type": "command", "command": "$MT hook session-end", "timeout": 10}]}]
}}
EOF
export PATH="$(dirname "$MT"):$PATH"
run() { # $1 = label, rest = claude args; prompt on stdin
  local label=$1; shift
  timeout 600 claude -p --model "$MODEL" --settings "$W/settings.json" --setting-sources project \
    --output-format stream-json --verbose --allowedTools "Bash" "$@" > "$W/$label.jsonl" 2> "$W/$label.err"
}

TASK="Move Slugify and its helper isSep out of util.go into a new file slug.go in the same package (Words still needs isSep). Run go vet and go test. Then give me a table of every function in the package with its file and line count."
echo "=== workdir $W, model $MODEL, mt $MT"
echo "=== TASK: $TASK"
echo "$TASK" | run task
SID=$(python3 -c "import json,sys;print(next(json.loads(l)['session_id'] for l in open(sys.argv[1]) if '\"session_id\"' in l))" "$W/task.jsonl")
echo "/compact" | run compact --resume "$SID"
echo "What exactly did you change before the compaction? Name the files and the current span of Slugify, and check it with one mt call. Be brief." | run after --resume "$SID"

python3 - "$W" <<'EOF'
import json, sys, os, glob
W = sys.argv[1]
def show(label):
    print(f"\n################ {label} ################")
    usage = None
    for l in open(f"{W}/{label}.jsonl"):
        r = json.loads(l)
        t = r.get("type")
        if t == "system" and r.get("subtype") == "informational":
            print(f"\n[USER-ONLY systemMessage, {len(r['content'])} chars]\n" + r["content"])
        elif t == "assistant":
            for b in r["message"]["content"]:
                if b["type"] == "text" and b["text"].strip(): print(f"\n[model text, {len(b['text'])} chars]\n{b['text']}")
                if b["type"] == "tool_use": print(f"\n[model -> {b['name']}, {len(json.dumps(b['input']))} chars of input]\n{b['input'].get('command', json.dumps(b['input']))}")
        elif t == "user":
            c = r["message"]["content"]
            for b in c if isinstance(c, list) else []:
                if b.get("type") == "tool_result":
                    txt = b["content"] if isinstance(b["content"], str) else json.dumps(b["content"])
                    print(f"\n[tool result -> model, {len(txt)} chars]\n" + txt[:1800] + ("\n…(cut for this printout)" if len(txt) > 1800 else ""))
        elif t == "result":
            usage = r
    if usage:
        u = usage["usage"]
        print(f"\n[{label}: {usage.get('num_turns')} turns, ${usage.get('total_cost_usd', 0):.4f}, output {u.get('output_tokens')} tokens, cache read {u.get('cache_read_input_tokens')}, cache write {u.get('cache_creation_input_tokens')}]")
for label in ("task", "compact", "after"): show(label)
# what the SessionStart(compact) hook put in context: from the session transcript
tr = glob.glob(os.path.expanduser("~/.claude/projects/*/*.jsonl")) + glob.glob(os.environ.get("CLAUDE_CONFIG_DIR", "/nonexistent") + "/projects/*/*.jsonl")
sid = next(json.loads(l)["session_id"] for l in open(f"{W}/task.jsonl") if '"session_id"' in l)
for f in tr:
    if sid in f:
        for l in open(f):
            if "Working set from before the compaction" in l:
                s = l[l.index("## Working set"):]
                print("\n################ injected at SessionStart(compact) ################\n" + s[:1500].encode().decode("unicode_escape", "ignore").split("The mt reference follows")[0])
                break
        break
print("\n################ repo after ################", flush=True)
os.system(f"cd {W}/repo && git status --short && echo '--- slug.go' && cat slug.go && echo '--- util.go' && cat util.go && go vet ./... && go test ./... 2>&1 | tail -2")
EOF
