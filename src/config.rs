//! `mt config`: every knob mt has, in one place. Each knob is an environment variable the code
//! already reads; config files only fill in the variables you did not set yourself, at the very
//! start of every mt process (CLI, daemon and hooks alike), so nothing else in mt knows config
//! files exist.
//!
//! Precedence, highest first: a `locked` project config > the environment > the project config
//! (`<repo>/.mt/config.json`) > the user config (`$XDG_CONFIG_HOME/mt/config.json`, else
//! `~/.config/mt/config.json`) > the built-in default.
//!
//!   mt config                      guided setup (on a terminal), else the same as `list`
//!   mt config list [--json]        every knob: value, where it came from, what it does
//!   mt config get KEY | set KEY VALUE | unset KEY     (--global: the user file)
//!   mt config preset lockdown|balanced|nudge|off      (--global)
//!   mt config env                  the effective settings as `export` lines
//!   mt config doctor               binary, plugin, daemon, index, hooks: what works and what doesn't
//!   mt config path                 where the files are

use serde_json::{json, Map, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Kind {
    /// One of these values, written to the variable as is.
    Choice(&'static [&'static str]),
    /// A whole number.
    Num,
    /// Free text or a path.
    Text,
    /// On/off. Each side says what the variable must hold for it (None = unset).
    Switch { on: Option<&'static str>, off: Option<&'static str> },
}

pub struct Knob {
    pub key: &'static str,
    pub env: &'static str,
    pub group: &'static str,
    pub kind: Kind,
    /// Shown default ("on"/"off" for switches).
    pub default: &'static str,
    pub help: &'static str,
}

const ON_UNSET_OFF_0: Kind = Kind::Switch { on: None, off: Some("0") };
const ON_UNSET_OFF_1: Kind = Kind::Switch { on: None, off: Some("1") };
const OFF_UNSET_ON_1: Kind = Kind::Switch { on: Some("1"), off: None };

