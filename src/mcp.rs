//! `mt mcp`: call the tools of the MCP servers Claude Code is configured with, through mt, in
//! batches, with long results compressed into mt's output store (`mt out <id>` queries them).
//!
//! Permissions stay Claude Code's. A call is a visible Bash command (`mt mcp call graft
//! graft_find_code '{...}'`), so Claude Code's own gate judges it (the auto-mode classifier, or a
//! prompt). mt adds nothing and loosens nothing: a tool that a `deny` or `ask` rule names
//! (`mcp__server__tool`, `mcp__server`, `mcp__server__*`), or a server in `deniedMcpServers`, is
//! refused with "call it natively", so the user's rule applies exactly as written.
//!
//! stdio servers only: HTTP/SSE servers authenticate with the session's OAuth tokens, which mt
//! does not hold; those stay native.
//! ponytail: one server process per `mt mcp call` (spawned, used, killed); keep them warm in the
//! daemon if start-up time (npx servers: 1-3 s) shows up in the measurements.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::tools::{config_dir, mcp_prefix, read_json};

pub struct Server {
    /// as Claude Code shows it: `graft`, or `plugin:<plugin>:<server>`
    pub name: String,
    /// the short name a call uses: `graft`, or `<server>` for a plugin's
    pub short: String,
    /// `mcp__graft__`, `mcp__plugin_<plugin>_<server>__`
    pub prefix: String,
    pub cfg: Value,
    pub root: String,
}

fn transport(cfg: &Value) -> &str {
    cfg.get("type").and_then(|t| t.as_str()).unwrap_or(if cfg.get("url").is_some() { "http" } else { "stdio" })
}

/// Every MCP server this project's Claude Code session would start: user and local scope
/// (`~/.claude.json`), approved project scope (`.mcp.json`), and enabled plugins.
pub fn servers(project: &Path) -> Vec<Server> {
    let cfg = config_dir();
    let mut out: Vec<Server> = vec![];
    let mut add = |servers: &Value, plugin: &str, root: &str, filter: &dyn Fn(&str) -> bool| {
        for (name, c) in servers.as_object().into_iter().flatten() {
            let (full, prefix) = if plugin.is_empty() { (name.clone(), mcp_prefix(name)) } else { (format!("plugin:{plugin}:{name}"), mcp_prefix(&format!("plugin_{plugin}_{name}"))) };
            // the first scope that names a server wins (local over user over project), once each
            if !filter(name) || out.iter().any(|s| s.name == full) { continue; }
            out.push(Server { name: full, short: name.clone(), prefix, cfg: c.clone(), root: root.to_string() });
        }
    };
    // servers a session got with `claude --mcp-config <file>` (Claude Code does not record those
    // anywhere mt can read): name the same files in MT_MCP_CONFIG, `:`-separated
    for f in std::env::var("MT_MCP_CONFIG").unwrap_or_default().split(':').filter(|f| !f.is_empty()) {
        let m = read_json(Path::new(f));
        add(m.get("mcpServers").unwrap_or(&m), "", "", &|_| true);
    }
    let cj = read_json(&cfg.join(".claude.json"));
    let proj = project.to_string_lossy();
    let local = cj.pointer(&format!("/projects/{}", proj.replace('~', "~0").replace('/', "~1"))).cloned().unwrap_or(Value::Null);
    add(local.get("mcpServers").unwrap_or(&Value::Null), "", "", &|_| true);
    add(cj.get("mcpServers").unwrap_or(&Value::Null), "", "", &|_| true);
    // project scope runs only once the user approved it
    let settings: Vec<Value> = settings_layers(project);
    let approved = |n: &str| {
        let lists = [local.get("enabledMcpjsonServers")].into_iter().chain(settings.iter().map(|s| s.get("enabledMcpjsonServers")));
        let all = settings.iter().chain([&local]).any(|s| s.get("enableAllProjectMcpServers").and_then(|b| b.as_bool()) == Some(true));
        let disabled = settings.iter().chain([&local]).any(|s| s.get("disabledMcpjsonServers").and_then(|a| a.as_array()).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(n))));
        !disabled && (all || lists.flatten().any(|a| a.as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(n)))))
    };
    let pm = read_json(&project.join(".mcp.json"));
    add(pm.get("mcpServers").unwrap_or(&Value::Null), "", "", &approved);
    let enabled: HashMap<String, bool> = settings.iter().filter_map(|s| s.get("enabledPlugins").and_then(|e| e.as_object()))
        .flat_map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_bool()?)))).collect();
    let inst = read_json(&cfg.join("plugins/installed_plugins.json"));
    for (key, v) in inst.get("plugins").and_then(|p| p.as_object()).into_iter().flatten() {
        if !enabled.get(key).copied().unwrap_or(false) { continue; }
        let Some(path) = v.pointer("/0/installPath").and_then(|p| p.as_str()) else { continue };
        let p = Path::new(path);
        let manifest = read_json(&p.join(".claude-plugin/plugin.json"));
        let name = manifest.get("name").and_then(|n| n.as_str()).map(String::from).unwrap_or_else(|| key.split('@').next().unwrap_or(key).to_string());
        for f in [".mcp.json", "mcp.json"] {
            let m = read_json(&p.join(f));
            if !m.is_null() { add(m.get("mcpServers").unwrap_or(&m), &name, path, &|_| true); }
        }
        if let Some(m) = manifest.get("mcpServers").filter(|m| m.is_object()) { add(m, &name, path, &|_| true); }
    }
    out
}

