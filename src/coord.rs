//! Agent coordination: what one agent hands another, and what keeps agents working in parallel
//! from redoing or clobbering each other's work. Every piece of state is a file under
//! `<repo>/.mt/coord/`, so every agent sees it, whether it talks to its own session daemon, a
//! shared one, or none.
//!
//! - **Identity**: a subagent shares its parent's session id (the same environment), so the hook
//!   exports `MT_AGENT` into the subagent's mt commands and mt keys its state by
//!   `<session>~<agent>`. Without that, parent and subagents shared one seen log and one pointer
//!   table: dedupe hid code from a subagent that had never seen it, and siblings renumbered each
//!   other's `mt expand N`.
//! - **Briefs**: `mt brief save <name> <items>`; the receiver runs `mt expand @<name>` and gets
//!   every span in one call, hash-checked (MOVED / CHANGED when the code moved on). The hook saves
//!   one automatically when an agent is spawned with a prompt that names code the parent read.
//! - **Reports**: `mt brief report <name> -` stores the receiver's claims (`mt query` JSON with
//!   `expect`); `mt brief verify <name>` re-checks them and lists what other agents changed since
//!   the brief, so the parent verifies in one call instead of re-reading.
//! - **Changes**: every `mt apply` appends what it changed to `changes.jsonl`. `mt changes` lists
//!   what *other* agents changed in code this agent read; any mt reply carries a one-line notice
//!   while there is something new.
//! - **Leases**: `mt claim`, `mt release`, `mt claims`. `mt apply` refuses a batch that edits
//!   inside another agent's live lease (`MT_LEASE=off` overrides). Hash checks already refuse
//!   stale edits; leases stop the second agent before it does the work.
//! - **Split**: `mt split <targets> --agents N`: slices with no code-graph edge between them,
//!   one brief per slice.

use crate::index::Store;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn dir(root: &Path) -> PathBuf {
    root.join(".mt/coord")
}

/// Milliseconds: a change and the cursor that acknowledges it can fall in the same second.
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

fn ensure(root: &Path) -> PathBuf {
    let d = dir(root);
    let _ = std::fs::create_dir_all(d.join("briefs"));
    d
}

/// Write via a temp file and rename, so a reader never sees half a file.
fn write_atomic(p: &Path, s: &str) -> Result<(), String> {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, s).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| format!("{}: {e}", p.display()))
}

/// This process's identity: the session, plus the agent when the hook said which one.
pub fn me() -> String {
    crate::query::session_id().unwrap_or_else(|| "anonymous".into())
}

/// The session id with the agent's id attached: `<session>~<agent>`.
pub fn with_agent(sid: String, agent: Option<String>) -> String {
    match agent.filter(|a| !a.is_empty()) {
        Some(a) if !sid.ends_with(&format!("~{a}")) => format!("{sid}~{a}"),
        _ => sid,
    }
}

/// The hook's view of who is calling: the event's session, plus `agent_id` inside a subagent.
pub fn hook_sid(d: &serde_json::Value) -> Option<String> {
    let s = d.get("session_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    Some(with_agent(s.to_string(), d.get("agent_id").and_then(|v| v.as_str()).map(String::from)))
}

/// Short, readable name for an owner: `agent a1b2c3d4` or `session 1f3ca6ab`.
pub fn label(sid: &str) -> String {
    match sid.rsplit_once('~') {
        Some((_, a)) => format!("agent {}", a.chars().take(10).collect::<String>()),
        None => format!("session {}", sid.chars().take(8).collect::<String>()),
    }
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t) / 1000;
    if d < 60 { format!("{d}s ago") } else if d < 3600 { format!("{}m ago", d / 60) } else { format!("{}h ago", d / 3600) }
}

fn lines_of(root: &Path, path: &str) -> Vec<String> {
    std::fs::read(root.join(path)).map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect()).unwrap_or_default()
}

fn hash(lines: &[String], s0: usize, e0: usize) -> String {
    if lines.is_empty() { return String::new(); }
    crate::context::span_hash(lines, s0.min(lines.len() - 1), e0.min(lines.len() - 1))
}

/// One span of code, 1-based inclusive lines, with the hash it had when it was recorded.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct Row {
    pub path: String,
    pub s: usize,
    pub e: usize,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub hash: String,
}

impl Row {
    pub fn handle(&self) -> String {
        if self.hash.is_empty() { format!("{}:L{}-{}", self.path, self.s, self.e) } else { format!("{}:L{}-{}@{}", self.path, self.s, self.e, self.hash) }
    }
    fn what(&self) -> String {
        if self.name.is_empty() { self.kind.clone() } else { format!("{} {}", self.kind, self.name) }
    }
}

/// Where a recorded span is now: found again by name when the file moved under it.
fn current(root: &Path, r: &Row) -> (usize, usize) {
    if r.name.is_empty() { return (r.s, r.e); }
    let src = std::fs::read_to_string(root.join(&r.path)).unwrap_or_default();
    let defs = crate::definitions(Path::new(&r.path), &src);
    defs.iter()
        .filter(|d| d.name == r.name || r.name.ends_with(&format!(".{}", d.name)))
        .min_by_key(|d| d.start.abs_diff(r.s.saturating_sub(1)))
        .map(|d| (d.start + 1, d.end + 1))
        .unwrap_or((r.s, r.e))
}