pub const KNOBS: &[Knob] = &[
    // ---- the Claude Code plugin
    Knob { key: "enforce", env: "MT_ENFORCE", group: "Agent hooks", kind: Kind::Choice(&["strict", "nudge", "off"]), default: "strict",
        help: "What the PreToolUse hook does with an old-style code read (sed -n ranges, cat/head of code, identifier greps). strict: rewrite or deny it with the mt command. nudge: allow it and add a hint. off: never interfere." },
    Knob { key: "rewrite", env: "MT_REWRITE", group: "Agent hooks", kind: ON_UNSET_OFF_0, default: "on",
        help: "In strict mode, rewrite a lone sed/grep read into the mt call in place, so it costs no denied turn. Off: deny it instead." },
    Knob { key: "inject_skill", env: "MT_INJECT_SKILL", group: "Agent hooks", kind: ON_UNSET_OFF_0, default: "on",
        help: "Brief the model with the mt reference at session start. Turn off only if the session already carries it (e.g. --append-system-prompt-file)." },
    Knob { key: "server_scope", env: "MT_SERVER_SCOPE", group: "Agent hooks", kind: Kind::Choice(&["session", "repo", "off"]), default: "session",
        help: "session: each Claude session starts its own mt daemon. repo: sessions share one per repository. off: the hook starts none." },
    Knob { key: "compress", env: "MT_COMPRESS", group: "Agent hooks", kind: ON_UNSET_OFF_0, default: "on",
        help: "PostToolUse: shrink large Bash, Read and MCP results (logs folded, JSON turned into a queryable graph). The original is kept for `mt out`." },
    Knob { key: "compress_lines", env: "MT_COMPRESS_LINES", group: "Agent hooks", kind: Kind::Num, default: "120",
        help: "Line budget for a compressed tool result. Higher keeps more text, lower saves more context." },
    // ---- the daemon
    Knob { key: "daemon", env: "MT_NO_DAEMON", group: "Daemon", kind: ON_UNSET_OFF_1, default: "on",
        help: "Answer from the in-memory daemon (graph kept live by a file watcher). Off: every call opens the SQLite index itself (slower, no background process)." },
    Knob { key: "idle_minutes", env: "MT_IDLE_MIN", group: "Daemon", kind: Kind::Num, default: "30",
        help: "The daemon exits after this many idle minutes." },
    // ---- what replies contain
    Knob { key: "max_output", env: "MT_MAX_OUTPUT", group: "Replies", kind: Kind::Num, default: "28000",
        help: "Hard cap on one reply, in characters. Claude Code spills tool results over ~30k to a file, which costs a turn." },
    Knob { key: "fold", env: "MT_NO_FOLD", group: "Replies", kind: ON_UNSET_OFF_1, default: "on",
        help: "Fold long caller/test bodies to their signature and the lines that reference the target." },
    Knob { key: "expand_hints", env: "MT_NO_EXPAND", group: "Replies", kind: ON_UNSET_OFF_1, default: "on",
        help: "Number the pointers in replies and add the `mt expand <n>` footer." },
    Knob { key: "line_dedupe", env: "MT_LINE_DEDUPE", group: "Replies", kind: ON_UNSET_OFF_0, default: "on",
        help: "Don't resend lines this session has already been shown unchanged." },
    // ---- kept output
    Knob { key: "out_dir", env: "MT_OUT_DIR", group: "Kept output", kind: Kind::Text, default: "<repo>/.mt/out",
        help: "Where kept outputs (`mt run`, `mt compress`, the hook) are stored." },
    Knob { key: "out_name", env: "MT_OUT_NAME", group: "Kept output", kind: Kind::Text, default: "(from the command)",
        help: "Store name for output nobody named, so one agent's data never mixes with another's." },
    Knob { key: "jq_seconds", env: "MT_JQ_SECONDS", group: "Kept output", kind: Kind::Num, default: "10",
        help: "A `mt out --jq` filter running longer than this is stopped." },
    // ---- mt tools: skill/agent/MCP ranking
    Knob { key: "tools_db", env: "MT_TOOLS_DB", group: "Tools graph", kind: Kind::Text, default: "~/.cache/mt/tools.sqlite",
        help: "The skills/agents/MCP catalog database." },
    Knob { key: "embed", env: "MT_EMBED", group: "Tools graph", kind: Kind::Choice(&["text", "graph", "off"]), default: "text",
        help: "Embeddings in tool ranking. text: plain text vectors (best on held-out prompts). graph: vectors propagated over code edges. off: words only." },
    Knob { key: "embed_model", env: "MT_EMBED_MODEL", group: "Tools graph", kind: Kind::Text, default: "~/.cache/mt/models/potion-base-8M",
        help: "Embedding model directory (Model2Vec, or bge-small when built with --features bert)." },
    Knob { key: "chain_cards", env: "MT_CHAIN", group: "Tools graph", kind: Kind::Switch { on: None, off: Some("off") }, default: "on",
        help: "For prompts that span layers, add a chain card: client call -> route -> handler -> store -> tables." },
    Knob { key: "wire_keys", env: "MT_WIRE", group: "Tools graph", kind: Kind::Switch { on: None, off: Some("off") }, default: "on",
        help: "Link sub-projects by shared literals (routes, event names, keys)." },
    // ---- diagnostics
    Knob { key: "timing", env: "MT_TIMING", group: "Diagnostics", kind: OFF_UNSET_ON_1, default: "off",
        help: "Print per-stage timings (daemon log and stderr)." },
    Knob { key: "debug", env: "MT_DEBUG", group: "Diagnostics", kind: OFF_UNSET_ON_1, default: "off",
        help: "Verbose parser and query diagnostics." },
    Knob { key: "hook_debug", env: "MT_HOOK_DEBUG", group: "Diagnostics", kind: OFF_UNSET_ON_1, default: "off",
        help: "Log every PostToolUse payload the hook sees." },
];

