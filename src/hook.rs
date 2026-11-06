//! `mt hook <event>`: the Claude Code plugin hooks, in the binary. They run before every Bash,
//! Read and Grep call, so they must cost almost nothing: no interpreter start-up, no
//! subprocesses (definitions come from the in-process parser), and the session's state files
//! come from the same functions `mt query` and `mt expand` use.
//!
//!   session-start  start this session's `mt serve`, export MT_SESSION / MT_SOCKET to every Bash
//!                  call (CLAUDE_ENV_FILE), and brief Claude with the skill
//!   prompt         one line on every user prompt
//!   pretool        an old-style code read (sed -n ranges, cat/head/tail of code, grep -A on a
//!                  func, repo-wide identifier grep) gets the mt command that returns the same
//!                  lines. A lone `sed -n A,Bp file` is rewritten in place (updatedInput), so the
//!                  read costs no extra turn; anything else is denied with the command (strict)
//!                  or allowed with a hint (nudge)
//!   session-end    stop this session's server
//!
//! Knobs: MT_SERVER_SCOPE=session (default) | repo | off; MT_ENFORCE=strict (default) | nudge |
//! off; MT_REWRITE=0 denies instead of rewriting; MT_INJECT_SKILL=0 skips the skill text.
//! Every decision goes to <repo>/.mt/hooks.log (the repo of the event's cwd) with the rule that fired.

use crate::{definitions_with, fnv1a, index, languages, query, serve};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SKILL: &str = include_str!("../skills/mt/SKILL.md");
/// `cat`/`head` of code files up to this many lines in total pass through untouched.
const SMALL_FILE: usize = 300;

/// Code is whatever one of mt's grammars parses, minus config and docs (a `sed` on a manifest
/// or a README is fine as it is). The grammar table is the only language list there is.
fn is_code(p: &str) -> bool {
    matches!(languages::detect(Path::new(p), ""), languages::Tier::TreeSitter(_)) && !matches!(index::role_of(p), "config" | "doc")
}

/// The repo this hook event belongs to (from the event's `cwd`): its decisions also go to
/// `<repo>/.mt/hooks.log`, so `mt logs` shows only that repo's. Unset when the event names no cwd.
static LOG_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn log(msg: &str) {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    // one log per repo: `mt logs`, `mt visualize` and `mt config doctor` read only that repo's
    let root = LOG_ROOT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(r) = root.filter(|r| r.join(".mt").is_dir()) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(repo_log(&r)) {
            let _ = writeln!(f, "{t:.3} {msg}");
        }
    }
}

/// This repo's hook decisions.
pub fn repo_log(root: &Path) -> PathBuf {
    root.join(".mt/hooks.log")
}

/// While a PostToolUse answer is being built, `emit` keeps it here instead of printing, so the next
/// part of the reference can ride along in the same answer.
static CAPTURE: std::sync::Mutex<Option<Option<Value>>> = std::sync::Mutex::new(None);

fn emit(v: Value) {
    let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
    match c.as_mut() { Some(slot) => *slot = Some(v), None => print!("{v}") }
}

fn ident_ok(s: &str) -> bool {
    s.len() >= 3 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The socket of a session's server. macOS caps socket paths at ~104 bytes: short, outside the repo.
pub fn sock_for(sid: &str) -> String {
    format!("/tmp/mt-s-{:012x}.sock", fnv1a(sid.as_bytes()) & 0xffff_ffff_ffff)
}

fn short(sid: &str) -> &str {
    &sid[..sid.len().min(12)]
}

pub fn run(event: &str) -> i32 {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    let d: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    *LOG_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = d.get("cwd").and_then(|v| v.as_str()).map(|c| index::find_root(Path::new(c)));
    // what this hook records (compressions, in `.mt/events.jsonl`) belongs to the session it runs for
    if let Some(sid) = d.get("session_id").and_then(|v| v.as_str()).filter(|_| std::env::var_os("MT_SESSION").is_none()) {
        // SAFETY: set once, before this single-threaded hook starts any thread
        unsafe { std::env::set_var("MT_SESSION", sid); }
    }
    match event {
        "session-start" => session_start(&d),
        "session-end" => session_end(&d),
        "prompt" => prompt(&d),
        "pretool" => pretool(&d),
        "posttool" => {
            // the tool's own answer (if any), plus the next part of the reference while one is
            // pending: it arrives with the first tool results, before the model writes anything
            *CAPTURE.lock().unwrap_or_else(|e| e.into_inner()) = Some(None);
            posttool(&d);
            let mut out = CAPTURE.lock().unwrap_or_else(|e| e.into_inner()).take().flatten();
            let sid = d.get("session_id").and_then(|v| v.as_str()).unwrap_or("nosession");
            if let Some(part) = next_pending(sid) {
                let o = out.get_or_insert_with(|| json!({"hookSpecificOutput": {"hookEventName": "PostToolUse"}}));
                o["hookSpecificOutput"]["additionalContext"] = Value::String(format!("More of the mt reference (it was too long for the session start):\n\n{part}"));
            }
            if let Some(o) = out { print!("{o}"); }
        }
        _ => {}
    }
    0
}

fn cwd_of(d: &Value) -> PathBuf {
    d.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

// ------------------------------------------------------------------ session start / end

fn session_start(d: &Value) {
    // after a compaction (or /clear) what the session was shown is gone from context: forget it,
    // so neither `mt query` nor the output dedup points at text the model no longer has
    // A compaction keeps a prose summary; the working set puts back the exact handles (read
    // before the reset below forgets them), so the model expands instead of searching again.
    let mut working = String::new();
    // after a compaction: the code it was working on, as it is now (MT_PRELOAD=0 turns it off)
    let mut preload: Vec<String> = vec![];
    if let (Some(sid), Some("compact" | "clear")) = (d.get("session_id").and_then(|v| v.as_str()), d.get("source").and_then(|v| v.as_str())) {
        if d["source"] == "compact" { working = crate::vctx::working_set(sid, 2_500); preload = crate::vctx::recent_spans(sid, 6); }
        crate::compress::reset_seen(sid);
        if let Some(p) = query::session_file(Some(sid)) {
            // the numbered items are no longer on screen either: `#n` and `mt expand n` must not
            // resolve to something the model cannot see
            let _ = std::fs::remove_file(p.with_extension("ptrs"));
            // nor are the lines shown so far: a later reply must send them again, not point at them
            let _ = std::fs::remove_file(p.with_extension("ranges"));
            let _ = std::fs::remove_file(p);
        }
        log(&format!("session-start {sid}: reset what was shown (source {})", d["source"]));
    }
    use std::os::unix::process::CommandExt;
    let sid = d.get("session_id").and_then(|v| v.as_str()).unwrap_or("nosession");
    let root = index::find_root(&cwd_of(d));
    let scope = std::env::var("MT_SERVER_SCOPE").unwrap_or_else(|_| "session".into());
    let mut env_lines = vec![format!("export MT_SESSION=\"{sid}\"")];
    let mut server = "off".to_string();
    if scope != "off" {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("serve").current_dir(&root).stdin(std::process::Stdio::null()).process_group(0);
            if let Ok(f) = std::fs::File::create(format!("/tmp/mt-serve-{}.log", short(sid))) {
                if let Ok(f2) = f.try_clone() { cmd.stderr(f2); }
                cmd.stdout(f);
            }
            let sock = sock_for(sid);
            if scope == "session" {
                cmd.env("MT_SOCKET", &sock).env("MT_SOCKET_ROOT", &root);
                env_lines.push(format!("export MT_SOCKET=\"{sock}\""));
                env_lines.push(format!("export MT_SOCKET_ROOT=\"{}\"", root.display()));
            }
            let t = Instant::now();
            match cmd.spawn() {
                Ok(child) => {
                    let _ = std::fs::write(format!("/tmp/mt-serve-{}.pid", short(sid)), child.id().to_string());
                    if scope == "session" {
                        // wait briefly so the first mt call is warm; never block the session long
                        while t.elapsed() < Duration::from_secs(3) && !Path::new(&sock).exists() {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        let up = if Path::new(&sock).exists() { "up" } else { "still loading" };
                        server = format!("session server pid {}, socket {sock}, {up} after {} ms", child.id(), t.elapsed().as_millis());
                    } else {
                        server = format!("per-repo server pid {}", child.id());
                    }
                }
                Err(e) => server = format!("not started: {e}"),
            }
        }
        log(&format!("session-start {sid} {} {server}", root.display()));
    }
    match std::env::var("CLAUDE_ENV_FILE") {
        Ok(p) => {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", env_lines.join("\n"));
            }
        }
        Err(_) => log("session-start: no CLAUDE_ENV_FILE; mt calls will use the per-repo server"),
    }
    let mut ctx = format!(
        "mt is installed for this repository ({}) and its server is running ({server}).\n\
         Use mt, and only mt, for ALL code reading, searching and changing in this session, and for long\n\
         output (builds, tests, logs, JSON). Not grep/sed/cat/head/Read, and not other code-search tools\n\
         (graft, graphify): where other instructions say to use one of those first, mt takes precedence.\n\
         A hook redirects old-style reads to mt. Plain grep stays fine for literal text in docs and config. ",
        root.display()
    );
    if !working.is_empty() { ctx.push_str(&format!("\n\n{working}\n")); }
    // the heads-up after a compaction: not only the handles, the current code of the spans it was
    // most recently working on, hash-checked, about 2k tokens, so it can carry on without
    // re-reading. Recorded as shown (after the reset above), so the next query points at it.
    if !preload.is_empty() && std::env::var("MT_PRELOAD").as_deref() != Ok("0") {
        // SAFETY: the hook is single-threaded here; nothing else reads the environment meanwhile
        unsafe { std::env::set_var("MT_MAX_OUTPUT", "8000"); }
        let sid = d.get("session_id").and_then(|v| v.as_str());
        let (_, body) = query::expand(&root, &preload, sid);
        if !body.trim().is_empty() {
            ctx.push_str("\n## Preloaded after the compaction: the code you were working on, as it is on disk now (hash-checked)\n");
            ctx.push_str(&body);
        }
    }
    let pending = pending_file(sid);
    let _ = std::fs::remove_file(&pending);
    if std::env::var("MT_INJECT_SKILL").as_deref() == Ok("0") {
        // the session already carries the skill (e.g. --append-system-prompt-file): don't pay twice
        ctx.push_str("The mt reference is already in your instructions.\n");
    } else {
        // Claude Code moves hook context over 10,000 characters into a file, and reading that back
        // costs a turn. So the reference goes in parts that each fit: the first here, the rest with
        // the next prompts (each hook has its own limit). Whole `##` sections, in order.
        let intro = "The mt reference follows; you do not need to load the skill.";
        let mut parts = split_sections(skill_body().trim(), CONTEXT_LIMIT - utf16(&ctx) - utf16(intro) - 120);
        // two hooks carry the reference before the first call (a one-prompt run has no third):
        // what does not fit in two becomes a map at the end of part 2, each section one
        // `mt ref <section>` away, so nothing is silently missing
        if parts.len() > 2 {
            let mut over: Vec<String> = parts.split_off(2);
            let full = over.clone();
            loop {
                let map = ref_map(&over);
                if utf16(&parts[1]) + utf16(&map) + 200 <= CONTEXT_LIMIT { parts[1] = format!("{}\n\n{map}", parts[1]); break; }
                // part 2 too full for the map: its last section joins the mapped ones
                let p = parts[1].clone();
                match p.rfind("\n## ") { Some(i) => { over.insert(0, p[i + 1..].to_string()); parts[1] = p[..i].to_string(); } None => { parts[1] = map; break; } }
            }
            // the mapped sections themselves follow, whole, with the first tool results
            let moved = over.len() > full.len();
            parts.extend(if moved { split_sections(&over.join("\n\n"), CONTEXT_LIMIT - 200) } else { full });
        }
        if parts.len() > 1 {
            ctx.push_str(&format!("{intro} Part 1 of {} is here; the rest arrives with the user's prompt.\n\n", parts.len()));
            let _ = std::fs::write(&pending, parts[1..].join(PART_SEP));
        } else {
            ctx.push_str(&format!("{intro}\n\n"));
        }
        ctx.push_str(parts.first().map_or("", |s| s.as_str()));
    }
    emit(json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ctx}}));
}