/// The settings files Claude Code merges for this project, lowest precedence first.
fn settings_layers(project: &Path) -> Vec<Value> {
    let mut v = vec![read_json(&config_dir().join("settings.json")), read_json(&project.join(".claude/settings.json")), read_json(&project.join(".claude/settings.local.json"))];
    for m in ["/Library/Application Support/ClaudeCode/managed-settings.json", "/etc/claude-code/managed-settings.json"] { v.push(read_json(Path::new(m))); }
    v
}

/// Why the user's own rules keep this tool from running through mt, if they do.
pub fn blocked(project: &Path, s: &Server, tool: &str) -> Option<String> {
    let full = format!("{}{tool}", s.prefix);
    let server_rule = s.prefix.trim_end_matches('_').to_string();
    for st in settings_layers(project) {
        for d in st.get("deniedMcpServers").and_then(|a| a.as_array()).into_iter().flatten() {
            if d.get("serverName").and_then(|n| n.as_str()) == Some(s.short.as_str()) { return Some(format!("server {} is in deniedMcpServers", s.short)); }
        }
        for kind in ["deny", "ask"] {
            for r in st.pointer(&format!("/permissions/{kind}")).and_then(|a| a.as_array()).into_iter().flatten().filter_map(|r| r.as_str()) {
                let hit = r == full || r == server_rule || (r.ends_with('*') && full.starts_with(r.trim_end_matches('*')));
                if hit { return Some(format!("a permissions.{kind} rule ({r}) covers {full}: call it natively, so Claude Code applies that rule")); }
            }
        }
    }
    None
}

fn find<'a>(all: &'a [Server], name: &str) -> Result<&'a Server, String> {
    all.iter().find(|s| s.name == name).or_else(|| all.iter().find(|s| s.short == name))
        .ok_or_else(|| format!("no MCP server `{name}` configured here; `mt mcp list` shows them"))
}

// ------------------------------------------------------------------ a stdio connection

struct Conn { child: Child, stdin: ChildStdin, rx: Receiver<Value>, next: i64, err: std::sync::Arc<std::sync::Mutex<String>> }

fn expand(s: &str, root: &str) -> String {
    let s = s.replace("${CLAUDE_PLUGIN_ROOT}", root);
    // ${VAR} and ${VAR:-default}, as Claude Code expands them in MCP configs
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap();
    re.replace_all(&s, |c: &regex::Captures| std::env::var(&c[1]).unwrap_or_else(|_| c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default())).into_owned()
}