/// Presets: (name, what it is for, values). Keys left out fall back to the default.
pub const PRESETS: &[(&str, &str, &[(&str, &str)])] = &[
    ("lockdown", "Every code read and output goes through mt, and the project config can't be overridden by env or user config. For teams that want the measured savings on every session.",
        &[("enforce", "strict"), ("rewrite", "on"), ("inject_skill", "on"), ("compress", "on"), ("daemon", "on"), ("server_scope", "session"), ("fold", "on"), ("expand_hints", "on"), ("line_dedupe", "on")]),
    ("balanced", "The defaults: strict hooks with in-place rewrites, compression on.", &[]),
    ("nudge", "mt suggests but never blocks: old-style reads run with a hint. For trying mt out.",
        &[("enforce", "nudge"), ("compress", "on")]),
    ("off", "Hooks don't interfere and no daemon starts. The CLI still works when called directly.",
        &[("enforce", "off"), ("compress", "off"), ("server_scope", "off"), ("inject_skill", "off")]),
];

pub fn knob(key: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|k| k.key == key || k.env.eq_ignore_ascii_case(key))
}

fn user_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("mt/config.json")
}

fn project_path(root: &Path) -> PathBuf { root.join(".mt/config.json") }

fn root() -> PathBuf { crate::index::find_root(&std::env::current_dir().unwrap_or_default()) }

fn load(p: &Path) -> Map<String, Value> {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn save(p: &Path, m: &Map<String, Value>) -> Result<(), String> {
    if let Some(d) = p.parent() { std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?; }
    let s = serde_json::to_string_pretty(&Value::Object(m.clone())).unwrap_or_default();
    std::fs::write(p, s + "\n").map_err(|e| format!("{}: {e}", p.display()))
}

fn as_str(v: &Value) -> String {
    match v { Value::String(s) => s.clone(), Value::Bool(b) => if *b { "on".into() } else { "off".into() }, other => other.to_string() }
}

/// Check a value against its knob and normalise it ("true"/"1"/"yes" -> "on").
pub fn validate(k: &Knob, v: &str) -> Result<String, String> {
    let v = v.trim();
    match k.kind {
        Kind::Choice(cs) => cs.iter().find(|c| c.eq_ignore_ascii_case(v)).map(|c| c.to_string())
            .ok_or_else(|| format!("{}: expected one of {}", k.key, cs.join(" | "))),
        Kind::Num => v.parse::<u64>().map(|n| n.to_string()).map_err(|_| format!("{}: expected a whole number", k.key)),
        Kind::Text => if v.is_empty() { Err(format!("{}: empty value (use `unset`)", k.key)) } else { Ok(v.to_string()) },
        Kind::Switch { .. } => match v.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok("on".into()),
            "off" | "false" | "no" | "0" => Ok("off".into()),
            _ => Err(format!("{}: expected on | off", k.key)),
        },
    }
}

/// The variable's value for a configured value; None = leave it unset.
fn env_value(k: &Knob, v: &str) -> Option<String> {
    match k.kind {
        Kind::Switch { on, off } => (if v == "on" { on } else { off }).map(String::from),
        _ => Some(v.to_string()),
    }
}

/// The knob's current value as the code will see it, from its variable.
fn from_env(k: &Knob) -> Option<String> {
    let raw = std::env::var(k.env).ok();
    match k.kind {
        Kind::Switch { on, off } => {
            let is = |side: Option<&str>| match (side, raw.as_deref()) { (None, None) => true, (Some(a), Some(b)) => a == b, _ => false };
            if raw.is_none() { return None; }
            // a variable that is set but matches neither side: report the side the code takes
            Some(if is(off) { "off" } else if is(on) { "on" } else if off.is_some() && on.is_none() { "on" } else { "off" }.to_string())
        }
        _ => raw,
    }
}

