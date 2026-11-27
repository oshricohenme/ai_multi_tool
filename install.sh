#!/bin/sh
# Install the Token Accounting Statement for every coding agent on this machine, then open it.
#
#   curl -fsSL https://raw.githubusercontent.com/oshricohenme/ai_multi_tool/main/install.sh | sh
#   sh install.sh [flags]                 # from a checkout
#
# What it does, for the agents it finds:
#   - token-statement, a command on your PATH (~/.local/bin) that reads the logs of Claude Code, Codex,
#     opencode and Gemini CLI. Any agent that can run a shell command can use it.
#   - Claude Code: the plugin from the ai-multi-tool marketplace, with auto-update on (/token-statement,
#     a live pane and a status line).
#   - Codex and other agents that read ~/.agents/skills: a token-statement skill.
#   - opencode and Gemini CLI: a /token-statement command.
#   Then it opens the last 7 days in your browser.
#
#   (no flag)             Claude Code plugin for you, in every repo (user scope). Also removes per-repo
#                         installs of the plugin, which would otherwise load twice.
#   --repo                Claude Code: this repo only, .claude/settings.local.json (Claude scope local)
#   --project             Claude Code: this repo, shared with the team via .claude/settings.json
#   --keep-repo-installs  Claude Code, user scope: do not remove per-repo installs of the plugin
#   --no-claude           skip the Claude Code plugin
#   --no-start            do not open the statement at the end (same as NO_START=1)
#   -h, --help            this text
#   (SCOPE=user|project|local still works as a fallback. PREFIX and BIN_DIR move the command:
#    defaults ~/.local/share/token-accounting-statement and ~/.local/bin.)
set -eu
GH=oshricohenme/ai_multi_tool
MKT=ai-multi-tool
P=token-accounting-statement
SCOPE=${SCOPE:-user}
KEEP=0
CLAUDE=1
PREFIX=${PREFIX:-$HOME/.local/share/$P}
BIN_DIR=${BIN_DIR:-$HOME/.local/bin}
step() { printf '\n== %s\n' "$*"; }
usage() { cat <<'U'
usage: install.sh [--repo | --project] [--keep-repo-installs] [--no-claude] [--no-start] [-h]
  (none)               install for every agent found; the Claude Code plugin for you, all repos
  --repo               Claude Code: this repo only (.claude/settings.local.json)
  --project            Claude Code: this repo, shared with the team (.claude/settings.json)
  --keep-repo-installs Claude Code, user scope: keep per-repo installs of the plugin
  --no-claude          skip the Claude Code plugin
  --no-start           do not open the statement at the end
  -h, --help           this text
U
}
for a in "$@"; do
  case "$a" in
    --repo) SCOPE=local ;;
    --project) SCOPE=project ;;
    --keep-repo-installs) KEEP=1 ;;
    --no-claude) CLAUDE=0 ;;
    --no-start) NO_START=1 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown flag: $a" >&2; usage >&2; exit 2 ;;
  esac
done

# Uninstall every per-repo (local/project) entry of the plugin listed in the plugin-list JSON file.
# DRY_RUN=1 prints the commands instead of running them. Other plugins are never touched.
cleanup_repo_installs() {
  TAB=$(printf '\t')
  python3 - "$1" "$P@$MKT" <<'PY' | while IFS=$TAB read -r scope dir; do
import json, sys
try: d = json.load(open(sys.argv[1]))
except Exception: d = []
for e in d if isinstance(d, list) else []:
    if e.get("id") == sys.argv[2] and e.get("scope") in ("local", "project") and e.get("projectPath"):
        print(e["scope"], e["projectPath"], sep="\t")
PY
    if [ ! -d "$dir" ]; then echo "skip (missing dir): $dir [$scope]"; continue; fi
    echo "remove per-repo $P: $dir [$scope]"
    if [ "${DRY_RUN:-0}" = 1 ]; then echo "  (cd \"$dir\" && claude plugin uninstall $P@$MKT -s $scope)"; continue; fi
    (cd "$dir" && claude plugin uninstall "$P@$MKT" -s "$scope") || echo "  failed, continuing" >&2
  done
}
if [ -n "${CLEANUP_TEST_JSON:-}" ]; then cleanup_repo_installs "$CLEANUP_TEST_JSON"; exit 0; fi

command -v node >/dev/null || { echo "install.sh needs Node.js 20 or later: https://nodejs.org" >&2; exit 1; }
node -e 'process.exit(+process.versions.node.split(".")[0] >= 20 ? 0 : 1)' || { echo "install.sh needs Node.js 20 or later, found $(node --version)" >&2; exit 1; }
case "$SCOPE" in user | project | local) ;; *) echo "SCOPE must be user, project or local" >&2; exit 1 ;; esac

