//! The parts of `mt tools` that reach past the settings layer:
//!
//! - `slim`: trimmed copies of the plugins a project keeps, holding only the skills its code needs,
//!   served from a project-local marketplace (`.mt/marketplace`). Claude Code has no per-skill
//!   switch for plugin skills; a copy is the only way to drop some and keep the rest.
//! - `gate`: runs a hook command unchanged (same stdin, exit code and decisions) and filters only the
//!   context text it adds: a repeat within the session, or text about a product this code doesn't
//!   use, is dropped. Every call is logged with its bytes in and out.
//! - `hooks`: every configured hook with what the gate measured, and `wrap` to put a settings
//!   file's hooks behind the gate (with a backup).
//! - `refresh`: at SessionStart, the profile is recomputed and the difference applied, so a new
//!   dependency turns its tools back on.
//! - `capture`: PostToolUse records MCP tool names the session actually uses, so HTTP servers
//!   (which need the session's auth to list tools) get tool nodes too.
//! - `explain`: one real request taken apart, each listed item with mt's decision for it.
//! - `agents`: the same graph wired into Codex (hooks) and Cursor (rules).

use crate::tools::{self, read_json, split_words, Graph};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GATE_LOG: &str = "/tmp/mt-gate.log";

// ---------------------------------------------------------------- slim plugins

pub struct SlimPlan { pub plugin: String, pub key: String, pub src: PathBuf, pub keep: Vec<String>, pub drop: Vec<(String, String)> }

/// Which skills of each kept plugin this code gives a reason to load. A skill is dropped when its
/// own name has specific words (`next-upgrade`, `vercel-sandbox`) and none of them is a strong fact
/// of the code; a skill whose name is only generic words stays.
pub fn slim_plans(g: &Graph, off: &HashSet<String>) -> Vec<SlimPlan> {
    let mut plans = vec![];
    let mut plugins: Vec<&tools::Node> = g.nodes.values().filter(|n| n.kind == "plugin").collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    for p in plugins {
        let extra: Value = serde_json::from_str(&p.extra).unwrap_or(Value::Null);
        let key = extra.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        if !extra.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) || off.contains(&key) { continue; }
        // only a product plugin is trimmed: a workflow plugin's skill names (brainstorming,
        // executing-plans) are activities, not technologies the code could show
        let Some(product) = g.product.get(&p.name).cloned() else { continue };
        let skills: Vec<&tools::Node> = g.out(&p.id, "provides").iter().filter_map(|(k, _)| g.nodes.get(*k)).filter(|n| n.kind == "skill" && n.extra != "command" && !n.path.is_empty()).collect();
        let (mut keep, mut drop) = (vec![], vec![]);
        for s in skills {
            let dir = Path::new(&s.path).parent().map(|d| d.file_name().unwrap().to_string_lossy().into_owned()).unwrap_or_default();
            let short = s.name.rsplit(':').next().unwrap_or(&s.name);
            let words: Vec<String> = split_words(short).into_iter().filter(|w| !tools::generic(w) && *w != product && *w != p.name).collect();
            let grounded = words.iter().find_map(|w| g.facts.evidence(w, false).map(|e| format!("{w}: {e}")));
            match (words.is_empty(), grounded) {
                (true, _) => keep.push(dir),
                (false, Some(_)) => keep.push(dir),
                (false, None) => drop.push((dir, format!("no {} in the code", words.join("/")))),
            }
        }
        if drop.len() >= 3 { plans.push(SlimPlan { plugin: p.name.clone(), key, src: PathBuf::from(&p.path), keep, drop }); }
    }
    plans
}

#[cfg(unix)]
fn link(src: &Path, dst: &Path) -> std::io::Result<()> { std::os::unix::fs::symlink(src, dst) }

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)?.flatten() {
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if s.is_dir() { copy_dir(&s, &d)?; } else { fs::copy(&s, &d)?; }
    }
    Ok(())
}

