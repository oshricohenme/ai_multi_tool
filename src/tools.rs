//! `mt tools`: a graph of every skill, command, agent, plugin and MCP server Claude Code can load,
//! linked to the code of the project at hand. Nodes come from their definitions: plugin manifests,
//! SKILL.md, commands and agent files on disk, and each MCP server's own `tools/list` (mt starts
//! stdio servers itself; HTTP servers need the session's auth and stay tool-less). Project facts
//! come from the repo: dependency manifests, file names and extensions, and identifiers in mt's code
//! index. Each item's anchors (the product or technology it is about, e.g. `posthog`, `prisma`,
//! `chrome`) either match a project fact, with evidence, or don't.
//!
//! `profile` writes a settings layer that turns off what the code gives no reason to load, `hook`
//! puts the few items relevant to a prompt back into context, `show` serves a skill or agent body
//! without loading it, `enable` brings one back. Store: ~/.cache/mt/tools.sqlite (MT_TOOLS_DB).

use crate::index;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

pub fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}
fn home() -> PathBuf { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default() }
fn db_path() -> PathBuf {
    std::env::var_os("MT_TOOLS_DB").map(PathBuf::from).unwrap_or_else(|| home().join(".cache/mt/tools.sqlite"))
}

pub fn open() -> rusqlite::Result<Connection> {
    let p = db_path();
    if let Some(d) = p.parent() { let _ = fs::create_dir_all(d); }
    let c = Connection::open(p)?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS nodes(id TEXT PRIMARY KEY, kind TEXT, name TEXT, plugin TEXT, descr TEXT, path TEXT, chars INTEGER, extra TEXT);
         CREATE TABLE IF NOT EXISTS edges(src TEXT, rel TEXT, dst TEXT, w INTEGER, PRIMARY KEY(src,rel,dst));
         CREATE TABLE IF NOT EXISTS mcp_probe(server TEXT PRIMARY KEY, cfg TEXT, status TEXT, instructions TEXT, tools TEXT, ms INTEGER);
         CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER);
         CREATE TABLE IF NOT EXISTS seen_mcp(tool TEXT PRIMARY KEY, n INTEGER);",
    )?;
    Ok(c)
}


/// The closest ancestor holding .git, so a subdirectory and its repo are one project.
pub fn repo_root(cwd: &str) -> String {
    let mut p = PathBuf::from(cwd);
    loop {
        if p.join(".git").exists() { return p.to_string_lossy().into_owned(); }
        if !p.pop() { return cwd.to_string(); }
    }
}

/// Every hook decision, with the prompt head and the cards it added, for the eval and for debugging.
pub fn hook_log(project: &Path, prompt: &str, cards: &[String], ms: u128, stages: Value) {
    let line = json!({"t": SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), "ms": ms, "stages_ms": stages, "project": project, "prompt": prompt.chars().take(200).collect::<String>(), "cards": cards}).to_string();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open("/tmp/mt-tools-hook.log") { let _ = writeln!(f, "{line}"); }
}

pub struct IndexStats { pub nodes: usize, pub edges: usize, pub probed: Vec<String>, pub ms: u128 }

pub fn index(c: &mut Connection, reprobe: bool) -> Result<IndexStats, String> {
    let t = Instant::now();
    if reprobe { let _ = c.execute("DELETE FROM mcp_probe", []); }
    let (nodes, edges, probed) = build_catalog(c)?;
    Ok(IndexStats { nodes, edges, probed, ms: t.elapsed().as_millis() })
}

// ---------------------------------------------------------------- catalog from disk

struct Fm { name: Option<String>, desc: String, body: String }

/// Minimal frontmatter reader: `name:` and `description:` including folded (`>`, `|`) and
/// indented continuation lines.
fn frontmatter(src: &str) -> Fm {
    let mut fm = Fm { name: None, desc: String::new(), body: src.to_string() };
    let Some(rest) = src.strip_prefix("---") else { return fm };
    let Some(end) = rest.find("\n---") else { return fm };
    let head = &rest[..end];
    fm.body = rest[end + 4..].trim_start_matches(|c| c == '-' || c == '\n').to_string();
    let lines: Vec<&str> = head.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let Some(v) = l.strip_prefix("name:") { fm.name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string()); }
        if let Some(v) = l.strip_prefix("description:") {
            let mut v = v.trim().to_string();
            if v.is_empty() || v.starts_with('>') || v.starts_with('|') { v.clear(); }
            while i + 1 < lines.len() && (lines[i + 1].starts_with(' ') || lines[i + 1].starts_with('\t')) {
                i += 1;
                if !v.is_empty() { v.push(' '); }
                v.push_str(lines[i].trim());
            }
            fm.desc = v.trim_matches('"').trim_matches('\'').to_string();
        }
        i += 1;
    }
    fm
}

struct Catalog { nodes: BTreeMap<String, (String, String, String, String, String, i64, String)>, edges: BTreeSet<(String, String, String, i64)> }

impl Catalog {
    fn node(&mut self, id: &str, kind: &str, name: &str, plugin: &str, desc: &str, path: &str, chars: i64, extra: &str) {
        let e = self.nodes.entry(id.to_string()).or_insert_with(|| (kind.into(), name.into(), plugin.into(), String::new(), path.into(), chars, extra.into()));
        if desc.len() > e.3.len() { e.3 = desc.to_string(); }
        if e.4.is_empty() { e.4 = path.to_string(); }
        if !plugin.is_empty() { e.2 = plugin.to_string(); }
        if chars > e.5 { e.5 = chars; }
        if !extra.is_empty() { e.6 = extra.to_string(); }
    }
    fn edge(&mut self, s: &str, r: &str, d: &str, w: i64) { self.edges.insert((s.into(), r.into(), d.into(), w)); }
}

pub(crate) fn read_json(p: &Path) -> Value { fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null) }

fn skills_in(cat: &mut Catalog, dir: &Path, prefix: &str, plugin: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let f = e.path().join("SKILL.md");
        let Ok(src) = fs::read_to_string(&f) else { continue };
        let fm = frontmatter(&src);
        let base = e.file_name().to_string_lossy().into_owned();
        let name = format!("{prefix}{}", fm.name.clone().unwrap_or(base));
        let id = format!("skill:{name}");
        cat.node(&id, "skill", &name, plugin, &fm.desc, &f.to_string_lossy(), src.len() as i64, "");
        if !plugin.is_empty() { cat.edge(&format!("plugin:{plugin}"), "provides", &id, 1); }
    }
}

/// Plugin and user commands (`commands/*.md`) are listed and invoked like skills.
fn commands_in(cat: &mut Catalog, dir: &Path, prefix: &str, plugin: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "md") { continue; }
        let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
        if stem.starts_with('_') { continue; }
        let Ok(src) = fs::read_to_string(&p) else { continue };
        let fm = frontmatter(&src);
        let name = format!("{prefix}{stem}");
        let id = format!("skill:{name}");
        let desc = if fm.desc.is_empty() { fm.body.lines().find(|l| !l.trim().is_empty() && !l.starts_with('#')).unwrap_or("").to_string() } else { fm.desc.clone() };
        cat.node(&id, "skill", &name, plugin, &desc, &p.to_string_lossy(), src.len() as i64, "command");
        if !plugin.is_empty() { cat.edge(&format!("plugin:{plugin}"), "provides", &id, 1); }
    }
}

fn agents_in(cat: &mut Catalog, dir: &Path, prefix: &str, plugin: &str) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "md") { continue; }
        let Ok(src) = fs::read_to_string(&p) else { continue };
        let fm = frontmatter(&src);
        let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
        let name = format!("{prefix}{}", fm.name.clone().unwrap_or(stem));
        let id = format!("agent:{name}");
        let desc: String = fm.desc.split("<example>").next().unwrap_or("").replace("\\n", " ").chars().take(600).collect();
        cat.node(&id, "agent", &name, plugin, &desc, &p.to_string_lossy(), fm.desc.len() as i64, "");
        if !plugin.is_empty() { cat.edge(&format!("plugin:{plugin}"), "provides", &id, 1); }
    }
}

fn mcp_servers(cat: &mut Catalog, servers: &Value, plugin: &str, scope: &str) {
    let Some(m) = servers.as_object() else { return };
    for (name, cfg) in m {
        let full = if plugin.is_empty() { name.clone() } else { format!("plugin:{plugin}:{name}") };
        let transport = cfg.get("type").and_then(|t| t.as_str()).unwrap_or("stdio");
        cat.node(&format!("mcp:{full}"), "mcp", &full, plugin, "", scope, 0, transport);
        if !plugin.is_empty() { cat.edge(&format!("plugin:{plugin}"), "provides", &format!("mcp:{full}"), 1); }
    }
}

/// MCP tool name prefix for a server: mcp__<server>__ with Claude Code's normalisation.
pub(crate) fn mcp_prefix(server: &str) -> String {
    let s: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("mcp__{s}__")
}

/// Ask one MCP server for its tools the way Claude Code does at startup: initialize, then
/// tools/list (paged). stdio only; HTTP servers need the session's OAuth tokens.
fn probe_stdio(cfg: &Value, plugin_root: &str) -> Result<(String, Vec<(String, String, i64)>), String> {
    let sub = |s: &str| s.replace("${CLAUDE_PLUGIN_ROOT}", plugin_root);
    let cmd = cfg.get("command").and_then(|c| c.as_str()).ok_or("no command")?;
    let args: Vec<String> = cfg.get("args").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|a| a.as_str().map(sub)).collect();
    let mut command = Command::new(sub(cmd));
    command.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    for (k, v) in cfg.get("env").and_then(|e| e.as_object()).into_iter().flatten() { if let Some(v) = v.as_str() { command.env(k, sub(v)); } }
    let mut child = command.spawn().map_err(|e| format!("spawn {cmd}: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l) { if tx.send(v).is_err() { break; } }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let wait = |id: i64| -> Result<Value, String> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = rx.recv_timeout(left).map_err(|_| "no answer within 20 s".to_string())?;
            if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                return v.get("result").cloned().ok_or_else(|| format!("error: {}", v.get("error").map(|e| e.to_string()).unwrap_or_default()));
            }
        }
    };
    let mut send = |v: Value| { let _ = writeln!(stdin, "{v}"); let _ = stdin.flush(); };
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mt", "version": "0.1"}}}));
    let init = wait(1);
    let out = (|| {
        let init = init?;
        send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        let instructions = init.get("instructions").and_then(|i| i.as_str()).unwrap_or("").to_string();
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        for page in 0..20 {
            let mut params = json!({});
            if let Some(c) = &cursor { params["cursor"] = json!(c); }
            send(json!({"jsonrpc": "2.0", "id": 2 + page, "method": "tools/list", "params": params}));
            let r = wait(2 + page)?;
            for t in r.get("tools").and_then(|t| t.as_array()).into_iter().flatten() {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                tools.push((name, t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(), t.to_string().len() as i64));
            }
            cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
            if cursor.is_none() { break; }
        }
        Ok((instructions, tools))
    })();
    let _ = child.kill();
    let _ = child.wait();
    out
}