step "1. token-statement command: $BIN_DIR/token-statement"
# From a checkout, install what is in it; through curl, download the repo.
SRC=$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$P
if [ ! -f "$SRC/bin/statement.mjs" ]; then
  for c in curl tar; do command -v $c >/dev/null || { echo "install.sh needs $c" >&2; exit 1; }; done
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  curl -fsSL "https://codeload.github.com/$GH/tar.gz/refs/heads/${REF:-main}" | tar -xz -C "$TMP"
  SRC=$(ls -d "$TMP"/*/$P)
fi
mkdir -p "$(dirname "$PREFIX")" "$BIN_DIR"
rm -rf "$PREFIX.new"
cp -R "$SRC" "$PREFIX.new"
rm -rf "$PREFIX"
mv "$PREFIX.new" "$PREFIX"
printf '#!/bin/sh\nexec node "%s/bin/statement.mjs" "$@"\n' "$PREFIX" >"$BIN_DIR/token-statement"
chmod 755 "$BIN_DIR/token-statement"
TS=$BIN_DIR/token-statement
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "note: $BIN_DIR is not on your PATH. Add it, or run $TS" ;; esac

if [ "$CLAUDE" = 1 ] && command -v claude >/dev/null; then
  command -v python3 >/dev/null || { echo "the Claude Code plugin step needs python3 (or pass --no-claude)" >&2; exit 1; }
  case "$SCOPE" in
    user) SETTINGS=${HOME}/.claude/settings.json ;;
    *)
      REPO=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
      if [ "$SCOPE" = project ]; then SETTINGS=$REPO/.claude/settings.json; else SETTINGS=$REPO/.claude/settings.local.json; fi ;;
  esac

  step "2. Claude Code: the marketplace"
  if claude plugin marketplace list --json 2>/dev/null | grep -q "\"name\": *\"$MKT\""; then
    claude plugin marketplace update $MKT
  else
    claude plugin marketplace add $GH
  fi

  step "2a. Claude Code: $P (scope: $SCOPE)"
  # an update when installed, else an install
  claude plugin update "$P@$MKT" -s "$SCOPE" 2>/dev/null || claude plugin install "$P@$MKT" -s "$SCOPE"

  if [ "$SCOPE" = user ] && [ "$KEEP" != 1 ]; then
    step "2b. Claude Code: remove per-repo $P installs (they would load twice)"
    LIST=$(mktemp)
    claude plugin list --json >"$LIST" 2>/dev/null || echo "[]" >"$LIST"
    cleanup_repo_installs "$LIST"
    rm -f "$LIST"
  fi

  step "2c. Claude Code: auto-update in $SETTINGS"
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
else
  step "2. Claude Code: skipped ($([ "$CLAUDE" = 1 ] && echo 'claude not found' || echo '--no-claude'))"
fi

step "3. other agents"
# One skill for every agent that reads ~/.agents/skills (Codex, and others that follow the Agent Skills layout).
mkdir -p "$HOME/.agents/skills/token-statement"
cat >"$HOME/.agents/skills/token-statement/SKILL.md" <<EOF
---
name: token-statement
description: Token Accounting Statement. What this machine's coding agent sessions (Claude Code, Codex, opencode, Gemini CLI) spent, itemized per request like a bank statement, with the context balance, why it changed and the API-equivalent cost. Use when the user asks about token use, cost, spend, context size or cache hits.
---

Run the statement with the shell, and show its output to the user as it is. It only reads local logs.

- This session (the last one written): \`$TS open --session latest\` opens it live in the browser and prints a summary.
- As text, for the terminal: \`$TS --session latest --text\`
- The last 7 days: \`$TS open --since 7d\`. This month: \`$TS open --month current\`.
- One request per line, for a spreadsheet: \`$TS --since 30d --csv\`
- Everything else: \`$TS --help\`
EOF
echo "skill: ~/.agents/skills/token-statement"
if command -v opencode >/dev/null || [ -d "$HOME/.config/opencode" ]; then
  mkdir -p "$HOME/.config/opencode/command"
  cat >"$HOME/.config/opencode/command/token-statement.md" <<EOF
---
description: Token accounting statement for this session, opened in the browser
---
The token accounting statement is open in the browser. Show the user this summary as it is, without analysis:

!\`$TS open --session latest\`
EOF
  echo "opencode: /token-statement"
fi
if command -v gemini >/dev/null || [ -d "$HOME/.gemini" ]; then
  mkdir -p "$HOME/.gemini/commands"
  cat >"$HOME/.gemini/commands/token-statement.toml" <<EOF
description = "Token accounting statement for this session"
prompt = """Show the user this token accounting statement summary as it is, without analysis:

!{$TS open --session latest}"""
EOF
  echo "Gemini CLI: /token-statement"
fi

step "done"
echo "run:     token-statement --help"
[ "$CLAUDE" = 1 ] && command -v claude >/dev/null && echo "Claude:  /token-statement, after /reload-plugins or in a new session"
if [ "${NO_START:-0}" != 1 ] && [ -t 1 ]; then
  step "the last 7 days"
  "$TS" open --since 7d
fi