/// The definition that exactly covers lines `s..=e` (1-based), for a span given only by lines.
fn def_at(path: &str, src: &str, s: usize, e: usize) -> Option<crate::Def> {
    crate::definitions(Path::new(path), src).into_iter().find(|d| d.start + 1 == s && d.end + 1 == e)
}

// ------------------------------------------------------------------ items

/// Turn what an agent typed into spans: `3` / `#3` (the last reply's items), `last` (all of
/// them), `seen` / `seen:N` (the spans read most recently), `path:L10-20[@hash]`,
/// `path:Name`, `path` (the whole file), `@brief`.
pub fn resolve(root: &Path, sid: Option<&str>, items: &[String]) -> (Vec<Row>, Vec<String>) {
    let mut rows = vec![];
    let mut errs = vec![];
    let ptrs: Vec<Vec<String>> = crate::query::session_file(sid)
        .and_then(|p| std::fs::read_to_string(p.with_extension("ptrs")).ok())
        .map(|t| t.lines().map(|l| l.split('\t').map(String::from).collect::<Vec<_>>()).filter(|r| r.len() == 7).collect())
        .unwrap_or_default();
    let from_ptr = |r: &Vec<String>| Row { path: r[1].clone(), s: r[2].parse().unwrap_or(1), e: r[3].parse().unwrap_or(1), kind: r[4].clone(), name: r[5].clone(), hash: r[6].clone() };
    for it in items.iter().flat_map(|i| i.split_whitespace().map(String::from).collect::<Vec<_>>()) {
        let t = it.trim_matches(|c| c == '[' || c == ']' || c == ',');
        if t.is_empty() { continue; }
        if t == "last" {
            if ptrs.is_empty() { errs.push("last: no numbered mt reply in this session yet".into()); }
            rows.extend(ptrs.iter().map(from_ptr));
            continue;
        }
        if t == "seen" || t.starts_with("seen:") {
            let n = t.strip_prefix("seen:").and_then(|n| n.parse().ok()).unwrap_or(20);
            let Some(sid) = sid.map(String::from).or_else(crate::query::session_id) else { errs.push("seen: no session".into()); continue };
            let hs = crate::vctx::recent_spans(&sid, n);
            if hs.is_empty() { errs.push("seen: this session has read no spans with mt yet".into()); }
            for h in hs {
                if let Some((p, s, e, hh)) = crate::query::parse_handle(&h) {
                    let src = std::fs::read_to_string(root.join(&p)).unwrap_or_default();
                    let d = def_at(&p, &src, s + 1, e + 1);
                    rows.push(Row { path: p, s: s + 1, e: e + 1, kind: d.as_ref().map(|d| d.kind.clone()).unwrap_or_else(|| "lines".into()), name: d.map(|d| d.name).unwrap_or_default(), hash: hh.unwrap_or_default() });
                }
            }
            continue;
        }
        if let Some(b) = t.strip_prefix('@') {
            match at_rows(root, &format!("@{b}")) { Some(Ok(r)) => rows.extend(r), Some(Err(e)) => errs.push(e), None => {} }
            continue;
        }
        if let Some(n) = crate::vctx::code(t) {
            match ptrs.iter().find(|r| r[0] == n.to_string()) {
                Some(r) => rows.push(from_ptr(r)),
                None => errs.push(format!("{t}: the last mt reply in this session has no item [{n}]")),
            }
            continue;
        }
        if let Some((p, s, e, h)) = crate::query::parse_handle(t) {
            let p = Path::new(&p).strip_prefix(root).map(|x| x.to_string_lossy().into_owned()).unwrap_or(p);
            let src = std::fs::read_to_string(root.join(&p)).unwrap_or_default();
            if src.is_empty() { errs.push(format!("{t}: {p} is missing or empty")); continue; }
            let lines: Vec<String> = src.lines().map(String::from).collect();
            let d = def_at(&p, &src, s + 1, e + 1);
            rows.push(Row { path: p, s: s + 1, e: e + 1, kind: d.as_ref().map(|d| d.kind.clone()).unwrap_or_else(|| "lines".into()), name: d.map(|d| d.name).unwrap_or_default(), hash: h.unwrap_or_else(|| hash(&lines, s, e)) });
            continue;
        }
        // path:Name, or a whole file
        let (p, name) = match t.rsplit_once(':') { Some((p, n)) if root.join(p).is_file() => (p.to_string(), Some(n.to_string())), _ => (t.to_string(), None) };
        let p = Path::new(&p).strip_prefix(root).map(|x| x.to_string_lossy().into_owned()).unwrap_or(p);
        let Ok(src) = std::fs::read_to_string(root.join(&p)) else { errs.push(format!("{t}: not a pointer number, span, file or path:Name")); continue };
        let lines: Vec<String> = src.lines().map(String::from).collect();
        match name {
            None => rows.push(Row { path: p, s: 1, e: lines.len().max(1), kind: "file".into(), name: String::new(), hash: hash(&lines, 0, lines.len().saturating_sub(1)) }),
            Some(n) => {
                let defs = crate::definitions(Path::new(&p), &src);
                let full = |d: &crate::Def| if d.path.is_empty() { d.name.clone() } else { format!("{}.{}", d.path.join("."), d.name) };
                let hits: Vec<&crate::Def> = defs.iter().filter(|d| d.name == n || full(d) == n).collect();
                if hits.is_empty() { errs.push(format!("{t}: no definition named {n} in {p}")); }
                for d in hits { rows.push(Row { path: p.clone(), s: d.start + 1, e: d.end + 1, kind: d.kind.clone(), name: d.name.clone(), hash: hash(&lines, d.start, d.end) }); }
            }
        }
    }
    let mut seen = BTreeSet::new();
    rows.retain(|r| seen.insert((r.path.clone(), r.s, r.e)));
    (rows, errs)
}

