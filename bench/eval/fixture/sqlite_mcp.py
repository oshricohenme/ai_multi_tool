#!/usr/bin/env python3
"""A read-only SQLite MCP server over stdio, stdlib only (a test fixture for the mt mcp A/B).
Tools as in the reference mcp-server-sqlite: list_tables, describe_table, read_query."""
import json, sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
TOOLS = [
    {"name": "list_tables", "description": "List the tables in the SQLite database.", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "describe_table", "description": "Get the schema (columns and types) of one table.", "inputSchema": {"type": "object", "properties": {"table_name": {"type": "string"}}, "required": ["table_name"]}},
    {"name": "read_query", "description": "Run a SELECT query on the SQLite database and return the rows as JSON.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string", "description": "SELECT SQL query"}}, "required": ["query"]}},
]
def rows(q, args=()):
    cur = db.execute(q, args); cols = [d[0] for d in cur.description]
    return [dict(zip(cols, r)) for r in cur.fetchall()]
def call(name, a):
    if name == "list_tables": return rows("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
    if name == "describe_table": return rows(f"PRAGMA table_info({json.dumps(a['table_name'])})")
    if name == "read_query":
        q = a["query"].strip()
        if not q.lower().startswith(("select", "with")): raise ValueError("only SELECT queries are allowed")
        return rows(q)
    raise ValueError(f"unknown tool {name}")
for line in sys.stdin:
    try: m = json.loads(line)
    except Exception: continue
    if "id" not in m: continue
    meth, p = m.get("method"), m.get("params") or {}
    try:
        if meth == "initialize": r = {"protocolVersion": p.get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "shop-sqlite", "version": "0.1"}}
        elif meth == "tools/list": r = {"tools": TOOLS}
        elif meth == "tools/call":
            try: r = {"content": [{"type": "text", "text": json.dumps(call(p["name"], p.get("arguments") or {}))}]}
            except Exception as e: r = {"content": [{"type": "text", "text": f"Error: {e}"}], "isError": True}
        elif meth == "ping": r = {}
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": f"no method {meth}"}}), flush=True); continue
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
    except Exception as e:
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32000, "message": str(e)}}), flush=True)