fn build_catalog(c: &mut Connection) -> Result<(usize, usize, Vec<String>), String> {
    let cfg = config_dir();
    let mut cat = Catalog { nodes: BTreeMap::new(), edges: BTreeSet::new() };
    let settings = read_json(&cfg.join("settings.json"));
    let enabled = settings.get("enabledPlugins").cloned().unwrap_or(Value::Null);
    // (server node id, config, plugin root) for every configured server, probed below
    let mut server_cfgs: Vec<(String, Value, String)> = vec![];
    let add_servers = |cat: &mut Catalog, servers: &Value, plugin: &str, scope: &str, root: &str, out: &mut Vec<(String, Value, String)>| {
        mcp_servers(cat, servers, plugin, scope);
        for (name, cfg) in servers.as_object().into_iter().flatten() {
            let full = if plugin.is_empty() { name.clone() } else { format!("plugin:{plugin}:{name}") };
            out.push((format!("mcp:{full}"), cfg.clone(), root.to_string()));
        }
    };
    let inst = read_json(&cfg.join("plugins/installed_plugins.json"));
    for (key, v) in inst.get("plugins").and_then(|p| p.as_object()).into_iter().flatten() {
        // mt's own trimmed copies (marketplace mt-*) are not catalog entries: they would shadow
        // the plugin they were made from
        if key.split('@').nth(1).is_some_and(|m| m.starts_with("mt-")) { continue; }
        let Some(path) = v.pointer("/0/installPath").and_then(|p| p.as_str()) else { continue };
        let p = Path::new(path);
        let manifest = read_json(&p.join(".claude-plugin/plugin.json"));
        let name = manifest.get("name").and_then(|n| n.as_str()).map(String::from).unwrap_or_else(|| key.split('@').next().unwrap_or(key).to_string());
        let on = enabled.get(key).and_then(|x| x.as_bool()).unwrap_or(false);
        let mut hooks: BTreeSet<String> = BTreeSet::new();
        for hv in [read_json(&p.join("hooks/hooks.json")), manifest.clone()] {
            if let Some(h) = hv.get("hooks").and_then(|h| h.as_object()) { hooks.extend(h.keys().cloned()); }
        }
        let extra = json!({"key": key, "enabled": on, "hooks": hooks}).to_string();
        let desc = manifest.get("description").and_then(|d| d.as_str()).unwrap_or("");
        cat.node(&format!("plugin:{name}"), "plugin", &name, &name, desc, path, 0, &extra);
        skills_in(&mut cat, &p.join("skills"), &format!("{name}:"), &name);
        agents_in(&mut cat, &p.join("agents"), &format!("{name}:"), &name);
        commands_in(&mut cat, &p.join("commands"), &format!("{name}:"), &name);
        if on {
            for f in [".mcp.json", "mcp.json"] {
                let m = read_json(&p.join(f));
                if m.is_null() { continue; }
                add_servers(&mut cat, m.get("mcpServers").unwrap_or(&m), &name, "plugin", path, &mut server_cfgs);
            }
        }
    }
    skills_in(&mut cat, &cfg.join("skills"), "", "");
    agents_in(&mut cat, &cfg.join("agents"), "", "");
    commands_in(&mut cat, &cfg.join("commands"), "", "");
    let cj = read_json(&cfg.join(".claude.json"));
    add_servers(&mut cat, cj.get("mcpServers").unwrap_or(&Value::Null), "", "user", "", &mut server_cfgs);
    add_servers(&mut cat, settings.get("mcpServers").unwrap_or(&Value::Null), "", "user", "", &mut server_cfgs);
    // MCP tools from the servers themselves, cached per server config
    let cached: HashMap<String, (String, String, String, String)> = {
        let mut st = c.prepare("SELECT server, cfg, status, instructions, tools FROM mcp_probe").map_err(|e| e.to_string())?;
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))).map_err(|e| e.to_string())?.flatten().collect()
    };
    let todo: Vec<&(String, Value, String)> = server_cfgs.iter().filter(|(sid, cfg, _)| cached.get(sid).is_none_or(|x| x.0 != cfg.to_string())).collect();
    let probed: Vec<(String, String, String, String, String, u128)> = std::thread::scope(|s| {
        let hs: Vec<_> = todo.iter().map(|(sid, cfg, root)| s.spawn(move || {
            let t = Instant::now();
            let transport = cfg.get("type").and_then(|t| t.as_str()).unwrap_or("stdio");
            let (status, instr, tools) = if transport != "stdio" {
                (format!("{transport}: not probed (tools come from the session's authenticated connection)"), String::new(), "[]".to_string())
            } else {
                match probe_stdio(cfg, root) {
                    Ok((i, t)) => (format!("ok: {} tools", t.len()), i, serde_json::to_string(&t).unwrap()),
                    Err(e) => (format!("failed: {e}"), String::new(), "[]".to_string()),
                }
            };
            (sid.clone(), cfg.to_string(), status, instr, tools, t.elapsed().as_millis())
        })).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (sid, cfgs, status, instr, tools, ms) in &probed {
        c.execute("INSERT OR REPLACE INTO mcp_probe VALUES(?1,?2,?3,?4,?5,?6)", params![sid, cfgs, status, instr, tools, *ms as i64]).map_err(|e| e.to_string())?;
    }
    let report: Vec<String> = probed.iter().map(|p| format!("{} — {} ({} ms)", p.0, p.2, p.5)).collect();
    let mut st = c.prepare("SELECT server, status, instructions, tools FROM mcp_probe").map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(|e| e.to_string())?.flatten().collect();
    drop(st);
    for (sid, status, instr, tools) in rows {
        let Some(n) = cat.nodes.get(&sid).cloned() else { continue };
        let extra = json!({"transport": n.6, "probe": status}).to_string();
        cat.node(&sid, "mcp", &n.1, &n.2, &instr, &n.4, instr.len() as i64, &extra);
        let short = n.1.rsplit(':').next().unwrap_or(&n.1).to_string();
        let prefix = if n.2.is_empty() { mcp_prefix(&short) } else { mcp_prefix(&format!("plugin_{}_{}", n.2, short)) };
        let list: Vec<(String, String, i64)> = serde_json::from_str(&tools).unwrap_or_default();
        for (t, d, ch) in list {
            let name = format!("{prefix}{t}");
            let id = format!("tool:{name}");
            cat.node(&id, "tool", &name, &n.2, &d, "", ch, "mcp");
            cat.edge(&sid, "exposes", &id, 1);
        }
    }
    // MCP tools the sessions themselves used or found (PostToolUse capture): the only source for
    // servers mt can't probe without the session's auth
    let seen: Vec<String> = c.prepare("SELECT tool FROM seen_mcp").ok().map(|mut st| st.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
    let servers0: Vec<(String, String, String)> = cat.nodes.iter().filter(|(_, v)| v.0 == "mcp").map(|(k, v)| (k.clone(), v.1.clone(), v.2.clone())).collect();
    for t in seen {
        let id = format!("tool:{t}");
        if cat.nodes.contains_key(&id) { continue; }
        if let Some((sid, _, plugin)) = servers0.iter().find(|(_, n, pl)| { let short = n.rsplit(':').next().unwrap_or(n); t.starts_with(&if pl.is_empty() { mcp_prefix(short) } else { mcp_prefix(&format!("plugin_{pl}_{short}")) }) }) {
            cat.node(&id, "tool", &t, plugin, "", "", 0, "mcp:seen");
            cat.edge(sid, "exposes", &id, 1);
        }
    }
    // mentions: a skill or agent body naming an MCP server's tools
    let servers: Vec<(String, String, String)> = cat.nodes.iter().filter(|(_, v)| v.0 == "mcp").map(|(k, v)| (k.clone(), v.1.clone(), v.2.clone())).collect();
    let mut mentions = vec![];
    for (id, v) in &cat.nodes {
        if (v.0 != "skill" && v.0 != "agent") || v.4.is_empty() { continue; }
        let Ok(src) = fs::read_to_string(&v.4) else { continue };
        for (sid, sname, plugin) in &servers {
            let short = sname.rsplit(':').next().unwrap_or(sname);
            let prefix = if plugin.is_empty() { mcp_prefix(short) } else { mcp_prefix(&format!("plugin_{plugin}_{short}")) };
            if short.len() > 3 && src.contains(&prefix) { mentions.push((id.clone(), sid.clone())); }
        }
    }
    for (a, b) in mentions { cat.edge(&a, "mentions", &b, 1); }
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute_batch("DELETE FROM nodes; DELETE FROM edges;").map_err(|e| e.to_string())?;
    for (id, v) in &cat.nodes {
        tx.execute("INSERT INTO nodes VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id, v.0, v.1, v.2, v.3, v.4, v.5, v.6]).map_err(|e| e.to_string())?;
    }
    for (s, r, d, w) in &cat.edges { tx.execute("INSERT OR REPLACE INTO edges VALUES(?1,?2,?3,?4)", params![s, r, d, w]).map_err(|e| e.to_string())?; }
    tx.commit().map_err(|e| e.to_string())?;
    Ok((cat.nodes.len(), cat.edges.len(), report))
}

// ---------------------------------------------------------------- project facts from the code

/// Words that name an activity rather than a product or technology: they never anchor an item.
const GENERIC: &[&str] = &["query", "optimizer", "specialist", "expert", "engineer", "architect", "reviewer", "orchestrator",
    "design", "designer", "review", "code", "test", "tests", "testing", "debug", "debugging", "plan", "planning", "writing", "write",
    "build", "builder", "data", "database", "migration", "api", "backend", "frontend", "fullstack", "app", "apps", "web",
    "page", "site", "docs", "doc", "help", "helper", "setup", "config", "status", "deploy", "management", "manager", "best", "practices",
    "git", "workflow", "agent", "agents", "skill", "skills", "system", "systems", "plugin", "official", "claude", "ai", "mcp", "server",
    "tool", "tools", "dev", "development", "secure", "security", "qa", "saas", "conversion", "mobile", "md", "improver", "revise",
    "simplifier", "feature", "lsp", "cost", "realistic", "market", "me", "local", "directory", "store", "cli", "cc", "init", "run",
    "loop", "schedule", "simplify", "update", "keybindings", "batch", "fast", "prototype", "jam", "kit", "receipt", "pre", "superpowers",
    "github", "com", "org", "io", "net", "golang", "go", "js", "ts", "types", "node", "core", "utils", "util", "lib", "sdk", "client", "x"];

pub(crate) fn generic(t: &str) -> bool { GENERIC.contains(&t) || STOP.contains(&t) || t.len() < 3 || t.chars().all(|c| c.is_ascii_digit()) }

/// What the repo is made of, each fact with the file that shows it.
#[derive(Default)]
pub struct Facts { pub strong: BTreeMap<String, String>, pub refs: HashMap<String, i64>, pub langs: Vec<(String, i64)>, pub files: usize, pub ms: u128 }

impl Facts {
    /// Evidence that the project uses `anchor`, if any: a dependency or file is strong, an
    /// identifier counts when it is referenced at least 3 times.
    pub fn evidence(&self, anchor: &str, refs_count: bool) -> Option<String> {
        if let Some(e) = self.strong.get(anchor) { return Some(e.clone()); }
        if !refs_count { return None; }
        let n = self.refs.get(anchor).copied().unwrap_or(0);
        if n >= 3 { return Some(format!("{n} identifier references in code")); }
        None
    }
}

pub(crate) fn split_words(s: &str) -> Vec<String> {
    // camelCase and separators: "PostHogClient" -> post, hog, client and the joined "posthog"
    let mut out = vec![];
    for part in s.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() { continue; }
        out.push(part.to_lowercase());
        let mut w = String::new();
        let mut pieces = vec![];
        for ch in part.chars() {
            if ch.is_uppercase() && !w.is_empty() && !w.chars().last().unwrap().is_uppercase() { pieces.push(std::mem::take(&mut w)); }
            w.push(ch);
        }
        pieces.push(w);
        if pieces.len() > 1 { for p in pieces { out.push(p.to_lowercase()); } }
    }
    out
}

pub fn facts(root: &Path) -> Facts {
    let t = Instant::now();
    let mut f = Facts::default();
    let strong = |tok: &str, ev: String, f: &mut Facts| {
        for w in split_words(tok) { if !generic(&w) { f.strong.entry(w).or_insert_with(|| ev.clone()); } }
    };
    let mut langs: HashMap<String, i64> = HashMap::new();
    let walker = ignore::WalkBuilder::new(root).hidden(false).max_depth(Some(8)).filter_entry(|e| e.file_name() != ".git" && e.file_name() != "node_modules").build();
    for e in walker.flatten().take(60_000) {
        let p = e.path();
        if !p.is_file() { continue; }
        f.files += 1;
        let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let depth = rel.matches('/').count();
        if let Some(ext) = p.extension() { *langs.entry(ext.to_string_lossy().to_lowercase()).or_default() += 1; }
        // marker files near the root: vercel.json, next.config.ts, Dockerfile, .github, schema.prisma
        if depth <= 2 {
            let stem = name.trim_start_matches('.').split('.').next().unwrap_or("").to_string();
            strong(&stem, format!("file {rel}"), &mut f);
            for d in rel.split('/').take(depth) { strong(d.trim_start_matches('.'), format!("dir {rel}"), &mut f); }
        }
        let _ = name;
    }
    // declaration files: what a project says it depends on lives in files that are not code. mt
    // knows no manifest format; a file near a project root counts when the index sees it as
    // config, or finds at most one definition in it (go.mod, requirements.txt, Gemfile, a .csproj
    // all qualify by that rule). Lockfiles list transitive dependencies, so they are skipped.
    let decl_ok = |path: &str, role: &str, ndefs: i64| -> bool {
        let lname = path.rsplit('/').next().unwrap_or(path).to_lowercase();
        let depth = path.matches('/').count();
        depth <= 3 && !lname.contains("lock") && !lname.ends_with(".sum") && !lname.ends_with(".md") && !lname.ends_with(".mdx")
            && (role == "config" || ndefs <= 1) && !matches!(role, "test" | "integration" | "migration")
    };
    if let Ok(db) = Connection::open_with_flags(root.join(".mt/index.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        let rows: Vec<(String, String, i64, i64)> = db.prepare("SELECT f.path, f.role, f.size, (SELECT count(*) FROM defs d WHERE d.file = f.id) FROM files f").ok()
            .map(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
        let ident = regex::Regex::new(r"[A-Za-z@][A-Za-z0-9_.\-/@]{1,}").unwrap();
        let version = regex::Regex::new(r#"[\^~>=<!: '"v]*\d+\.\d+"#).unwrap();
        for (path, role, size, ndefs) in rows {
            if size > 200_000 || !decl_ok(&path, &role, ndefs) { continue; }
            // ignore lists name what is NOT part of the project
            if path.rsplit('/').next().unwrap_or(&path).to_lowercase().ends_with("ignore") { continue; }
            let Ok(text) = fs::read_to_string(root.join(&path)) else { continue };
            // code (a script, a source file) is not a declaration, whatever its definition count;
            // data grammars (json, yaml, toml, xml) and plain text are
            if role != "config" {
                if let crate::languages::Tier::TreeSitter(l) = crate::languages::detect(Path::new(&path), &text) {
                    if !matches!(l.id, "json" | "yaml" | "toml" | "xml") { continue; }
                }
            }
            // a dependency is the name written just before a version: "posthog-js": "^1.2",
            // github.com/x/y v1.2.3, requests==2.31, serde = "1.0", gem 'rails', '~> 7.0', redis:7.2
            for (i, l) in text.lines().enumerate().take(3000) {
                for v in version.find_iter(l) {
                    let before = &l[..v.start()];
                    if let Some(m) = ident.find_iter(before).filter(|m| !matches!(m.as_str().to_lowercase().as_str(), "version" | "ver" | "v")).last() {
                        let dep = m.as_str().trim_end_matches(['-', '.', '@', '/']);
                        strong(dep, format!("{path}:{}: {dep}", i + 1), &mut f);
                    }
                }
            }
        }
    }
    // identifiers the code references, from mt's own code index when it exists
    if let Ok(db) = Connection::open_with_flags(root.join(".mt/index.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        if let Ok(mut st) = db.prepare("SELECT r.name, count(*) FROM refs r JOIN files f ON f.id = r.file WHERE f.role != 'doc' GROUP BY r.name") {
            for (n, k) in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).into_iter().flatten().flatten() {
                for w in split_words(&n) { if !generic(&w) { *f.refs.entry(w).or_default() += k; } }
            }
        }
    }
    let mut l: Vec<(String, i64)> = langs.into_iter().collect();
    l.sort_by_key(|x| -x.1);
    l.truncate(12);
    for (ext, n) in &l { if !generic(ext) { f.strong.entry(ext.clone()).or_insert_with(|| format!("{n} .{ext} files")); } }
    f.langs = l;
    f.ms = t.elapsed().as_millis();
    f
}

// ---------------------------------------------------------------- graph in memory

#[derive(Clone, Debug)]
pub struct Node { pub id: String, pub kind: String, pub name: String, pub plugin: String, pub desc: String, pub path: String, pub chars: i64, pub extra: String }

pub struct Graph {
    pub nodes: HashMap<String, Node>,
    out: HashMap<String, Vec<(String, String, i64)>>,
    inn: HashMap<String, Vec<(String, String, i64)>>,
    pub facts: Facts,
    /// plugin name -> its product anchor, when the plugin is about one product
    pub(crate) product: HashMap<String, String>,
}

impl Graph {
    pub fn load(c: &Connection, facts: Facts) -> rusqlite::Result<Graph> {
        let mut g = Graph { nodes: HashMap::new(), out: HashMap::new(), inn: HashMap::new(), facts, product: HashMap::new() };
        let mut s = c.prepare("SELECT id, kind, name, plugin, descr, path, chars, extra FROM nodes")?;
        for n in s.query_map([], |r| Ok(Node { id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, plugin: r.get(3)?, desc: r.get(4)?, path: r.get(5)?, chars: r.get(6)?, extra: r.get(7)? }))?.flatten() {
            g.nodes.insert(n.id.clone(), n);
        }
        let mut s = c.prepare("SELECT src, rel, dst, w FROM edges")?;
        for (a, r, b, w) in s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))?.flatten() {
            g.out.entry(a.clone()).or_default().push((r.clone(), b.clone(), w));
            g.inn.entry(b).or_default().push((r, a, w));
        }
        g.product = g.products();
        Ok(g)
    }
    #[cfg(test)]
    fn from_parts(nodes: Vec<Node>, edges: &[(&str, &str, &str)], facts: Facts) -> Graph {
        let mut g = Graph { nodes: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(), out: HashMap::new(), inn: HashMap::new(), facts, product: HashMap::new() };
        for (a, r, b) in edges {
            g.out.entry(a.to_string()).or_default().push((r.to_string(), b.to_string(), 1));
            g.inn.entry(b.to_string()).or_default().push((r.to_string(), a.to_string(), 1));
        }
        g.product = g.products();
        g
    }
    /// A plugin is about one product when at least half of its items name that product in their
    /// description (posthog, vercel, stripe); workflow plugins (superpowers) are not.
    fn products(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        for p in self.nodes.values().filter(|n| n.kind == "plugin") {
            let kids: Vec<&Node> = self.out(&p.id, "provides").iter().filter_map(|(k, _)| self.nodes.get(*k)).collect();
            for w in split_words(&p.name) {
                if generic(&w) { continue; }
                let named = kids.iter().filter(|k| k.desc.to_lowercase().contains(&w)).count();
                if kids.is_empty() || named * 2 >= kids.len() { m.insert(p.name.clone(), w); break; }
            }
        }
        m
    }
    pub fn out(&self, id: &str, rel: &str) -> Vec<(&str, i64)> {
        self.out.get(id).into_iter().flatten().filter(|e| e.0 == rel).map(|e| (e.1.as_str(), e.2)).collect()
    }
    pub fn inn(&self, id: &str, rel: &str) -> Vec<(&str, i64)> {
        self.inn.get(id).into_iter().flatten().filter(|e| e.0 == rel).map(|e| (e.1.as_str(), e.2)).collect()
    }
    /// The products or technologies an item is about: its plugin's product, an MCP server's own
    /// name, or the specific words in a standalone skill's or agent's name (prisma-query-optimizer
    /// -> prisma). Empty means general-purpose.
    pub fn anchors(&self, n: &Node) -> Vec<String> {
        if !n.plugin.is_empty() {
            return self.product.get(&n.plugin).cloned().into_iter().collect();
        }
        let name = match n.kind.as_str() {
            "tool" => self.inn(&n.id, "exposes").first().and_then(|(s, _)| self.nodes.get(*s)).map(|s| s.name.clone()).unwrap_or_default(),
            _ => n.name.clone(),
        };
        let mut v: Vec<String> = split_words(&name).into_iter().filter(|w| !generic(w)).collect();
        v.sort(); v.dedup();
        v
    }
    /// (anchor, evidence) for the first anchor the code shows, or None. Product names (a plugin's
    /// product, an MCP server) also count when code identifiers reference them; the plain words in a
    /// standalone skill's name (level-design, save-systems) need a dependency, file or extension.
    pub fn grounded(&self, n: &Node) -> Option<(String, String)> {
        let product = !n.plugin.is_empty() || n.kind == "mcp" || n.kind == "tool";
        self.anchors(n).into_iter().find_map(|a| self.facts.evidence(&a, product).map(|e| (a, e)))
    }
    pub fn resolve(&self, q: &str) -> Option<&Node> {
        if let Some(n) = self.nodes.get(q) { return Some(n); }
        for k in ["skill", "agent", "tool", "mcp", "plugin"] { if let Some(n) = self.nodes.get(&format!("{k}:{q}")) { return Some(n); } }
        let ql = q.to_lowercase();
        self.nodes.values().filter(|n| n.name.to_lowercase() == ql || n.name.to_lowercase().ends_with(&format!(":{ql}"))).min_by_key(|n| n.id.len())
    }
}

// ---------------------------------------------------------------- effective state for one project

/// What the project's settings layers (user, project, local, plus an extra --settings file) say
/// about each item, merged the way Claude Code merges them for these keys.
pub struct State { pub(crate) deny: HashSet<String>, denied_servers: HashSet<String>, overrides: HashMap<String, String>, plugins: HashMap<String, bool> }

pub fn state(project: &Path, extra: Option<&Path>) -> State {
    let mut st = State { deny: HashSet::new(), denied_servers: HashSet::new(), overrides: HashMap::new(), plugins: HashMap::new() };
    let mut layers = vec![config_dir().join("settings.json"), project.join(".claude/settings.json"), project.join(".claude/settings.local.json")];
    if let Some(e) = extra { layers.push(e.to_path_buf()); }
    for l in layers {
        let v = read_json(&l);
        for d in v.pointer("/permissions/deny").and_then(|x| x.as_array()).into_iter().flatten() { if let Some(s) = d.as_str() { st.deny.insert(s.to_string()); } }
        for d in v.get("deniedMcpServers").and_then(|x| x.as_array()).into_iter().flatten() {
            if let Some(s) = d.get("serverName").and_then(|x| x.as_str()) { st.denied_servers.insert(s.to_string()); }
        }
        for (k, o) in v.get("skillOverrides").and_then(|x| x.as_object()).into_iter().flatten() { if let Some(s) = o.as_str() { st.overrides.insert(k.clone(), s.to_string()); } }
        for (k, o) in v.get("enabledPlugins").and_then(|x| x.as_object()).into_iter().flatten() { if let Some(b) = o.as_bool() { st.plugins.insert(k.clone(), b); } }
    }
    st
}

impl Graph {
    fn plugin_on(&self, st: &State, plugin: &str) -> bool {
        if plugin.is_empty() { return true; }
        let Some(p) = self.nodes.get(&format!("plugin:{plugin}")) else { return true };
        let extra: Value = serde_json::from_str(&p.extra).unwrap_or(Value::Null);
        let key = extra.get("key").and_then(|k| k.as_str()).unwrap_or("");
        st.plugins.get(key).copied().unwrap_or(extra.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
    }
    /// "on", "deferred", or why it is unavailable here.
    pub fn availability(&self, st: &State, n: &Node) -> String {
        if !self.plugin_on(st, &n.plugin) { return format!("off: plugin {} disabled here", n.plugin); }
        match n.kind.as_str() {
            "skill" => match st.overrides.get(&n.name).map(|s| s.as_str()) {
                Some("off") => "off: skillOverrides".into(), Some("name-only") => "name-only".into(), _ => "on".into() },
            "agent" => if st.deny.contains(&format!("Agent({})", n.name)) { "off: denied".into() } else { "on".into() },
            "mcp" => if st.denied_servers.contains(n.name.rsplit(':').next().unwrap_or(&n.name)) { "off: deniedMcpServers".into() } else { "on".into() },
            "tool" => {
                if st.deny.contains(&n.name) { return "off: denied".into(); }
                if let Some(rest) = n.name.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or("");
                    if st.deny.contains(&format!("mcp__{server}")) { return "off: denied".into(); }
                    for (s, _) in self.inn(&n.id, "exposes") {
                        if let Some(sn) = self.nodes.get(s) { let a = self.availability(st, sn); if a.starts_with("off") { return a; } }
                    }
                }
                if n.extra.ends_with("deferred") { "deferred".into() } else { "on".into() }
            }
            _ => "on".into(),
        }
    }
}

// ---------------------------------------------------------------- find

const STOP: &[&str] = &["find", "give", "show", "shows", "wrong", "each", "line", "lines", "anything", "nothing", "modify", "does", "doing", "done", "look", "tell", "explain", "then", "convention", "function", "functions", "method", "methods", "which", "where", "what", "whose", "why", "of", "in", "to", "is", "it", "an", "as", "at", "by", "on", "or", "we", "me", "my", "so", "do", "be", "if", "no", "up", "us", "am", "go", "he", "la", "vs", "the", "and", "for", "with", "this", "that", "use", "using", "when", "from", "into", "you", "your", "are", "can",
    "how", "what", "want", "need", "please", "make", "get", "all", "any", "our", "about", "like", "should", "would", "could", "will",
    "just", "also", "have", "has", "not", "but", "its", "it's", "let", "lets", "let's", "then", "them", "there", "here", "via", "one",
    "some", "out", "new", "add", "work", "working", "file", "files", "code", "user", "users", "skill", "tool", "tools", "mcp", "claude"];

fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    if w.len() > 4 && w.ends_with("ies") { return format!("{}y", &w[..w.len() - 3]); }
    if w.len() > 5 && w.ends_with("ing") { return w[..w.len() - 3].to_string(); }
    if w.len() > 7 && w.ends_with("ment") { return w[..w.len() - 4].to_string(); }
    if w.len() > 4 && w.ends_with("ed") && !w.ends_with("eed") { return w[..w.len() - 2].to_string(); }
    // "es" only after s, x, z, ch, sh (boxes, matches); "invoices" -> "invoice", not "invoic"
    if w.len() > 4 && w.ends_with("es") && ["ses", "xes", "zes", "ches", "shes"].iter().any(|x| w.ends_with(x)) { return w[..w.len() - 2].to_string(); }
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") { return w[..w.len() - 1].to_string(); }
    w
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1 && !STOP.contains(&w.to_lowercase().as_str())).map(stem).collect()
}

pub struct Hit { pub id: String, pub score: f64, pub text: f64, pub why: Vec<String> }