// ------------------------------------------------------------------ briefs

#[derive(Serialize, Deserialize, Default)]
pub struct Brief {
    pub name: String,
    pub by: String,
    pub at: u64,
    #[serde(default)]
    pub note: String,
    pub rows: Vec<Row>,
    #[serde(default)]
    pub report: Option<Report>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Report {
    pub by: String,
    pub at: u64,
    #[serde(default)]
    pub note: String,
    /// `mt query` input: the receiver's claims about the code after its work
    #[serde(default)]
    pub queries: serde_json::Value,
}

fn brief_path(root: &Path, name: &str) -> PathBuf {
    dir(root).join("briefs").join(format!("{}.json", safe(name)))
}

pub fn load_brief(root: &Path, name: &str) -> Result<Brief, String> {
    let p = brief_path(root, name);
    let s = std::fs::read_to_string(&p).map_err(|_| format!("@{name}: no brief by that name (`mt brief` lists them)"))?;
    serde_json::from_str(&s).map_err(|e| format!("@{name}: {e}"))
}

fn store_brief(root: &Path, b: &Brief) -> Result<(), String> {
    ensure(root);
    write_atomic(&brief_path(root, &b.name), &serde_json::to_string_pretty(b).unwrap_or_default())
}

pub fn save_brief(root: &Path, sid: &str, name: &str, rows: Vec<Row>, note: &str) -> Result<String, String> {
    if safe(name).is_empty() { return Err("mt brief save: give the brief a name ([A-Za-z0-9_-])".into()); }
    if rows.is_empty() { return Err("mt brief save: nothing to save".into()); }
    // a span inside another saved span of the same file adds nothing but a second copy
    let all = rows.clone();
    let rows: Vec<Row> = rows.into_iter().filter(|r| !all.iter().any(|o| o.path == r.path && (o.s, o.e) != (r.s, r.e) && o.s <= r.s && r.e <= o.e)).collect();
    let n = rows.len();
    let lines: usize = rows.iter().map(|r| r.e + 1 - r.s.min(r.e)).sum();
    store_brief(root, &Brief { name: safe(name), by: sid.to_string(), at: now(), note: note.to_string(), rows, report: None })?;
    Ok(format!("brief @{} saved: {n} span(s), {lines} lines. The receiving agent reads all of it in one call: `mt expand @{}`\n", safe(name), safe(name)))
}

pub fn list_briefs(root: &Path) -> String {
    let mut v: Vec<Brief> = std::fs::read_dir(dir(root).join("briefs")).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str(&s).ok()))
        .collect();
    if v.is_empty() { return "no briefs in this repo (`mt brief save <name> <items>`)\n".into(); }
    v.sort_by_key(|b| std::cmp::Reverse(b.at));
    let mut out = format!("# {} brief(s), newest first\n", v.len());
    for b in v {
        out.push_str(&format!("@{}  {} span(s), by {}, {}{}{}\n", b.name, b.rows.len(), label(&b.by), ago(b.at),
            if b.report.is_some() { ", reported" } else { "" }, if b.note.is_empty() { String::new() } else { format!(": {}", b.note) }));
    }
    out
}

/// `@name` → the brief's spans; `@name/changes` → what other agents changed since it was saved.
/// None: not an `@` token.
pub fn at_rows(root: &Path, tok: &str) -> Option<Result<Vec<Row>, String>> {
    let t = tok.strip_prefix('@')?;
    let (name, part) = t.split_once('/').unwrap_or((t, ""));
    Some(load_brief(root, name).and_then(|b| match part {
        "" => Ok(b.rows),
        "changes" => Ok(changed_since(root, b.at, |by| by != b.by).into_iter().map(|(_, r)| r).collect()),
        other => Err(format!("@{name}/{other}: unknown part (use @{name} or @{name}/changes)")),
    }))
}

pub fn report(root: &Path, sid: &str, name: &str, note: &str, queries: Option<serde_json::Value>) -> Result<String, String> {
    let mut b = load_brief(root, name)?;
    if let Some(q) = &queries {
        let ok = q.is_array() || q.get("queries").is_some_and(|x| x.is_array());
        if !ok { return Err("mt brief report: claims must be `mt query` JSON: an array, or {\"queries\": [...]}".into()); }
    }
    b.report = Some(Report { by: sid.to_string(), at: now(), note: note.to_string(), queries: queries.unwrap_or(serde_json::Value::Null) });
    store_brief(root, &b)?;
    Ok(format!("report on @{name} saved; the parent checks it in one call: `mt brief verify {name}`\n"))
}