/// Build the trimmed copies under `<project>/.mt/marketplace` and return the settings that switch
/// each original off and its copy on. Claude Code follows symlinks for most of a plugin but not
/// for `.claude-plugin/`, `skills/` or `commands/` (measured), so those are copied; everything else
/// (scripts, node_modules) is linked. Hook commands in the copy are put behind `mt tools gate`.
/// The marketplace name for one project: Claude Code installs what a project-declared marketplace
/// offers (scope local) and caches it, so the name must not collide across projects.
pub fn marketplace_name(project: &Path) -> String { format!("mt-{}", &hash(&project.to_string_lossy())[..8]) }

pub fn write_slim(project: &Path, plans: &[SlimPlan], mt: &str) -> Result<Value, String> {
    let mkt = marketplace_name(project);
    let root = project.join(".mt/marketplace");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".claude-plugin")).map_err(|e| e.to_string())?;
    let mut listed = vec![];
    let mut enabled = serde_json::Map::new();
    for p in plans {
        let dst = root.join("plugins").join(&p.plugin);
        fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
        for e in fs::read_dir(&p.src).map_err(|e| e.to_string())?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let (s, d) = (e.path(), dst.join(&name));
            match name.as_str() {
                ".claude-plugin" => {
                    copy_dir(&s, &d).map_err(|e| e.to_string())?;
                    // a version of its own: Claude Code serves the cached copy until the version changes
                    let pj = d.join("plugin.json");
                    let mut v = read_json(&pj);
                    let base = v.get("version").and_then(|x| x.as_str()).unwrap_or("0.0.0").to_string();
                    let mut keep = p.keep.clone(); keep.sort();
                    v["version"] = json!(format!("{base}-mt.{}", &hash(&keep.join(","))[..8]));
                    fs::write(&pj, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
                }
                "commands" => copy_dir(&s, &d).map_err(|e| e.to_string())?,
                "skills" => {
                    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
                    for k in &p.keep { let ks = s.join(k); if ks.is_dir() { copy_dir(&ks, &d.join(k)).map_err(|e| e.to_string())?; } }
                }
                "hooks" => {
                    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
                    for h in fs::read_dir(&s).map_err(|e| e.to_string())?.flatten() {
                        let hn = h.file_name().to_string_lossy().into_owned();
                        if hn == "hooks.json" {
                            let mut v = read_json(&h.path());
                            wrap_hooks(&mut v, mt);
                            fs::write(d.join(&hn), serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
                        } else { link(&h.path(), &d.join(&hn)).map_err(|e| e.to_string())?; }
                    }
                }
                _ => link(&s, &d).map_err(|e| e.to_string())?,
            }
        }
        listed.push(json!({"name": p.plugin, "source": format!("./plugins/{}", p.plugin), "description": format!("{} trimmed by mt: {} of {} skills", p.plugin, p.keep.len(), p.keep.len() + p.drop.len())}));
        enabled.insert(p.key.clone(), json!(false));
        enabled.insert(format!("{}@{mkt}", p.plugin), json!(true));
    }
    fs::write(root.join(".claude-plugin/marketplace.json"), serde_json::to_string_pretty(&json!({"name": mkt, "owner": {"name": "mt"}, "plugins": listed})).unwrap()).map_err(|e| e.to_string())?;
    let mut ekm = serde_json::Map::new();
    ekm.insert(mkt.clone(), json!({"source": {"source": "directory", "path": root}}));
    Ok(json!({"extraKnownMarketplaces": ekm, "enabledPlugins": enabled}))
}

/// Install the trimmed copies for this project (`claude plugin marketplace add` + `install --scope
/// local`). A marketplace declared in project settings is not installed by a headless session, and
/// an interactive one asks first; this does it explicitly. It changes the user's plugin registry,
/// so it runs only when asked (`--install`) and `--undo` reverses it.
pub fn install_slim(project: &Path) -> Vec<String> {
    let mkt = marketplace_name(project);
    let root = project.join(".mt/marketplace");
    let mut out = vec![];
    let run = |args: &[&str]| -> (bool, String) {
        match Command::new("claude").args(args).current_dir(project).stdin(Stdio::null()).output() {
            Ok(r) => (r.status.success(), String::from_utf8_lossy(&r.stdout).trim().lines().last().unwrap_or("").to_string() + &String::from_utf8_lossy(&r.stderr).trim().lines().last().map(|l| format!(" {l}")).unwrap_or_default()),
            Err(e) => (false, e.to_string()),
        }
    };
    let (ok, msg) = run(&["plugin", "marketplace", "add", &root.to_string_lossy()]);
    out.push(format!("marketplace add {mkt}: {} {msg}", if ok { "ok" } else { "failed" }));
    let m = read_json(&root.join(".claude-plugin/marketplace.json"));
    for p in m["plugins"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str()) {
        let (ok, msg) = run(&["plugin", "install", &format!("{p}@{mkt}"), "--scope", "local"]);
        out.push(format!("install {p}@{mkt} (scope local): {} {msg}", if ok { "ok" } else { "failed" }));
    }
    out
}