impl Conn {
    fn open(s: &Server) -> Result<Conn, String> {
        if transport(&s.cfg) != "stdio" { return Err(format!("{} is an {} server: it needs the session's OAuth connection, so call it natively ({}<tool>)", s.short, transport(&s.cfg), s.prefix)); }
        let cmd = s.cfg.get("command").and_then(|c| c.as_str()).ok_or("no command in its config")?;
        let args: Vec<String> = s.cfg.get("args").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|a| a.as_str().map(|a| expand(a, &s.root))).collect();
        let mut c = Command::new(expand(cmd, &s.root));
        c.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in s.cfg.get("env").and_then(|e| e.as_object()).into_iter().flatten() { if let Some(v) = v.as_str() { c.env(k, expand(v, &s.root)); } }
        let mut child = c.spawn().map_err(|e| format!("start {cmd}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        // the last 2 KB of stderr: why a server that exits did, instead of a bare timeout
        let err = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(se) = child.stderr.take() {
            let err = err.clone();
            std::thread::spawn(move || for l in BufReader::new(se).lines().map_while(Result::ok) {
                let mut e = err.lock().unwrap_or_else(|p| p.into_inner());
                e.push_str(&l); e.push('\n');
                if e.len() > 2048 { let cut = e.len() - 2048; *e = e[e.ceil_char_boundary(cut)..].to_string(); }
            });
        }
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&l) { if tx.send(v).is_err() { break; } }
            }
        });
        let mut conn = Conn { child, stdin, rx, next: 1, err };
        conn.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mt", "version": env!("CARGO_PKG_VERSION")}}), 60)?;
        conn.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        Ok(conn)
    }

    fn send(&mut self, v: Value) { let _ = writeln!(self.stdin, "{v}"); let _ = self.stdin.flush(); }

    fn request(&mut self, method: &str, params: Value, secs: u64) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let v = match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(v) => v,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Err(format!("{method}: no answer within {secs} s")),
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    let code = self.child.try_wait().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "closed its output".into());
                    let e = self.err.lock().unwrap_or_else(|p| p.into_inner()).trim().lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                    return Err(format!("{method}: the server exited ({code}){}", if e.is_empty() { String::new() } else { format!(":\n{e}") }));
                }
            };
            if v.get("id").and_then(|i| i.as_i64()) != Some(id) { continue; }
            if let Some(e) = v.get("error") { return Err(format!("{method}: {}", e.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| e.to_string()))); }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn tools(&mut self) -> Result<Vec<Value>, String> {
        let mut all = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let r = self.request("tools/list", cursor.as_ref().map(|c| json!({"cursor": c})).unwrap_or(json!({})), 60)?;
            all.extend(r.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default());
            cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
            if cursor.is_none() { break; }
        }
        Ok(all)
    }
}

impl Drop for Conn {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

// ------------------------------------------------------------------ commands

fn first_sentence(d: &str) -> String {
    let d = d.trim().lines().next().unwrap_or("");
    let end = d.find(". ").map(|i| i + 1).unwrap_or(d.len()).min(160);
    d.chars().take(end).collect()
}

/// `name(req: type, opt?: type) — what it does`: a tool in one line.
fn signature(t: &Value) -> String {
    let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    let req: Vec<&str> = t.pointer("/inputSchema/required").and_then(|r| r.as_array()).into_iter().flatten().filter_map(|r| r.as_str()).collect();
    let args: Vec<String> = t.pointer("/inputSchema/properties").and_then(|p| p.as_object()).into_iter().flatten().map(|(k, v)| {
        let ty = v.get("type").map(|t| t.as_str().map(String::from).unwrap_or_else(|| t.to_string())).or_else(|| v.get("enum").map(|_| "enum".into())).unwrap_or_else(|| "any".into());
        format!("{k}{}: {ty}", if req.contains(&k.as_str()) { "" } else { "?" })
    }).collect();
    format!("{name}({}) — {}", args.join(", "), first_sentence(t.get("description").and_then(|d| d.as_str()).unwrap_or("")))
}

pub fn list(project: &Path) -> String {
    let all = servers(project);
    let mut o = format!("# mt mcp: {} server(s) configured for {}\n", all.len(), project.display());
    for s in &all {
        let t = transport(&s.cfg);
        let how = if t == "stdio" { "callable through mt".to_string() } else { format!("{t}: native only (OAuth)") };
        o.push_str(&format!("{:<32} {:<40} {how}\n", s.name, format!("{}<tool>", s.prefix)));
    }
    o.push_str("Tools of one server: `mt mcp tools <server> [tool ...]`. Calls: `mt mcp call` (see `mt mcp help`).\n");
    o
}

pub fn tools(project: &Path, server: &str, names: &[String]) -> Result<String, String> {
    let all = servers(project);
    let s = find(&all, server)?;
    let t0 = Instant::now();
    let ts = Conn::open(s)?.tools()?;
    let mut o = format!("# mt mcp tools {}: {} tool(s), {} ms\n", s.short, ts.len(), t0.elapsed().as_millis());
    for t in &ts {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if !names.is_empty() && !names.iter().any(|n| n == name) { continue; }
        let note = blocked(project, s, name).map(|_| "   [your deny/ask rule: native only]").unwrap_or("");
        if names.is_empty() { o.push_str(&format!("{}{note}\n", signature(t))); }
        else { o.push_str(&format!("{}{note}\n  description: {}\n  inputSchema: {}\n", signature(t), t.get("description").and_then(|d| d.as_str()).unwrap_or("").trim(), t.get("inputSchema").cloned().unwrap_or(Value::Null))); }
    }
    Ok(o)
}

/// A call result as JSON when it is JSON (structuredContent, or a text item that parses), and
/// its text either way.
fn result_value(r: &Value) -> (Option<Value>, String) {
    let text: String = r.get("content").and_then(|c| c.as_array()).into_iter().flatten().map(|c| match c.get("type").and_then(|t| t.as_str()) {
        Some("text") => c.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        Some(k) => format!("[{k} content, {} chars]", c.to_string().len()),
        None => c.to_string(),
    }).collect::<Vec<_>>().join("\n");
    let v = r.get("structuredContent").cloned().or_else(|| serde_json::from_str::<Value>(text.trim()).ok().filter(|v| v.is_object() || v.is_array()));
    (v, text)
}

/// `"$a"` is call a's whole result; `"$a.items[0].id"` the first match of that path in it. Only
/// ids of calls in this batch are references: any other `$...` string is passed as written.
fn resolve(v: &Value, done: &HashMap<String, Value>, ids: &[String]) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) if s.starts_with('$') && s.len() > 1 => {
            let body = &s[1..];
            let (id, path) = body.find(['.', '[']).map(|i| (&body[..i], &body[i..])).unwrap_or((body, ""));
            if !ids.iter().any(|i| i == id) { return Ok(v.clone()); }
            let Some(r) = done.get(id) else { return Err(format!("{s}: no earlier call `{id}` succeeded")) };
            if path.is_empty() { r.clone() } else {
                let hits = crate::compress::json_path(r, &format!("${path}"))?;
                hits.into_iter().next().map(|(_, v)| v).ok_or_else(|| format!("{s}: the path matched nothing in `{id}`"))?
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve(x, done, ids)).collect::<Result<_, _>>()?),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), resolve(x, done, ids)?))).collect::<Result<_, String>>()?),
        _ => v.clone(),
    })
}

