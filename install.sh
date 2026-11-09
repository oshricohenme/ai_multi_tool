#!/bin/sh
# Install Claude coach (inspect every model call and tool call) and keep it updated in the background.
#
#   sh ~/orca/ai_multi_tool/install.sh [flags]
#   curl -fsSL https://raw.githubusercontent.com/oshricohenme/ai_multi_tool/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/oshricohenme/ai_multi_tool/main/install.sh | sh -s -- --repo
#
# It adds the ai-multi-tool marketplace, installs (or updates) the coach plugin, and turns on
# marketplace auto-update so new pushes arrive by themselves.
#
#   (no flag)             install once for you, in every repo (user scope). Also removes per-repo
#                         installs of coach, which would otherwise load twice.
#   --repo                this repo only, .claude/settings.local.json (Claude scope local)
#   --project             this repo, shared with the team via .claude/settings.json (scope project)
#   --keep-repo-installs  user scope: do not remove per-repo installs of coach
#   --no-start            do not start claude at the end (same as NO_START=1)
#   -h, --help            this text
#   (SCOPE=user|project|local still works as a fallback)
#
# The mt plugin (a Rust binary from GitHub Releases) is hidden from the marketplace for now. Its
# install steps (CI binary wait, release tag, binary download, `mt build`, PATH check, MT_DEBUG)
# were removed from this script; they stay in git history.
set -eu
GH=oshricohenme/ai_multi_tool
MKT=ai-multi-tool
SCOPE=${SCOPE:-user}
KEEP=0
step() { printf '\n== %s\n' "$*"; }
usage() { cat <<'U'
usage: install.sh [--repo | --project] [--keep-repo-installs] [--no-start] [-h]
  (none)               install coach once for you, all repos; removes per-repo installs
  --repo               this repo only (.claude/settings.local.json)
  --project            this repo, shared with the team (.claude/settings.json)
  --keep-repo-installs user scope: keep per-repo installs of coach
  --no-start           do not start claude at the end
  -h, --help           this text
U
}
for a in "$@"; do
  case "$a" in
    --repo) SCOPE=local ;;
    --project) SCOPE=project ;;
    --keep-repo-installs) KEEP=1 ;;
    --no-start) NO_START=1 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown flag: $a" >&2; usage >&2; exit 2 ;;
  esac
done

# Uninstall every per-repo (local/project) coach entry listed in the plugin-list JSON file.
# DRY_RUN=1 prints the commands instead of running them. Other plugins are never touched.
cleanup_repo_installs() {
  TAB=$(printf '\t')
  python3 - "$1" "coach@$MKT" <<'PY' | while IFS=$TAB read -r scope dir; do
import json, sys
try: d = json.load(open(sys.argv[1]))
except Exception: d = []
for e in d if isinstance(d, list) else []:
    if e.get("id") == sys.argv[2] and e.get("scope") in ("local", "project") and e.get("projectPath"):
        print(e["scope"], e["projectPath"], sep="\t")
PY
    if [ ! -d "$dir" ]; then echo "skip (missing dir): $dir [$scope]"; continue; fi
    echo "remove per-repo coach: $dir [$scope]"
    if [ "${DRY_RUN:-0}" = 1 ]; then echo "  (cd \"$dir\" && claude plugin uninstall coach@$MKT -s $scope)"; continue; fi
    (cd "$dir" && claude plugin uninstall "coach@$MKT" -s "$scope") || echo "  failed, continuing" >&2
  done
}
if [ -n "${CLEANUP_TEST_JSON:-}" ]; then cleanup_repo_installs "$CLEANUP_TEST_JSON"; exit 0; fi

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

if [ "$SCOPE" = user ] && [ "$KEEP" != 1 ]; then
  step "2b. remove per-repo coach installs (they would load twice)"
  LIST=$(mktemp)
  claude plugin list --json >"$LIST" 2>/dev/null || echo "[]" >"$LIST"
  cleanup_repo_installs "$LIST"
  rm -f "$LIST"
fi

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