/// Take the trimmed plugins back out: uninstall each from this project (Claude Code installed them
/// when it first loaded the marketplace) and remove the marketplace, through `claude plugin`.
pub fn remove_slim(project: &Path) -> Vec<String> {
    let mkt = marketplace_name(project);
    let m = read_json(&project.join(".mt/marketplace/.claude-plugin/marketplace.json"));
    let mut out = vec![];
    for p in m["plugins"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str()) {
        for scope in ["local", "project", "user"] {
            let r = Command::new("claude").args(["plugin", "uninstall", &format!("{p}@{mkt}"), "--scope", scope]).current_dir(project).stdin(Stdio::null()).output();
            if let Ok(r) = r { if r.status.success() { out.push(format!("uninstalled {p}@{mkt} (scope {scope})")); } }
        }
    }
    if let Ok(r) = Command::new("claude").args(["plugin", "marketplace", "remove", &mkt]).stdin(Stdio::null()).output() { if r.status.success() { out.push(format!("removed marketplace {mkt}")); } }
    let cache = tools::config_dir().join("plugins/cache").join(&mkt);
    if cache.exists() && fs::remove_dir_all(&cache).is_ok() { out.push(format!("removed plugin cache {}", cache.display())); }
    let _ = fs::remove_dir_all(project.join(".mt/marketplace"));
    out
}

// ---------------------------------------------------------------- hook gate

fn b64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { if i <= c.len() { o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char); } else { o.push('='); } }
    }
    o
}

pub fn unb64(s: &str) -> String {
    let val = |c: u8| -> u32 { match c { b'A'..=b'Z' => (c - b'A') as u32, b'a'..=b'z' => (c - b'a' + 26) as u32, b'0'..=b'9' => (c - b'0' + 52) as u32, b'+' => 62, b'/' => 63, _ => 0 } };
    let b: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut o = vec![];
    for c in b.chunks(4) {
        let n = c.iter().enumerate().fold(0u32, |a, (i, x)| a | val(*x) << (18 - 6 * i));
        for i in 0..c.len().saturating_sub(1) { o.push((n >> (16 - 8 * i) & 255) as u8); }
    }
    String::from_utf8_lossy(&o).into_owned()
}

/// Put every command of a hooks object (`{"hooks": {Event: [{matcher, hooks: [{command}]}]}}` or the
/// bare event map) behind the gate. Commands already gated are left alone.
pub fn wrap_hooks(v: &mut Value, mt: &str) -> usize {
    let mut n = 0;
    let events = if v.get("hooks").is_some_and(|h| h.is_object()) { v.get_mut("hooks").unwrap() } else { v };
    let Some(m) = events.as_object_mut() else { return 0 };
    for (event, groups) in m.iter_mut() {
        for g in groups.as_array_mut().into_iter().flatten() {
            for h in g.get_mut("hooks").and_then(|x| x.as_array_mut()).into_iter().flatten() {
                let Some(cmd) = h.get("command").and_then(|c| c.as_str()).map(String::from) else { continue };
                if cmd.contains(" tools gate ") { continue; }
                h["command"] = json!(format!("\"{mt}\" tools gate --event {event} --cmd-b64 {}", b64(&cmd)));
                n += 1;
            }
        }
    }
    n
}

fn hash(s: &str) -> String { format!("{:016x}", crate::fnv1a(s.as_bytes())) }