/// The text part of `mt brief verify`, plus the stored claims for `mt query` to check.
pub fn verify(root: &Path, name: &str) -> Result<(String, Option<String>), String> {
    let b = load_brief(root, name)?;
    let mut out = format!("# mt brief verify @{name}: saved by {} {}\n", label(&b.by), ago(b.at));
    match &b.report {
        Some(r) => out.push_str(&format!("report by {} {}{}\n", label(&r.by), ago(r.at), if r.note.is_empty() { String::new() } else { format!(": {}", r.note) })),
        None => out.push_str("no report yet (the receiver runs `mt brief report <name> [--note ..] - <<'EOF' [claims] EOF`)\n"),
    }
    let ch = changed_since(root, b.at, |by| by != b.by);
    if ch.is_empty() {
        out.push_str("## changed since the brief, by other agents: nothing (through mt apply)\n");
    } else {
        out.push_str(&format!("## changed since the brief, by other agents: {} span(s)\n", ch.len()));
        for (c, r) in &ch {
            let lines = lines_of(root, &r.path);
            let (s, e) = current(root, r);
            let now_h = hash(&lines, s - 1, e - 1);
            let state = if now_h == r.hash { "as written".to_string() } else { format!("CHANGED again since (now L{s}-{e} h={now_h})") };
            out.push_str(&format!("  {} {}  by {} {}, {state}\n", r.handle(), r.what(), label(&c.by), ago(c.at)));
        }
        out.push_str(&format!("read them in one call: `mt expand @{name}/changes`\n"));
    }
    let q = b.report.as_ref().map(|r| &r.queries).filter(|q| !q.is_null()).map(|q| q.to_string());
    if q.is_some() { out.push_str("## the receiver's claims, checked against the code now:\n"); }
    Ok((out, q))
}

// ------------------------------------------------------------------ changes

#[derive(Serialize, Deserialize, Clone)]
pub struct Change {
    pub at: u64,
    pub by: String,
    #[serde(flatten)]
    pub row: Row,
}

/// Changed lines of `new` against `old`, grouped and widened to the smallest enclosing
/// definition: 1-based (start, end, kind, name) in the new text.
pub fn changed_blocks(path: &str, old: &str, new: &str) -> Vec<(usize, usize, String, String)> {
    let nl: Vec<&str> = new.lines().collect();
    if nl.is_empty() { return vec![]; }
    let diff = similar::TextDiff::from_lines(old, new);
    let mut changed: Vec<usize> = vec![];
    for op in diff.ops() {
        match op.tag() {
            similar::DiffTag::Insert | similar::DiffTag::Replace => changed.extend(op.new_range()),
            // a deletion marks the line now in its place
            similar::DiffTag::Delete => changed.push(op.new_range().start.min(nl.len() - 1)),
            _ => {}
        }
    }
    changed.retain(|&l| l < nl.len());
    changed.sort_unstable();
    changed.dedup();
    let defs = crate::definitions(Path::new(path), new);
    let mut out: Vec<(usize, usize, String, String)> = vec![];
    for l in changed {
        let d = defs.iter().filter(|d| d.start <= l && l <= d.end).min_by_key(|d| d.end - d.start);
        let (s, e, k, n) = match d { Some(d) => (d.start, d.end, d.kind.clone(), d.name.clone()), None => (l, l, "lines".to_string(), String::new()) };
        match out.last_mut() {
            Some(b) if b.3 == n && !n.is_empty() && s + 1 <= b.1 + 1 => {}
            Some(b) if n.is_empty() && b.3.is_empty() && s <= b.1 + 4 => b.1 = b.1.max(e + 1),
            _ => out.push((s + 1, e + 1, k, n)),
        }
    }
    out
}

/// After a successful `mt apply`: one line per changed block, for every other agent's feed.
pub fn note_change(root: &Path, sid: Option<&str>, files: &[(String, Option<String>, String)]) {
    let by = sid.map(String::from).unwrap_or_else(me);
    let at = now();
    let mut s = String::new();
    for (path, old, new) in files {
        let lines: Vec<String> = new.lines().map(String::from).collect();
        let blocks = match old {
            Some(o) => changed_blocks(path, o, new),
            None => vec![(1, lines.len().max(1), "file".to_string(), String::new())],
        };
        for (bs, be, kind, name) in blocks {
            let row = Row { path: path.clone(), s: bs, e: be, kind, name, hash: hash(&lines, bs - 1, be - 1) };
            s.push_str(&serde_json::to_string(&Change { at, by: by.clone(), row }).unwrap_or_default());
            s.push('\n');
        }
    }
    if s.is_empty() { return; }
    let d = ensure(root);
    // one write per batch with O_APPEND: lines from agents writing at once never interleave
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(d.join("changes.jsonl")) {
        use std::io::Write;
        let _ = f.write_all(s.as_bytes());
    }
}

fn read_changes(root: &Path) -> Vec<Change> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(dir(root).join("changes.jsonl")) else { return vec![] };
    // ponytail: the last 1 MB is the feed; rotate the file if a repo ever outgrows that
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(1 << 20);
    let _ = f.seek(SeekFrom::Start(from));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    s.lines().skip(usize::from(from > 0)).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Changes after `t` by agents `who` accepts, newest per span first.
