#!/bin/sh
# Install Claude coach (inspect every model call and tool call) and keep it updated in the background.
#
#   sh ~/orca/ai_multi_tool/install.sh
#   curl -fsSL https://raw.githubusercontent.com/oshricohenme/ai_multi_tool/main/install.sh | sh
#
# It adds the ai-multi-tool marketplace, installs (or updates) the coach plugin, and turns on
# marketplace auto-update so new pushes arrive by themselves.
#
#   SCOPE=user|project|local   user (default): all your repos. project: this repo, shared via
#                              .claude/settings.json. local: this repo only, .claude/settings.local.json
#   NO_START=1                 do not start claude at the end
#
# The mt plugin (a Rust binary from GitHub Releases) is hidden from the marketplace for now. Its
# install steps (CI binary wait, release tag, binary download, `mt build`, PATH check, MT_DEBUG)
# were removed from this script; they stay in git history.
set -eu
GH=oshricohenme/ai_multi_tool
MKT=ai-multi-tool
SCOPE=${SCOPE:-user}
step() { printf '\n== %s\n' "$*"; }
for c in claude python3; do command -v $c >/dev/null || { echo "install.sh needs $c" >&2; exit 1; }; done

case "$SCOPE" in
  user) SETTINGS=${HOME}/.claude/settings.json ;;
  project | local)
    REPO=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
    if [ "$SCOPE" = project ]; then SETTINGS=$REPO/.claude/settings.json; else SETTINGS=$REPO/.claude/settings.local.json; fi ;;
  *) echo "SCOPE must be user, project or local" >&2; exit 1 ;;
esac

step "1. the marketplace"
if claude plugin marketplace list --json 2>/dev/null | grep -q "\"name\": *\"$MKT\""; then
  claude plugin marketplace update $MKT
else
  claude plugin marketplace add $GH
fi

step "2. coach (scope: $SCOPE)"
# an update when installed, else an install
claude plugin update "coach@$MKT" -s "$SCOPE" 2>/dev/null || claude plugin install "coach@$MKT" -s "$SCOPE"

step "3. auto-update: $SETTINGS"
# Claude Code has no flag for this; merge extraKnownMarketplaces.<name>.autoUpdate by hand,
# keep every other key, write atomically, keep the old file as .bak
python3 - "$SETTINGS" "$MKT" "$GH" <<'PY'
import json, os, shutil, sys
path, mkt, gh = sys.argv[1:4]
try:
    with open(path) as f: d = json.load(f)
except FileNotFoundError:
    d = {}
if not isinstance(d, dict): sys.exit(f"{path} is not a JSON object; fix it by hand")
e = d.setdefault("extraKnownMarketplaces", {}).setdefault(mkt, {})
e["source"] = {"source": "github", "repo": gh}
e["autoUpdate"] = True
os.makedirs(os.path.dirname(path), exist_ok=True)
if os.path.exists(path): shutil.copy2(path, path + ".bak")
tmp = path + ".tmp"
with open(tmp, "w") as f: json.dump(d, f, indent=2); f.write("\n")
os.replace(tmp, path)
print("autoUpdate on for", mkt)
PY

step "done"
echo "verify:  claude plugin list"
echo "updates arrive in the background (after the first message of a session, up to 10 min later);"
echo "they apply on /reload-plugins or the next launch."
if [ "${NO_START:-0}" != 1 ] && [ -t 1 ]; then exec claude; fi