/// Claude Code's inline limit for hook context, in its own units (UTF-16, a JS string's length).
/// Measured: 9,900 characters arrive inline, 10,100 are moved to a file. Kept a little under.
const CONTEXT_LIMIT: usize = 9_800;
const PART_SEP: &str = "\n\u{1e}\n";

fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Reference parts the session has not been given yet.
fn pending_file(sid: &str) -> String {
    format!("/tmp/mt-brief-{}.txt", short(sid))
}

/// Split markdown into parts of at most `max` UTF-16 units, at `## ` headings (a section bigger
/// than `max` is split at blank lines, a paragraph bigger than that at line ends).
fn split_sections(text: &str, max: usize) -> Vec<String> {
    let mut units: Vec<String> = vec![];
    for line in text.split_inclusive('\n') {
        if line.starts_with("## ") || units.is_empty() { units.push(String::new()); }
        units.last_mut().unwrap().push_str(line);
    }
    // a section that alone is over the limit: split it at paragraphs, then at lines
    let mut pieces: Vec<String> = vec![];
    for u in units {
        if utf16(&u) <= max { pieces.push(u); continue; }
        let mut cur = String::new();
        for para in u.split_inclusive("\n\n").flat_map(|p| if utf16(p) <= max { vec![p.to_string()] } else { p.split_inclusive('\n').map(String::from).collect() }) {
            if !cur.is_empty() && utf16(&cur) + utf16(&para) > max { pieces.push(std::mem::take(&mut cur)); }
            cur.push_str(&para);
        }
        if !cur.is_empty() { pieces.push(cur); }
    }
    let mut parts: Vec<String> = vec![];
    for p in pieces {
        match parts.last_mut() {
            Some(last) if utf16(last) + utf16(&p) <= max => last.push_str(&p),
            _ => parts.push(p),
        }
    }
    parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// The next pending part of the reference, removed from the queue (for PostToolUse delivery).
fn next_pending(sid: &str) -> Option<String> {
    let pending = pending_file(sid);
    let rest = std::fs::read_to_string(&pending).ok()?;
    let mut parts: Vec<&str> = rest.split(PART_SEP).filter(|p| !p.trim().is_empty()).collect();
    if parts.is_empty() { let _ = std::fs::remove_file(&pending); return None; }
    let next = parts.remove(0).to_string();
    if parts.is_empty() { let _ = std::fs::remove_file(&pending); } else { let _ = std::fs::write(&pending, parts.join(PART_SEP)); }
    Some(next)
}

/// UserPromptSubmit: the next part of the reference while any is pending, then the one-line plan.
fn prompt(d: &Value) {
    let line = "mt is active. Plan the whole task before the first call, then: ONE `mt query` read batch with every question and every `span` you need (chain `\"span\": \"$id\"` to read what a query finds; `mt expand <n>` opens pointers from the last reply), and ONE `mt apply` write batch as an `@@` bundle that checks, prepares, changes and proves in one call: `@@ expect` claims, `@@ before` setup commands, every file, edit and test, `@@ format`, and `@@ run` build and tests. No Python/shell scripts or loops over files: the same edit in many files is one `@@ regex in <dir> ext <exts> expect N`. Never retype code that exists: to move or duplicate it, `@@ move <span> <hash> to <file>` / `@@ copy ...` (`#n` = item n of your last reply); into a new file, `@@ create <file>` with only its header, then the moves.";
    let sid = d.get("session_id").and_then(|v| v.as_str()).unwrap_or("nosession");
    let pending = pending_file(sid);
    let mut ctx = String::new();
    if let Ok(rest) = std::fs::read_to_string(&pending) {
        let mut parts: Vec<&str> = rest.split(PART_SEP).filter(|p| !p.trim().is_empty()).collect();
        if !parts.is_empty() {
            let next = parts.remove(0);
            // later parts are delivered with tool results (next_pending), not with more prompts
            ctx = format!("The rest of the mt reference{}:\n\n{next}\n\n", if parts.is_empty() { "" } else { " (more arrives with the next prompt)" });
            if parts.is_empty() { let _ = std::fs::remove_file(&pending); } else { let _ = std::fs::write(&pending, parts.join(PART_SEP)); }
        }
    }
    if utf16(&ctx) + utf16(line) <= CONTEXT_LIMIT { ctx.push_str(line); }
    emit(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}}));
}