fn changed_since(root: &Path, t: u64, who: impl Fn(&str) -> bool) -> Vec<(Change, Row)> {
    let mut seen = BTreeSet::new();
    let mut v: Vec<(Change, Row)> = read_changes(root).into_iter().rev()
        .filter(|c| c.at > t && who(&c.by))
        .filter(|c| seen.insert((c.row.path.clone(), c.row.name.clone(), if c.row.name.is_empty() { c.row.s } else { 0 })))
        .map(|c| { let r = c.row.clone(); (c, r) })
        .collect();
    v.truncate(200);
    v
}

/// The spans this agent read with mt: path → (1-based range, the hash it read), newest last.
fn my_reads(sid: &str) -> HashMap<String, Vec<(usize, usize, String)>> {
    let mut m: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
    let txt = crate::query::session_file(Some(sid)).and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    for l in txt.lines() {
        let r: Vec<&str> = l.split('|').collect();
        if let (Some(p), Some(Ok(s)), Some(Ok(e))) = (r.first(), r.get(1).map(|x| x.parse::<usize>()), r.get(2).map(|x| x.parse::<usize>())) {
            m.entry(p.to_string()).or_default().push((s + 1, e + 1, r.get(3).unwrap_or(&"").to_string()));
        }
    }
    m
}

fn cursor_path(root: &Path, sid: &str) -> PathBuf {
    dir(root).join(format!("cursor-{}", safe(sid)))
}

fn cursor(root: &Path, sid: &str) -> Option<u64> {
    std::fs::read_to_string(cursor_path(root, sid)).ok().and_then(|s| s.trim().parse().ok())
}

/// Other agents' changes, after this agent's cursor, in files it read: (change, stale), where
/// stale means a span it read overlaps the change and no longer reads as it did. A change the
/// agent has since re-read (its last read of those lines matches the disk) is not news.
fn pending(root: &Path, sid: &str, since: u64, all: bool) -> Vec<(Change, bool)> {
    let reads = my_reads(sid);
    let mut files: HashMap<String, Vec<String>> = HashMap::new();
    changed_since(root, since, |by| by != sid).into_iter().filter_map(|(c, r)| {
        let Some(v) = reads.get(&r.path) else { return all.then_some((c, false)) };
        let lines = files.entry(r.path.clone()).or_insert_with(|| lines_of(root, &r.path));
        // the newest read of lines overlapping the change decides: stale if the disk differs now
        let last = v.iter().rev().find(|(s, e, _)| *s <= r.e && r.s <= *e);
        let stale = last.is_some_and(|(s, e, h)| !h.is_empty() && hash(lines, s - 1, e - 1) != *h);
        let reread = last.is_some() && !stale;
        (!reread || all).then_some((c, stale))
    }).collect()
}

/// `mt changes`: what other agents changed, in code this agent read, since it last asked.
pub fn changes(root: &Path, sid: &str, all: bool, keep_cursor: bool) -> String {
    let since = cursor(root, sid).unwrap_or(0);
    let p = pending(root, sid, since, all);
    if !keep_cursor { ensure(root); let _ = write_atomic(&cursor_path(root, sid), &now().to_string()); }
    if p.is_empty() {
        return format!("# mt changes: nothing new by other agents{} since {}\n", if all { "" } else { " in code you read" }, if since == 0 { "the start".into() } else { ago(since) });
    }
    let mut out = format!("# mt changes: {} span(s) changed by other agents since {}{}\n", p.len(), if since == 0 { "the start".into() } else { ago(since) }, if all { "" } else { ", in files you read" });
    let mut by: BTreeMap<String, Vec<(Change, bool)>> = BTreeMap::new();
    for x in p { by.entry(x.0.by.clone()).or_default().push(x); }
    let mut handles = vec![];
    for (who, v) in by {
        out.push_str(&format!("{}:\n", label(&who)));
        for (c, overlap) in v {
            let lines = lines_of(root, &c.row.path);
            let (s, e) = current(root, &c.row);
            let state = if hash(&lines, s - 1, e - 1) == c.row.hash { "" } else { "  (changed again since)" };
            out.push_str(&format!("  {} {} {}{}{state}\n", c.row.handle(), c.row.what(), ago(c.at), if overlap { "  OVERLAPS code you read (stale in your context)" } else { "" }));
            handles.push(c.row.handle());
        }
    }
    handles.truncate(20);
    out.push_str(&format!("read the current code in one call: `mt expand {}`\n", handles.join(" ")));
    out
}

/// One line for the end of any mt reply while other agents changed code this agent read.
pub fn notice(root: &Path, sid: &str) -> Option<String> {
    if std::env::var_os("MT_NO_FEED").is_some() || !root.join(".mt").is_dir() { return None; }
    let Some(since) = cursor(root, sid) else {
        // first mt call of this agent: the feed starts now (what it reads from here on is current)
        ensure(root);
        let _ = write_atomic(&cursor_path(root, sid), &now().to_string());
        return None;
    };
    let p = pending(root, sid, since, false);
    let hot = p.iter().filter(|x| x.1).count();
    (hot > 0).then(|| format!("## {hot} span(s) you read were changed by another agent since: `mt changes`\n"))
}

// ------------------------------------------------------------------ leases