/// Called first thing in `main`, before any thread starts: set every variable a config file
/// names and the environment does not (a locked project file wins over the environment too).
/// Records where each value came from in MT_CONFIG_SOURCES so child processes report it right.
pub fn apply_env() {
    // a child mt in the same repo inherits the result; one in another repo applies its own
    let r = root();
    let tag = r.display().to_string();
    if std::env::var("MT_CONFIG_APPLIED").as_deref() == Ok(tag.as_str()) { return; }
    let user = load(&user_path());
    let proj = load(&project_path(&r));
    let locked = proj.get("locked").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut sources = Vec::new();
    for k in KNOBS {
        let env_set = std::env::var_os(k.env).is_some();
        let (val, src) = match (proj.get(k.key), user.get(k.key)) {
            (Some(p), _) if locked => (Some(as_str(p)), "project (locked)"),
            _ if env_set => (None, "env"),
            (Some(p), _) => (Some(as_str(p)), "project"),
            (None, Some(u)) => (Some(as_str(u)), "user"),
            (None, None) => (None, "default"),
        };
        if let Some(v) = val.and_then(|v| validate(k, &v).ok()) {
            // SAFETY: called at the top of main, before any thread is spawned
            match env_value(k, &v) {
                Some(e) => unsafe { std::env::set_var(k.env, e) },
                None => unsafe { std::env::remove_var(k.env) },
            }
            sources.push(format!("{}={src}", k.key));
        } else if env_set {
            sources.push(format!("{}=env", k.key));
        }
    }
    unsafe {
        std::env::set_var("MT_CONFIG_APPLIED", &tag);
        std::env::set_var("MT_CONFIG_SOURCES", sources.join(","));
    }
}

fn source_of(key: &str) -> String {
    std::env::var("MT_CONFIG_SOURCES").ok().and_then(|s| s.split(',').find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(String::from)))
        .unwrap_or_else(|| "default".into())
}

fn effective(k: &Knob) -> String { from_env(k).unwrap_or_else(|| k.default.to_string()) }

// ---------------------------------------------------------------- the commands

pub fn list(json_out: bool) -> String {
    if json_out {
        let v: Vec<Value> = KNOBS.iter().map(|k| json!({"key": k.key, "env": k.env, "group": k.group, "value": effective(k), "source": source_of(k.key), "default": k.default, "help": k.help})).collect();
        return serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
    }
    let mut out = String::new();
    let mut group = "";
    for k in KNOBS {
        if k.group != group {
            group = k.group;
            out += &format!("\n{group}\n");
        }
        let v = effective(k);
        let src = source_of(k.key);
        let mark = if src == "default" { " " } else { "*" };
        out += &format!("  {mark} {:<15} {:<12} {:<18} {}\n", k.key, v, format!("[{src}]"), k.env);
    }
    out += &format!("\n  * = set by a config file or the environment.  Files: {} and {}\n  `mt config get <key>` explains one; `mt config` walks through all of them.\n",
        project_path(&root()).display(), user_path().display());
    out
}

pub fn get(key: &str) -> Result<String, String> {
    let k = knob(key).ok_or_else(|| unknown(key))?;
    let choices = match k.kind { Kind::Choice(c) => c.join(" | "), Kind::Num => "a number".into(), Kind::Text => "text or a path".into(), Kind::Switch { .. } => "on | off".into() };
    Ok(format!("{} = {}  [{}]\n  variable: {}\n  default:  {}\n  values:   {choices}\n  {}\n", k.key, effective(k), source_of(k.key), k.env, k.default, k.help))
}

fn unknown(key: &str) -> String {
    let near: Vec<&str> = KNOBS.iter().map(|k| k.key).filter(|n| n.contains(key) || key.contains(n)).collect();
    format!("no setting named {key:?}{}; `mt config list` shows them all", if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.join(", ")) })
}

fn file_for(global: bool) -> PathBuf { if global { user_path() } else { project_path(&root()) } }

