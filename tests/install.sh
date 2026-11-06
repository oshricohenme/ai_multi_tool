#!/bin/sh
# Install mt and coach the way a developer would (marketplace add, plugin install), in a
# throwaway Claude config, then test what they got. Your own ~/.claude and profiles are not touched.
#
#   tests/install.sh                      # from GitHub: oshricohenme/ai_multi_tool
#   tests/install.sh "$PWD"               # from this checkout (a local marketplace)
#   KEEP=1 tests/install.sh               # keep the temp dir to look around
#   LIVE=0 tests/install.sh               # skip the real `claude -p` session
#
# The live session runs the installed copies with --plugin-dir under your own login, ignoring your
# settings and MCP servers (--setting-sources project --strict-mcp-config): nothing is installed there.
set -u
SRC=${1:-oshricohenme/ai_multi_tool}
MKT=ai-multi-tool
REAL_CFG=${CLAUDE_CONFIG_DIR:-}
T=$(mktemp -d "${TMPDIR:-/tmp}/mt-install.XXXXXX")
export CLAUDE_CONFIG_DIR="$T/config"
mkdir -p "$CLAUDE_CONFIG_DIR"
[ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$T"' EXIT
pass=0 fail=0 skip=0
ok() { pass=$((pass + 1)); echo "PASS  $*"; }
no() { fail=$((fail + 1)); echo "FAIL  $*"; }
sk() { skip=$((skip + 1)); echo "SKIP  $*"; }
step() { echo; echo "== $*"; }
# run a command, print its output indented, return its exit code
run() { out=$("$@" 2>&1); rc=$?; printf '%s\n' "$out" | sed 's/^/    /' | tail -20; return $rc; }

echo "source: $SRC"
echo "config: $CLAUDE_CONFIG_DIR (throwaway)"

step "1. add the marketplace"
run claude plugin marketplace add "$SRC" && ok "marketplace add $SRC" || no "marketplace add $SRC"

step "2. install both plugins"
for p in mt coach; do
  run claude plugin install "$p@$MKT" -y && ok "install $p@$MKT" || no "install $p@$MKT"
done

step "3. both listed, with their install paths"
claude plugin list --json > "$T/list.json" 2>"$T/list.err" || no "plugin list: $(cat "$T/list.err")"
path_of() { python3 -c '
import json,sys
d=json.load(open(sys.argv[1]))
d=d.get("plugins",d) if isinstance(d,dict) else d
for p in d:
    if str(p.get("id",p.get("name",""))).split("@")[0]==sys.argv[2]:
        print(p.get("installPath") or p.get("path") or ""); break
' "$T/list.json" "$1"; }
MT=$(path_of mt); COACH=$(path_of coach)
[ -n "$MT" ] && [ -d "$MT" ] && ok "mt at $MT" || no "mt not listed (see $T/list.json)"
[ -n "$COACH" ] && [ -d "$COACH" ] && ok "coach at $COACH" || no "coach not listed"
[ -n "$MT" ] && [ -d "$MT" ] || { echo; echo "cannot go on without mt installed"; echo "$pass passed, $fail failed"; exit 1; }

step "4. the installed copies validate"
for p in mt coach; do
  d=$(path_of $p); o=$(claude plugin validate "$d" 2>&1) && ok "validate $p: $(echo "$o" | tail -1)" || { no "validate $p"; echo "$o" | sed 's/^/    /' | grep -E '✘|error' ; }
done

step "5. first run of bin/mt downloads the binary (mt hidden from PATH)"
L="$MT/bin/mt"
[ -x "$L" ] && ok "launcher is executable" || no "launcher missing or not executable: $L"
v=$(env -u MT_BIN PATH=/usr/bin:/bin "$L" --version 2>&1) && ok "bin/mt --version: $v" || no "bin/mt --version: $v"
ls "$MT"/bin/mt-* >/dev/null 2>&1 && ok "binary cached: $(ls "$MT"/bin/mt-* | xargs -n1 basename) ($(du -h "$MT"/bin/mt-* | cut -f1 | head -1))" \
  || no "no bin/mt-<target> after first run (download failed?)"
v2=$(env -u MT_BIN PATH=/usr/bin:/bin "$L" --version 2>&1) && [ "$v2" = "$v" ] && ok "second run uses the cache" || no "second run: $v2"

step "6. mt works on a small repo"
R="$T/repo"; mkdir -p "$R"
printf 'def add(a, b):\n    return a + b\n\n\ndef total(xs):\n    return sum(add(x, 0) for x in xs)\n' > "$R/calc.py"
(cd "$R" && git init -q && git add . && git -c user.name=t -c user.email=t@t commit -qm init)
o=$(cd "$R" && PATH=/usr/bin:/bin "$L" context add --map 2>&1) && echo "$o" | grep -q "calc.py" \
  && ok "mt context add --map finds calc.py" || { no "mt context add --map"; echo "$o" | sed 's/^/    /' | head -20; }

step "7. the hooks run as Claude Code would run them"
for h in session-start prompt session-end; do
  o=$(cd "$R" && printf '{"session_id":"install-test","cwd":"%s","hook_event_name":"x","prompt":"hi"}' "$R" \
      | CLAUDE_PLUGIN_ROOT="$MT" PATH=/usr/bin:/bin sh -c '"${CLAUDE_PLUGIN_ROOT}/bin/mt" hook '"$h" 2>&1)
  rc=$?
  [ $rc = 0 ] && ok "hook $h (exit 0, $(printf '%s' "$o" | wc -c | tr -d ' ') chars out)" || { no "hook $h exit $rc"; echo "$o" | sed 's/^/    /' | head -10; }
done

step "8. the mods' own tests, in the installed copies"
for p in mt coach; do
  d=$(path_of $p); o=$(claude plugin test "$d" 2>&1) && ok "plugin test $p: $(echo "$o" | grep -E '^ *[0-9]+ pass' | tr -d ' ')" || { no "plugin test $p"; echo "$o" | sed 's/^/    /' | tail -20; }
done

step "9. a real headless session with both plugins"
if [ "${LIVE:-1}" = 0 ]; then sk "LIVE=0"
else
  claude_bin=$(command -v claude)
  o=$(cd "$R" && if [ -n "$REAL_CFG" ]; then export CLAUDE_CONFIG_DIR="$REAL_CFG"; else unset CLAUDE_CONFIG_DIR; fi
      PATH=/usr/bin:/bin "$claude_bin" -p "Where is the function add defined, and who calls it? Answer as file:line lines." \
      --plugin-dir "$MT" --plugin-dir "$COACH" --setting-sources project --strict-mcp-config \
      --output-format json --max-turns 8 2>&1)
  echo "$o" > "$T/live.json"
  echo "$o" | grep -q "calc.py" && ok "the answer names calc.py" || no "answer (see $T/live.json)"
  [ -d "$R/.mt" ] && ok "mt indexed the repo during the session (.mt/ exists)" || no "no .mt/ in the repo: mt's hooks did not run"
fi

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "${KEEP:-0}" = 1 ] && echo "kept: $T"
[ $fail = 0 ]