#[derive(Serialize, Deserialize, Clone)]
pub struct Lease {
    pub by: String,
    pub until: u64,
    #[serde(default)]
    pub note: String,
    #[serde(flatten)]
    pub row: Row,
}

fn leases_path(root: &Path) -> PathBuf {
    dir(root).join("leases.json")
}

pub fn load_leases(root: &Path) -> Vec<Lease> {
    let t = now();
    std::fs::read_to_string(leases_path(root)).ok().and_then(|s| serde_json::from_str::<Vec<Lease>>(&s).ok()).unwrap_or_default()
        .into_iter().filter(|l| l.until > t).collect()
}

/// Read-modify-write the lease table under an exclusive lock file.
fn with_leases<T>(root: &Path, f: impl FnOnce(&mut Vec<Lease>) -> Result<T, String>) -> Result<T, String> {
    let d = ensure(root);
    let lock = d.join("leases.lock");
    let t0 = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(_) if t0.elapsed() < Duration::from_secs(3) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                // a holder that died leaves its lock: older than 10 s is stale
                let old = std::fs::metadata(&lock).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|e| e > Duration::from_secs(10));
                if old { let _ = std::fs::remove_file(&lock); continue; }
                return Err("mt: the lease table is locked by another agent; try again".into());
            }
        }
    }
    let mut v = load_leases(root);
    let r = f(&mut v);
    if r.is_ok() { let _ = write_atomic(&leases_path(root), &serde_json::to_string_pretty(&v).unwrap_or_default()); }
    let _ = std::fs::remove_file(&lock);
    r
}

fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

fn lease_line(root: &Path, l: &Lease) -> String {
    let (s, e) = current(root, &l.row);
    format!("{}:L{s}-{e} {} held by {} for {} more min{}", l.row.path, l.row.what(), label(&l.by), l.until.saturating_sub(now()).div_ceil(60_000), if l.note.is_empty() { String::new() } else { format!(" ({})", l.note) })
}

/// Claim every span or none: a span inside another agent's live lease refuses the whole claim.
pub fn claim(root: &Path, sid: &str, rows: Vec<Row>, ttl_s: u64, note: &str) -> Result<String, String> {
    if rows.is_empty() { return Err("mt claim: nothing to claim".into()); }
    with_leases(root, |v| {
        let mut clash = vec![];
        for r in &rows {
            for l in v.iter().filter(|l| l.by != sid && l.row.path == r.path) {
                if overlaps(current(root, &l.row), (r.s, r.e)) { clash.push(format!("{} overlaps {}", r.handle(), lease_line(root, l))); }
            }
        }
        if !clash.is_empty() {
            return Err(format!("mt claim: nothing claimed, {} conflict(s):\n  {}\nAsk the holder (SendMessage), wait for its `mt release`, or take other work; `mt claims` lists every claim.", clash.len(), clash.join("\n  ")));
        }
        let until = now() + ttl_s * 1000;
        let n = rows.len();
        for r in rows {
            v.retain(|l| !(l.by == sid && l.row.path == r.path && l.row.s == r.s && l.row.e == r.e));
            v.push(Lease { by: sid.to_string(), until, note: note.to_string(), row: r });
        }
        Ok(format!("claimed {n} span(s) for {} min as {}. Other agents' `mt apply` into them is refused until you `mt release`.\n", ttl_s.div_ceil(60), label(sid)))
    })
}

pub fn release(root: &Path, sid: &str, rows: Option<Vec<Row>>) -> Result<String, String> {
    with_leases(root, |v| {
        let before = v.len();
        v.retain(|l| l.by != sid || rows.as_ref().is_some_and(|rs| !rs.iter().any(|r| r.path == l.row.path && overlaps((r.s, r.e), (l.row.s, l.row.e)))));
        Ok(format!("released {} span(s)\n", before - v.len()))
    })
}

pub fn list_leases(root: &Path) -> String {
    let v = load_leases(root);
    if v.is_empty() { return "no live claims in this repo\n".into(); }
    let mut out = format!("# {} live claim(s)\n", v.len());
    for l in &v { out.push_str(&format!("  {}\n", lease_line(root, l))); }
    out
}

/// `mt apply`'s gate: Some(reason) when the batch edits inside another agent's live lease.
/// `files`: (path, old text or None for a new file, new text).
pub fn lease_conflicts(root: &Path, sid: Option<&str>, files: &[(&str, Option<&str>, &str)]) -> Option<String> {
    if std::env::var("MT_LEASE").as_deref() == Ok("off") { return None; }
    let v = load_leases(root);
    if v.is_empty() { return None; }
    let me = sid.map(String::from).unwrap_or_else(me);
    let mut clash = vec![];
    for (path, old, new) in files {
        let theirs: Vec<&Lease> = v.iter().filter(|l| l.by != me && l.row.path == *path).collect();
        if theirs.is_empty() { continue; }
        let touched: Vec<(usize, usize)> = match old {
            None => vec![(1, usize::MAX)],
            Some(o) => similar::TextDiff::from_lines(*o, *new).ops().iter().filter(|op| op.tag() != similar::DiffTag::Equal).map(|op| {
                let r = op.old_range();
                // an insertion touches the lines on both sides of where it lands
                if r.is_empty() { (r.start.max(1), r.start + 1) } else { (r.start + 1, r.end) }
            }).collect(),
        };
        for l in theirs {
            let span = current(root, &l.row);
            if touched.iter().any(|t| overlaps(*t, span)) { clash.push(lease_line(root, l)); }
        }
    }
    (!clash.is_empty()).then(|| format!(
        "mt apply: nothing written: the batch edits code another agent claimed:\n  {}\nAsk the holder (SendMessage), wait for its `mt release`, or change other code; `mt claims` lists every claim. MT_LEASE=off overrides.",
        clash.join("\n  ")))
}