/// BM25 over each item's name (x3) and description, plus a prior when the item's anchor is in the
/// project's code.
pub fn find(g: &Graph, query: &str, limit: usize) -> Vec<Hit> {
    let q: Vec<String> = { let mut v = tokens(query); v.sort(); v.dedup(); v };
    if q.is_empty() { return vec![]; }
    let docs: Vec<(&Node, HashMap<String, f64>, f64)> = g.nodes.values().filter(|n| n.kind != "plugin").map(|n| {
        let mut tf: HashMap<String, f64> = HashMap::new();
        let mut len = 0.0;
        let mut add = |s: &str, w: f64| { for t in tokens(s) { *tf.entry(t).or_default() += w; len += w; } };
        add(&n.name.replace("__", " ").replace(['_', '-', ':'], " "), 3.0);
        add(&n.desc.chars().take(1200).collect::<String>(), 1.0);
        if !n.plugin.is_empty() { add(&n.plugin, 1.0); }
        (n, tf, len)
    }).collect();
    let nd = docs.len() as f64;
    let avg = docs.iter().map(|d| d.2).sum::<f64>() / nd.max(1.0);
    let df: HashMap<&String, f64> = q.iter().map(|t| (t, docs.iter().filter(|d| d.1.contains_key(t)).count() as f64)).collect();
    let mut hits: Vec<Hit> = docs.iter().filter_map(|(n, tf, len)| {
        let mut s = 0.0; let mut why = vec![];
        for t in &q {
            let Some(f) = tf.get(t) else { continue };
            let idf = ((nd - df[t] + 0.5) / (df[t] + 0.5) + 1.0).ln();
            s += idf * f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / avg));
            why.push(t.clone());
        }
        if s <= 0.0 { return None; }
        // the prompt names the item: "use the humanizer skill", "posthog:creating-surveys"
        let short = n.name.rsplit(':').next().unwrap_or(&n.name).to_lowercase();
        let ql = query.to_lowercase();
        let distinctive = short.contains('-') || short.len() >= 8;
        let s = if ql.contains(&n.name.to_lowercase()) || (distinctive && ql.contains(&short)) { why.push("named in the prompt".into()); s * 2.0 } else { s };
        let mut prior = 0.0;
        let mut s = s;
        if let Some((a, e)) = g.grounded(n) { prior = 0.25 * s; why.push(format!("{a} in code: {e}")); }
        else if !n.plugin.is_empty() && !g.anchors(n).is_empty() { s *= 0.6; why.push(format!("no {} in this code", g.anchors(n).join("/"))); }
        Some(Hit { id: n.id.clone(), score: s + prior, text: s, why })
    }).collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(limit * 4);
    hits
}

/// One card per result. MCP tools of one server collapse into a single card with one `select:`
/// line (hit tools first); a server whose tools are unknown (HTTP) gets a keyword ToolSearch.
pub fn cards(g: &Graph, st: &State, hits: &[Hit], limit: usize, min_text: f64) -> Vec<String> {
    let mut out = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let top = hits.first().map(|h| h.text).unwrap_or(0.0);
    let qwords: Vec<String> = hits.iter().flat_map(|h| h.why.iter().filter(|w| !w.contains(' ')).cloned()).collect::<BTreeSet<_>>().into_iter().take(3).collect();
    for h in hits {
        if out.len() >= limit || h.text < min_text || h.text < top * 0.5 { continue; }
        let Some(n) = g.nodes.get(&h.id) else { continue };
        // a vendor plugin's item (posthog, stripe) for a product this code doesn't use: only when the
        // prompt names the product. MCP servers and agents are capabilities and only rank lower.
        if !n.plugin.is_empty() && n.kind != "mcp" && g.grounded(n).is_none() {
            let anchors = g.anchors(n);
            if !anchors.is_empty() && !anchors.iter().any(|a| h.why.iter().any(|w| w == a)) { continue; }
        }
        let words: Vec<&String> = h.why.iter().filter(|w| !w.contains(' ')).collect();
        let named = h.why.iter().any(|w| w == "named in the prompt");
        // something the profile turned off (the code doesn't use it) shows only when the prompt
        // names it or its product: "use the humanizer skill", "check my meshy balance"
        {
            let sid1 = if n.kind == "tool" { g.inn(&n.id, "exposes").first().map(|x| x.0.to_string()) } else { None };
            let target = sid1.as_deref().and_then(|s| g.nodes.get(s)).unwrap_or(n);
            let off = g.availability(st, target).starts_with("off");
            let anchor_named = g.anchors(target).iter().any(|a| h.why.iter().any(|w| w == a));
            if off && !named && !anchor_named { continue; }
        }
        // an MCP server is named by its tools too (tabs_context, navigate, read_page)
        let sid0 = if n.kind == "tool" { g.inn(&n.id, "exposes").first().map(|x| x.0.to_string()) } else if n.kind == "mcp" { Some(n.id.clone()) } else { None };
        let mut name_text = n.name.clone();
        if let Some(sid) = &sid0 { for (t, _) in g.out(sid, "exposes") { name_text.push(' '); name_text.push_str(t); } }
        let name_words = tokens(&name_text.replace("__", " ").replace(['_', '-', ':'], " "));
        if !named && !(words.len() >= 2 && words.iter().any(|w| name_words.contains(w))) { continue; }
        let server = if n.kind == "tool" { g.inn(&n.id, "exposes").first().map(|x| x.0.to_string()) } else if n.kind == "mcp" { Some(n.id.clone()) } else { None };
        if !done.insert(server.clone().unwrap_or(n.id.clone())) { continue; }
        let avail = g.availability(st, n);
        let desc: String = n.desc.split('\n').find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect();
        let card = match (n.kind.as_str(), &server) {
            ("skill", _) => {
                let how = match avail.as_str() { "on" | "name-only" => format!("Skill(\"{}\")", n.name), _ => format!("mt tools show {}   (reads the skill without loading it)", n.name) };
                let mut c = format!("skill {} [{avail}] — {desc}\n  use: {how}", n.name);
                let needs: Vec<String> = g.out(&n.id, "mentions").iter().filter_map(|(m, _)| g.nodes.get(*m)).map(|m| m.name.clone()).collect();
                if !needs.is_empty() { c += &format!("\n  works with mcp: {}", needs.join(", ")); }
                c
            }
            ("agent", _) => {
                let how = if avail == "on" { format!("Agent(subagent_type=\"{}\")", n.name) } else { format!("mt tools show {}   (then Agent general-purpose with that prompt)", n.name) };
                format!("agent {} [{avail}] — {desc}\n  use: {how}", n.name)
            }
            (_, Some(sid)) => {
                let Some(s) = g.nodes.get(sid) else { continue };
                let sav = g.availability(st, s);
                let mut pick: Vec<String> = hits.iter().filter(|x| x.text >= top * 0.2).filter_map(|x| g.nodes.get(&x.id))
                    .filter(|t| t.kind == "tool" && g.inn(&t.id, "exposes").iter().any(|e| e.0 == sid)).map(|t| t.name.clone()).take(8).collect();
                let all: Vec<&str> = g.out(sid, "exposes").into_iter().map(|(t, _)| &t[5..]).collect();
                if pick.is_empty() { pick = all.iter().take(6).map(|t| t.to_string()).collect(); }
                let sdesc: String = s.desc.split('\n').find(|l| !l.trim().is_empty() && !l.starts_with('#')).unwrap_or(&desc).chars().take(160).collect();
                let short = s.name.rsplit(':').next().unwrap_or(&s.name).to_string();
                let how = if sav.starts_with("off") { format!("mt tools enable {}   (then restart the session)", s.name) }
                    else if pick.is_empty() { format!("ToolSearch \"+{} {}\"", mcp_prefix(if s.plugin.is_empty() { short.clone() } else { format!("plugin_{}_{}", s.plugin, short) }.as_str()).trim_start_matches("mcp__").trim_end_matches('_'), qwords.join(" ")) }
                    else { format!("ToolSearch select:{}", pick.join(",")) };
                format!("mcp {} [{sav}] ({} tools) — {sdesc}\n  use: {how}", s.name, all.len())
            }
            _ => continue,
        };
        out.push(format!("{card}\n  matched: {}", h.why.join(", ")));
    }
    out
}

// ---------------------------------------------------------------- code side of the graph

/// The lines `start..=end` (0-based) of `path` as they are on disk now, numbered, capped at `max`
/// lines, headed by the same span hash `mt span` prints, so the reader can quote them instead of
/// reading the file again.
fn inline_lines(root: &Path, path: &str, start: i64, end: i64, max: usize) -> Option<String> {
    let src = fs::read_to_string(root.join(path)).ok()?;
    let lines: Vec<String> = src.lines().map(String::from).collect();
    let (s, e) = (start.max(0) as usize, (end.max(start) as usize).min(lines.len().saturating_sub(1)));
    if s >= lines.len() { return None; }
    let h = crate::context::span_hash(&lines, s, e);
    let shown = (e - s + 1).min(max);
    let mut out = format!("\n  lines {path}:{}-{} h={h} (read from disk now):", s + 1, e + 1);
    for (i, l) in lines[s..s + shown].iter().enumerate() { out += &format!("\n  {:>5}  {}", s + i + 1, l); }
    if shown < e - s + 1 { out += &format!("\n  … {} more lines: mt span {path} L{}-{}", e - s + 1 - shown, s + shown + 1, e + 1); }
    Some(out)
}

/// Personalized PageRank: `p` is the restart distribution (seed scores), edges are undirected and
/// weighted. Mass on a node without edges goes back to the seeds, so an isolated seed keeps what
/// its lexical score gave it and a connected cluster of seeds lifts every member.
pub fn ppr(n: usize, edges: &[(usize, usize, f64)], p: &[f64], alpha: f64, iters: usize) -> Vec<f64> {
    let total: f64 = p.iter().sum::<f64>().max(1e-12);
    let p: Vec<f64> = p.iter().map(|x| x / total).collect();
    let mut adj: Vec<Vec<(usize, f64)>> = vec![vec![]; n];
    for &(a, b, w) in edges { if a != b { adj[a].push((b, w)); adj[b].push((a, w)); } }
    let deg: Vec<f64> = adj.iter().map(|v| v.iter().map(|x| x.1).sum()).collect();
    let mut r = p.clone();
    for _ in 0..iters {
        let mut next: Vec<f64> = p.iter().map(|x| alpha * x).collect();
        let mut dangling = 0.0;
        for u in 0..n {
            if deg[u] == 0.0 { dangling += r[u]; continue; }
            for &(v, w) in &adj[u] { next[v] += (1.0 - alpha) * r[u] * w / deg[u]; }
        }
        for (x, pv) in next.iter_mut().zip(&p) { *x += (1.0 - alpha) * dangling * pv; }
        r = next;
    }
    r
}

/// What the prompt asks for: a change (add, fix, rename …), a debugging hunt (fails, drops,
/// panics …) or a question. Cards are shaped by it: a change gets an edit plan, a hunt gets recent
/// commits, a question gets completeness claims.
pub fn task_kind(query: &str) -> &'static str {
    let w: Vec<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric()).map(String::from).collect();
    let has = |xs: &[&str]| w.iter().any(|x| xs.contains(&x.as_str()));
    if has(&["fails", "failing", "error", "errors", "bug", "broken", "crash", "crashes", "panic", "panics", "drops", "hangs", "flaky", "regression", "wrong", "debug", "500", "timeout"]) { return "debug"; }
    if has(&["add", "implement", "create", "change", "rename", "remove", "delete", "update", "refactor", "migrate", "support", "make", "fix", "replace", "introduce", "extend", "expose"]) { return "change"; }
    "question"
}