/// Run a batch, in order: `[{"id": "a", "server": "posthog", "tool": "exec", "args": {...}}, ...]`.
/// One process per server for the whole batch. A failed call fails only the calls that use it.
pub fn call(project: &Path, batch: &Value) -> (bool, String) {
    let items: Vec<Value> = match batch { Value::Array(a) => a.clone(), Value::Object(_) => vec![batch.clone()], _ => return (false, "mt mcp call: give a JSON array of calls (or one call object)\n".into()) };
    let all = servers(project);
    let mut conns: HashMap<String, Conn> = HashMap::new();
    let mut done: HashMap<String, Value> = HashMap::new();
    let ids: Vec<String> = items.iter().enumerate().map(|(i, it)| it.get("id").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| format!("c{}", i + 1))).collect();
    let (mut ok_n, mut fail_n) = (0, 0);
    let mut body = String::new();
    let t0 = Instant::now();
    for (i, it) in items.iter().enumerate() {
        let id = ids[i].clone();
        let server = it.get("server").and_then(|x| x.as_str()).unwrap_or("");
        let tool = it.get("tool").and_then(|x| x.as_str()).unwrap_or("");
        let t = Instant::now();
        let r: Result<(Option<Value>, String), String> = (|| {
            if server.is_empty() || tool.is_empty() { return Err("each call needs \"server\" and \"tool\"".into()); }
            let s = find(&all, server)?;
            if let Some(why) = blocked(project, s, tool) { return Err(why); }
            let args = resolve(it.get("args").unwrap_or(&json!({})), &done, &ids)?;
            if !conns.contains_key(&s.name) { conns.insert(s.name.clone(), Conn::open(s)?); }
            let secs = it.get("timeout").and_then(|x| x.as_u64()).unwrap_or(120);
            let r = conns.get_mut(&s.name).unwrap().request("tools/call", json!({"name": tool, "arguments": args}), secs)?;
            let (v, text) = result_value(&r);
            if r.get("isError").and_then(|b| b.as_bool()) == Some(true) { return Err(format!("the tool reported an error: {}", text.trim())); }
            Ok((v, text))
        })();
        let ms = t.elapsed().as_millis();
        crate::events::emit(project, json!({"kind": "mcp", "server": server, "tool": tool, "ok": r.is_ok(), "ms": ms as u64,
            "chars": r.as_ref().map(|(v, t)| v.as_ref().map(|v| v.to_string().len()).unwrap_or(t.len())).unwrap_or(0), "session": crate::events::session8()}));
        match r {
            Ok((v, text)) => {
                ok_n += 1;
                let name = it.get("name").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| format!("mcp-{server}-{tool}"));
                let budget = it.get("lines").and_then(|x| x.as_u64()).map(|n| n as usize).unwrap_or(crate::compress::DEFAULT_LINES);
                let input = v.as_ref().map(|v| v.to_string()).unwrap_or_else(|| text.clone());
                let kind = if v.is_some() { crate::compress::Kind::Json } else { crate::compress::Kind::Auto };
                let (shown, kept) = crate::compress::compress_and_store(&input, kind, budget, &format!("mcp {server}.{tool}"), &name);
                body.push_str(&format!("## {id} = {server}.{tool}  ✓ {ms} ms, {} chars{}\n{}\n", input.len(), kept.map(|k| format!(", kept as {k}")).unwrap_or_default(), shown.trim_end()));
                done.insert(id, v.unwrap_or(Value::String(text)));
            }
            Err(e) => { fail_n += 1; body.push_str(&format!("## {id} = {server}.{tool}  ✗ {ms} ms\n{e}\n")); }
        }
    }
    let head = format!("# mt mcp call: {} call(s), {ok_n} ✓ {fail_n} ✗, {} ms, {} server process(es)\n", items.len(), t0.elapsed().as_millis(), conns.len());
    (fail_n == 0, head + &body)
}

