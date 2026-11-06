#!/bin/bash
# Prepare this folder for A/B runs: a frozen copy of the plugin (so a rebuild mid-run cannot change
# it), the shop database, and the MCP configs with this machine's paths. Run from anywhere.
set -e
S="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; ROOT="$(cd "$S/../.." && pwd)"
rm -rf "$S/plugin-frozen"; mkdir -p "$S/plugin-frozen"
cp -R "$ROOT/plugin/hooks" "$ROOT/plugin/skills" "$S/plugin-frozen/"; mkdir -p "$S/plugin-frozen/bin"
# a new file (rm + cp), never a copy over a binary that ran: macOS kills one whose signature changed
BIN="${MT_BIN:-$ROOT/target/release/mt}"; rm -f "$S/plugin-frozen/bin/mt"; cp "$BIN" "$S/plugin-frozen/bin/mt"; "$S/plugin-frozen/bin/mt" --version
cp "$ROOT/skills/mt/SKILL.md" "$S/mt-skill-now.md"
python3 "$S/gen_shop_db.py" "$S/shop.db"
python3 - "$S" <<'PY'
import json, shutil, sys
s = sys.argv[1]
json.dump({"mcpServers": {"graft": {"command": shutil.which("graft") or "graft", "args": ["mcp"]}}}, open(f"{s}/mcp-ab.json", "w"))
json.dump({"mcpServers": {"shop": {"command": "/usr/bin/env", "args": ["python3", f"{s}/fixture/sqlite_mcp.py", f"{s}/shop.db"]}}}, open(f"{s}/mcp-shop.json", "w"))
PY
echo "ready: $S"