/// The plugin's copy of the skill (editable without a rebuild), else the one built in.
/// A section's short name for `mt ref`: its heading's first words, lowercased, dashed.
fn slug(title: &str) -> String {
    let t: String = title.trim_start_matches('#').trim().chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    t.split_whitespace().filter(|w| !["the", "a", "an", "mt"].contains(w)).take(3).collect::<Vec<_>>().join("-")
}

/// Every `## ` section of `parts`: (title, slug, size in chars).
fn sections_of(parts: &[String]) -> Vec<(String, String, usize)> {
    let mut v = vec![];
    for p in parts {
        let mut cur: Option<(String, usize)> = None;
        for l in p.lines() {
            if let Some(t) = l.strip_prefix("## ") {
                if let Some((ti, n)) = cur.take() { v.push((ti.clone(), slug(&ti), n)); }
                cur = Some((t.to_string(), 0));
            }
            if let Some((_, n)) = cur.as_mut() { *n += l.len() + 1; }
        }
        if let Some((ti, n)) = cur { v.push((ti.clone(), slug(&ti), n)); }
    }
    v
}

fn ref_map(over: &[String]) -> String {
    let rows: Vec<String> = sections_of(over).into_iter().map(|(t, s, n)| format!("- `mt ref {s}`: {t} ({n} chars)")).collect();
    format!("## The rest of the mt reference arrives with your first tool results; to read a section now: `mt ref <section>`\n{}", rows.join("\n"))
}

/// `mt ref [section]`: one section of the mt reference, or the list of them.
pub fn reference(section: Option<&str>) -> (bool, String) {
    let body = skill_body();
    let all = sections_of(&[body.clone()]);
    let Some(want) = section.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) else {
        return (true, all.iter().map(|(t, s, n)| format!("{s:<28} {t} ({n} chars)\n")).collect());
    };
    let Some((title, _, _)) = all.iter().find(|(t, s, _)| *s == want || s.starts_with(&want) || t.to_lowercase().contains(&want)) else {
        return (false, format!("no section {want:?}; the sections: {}\n", all.iter().map(|x| x.1.clone()).collect::<Vec<_>>().join(", ")));
    };
    let start = body.find(&format!("## {title}")).unwrap_or(0);
    let rest = &body[start + 3..];
    let end = rest.find("\n## ").map(|i| start + 3 + i).unwrap_or(body.len());
    (true, format!("{}\n", body[start..end].trim_end()))
}

fn skill_body() -> String {
    let s = std::env::var("CLAUDE_PLUGIN_ROOT").ok().and_then(|r| std::fs::read_to_string(Path::new(&r).join("skills/mt/SKILL.md")).ok()).unwrap_or_else(|| SKILL.to_string());
    match s.strip_prefix("---").and_then(|r| r.split_once("\n---")) {
        Some((_, body)) => body.to_string(),
        None => s,
    }
}

fn session_end(d: &Value) {
    let sid = d.get("session_id").and_then(|v| v.as_str()).unwrap_or("nosession");
    let pidf = format!("/tmp/mt-serve-{}.pid", short(sid));
    let sock = sock_for(sid);
    let req = serve::Req { cmd: "stop".into(), exe: serve::exe_id(), ..Default::default() };
    if serve::request_at(Path::new(&sock), &req).is_some() {
        log(&format!("session-end {sid} stopped via socket"));
    } else if let Some(pid) = std::fs::read_to_string(&pidf).ok().and_then(|p| p.trim().parse::<u32>().ok()) {
        // no answer on the socket (still loading, or another build): signal it
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        log(&format!("session-end {sid} stopped pid {pid}"));
    } else {
        log(&format!("session-end {sid} nothing to stop"));
    }
    let _ = std::fs::remove_file(&pidf);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pending_file(sid));
}

// ------------------------------------------------------------------ redirect old-style reads

/// The command before the first unquoted pipe.
fn first_stage(seg: &str) -> &str {
    let (mut q, mut esc) = (None::<char>, false);
    let b = seg.as_bytes();
    for (i, ch) in seg.char_indices() {
        if esc { esc = false; continue; }
        match q {
            Some(c) => { if ch == '\\' { esc = true } else if ch == c { q = None } }
            None => {
                if ch == '\'' || ch == '"' { q = Some(ch) } else if ch == '|' && b.get(i + 1) != Some(&b'|') && (i == 0 || b[i - 1] != b'|') { return seg[..i].trim(); }
            }
        }
    }
    seg.trim()
}

/// Shell words, POSIX-style quoting; None when the quotes don't balance.
fn words(s: &str) -> Option<Vec<String>> {
    let (mut out, mut cur, mut have) = (vec![], String::new(), false);
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\'' => { have = true; loop { match it.next()? { '\'' => break, x => cur.push(x) } } }
            '"' => {
                have = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => { let x = it.next()?; if !matches!(x, '"' | '\\' | '$' | '`') { cur.push('\\'); } cur.push(x) }
                        x => cur.push(x),
                    }
                }
            }
            '\\' => { have = true; if let Some(x) = it.next() { cur.push(x) } }
            c if c.is_whitespace() => { if have { out.push(std::mem::take(&mut cur)); have = false; } }
            c => { have = true; cur.push(c) }
        }
    }
    if have { out.push(cur); }
    Some(out)
}

// ------------------------------------------------------------------ compress tool output (PostToolUse)

/// Bash output below this many characters and lines passes untouched (only session dedup).
const BASH_MIN_CHARS: usize = 3_000;
const BASH_MIN_LINES: usize = 80;
/// A whole-file Read of a code file longer than this comes back as its outline.
const READ_FOLD_LINES: usize = 800;
/// MCP text blocks below this size pass untouched.
const MCP_MIN_CHARS: usize = 1_500;