pub fn set(key: &str, val: &str, global: bool) -> Result<String, String> {
    if key == "locked" {
        let p = file_for(false);
        let mut m = load(&p);
        let on = validate(&Knob { key: "locked", env: "", group: "", kind: ON_UNSET_OFF_0, default: "off", help: "" }, val)? == "on";
        m.insert("locked".into(), Value::Bool(on));
        save(&p, &m)?;
        return Ok(format!("locked = {} in {}\n", if on { "on" } else { "off" }, p.display()));
    }
    let k = knob(key).ok_or_else(|| unknown(key))?;
    let v = validate(k, val)?;
    let p = file_for(global);
    let mut m = load(&p);
    m.insert(k.key.into(), Value::String(v.clone()));
    save(&p, &m)?;
    Ok(format!("{} = {v} in {}{}\n", k.key, p.display(), restart_note()))
}

pub fn unset(key: &str, global: bool) -> Result<String, String> {
    let p = file_for(global);
    let mut m = load(&p);
    let name = if key == "locked" { "locked" } else { knob(key).ok_or_else(|| unknown(key))?.key };
    let had = m.remove(name).is_some();
    save(&p, &m)?;
    Ok(if had { format!("{name} removed from {}{}\n", p.display(), restart_note()) } else { format!("{name} was not set in {}\n", p.display()) })
}

fn restart_note() -> &'static str {
    "\n  (new mt calls see it now; a running daemon keeps its settings until it restarts: `mt serve --stop`)"
}

pub fn preset(name: &str, global: bool) -> Result<String, String> {
    let (n, what, vals) = PRESETS.iter().find(|(n, ..)| *n == name)
        .ok_or_else(|| format!("no preset {name:?}; presets: {}", PRESETS.iter().map(|p| p.0).collect::<Vec<_>>().join(", ")))?;
    let p = file_for(global);
    let mut m = load(&p);
    for k in KNOBS { if matches!(k.group, "Agent hooks" | "Daemon" | "Replies") { m.remove(k.key); } }
    for (k, v) in vals.iter() { m.insert(k.to_string(), Value::String(v.to_string())); }
    if !global {
        if *n == "lockdown" { m.insert("locked".into(), Value::Bool(true)); } else { m.remove("locked"); }
    }
    save(&p, &m)?;
    Ok(format!("preset {n} written to {}: {what}{}\n", p.display(), restart_note()))
}

pub fn env_lines() -> String {
    KNOBS.iter().map(|k| match std::env::var(k.env) {
        Ok(v) => format!("export {}={}\n", k.env, shell_quote(&v)),
        Err(_) => format!("unset {}  # {} = {}\n", k.env, k.key, k.default),
    }).collect()
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:~".contains(c)) { s.into() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

pub fn paths() -> String {
    let r = root();
    format!("project: {}\nuser:    {}\nrepo:    {}\nevents:  {}\nhook log: {}\n", project_path(&r).display(), user_path().display(), r.display(), r.join(".mt/events.jsonl").display(), r.join(".mt/hooks.log").display())
}

/// What works and what doesn't, each line with the fix.
pub fn doctor() -> String {
    let mut out = String::new();
    let mut line = |ok: bool, what: String, fix: &str| {
        out += &format!("  {} {what}{}\n", if ok { "ok  " } else { "FAIL" }, if ok || fix.is_empty() { String::new() } else { format!("\n         fix: {fix}") });
    };
    let exe = std::env::current_exe().ok();
    line(true, format!("mt {} at {}", env!("CARGO_PKG_VERSION"), exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default()), "");
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join("mt").is_file())).unwrap_or(false);
    line(on_path, "mt on PATH".into(), "copy or link target/release/mt into a directory on PATH (e.g. ~/.local/bin)");
    let r = root();
    let is_repo = r.join(".git").exists() || r.join(".mt").is_dir();
    line(is_repo, format!("repository root {}", r.display()), "run inside a git repository, or `mt index` to make this directory one for mt");
    let idx = r.join(".mt/index.sqlite");
    line(idx.is_file(), format!("index {}", idx.display()), "`mt index` (the daemon also builds it on first use)");
    #[cfg(unix)]
    {
        let sock = std::env::var_os("MT_SOCKET").map(PathBuf::from).unwrap_or_else(|| crate::serve::socket_path(&r));
        let up = std::os::unix::net::UnixStream::connect(&sock).is_ok();
        line(up, format!("daemon socket {}", sock.display()), "starts on the first mt call; `mt serve` runs one in the foreground");
    }
    let hook_log = r.join(".mt/hooks.log");
    let recent = hook_log.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs() < 86_400).unwrap_or(false);
    line(recent, format!("Claude Code plugin hooks fired here in the last 24 h ({})", hook_log.display()), "start Claude with `claude --plugin-dir <mt>/plugin`, or install the plugin; see docs/USAGE.md");
    for k in KNOBS {
        if let Ok(v) = std::env::var(k.env) {
            let shown = from_env(k).unwrap_or(v.clone());
            let ok = validate(k, &shown).is_ok();
            line(ok, format!("{} = {shown} ({})", k.key, source_of(k.key)), "`mt config get` lists the valid values");
        }
    }
    format!("mt doctor\n{out}")
}