/// Run one hook command exactly as Claude Code would, then decide about its context text only.
pub fn gate(event: &str, cmd: &str) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let t = std::time::Instant::now();
    let child = Command::new("sh").arg("-c").arg(cmd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let Ok(mut child) = child else { eprintln!("mt gate: cannot run {cmd}"); return 1 };
    if let Some(mut si) = child.stdin.take() { let _ = si.write_all(input.as_bytes()); }
    let out = child.wait_with_output();
    let Ok(out) = out else { return 1 };
    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let _ = std::io::stderr().write_all(&out.stderr);
    let mut action = "pass";
    let mut emit = stdout.clone();
    // decisions, blocks and errors are never touched: only context text is filtered
    let parsed: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    let decides = parsed.as_ref().is_some_and(|v| v.get("decision").is_some() || v.get("continue").is_some()
        || v.pointer("/hookSpecificOutput/permissionDecision").is_some() || v.get("systemMessage").is_some());
    if code == 0 && !decides && !stdout.trim().is_empty() && matches!(event, "SessionStart" | "UserPromptSubmit" | "PostToolUse" | "PreToolUse" | "SubagentStart") {
        let text = parsed.as_ref().and_then(|v| v.pointer("/hookSpecificOutput/additionalContext")).and_then(|x| x.as_str()).map(String::from)
            .unwrap_or_else(|| if parsed.is_some() { String::new() } else { stdout.clone() });
        if !text.trim().is_empty() {
            let sid = payload.get("session_id").and_then(|s| s.as_str()).unwrap_or("nosession");
            let seen_path = format!("/tmp/mt-gate-{}.seen", hash(sid));
            let seen = fs::read_to_string(&seen_path).unwrap_or_default();
            let h = hash(text.trim());
            let cwd = payload.get("cwd").and_then(|c| c.as_str()).map(String::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned());
            if seen.lines().any(|l| l == h) {
                action = "dropped: repeat in this session";
            } else if let Some(off) = off_product(&text, Path::new(&tools::repo_root(&cwd))) {
                action = "dropped: off-project";
                emit = String::new();
                log_gate(event, cmd, input.len(), stdout.len(), 0, &format!("{action} ({off})"), t.elapsed().as_millis());
                return code;
            } else {
                let _ = fs::OpenOptions::new().create(true).append(true).open(&seen_path).and_then(|mut f| writeln!(f, "{h}"));
            }
            if action.starts_with("dropped") { emit = String::new(); }
        }
    }
    print!("{emit}");
    log_gate(event, cmd, input.len(), stdout.len(), emit.len(), action, t.elapsed().as_millis());
    code
}

/// The product a text is about, when the code doesn't use it: a product word (a plugin's product
/// or an MCP server name) named at least 3 times, more than any other, with no fact behind it.
fn off_product(text: &str, project: &Path) -> Option<String> {
    let c = tools::open().ok()?;
    let g = Graph::load(&c, tools::facts(project)).ok()?;
    let mut anchors: HashSet<String> = g.product.values().cloned().collect();
    for n in g.nodes.values().filter(|n| n.kind == "mcp") { anchors.extend(g.anchors(n)); }
    let low = text.to_lowercase();
    let mut counts: Vec<(usize, String)> = anchors.into_iter().filter(|a| a.len() >= 4).map(|a| (low.matches(a.as_str()).count(), a)).filter(|x| x.0 >= 3).collect();
    counts.sort_by(|a, b| b.0.cmp(&a.0));
    let (n, top) = counts.first()?.clone();
    if g.facts.evidence(&top, true).is_some() { return None; }
    Some(format!("{top} named {n} times, not in this code"))
}

fn log_gate(event: &str, cmd: &str, inb: usize, outb: usize, kept: usize, action: &str, ms: u128) {
    let line = json!({"event": event, "cmd": cmd.chars().take(240).collect::<String>(), "stdin": inb, "stdout": outb, "kept": kept, "action": action, "ms": ms});
    let _ = fs::OpenOptions::new().create(true).append(true).open(GATE_LOG).and_then(|mut f| writeln!(f, "{line}"));
}