/// PostToolUse: tool output is compressed before the model sees it. The replacement keeps each
/// tool's own structure (Claude Code ignores a bare string) and always says what was reduced and
/// how to get the rest, so the model never mistakes it for the real thing.
fn posttool(d: &Value) {
    use crate::compress::{self, Kind};
    if std::env::var("MT_COMPRESS").as_deref() == Ok("0") { return; }
    let tool = d.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let sid_own = crate::coord::hook_sid(d).unwrap_or_else(|| "nosession".into());
    let sid = sid_own.as_str();
    let inp = d.get("tool_input").cloned().unwrap_or(Value::Null);
    if std::env::var_os("MT_HOOK_DEBUG").is_some() {
        let r = d.get("tool_response").map(|v| v.to_string()).unwrap_or_default();
        log(&format!("posttool debug {tool}: response {} chars: {}", r.len(), r.chars().take(160).collect::<String>()));
    }
    let Some(resp) = d.get("tool_response").cloned() else { return };
    let budget = std::env::var("MT_COMPRESS_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(compress::DEFAULT_LINES);
    let replace = |new: Value, what: String| {
        log(&format!("posttool {tool} {what}"));
        emit(json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": new}}));
    };
    match tool {
        "Bash" => {
            let cmd = inp.get("command").and_then(|v| v.as_str()).unwrap_or("");
            // `mt show`: the text goes to the user (systemMessage: shown, never sent to the model),
            // and the model gets a receipt. Big output may arrive as a persisted file.
            let out = resp.get("persistedOutputPath").and_then(|v| v.as_str()).and_then(|p| std::fs::read_to_string(p).ok())
                .or_else(|| resp.get("stdout").and_then(|v| v.as_str()).map(|s| persisted(s).unwrap_or_else(|| s.to_string())));
            if let Some((user, model)) = out.as_deref().and_then(crate::vctx::show_split) {
                let mut new = resp.clone();
                if let Some(o) = new.as_object_mut() { o.remove("persistedOutputPath"); o.remove("persistedOutputSize"); }
                new["stdout"] = Value::String(model);
                log(&format!("posttool Bash mt show: {} chars to the user", user.len()));
                emit(json!({"systemMessage": user, "hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": new}}));
                return;
            }
            // mt's own output is already budgeted and carries hashes an edit needs: never touch it
            if Regex::new(r"(?:^|[;&|(]\s*)(?:\S*/)?mt\s").is_ok_and(|r| r.is_match(cmd)) { return; }
            if resp.get("interrupted").and_then(|v| v.as_bool()) == Some(true) || resp.get("isImage").and_then(|v| v.as_bool()) == Some(true) { return; }
            // a command that prints code or a diff keeps it exact (lossless steps only)
            let head = words(first_stage(cmd)).and_then(|w| w.first().cloned()).unwrap_or_default();
            // ...but a log file printed with cat/tail/head is a log, not code: it is compressed like one
            let log_file = Regex::new(r"(?i)(\.(log|out|err|jsonl|ndjson)\b|/logs?/|\blogs?/)").is_ok_and(|r| r.is_match(first_stage(cmd)));
            let exact = !log_file && (matches!(head.as_str(), "cat" | "sed" | "head" | "tail" | "nl" | "less" | "bat" | "awk") || Regex::new(r"^\s*git\s+(diff|show|log\s+.*-p|blame)").is_ok_and(|r| r.is_match(cmd)));
            // printed with cat & co: exact, unless it is a big JSON document (data, not something to edit)
            let big_json = |s: &str| s.len() > 8_000 && serde_json::from_str::<Value>(s.trim()).is_ok();
            let kind = if exact && !resp.get("stdout").and_then(|v| v.as_str()).is_some_and(big_json) { Kind::Verbatim } else { Kind::Auto };
            let short: String = cmd.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
            // each kind of output keeps to its own store unless the agent named one (MT_OUT_NAME)
            let store_name = compress::default_name(&compress::command_name(cmd));
            let mut new = resp.clone();
            let mut notes: Vec<String> = vec![];
            // too big for Claude Code: it kept the whole output in a file (`persistedOutputPath`) and
            // would show a preview. Compress the whole output instead, and drop the path so the
            // compressed text is what the model gets.
            if let Some(full) = resp.get("persistedOutputPath").and_then(|v| v.as_str()).and_then(|p| std::fs::read_to_string(p).ok()) {
                let kind = if exact && !big_json(&full) { Kind::Verbatim } else { Kind::Auto };
                let (text, _) = compress::compress_and_store(&full, kind, budget, &format!("`{short}` output"), &store_name);
                if let Some(o) = new.as_object_mut() { o.remove("persistedOutputPath"); o.remove("persistedOutputSize"); }
                new["stdout"] = Value::String(text.clone());
                replace(new, format!("`{short}`: persisted {}->{} chars", full.len(), text.len()));
                return;
            }
            for field in ["stdout", "stderr"] {
                let Some(s) = resp.get(field).and_then(|v| v.as_str()) else { continue };
                // output too big for Claude Code arrives as a 2 KB preview plus the path of the full
                // text: compress the full text instead, so the model needs neither the preview nor a read
                if let Some(full) = persisted(s) {
                    let kind = if exact && !big_json(&full) { Kind::Verbatim } else { Kind::Auto };
                    let (text, _) = compress::compress_and_store(&full, kind, budget, &format!("`{short}` {field}"), &store_name);
                    notes.push(format!("{field} persisted {}->{} chars", full.len(), text.len()));
                    new[field] = Value::String(text);
                    continue;
                }
                let big = s.len() >= BASH_MIN_CHARS || s.lines().count() >= BASH_MIN_LINES;
                let text = if big { compress::compress_and_store(s, kind, budget, &format!("`{short}` {field}"), &store_name).0 } else { s.to_string() };
                let (text, dropped) = compress::dedup_seen(&text, sid);
                if text != s {
                    notes.push(format!("{field} {}->{} chars{}", s.len(), text.len(), if dropped > 0 { format!(", {dropped} lines seen before") } else { String::new() }));
                    new[field] = Value::String(text);
                }
            }
            if !notes.is_empty() { replace(new, format!("`{short}`: {}", notes.join("; "))); }
        }
        "Read" => {
            if inp.get("offset").is_some() || inp.get("limit").is_some() { return; } // an explicit range is always served whole
            let Some(content) = resp.pointer("/file/content").and_then(|v| v.as_str()) else { return };
            let path = resp.pointer("/file/filePath").and_then(|v| v.as_str()).or_else(|| inp.get("file_path").and_then(|v| v.as_str())).unwrap_or("");
            let mut new = resp.clone();
            if compress::read_seen(sid, path, content) {
                new["file"]["content"] = Value::String(format!("[mt: unchanged since your earlier Read of this file in this session; that copy is still in your context. Read it with offset/limit for a fresh copy.]"));
                replace(new, format!("{path}: unchanged re-read, {} chars not repeated", content.len()));
                return;
            }
            let n = content.lines().count();
            if n > READ_FOLD_LINES && is_code(path) {
                let id = compress::store(content, path, &compress::default_name("read"));
                let defs = defs_of(Path::new(path));
                let all: Vec<String> = defs.iter().filter(|d| d.depth <= 1).map(|d| format!("{}{} {} L{}-{}", "  ".repeat(d.depth), d.kind, d.name, d.s, d.e)).collect();
                // a file with very many definitions: its first 200 and last 50, the middle by name
                let mut outline: Vec<String> = if all.len() > 250 {
                    let mut o = all[..200].to_vec();
                    o.push(format!("... {} more definitions: mt skeleton {path}", all.len() - 250));
                    o.extend_from_slice(&all[all.len() - 50..]);
                    o
                } else { all };
                if outline.is_empty() { outline = content.lines().take(60).map(String::from).collect(); }
                let text = format!(
                    "[mt: {n}-line file shown as its outline (every definition with its lines), not its text. Read the part you need with offset/limit, or `mt span {path} <Name>`; to edit, read that part first. Search it: mt out {id} \"<question>\" or --grep RE]\n{}",
                    outline.join("\n"));
                new["file"]["content"] = Value::String(text);
                replace(new, format!("{path}: {n} lines -> outline of {} definitions", defs.len()));
            }
        }
        t if t.starts_with("mcp__") => {
            // any text in the response (a bare string, content blocks, nested) that is large
            fn walk(v: &mut Value, label: &str, name: &str, budget: usize, n: &mut Vec<String>) {
                match v {
                    Value::String(s) if persisted(s).is_some() || s.len() >= MCP_MIN_CHARS => {
                        let src = persisted(s).unwrap_or_else(|| s.clone());
                        let (t, _) = crate::compress::compress_and_store(&src, Kind::Auto, budget, label, name);
                        if t.len() < s.len() { n.push(format!("{}->{} chars", s.len(), t.len())); *s = t; }
                    }
                    Value::Array(a) => for x in a { walk(x, label, name, budget, n) },
                    Value::Object(m) => for (_, x) in m.iter_mut() { walk(x, label, name, budget, n) },
                    _ => {}
                }
            }
            let mut new = resp.clone();
            let mut notes = vec![];
            // an MCP tool's output keeps to a store named after its server and tool
            let parts: Vec<&str> = t.trim_start_matches("mcp__").splitn(2, "__").collect();
            let store_name = compress::default_name(&parts.join("-"));
            walk(&mut new, t, &store_name, budget, &mut notes);
            // once per session, after the first native call to a server mt can run: the next calls
            // can be one batch (`mt mcp call`), chained, with results kept. Nothing about
            // permissions changes, so a tool the user's rules restrict is not suggested.
            let nudge = (|| {
                let mark = std::env::temp_dir().join(format!("mt-mcp-nudge-{}", sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>()));
                if mark.exists() { return None; }
                let project = std::path::PathBuf::from(crate::tools::repo_root(&cwd_of(d).to_string_lossy()));
                let all = crate::mcp::servers(&project);
                let srv = all.iter().find(|s| t.starts_with(&s.prefix))?;
                if srv.cfg.get("command").is_none() || crate::mcp::blocked(&project, srv, &t[srv.prefix.len()..]).is_some() { return None; }
                let _ = std::fs::write(&mark, "");
                let big = resp.to_string().len();
                Some(format!("mt: `{}` runs through mt too. When the next steps need more than one call to it (a lookup, then its follow-ups), send them as ONE `mt mcp call - <<'EOF' [{{\"id\": \"a\", \"server\": \"{}\", \"tool\": \"...\", \"args\": {{...}}}}, {{\"id\": \"b\", ..., \"args\": {{\"x\": \"$a.path\"}}}}] EOF`: one turn instead of one per call, and large results are kept for `mt out <id>`.{} `mt mcp tools {}` lists its tools in one line each.", srv.short, srv.short, if big > 8_000 { format!(" This result was {big} chars.") } else { String::new() }, srv.short))
            })();
            if !notes.is_empty() || nudge.is_some() {
                log(&format!("posttool {t}: {}{}", notes.join("; "), if nudge.is_some() { " +mcp nudge" } else { "" }));
                let mut out = json!({"hookEventName": "PostToolUse"});
                if !notes.is_empty() { out["updatedMCPToolOutput"] = new.clone(); out["updatedToolOutput"] = new; }
                if let Some(n) = nudge { out["additionalContext"] = json!(n); }
                emit(json!({"hookSpecificOutput": out}));
            }
        }
        _ => {}
    }
}

/// Claude Code's stand-in for an output it found too large: `<persisted-output>`, then
/// `Full output saved to: <path>`, then a 2 KB preview. The full text, read back from that file.
fn persisted(s: &str) -> Option<String> {
    if !s.trim_start().starts_with("<persisted-output>") { return None; }
    let i = s.find("saved to: ")? + "saved to: ".len();
    let path = s[i..].lines().next()?.trim();
    std::fs::read_to_string(path).ok()
}

struct Def0 { kind: String, name: String, s: usize, e: usize, depth: usize }

fn defs_of(path: &Path) -> Vec<Def0> {
    let Ok(b) = std::fs::read(path) else { return vec![] };
    let src = String::from_utf8_lossy(&b);
    let tier = languages::detect(path, &src);
    definitions_with(&tier, &src).into_iter().map(|d| Def0 { kind: d.kind, name: d.name, s: d.start + 1, e: d.end + 1, depth: d.path.len() }).collect()
}

/// Outermost definitions overlapping lines a..b (1-based), not containers far bigger than the read.
fn defs_covering(path: &Path, a: usize, b: usize) -> Vec<Def0> {
    let mut all = defs_of(path);
    all.sort_by_key(|d| (d.s, std::cmp::Reverse(d.e - d.s)));
    let mut chosen: Vec<Def0> = vec![];
    for d in all {
        if d.e < a || d.s > b || d.e - d.s + 1 > (3 * (b - a + 1)).max(150) { continue; }
        if chosen.iter().any(|c| c.s <= d.s && d.e <= c.e) { continue; }
        chosen.push(d);
    }
    chosen
}

fn mt_name(kind: &str, name: &str) -> String {
    if kind == "method" { name.rsplit('.').next().unwrap_or(name).to_string() } else { name.to_string() }
}

/// Repo-relative path of `f` as `mt query` stores it.
fn rel_of(root: &Path, cwd: &Path, f: &str) -> String {
    let p = cwd.join(f);
    let p = std::fs::canonicalize(&p).unwrap_or(p);
    p.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| f.to_string())
}