pub const HELP: &str = "mt mcp: call MCP tools through mt, in one batch, with long results kept and queryable.

  mt mcp list                          the servers configured here, and which mt can call (stdio)
  mt mcp tools <server> [tool ...]     each tool in one line; name tools for their full schema
  mt mcp call <server> <tool> ['{args}']
  mt mcp call - <<'EOF'                a batch, in order, one server process per server
  [{\"id\": \"a\", \"server\": \"posthog\", \"tool\": \"exec\", \"args\": {\"command\": \"...\"}},
   {\"id\": \"b\", \"server\": \"posthog\", \"tool\": \"exec\", \"args\": {\"command\": \"$a.results[0].id\"}}]
  EOF

\"$a\" is call a's whole result, \"$a.path\" the first JSONPath match in it. Per call: \"name\" (store
name), \"lines\" (output budget), \"timeout\" (seconds, default 120). A long result is compressed
and kept: `mt out <id> --json '$.path'`, `--jq '<filter>'` or `mt out <id> \"<question>\"`.

Permissions are Claude Code's: the command is judged like any Bash command, and a tool your
deny/ask rules name (or a server in deniedMcpServers) is refused, to be called natively. HTTP/SSE
servers use the session's OAuth connection: call those natively.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_resolve_and_rules_refuse() {
        let done: HashMap<String, Value> = [("a".to_string(), json!({"items": [{"id": 7}], "hash": "h1"}))].into();
        let ids = vec!["a".to_string(), "b".to_string()];
        let r = resolve(&json!({"x": "$a.items[0].id", "h": "$a.hash", "lit": "$5 off", "env": "$HOME", "all": ["$a"]}), &done, &ids).unwrap();
        assert_eq!(r, json!({"x": 7, "h": "h1", "lit": "$5 off", "env": "$HOME", "all": [{"items": [{"id": 7}], "hash": "h1"}]}));
        assert!(resolve(&json!("$b.id"), &done, &ids).unwrap_err().contains("no earlier call `b`"));
        assert!(resolve(&json!("$a.nothing"), &done, &ids).unwrap_err().contains("matched nothing"));

        let d = std::env::temp_dir().join(format!("mt-mcp-perm-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/settings.json"), r#"{"permissions": {"deny": ["mcp__s__drop"], "ask": ["mcp__plugin_p_q__*"]}, "deniedMcpServers": [{"serverName": "evil"}]}"#).unwrap();
        let srv = |short: &str, plugin: &str| Server { name: short.into(), short: short.into(), prefix: if plugin.is_empty() { mcp_prefix(short) } else { mcp_prefix(&format!("plugin_{plugin}_{short}")) }, cfg: json!({}), root: String::new() };
        assert!(blocked(&d, &srv("s", ""), "drop").unwrap().contains("permissions.deny"));
        assert!(blocked(&d, &srv("s", ""), "read").is_none());
        assert!(blocked(&d, &srv("q", "p"), "anything").unwrap().contains("permissions.ask"));
        assert!(blocked(&d, &srv("evil", ""), "x").unwrap().contains("deniedMcpServers"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