// ---------------------------------------------------------------- the guided setup

fn ask(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) { Ok(0) | Err(_) => None, Ok(_) => Some(s.trim().to_string()) }
}

fn choose(title: &str, options: &[(&str, &str)], default: usize) -> Option<usize> {
    println!("\n{title}");
    for (i, (name, what)) in options.iter().enumerate() {
        println!("  {}{}) {name:<10} {what}", if i == default { ">" } else { " " }, i + 1);
    }
    loop {
        let a = ask(&format!("choose 1-{} [{}]: ", options.len(), default + 1))?;
        if a.is_empty() { return Some(default); }
        if let Ok(n) = a.parse::<usize>() { if (1..=options.len()).contains(&n) { return Some(n - 1); } }
        if let Some(i) = options.iter().position(|(n, _)| n.eq_ignore_ascii_case(&a)) { return Some(i); }
        println!("  not an option");
    }
}

pub fn wizard() -> Result<String, String> {
    if !std::io::stdin().is_terminal() { return Ok(list(false)); }
    println!("mt setup. Enter keeps the value shown in [brackets]; Ctrl-D stops without writing.");
    println!("Repository: {}", root().display());
    let scope = choose("Where should the settings live?", &[
        ("project", "this repository only (.mt/config.json)"),
        ("user", "every repository on this machine (~/.config/mt/config.json)"),
    ], 0).ok_or("stopped")?;
    let global = scope == 1;
    let p = file_for(global);
    let mut m = load(&p);

    let opts: Vec<(&str, &str)> = PRESETS.iter().map(|(n, w, _)| (*n, *w)).chain([("custom", "go through every setting one by one")]).collect();
    let pick = choose("Start from a preset:", &opts, 1).ok_or("stopped")?;
    if pick < PRESETS.len() {
        let (n, _, vals) = PRESETS[pick];
        for k in KNOBS { if matches!(k.group, "Agent hooks" | "Daemon" | "Replies") { m.remove(k.key); } }
        for (k, v) in vals.iter() { m.insert(k.to_string(), Value::String(v.to_string())); }
        if !global { if n == "lockdown" { m.insert("locked".into(), Value::Bool(true)); } else { m.remove("locked"); } }
    }
    let go_through = pick == PRESETS.len() || ask("\nFine-tune individual settings too? [y/N]: ").ok_or("stopped")?.eq_ignore_ascii_case("y");
    if go_through {
        let (mut group, mut skip) = ("", false);
        for k in KNOBS {
            if k.group != group {
                group = k.group;
                skip = ask(&format!("\n== {group} == go through this group? [Y/n]: ")).ok_or("stopped")?.eq_ignore_ascii_case("n");
            }
            if skip { continue; }
            let cur = m.get(k.key).map(as_str).unwrap_or_else(|| k.default.to_string());
            let vals = match k.kind { Kind::Choice(c) => format!(" ({})", c.join("/")), Kind::Switch { .. } => " (on/off)".into(), Kind::Num => " (number)".into(), Kind::Text => " (text, '-' to clear)".into() };
            println!("\n  {}  [{}]\n  {}", k.key, k.env, wrap(k.help, 76, "  "));
            loop {
                let a = ask(&format!("  {}{vals} [{cur}]: ", k.key)).ok_or("stopped")?;
                if a.is_empty() { break; }
                if a == "-" { m.remove(k.key); break; }
                match validate(k, &a) {
                    Ok(v) => { if v == k.default { m.remove(k.key); } else { m.insert(k.key.into(), Value::String(v)); } break; }
                    Err(e) => println!("  {e}"),
                }
            }
        }
        if !global {
            let cur = m.get("locked").and_then(|v| v.as_bool()).unwrap_or(false);
            let a = ask(&format!("\n  locked: project values win over env and user config (team lockdown) [{}]: ", if cur { "on" } else { "off" })).ok_or("stopped")?;
            match a.to_ascii_lowercase().as_str() { "on" | "y" | "yes" => { m.insert("locked".into(), Value::Bool(true)); } "off" | "n" | "no" => { m.remove("locked"); } _ => {} }
        }
    }

    println!("\nWill write {}:\n{}", p.display(), serde_json::to_string_pretty(&Value::Object(m.clone())).unwrap_or_default());
    if !ask("Write it? [Y/n]: ").ok_or("stopped")?.eq_ignore_ascii_case("n") {
        save(&p, &m)?;
        println!("written.");
    } else {
        return Ok("nothing written\n".into());
    }

    if !global && ask("\nAdd mt instructions to this repo's CLAUDE.md / AGENTS.md / GEMINI.md / Cursor rules (`mt init`)? [y/N]: ").unwrap_or_default().eq_ignore_ascii_case("y") {
        let (_, s) = crate::init::run(&root(), false, false);
        print!("{s}");
    }
    Ok("\nNext:\n  mt config doctor          check the binary, plugin, daemon and index\n  mt tools profile          see which skills/agents/MCP servers this project never uses (add --write to switch them off)\n  mt visualize              watch the daemon work, live\n  mt serve --stop           restart the daemon so it picks up the new settings\n".to_string())
}