/// Numbers of the last mt reply's items in file `rel` (overlapping lines a..b), from the table
/// `mt expand` reads. An item inside another hit (a var inside its function) adds nothing.
fn pointer_numbers(sid: Option<&str>, rel: &str, range: Option<(usize, usize)>) -> Vec<String> {
    let Some(sid) = sid else { return vec![] };
    let Some(p) = query::session_file(Some(sid)) else { return vec![] };
    let Ok(t) = std::fs::read_to_string(p.with_extension("ptrs")) else { return vec![] };
    let hit: Vec<(String, usize, usize)> = t
        .lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|r| r.len() == 7 && r[1] == rel)
        .filter_map(|r| Some((r[0].to_string(), r[2].parse().ok()?, r[3].parse().ok()?)))
        .filter(|(_, s, e)| range.is_none_or(|(a, b)| *s <= b && *e >= a))
        .collect();
    hit.iter()
        .filter(|(n, s, e)| !hit.iter().any(|(o, os, oe)| o != n && os <= s && e <= oe && (os, oe) != (s, e)))
        .map(|(n, _, _)| n.clone())
        .collect()
}

/// `only_reads`: every part of the command is an old-style code read. Only then is a denial
/// worth its turn; a command that also does other work (an mt call, an apply, a build) is let
/// through with the hint, since denying it would throw that work away.
struct Suggestion { rules: Vec<&'static str>, cmds: Vec<String>, rewrite: bool, only_reads: bool }

fn suggest(cmd: &str, cwd: &Path, root: &Path, sid: Option<&str>) -> Option<Suggestion> {
    let seg_rx = Regex::new(r"\s*(?:&&|\|\||;|\n)\s*").ok()?;
    let skip_rx = Regex::new(r"^(\S*/)?mt\s").ok()?;
    let sed_rx = Regex::new(r#"^sed\s+-n\s+['"]?(\d+),(\d+)p['"]?\s+(\S+)$"#).ok()?;
    let ctx_rx = Regex::new(r"\s-[ABC]\s*\d+").ok()?;
    let recur_rx = Regex::new(r"^-\w*r").ok()?;
    let alt_rx = Regex::new(r"\\\||\|").ok()?;
    let segs: Vec<&str> = seg_rx.split(cmd).map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let (mut sugg, mut rules): (Vec<String>, Vec<&'static str>) = (vec![], vec![]);
    let mut rewritable = segs.len() == 1;
    for seg in &segs {
        let first = first_stage(seg);
        if first.len() != seg.len() { rewritable = false; } // piped: the output is post-processed
        if skip_rx.is_match(first) { continue; }
        let Some(toks) = words(first) else { continue };
        let Some(head) = toks.first().map(|s| s.as_str()) else { continue };
        let files: Vec<&String> = toks[1..].iter().filter(|t| !t.starts_with('-') && is_code(t)).collect();
        // sed -n 'A,Bp' file
        if let Some(m) = sed_rx.captures(first) {
            let f = &m[3];
            if is_code(f) {
                let (a, b): (usize, usize) = (m[1].parse().unwrap_or(1), m[2].parse().unwrap_or(1));
                let nums = pointer_numbers(sid, &rel_of(root, cwd, f), Some((a, b)));
                if !nums.is_empty() {
                    sugg.push(format!("mt expand {}", nums.join(" ")));
                    rules.push("sed-range-on-pointer");
                    continue;
                }
                let ds = defs_covering(&cwd.join(f), a, b.max(a));
                if !ds.is_empty() {
                    sugg.extend(ds.iter().map(|d| format!("mt span {f} {}", mt_name(&d.kind, &d.name))));
                    rules.push("sed-range-on-code");
                    continue;
                }
            }
        }
        // cat / head / tail / nl / bat of code files. A small file is cheaper to read whole than
        // to deny (a denial costs a turn); only big files are redirected to skeleton + spans.
        if matches!(head, "cat" | "head" | "tail" | "nl" | "bat" | "less") && !files.is_empty() {
            rewritable = false;
            // `head -20 f`, `tail -n 50 f`: a few lines, whatever the file's size
            let count = toks.iter().zip(toks.iter().skip(1)).find_map(|(a, b)| (a == "-n").then(|| b.parse::<usize>().ok()).flatten())
                .or_else(|| toks.iter().find_map(|t| t.strip_prefix("-n").or_else(|| t.strip_prefix('-')).and_then(|n| n.parse::<usize>().ok())));
            if matches!(head, "head" | "tail") && count.is_some_and(|n| n <= SMALL_FILE) { continue; }
            let lines: usize = files.iter().map(|f| std::fs::read(cwd.join(f.as_str())).map(|b| b.iter().filter(|&&c| c == b'\n').count()).unwrap_or(0)).sum();
            if lines <= SMALL_FILE { continue; }
            let n0 = sugg.len();
            for f in &files {
                let tops: Vec<Def0> = defs_of(&cwd.join(f)).into_iter().filter(|d| d.depth == 0).collect();
                if !tops.is_empty() {
                    sugg.push(format!("mt skeleton {f}"));
                    sugg.extend(tops.iter().take(4).map(|d| format!("mt span {f} {}", mt_name(&d.kind, &d.name))));
                }
            }
            if sugg.len() > n0 { rules.push(if head == "cat" { "cat-of-code-file" } else { "head-of-code-file" }); continue; }
        }
        // grep -n -A80 "<pattern naming a definition>" file: the definitions of that file whose
        // name the pattern spells out as a word (no keyword list: the file's own grammar decides)
        if matches!(head, "grep" | "rg") && !files.is_empty() && ctx_rx.is_match(first) {
            let pats: Vec<&String> = toks[1..].iter().filter(|t| !t.starts_with('-') && !is_code(t) && !t.chars().all(|c| c.is_ascii_digit())).collect();
            let mut names: Vec<String> = defs_of(&cwd.join(files[0].as_str())).iter().map(|d| mt_name(&d.kind, &d.name))
                .filter(|n| ident_ok(n) && pats.iter().any(|p| Regex::new(&format!(r"\b{}\b", regex::escape(n))).is_ok_and(|r| r.is_match(p))))
                .collect();
            names.dedup();
            names.truncate(4);
            if !names.is_empty() {
                sugg.extend(names.iter().map(|n| format!("mt span {} {n}", files[0])));
                rules.push("grep-context-on-func");
                continue;
            }
        }
        rewritable &= matches!(head, "grep" | "rg" | "egrep");
        // repo-wide grep for an identifier
        if matches!(head, "grep" | "rg" | "egrep") && (head == "rg" || toks[1..].iter().any(|t| recur_rx.is_match(t))) {
            let pats: Vec<&String> = toks[1..].iter().filter(|t| !t.starts_with('-') && t.as_str() != "." && !cwd.join(t.as_str()).exists()).collect();
            if let Some(p) = pats.first() {
                let alts: Vec<&str> = alt_rx.split(p).filter(|a| !a.is_empty()).collect();
                if !alts.is_empty() && alts.iter().all(|a| ident_ok(a)) {
                    // one name: `--edit` lists every referencing line with 2 lines either side,
                    // what the grep was after; several names are denied with one command each
                    if alts.len() <= 4 && rewritable {
                        sugg.push(alts.iter().map(|a| format!("mt context {a} --edit")).collect::<Vec<_>>().join(" && "));
                    } else {
                        rewritable = false;
                        sugg.extend(alts.iter().take(4).map(|i| format!("mt context {i}")));
                    }
                    rules.push("repo-grep-for-identifier");
                    continue;
                }
            }
        }
        rewritable = false;
    }
    if sugg.is_empty() { return None; }
    let only_reads = rules.len() == segs.len(); // one rule per matched part, before dedup
    let mut seen = std::collections::HashSet::new();
    sugg.retain(|s| seen.insert(s.clone()));
    rules.dedup();
    Some(Suggestion { rules, cmds: sugg, rewrite: rewritable, only_reads })
}

/// A Bash command that rewrites code files with a script or a stream editor: a Python, Node,
/// Perl or Ruby script that replaces text and writes files back, `sed -i`, `perl -pi`, or either
/// through `find -exec` / `xargs`. Only when it names code files (or a glob of them): scripts
/// writing data, docs or generated assets, or only temp files, are left alone.
/// Returns (rule, hint): how the same edit reads as one `mt apply` bundle.
fn edit_script(cmd: &str) -> Option<(&'static str, String)> {
    // tokens that look like paths or globs: split on whitespace, quotes, brackets, commas
    let tok = |c: char| c.is_whitespace() || "'\"()[],;=<>|&`{}".contains(c);
    let looks = |t: &&str| t.contains('.') && t.rsplit('.').next().map(|e| !e.is_empty() && e.len() <= 6 && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false);
    let code: Vec<String> = cmd.split(tok).filter(|t| !t.is_empty()).filter(looks).map(String::from)
        .filter(|p| !p.starts_with("/tmp/") && !p.starts_with("/private/tmp/") && !p.contains("/scratchpad/"))
        .filter(|p| is_code(&p.replace("**/", "").replace('*', "x"))).collect();
    if code.is_empty() { return None; }
    let script = regex::Regex::new(r"(?s)\b(python3?|node|perl|ruby|deno|bun)\b[^\n]*(?:-c|-e|<<|\s-\s)").unwrap().is_match(cmd);
    let writes = regex::Regex::new(r#"open\([^)]*['"][wa]\+?['"]|write_text|writeFileSync|fs\.writeFile|File\.write|\.write\("#).unwrap().is_match(cmd);
    let replaces = regex::Regex::new(r"\.replace(All)?\(|re\.sub\(|\.sub\(|\.gsub\(|s/").unwrap().is_match(cmd);
    let sed = regex::Regex::new(r"\bsed\s+(-[a-zA-Z]*\s+)*-i\b|\bsed\s+-[a-zA-Z]*i").unwrap().is_match(cmd);
    let perl = regex::Regex::new(r"\bperl\s+-[a-zA-Z]*(pi|i)").unwrap().is_match(cmd);
    let one_file = code.len() == 1 && !code[0].contains('*');
    let mut uniq: Vec<String> = code.iter().filter(|p| !p.contains('*')).cloned().collect();
    uniq.dedup();
    uniq.sort();
    uniq.dedup();
    let files = uniq.join(",");
    let general = format!("Every edit, in every file, goes in ONE bundle: `@@ find <file>` with the exact old lines then `@@ with` and the new lines (one pair per site, many files per bundle), `@@ rename Old New` for an identifier everywhere, or for the same pattern in many files `@@ regex in <dir> ext <exts> expect <N>` (pattern as its body) then `@@ with` (the replacement; $1 works). All ops address the original files, so one edit never shifts another's lines. Add `@@ format: <cmd> {{changed}}` and `@@ run …` steps to the same bundle.");
    if sed || perl {
        // a simple s/old/new/ becomes the exact @@ regex
        if let Some((old, new)) = sed_subst(cmd) {
            let (old, new) = (old.as_str(), new.as_str());
            let target = if one_file || !files.is_empty() { format!("files {files}") } else { "in <dir> ext <exts>".into() };
            return Some(("stream-edit", format!("Instead: `mt apply - <<'EOF'` then\n@@ regex {target} expect <N>\n{old}\n@@ with\n{}\nEOF\n(`expect` refuses the batch if the count is wrong.) {general}", new.replace("\\1", "$1").replace('&', "$0"))));
        }
        return Some(("stream-edit", general));
    }
    if script && writes && replaces { return Some(("edit-script", general)); }
    None
}

/// The first `s<d>old<d>new<d>` in a sed/perl command (d one of / | # ,), split on unescaped d.
fn sed_subst(cmd: &str) -> Option<(String, String)> {
    let b: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i + 1 < b.len() {
        let starts = b[i] == 's' && "/|#,".contains(b[i + 1]) && (i == 0 || "'\" -;".contains(b[i - 1]));
        if starts {
            let d = b[i + 1];
            let mut parts = vec![String::new()];
            let mut j = i + 2;
            while j < b.len() && parts.len() < 3 {
                if b[j] == '\\' && j + 1 < b.len() { parts.last_mut()?.push(b[j]); parts.last_mut()?.push(b[j + 1]); j += 2; continue; }
                if b[j] == d { parts.push(String::new()); } else { parts.last_mut()?.push(b[j]); }
                j += 1;
            }
            if parts.len() == 3 { return Some((parts[0].clone(), parts[1].clone())); }
        }
        i += 1;
    }
    None
}

/// Consecutive Edit/Write calls on code files in this session (any other tool resets it).
fn edit_run(sid: Option<&str>, edit: bool) -> usize {
    let Some(sid) = sid else { return 0 };
    let safe: String = sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let p = std::env::temp_dir().join(format!("mt-hook-edits-{safe}"));
    let n = if edit { std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0) + 1 } else { 0 };
    let _ = std::fs::write(&p, n.to_string());
    n
}

fn pretool(d: &Value) {
    let mode = std::env::var("MT_ENFORCE").unwrap_or_else(|_| "strict".into());
    if mode == "off" { return; }
    let tool = d.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let inp = d.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = cwd_of(d);
    // inside a subagent the event names the agent: its own seen log, pointers and claims
    let sid_own = crate::coord::hook_sid(d);
    let sid = sid_own.as_deref();
    let pre = |ctx: String| json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": ctx}});
    let editing = matches!(tool, "Edit" | "Write" | "MultiEdit");
    let run = edit_run(sid, editing && is_code(inp.get("file_path").and_then(|v| v.as_str()).unwrap_or("")));
    match tool {
        "Edit" | "Write" | "MultiEdit" => {
            // a second edit call in a row: the rest belong in one bundle (Edit itself stays allowed)
            if run >= 2 {
                log(&format!("pretool nudge edit-run {run}"));
                emit(pre(format!("This is edit call {run} in a row. Put every remaining edit, in every file (and new files, and the build/test steps), in ONE `mt apply` bundle: `@@ find <file>`/`@@ with` per site, `@@ create`, `@@ regex in <dir> ext <exts> expect N`, `@@ run gate build: …`. Ops address the original files, so they never shift each other.")));
            }
        }
        "Agent" | "Task" => {
            // a spawn whose prompt names code this agent read: hand it over as a brief
            let root = index::find_root(&cwd);
            let tu = d.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("x");
            if let Some(new) = sid.and_then(|s| crate::coord::auto_brief(&root, s, tu, &inp)) {
                log("pretool auto-brief");
                emit(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": new}}));
            }
        }
        "Bash" => {
            let cmd = inp.get("command").and_then(|v| v.as_str()).unwrap_or("");
            // a subagent's mt commands carry its agent id (it shares its parent's environment).
            // updatedInput with no permission decision: the user's permission rules still apply
            let agent = d.get("agent_id").and_then(|v| v.as_str()).filter(|a| !a.is_empty() && crate::coord::mentions_mt(cmd) && !cmd.contains("MT_AGENT="));
            let tag = |c: &str| match agent { Some(a) => format!("export MT_AGENT={a}; {c}"), None => c.to_string() };
            let tagged = |ctx: Option<String>| { let mut new = inp.clone(); new["command"] = Value::String(tag(cmd)); let mut h = json!({"hookEventName": "PreToolUse", "updatedInput": new}); if let Some(c) = ctx { h["additionalContext"] = Value::String(c); } json!({"hookSpecificOutput": h}) };
            if let Some((rule, hint)) = edit_script(cmd) {
                let reason = format!("mt: this rewrites code files with a {} ({rule}). {hint}", if rule == "stream-edit" { "stream editor" } else { "one-off script" });
                log(&format!("pretool {mode} {rule} | {}", cmd.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>()));
                if mode == "strict" { emit(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}})); } else { emit(pre(reason)); }
                return;
            }
            let root = index::find_root(&cwd);
            let Some(s) = suggest(cmd, &cwd, &root, sid) else {
                if agent.is_some() { emit(tagged(None)); }
                return;
            };
            let batch = s.cmds.join("; ");
            let what = format!("{} | {} => {batch}", s.rules.join(","), cmd.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>());
            if mode == "strict" && s.rewrite && std::env::var("MT_REWRITE").as_deref() != Ok("0") {
                // the same lines, read through mt, in this very call: no denied turn, no retry
                let mut new = inp.clone();
                // mt's answer when it has one; otherwise exactly what was asked for, with no mt noise
                let run = format!("out=$({batch}) && printf '%s\\n' \"$out\" || {cmd}");
                new["command"] = Value::String(tag(&run));
                log(&format!("pretool rewrite {what}"));
                emit(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow",
                    "permissionDecisionReason": format!("mt: ran `{batch}` for the same lines"),
                    "updatedInput": new,
                    "additionalContext": format!("The hook ran `{batch}` in place of `{cmd}`: the same lines, with spans and hashes for `mt apply`. Read code with mt directly next time.")}}));
                return;
            }
            let reason = format!("This repo has mt. Same lines, structured, in one call: `{batch}`. Put any other questions in the same call (or one `mt query`). Plain grep is still fine for literal text, logs and config.");
            log(&format!("pretool {mode} {what}"));
            if mode == "strict" && s.only_reads {
                emit(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}));
            } else if agent.is_some() {
                emit(tagged(Some(reason)));
            } else {
                emit(pre(reason));
            }
        }
        "Read" => {
            let f = inp.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            if !is_code(f) { return; }
            let root = index::find_root(&cwd);
            let rel = rel_of(&root, &cwd, f);
            let nums = pointer_numbers(sid, &rel, None);
            if !nums.is_empty() {
                log(&format!("pretool nudge read-pointer-file {rel} {nums:?}"));
                let refs: Vec<String> = nums.iter().map(|n| format!("[{n}]")).collect();
                emit(pre(format!("Items {} of your last mt reply are in {rel}: `mt expand {}` gives their current bodies with hashes for `mt apply` (no Read needed).", refs.join(" "), nums.join(" "))));
            } else {
                log(&format!("pretool nudge read-code-file {rel}"));
                emit(pre(format!("Prefer `mt span {rel} <Name>` or `mt context <Name>` for code, and `mt apply` for edits (no Read needed).")));
            }
        }
        "Grep" => {
            let p = inp.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            if ident_ok(p) {
                log(&format!("pretool nudge grep-tool-identifier {p}"));
                emit(pre(format!("`mt context {p}` returns the definition, every reference with its lines, tests and docs in one call.")));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn edit_scripts_are_redirected_and_others_are_not() {
        use super::edit_script;
        // stream editors and rewrite scripts on code files
        let (r, h) = edit_script("sed -i '' 's/ResolveAccount(/ResolveAccountV2(/g' internal/a.go internal/b.go").unwrap();
        assert_eq!(r, "stream-edit");
        assert!(h.contains("@@ regex files internal/a.go,internal/b.go") && h.contains("ResolveAccountV2("), "{h}");
        assert!(edit_script("grep -rlw Old --include='*.ts' src | xargs perl -pi -e 's/\\bOld\\b/New/g'").is_some());
        assert!(edit_script("find src -name '*.py' -exec sed -i 's/a/b/' {} +").is_some());
        assert!(edit_script("python3 - <<'EOF'\np='web/src/lib/types.ts'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF").is_some());
        assert!(edit_script("node -e \"const fs=require('fs');let s=fs.readFileSync('app.js','utf8');fs.writeFileSync('app.js',s.replace(/x/g,'y'))\"").is_some());
        // left alone: reads, data/doc generators, temp files, mt itself
        assert!(edit_script("sed -n '10,20p' internal/a.go").is_none());
        assert!(edit_script("python3 - <<'EOF'\nimport json\nm=json.load(open('art/manifest.json'))\nopen('art/out.json','w').write(json.dumps(m).replace('a','b'))\nEOF").is_none());
        assert!(edit_script("sed -i '' 's/a/b/' README.md docs/guide.md").is_none());
        assert!(edit_script("python3 -c \"s=open('/tmp/x.go').read().replace('a','b'); open('/tmp/x.go','w').write(s)\"").is_none());
        assert!(edit_script("mt apply - <<'EOF'\n@@ regex in src ext ts\nold\n@@ with\nnew\nEOF").is_none());
    }

    use super::*;

    #[test]
    fn reference_splits_into_parts_that_fit() {
        let body = format!("# mt\nintro\n\n## A\n{}\n\n## B\n{}\n\n## C ✓→\n{}\n", "a".repeat(4000), "b".repeat(4000), "c".repeat(3000));
        let parts = split_sections(&body, 9000);
        assert_eq!(parts.len(), 2, "{:?}", parts.iter().map(|p| utf16(p)).collect::<Vec<_>>());
        assert!(parts.iter().all(|p| utf16(p) <= 9000) && parts[0].contains("## B") && parts[1].starts_with("## C"));
        // one section bigger than a part is split at paragraphs and lines, nothing lost
        let big = format!("## Big\n{}\n", (0..400).map(|i| format!("line {i} {}", "z".repeat(40))).collect::<Vec<_>>().join("\n"));
        let parts = split_sections(&big, 5000);
        assert!(parts.len() >= 4 && parts.iter().all(|p| utf16(p) <= 5000));
        assert_eq!(parts.join("\n").matches("line ").count(), 400);
    }

    #[test]
    fn shell_words_and_stages() {
        assert_eq!(words(r#"sed -n '10,20p' "a b.go""#).unwrap(), vec!["sed", "-n", "10,20p", "a b.go"]);
        assert_eq!(words(r#"grep "x\|y" -r ."#).unwrap(), vec!["grep", r"x\|y", "-r", "."]);
        assert!(words("echo 'open").is_none());
        assert_eq!(first_stage("grep -n 'a|b' f.go | head -5"), "grep -n 'a|b' f.go");
        assert_eq!(first_stage("a || b"), "a || b");
    }

    #[test]
    fn suggestions() {
        let dir = std::env::temp_dir().join(format!("mt-hook-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".mt"));
        let src = "package a\n\nfunc One() {\n\treturn\n}\n\nfunc Two() int {\n\treturn 2\n}\n";
        std::fs::write(dir.join("a.go"), src).unwrap();
        let root = index::find_root(&dir);
        // a lone sed range: rewritten in place to the covering definition
        let s = suggest("sed -n 7,9p a.go", &root, &root, None).unwrap();
        assert!(s.rewrite && s.cmds == vec!["mt span a.go Two"], "{:?}", s.cmds);
        // piped: same command, but denied rather than rewritten (the pipe expects sed's output)
        let s = suggest("sed -n 7,9p a.go | grep return", &root, &root, None).unwrap();
        assert!(!s.rewrite);
        // cat of a small code file: let through (a denial would cost a turn)
        assert!(suggest("cat a.go", &root, &root, None).is_none());
        // cat of a big one: skeleton plus top-level spans, denied, never rewritten
        let big: String = (0..400).map(|i| format!("func F{i}() {{\n}}\n")).collect();
        std::fs::write(root.join("big.go"), format!("package a\n{big}")).unwrap();
        let s = suggest("cat big.go", &root, &root, None).unwrap();
        assert!(!s.rewrite && s.cmds[0] == "mt skeleton big.go" && s.cmds.len() == 5, "{:?}", s.cmds);
        // grep -A on a func: rewritten to the span
        let s = suggest("grep -n -A20 'func Two' a.go", &root, &root, None).unwrap();
        assert!(s.rewrite && s.cmds == vec!["mt span a.go Two"], "{:?}", s.cmds);
        // repo-wide grep: up to four names are rewritten (all must resolve, else the grep runs)
        let s = suggest(r#"grep -rn "Two" ."#, &root, &root, None).unwrap();
        assert!(s.rewrite && s.cmds == vec!["mt context Two --edit"], "{:?}", s.cmds);
        let s = suggest(r#"grep -rn "One\|Two" ."#, &root, &root, None).unwrap();
        assert!(s.rewrite && s.cmds == vec!["mt context One --edit && mt context Two --edit"], "{:?}", s.cmds);
        let s = suggest(r#"grep -rn "One\|Two" . | head"#, &root, &root, None).unwrap();
        assert!(!s.rewrite && s.cmds == vec!["mt context One", "mt context Two"], "{:?}", s.cmds);
        // a code read inside a longer command: denied, not rewritten
        let s = suggest("go build ./... && grep -rn Two .", &root, &root, None).unwrap();
        assert!(!s.rewrite);
        // pointer table wins over definitions; nested items are dropped
        let sid = format!("hook-test-{}", std::process::id());
        let sess = query::session_file(Some(&sid)).unwrap();
        std::fs::write(sess.with_extension("ptrs"), "4\ta.go\t7\t9\tfunction\tTwo\tab\n5\ta.go\t8\t8\tvar\tx\tcd\n").unwrap();
        let s = suggest("sed -n 7,9p a.go", &root, &root, Some(&sid)).unwrap();
        assert_eq!(s.cmds, vec!["mt expand 4"]);
        // grep -A matched by the file's own definitions, any spelling of the pattern
        let s = suggest(r#"grep -n -A12 "^func (s \*X) Two\b" a.go"#, &root, &root, None).unwrap();
        assert_eq!(s.cmds, vec!["mt span a.go Two"]);
        // a sed range on config or docs is left alone
        std::fs::write(root.join("cfg.json"), "{\"a\": 1}\n").unwrap();
        assert!(suggest("sed -n 1,1p cfg.json", &root, &root, None).is_none());
        // a code read next to other work is never denied: the mt call in it would be lost
        let s = suggest("mt expand 3; grep -rn Two .", &root, &root, None).unwrap();
        assert!(!s.only_reads && !s.rewrite);
        let s = suggest("grep -rn One . ; grep -rn Two .", &root, &root, None).unwrap();
        assert!(s.only_reads);
        // head of a few lines passes whatever the file's size
        assert!(suggest("head -20 big.go", &root, &root, None).is_none());
        assert!(suggest("tail -n 50 big.go", &root, &root, None).is_none());
        assert!(suggest("head -n 400 big.go", &root, &root, None).is_some());
        // not code reads: nothing
        assert!(suggest("go test ./...", &root, &root, None).is_none());
        assert!(suggest("grep -rn \"some text here\" docs", &root, &root, None).is_none());
        let _ = std::fs::remove_file(sess.with_extension("ptrs"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