// ------------------------------------------------------------------ split

/// `mt split`: the target files grouped so that no group references another group's
/// definitions, packed into `n` slices by size, one brief per slice.
pub fn split(g: &dyn Store, root: &Path, sid: &str, targets: &[String], n: usize, prefix: &str) -> Result<String, String> {
    let n = n.max(1);
    // target → files, and the spans to brief per file (a symbol target briefs its definition)
    let mut files: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut errs = vec![];
    for t in targets {
        if root.join(t).is_file() {
            files.entry(t.clone()).or_default();
            continue;
        }
        if root.join(t).is_dir() {
            for (p, _) in g.text_files() { if p.starts_with(&format!("{}/", t.trim_end_matches('/'))) { files.entry(p).or_default(); } }
            continue;
        }
        let (scope, name) = match t.rsplit_once(':') { Some((p, nm)) => (Some(p), nm), None => (None, t.as_str()) };
        let last = name.rsplit('.').next().unwrap_or(name);
        let defs: Vec<_> = g.defs_named(last).into_iter().filter(|d| scope.is_none_or(|p| d.path == p)).collect();
        if defs.is_empty() { errs.push(format!("{t}: no file, directory or definition by that name")); }
        for d in defs {
            let lines = lines_of(root, &d.path);
            files.entry(d.path.clone()).or_default().push(Row { path: d.path.clone(), s: d.start + 1, e: d.end + 1, kind: d.kind.clone(), name: d.name.clone(), hash: hash(&lines, d.start, d.end) });
        }
    }
    if files.is_empty() { return Err(format!("mt split: no targets found\n  {}", errs.join("\n  "))); }
    let names: Vec<String> = files.keys().cloned().collect();
    let idx: HashMap<&str, usize> = names.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let mut parent: Vec<usize> = (0..names.len()).collect();
    fn find(p: &mut Vec<usize>, x: usize) -> usize { let mut r = x; while p[r] != r { r = p[r]; } let mut y = x; while p[y] != r { let nx = p[y]; p[y] = r; y = nx; } r }
    // an edge: one target file mentions a definition another target file defines.
    // ponytail: by name; names defined in more than 3 files or shorter than 3 chars are skipped
    // as noise (new, run, get), so a coupling through such a name is missed
    let mut why: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
    for (i, p) in names.iter().enumerate() {
        for d in g.defs_in(p) {
            if d.name.len() < 3 || g.def_count(&d.name) > 3 { continue; }
            for (rp, _, _) in g.refs(&d.name) {
                if let Some(&j) = idx.get(rp.as_str()) {
                    if j != i {
                        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                        if a != b { parent[a] = b; }
                        why.entry((i.min(j), i.max(j))).or_default().insert(d.name.clone());
                    }
                }
            }
        }
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..names.len() { let r = find(&mut parent, i); comps.entry(r).or_default().push(i); }
    let size = |i: usize| lines_of(root, &names[i]).len().max(1);
    let mut comps: Vec<(usize, Vec<usize>)> = comps.into_values().map(|v| (v.iter().map(|&i| size(i)).sum(), v)).collect();
    comps.sort_by_key(|c| std::cmp::Reverse(c.0));
    let k = n.min(comps.len());
    let mut slices: Vec<(usize, Vec<usize>, usize)> = (0..k).map(|_| (0, vec![], 0)).collect();
    for (sz, members) in comps {
        let s = slices.iter_mut().min_by_key(|s| s.0).expect("k >= 1");
        s.0 += sz;
        s.1.extend(members);
        s.2 += 1;
    }
    let mut out = format!("# mt split: {} file(s) into {k} independent slice(s){}\n", names.len(),
        if k < n { format!(" (asked for {n}: only {k} groups share no definitions; more agents would edit coupled code)") } else { String::new() });
    for (si, (sz, members, ncomp)) in slices.iter().enumerate() {
        let name = format!("{}-{}", safe(prefix), si + 1);
        let mut rows = vec![];
        for &i in members {
            let r = &files[&names[i]];
            if r.is_empty() {
                let lines = lines_of(root, &names[i]);
                rows.push(Row { path: names[i].clone(), s: 1, e: lines.len().max(1), kind: "file".into(), name: String::new(), hash: hash(&lines, 0, lines.len().saturating_sub(1)) });
            } else { rows.extend(r.iter().cloned()); }
        }
        store_brief(root, &Brief { name: name.clone(), by: sid.to_string(), at: now(), note: format!("slice {} of {k} from mt split", si + 1), rows, report: None })?;
        out.push_str(&format!("slice {} → @{name}: {} file(s), {sz} lines, {ncomp} coupled group(s)\n", si + 1, members.len()));
        let set: BTreeSet<usize> = members.iter().copied().collect();
        for &i in members { out.push_str(&format!("  {}\n", names[i])); }
        let inner: Vec<String> = why.iter().filter(|((a, b), _)| set.contains(a) && set.contains(b)).take(6)
            .map(|((a, b), w)| format!("{} ↔ {} via {}", names[*a], names[*b], w.iter().take(3).cloned().collect::<Vec<_>>().join(", "))).collect();
        if !inner.is_empty() { out.push_str(&format!("  kept together: {}\n", inner.join("; "))); }
    }
    out.push_str(&format!("Give each agent its slice: `mt expand @{p}-N` to read it, `mt claim @{p}-N` before editing, `mt brief report {p}-N -` when done.\n", p = safe(prefix)));
    for e in errs { out.push_str(&format!("## {e}\n")); }
    Ok(out)
}

// ------------------------------------------------------------------ the hook's automatic brief

/// Whether a shell command runs mt (as a command word, not inside another word).
pub fn mentions_mt(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    cmd.match_indices("mt ").any(|(i, _)| i == 0 || matches!(b[i - 1], b' ' | b';' | b'&' | b'|' | b'(' | b'`' | b'$' | b'\n' | b'/'))
}

/// PreToolUse on Agent/Task: when the prompt names code the parent already read with mt, save
/// those spans as a brief and tell the subagent to open it in one call. None: nothing to add.
/// Only reading and writing are asked for: asking for a report or claims cost a delegate 2-4 calls
/// in the A/B (DEVLOG 49) and no parent used them; `mt brief verify` works without a report.
pub fn auto_brief(root: &Path, sid: &str, tool_use_id: &str, input: &serde_json::Value) -> Option<serde_json::Value> {
    if std::env::var("MT_AUTO_BRIEF").as_deref() == Ok("0") || !root.join(".mt").is_dir() { return None; }
    let prompt = input.get("prompt").and_then(|v| v.as_str())?;
    if prompt.contains("mt expand @") { return None; }
    // a path ends a sentence as often as not: "see internal/x/y.go." names y.go
    let words: BTreeSet<&str> = prompt.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-'))
        .map(|w| w.trim_matches(|c| c == '.' || c == '/' || c == '-')).filter(|w| w.len() >= 3).collect();
    let mentioned = |r: &Row| {
        let base = r.path.rsplit('/').next().unwrap_or(&r.path);
        words.iter().any(|w| *w == r.path || *w == base || w.ends_with(&format!("/{base}")) || (!r.name.is_empty() && (*w == r.name || w.ends_with(&format!(".{}", r.name)))))
    };
    // what the parent read most recently, and its last reply's pointers
    let (mut rows, _) = resolve(root, Some(sid), &["seen:60".to_string()]);
    let (ptr_rows, _) = resolve(root, Some(sid), &["last".to_string()]);
    rows.extend(ptr_rows);
    let by_name: BTreeSet<String> = rows.iter().filter(|r| !r.name.is_empty() && words.contains(r.name.as_str())).map(|r| r.path.clone()).collect();
    let mut keep: Vec<Row> = rows.into_iter().filter(|r| mentioned(r) && (r.name.is_empty() || by_name.is_empty() || words.contains(r.name.as_str()) || !by_name.contains(&r.path))).collect();
    let mut seen = BTreeSet::new();
    keep.retain(|r| seen.insert((r.path.clone(), r.s, r.e)));
    keep.truncate(24);
    if keep.is_empty() { return None; }
    let name = format!("a-{}", safe(tool_use_id).chars().rev().take(8).collect::<String>().chars().rev().collect::<String>());
    save_brief(root, sid, &name, keep, "saved by the hook when this agent was spawned").ok()?;
    let mut new = input.clone();
    new["prompt"] = serde_json::Value::String(format!(
        "{prompt}\n\nmt brief @{name}: the code named above, as the parent read it, is saved. Read it in ONE call first, instead of searching: `mt expand @{name}` (hash-checked: anything changed since is marked MOVED/CHANGED; its hashes vouch for any lines inside their span). Change code in ONE call: `mt apply - <<'EOF'` with sections `@@ replace <path:Ls-e> <hash>` (new text follows), `@@ find <path>` (exact existing lines) then `@@ with` (their replacement), `@@ create <path>`, `@@ run gate build: <cmd>`, `@@ run test: <cmd>`, then `EOF`."));
    Some(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_widen_to_the_enclosing_definition() {
        let old = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let new = "fn a() {\n    1\n}\n\nfn b() {\n    3\n}\n";
        let b = changed_blocks("x.rs", old, new);
        assert_eq!(b, vec![(5, 7, "function".to_string(), "b".to_string())]);
    }

    #[test]
    fn agent_ids_attach_once() {
        assert_eq!(with_agent("s1".into(), Some("a9".into())), "s1~a9");
        assert_eq!(with_agent("s1~a9".into(), Some("a9".into())), "s1~a9");
        assert_eq!(with_agent("s1".into(), None), "s1");
        assert_eq!(label("s1~abc"), "agent abc");
    }

    #[test]
    fn overlap_is_inclusive() {
        assert!(overlaps((1, 5), (5, 9)));
        assert!(!overlaps((1, 4), (5, 9)));
    }
}