/// Every hook Claude Code will run here (user settings and enabled plugins), and what the gate has
/// measured for each command so far.
pub fn hooks_report(g: &Graph) -> String {
    let mut rows: Vec<(String, String, String)> = vec![]; // (event, source, command)
    let collect = |v: &Value, src: &str, rows: &mut Vec<(String, String, String)>| {
        let events = v.get("hooks").filter(|h| h.is_object()).unwrap_or(v);
        for (event, groups) in events.as_object().into_iter().flatten() {
            for gr in groups.as_array().into_iter().flatten() {
                for h in gr.get("hooks").and_then(|x| x.as_array()).into_iter().flatten() {
                    if let Some(c) = h.get("command").and_then(|c| c.as_str()) { rows.push((event.clone(), src.to_string(), c.to_string())); }
                }
            }
        }
    };
    collect(&read_json(&tools::config_dir().join("settings.json")), "user settings", &mut rows);
    for p in g.nodes.values().filter(|n| n.kind == "plugin") {
        let extra: Value = serde_json::from_str(&p.extra).unwrap_or(Value::Null);
        if !extra.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) { continue; }
        collect(&read_json(&Path::new(&p.path).join("hooks/hooks.json")), &format!("plugin {}", p.name), &mut rows);
    }
    let mut stats: HashMap<String, (usize, usize, usize, usize)> = HashMap::new(); // calls, out, kept, dropped
    for l in fs::read_to_string(GATE_LOG).unwrap_or_default().lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let e = stats.entry(v["cmd"].as_str().unwrap_or("").to_string()).or_default();
        e.0 += 1; e.1 += v["stdout"].as_u64().unwrap_or(0) as usize; e.2 += v["kept"].as_u64().unwrap_or(0) as usize;
        if v["action"].as_str().unwrap_or("").starts_with("dropped") { e.3 += 1; }
    }
    rows.sort();
    let mut out = format!("{} hook commands\n", rows.len());
    for (event, src, cmd) in &rows {
        let inner = cmd.split("--cmd-b64 ").nth(1).map(unb64).unwrap_or_else(|| cmd.clone());
        let gated = cmd.contains(" tools gate ");
        let st = stats.get(&inner.chars().take(240).collect::<String>()).map(|s| format!("{} calls, {} chars out, {} kept, {} dropped", s.0, s.1, s.2, s.3)).unwrap_or_else(|| "not measured (not gated yet)".into());
        out += &format!("  {event:18} {src:28} {}{}\n      {st}\n", if gated { "[gated] " } else { "" }, inner.chars().take(110).collect::<String>());
    }
    out
}

/// Put a settings file's hooks behind the gate, keeping `<file>.mt-backup` for undo.
pub fn wrap_settings(file: &Path, mt: &str) -> Result<String, String> {
    let src = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut v: Value = serde_json::from_str(&src).map_err(|e| e.to_string())?;
    let n = wrap_hooks(&mut v, mt);
    let backup = PathBuf::from(format!("{}.mt-backup", file.display()));
    if !backup.exists() { fs::write(&backup, &src).map_err(|e| e.to_string())?; }
    fs::write(file, serde_json::to_string_pretty(&v).unwrap() + "\n").map_err(|e| e.to_string())?;
    Ok(format!("{n} hook commands in {} now run behind mt tools gate (backup {})", file.display(), backup.display()))
}

// ---------------------------------------------------------------- profile refresh at session start