/// Files committed together in the last 400 commits (commits touching more than 30 files are
/// skipped as sweeping). Cached per HEAD in the tools db.
pub fn cochange(root: &Path) -> HashMap<(String, String), i64> {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(root).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut m = HashMap::new();
    if head.is_empty() { return m; }
    let key = root.to_string_lossy().into_owned();
    if let Ok(c) = open() {
        let _ = c.execute_batch("CREATE TABLE IF NOT EXISTS cochange(root TEXT, head TEXT, a TEXT, b TEXT, n INTEGER)");
        let cached: Vec<(String, String, i64)> = c.prepare("SELECT a, b, n FROM cochange WHERE root = ?1 AND head = ?2").ok()
            .map(|mut s| s.query_map(params![key, head], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
        if !cached.is_empty() { for (a, b, n) in cached { if a != "-" { m.insert((a, b), n); } } return m; }
        let out = Command::new("git").args(["log", "-n", "400", "--name-only", "--format=%x01"]).current_dir(root).output().ok();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        for commit in text.split('\u{1}') {
            let files: Vec<&str> = commit.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
            if files.len() < 2 || files.len() > 30 { continue; }
            for a in &files { for b in &files { if a < b { *m.entry((a.to_string(), b.to_string())).or_default() += 1; } } }
        }
        let tx = c.unchecked_transaction();
        let _ = c.execute("DELETE FROM cochange WHERE root = ?1", params![key]);
        if m.is_empty() { let _ = c.execute("INSERT INTO cochange VALUES(?1,?2,'-','-',0)", params![key, head]); }
        for ((a, b), n) in &m { if *n >= 2 { let _ = c.execute("INSERT INTO cochange VALUES(?1,?2,?3,?4,?5)", params![key, head, a, b, n]); } }
        if let Ok(tx) = tx { let _ = tx.commit(); }
        m.retain(|_, n| *n >= 2);
    }
    m
}

/// The next migration in a directory of numbered migrations: (prefix width, next number, whether
/// migrations come as .up.sql/.down.sql pairs).
/// The next migration in a directory of numbered files: (prefix width, next number, and the
/// suffixes the latest migration was written with, e.g. [".up.sql", ".down.sql"] or [".sql"]).
/// Everything comes from the files already there; no format is assumed.
fn next_migration(root: &Path, dir: &str) -> Option<(usize, u64, Vec<String>)> {
    let rd = fs::read_dir(root.join(dir)).ok()?;
    let (mut max, mut width) = (0u64, 0usize);
    let mut by_num: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let digits: String = n.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(v) = digits.parse::<u64>() else { continue };
        if v >= max { max = v; width = digits.len(); }
        by_num.entry(v).or_default().push(n);
    }
    if width == 0 { return None; }
    // the suffixes of the latest migration: what follows its slug ("0005_x.up.sql" -> ".up.sql")
    let mut suffixes: Vec<String> = by_num.get(&max).into_iter().flatten().map(|n| {
        let rest = &n[n.find(|c: char| !c.is_ascii_digit()).unwrap_or(0)..];
        let parts: Vec<&str> = rest.split('.').collect();
        if parts.len() >= 3 && parts[parts.len() - 2].len() <= 5 { format!(".{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]) } else { format!(".{}", parts.last().unwrap_or(&"")) }
    }).collect();
    suffixes.sort(); suffixes.dedup();
    suffixes.sort_by_key(|x| x.contains("down") as u8);
    Some((width, max + 1, suffixes))
}

/// An HTTP route's shape for matching: `/v1/sessions/{id}`, `/v1/sessions/:id` and
/// `/v1/sessions/${id}` all become `/v1/sessions/*`.
pub fn route_shape(r: &str) -> String {
    let r = r.split(['?', '#']).next().unwrap_or(r).trim_end_matches('/');
    r.split('/').map(|seg| if seg.starts_with('{') || seg.starts_with(':') || seg.contains("${") || seg.starts_with('<') { "*" } else { seg }).collect::<Vec<_>>().join("/")
}

fn route_literal_rx() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| regex::Regex::new(r#"["'`](?:(?:GET|POST|PUT|PATCH|DELETE)\s+)?(/[A-Za-z0-9_\-./{}:$<>]+)["'`]"#).unwrap())
}

fn route_register_rx() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| regex::Regex::new(r#"(?i)(\.|@)(get|post|put|patch|delete|head|options|handle|handlefunc|route|method|all|api_view)\s*\("#).unwrap())
}

/// Routes registered to handlers, and every other line in the repo that names one of those routes
/// (frontend ajax, other services, tests): the edges that cross sub-projects.
pub struct Routes { pub handlers: Vec<(String, String, usize, String)>, pub calls: Vec<(String, String, usize)> }

pub fn routes(text: &HashMap<String, Vec<String>>, role_of: &HashMap<&str, &str>, def_names: &HashSet<String>) -> Routes {
    let ident = regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
    let mut handlers = vec![]; // (shape, handler last name, line0, path)
    let mut lits = vec![];     // (shape, path, line0)
    let mut paths: Vec<&String> = text.keys().collect();
    paths.sort();
    for p in paths {
        if role_of.get(p.as_str()) == Some(&"doc") { continue; }
        let lines = &text[p];
        for (i, l) in lines.iter().enumerate() {
            for c in route_literal_rx().captures_iter(l) {
                let shape = route_shape(&c[1]);
                if shape.matches('/').count() < 1 || shape.len() < 4 { continue; }
                let mut handler = None;
                if route_register_rx().is_match(l) {
                    let after = &l[c.get(0).unwrap().end()..];
                    handler = ident.find_iter(after).map(|m| m.as_str().to_string()).filter(|n| def_names.contains(n)).last();
                    if handler.is_none() && l.trim_start().starts_with('@') {
                        // decorator: the handler is the next definition
                        handler = lines.iter().skip(i + 1).take(3).flat_map(|n| ident.find_iter(n).map(|m| m.as_str().to_string()).collect::<Vec<_>>()).find(|n| def_names.contains(n));
                    }
                }
                match handler { Some(h) => handlers.push((shape, h, i, p.clone())), None => lits.push((shape, p.clone(), i)) }
            }
        }
    }
    let known: HashSet<&String> = handlers.iter().map(|h| &h.0).collect();
    let calls = lits.into_iter().filter(|(s, _, _)| known.contains(s)).collect();
    Routes { handlers, calls }
}

/// Every HTTP route in the repo: its handler and every place that calls it (for `mt tools routes`).
pub fn routes_report(root: &Path) -> String {
    let Ok(db) = Connection::open_with_flags(root.join(".mt/index.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return "no mt index here (run mt index)".into() };
    let files: Vec<(String, String)> = db.prepare("SELECT path, role FROM files").ok().map(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
    let text: HashMap<String, Vec<String>> = files.iter().filter_map(|(p, _)| fs::read_to_string(root.join(p)).ok().map(|t| (p.clone(), t.lines().map(String::from).collect()))).collect();
    let role_of: HashMap<&str, &str> = files.iter().map(|(p, r)| (p.as_str(), r.as_str())).collect();
    let names: HashSet<String> = db.prepare("SELECT name FROM defs").ok().map(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().map(|n| n.rsplit('.').next().unwrap_or(&n).to_string()).collect()).unwrap_or_default()).unwrap_or_default();
    let r = routes(&text, &role_of, &names);
    let mut out = format!("{} routes registered, {} call sites elsewhere\n", r.handlers.len(), r.calls.len());
    for (shape, h, line0, p) in &r.handlers {
        let calls: Vec<String> = r.calls.iter().filter(|c| &c.0 == shape).map(|c| format!("{}:{} ({})", c.1, c.2 + 1, role_of.get(c.1.as_str()).unwrap_or(&"?"))).collect();
        out += &format!("{shape}  → {h}  ({p}:{})\n", line0 + 1);
        for c in calls { out += &format!("    called from {c}\n"); }
    }
    out
}

/// Score a gold set: each entry is {"prompt", "expect": [names], "k"?}; a name is found when a code
/// card's heading or a tool card's heading carries it. Prints every entry, then recall@3, recall@k
/// and MRR, so a ranking change is a number, not an impression.
pub fn bench(g: &Graph, st: &State, root: &Path, gold: &Value) -> String {
    let items = gold.as_array().cloned().unwrap_or_default();
    let (mut r3, mut rk, mut mrr, mut total, mut mentioned) = (0.0, 0.0, 0.0, 0usize, 0.0);
    let mut out = String::new();
    for it in &items {
        let prompt = it["prompt"].as_str().unwrap_or("");
        let k = it.get("k").and_then(|x| x.as_u64()).unwrap_or(6) as usize;
        let code = code_cards_with(g, root, prompt, &CodeOpts { limit: k, budget: usize::MAX / 2, focus: None });
        let body = code.join("\n");
        // code cards and tool cards are separate lists in the hook, so each item is ranked in its own
        let code_heads: Vec<String> = code.iter().filter(|c| !c.starts_with("plan")).map(|c| c.lines().next().unwrap_or("").to_string()).collect();
        let tool_heads: Vec<String> = cards(g, st, &find(g, prompt, k), k, 0.0).iter().map(|c| c.lines().next().unwrap_or("").to_string()).collect();
        for e in it["expect"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            total += 1;
            if body.contains(e) { mentioned += 1.0; }
            let is = |h: &String| h.split_whitespace().any(|w| w == e || w.ends_with(&format!(".{e}")) || w.trim_end_matches(':') == e);
            let rank = code_heads.iter().position(is).or_else(|| tool_heads.iter().position(is));
            let heads: Vec<String> = code_heads.iter().chain(tool_heads.iter()).cloned().collect();
            match rank { Some(r) => { if r < 3 { r3 += 1.0; } if r < k { rk += 1.0; } mrr += 1.0 / (r as f64 + 1.0); out += &format!("  ✓ @{} {e:28} {prompt}\n", r + 1); }
                None => out += &format!("  ✗    {e:28} {prompt}\n      got: {}\n", heads.iter().take(4).map(|h| h.split(" — ").next().unwrap_or(h)).collect::<Vec<_>>().join(" | ")) }
        }
    }
    let n = total.max(1) as f64;
    out += &format!("\n{} prompts, {total} expected items: recall@3 {:.0}%, recall@k {:.0}%, MRR {:.3}; named anywhere in the code cards {:.0}%\n", items.len(), 100.0 * r3 / n, 100.0 * rk / n, mrr / n, 100.0 * mentioned / n);
    out
}

/// The embedding model, loaded once per process (None when it isn't on disk).
/// The static Model2Vec model, the fallback when a BERT cache is incomplete.
fn static_model() -> Option<&'static crate::embed::Embedder> {
    static M: std::sync::OnceLock<Option<crate::embed::Embedder>> = std::sync::OnceLock::new();
    M.get_or_init(|| { let d = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/mt/models/potion-base-8M"); crate::embed::Embedder::load(&d).ok() }).as_ref()
}

fn model() -> Option<&'static crate::embed::Embedder> {
    static M: std::sync::OnceLock<Option<crate::embed::Embedder>> = std::sync::OnceLock::new();
    M.get_or_init(|| match crate::embed::Embedder::load(&crate::embed::model_dir()) {
        Ok(m) => Some(m),
        Err(e) => { if std::env::var_os("MT_DEBUG").is_some() { eprintln!("mt embed: {e}"); } None }
    }).as_ref()
}

/// Vectors for `texts`, from the cache in tools.sqlite when present. A model too slow to embed
/// every definition per prompt (BERT) only answers when the cache already covers them all, or when
/// `fill` is set (`mt tools embed`, the bench); otherwise None and the caller falls back.
pub fn cached_vectors(m: &crate::embed::Embedder, texts: &[String], fill: bool) -> Option<Vec<Vec<f32>>> {
    if m.is_cheap() { return Some(m.embed_docs(texts)); }
    let c = open().ok()?;
    let _ = c.execute_batch("CREATE TABLE IF NOT EXISTS embcache(model TEXT, h TEXT, v BLOB, PRIMARY KEY(model, h))");
    let key = |t: &str| format!("{:016x}", crate::fnv1a(t.as_bytes()));
    let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    {
        // one read of the whole model's cache, not one query per definition
        let mut st = c.prepare("SELECT h, v FROM embcache WHERE model = ?1").ok()?;
        let all: HashMap<String, Vec<u8>> = st.query_map(params![m.name()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))).ok()?.flatten().collect();
        for (i, t) in texts.iter().enumerate() {
            if let Some(b) = all.get(&key(t)) { out[i] = Some(b.chunks_exact(4).map(|x| f32::from_le_bytes(x.try_into().unwrap())).collect()); }
        }
    }
    let missing: Vec<usize> = (0..texts.len()).filter(|i| out[*i].is_none()).collect();
    if !missing.is_empty() {
        if !fill { return None; }
        let todo: Vec<String> = missing.iter().map(|&i| texts[i].clone()).collect();
        let vs = m.embed_docs(&todo);
        let tx = c.unchecked_transaction().ok()?;
        for (&i, v) in missing.iter().zip(vs) {
            let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            let _ = c.execute("INSERT OR REPLACE INTO embcache VALUES(?1, ?2, ?3)", params![m.name(), key(&texts[i]), b]);
            out[i] = Some(v);
        }
        let _ = tx.commit();
    }
    Some(out.into_iter().map(|v| v.unwrap_or_default()).collect())
}

pub struct CodeOpts { pub limit: usize, pub budget: usize, pub focus: Option<PathBuf> }

/// Code nodes from mt's own index (`.mt/index.sqlite`): definitions, and through refs and tables
/// the tests, integration tests, migrations, models, queries and docs connected to each one, plus
/// the skills and MCP servers its file's technology points to. One card per symbol, every line a
/// file:line pointer; `mt context <name>` returns all the bodies in one more call.
pub fn code_cards(g: &Graph, root: &Path, query: &str, limit: usize) -> Vec<String> {
    code_cards_with(g, root, query, &CodeOpts { limit, budget: 9000, focus: None })
}

pub fn code_cards_with(g: &Graph, root: &Path, query: &str, o: &CodeOpts) -> Vec<String> {
    let limit = o.limit;
    let task = task_kind(query);
    let Ok(db) = Connection::open_with_flags(root.join(".mt/index.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return vec![] };
    let q: Vec<String> = { let mut v = tokens(query); v.sort(); v.dedup(); v };
    if q.is_empty() { return vec![]; }
    let joined: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect::<Vec<_>>()
        .windows(2).map(|w| format!("{}{}", w[0], w[1])).collect();
    struct D { name: String, kind: String, path: String, role: String, start: i64, end: i64, file: i64, head: i64 }
    let Ok(mut st) = db.prepare("SELECT d.name, d.kind, f.path, f.role, d.start, d.end, d.file, d.head FROM defs d JOIN files f ON f.id = d.file") else { return vec![] };
    let defs: Vec<D> = st.query_map([], |r| Ok(D { name: r.get(0)?, kind: r.get(1)?, path: r.get(2)?, role: r.get(3)?, start: r.get(4)?, end: r.get(5)?, file: r.get(6)?, head: r.get::<_, Option<i64>>(7)?.unwrap_or(0) }))
        .map(|it| it.flatten().collect()).unwrap_or_default();
    // every indexed file's lines, read once: doc comments, bridges, exhaustive claims
    let files: Vec<(String, String)> = db.prepare("SELECT path, role FROM files").ok()
        .map(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
    let text: HashMap<String, Vec<String>> = files.iter().filter_map(|(p, _)| {
        let full = root.join(p);
        if fs::metadata(&full).map(|m| m.len() > 600_000).unwrap_or(true) { return None; }
        fs::read_to_string(&full).ok().map(|t| (p.clone(), t.lines().map(String::from).collect()))
    }).collect();
    let role_of: HashMap<&str, &str> = files.iter().map(|(p, r)| (p.as_str(), r.as_str())).collect();
    // doc bridges: a Markdown or comment line naming `an_identifier` and two prompt words links the
    // plain-language phrase to that identifier ("soft delete" -> archived_at)
    let qset0: HashSet<String> = tokens(query).into_iter().collect();
    let tick = regex::Regex::new(r"`([A-Za-z_][A-Za-z0-9_.]{2,})`").unwrap();
    let mut bridges: HashMap<String, (usize, String)> = HashMap::new();
    for (p, lines) in &text {
        if role_of.get(p.as_str()) != Some(&"doc") { continue; }
        for (i, l) in lines.iter().enumerate() {
            if !l.contains('`') { continue; }
            let lw: HashSet<String> = tokens(l).into_iter().collect();
            let shared = lw.iter().filter(|w| qset0.contains(*w)).count();
            if shared < 2 { continue; }
            for c in tick.captures_iter(l) {
                let raw = c[1].rsplit('.').next().unwrap_or(&c[1]).to_string();
                // only distinctive, code-like names: `audit_log`, `WriteAudit`, not `state` or `List`
                if !(raw.contains('_') || raw.chars().skip(1).any(|ch| ch.is_uppercase()) || raw.len() >= 8) { continue; }
                let id = index::norm(&raw);
                let e = bridges.entry(id).or_insert((0, format!("{p}:{}", i + 1)));
                e.0 += shared;
            }
        }
    }
    let def_names: HashSet<String> = defs.iter().map(|d| d.name.rsplit('.').next().unwrap_or(&d.name).to_string()).collect();
    let rmap = routes(&text, &role_of, &def_names);
    let cased: HashSet<String> = query.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.chars().next().is_some_and(|c| c.is_uppercase()) || w.contains('_') || w.chars().skip(1).any(|c| c.is_uppercase())).map(index::norm).collect();
    let qwords: HashSet<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric()).map(String::from).collect();
    let wants_handler = ["endpoint", "endpoints", "route", "routes", "handler", "api"].iter().any(|w| qwords.contains(*w));
    let wants_ui = ["screen", "page", "panel", "button", "ui", "view", "form", "modal", "dashboard"].iter().any(|w| qwords.contains(*w));
    let handler_names: HashSet<String> = rmap.handlers.iter().map(|h| h.1.clone()).collect();
    // the clients: files that call routes and register none (a frontend, a CLI, another service)
    let server_files: HashSet<&str> = rmap.handlers.iter().map(|h| h.3.as_str()).collect();
    let client_files: HashSet<&str> = rmap.calls.iter().map(|c| c.1.as_str()).filter(|p| !server_files.contains(p) && !matches!(role_of.get(*p), Some(&"test") | Some(&"integration") | Some(&"config") | Some(&"doc"))).collect();
    let mut defs_by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, d) in defs.iter().enumerate() { defs_by_file.entry(d.path.as_str()).or_default().push(i); }
    let enclosing_def = |path: &str, line0: i64| -> Option<usize> {
        defs_by_file.get(path)?.iter().copied().filter(|&i| defs[i].start <= line0 && line0 <= defs[i].end).min_by_key(|&i| defs[i].end - defs[i].start)
    };
    // wire keys: what two components agree on without calling each other. A string literal, or an
    // identifier the repo doesn't define (StatusConflict, a header name), found in 2 to 8 files
    // spread over at least two sub-projects: event kinds, queue and channel names, JSON keys,
    // status codes. Routes have their own edges and are left out.
    let sub_of = |p: &str| -> String { p.split('/').take(2).collect::<Vec<_>>().join("/") };
    let lit_rx = regex::Regex::new(r#"["'`]([A-Za-z][A-Za-z0-9_.:\-]{3,48})["'`]"#).unwrap();
    let id_rx = regex::Regex::new(r"\b([A-Z][A-Za-z0-9]{5,40})\b").unwrap();
    let mut wire_at: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for (p, lines) in &text {
        if matches!(role_of.get(p.as_str()), Some(&"doc") | Some(&"config") | Some(&"migration")) || p.ends_with(".html") { continue; }
        for (i, l) in lines.iter().enumerate() {
            for c in lit_rx.captures_iter(l) { let k = c[1].to_string(); if !k.starts_with('/') && !generic(&k.to_lowercase()) { wire_at.entry(k).or_default().push((p.clone(), i)); } }
            for c in id_rx.captures_iter(l) { let k = c[1].to_string(); if !def_names.contains(&k) && !generic(&k.to_lowercase()) { wire_at.entry(k).or_default().push((p.clone(), i)); } }
        }
    }
    if std::env::var("MT_WIRE").as_deref() == Ok("off") { wire_at.clear(); }
    wire_at.retain(|_, v| {
        let files: HashSet<&str> = v.iter().map(|x| x.0.as_str()).collect();
        let subs: HashSet<String> = files.iter().map(|f| sub_of(f)).collect();
        (2..=8).contains(&files.len()) && subs.len() >= 2
    });
    // the definitions each key sits in
    let mut wire_defs: HashMap<String, Vec<usize>> = HashMap::new();
    for (k, v) in &wire_at { let mut ds: Vec<usize> = v.iter().filter_map(|(p, i)| enclosing_def(p, *i as i64)).collect(); ds.sort(); ds.dedup(); if ds.len() >= 2 { wire_defs.insert(k.clone(), ds); } }
    let mut keys_of: HashMap<usize, Vec<String>> = HashMap::new();
    for (k, ds) in &wire_defs { for &d in ds { keys_of.entry(d).or_default().push(k.clone()); } }
    // a definition's words: its name (weight 1) and its file's stem (0.5), so "audit log write"
    // finds WriteAudit in audit_write.go
    let words = |d: &D| -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = split_words(&d.name).into_iter().map(|w| (stem(&w), 1.0)).collect();
        let stem_of = Path::new(&d.path).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        // and its directory (web/src/screens -> screen, internal/store -> store)
        let dir_of = Path::new(&d.path).parent().and_then(|p| p.file_name()).map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        for w in split_words(&stem_of).into_iter().chain(split_words(&dir_of)).chain([d.kind.clone()]) { let w = stem(&w); if !v.iter().any(|x| x.0 == w) { v.push((w, 0.5)); } }
        // the doc comment above the definition (lines start..head), weight 0.6
        if let Some(lines) = text.get(&d.path) {
            let (a, b) = (d.start.max(0) as usize, (d.head.max(d.start) as usize).min(lines.len()));
            for l in lines.get(a..b).unwrap_or(&[]).iter().take(12) { for w in tokens(l) { if !v.iter().any(|x| x.0 == w) { v.push((w, 0.6)); } } }
        }
        v
    };
    let mut df: HashMap<String, f64> = HashMap::new();
    let dw: Vec<Vec<(String, f64)>> = defs.iter().map(|d| { let w = words(d); for x in &w { *df.entry(x.0.clone()).or_default() += 1.0; } w }).collect();
    let n = defs.len().max(1) as f64;
    let role_w = |r: &str| match r { "source" => 1.0, "model" | "query" => 0.95, "migration" => 0.8, "config" => 0.6, "test" | "integration" => 0.45, _ => 0.3 };
    let kind_w = |k: &str| match k { "var" | "const" | "constant" | "field" | "property" | "let" | "variable" | "rule" | "selector" | "key" => 0.45, "index" => 0.5, _ => 1.0 };
    let mut scored: Vec<(f64, usize, Vec<String>)> = defs.iter().enumerate().filter_map(|(i, d)| {
        if d.path.ends_with(".md") || d.path.ends_with(".html") { return None; }
        let w = &dw[i];
        // exact word, a close form (revok/revoke), or a code abbreviation of the prompt word (seq/sequence)
        let close = |a: &str, b: &str| a == b || {
            let (sh, lo) = if a.len() <= b.len() { (a, b) } else { (b, a) };
            sh.len() >= 3 && lo.starts_with(sh) && (lo.len() - sh.len() <= 2 || (sh == b && sh.len() >= 3))
        };
        let hit: Vec<(String, f64)> = q.iter().filter_map(|t| w.iter().find(|x| &x.0 == t).cloned()
            .or_else(|| w.iter().find(|x| x.1 >= 0.99 && close(t, &x.0)).map(|x| (t.clone(), x.1 * 0.8)))).collect();
        let bridge = bridges.get(&index::norm(d.name.rsplit('.').next().unwrap_or(&d.name))).filter(|_| kind_w(&d.kind) >= 1.0);
        if hit.is_empty() && bridge.is_none() { return None; }
        let in_name = hit.iter().filter(|x| x.1 >= 1.0).count();
        let in_doc = hit.iter().filter(|x| (x.1 - 0.6).abs() < 1e-9).count();
        // a name word, or two words of its doc comment, or a doc line that names it
        if in_name == 0 && in_doc < 2 && bridge.is_none() { return None; }
        let nn0 = index::norm(d.name.rsplit('.').next().unwrap_or(&d.name));
        let exact0 = nn0.len() >= 4 && (q.iter().any(|t| nn0 == *t || nn0 == format!("{t}s")) || joined.iter().any(|j| nn0 == *j));
        // variables, constants, config keys and CSS rules only when the prompt names them exactly
        if kind_w(&d.kind) < 1.0 && !(exact0 && nn0.len() >= 6) { return None; }
        // one common word is not a match: a code card needs two prompt words or the exact name
        if hit.len() < 2 && bridge.is_none() && !(nn0.len() >= 4 && q.iter().any(|t| nn0 == *t || nn0 == format!("{t}s"))) { return None; }
        // doc-comment matches count at most twice: a long comment must not outscore a name
        let mut doc_seen = 0;
        let idf: f64 = hit.iter().filter(|(_, wt)| { if (wt - 0.6).abs() < 1e-9 { doc_seen += 1; doc_seen <= 2 } else { true } })
            .map(|(t, wt)| wt * (n / df.get(t).copied().unwrap_or(1.0)).ln().max(0.1)).sum();
        // the whole name is a prompt word or two adjacent prompt words: `session`, `audit_log`
        let nn = nn0.clone();
        // a single prompt word is an exact name only when written like one (Sessions, WriteAudit,
        // audit_log) or when it names a table; "applies" is a verb, not `apply`
        let exact = joined.iter().any(|j| nn == *j) || (nn.len() >= 4 && (d.kind == "table" || cased.contains(&nn) || cased.contains(&format!("{nn}s")) || cased.iter().any(|c| format!("{c}s") == nn)) && q.iter().any(|t| nn == *t || nn == format!("{t}s")));
        let full = w.iter().filter(|x| x.1 >= 1.0).all(|x| hit.iter().any(|h| h.0 == x.0));
        // name and file words count fully toward the multi-word bonus, doc-comment words a third
        let weight_hits: f64 = hit.iter().map(|x| if (x.1 - 0.6).abs() < 1e-9 { 0.33 } else { 1.0 }).sum();
        // an exact name is worth more when it is rare here: `audit_log`, not `session`
        let rarity = (n / df.get(&stem(&nn)).copied().unwrap_or(1.0).max(1.0)).ln() / (n / 3.0).ln().max(1.0);
        let exact_bonus = 1.0 + 1.5 * rarity.clamp(0.0, 1.0);
        let mut s = idf * (1.0 + 0.6 * (weight_hits - 1.0).max(0.0)) * role_w(&d.role) * kind_w(&d.kind) * if exact { exact_bonus } else { 1.0 } * if full { 1.3 } else { 1.0 };
        let last = d.name.rsplit('.').next().unwrap_or(&d.name);
        if wants_handler && handler_names.contains(last) { s *= 1.6; }
        if wants_ui && client_files.contains(d.path.as_str()) { s *= 1.4; }
        let mut why: Vec<String> = hit.into_iter().map(|x| x.0).collect();
        if in_name >= 2 || exact { why.push("name match".into()); }
        if let Some((k, ev)) = bridge { s += 1.5 * (*k as f64).min(4.0); why.push(format!("named by {ev}")); }
        // the sub-project the session is in (monorepos): its definitions first
        if let Some(f) = &o.focus { if !f.as_os_str().is_empty() && Path::new(&d.path).starts_with(f) { s *= 1.25; } }
        Some((s, i, why))
    }).collect();
    // a code question names code: two prompt words in some definition's name, an exact name, or an
    // identifier written like code. A prose request (rewrite this sentence, deploy a preview) gets no
    // code cards, however its words happen to match doc comments or design docs.
    // A prompt that asks for a skill, agent or MCP server by name ("use the humanizer skill") is a
    // tool request; unless some definition matches it by name, it gets no code cards.
    let ql = query.to_lowercase();
    let names_tool = g.nodes.values().any(|n| matches!(n.kind.as_str(), "skill" | "agent" | "mcp") && {
        let short = n.name.rsplit(':').next().unwrap_or(&n.name).to_lowercase();
        (short.contains('-') || short.len() >= 8) && ql.contains(&short)
    });
    let code_like = query.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w.len() >= 5 && (w.contains('_') || w.chars().skip(1).any(|c| c.is_uppercase())));
    if names_tool && !code_like && !scored.iter().any(|x| x.2.iter().any(|w| w == "name match")) { return vec![]; }
    // embeddings: every definition's text vector (name, file, directory, doc comment), optionally
    // propagated over the code graph's typed edges, scored by cosine against the prompt. They add
    // to the word score and bring in definitions no prompt word names. MT_EMBED=off|text|graph
    // (default text), MT_EMBED_W (default 3 for BERT, 1.5 for model2vec), MT_EMBED_SELF (default 0.5).
    // defaults from the independent gold set (section 30's follow-up): text vectors beat graph-
    // propagated ones on held-out prompts, at weight 3 for BERT and 1.5 for model2vec
    let mode = std::env::var("MT_EMBED").unwrap_or_else(|_| "text".into());
    let ew_env: Option<f64> = std::env::var("MT_EMBED_W").ok().and_then(|x| x.parse().ok());
    if mode != "off" {
        if let Some(m) = model() {
            let ew = ew_env.unwrap_or(if m.is_cheap() { 1.5 } else { 3.0 });
            let te = Instant::now();
            let def_text = |d: &D| -> String {
                let mut t = split_words(&d.name).join(" ");
                let stem_of = Path::new(&d.path).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
                let dir_of = Path::new(&d.path).parent().and_then(|p| p.file_name()).map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
                t += &format!(" {} {} {}", split_words(&stem_of).join(" "), split_words(&dir_of).join(" "), d.kind);
                if let Some(lines) = text.get(&d.path) {
                    let (a, b) = (d.start.max(0) as usize, (d.head.max(d.start) as usize).min(lines.len()));
                    for l in lines.get(a..b).unwrap_or(&[]).iter().take(4) { t.push(' '); t.push_str(l.trim_start_matches(|c: char| c == '/' || c == '*' || c == '#' || c.is_whitespace())); }
                }
                t
            };
            let skip = |d: &D| d.path.ends_with(".md") || d.path.ends_with(".html") || d.role == "doc";
            let texts: Vec<String> = defs.iter().map(|d| if skip(d) { String::new() } else { def_text(d) }).collect();
            let fill = std::env::var_os("MT_EMBED_FILL").is_some();
            let got = cached_vectors(m, &texts, fill);
            // a slow model without a full cache: fall back to the static model for this prompt
            let (m, got) = match got {
                Some(v) => (m, v),
                None => match static_model() { Some(sm) => (sm, sm.embed_docs(&texts)), None => (m, vec![]) },
            };
            // no model answered: zero vectors, so embeddings add nothing and the words decide
            let got = if got.is_empty() { vec![Vec::new(); texts.len()] } else { got };
            let mut vecs: Vec<Vec<f32>> = got.into_iter().map(|v| if v.is_empty() { vec![0.0; m.dim()] } else { v }).collect();
            if mode == "graph" {
                // typed edges over the whole repo: calls to distinctive names, routes, tables
                let mut edges: Vec<(usize, usize, f32)> = vec![];
                let mut by_last: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, d) in defs.iter().enumerate() { by_last.entry(d.name.rsplit('.').next().unwrap_or(&d.name).to_string()).or_default().push(i); }
                let refs_all: Vec<(String, String, String)> = db.prepare("SELECT f.path, r.name, r.lines FROM refs r JOIN files f ON f.id = r.file").ok()
                    .map(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default();
                for (path, name, lines) in &refs_all {
                    if name.len() < 5 || generic(&name.to_lowercase()) { continue; }
                    let Some(targets) = by_last.get(name) else { continue };
                    if targets.len() > 3 { continue; }
                    for l in lines.split(',').filter_map(|x| x.trim().parse::<i64>().ok()).take(5) {
                        if let Some(src) = enclosing_def(path, l - 1) { for &t in targets { if defs[t].path != *path { edges.push((src, t, 1.0)); } } }
                    }
                }
                for (shape, h, _, _) in &rmap.handlers {
                    let hs: Vec<usize> = by_last.get(h).cloned().unwrap_or_default();
                    for c in rmap.calls.iter().filter(|c| &c.0 == shape) { if let Some(src) = enclosing_def(&c.1, c.2 as i64) { for &t in &hs { edges.push((src, t, 1.5)); } } }
                }
                let table_def: HashMap<String, usize> = defs.iter().enumerate().filter(|(_, d)| d.kind == "table").map(|(i, d)| (d.name.to_lowercase(), i)).collect();
                if let Ok(mut st) = db.prepare("SELECT f.path, lower(t.name), t.line FROM tables t JOIN files f ON f.id = t.file WHERE t.kind = 'use'") {
                    for (path, t, line) in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))).into_iter().flatten().flatten() {
                        if let (Some(&td), Some(src)) = (table_def.get(&t), enclosing_def(&path, line)) { edges.push((src, td, 1.0)); }
                    }
                }
                let sw: f32 = std::env::var("MT_EMBED_SELF").ok().and_then(|x| x.parse().ok()).unwrap_or(0.5);
                vecs = crate::embed::propagate(&vecs, &edges, sw, 2);
                if std::env::var_os("MT_DEBUG").is_some() { eprintln!("embed: {} defs, {} edges", defs.len(), edges.len()); }
            }
            let qv = m.embed_query(query);
            let cos: Vec<f64> = vecs.iter().map(|v| crate::embed::dot(&qv, v) as f64).collect();
            let lmax = scored.iter().map(|x| x.0).fold(1e-9, f64::max);
            // cosine above the repo's typical similarity adds, scaled to the word scores
            let mut sorted_c: Vec<f64> = cos.clone(); sorted_c.sort_by(|a, b| b.total_cmp(a));
            let base = sorted_c.get(sorted_c.len() / 2).copied().unwrap_or(0.0);
            let top_c = sorted_c.first().copied().unwrap_or(1.0).max(base + 1e-6);
            // the same role and kind weights the word score uses: a test whose name reads like the
            // prompt must not outrank the code it tests
            let bump = |i: usize| -> f64 { ew * lmax * ((cos[i] - base) / (top_c - base)).max(0.0).powi(2) * role_w(&defs[i].role) * kind_w(&defs[i].kind) };
            let mut present: HashSet<usize> = HashSet::new();
            for x in scored.iter_mut() { x.0 += bump(x.1); present.insert(x.1); x.2.push(format!("cos {:.2}", cos[x.1])); }
            let mut extra: Vec<(f64, usize)> = cos.iter().enumerate().filter(|(i, _)| !present.contains(i) && !skip(&defs[*i]) && kind_w(&defs[*i].kind) >= 1.0).map(|(i, c)| (*c, i)).collect();
            extra.sort_by(|a, b| b.0.total_cmp(&a.0));
            // embeddings re-rank and extend a code question; they never create one: with no definition
            // matched by the prompt's words (a prose rewrite, a deploy request), no vector-only cards
            if !present.is_empty() {
                for (c, i) in extra.into_iter().take(15) { scored.push((bump(i), i, vec![format!("cos {c:.2} (embedding only)")])); }
            }
            if std::env::var_os("MT_DEBUG").is_some() { eprintln!("embed: {mode} in {} ms", te.elapsed().as_millis()); }
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let debug = std::env::var_os("MT_DEBUG").is_some();
    if debug { for x in scored.iter().take(15) { eprintln!("seed {:7.3} {} {} [{}]", x.0, defs[x.1].kind, defs[x.1].name, x.2.join(",")); } }
    let mut seen = HashSet::new();
    let mut out = vec![];
    // product anchors the tool graph knows, so a file's identifiers can point to skills/servers
    let mut anchor_items: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in g.nodes.values().filter(|x| x.kind == "plugin" || x.kind == "mcp") {
        let a = if item.kind == "plugin" { g.product.get(&item.name).cloned().into_iter().collect() } else { g.anchors(item) };
        for a in a { anchor_items.entry(a).or_default().push(format!("{} {}", item.kind, item.name)); }
    }
    let rows = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Vec<(String, String, i64)> {
        db.prepare(sql).ok().map(|mut s| s.query_map(p, |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2).unwrap_or(0)))).map(|it| it.flatten().collect()).unwrap_or_default()).unwrap_or_default()
    };
    // graph rerank: the top lexical hits are seeds; connect them through the code (one seed's body
    // references another, both touch a table, a test or migration file references both) and let
    // personalized PageRank decide. Connected seeds lift each other; one-word coincidences fade.
    scored.truncate(40);
    let mut keys: Vec<String> = scored.iter().map(|x| format!("d:{}", x.1)).collect();
    let mut at: HashMap<String, usize> = keys.iter().enumerate().map(|(i, k)| (k.clone(), i)).collect();
    let mut edges: Vec<(usize, usize, f64)> = vec![];
    let mut node = |k: String, keys: &mut Vec<String>| -> usize { if let Some(i) = at.get(&k) { return *i; } keys.push(k.clone()); at.insert(k, keys.len() - 1); keys.len() - 1 };
    let by_name: HashMap<String, Vec<usize>> = scored.iter().enumerate().fold(HashMap::new(), |mut m, (si, x)| { m.entry(defs[x.1].name.rsplit('.').next().unwrap_or(&defs[x.1].name).to_string()).or_default().push(si); m });
    for (si, (_, i, _)) in scored.iter().enumerate() {
        let d = &defs[*i];
        for (name, lines, _) in rows("SELECT n.name, r.lines, 0 FROM refs_ids r JOIN names n ON n.id = r.name_id WHERE r.rowid BETWEEN (SELECT refs_lo FROM files WHERE id = ?1) AND (SELECT refs_hi FROM files WHERE id = ?1)", &[&d.file]) {
            let Some(targets) = by_name.get(&name) else { continue };
            let inside = lines.split(',').filter_map(|l| l.trim().parse::<i64>().ok()).any(|l| l - 1 >= d.start && l - 1 <= d.end);
            // a body that calls another seed links them; two seeds that merely share a name don't
            let own = d.name.rsplit('.').next().unwrap_or(&d.name);
            if inside && name != own { for &sj in targets { if sj != si { edges.push((si, sj, 1.0)); } } }
        }
        for (t, _, _) in rows("SELECT lower(name), kind, line FROM tables WHERE file = ?1 AND line BETWEEN ?2 AND ?3", &[&d.file, &d.start, &d.end]) {
            if STOP.contains(&t.as_str()) { continue; }
            let tn = node(format!("t:{t}"), &mut keys);
            edges.push((si, tn, 1.0));
        }
        if d.kind == "table" { let tn = node(format!("t:{}", d.name.to_lowercase()), &mut keys); edges.push((si, tn, 2.0)); }
        let short = d.name.rsplit('.').next().unwrap_or(&d.name).to_string();
        for (f, _, _) in rows("SELECT f.path, f.role, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role IN ('test','integration','migration') AND f.path NOT LIKE '%.md' AND r.file != ?2 GROUP BY f.path LIMIT 12", &[&short, &d.file]) {
            let fnode = node(format!("f:{f}"), &mut keys);
            edges.push((si, fnode, 0.5));
        }
        let fnode = node(format!("f:{}", d.path), &mut keys);
        edges.push((si, fnode, 0.3));
    }
    // files usually committed together are linked (git history of this repo)
    let co = cochange(root);
    let seed_files: Vec<String> = scored.iter().map(|x| defs[x.1].path.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    for a in &seed_files { for b in &seed_files {
        if a < b { if let Some(n) = co.get(&(a.clone(), b.clone())) {
            let (fa, fb) = (node(format!("f:{a}"), &mut keys), node(format!("f:{b}"), &mut keys));
            edges.push((fa, fb, 0.3 * (*n as f64).min(5.0)));
        } }
    } }
    // wire keys shared by seeds
    for (si, x) in scored.iter().enumerate() {
        for k in keys_of.get(&x.1).into_iter().flatten() { let kn = node(format!("w:{k}"), &mut keys); edges.push((si, kn, 0.7)); }
    }
    // HTTP routes: the code that calls a route (ajax in web/, another service, a test) is linked to
    // the handler that serves it, across sub-projects
    let seed_of: HashMap<usize, usize> = scored.iter().enumerate().map(|(si, x)| (x.1, si)).collect();
    for (shape, h, _, _) in &rmap.handlers {
        let hseeds: Vec<usize> = scored.iter().enumerate().filter(|(_, x)| defs[x.1].name.rsplit('.').next() == Some(h.as_str())).map(|(si, _)| si).collect();
        let callers: Vec<usize> = rmap.calls.iter().filter(|c| &c.0 == shape).filter_map(|c| enclosing_def(&c.1, c.2 as i64)).filter_map(|i| seed_of.get(&i).copied()).collect();
        if hseeds.is_empty() && callers.is_empty() { continue; }
        let rn = node(format!("r:{shape}"), &mut keys);
        for &si in hseeds.iter().chain(callers.iter()) { edges.push((si, rn, 1.0)); }
    }
    let mut p = vec![0.0; keys.len()];
    for (si, x) in scored.iter().enumerate() { p[si] = x.0; }
    let r = ppr(keys.len(), &edges, &p, 0.25, 30);
    // half lexical, half graph, each scaled to its own maximum: the graph promotes what is
    // connected without letting a hub (a table everything touches) outrank the named target
    let lmax = scored.iter().map(|x| x.0).fold(1e-12, f64::max);
    let rmax = (0..scored.len()).map(|i| r[i]).fold(1e-12, f64::max);
    let mut ranked: Vec<(f64, usize, Vec<String>)> = scored.iter().enumerate().map(|(si, x)| (0.5 * x.0 / lmax + 0.5 * r[si] / rmax, x.1, x.2.clone())).collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    let top = ranked.first().map(|x| x.0).unwrap_or(0.0);
    let mut chosen: Vec<usize> = vec![];
    let mut used = 0usize;
    // every line in the repo that writes or reads a table, by regex over all indexed files: complete,
    // so a card can say so
    let table_lines = |t: &str| -> (Vec<(String, usize)>, Vec<(String, usize)>, usize) {
        let esc = regex::escape(t);
        let w = regex::Regex::new(&format!(r#"(?i)\b(insert\s+into|update|delete\s+from|merge\s+into)\s+["`\[]?(\w+["`\]]?\.)?["`\[]?{esc}\b"#)).unwrap();
        let rd = regex::Regex::new(&format!(r#"(?i)\b(from|join)\s+["`\[]?(\w+["`\]]?\.)?["`\[]?{esc}\b"#)).unwrap();
        let (mut ws, mut rs, mut n) = (vec![], vec![], 0usize);
        for (p, lines) in &text {
            if matches!(role_of.get(p.as_str()), Some(&"doc") | Some(&"migration") | Some(&"test") | Some(&"integration")) { continue; }
            n += 1;
            for (i, l) in lines.iter().enumerate() {
                if w.is_match(l) { ws.push((p.clone(), i)); } else if rd.is_match(l) { rs.push((p.clone(), i)); }
            }
        }
        ws.sort(); rs.sort();
        (ws, rs, n)
    };
    let by_def = |hits: &[(String, usize)]| -> String {
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (p, i) in hits {
            let name = enclosing_def(p, *i as i64).map(|d| defs[d].name.clone()).unwrap_or_else(|| "(top level)".into());
            groups.entry(format!("{name} {p}")).or_default().push(i + 1);
        }
        groups.iter().map(|(k, v)| format!("{k}:{}", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))).collect::<Vec<_>>().join("; ")
    };
    let route_lines = |di: usize| -> String {
        let d = &defs[di];
        let last = d.name.rsplit('.').next().unwrap_or(&d.name);
        let mut out = String::new();
        for (shape, h, line0, rp) in rmap.handlers.iter().filter(|h| h.1 == last) {
            let callers: Vec<String> = rmap.calls.iter().filter(|c| &c.0 == shape).map(|c| format!("{}:{}{}", c.1, c.2 + 1, enclosing_def(&c.1, c.2 as i64).map(|e| format!(" ({})", defs[e].name)).unwrap_or_default())).take(6).collect();
            out += &format!("\n  serves route {shape} (registered {rp}:{}) — called from: {}", line0 + 1, if callers.is_empty() { "nothing else in this repo".into() } else { callers.join(", ") });
            let _ = h;
        }
        for c in rmap.calls.iter().filter(|c| c.1 == d.path && (c.2 as i64) >= d.start && (c.2 as i64) <= d.end) {
            if let Some(h) = rmap.handlers.iter().find(|h| h.0 == c.0) {
                let at = defs.iter().find(|x| x.name.rsplit('.').next() == Some(h.1.as_str()) && x.path != d.path).map(|x| format!(" {}:{}-{}", x.path, x.start + 1, x.end + 1)).unwrap_or_default();
                out += &format!("\n  calls route {} (line {}) → served by {}{at}", c.0, c.2 + 1, h.1);
            }
        }
        out
    };
    // identifiers the prompt spells out (`getInvoice`, `WriteAudit`) are always shown; a change
    // touches several places, so it keeps more of the ranking than a question does
    let looks_like_code = |w: &str| w.contains('_') || w.chars().skip(1).any(|c| c.is_uppercase());
    let spelled: HashSet<String> = query.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.len() >= 5 && looks_like_code(w)).map(index::norm).collect();
    let cutoff = if task == "change" { 0.25 } else { 0.5 };
    let mut ranked = ranked;
    ranked.sort_by(|a, b| {
        let sa = spelled.contains(&index::norm(defs[a.1].name.rsplit('.').next().unwrap_or(""))) as u8;
        let sb = spelled.contains(&index::norm(defs[b.1].name.rsplit('.').next().unwrap_or(""))) as u8;
        sb.cmp(&sa).then(b.0.total_cmp(&a.0))
    });
    if debug { for x in ranked.iter().take(15) { eprintln!("rank {:7.3} {} {}", x.0, defs[x.1].kind, defs[x.1].name); } }
    // a chosen card that calls a route brings its handler, and a chosen handler brings its callers:
    // the chain from ajax in web/ to the API to internal/ is followed, whatever the scores
    let mut linked: HashSet<usize> = HashSet::new();
    let link_from = |di: usize, linked: &mut HashSet<usize>| {
        let d = &defs[di];
        let last = d.name.rsplit('.').next().unwrap_or(&d.name);
        for c in rmap.calls.iter().filter(|c| c.1 == d.path && (c.2 as i64) >= d.start && (c.2 as i64) <= d.end) {
            for h in rmap.handlers.iter().filter(|h| h.0 == c.0) {
                for (j, x) in defs.iter().enumerate() { if x.name.rsplit('.').next() == Some(h.1.as_str()) && x.path != d.path { linked.insert(j); } }
            }
        }
        for h in rmap.handlers.iter().filter(|h| h.1 == last) {
            for c in rmap.calls.iter().filter(|c| c.0 == h.0) { if let Some(e) = enclosing_def(&c.1, c.2 as i64) { linked.insert(e); } }
        }
    };
    // a handler's own calls into other packages (the API calling internal/): one more hop
    let last_of = |n: &str| -> String { n.rsplit('.').next().unwrap_or(n).to_string() };
    let callees_of = |di: usize| -> Vec<usize> {
        let d = &defs[di];
        let Some(lines) = text.get(&d.path) else { return vec![] };
        let body = lines.get(d.start.max(0) as usize..=(d.end.max(d.start) as usize).min(lines.len().saturating_sub(1))).map(|x| x.join("\n")).unwrap_or_default();
        let mut out = vec![];
        for m in regex::Regex::new(r"([A-Za-z_][A-Za-z0-9_]{4,})\s*\(").unwrap().captures_iter(&body) {
            let name = &m[1];
            if name == last_of(&d.name) || generic(&name.to_lowercase()) { continue; }
            for (j, x) in defs.iter().enumerate() {
                if x.name.rsplit('.').next() == Some(name) && x.path != d.path && !out.contains(&j) && matches!(x.role.as_str(), "source" | "query" | "model") { out.push(j); }
            }
            if out.len() >= 3 { break; }
        }
        out
    };
    // selection first, rendering after: a skipped item that a later choice links to comes back
    let mut sel: Vec<(f64, usize, Vec<String>)> = vec![];
    let mut pending: Vec<(f64, usize, Vec<String>)> = vec![];
    for (score, i, hit) in &ranked {
        if sel.len() >= limit + 2 { break; }
        let named = spelled.contains(&index::norm(defs[*i].name.rsplit('.').next().unwrap_or("")));
        if !named && !linked.contains(i) && *score < top * cutoff { pending.push((*score, *i, hit.clone())); continue; }
        link_from(*i, &mut linked);
        {
            let mine = sub_of(&defs[*i].path);
            let mut added = 0;
            for k in keys_of.get(i).into_iter().flatten() {
                for &j in wire_defs.get(k).into_iter().flatten() {
                    if added >= 2 { break; }
                    if j != *i && sub_of(&defs[j].path) != mine && kind_w(&defs[j].kind) >= 1.0 && linked.insert(j) { added += 1; }
                }
            }
        }
        if handler_names.contains(defs[*i].name.rsplit('.').next().unwrap_or("")) { for c in callees_of(*i) { linked.insert(c); } }
        sel.push((*score, *i, hit.clone()));
    }
    let handlers_linked: Vec<usize> = linked.iter().copied().filter(|j| handler_names.contains(defs[*j].name.rsplit('.').next().unwrap_or(""))).collect();
    for h in handlers_linked { for c in callees_of(h) { linked.insert(c); } }
    for x in pending { if linked.contains(&x.1) && !sel.iter().any(|y| y.1 == x.1) { let mut x = x; x.2.push("linked by route".into()); sel.push(x); } }
    // a definition linked by route but not among the seeds at all (the handler of the top card)
    for &j in linked.iter() { if !sel.iter().any(|y| y.1 == j) && sel.len() < limit + 2 { sel.push((0.0, j, vec!["linked by route".into()])); } }
    for (_score, i, hit) in &sel {
        if out.len() >= limit + if sel.iter().any(|x| x.2.iter().any(|w| w == "linked by route")) { 1 } else { 0 } { break; }
        let d = &defs[*i];
        if d.role == "doc" || !seen.insert(format!("{}:{}", d.kind, index::norm(&d.name))) { continue; }
        let mut c = format!("code {} {} — {}:{}-{} ({})", d.kind, d.name, d.path, d.start + 1, d.end + 1, d.role);
        let lines_of = |l: &str| l.split(',').next().unwrap_or("").to_string();
        if d.kind == "table" {
            // a table's neighbours: the code that reads or writes it (named by enclosing definition),
            // those functions' tests, the other migrations that touch it, and the tables it references
            let users = rows("SELECT f.path, coalesce((SELECT dd.name FROM defs dd WHERE dd.file = t.file AND t.line BETWEEN dd.start AND dd.end ORDER BY dd.end - dd.start LIMIT 1), ''), t.line FROM tables t JOIN files f ON f.id = t.file WHERE t.name IN (?1, lower(?1), upper(?1)) AND t.kind = 'use' AND f.role NOT IN ('migration','doc','test','integration') GROUP BY f.path, t.line LIMIT 12", &[&d.name]);
            let (ws, rs, nfiles) = table_lines(&d.name);
            c += &format!("\n  writes, all of them (regex over {nfiles} code files ✓ exhaustive): {}", if ws.is_empty() { "none".into() } else { by_def(&ws) });
            if !rs.is_empty() { c += &format!("\n  reads, all of them ✓: {}", by_def(&rs)); }
            if out.is_empty() {
                if let Some(l) = inline_lines(root, &d.path, d.start, d.end, 40) { c += &l; }
                // the function that writes the table (insert/write/create/update first), with its body
                let pick = users.iter().find(|u| { let n = u.1.to_lowercase(); ["write", "insert", "create", "upsert", "update", "save", "record"].iter().any(|v| n.contains(v)) }).or(users.first());
                if let Some(u) = pick {
                    let short = u.1.rsplit('.').next().unwrap_or(&u.1).to_string();
                    if let Some(sp) = rows("SELECT f.path, '', d.start FROM defs d JOIN files f ON f.id = d.file WHERE f.path = ?1 AND (d.name = ?2 OR d.last = ?3) AND ?4 BETWEEN d.start AND d.end LIMIT 1", &[&u.0, &u.1, &short, &u.2]).first() {
                        let end: i64 = db.query_row("SELECT d.end FROM defs d JOIN files f ON f.id = d.file WHERE f.path = ?1 AND d.start = ?2 LIMIT 1", params![sp.0, sp.2], |r| r.get(0)).unwrap_or(sp.2 + 40);
                        if let Some(l) = inline_lines(root, &u.0, sp.2, end, 50) { c += &l; }
                    }
                }
            }
            let fns: BTreeSet<String> = users.iter().map(|u| u.1.rsplit('.').next().unwrap_or(&u.1).to_string()).filter(|x| !x.is_empty()).collect();
            let mut tests = vec![];
            for f in fns.iter().take(6) {
                for t in rows("SELECT f.path, r.lines, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role IN ('test','integration') AND f.path NOT LIKE '%.md' GROUP BY f.path LIMIT 3", &[f]) {
                    let e = format!("{}:{} ({f})", t.0, lines_of(&t.1));
                    if !tests.contains(&e) { tests.push(e); }
                }
            }
            if !tests.is_empty() { c += &format!("\n  tests of that code: {}", tests.join(", ")); }
            let migs = rows("SELECT f.path, '', t.line FROM tables t JOIN files f ON f.id = t.file WHERE t.name IN (?1, lower(?1), upper(?1)) AND f.role = 'migration' AND t.file != ?2 GROUP BY f.path LIMIT 6", &[&d.name, &d.file]);
            if !migs.is_empty() { c += &format!("\n  other migrations: {}", migs.iter().map(|m| format!("{}:{}", m.0, m.2 + 1)).collect::<Vec<_>>().join(", ")); }
            let refs_to: Vec<String> = rows("SELECT name, kind, line FROM tables WHERE file = ?1 AND line BETWEEN ?2 AND ?3 AND kind = 'use' AND lower(name) != lower(?4) GROUP BY name", &[&d.file, &d.start, &d.end, &d.name]).into_iter().map(|t| t.0).filter(|t| !STOP.contains(&t.to_lowercase().as_str())).collect();
            if !refs_to.is_empty() { c += &format!("\n  references tables: {}", refs_to.join(", ")); }
            let docs = rows("SELECT f.path, t.line, 0 FROM tables t JOIN files f ON f.id = t.file WHERE t.name IN (?1, lower(?1), upper(?1)) AND f.role = 'doc' GROUP BY f.path LIMIT 3", &[&d.name]);
            if !docs.is_empty() { c += &format!("\n  docs: {}", docs.iter().map(|t| format!("{}:{}", t.0, t.1.parse::<i64>().unwrap_or(0) + 1)).collect::<Vec<_>>().join(", ")); }
            c += &format!("\n  bodies of all of the above: mt context {}\n  matched: {}", d.name, hit.join(", "));
            if !out.is_empty() && used + c.len() > o.budget { break; }
            used += c.len();
            chosen.push(*i);
            out.push(c);
            continue;
        }
        let first = out.is_empty();
        // a short or common name (`state`, `live`) appears everywhere: refs can't link it to anything
        let last_name = d.name.rsplit('.').next().unwrap_or(&d.name).to_string();
        let common = last_name.len() < 5 || generic(&last_name.to_lowercase()) || STOP.contains(&last_name.to_lowercase().as_str());
        let tests = if common { vec![] } else { rows("SELECT f.path, r.lines, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role IN ('test','integration') AND f.path NOT LIKE '%.md' AND r.file != ?2 GROUP BY f.path LIMIT 6", &[&d.name, &d.file]) };
        if common { c += &format!("\n  tests and callers: the name `{last_name}` is too common to link by references; use mt context {}", d.name); }
        else { c += &format!("\n  tests, all that reference it (refs index ✓ exhaustive): {}", if tests.is_empty() { "none".into() } else { tests.iter().map(|t| format!("{}:{}", t.0, lines_of(&t.1))).collect::<Vec<_>>().join(", ") }); }
        c += &route_lines(*i);
        {
            let mine = sub_of(&d.path);
            let mut shared: Vec<String> = vec![];
            for k in keys_of.get(i).into_iter().flatten().take(6) {
                let others: Vec<String> = wire_defs.get(k).into_iter().flatten().filter(|&&j| j != *i && sub_of(&defs[j].path) != mine).take(2).map(|&j| format!("{} {}:{}", defs[j].name, defs[j].path, defs[j].start + 1)).collect();
                if !others.is_empty() { shared.push(format!("`{k}` also in {}", others.join(", "))); }
            }
            if !shared.is_empty() { c += &format!("\n  wire (shared with other components): {}", shared.iter().take(3).cloned().collect::<Vec<_>>().join("; ")); }
        }
        let callers = if common { vec![] } else { rows("SELECT f.path, r.lines, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role NOT IN ('test','integration','doc') AND f.path NOT LIKE '%.html' AND r.file != ?2 GROUP BY f.path LIMIT 40", &[&d.name, &d.file]) };
        if !callers.is_empty() { c += &format!("\n  used in {} files (✓ exhaustive): {}{}", callers.len(), callers.iter().take(5).map(|t| format!("{}:{}", t.0, lines_of(&t.1))).collect::<Vec<_>>().join(", "), if callers.len() > 5 { ", …" } else { "" }); }
        // data: tables the definition touches (or defines), and where each table is defined
        let tabs: Vec<(String, String, i64)> = rows("SELECT name, kind, line FROM tables WHERE file = ?1 AND line BETWEEN ?2 AND ?3 GROUP BY name, kind LIMIT 8", &[&d.file, &d.start, &d.end])
            .into_iter().filter(|t| !STOP.contains(&t.0.to_lowercase().as_str()) && t.0.len() > 2).collect();
        let mut data = vec![];
        for (t, kind, _) in &tabs {
            let defs_at = rows("SELECT f.path, f.role, t.line FROM tables t JOIN files f ON f.id = t.file WHERE t.name IN (?1, lower(?1), upper(?1)) AND (t.kind = 'define' OR f.role = 'migration') AND f.role != 'doc' AND t.file != ?2 GROUP BY f.path LIMIT 4", &[t, &d.file]);
            let at = defs_at.iter().map(|x| format!("{}:{} ({})", x.0, x.2 + 1, x.1)).collect::<Vec<_>>().join(", ");
            data.push(format!("{t} [{kind}]{}", if at.is_empty() { String::new() } else { format!(" ← {at}") }));
        }
        if !data.is_empty() { c += &format!("\n  data: {}", data.join("; ")); }
        let docs = rows("SELECT f.path, r.lines, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role = 'doc' GROUP BY f.path LIMIT 4", &[&d.name]);
        if !docs.is_empty() { c += &format!("\n  docs: {}", docs.iter().map(|t| format!("{}:{}", t.0, lines_of(&t.1))).collect::<Vec<_>>().join(", ")); }
        // technology this file uses that a skill or MCP server covers
        let fw: BTreeSet<String> = rows("SELECT n.name, '', 0 FROM refs_ids r JOIN names n ON n.id = r.name_id WHERE r.rowid BETWEEN (SELECT refs_lo FROM files WHERE id = ?1) AND (SELECT refs_hi FROM files WHERE id = ?1)", &[&d.file]).into_iter().flat_map(|r| split_words(&r.0)).collect();
        let tech: Vec<String> = anchor_items.iter().filter(|(a, _)| fw.contains(*a)).map(|(a, items)| format!("{a} → {}", items.iter().take(2).cloned().collect::<Vec<_>>().join(", "))).take(3).collect();
        if !tech.is_empty() { c += &format!("\n  tech: {}", tech.join("; ")); }
        if first && task == "debug" {
            if let Ok(o2) = Command::new("git").args(["log", "-n", "3", "--format=%h %ar %s", "--", &d.path]).current_dir(root).output() {
                let log = String::from_utf8_lossy(&o2.stdout).trim().to_string();
                if !log.is_empty() { c += &format!("\n  recent commits on {}:\n    {}", d.path, log.replace('\n', "\n    ")); }
            }
        }
        if first { if let Some(l) = inline_lines(root, &d.path, d.start, d.end, 60) { c += &l; } }
        c += &format!("\n  bodies of all of the above: mt context {}\n  matched: {}", d.name, hit.join(", "));
        if !out.is_empty() && used + c.len() > o.budget { break; }
        used += c.len();
        chosen.push(*i);
        out.push(c);
    }
    // a cross-layer prompt (screen and API, API and store, …) gets the whole path as one card: the
    // client's call site, the route registration, the handler, and the calls it makes into other
    // packages, every hop with file:line. Built from the best route-linked definition among the top
    // ranked, whether or not it won a card of its own. MT_CHAIN=off turns it off.
    let layers = [["screen", "page", "panel", "button", "ui", "view", "frontend", "web", "dashboard", "form"].as_slice(),
                  ["api", "endpoint", "endpoints", "route", "handler", "request", "server"].as_slice(),
                  ["store", "database", "db", "query", "table", "sql", "storage", "persist"].as_slice()];
    let layer_count = layers.iter().filter(|l| l.iter().any(|w| qwords.contains(*w))).count();
    if layer_count >= 2 && std::env::var("MT_CHAIN").as_deref() != Ok("off") {
        let on_route = |i: usize| -> Option<String> {
            let d = &defs[i];
            let last = d.name.rsplit('.').next().unwrap_or(&d.name);
            if let Some(h) = rmap.handlers.iter().find(|h| h.1 == last) { return Some(h.0.clone()); }
            rmap.calls.iter().find(|c| c.1 == d.path && (c.2 as i64) >= d.start && (c.2 as i64) <= d.end).map(|c| c.0.clone())
        };
        if let Some((start, shape)) = ranked.iter().take(10).find_map(|x| on_route(x.1).map(|r| (x.1, r))) {
            let mut lines: Vec<String> = vec![];
            for c in rmap.calls.iter().filter(|c| c.0 == shape && !matches!(role_of.get(c.1.as_str()), Some(&"test") | Some(&"integration") | Some(&"config") | Some(&"doc"))).take(3) {
                let who = enclosing_def(&c.1, c.2 as i64).map(|e| defs[e].name.clone()).unwrap_or_default();
                lines.push(format!("{}:{}  {who}  calls {shape}", c.1, c.2 + 1));
            }
            for (_, h, line0, rp) in rmap.handlers.iter().filter(|h| h.0 == shape).take(2) {
                let Some(hd) = defs.iter().position(|x| x.name.rsplit('.').next() == Some(h.as_str()) && matches!(x.role.as_str(), "source" | "query" | "model")) else { continue };
                let hdef = &defs[hd];
                lines.push(format!("{rp}:{}  registers {shape} → {} {}:{}-{}", line0 + 1, hdef.name, hdef.path, hdef.start + 1, hdef.end + 1));
                // every call in the handler's body to a definition in another file, calls into other
                // directories (the store, internal packages) first, then same-package helpers
                let chain_callees: Vec<usize> = {
                    let mut v: Vec<(bool, usize, usize)> = vec![];
                    if let Some(ls) = text.get(&hdef.path) {
                        let body = ls.get(hdef.start.max(0) as usize..=(hdef.end as usize).min(ls.len().saturating_sub(1))).map(|x| x.join("\n")).unwrap_or_default();
                        let hdir = Path::new(&hdef.path).parent().map(|x| x.to_path_buf());
                        for m in regex::Regex::new(r"((?:[A-Za-z_][A-Za-z0-9_]*\.)*)([A-Za-z_][A-Za-z0-9_]{3,})\s*\(").unwrap().captures_iter(&body) {
                            let name = &m[2];
                            let quals: Vec<String> = m[1].split('.').filter(|q| !q.is_empty()).map(|q| q.to_lowercase()).collect();
                            if generic(&name.to_lowercase()) { continue; }
                            // a call stays in its own language (same file extension as the caller),
                            // and a name defined in more than two places is too ambiguous to follow
                            let ext = Path::new(&hdef.path).extension().map(|e| e.to_owned());
                            let same_name: Vec<usize> = defs.iter().enumerate().filter(|(_, x)| x.name.rsplit('.').next() == Some(name) && Path::new(&x.path).extension().map(|e| e.to_owned()) == ext && kind_w(&x.kind) >= 1.0).map(|(j, _)| j).collect();
                            if same_name.len() > 2 { continue; }
                            for j in same_name {
                                let x = &defs[j];
                                // how the call is written: `s.d.Store.ListSessions(` names the receiver
                                // type, `queryCSV(` or `page.queryCSV(` a function of that package
                                let fits = match x.name.rsplit_once('.') {
                                    Some((recv, _)) => quals.iter().any(|q| *q == recv.to_lowercase()),
                                    None => quals.is_empty() || quals.last().map(|q| Some(q.as_str()) == Path::new(&x.path).parent().and_then(|p| p.file_name()).and_then(|f| f.to_str())).unwrap_or(false),
                                };
                                if !fits { continue; }
                                if x.path != hdef.path && matches!(x.role.as_str(), "source" | "query" | "model") && !v.iter().any(|y| y.2 == j) {
                                    let other_dir = Path::new(&x.path).parent().map(|p| p.to_path_buf()) != hdir;
                                    v.push((other_dir, m.get(0).unwrap().start(), j));
                                }
                            }
                        }
                    }
                    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                    // calls into other directories always; same-package helpers at most 3
                    let (other, local): (Vec<_>, Vec<_>) = v.into_iter().partition(|x| x.0);
                    other.into_iter().take(6).chain(local.into_iter().take(3)).map(|x| x.2).collect()
                };
                for ce in chain_callees {
                    let cdef = &defs[ce];
                    let cl = cdef.name.rsplit('.').next().unwrap_or(&cdef.name).to_string();
                    let at = text.get(&hdef.path).and_then(|ls| (hdef.start.max(0) as usize..=(hdef.end as usize).min(ls.len().saturating_sub(1))).find(|&k| ls[k].contains(&format!("{cl}(")))).map(|k| format!("{}:{}", hdef.path, k + 1)).unwrap_or_else(|| hdef.path.clone());
                    let tabs: Vec<String> = rows("SELECT name, kind, line FROM tables WHERE file = ?1 AND line BETWEEN ?2 AND ?3 GROUP BY name", &[&cdef.file, &cdef.start, &cdef.end]).into_iter().map(|t| t.0).filter(|t| !STOP.contains(&t.to_lowercase().as_str())).collect();
                    // the SQL a store function runs may live in a constant beside it: name the definitions it uses there
                    let consts: Vec<String> = text.get(&cdef.path).map(|ls| {
                        let body = ls.get(cdef.start.max(0) as usize..=(cdef.end as usize).min(ls.len().saturating_sub(1))).map(|x| x.join("\n")).unwrap_or_default();
                        defs_by_file.get(cdef.path.as_str()).into_iter().flatten().filter(|&&j| j != ce && defs[j].kind.contains("const") && body.contains(defs[j].name.as_str())).map(|&j| format!("{} {}:{}-{}", defs[j].name, defs[j].path, defs[j].start + 1, defs[j].end + 1)).collect()
                    }).unwrap_or_default();
                    lines.push(format!("{at}  {} calls {} {}:{}-{}{}{}", hdef.name, cdef.name, cdef.path, cdef.start + 1, cdef.end + 1,
                        if tabs.is_empty() { String::new() } else { format!(" (tables: {})", tabs.join(", ")) },
                        if consts.is_empty() { String::new() } else { format!(" (uses {})", consts.join(", ")) }));
                }
            }
            if lines.len() >= 2 {
                let _ = start;
                out.push(format!("chain (from the route map; every hop with file:line, each a place to read or change):\n  {}", lines.join("\n  ")));
            }
        }
    }
    if task == "change" && !chosen.is_empty() {
        if let Some(plan) = change_plan(root, query, &chosen.iter().map(|&i| (defs[i].name.clone(), defs[i].kind.clone(), defs[i].path.clone(), defs[i].role.clone(), defs[i].start, defs[i].end)).collect::<Vec<_>>(), &co, &rows) { out.push(plan); }
    }
    out
}

/// The edit as one `mt apply` bundle: the next migration's files, every target span with the hash
/// it was read with, the tests that reference the targets, the files usually committed alongside,
/// and the repo's own build check. Claude fills in the bodies and sends it once.
fn change_plan(root: &Path, query: &str, chosen: &[(String, String, String, String, i64, i64)], co: &HashMap<(String, String), i64>,
               rows: &dyn Fn(&str, &[&dyn rusqlite::ToSql]) -> Vec<(String, String, i64)>) -> Option<String> {
    let mut b = String::from("plan (change) — send as ONE `mt apply` bundle; spans are hash-checked and nothing is written unless all ops are valid:");
    let ident_rx = regex::Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b").unwrap();
    let idents: Vec<String> = ident_rx.find_iter(query).map(|m| m.as_str().to_string()).collect();
    let mut any = false;
    if let Some((tname, _, tpath, _, _, _)) = chosen.iter().find(|c| c.1 == "table" && c.3 == "migration") {
        let dir = Path::new(tpath).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some((w, next, suffixes)) = next_migration(root, &dir) {
            let slug = if idents.is_empty() { tname.to_lowercase() } else { format!("{}_{}", tname.to_lowercase(), idents.join("_")) };
            let base = format!("{dir}/{:0w$}_{slug}", next, w = w);
            for suf in &suffixes {
                let what = if suf.contains("down") { "<the exact reverse of the change>".to_string() } else { format!("<the change to {tname}>") };
                b += &format!("\n  @@ create {base}{suf}\n  {what}");
            }
            any = true;
        }
    }
    let mut targets = vec![];
    for (name, kind, path, role, s0, e0) in chosen.iter().filter(|c| c.1 != "table" && matches!(c.3.as_str(), "source" | "model" | "query")).take(4) {
        let Ok(src) = fs::read_to_string(root.join(path)) else { continue };
        let lines: Vec<String> = src.lines().map(String::from).collect();
        let h = crate::context::span_hash(&lines, *s0 as usize, *e0 as usize);
        b += &format!("\n  @@ replace {path}:L{}-{} {h}\n  <{kind} {name}, changed>   (or `@@ find {path} in {path}:L{}-{} {h}` for a smaller edit)", s0 + 1, e0 + 1, s0 + 1, e0 + 1);
        targets.push((name.rsplit('.').next().unwrap_or(name).to_string(), path.clone()));
        any = true;
    }
    if !any { return None; }
    // mt knows no toolchain: the repo's own build or test command goes here
    b += "\n  @@ check <the repo's build or test command>";
    let mut tests: Vec<String> = vec![];
    for (n, _) in &targets {
        for t in rows("SELECT f.path, r.lines, 0 FROM refs r JOIN files f ON f.id = r.file WHERE r.name = ?1 AND f.role IN ('test','integration') AND f.path NOT LIKE '%.md' GROUP BY f.path LIMIT 4", &[n]) {
            let e = format!("{}:{} ({n})", t.0, t.1.split(',').next().unwrap_or(""));
            if !tests.contains(&e) { tests.push(e); }
        }
    }
    if !tests.is_empty() { b += &format!("\n  tests that reference these targets (update if the signature changes): {}", tests.join(", ")); }
    let mut together: BTreeMap<String, i64> = BTreeMap::new();
    for (_, p) in &targets { for ((a, c), n) in co { if a == p && !targets.iter().any(|t| &t.1 == c) { *together.entry(c.clone()).or_default() += n; } if c == p && !targets.iter().any(|t| &t.1 == a) { *together.entry(a.clone()).or_default() += n; } } }
    let mut tg: Vec<(String, i64)> = together.into_iter().collect();
    tg.sort_by_key(|x| -x.1);
    if !tg.is_empty() { b += &format!("\n  usually committed together with these files (git): {}", tg.iter().take(5).map(|(f, n)| format!("{f} ({n}×)")).collect::<Vec<_>>().join(", ")); }
    Some(b)
}

// ---------------------------------------------------------------- show

pub fn show(g: &Graph, st: &State, q: &str) -> Result<String, String> {
    let n = g.resolve(q).ok_or_else(|| format!("mt tools: nothing named {q:?} (try `mt tools find {q}`)"))?;
    let mut s = format!("{} {} [{}]\n", n.kind, n.name, g.availability(st, n));
    let rel = |label: &str, v: Vec<(&str, i64)>| -> String {
        if v.is_empty() { return String::new(); }
        format!("{label}: {}\n", v.iter().take(12).map(|(x, _)| x.to_string()).collect::<Vec<_>>().join(", "))
    };
    s += &rel("provided by", g.inn(&n.id, "provides"));
    s += &rel("exposed by", g.inn(&n.id, "exposes"));
    s += &rel("works with", g.out(&n.id, "mentions"));
    s += &rel("needed by", g.inn(&n.id, "mentions"));
    let anchors = g.anchors(n);
    s += &match g.grounded(n) {
        Some((a, e)) => format!("this project: uses {a} ({e})\n"),
        None if anchors.is_empty() => "this project: general-purpose item, not tied to a technology\n".into(),
        None => format!("this project: no sign of {} in the code\n", anchors.join(", ")),
    };
    match n.kind.as_str() {
        "skill" | "agent" if !n.path.is_empty() => {
            let src = fs::read_to_string(&n.path).map_err(|e| format!("{}: {e}", n.path))?;
            let fm = frontmatter(&src);
            if n.kind == "skill" { s += &format!("\nBase directory for this skill: {}\n\n", Path::new(&n.path).parent().unwrap().display()); }
            else { s += "\nAgent prompt (when this agent is off, pass it to Agent(subagent_type=\"general-purpose\") as the brief's preamble):\n\n"; }
            s += fm.body.trim();
            s.push('\n');
        }
        "mcp" => {
            s += &format!("{}\n", n.extra);
            let tools: Vec<String> = g.out(&n.id, "exposes").iter().map(|(t, _)| t[5..].to_string()).collect();
            s += &format!("tools ({}): {}\n", tools.len(), tools.join(", "));
            if !n.desc.is_empty() { s += &format!("\n{}\n", n.desc); }
        }
        "plugin" => { s += &format!("{}\nprovides {} items; {}\n", n.desc, g.out(&n.id, "provides").len(), n.extra); }
        _ => { if !n.desc.is_empty() { s += &format!("\n{}\n", n.desc); } }
    }
    Ok(s)
}

// ---------------------------------------------------------------- profile

pub struct ProfileOpts { pub keep: Vec<String>, pub include_hooked: bool, pub keep_descriptions: bool }

/// The settings layer that turns off every item whose product or technology the code does not
/// use, one evidence line per decision. General-purpose items (no anchor) stay on.
pub fn profile(g: &Graph, o: &ProfileOpts) -> (Value, Vec<String>) {
    let keep = |n: &Node| o.keep.iter().any(|k| k == &n.name || k == &n.id || (!n.plugin.is_empty() && k == &n.plugin));
    let mut plugins_off = serde_json::Map::new();
    let mut overrides = serde_json::Map::new();
    let mut deny: Vec<String> = vec![];
    let mut denied_servers: Vec<Value> = vec![];
    let mut log = vec![];
    let mut sorted: Vec<&Node> = g.nodes.values().collect();
    sorted.sort_by(|a, b| (a.kind.as_str(), a.id.as_str()).cmp(&(b.kind.as_str(), b.id.as_str())));
    let verdict = |n: &Node| -> Result<String, String> {
        if keep(n) { return Ok("kept by --keep".into()); }
        let anchors = g.anchors(n);
        if anchors.is_empty() { return Ok("general-purpose".into()); }
        match g.grounded(n) { Some((a, e)) => Ok(format!("{a} in code: {e}")), None => Err(format!("no {} in the code", anchors.join("/"))) }
    };
    let mut off_plugins: HashSet<String> = HashSet::new();
    for n in sorted.iter().filter(|n| n.kind == "plugin") {
        let extra: Value = serde_json::from_str(&n.extra).unwrap_or(Value::Null);
        if !extra.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) { continue; }
        let hooks: Vec<String> = extra.get("hooks").and_then(|h| h.as_array()).into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        let kids = g.out(&n.id, "provides").len();
        match verdict(n) {
            Ok(why) => log.push(format!("keep  plugin {} — {why}", n.name)),
            Err(_) if !hooks.is_empty() && !o.include_hooked => log.push(format!("keep  plugin {} — {}, but it runs hooks ({}); --include-hooked turns it off", n.name, verdict(n).unwrap_err(), hooks.join(","))),
            Err(why) => {
                plugins_off.insert(extra.get("key").and_then(|k| k.as_str()).unwrap_or(&n.name).to_string(), Value::Bool(false));
                off_plugins.insert(n.name.clone());
                log.push(format!("off   plugin {} — {why}; drops {kids} skills/agents/commands/servers{}", n.name, if hooks.is_empty() { String::new() } else { format!(" and its hooks ({})", hooks.join(",")) }));
            }
        }
    }
    // two enabled plugins that ship mostly the same skills (vercel and vercel-plugin): keep the one
    // with more, turn the other off, since both would list and both would run their hooks
    let kept: Vec<&&Node> = sorted.iter().filter(|n| n.kind == "plugin" && !off_plugins.contains(&n.name) && serde_json::from_str::<Value>(&n.extra).ok().and_then(|e| e.get("enabled").and_then(|x| x.as_bool())).unwrap_or(false)).collect();
    let short_names = |p: &Node| -> HashSet<String> { g.out(&p.id, "provides").iter().filter_map(|(k, _)| g.nodes.get(*k)).filter(|k| k.kind == "skill").map(|k| k.name.rsplit(':').next().unwrap_or(&k.name).to_string()).collect() };
    for (i, a) in kept.iter().enumerate() {
        for b in kept.iter().skip(i + 1) {
            if off_plugins.contains(&a.name) || off_plugins.contains(&b.name) || keep(a) || keep(b) { continue; }
            let (sa, sb) = (short_names(a), short_names(b));
            let shared = sa.intersection(&sb).count();
            let small = sa.len().min(sb.len());
            if small >= 3 && shared * 10 >= small * 6 {
                let (lose, win) = if sa.len() >= sb.len() { (b, a) } else { (a, b) };
                let key = serde_json::from_str::<Value>(&lose.extra).ok().and_then(|e| e.get("key").and_then(|k| k.as_str()).map(String::from)).unwrap_or(lose.name.clone());
                plugins_off.insert(key, Value::Bool(false));
                off_plugins.insert(lose.name.clone());
                log.retain(|l| !l.starts_with(&format!("keep  plugin {} —", lose.name)));
                log.push(format!("off   plugin {} — duplicates plugin {} ({shared} of its {} skills have the same names); {} stays", lose.name, win.name, short_names(lose).len(), win.name));
            }
        }
    }
    for n in &sorted {
        if n.kind == "plugin" || n.kind == "tool" || (!n.plugin.is_empty() && off_plugins.contains(&n.plugin)) { continue; }
        // a kept plugin's own skills go with it; only standalone items are decided one by one
        if !n.plugin.is_empty() && n.kind != "mcp" { continue; }
        let why = match verdict(n) { Ok(_) => continue, Err(w) => w };
        match n.kind.as_str() {
            "skill" => { overrides.insert(n.name.clone(), json!("off")); log.push(format!("off   skill {} — {why}", n.name)); }
            "agent" => { deny.push(format!("Agent({})", n.name)); log.push(format!("off   agent {} — {why}", n.name)); }
            "mcp" => {
                let short = n.name.rsplit(':').next().unwrap_or(&n.name).to_string();
                if n.plugin.is_empty() { denied_servers.push(json!({"serverName": short})); }
                else { deny.push(mcp_prefix(&format!("plugin_{}_{}", n.plugin, short)).trim_end_matches('_').to_string()); }
                log.push(format!("off   mcp {} — {why}; {} tools", n.name, g.out(&n.id, "exposes").len()));
            }
            _ => {}
        }
    }
    let mut v = json!({});
    if !plugins_off.is_empty() { v["enabledPlugins"] = Value::Object(plugins_off); }
    if !overrides.is_empty() { v["skillOverrides"] = Value::Object(overrides); }
    if !deny.is_empty() { v["permissions"] = json!({"deny": deny}); }
    if !denied_servers.is_empty() { v["deniedMcpServers"] = Value::Array(denied_servers); }
    // the skill listing has a fixed character budget that refills with descriptions as skills go;
    // at the floor it lists names only, and the prompt hook supplies descriptions when they matter
    if !o.keep_descriptions {
        v["skillListingBudgetFraction"] = json!(0.002);
        log.push("names skill listing: names only (skillListingBudgetFraction 0.002); the hook adds descriptions for the skills a prompt needs; --keep-descriptions keeps them".into());
    }
    (v, log)
}

/// Merge the profile into `<project>/.claude/settings.local.json`, keeping a backup of what was there
/// in `.mt/tools-settings.backup.json` so `--undo` restores it exactly.
/// Hooks to register: (event, optional matcher, command). An existing hook running the same mt
/// subcommand is not added twice.
pub fn write_profile(project: &Path, layer: &Value, hooks: &[(&str, Option<&str>, String)]) -> Result<PathBuf, String> {
    let path = project.join(".claude/settings.local.json");
    let backup = project.join(".mt/tools-settings.backup.json");
    let before = fs::read_to_string(&path).ok();
    if !backup.exists() {
        let _ = fs::create_dir_all(project.join(".mt"));
        fs::write(&backup, json!({"existed": before.is_some(), "content": before.clone().unwrap_or_default()}).to_string()).map_err(|e| e.to_string())?;
    }
    let mut cur: Value = before.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!({}));
    for (k, val) in layer.as_object().unwrap() {
        match (k.as_str(), val) {
            ("permissions", p) => {
                let d = cur.pointer_mut("/permissions/deny").and_then(|x| x.as_array_mut()).map(|a| std::mem::take(a)).unwrap_or_default();
                let mut set: Vec<Value> = d; for x in p["deny"].as_array().unwrap() { if !set.contains(x) { set.push(x.clone()); } }
                if cur.get("permissions").is_none() { cur["permissions"] = json!({}); }
                cur["permissions"]["deny"] = Value::Array(set);
            }
            ("deniedMcpServers", Value::Array(a)) => {
                let mut set = cur.get("deniedMcpServers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                for x in a { if !set.contains(x) { set.push(x.clone()); } }
                cur["deniedMcpServers"] = Value::Array(set);
            }
            (_, Value::Object(m)) => { if cur.get(k).is_none() { cur[k] = json!({}); } for (a, b) in m { cur[k][a] = b.clone(); } }
            _ => { cur[k] = val.clone(); }
        }
    }
    for (event, matcher, cmd) in hooks {
        let sub = cmd.split("tools ").nth(1).unwrap_or(cmd).split_whitespace().next().unwrap_or("").to_string();
        let mut hook = json!({"hooks": [{"type": "command", "command": cmd, "timeout": 10}]});
        if let Some(m) = matcher { hook["matcher"] = json!(m); }
        let arr = cur.pointer(&format!("/hooks/{event}")).and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if !arr.iter().any(|h| h.to_string().contains(&format!("tools {sub}"))) {
            let mut arr = arr; arr.push(hook);
            if cur.get("hooks").is_none() { cur["hooks"] = json!({}); }
            cur["hooks"][*event] = Value::Array(arr);
        }
    }
    let _ = fs::create_dir_all(project.join(".claude"));
    fs::write(&path, serde_json::to_string_pretty(&cur).unwrap() + "\n").map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn undo_profile(project: &Path) -> Result<String, String> {
    let backup = project.join(".mt/tools-settings.backup.json");
    let b = read_json(&backup);
    if b.is_null() { return Err("mt tools: no profile backup in .mt/tools-settings.backup.json".into()); }
    let path = project.join(".claude/settings.local.json");
    if b["existed"].as_bool().unwrap_or(false) { fs::write(&path, b["content"].as_str().unwrap_or("")).map_err(|e| e.to_string())?; } else { let _ = fs::remove_file(&path); }
    let _ = fs::remove_file(&backup);
    Ok(format!("restored {}", path.display()))
}

/// Take one item back out of the project's local settings.
pub fn enable(g: &Graph, project: &Path, q: &str) -> Result<String, String> {
    let n = g.resolve(q).ok_or_else(|| format!("mt tools: nothing named {q:?}"))?;
    let path = project.join(".claude/settings.local.json");
    let mut cur: Value = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    let mut changed = vec![];
    let short = n.name.rsplit(':').next().unwrap_or(&n.name).to_string();
    let deny_keys: Vec<String> = match n.kind.as_str() {
        "agent" => vec![format!("Agent({})", n.name)],
        "tool" => vec![n.name.clone()],
        "mcp" => vec![mcp_prefix(&format!("plugin_{}_{}", n.plugin, short)).trim_end_matches('_').to_string()],
        _ => vec![],
    };
    if let Some(a) = cur.pointer_mut("/permissions/deny").and_then(|x| x.as_array_mut()) {
        a.retain(|x| { let hit = x.as_str().is_some_and(|s| deny_keys.iter().any(|k| k == s)); if hit { changed.push(format!("permissions.deny -{}", x)); } !hit });
    }
    if n.kind == "mcp" {
        if let Some(a) = cur.get_mut("deniedMcpServers").and_then(|x| x.as_array_mut()) {
            a.retain(|x| { let hit = x.get("serverName").and_then(|s| s.as_str()) == Some(short.as_str()); if hit { changed.push(format!("deniedMcpServers -{short}")); } !hit });
        }
    }
    if n.kind == "skill" { if let Some(m) = cur.get_mut("skillOverrides").and_then(|x| x.as_object_mut()) { if m.remove(&n.name).is_some() { changed.push(format!("skillOverrides -{}", n.name)); } } }
    if !n.plugin.is_empty() {
        if let Some(p) = g.nodes.get(&format!("plugin:{}", n.plugin)) {
            let key = serde_json::from_str::<Value>(&p.extra).ok().and_then(|e| e.get("key").and_then(|k| k.as_str()).map(String::from)).unwrap_or_default();
            if let Some(m) = cur.get_mut("enabledPlugins").and_then(|x| x.as_object_mut()) { if m.remove(&key).is_some() { changed.push(format!("enabledPlugins -{key} (run /reload-plugins)")); } }
        }
    }
    if changed.is_empty() { return Ok(format!("{} {} is not turned off in {}", n.kind, n.name, path.display())); }
    fs::write(&path, serde_json::to_string_pretty(&cur).unwrap() + "\n").map_err(|e| e.to_string())?;
    Ok(format!("{}: {}", path.display(), changed.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_folded_description() {
        let f = frontmatter("---\nname: x\ndescription: >\n  Sync columns\n  onto persons.\n---\nBody here\n");
        assert_eq!(f.name.as_deref(), Some("x"));
        assert_eq!(f.desc, "Sync columns onto persons.");
        assert_eq!(f.body.trim(), "Body here");
    }

    #[test]
    fn tokens_stem_and_drop_stopwords() {
        assert_eq!(tokens("Searching the web for flags"), vec!["search", "web", "flag"]);
        assert_eq!(tokens("tabs class status"), vec!["tab", "class", "status"]);
        assert_eq!(tokens("invoices boxes matches entries used"), vec!["invoice", "box", "match", "entry", "used"]);
        assert_eq!(tokens("make a 3d model so it sounds less like AI"), vec!["3d", "model", "sound", "less", "ai"]);
    }

    #[test]
    fn split_words_camel_and_separators() {
        assert_eq!(split_words("posthog-js"), vec!["posthog", "js"]);
        assert_eq!(split_words("PostHogClient"), vec!["posthogclient", "post", "hog", "client"]);
    }

    #[test]
    fn mcp_prefix_normalises() {
        assert_eq!(mcp_prefix("claude.ai Claude Docs"), "mcp__claude_ai_Claude_Docs__");
        assert_eq!(mcp_prefix("claude-in-chrome"), "mcp__claude-in-chrome__");
    }

    fn node(id: &str, kind: &str, name: &str, plugin: &str, desc: &str, extra: &str) -> Node {
        Node { id: id.into(), kind: kind.into(), name: name.into(), plugin: plugin.into(), desc: desc.into(), path: String::new(), chars: 100, extra: extra.into() }
    }
    fn graph(facts: Facts) -> Graph {
        Graph::from_parts(vec![
            node("plugin:posthog", "plugin", "posthog", "posthog", "analytics", r#"{"key":"posthog@x","enabled":true,"hooks":[]}"#),
            node("plugin:superpowers", "plugin", "superpowers", "superpowers", "workflow", r#"{"key":"superpowers@x","enabled":true,"hooks":[]}"#),
            node("plugin:recorder", "plugin", "recorder", "recorder", "records sessions", r#"{"key":"recorder@x","enabled":true,"hooks":["Stop"]}"#),
            node("skill:posthog:creating-surveys", "skill", "posthog:creating-surveys", "posthog", "Create a survey in PostHog to collect feedback", ""),
            node("skill:posthog:flags", "skill", "posthog:flags", "posthog", "Add PostHog feature flags", ""),
            node("skill:superpowers:brainstorming", "skill", "superpowers:brainstorming", "superpowers", "Use before any creative work", ""),
            node("skill:recorder:replay", "skill", "recorder:replay", "recorder", "Replay a recorder session", ""),
            node("skill:rpg", "skill", "rpg", "", "Build an RPG game with stats and quests", ""),
            node("skill:humanizer", "skill", "humanizer", "", "Remove signs of AI-generated writing", ""),
            node("agent:prisma-query-optimizer", "agent", "prisma-query-optimizer", "", "Optimise Prisma queries", ""),
            node("agent:cto-orchestrator", "agent", "cto-orchestrator", "", "Coordinates plans", ""),
            node("mcp:claude-in-chrome", "mcp", "claude-in-chrome", "", "Drive the browser: tabs, clicks, screenshots", "{}"),
            node("tool:mcp__claude-in-chrome__navigate", "tool", "mcp__claude-in-chrome__navigate", "", "Navigate a browser tab to a URL", "mcp"),
            node("tool:mcp__claude-in-chrome__tabs_context", "tool", "mcp__claude-in-chrome__tabs_context", "", "List the open browser tabs", "mcp"),
            node("tool:mcp__claude-in-chrome__computer", "tool", "mcp__claude-in-chrome__computer", "", "Click and type and take a screenshot", "mcp"),
            node("mcp:graft", "mcp", "graft", "", "Code graph", "{}"),
        ], &[("plugin:posthog", "provides", "skill:posthog:creating-surveys"), ("plugin:posthog", "provides", "skill:posthog:flags"),
             ("plugin:superpowers", "provides", "skill:superpowers:brainstorming"), ("plugin:recorder", "provides", "skill:recorder:replay"),
             ("mcp:claude-in-chrome", "exposes", "tool:mcp__claude-in-chrome__navigate"), ("mcp:claude-in-chrome", "exposes", "tool:mcp__claude-in-chrome__computer"), ("mcp:claude-in-chrome", "exposes", "tool:mcp__claude-in-chrome__tabs_context")], facts)
    }
    fn mkfacts(strong: &[(&str, &str)], refs: &[(&str, i64)]) -> Facts {
        Facts { strong: strong.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), refs: refs.iter().map(|(a, b)| (a.to_string(), *b)).collect(), ..Default::default() }
    }
    fn empty_state() -> State { State { deny: HashSet::new(), denied_servers: HashSet::new(), overrides: HashMap::new(), plugins: HashMap::new() } }

    #[test]
    fn anchors_come_from_products_and_specific_name_words() {
        let g = graph(Facts::default());
        assert_eq!(g.anchors(&g.nodes["skill:posthog:flags"]), vec!["posthog"]);
        assert!(g.anchors(&g.nodes["skill:superpowers:brainstorming"]).is_empty(), "workflow plugin is general");
        assert_eq!(g.anchors(&g.nodes["agent:prisma-query-optimizer"]), vec!["prisma"]);
        assert!(g.anchors(&g.nodes["agent:cto-orchestrator"]).contains(&"cto".to_string()));
        assert_eq!(g.anchors(&g.nodes["tool:mcp__claude-in-chrome__navigate"]), vec!["chrome"]);
    }

    #[test]
    fn profile_follows_the_code() {
        // a web app with posthog-js and Prisma, no browser automation in the code
        let g = graph(mkfacts(&[("posthog", "package.json: posthog-js"), ("prisma", "package.json: @prisma/client")], &[("graft", 1)]));
        let (v, log) = profile(&g, &ProfileOpts { keep: vec![], include_hooked: false, keep_descriptions: false });
        assert_eq!(v["skillListingBudgetFraction"], json!(0.002));
        assert!(v.get("enabledPlugins").and_then(|p| p.get("posthog@x")).is_none(), "posthog is in the code: {log:?}");
        assert!(v.get("enabledPlugins").and_then(|p| p.get("superpowers@x")).is_none(), "general plugin stays: {log:?}");
        assert!(v.get("enabledPlugins").and_then(|p| p.get("recorder@x")).is_none(), "hooked plugin stays: {log:?}");
        assert_eq!(v["skillOverrides"]["rpg"], json!("off"));
        assert!(v["skillOverrides"].get("humanizer").is_some(), "humanizer has its own anchor and no code for it");
        let deny: Vec<&str> = v["permissions"]["deny"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        assert!(deny.contains(&"Agent(cto-orchestrator)") && !deny.contains(&"Agent(prisma-query-optimizer)"), "{deny:?}");
        let servers: Vec<&str> = v["deniedMcpServers"].as_array().unwrap().iter().map(|x| x["serverName"].as_str().unwrap()).collect();
        assert!(servers.contains(&"claude-in-chrome") && servers.contains(&"graft"), "1 ref is below the threshold: {servers:?}");
        assert!(log.iter().any(|l| l.contains("posthog in code: package.json: posthog-js")), "{log:?}");
        let g2 = graph(mkfacts(&[], &[("rpg", 50), ("chrome", 5)]));
        assert!(g2.grounded(&g2.nodes["skill:rpg"]).is_none(), "identifier refs don't ground a standalone skill");
        assert!(g2.grounded(&g2.nodes["mcp:claude-in-chrome"]).is_some(), "but they do ground a product");
        // the same catalog against a repo without posthog
        let g = graph(Facts::default());
        let (v, _) = profile(&g, &ProfileOpts { keep: vec!["humanizer".into()], include_hooked: true, keep_descriptions: true });
        assert_eq!(v["enabledPlugins"]["posthog@x"], json!(false));
        assert_eq!(v["enabledPlugins"]["recorder@x"], json!(false));
        assert!(v["skillOverrides"].get("humanizer").is_none(), "--keep wins");
        assert!(v.get("skillListingBudgetFraction").is_none(), "--keep-descriptions leaves the listing alone");
    }

    #[test]
    fn find_groups_server_tools_and_prefers_code_grounded() {
        let g = graph(mkfacts(&[("posthog", "package.json: posthog-js")], &[]));
        let c = cards(&g, &empty_state(), &find(&g, "take a screenshot of the browser tab", 3), 3, 0.5);
        assert!(c[0].starts_with("mcp claude-in-chrome") && c[0].contains("select:mcp__claude-in-chrome__"), "{c:?}");
        assert_eq!(c.iter().filter(|x| x.starts_with("mcp claude-in-chrome")).count(), 1, "{c:?}");
        let h = find(&g, "feature flags", 3);
        assert_eq!(h[0].id, "skill:posthog:flags");
        assert!(h[0].why.iter().any(|w| w.contains("posthog in code")), "{:?}", h[0].why);
    }

    #[test]
    fn prose_with_incidental_words_gets_no_tool_card() {
        let g = graph(mkfacts(&[("posthog", "package.json: posthog-js")], &[]));
        // "feature" sits only in the description of posthog:flags: one incidental word is not a match
        let c = cards(&g, &empty_state(), &find(&g, "our platform ships every feature with seamless quality", 3), 3, 0.0);
        assert!(c.is_empty(), "{c:?}");
        let c = cards(&g, &empty_state(), &find(&g, "create a posthog survey", 3), 3, 0.0);
        assert!(c[0].contains("posthog:creating-surveys"), "named product + name word still matches: {c:?}");
    }

    #[test]
    fn off_items_point_to_mt_show_and_enable() {
        let g = graph(Facts::default());
        let mut st = empty_state();
        st.plugins.insert("posthog@x".into(), false);
        st.denied_servers.insert("claude-in-chrome".into());
        let c = cards(&g, &st, &find(&g, "create a posthog survey", 3), 3, 0.5);
        assert!(c[0].contains("[off: plugin posthog disabled here]") && c[0].contains("mt tools show posthog:creating-surveys"), "{c:?}");
        let c = cards(&g, &st, &find(&g, "navigate the browser", 3), 3, 0.5);
        assert!(c.is_empty(), "an off server stays hidden unless the prompt names it: {c:?}");
        let c = cards(&g, &st, &find(&g, "navigate the chrome browser", 3), 3, 0.5);
        assert!(c[0].contains("mt tools enable claude-in-chrome"), "{c:?}");
    }

    #[test]
    fn code_cards_link_tests_migrations_docs_and_tech() {
        let d = std::env::temp_dir().join(format!("mt-code-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for dir in ["api", "api/migrations", "docs"] { fs::create_dir_all(d.join(dir)).unwrap(); }
        fs::write(d.join("api/billing.go"), "package api\n\nimport \"github.com/stripe/stripe-go\"\n\nfunc ChargeInvoice(id string) error {\n\tdb.Exec(\"UPDATE invoices SET paid = true WHERE id = $1\", id)\n\treturn stripe.Charge(id)\n}\n").unwrap();
        fs::write(d.join("api/billing_test.go"), "package api\n\nfunc TestChargeInvoice(t *testing.T) {\n\tChargeInvoice(\"x\")\n}\n").unwrap();
        fs::write(d.join("api/migrations/001_invoices.sql"), "CREATE TABLE invoices (id text primary key, paid boolean);\n").unwrap();
        fs::write(d.join("docs/billing.md"), "# Billing\n\nChargeInvoice marks an invoice paid.\n").unwrap();
        fs::write(d.join("api/fixture.json"), "{\"type\": \"invoice\", \"charge\": 1}\n").unwrap();
        let mut idx = index::Index::open(&d).unwrap();
        idx.update().unwrap();
        let mut g = graph(Facts::default());
        g.nodes.insert("plugin:stripe".into(), node("plugin:stripe", "plugin", "stripe", "stripe", "payments", r#"{"key":"stripe@x","enabled":true,"hooks":[]}"#));
        g.product.insert("stripe".into(), "stripe".into());
        let c = code_cards(&g, &d, "charge the invoice when payment succeeds", 2);
        let card = &c[0];
        assert!(card.starts_with("code function ChargeInvoice — api/billing.go:5-8 (source)"), "{c:?}");
        assert!(card.contains("(refs index ✓ exhaustive): api/billing_test.go:"), "{card}");
        assert!(card.contains("invoices [use] ← api/migrations/001_invoices.sql:1 (migration)"), "{card}");
        assert!(card.contains("docs: docs/billing.md:"), "{card}");
        assert!(card.contains("tech: stripe → plugin stripe"), "{card}");
        assert!(card.contains("mt context ChargeInvoice"), "{card}");
        assert!(card.contains("lines api/billing.go:5-8 h="), "the first card carries the lines: {card}");
        let c3 = code_cards(&g, &d, "use the humanizer skill to rewrite this reminder email so it sounds less like AI", 3);
        assert!(c3.is_empty(), "a request for a skill, with no definition matched by name, gets no code cards: {c3:?}");
        let c2 = code_cards(&g, &d, "change the Go type that charges an invoice", 3);
        assert!(!c2.iter().any(|x| x.starts_with("code key type")), "a short JSON key is not a match for the word: {c2:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ppr_lifts_connected_seeds_over_an_isolated_one() {
        // seeds 0 and 1 are equal and linked through node 3 (a shared table); seed 2 scores higher
        // lexically but is connected to nothing
        let r = ppr(4, &[(0, 3, 1.0), (1, 3, 1.0), (0, 1, 1.0)], &[1.0, 1.0, 1.4, 0.0], 0.25, 50);
        assert!(r[0] > r[2] && r[1] > r[2], "{r:?}");
        let total: f64 = r.iter().sum();
        assert!((total - 1.0).abs() < 1e-9, "mass is conserved: {total}");
    }

    #[test]
    fn task_kind_and_route_shape() {
        assert_eq!(task_kind("add an archived_at column"), "change");
        assert_eq!(task_kind("the websocket drops events"), "debug");
        assert_eq!(task_kind("which test covers WriteAudit?"), "question");
        assert_eq!(route_shape("/v1/sessions/{id}"), "/v1/sessions/*");
        assert_eq!(route_shape("/v1/sessions/:id/"), "/v1/sessions/*");
        assert_eq!(route_shape("/v1/sessions/${id}?x=1"), "/v1/sessions/*");
    }

    #[test]
    fn routes_cross_subprojects_and_change_plan() {
        let d = std::env::temp_dir().join(format!("mt-routes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for dir in ["api", "api/migrations", "web/src"] { fs::create_dir_all(d.join(dir)).unwrap(); }
        fs::write(d.join("go.mod"), "module x\n").unwrap();
        fs::write(d.join("api/router.go"), "package api\n\nfunc routes(r Router) {\n\tr.Get(\"/v1/invoices/{id}\", s.wrap(s.getInvoice))\n}\n").unwrap();
        fs::write(d.join("api/invoice.go"), "package api\n\n// getInvoice loads one invoice for the detail page.\nfunc (s *Server) getInvoice(w http.ResponseWriter, r *http.Request) error {\n\trow := db.QueryRow(\"SELECT id, paid FROM invoices WHERE id = $1\")\n\treturn row.Scan()\n}\n").unwrap();
        fs::write(d.join("api/invoice_test.go"), "package api\n\nfunc TestGetInvoice(t *testing.T) { s.getInvoice(nil, nil) }\n").unwrap();
        fs::write(d.join("api/migrations/0001_init.up.sql"), "CREATE TABLE invoices (id text primary key, paid boolean);\n").unwrap();
        fs::write(d.join("api/migrations/0001_init.down.sql"), "DROP TABLE invoices;\n").unwrap();
        fs::write(d.join("web/src/InvoicePage.tsx"), "export function InvoicePage({ id }) {\n  const inv = useQuery(`/v1/invoices/${id}`)\n  return inv\n}\n").unwrap();
        let mut idx = index::Index::open(&d).unwrap();
        idx.update().unwrap();
        let g = graph(Facts::default());
        let c = code_cards(&g, &d, "the invoice page shows the wrong paid state for an invoice", 3);
        let all = c.join("\n");
        assert!(all.contains("serves route /v1/invoices/* (registered api/router.go:4) — called from: web/src/InvoicePage.tsx:2 (InvoicePage)"), "{all}");
        let ch = code_cards(&g, &d, "the invoice page shows a stale paid flag: trace it from the screen through the api to the database", 3).join("\n");
        assert!(ch.contains("chain (from the route map") && ch.contains("web/src/InvoicePage.tsx:2  InvoicePage  calls /v1/invoices/*")
            && ch.contains("registers /v1/invoices/* → Server.getInvoice api/invoice.go:3-7"), "{ch}");
        let c = code_cards(&g, &d, "add a paid_at column to the invoices table and return it from getInvoice", 4);
        let plan = c.iter().find(|x| x.starts_with("plan (change)")).expect("a change gets a plan");
        assert!(plan.contains("@@ create api/migrations/0002_invoices_paid_at.up.sql") && plan.contains("0002_invoices_paid_at.down.sql"), "{plan}");
        assert!(plan.contains("@@ replace api/invoice.go:L") && plan.contains("@@ check <the repo's build or test command>"), "{plan}");
        assert!(plan.contains("api/invoice_test.go"), "tests of the target are listed: {plan}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn wire_keys_link_components_that_never_call_each_other() {
        let d = std::env::temp_dir().join(format!("mt-wire-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for dir in ["agent/ship", "server/fold"] { fs::create_dir_all(d.join(dir)).unwrap(); }
        fs::write(d.join("agent/ship/emit.go"), "package ship\n\nfunc EmitTurnEnded(q Queue) {\n\tq.Put(\"turn.ended\", payload())\n}\n").unwrap();
        fs::write(d.join("server/fold/apply.go"), "package fold\n\nfunc FoldEvent(e Event) {\n\tif e.Kind == \"turn.ended\" { closeTurn(e) }\n}\n").unwrap();
        let mut idx = index::Index::open(&d).unwrap();
        idx.update().unwrap();
        let g = graph(Facts::default());
        let c = code_cards(&g, &d, "where does the agent emit the turn ended event", 3).join("\n");
        assert!(c.contains("wire (shared with other components): `turn.ended` also in FoldEvent server/fold/apply.go:3"), "{c}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn facts_read_manifests_and_markers() {
        let d = std::env::temp_dir().join(format!("mt-facts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("api")).unwrap();
        fs::write(d.join("package.json"), r#"{"dependencies":{"posthog-js":"^1.200.0","@prisma/client":"^5.1.0"}}"#).unwrap();
        fs::write(d.join("api/go.mod"), "module x\nrequire (\n\tgithub.com/stripe/stripe-go/v76 v76.0.0\n)\n").unwrap();
        fs::write(d.join("vercel.json"), "{}").unwrap();
        index::Index::open(&d).unwrap().update().unwrap();
        let f = facts(&d);
        assert_eq!(f.evidence("posthog", false).as_deref(), Some("package.json:1: posthog-js"));
        assert!(f.evidence("prisma", false).is_some() && f.evidence("stripe", false).is_some() && f.evidence("vercel", false).is_some(), "{:?}", f.strong);
        assert!(f.evidence("unity", true).is_none());
        let _ = fs::remove_dir_all(&d);
    }
}