fn wrap(s: &str, w: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for word in s.split_whitespace() {
        if col + word.len() + 1 > w && col > 0 { out += "\n"; out += indent; col = 0; }
        if col > 0 { out += " "; col += 1; }
        out += word;
        col += word.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_valid_and_presets_known() {
        let mut seen = std::collections::HashSet::new();
        for k in KNOBS {
            assert!(seen.insert(k.key), "duplicate key {}", k.key);
            assert!(k.env.starts_with("MT_"), "{}", k.env);
            if !matches!(k.kind, Kind::Text) { assert!(validate(k, k.default).is_ok(), "default of {} is invalid", k.key); }
        }
        for (n, _, vals) in PRESETS {
            for (k, v) in vals.iter() {
                let kn = knob(k).unwrap_or_else(|| panic!("preset {n}: unknown key {k}"));
                assert!(validate(kn, v).is_ok(), "preset {n}: {k}={v}");
            }
        }
    }

    #[test]
    fn switches_map_to_the_variable_the_code_reads() {
        let c = knob("compress").unwrap();
        assert_eq!(env_value(c, "off").as_deref(), Some("0"));
        assert_eq!(env_value(c, "on"), None);
        let f = knob("fold").unwrap();
        assert_eq!(env_value(f, "off").as_deref(), Some("1"));
        assert_eq!(validate(f, "yes").unwrap(), "on");
        assert!(validate(knob("enforce").unwrap(), "loose").is_err());
        assert_eq!(validate(knob("enforce").unwrap(), "NUDGE").unwrap(), "nudge");
    }
}