/// Recompute the profile with the options it was written with; apply what changed. Returns one
/// line per change, for the session's context.
pub fn refresh(project: &Path, g: &Graph) -> Vec<String> {
    let saved = read_json(&project.join(".mt/tools-profile.json"));
    if saved.is_null() { return vec![]; }
    let opts = tools::ProfileOpts {
        keep: saved["opts"]["keep"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(),
        include_hooked: saved["opts"]["include_hooked"].as_bool().unwrap_or(false),
        keep_descriptions: saved["opts"]["keep_descriptions"].as_bool().unwrap_or(false),
    };
    let (new, log) = tools::profile(g, &opts);
    let old = &saved["layer"];
    let flat = |v: &Value| -> HashSet<String> {
        let mut s = HashSet::new();
        for (k, x) in v.get("enabledPlugins").and_then(|x| x.as_object()).into_iter().flatten() { if x == &json!(false) { s.insert(format!("plugin {k}")); } }
        for k in v.get("skillOverrides").and_then(|x| x.as_object()).into_iter().flatten().map(|x| x.0) { s.insert(format!("skill {k}")); }
        for d in v.pointer("/permissions/deny").and_then(|x| x.as_array()).into_iter().flatten() { if let Some(d) = d.as_str() { s.insert(format!("deny {d}")); } }
        for d in v.get("deniedMcpServers").and_then(|x| x.as_array()).into_iter().flatten() { if let Some(d) = d["serverName"].as_str() { s.insert(format!("mcp {d}")); } }
        s
    };
    let (a, b) = (flat(old), flat(&new));
    let back_on: Vec<&String> = a.difference(&b).collect();
    let now_off: Vec<&String> = b.difference(&a).collect();
    if back_on.is_empty() && now_off.is_empty() { return vec![]; }
    // take back what mt turned off and the code now needs, then merge the new layer
    let path = project.join(".claude/settings.local.json");
    let mut cur = read_json(&path);
    if cur.is_null() { cur = json!({}); }
    for item in &back_on {
        let (kind, name) = item.split_once(' ').unwrap();
        match kind {
            "plugin" => { if let Some(m) = cur.get_mut("enabledPlugins").and_then(|x| x.as_object_mut()) { m.remove(name); } }
            "skill" => { if let Some(m) = cur.get_mut("skillOverrides").and_then(|x| x.as_object_mut()) { m.remove(name); } }
            "deny" => { if let Some(a) = cur.pointer_mut("/permissions/deny").and_then(|x| x.as_array_mut()) { a.retain(|x| x.as_str() != Some(name)); } }
            _ => { if let Some(a) = cur.get_mut("deniedMcpServers").and_then(|x| x.as_array_mut()) { a.retain(|x| x["serverName"].as_str() != Some(name)); } }
        }
    }
    let _ = fs::write(&path, serde_json::to_string_pretty(&cur).unwrap() + "\n");
    let _ = tools::write_profile(project, &new, &[]);
    save_profile(project, &new, &opts);
    let why = |item: &str| -> String {
        let name = item.split_once(' ').map(|x| x.1).unwrap_or(item);
        let short = name.split('@').next().unwrap_or(name);
        log.iter().find(|l| l.contains(short)).map(|l| l.split(" — ").skip(1).collect::<Vec<_>>().join(" — ")).unwrap_or_default()
    };
    let mut lines: Vec<String> = back_on.iter().map(|i| format!("back on: {i} — {}", why(i))).collect();
    lines.extend(now_off.iter().map(|i| format!("now off: {i} — {}", why(i))));
    lines
}

pub fn save_profile(project: &Path, layer: &Value, o: &tools::ProfileOpts) {
    let _ = fs::create_dir_all(project.join(".mt"));
    let v = json!({"layer": layer, "opts": {"keep": o.keep, "include_hooked": o.include_hooked, "keep_descriptions": o.keep_descriptions}});
    let _ = fs::write(project.join(".mt/tools-profile.json"), serde_json::to_string_pretty(&v).unwrap());
}

// ---------------------------------------------------------------- MCP tool names, live

/// PostToolUse: record the MCP tools this session called or that ToolSearch returned, so servers
/// mt can't probe (HTTP, OAuth) still get tool nodes.
pub fn capture(payload: &Value) -> usize {
    let mut names: HashSet<String> = HashSet::new();
    let rx = regex::Regex::new(r"mcp__[A-Za-z0-9_\-]+__[A-Za-z0-9_\-]+").unwrap();
    if let Some(t) = payload.get("tool_name").and_then(|t| t.as_str()) { if t.starts_with("mcp__") { names.insert(t.to_string()); } }
    if payload.get("tool_name").and_then(|t| t.as_str()) == Some("ToolSearch") {
        for m in rx.find_iter(&payload.get("tool_response").map(|r| r.to_string()).unwrap_or_default()) { names.insert(m.as_str().to_string()); }
    }
    if names.is_empty() { return 0; }
    let Ok(c) = tools::open() else { return 0 };
    let _ = c.execute_batch("CREATE TABLE IF NOT EXISTS seen_mcp(tool TEXT PRIMARY KEY, n INTEGER)");
    for n in &names { let _ = c.execute("INSERT INTO seen_mcp VALUES(?1, 1) ON CONFLICT(tool) DO UPDATE SET n = n + 1", params![n]); }
    names.len()
}

// ---------------------------------------------------------------- explain one request

/// A session's first request taken apart: every listing with its size, and each listed skill, agent
/// and MCP server with what mt's profile says about it. The transcript is read only to show what
/// was sent; nothing in the graph comes from it.
pub fn explain(project: &Path, g: &Graph, session: Option<&Path>) -> Result<String, String> {
    let file = match session {
        Some(f) => f.to_path_buf(),
        None => {
            let enc: String = project.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            let dir = tools::config_dir().join("projects").join(enc);
            let mut v: Vec<PathBuf> = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
            v.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
            v.pop().ok_or("no sessions recorded for this project")?
        }
    };
    let (layer, _) = tools::profile(g, &tools::ProfileOpts { keep: vec![], include_hooked: true, keep_descriptions: true });
    let off_plugins: HashSet<String> = layer.get("enabledPlugins").and_then(|x| x.as_object()).map(|m| m.keys().map(|k| k.split('@').next().unwrap_or(k).to_string()).collect()).unwrap_or_default();
    let off_skills: HashSet<String> = layer.get("skillOverrides").and_then(|x| x.as_object()).map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let mut sizes: BTreeMap<String, usize> = BTreeMap::new();
    let mut first_tokens = 0u64;
    let mut skills_listed: Vec<String> = vec![];
    let mut agents_listed: Vec<String> = vec![];
    let mut servers: Vec<(String, usize)> = vec![];
    for l in fs::read_to_string(&file).map_err(|e| e.to_string())?.lines() {
        let Ok(d) = serde_json::from_str::<Value>(l) else { continue };
        if d["type"] == "assistant" && first_tokens == 0 {
            let u = &d["message"]["usage"];
            first_tokens = u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        }
        if d["type"] == "api-request-shape" && !sizes.contains_key("tool schemas") {
            sizes.insert("tool schemas".into(), d["shape"]["tools"].to_string().len());
            sizes.insert("system prompt".into(), d["shape"]["system"].to_string().len());
        }
        let a = &d["attachment"];
        match a["type"].as_str().unwrap_or("") {
            "skill_listing" if skills_listed.is_empty() => {
                let c = a["content"].as_str().unwrap_or("");
                sizes.insert("skill listing".into(), c.len());
                skills_listed = c.split("\n- ").map(|x| x.trim_start_matches("- ").split(": ").next().unwrap_or("").to_string()).collect();
            }
            "agent_listing_delta" if agents_listed.is_empty() => {
                sizes.insert("agent listing".into(), a.to_string().len());
                agents_listed = a["addedTypes"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
            }
            "deferred_tools_delta" if !sizes.contains_key("deferred tool names") => { sizes.insert("deferred tool names".into(), a["addedNames"].to_string().len()); }
            "mcp_instructions_delta" if servers.is_empty() => {
                for (n, b) in a["addedNames"].as_array().into_iter().flatten().zip(a["addedBlocks"].as_array().into_iter().flatten()) {
                    servers.push((n.as_str().unwrap_or("").to_string(), b.as_str().unwrap_or("").len()));
                }
                sizes.insert("MCP instructions".into(), servers.iter().map(|x| x.1).sum());
            }
            "hook_success" | "hook_additional_context" => {
                let k = format!("hook text ({})", a["hookName"].as_str().unwrap_or("?").split(':').next().unwrap_or("?"));
                *sizes.entry(k).or_default() += a["content"].to_string().len();
            }
            _ => {}
        }
    }
    let mut out = format!("{}\nfirst request: {first_tokens} tokens\n\n", file.display());
    for (k, v) in &sizes { out += &format!("  {k:34} {v:>8} chars\n"); }
    let verdict = |name: &str, kind: &str| -> String {
        let plugin = name.split(':').next().unwrap_or("");
        if name.contains(':') && off_plugins.contains(plugin) { return format!("off (plugin {plugin}: profile)"); }
        if kind == "skill" && off_skills.contains(name) { return "off (skillOverrides: profile)".into(); }
        match g.resolve(name) {
            Some(n) => match g.grounded(n) { Some((a, e)) => format!("on — {a} in code: {e}"), None if g.anchors(n).is_empty() => "on — general-purpose".into(), None => format!("off — no {} in the code", g.anchors(n).join("/")) },
            None => "unknown to mt (bundled or remote)".into(),
        }
    };
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    out += "\nskills listed:\n";
    for s in skills_listed.iter().filter(|s| !s.is_empty()) { let v = verdict(s, "skill"); *tally.entry(format!("skill {}", v.split(' ').next().unwrap_or(""))).or_default() += 1; out += &format!("  {s:44} {v}\n"); }
    out += "\nagents listed:\n";
    for s in &agents_listed { let v = verdict(s, "agent"); *tally.entry(format!("agent {}", v.split(' ').next().unwrap_or(""))).or_default() += 1; out += &format!("  {s:44} {v}\n"); }
    out += "\nMCP instructions:\n";
    for (s, n) in &servers { out += &format!("  {s:30} {n:>6} chars  {}\n", verdict(s, "mcp")); }
    out += &format!("\ntally: {}\n", tally.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", "));
    Ok(out)
}

// ---------------------------------------------------------------- other agents

/// Wire the graph into Codex (a UserPromptSubmit hook in `.codex/hooks.json`, same schema as
/// Claude Code's) and Cursor (a rule, since Cursor's prompt hook can't add context).
pub fn agents(project: &Path, mt: &str) -> Result<Vec<String>, String> {
    let mut done = vec![];
    let codex = project.join(".codex/hooks.json");
    let mut v = read_json(&codex);
    if v.is_null() { v = json!({"hooks": {}}); }
    let arr = v["hooks"]["UserPromptSubmit"].as_array().cloned().unwrap_or_default();
    if !arr.iter().any(|h| h.to_string().contains("tools hook")) {
        let mut arr = arr;
        arr.push(json!({"hooks": [{"type": "command", "command": format!("\"{mt}\" tools hook"), "timeout": 10}]}));
        v["hooks"]["UserPromptSubmit"] = Value::Array(arr);
        fs::create_dir_all(project.join(".codex")).map_err(|e| e.to_string())?;
        fs::write(&codex, serde_json::to_string_pretty(&v).unwrap() + "\n").map_err(|e| e.to_string())?;
    }
    done.push(format!("{}: UserPromptSubmit → mt tools hook", codex.display()));
    let rule = project.join(".cursor/rules/mt-tools.mdc");
    fs::create_dir_all(rule.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&rule, format!("---\ndescription: Find the code, tests, data and tools a task needs before reading files\nalwaysApply: true\n---\n\nBefore reading or searching files for a task, run `\"{mt}\" tools find \"<the task in your words>\"` in the terminal. It returns, from mt's graph of this repo: the definitions involved with their tests, callers, tables and migrations, the HTTP routes that link frontend calls to their handlers, completeness claims (✓ exhaustive), and for a change, an `mt apply` plan with hash-checked spans. Lines marked \"read from disk now\" are current. Use them instead of re-reading.\n")).map_err(|e| e.to_string())?;
    done.push(format!("{}: always-on rule to run mt tools find first", rule.display()));
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_round_trips_hook_commands() {
        for s in ["node \"${CLAUDE_PLUGIN_ROOT}/hooks/x.cjs\" session-start", "a", "ab", "abc", "python3 -c 'print(1)' | tee /tmp/x"] {
            assert_eq!(unb64(&b64(s)), s);
        }
    }

    #[test]
    fn wrap_hooks_gates_each_command_once() {
        let mut v = json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}], "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}, {"type": "command", "command": "y"}]}]}});
        assert_eq!(wrap_hooks(&mut v, "/bin/mt"), 3);
        let c = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap().to_string();
        assert!(c.starts_with("\"/bin/mt\" tools gate --event SessionStart --cmd-b64 "), "{c}");
        assert_eq!(unb64(c.split("--cmd-b64 ").nth(1).unwrap()), "echo hi");
        assert_eq!(wrap_hooks(&mut v, "/bin/mt"), 0, "already gated commands are left alone");
        assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], json!("Bash"), "matchers kept");
    }
}
