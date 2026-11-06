#!/bin/sh
# Try mt and coach on any repo, and keep up with every push.
#
# Run it from inside the repo you want to evaluate mt on:
#   sh ~/orca/ai_multi_tool/install.sh
#   curl -fsSL https://raw.githubusercontent.com/oshricohenme/ai_multi_tool/main/install.sh | sh
#
# It installs (or updates to) the latest pushed mt and coach for THIS repo only (--scope local:
# .claude/settings.local.json, your other repos are untouched), fetches the binary CI built from
# that push, indexes the repo, warms the mt server, then starts `claude` here.
#
# The loop: change mt, commit, push, run this again. In a session that is already open, type
# /reload-plugins instead of restarting (a new mt binary replaces the old server by itself).
#
#   NO_START=1   do not start claude at the end
#   NO_WAIT=1    do not wait for CI to finish building the binary of the last push
set -eu
GH=oshricohenme/ai_multi_tool
URL=https://github.com/$GH
MKT=ai-multi-tool
REPO=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO"
step() { printf '\n== %s\n' "$*"; }
short() { printf '%.12s' "$1"; }
for c in claude git curl python3; do command -v $c >/dev/null || { echo "install.sh needs $c" >&2; exit 1; }; done

step "repo: $REPO"
main=$(git ls-remote "$URL" refs/heads/main | cut -f1)
echo "mt main is at $(short "$main")"

step "1. the CI binary for the last push"
if [ "${NO_WAIT:-0}" = 1 ]; then echo "skipped (NO_WAIT=1)"
elif ! command -v gh >/dev/null; then echo "skipped (no gh): if you just pushed a Rust change, the binary may be the previous one"
else
  run=$(gh run list -R $GH -w binaries -b main -L 1 --json databaseId,status,conclusion,headSha \
        --jq '.[0] | "\(.databaseId) \(.status) \(.conclusion) \(.headSha)"' 2>/dev/null || true)
  if [ -z "$run" ]; then echo "no CI run found"
  else
    set -- $run
    if [ "$2" != completed ]; then
      echo "CI is building $(short "$4"), waiting (about 5 min; NO_WAIT=1 to skip)..."
      gh run watch "$1" -R $GH --exit-status >/dev/null 2>&1 && echo "built" || echo "WARN: the build failed; the previous binary stays: gh run view $1 -R $GH --log-failed"
    else echo "last build: $(short "$4") ($3)"; fi
  fi
fi
tag=$(git ls-remote "$URL" refs/tags/latest | cut -f1)
echo "the release binary is from $(short "$tag")"

step "2. the marketplace"
if claude plugin marketplace list --json 2>/dev/null | grep -q "\"name\": *\"$MKT\""; then
  claude plugin marketplace update $MKT
else
  claude plugin marketplace add $GH
fi

step "3. mt and coach, for this repo only"
for p in mt coach; do
  # an update when installed, else an install; the version is the commit, so every push is an update
  claude plugin update "$p@$MKT" -s local 2>/dev/null || claude plugin install "$p@$MKT" -s local -y
done
P=$(claude plugin list --json | python3 -c '
import json, sys
d = json.load(sys.stdin); d = d.get("plugins", d) if isinstance(d, dict) else d
mine = [p for p in d if p.get("id") == "mt@" + sys.argv[1]]
here = [p for p in mine if p.get("scope") == "local" and p.get("projectPath") in (None, sys.argv[2])]
print((here or mine or [{}])[0].get("installPath", ""))' "$MKT" "$REPO")
[ -n "$P" ] && [ -x "$P/bin/mt" ] || { echo "mt did not install (claude plugin list --json)" >&2; exit 1; }
echo "mt plugin: $P"

step "4. the binary"
stamp="$P/bin/.release"
if [ "$(cat "$stamp" 2>/dev/null || true)" != "$tag" ]; then
  rm -f "$P"/bin/mt-*
  # a new plugin version, same binary: move it over from the version before instead of downloading
  for d in "$(dirname "$P")"/*/; do d=${d%/}
    if [ ! -L "$d" ] && [ "$d" != "$P" ] && [ "$(cat "$d/bin/.release" 2>/dev/null || true)" = "$tag" ] && ls "$d"/bin/mt-* >/dev/null 2>&1; then
      mv "$d"/bin/mt-* "$P/bin/" && echo "binary unchanged, moved over from $(basename "$d")"; break
    fi
  done
fi
v=$(MT_BIN= PATH=/usr/bin:/bin "$P/bin/mt" --version) || { echo "the binary did not download" >&2; exit 1; }
echo "$tag" > "$stamp"
echo "$v ($(short "$tag"))"
# older versions of the plugin keep their 80 MB binaries: drop them (a session still on one
# downloads again on its next call)
for d in "$(dirname "$P")"/*/; do d=${d%/}; [ -L "$d" ] && continue; [ "$d" = "$P" ] || rm -f "$d"/bin/mt-*; done
# a path that survives updates, for anything outside Claude that should run this mt
CUR="$(dirname "$P")/current"
ln -sfn "$P" "$CUR"

step "5. index this repo and warm the mt server"
MT_BIN= PATH=/usr/bin:/bin "$P/bin/mt" build

step "6. which mt Claude's own commands will run"
# Claude Code puts the plugin's bin/ on PATH, but after your shell profile's own entries
found=$(${SHELL:-/bin/sh} -lc 'command -v mt' 2>/dev/null || true)
real=$([ -n "$found" ] && python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$found" || true)
case "$real" in
  "" | "$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$P")"/*) echo "the plugin's (good)" ;;
  *) echo "WARN: your shell finds $found first, so Claude's mt commands would run that build, not this one."
     echo "      Remove it, or point it at this install (follows every update): ln -sf \"$CUR/bin/mt\" \"$found\"" ;;
esac

step "ready: mt $(short "$main"), binary $(short "$tag"), in $REPO"
echo "next time: push, run this again, then /reload-plugins in an open session"
if [ "${NO_START:-0}" != 1 ] && [ -t 1 ]; then exec claude; fi
