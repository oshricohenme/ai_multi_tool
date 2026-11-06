//! `mt apply`: every change, across every file, in one call.
//!
//! Input is JSON (file or stdin):
//! {"ops": [
//!   {"span": "path:L10-20", "hash": "3fa2c1d0", "replace": "new text"},
//!   {"span": "path:L10-20", "hash": "3fa2c1d0", "delete": true},
//!   {"span": "path:L10-20", "hash": "3fa2c1d0", "insert_after": "text"},
//!   {"file": "path", "find": "old", "with": "new", "all": false, "span": "path:L..", "hash": ".."},
//!   {"create": "path", "text": "...", "overwrite": false},
//!   {"rename": "Old", "to": "New"}            // word-boundary rename in every indexed file using it
//!   // any body can come from a file instead: replace_file, with_file, text_file, insert_after_file
//!   // (path absolute or relative to the repo root; read verbatim), so large code needs no JSON escaping
//! ], "check": "go build ./...", "dry_run": false}
//!
//! Or a plain-text bundle (no JSON, no escaping): sections start with a line `@@ <op> ...`, and
//! the lines up to the next `@@` line are that section's body (write a body line that starts
//! with `@@ ` as `\@@ `):
//!   @@ create <path> [overwrite]          then the file's text
//!   @@ replace <path:L10-20> <hash>       then the new text of those lines
//!   @@ insert_after <path:L10-20> <hash>  then the text to insert
//!   @@ delete <path:L10-20> <hash>
//!   @@ find <path> [all] [in <path:L1-9> <hash>]   then the old lines; then
//!   @@ with                               then the new lines
//!   @@ rename <Old> <New>
//!   @@ regex [in <prefix>] [ext ts,tsx] [refs <Name>] [files a,b] [expect N]   then the pattern;
//!   @@ with                               then the replacement ($1 captures): the same edit in
//!                                         every matching file, instead of a script looping over them
//!   @@ run [gate] <name>: <cmd>           @@ check <cmd>      @@ parallel      @@ echo
//!   @@ expect                             then a JSON array of `mt query` claims; if any fails or
//!                                         can't be checked, nothing is written
//!   @@ before [<name>]: <cmd>             runs before anything is written (add a dependency,
//!                                         generate code); a failure means nothing is written
//! One call can therefore say: check these, and if they hold, run these, write all of this,
//! format it, build and test it. Order: expect, before, write, format, run.
//!   @@ format: <cmd>                      the repo's formatter, run on exactly the files this
//!                                         batch wrote ({changed}, or appended), before the steps
//!
//! Every op becomes a byte-range replacement on the ORIGINAL file, so ops never shift each
//! other's line numbers. Spans must match the hash they were read with (the `h=` from
//! `mt context` / `mt span`), finds must be unique unless `all`, and ranges must not overlap.
//! Files are validated and written on all cores. Nothing is written unless every op is valid.
//! No backups are written: a failing gate restores the originals from memory. Reverting a
//! finished batch is git's job; mt has no undo.

use crate::fnv1a;
use crate::index::{Index, Store, UpdateStats};
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
struct Batch {
    ops: Vec<Op>,
    /// shorthand for one gate step that runs first
    check: Option<String>,
    /// commands to run after the edits are written, in order: build, tests, linters, anything
    #[serde(default)]
    run: Vec<Step>,
    /// run consecutive non-gate steps concurrently
    #[serde(default)]
    parallel: bool,
    #[serde(default)]
    dry_run: bool,
    /// the repo's formatter, run on exactly the files this batch wrote, before the steps:
    /// `{changed}` expands to them (otherwise they are appended). mt knows no languages.
    format: Option<String>,
    /// show the changed regions' code in the reply (default: span + hash only; you just wrote it)
    #[serde(default)]
    echo: bool,
    /// `mt query` claims that must all hold before anything is written
    expect: Option<serde_json::Value>,
    /// commands that run, in order, before anything is written; any failure writes nothing
    #[serde(default)]
    before: Vec<Step>,
}

/// The plain-text bundle form of a batch, as the JSON the rest of `run` takes.
fn parse_bundle(input: &str) -> Result<serde_json::Value, String> {
    use serde_json::{json, Value};
    let mut ops: Vec<Value> = vec![];
    let mut run: Vec<Value> = vec![];
    let mut before: Vec<Value> = vec![];
    let mut expect: Option<Value> = None;
    let (mut check, mut parallel, mut format, mut echo): (Option<String>, bool, Option<String>, bool) = (None, false, None, false);
    // split into (header words, body)
    let mut secs: Vec<(String, String)> = vec![];
    for line in input.split_inclusive('\n') {
        if let Some(h) = line.strip_prefix("@@ ").or_else(|| (line.trim_end() == "@@").then_some("")) {
            secs.push((h.trim().to_string(), String::new()));
        } else if let Some(last) = secs.last_mut() {
            last.1.push_str(line.strip_prefix("\\@@ ").map(|r| format!("@@ {r}")).as_deref().unwrap_or(line));
        } else if !line.trim().is_empty() {
            return Err(format!("text before the first @@ section: {:?}", line.trim()));
        }
    }
    let mut i = 0;
    while i < secs.len() {
        let (h, body) = &secs[i];
        let w: Vec<&str> = h.split_whitespace().collect();
        let n = i + 1;
        match w.first().copied().unwrap_or("") {
            "create" if w.len() >= 2 => ops.push(json!({"create": w[1], "text": body, "overwrite": w.get(2) == Some(&"overwrite")})),
            "replace" | "insert_after" if w.len() >= 2 => {
                let mut o = json!({"span": w[1]});
                if let Some(hh) = w.get(2) { o["hash"] = json!(hh.trim_start_matches('#')); }
                o[w[0]] = json!(body.strip_suffix('\n').unwrap_or(body));
                ops.push(o);
            }
            "delete" if w.len() >= 2 => {
                let mut o = json!({"span": w[1], "delete": true});
                if let Some(hh) = w.get(2) { o["hash"] = json!(hh.trim_start_matches('#')); }
                ops.push(o);
            }
            "find" if w.len() >= 2 => {
                let Some((wh, wb)) = secs.get(i + 1) else { return Err(format!("section {n}: find needs a following @@ with")) };
                if wh.split_whitespace().next() != Some("with") { return Err(format!("section {n}: find must be followed by @@ with")); }
                let mut o = json!({"file": w[1], "find": body, "with": wb, "all": w.contains(&"all")});
                if let Some(k) = w.iter().position(|x| *x == "in") {
                    if let Some(sp) = w.get(k + 1) { o["span"] = json!(sp); }
                    if let Some(hh) = w.get(k + 2) { o["hash"] = json!(hh.trim_start_matches('#')); }
                }
                ops.push(o);
                i += 1;
            }
            "rename" if w.len() >= 3 => ops.push(json!({"rename": w[1], "to": w[2]})),
            // @@ copy|move <span|#n> [hash] to <file | span | #n>
            "copy" | "move" => {
                let o = match (w.len(), w.get(2), w.get(3)) {
                    (4, Some(&"to"), _) => json!({w[0]: w[1], "to": w[3]}),
                    (5, _, Some(&"to")) => json!({w[0]: w[1], "hash": w[2].trim_start_matches('#'), "to": w[4]}),
                    _ => return Err(format!("section {n}: want \"@@ {} <span|#n> [hash] to <file|span|#n>\", got \"@@ {h}\"", w[0])),
                };
                ops.push(o);
            }
            // @@ regex [in <prefix>] [ext ts,tsx] [refs <Name>] [files a,b] [expect N]: the pattern
            // is the body, the replacement the body of the @@ with that follows ($1 works)
            "regex" => {
                let Some((wh, wb)) = secs.get(i + 1) else { return Err(format!("section {n}: regex needs a following @@ with")) };
                if wh.split_whitespace().next() != Some("with") { return Err(format!("section {n}: regex must be followed by @@ with")); }
                let pat = body.strip_suffix('\n').unwrap_or(body);
                let mut o = json!({"regex": pat, "with": wb.strip_suffix('\n').unwrap_or(wb)});
                let mut k = 1;
                while k < w.len() {
                    match (w[k], w.get(k + 1)) {
                        ("in", Some(v)) => { o["in"] = json!(v); k += 2; }
                        ("refs", Some(v)) => { o["refs"] = json!(v); k += 2; }
                        ("ext", Some(v)) => { o["ext"] = json!(v.split(',').collect::<Vec<_>>()); k += 2; }
                        ("files", Some(v)) => { o["files"] = json!(v.split(',').collect::<Vec<_>>()); k += 2; }
                        ("expect", Some(v)) => { o["expect"] = json!(v.parse::<usize>().map_err(|_| format!("section {n}: expect needs a number"))?); k += 2; }
                        (x, _) => return Err(format!("section {n}: regex: unknown word {x:?} (in, ext, refs, files, expect)")),
                    }
                }
                ops.push(o);
                i += 1;
            }
            "run" => {
                let rest = h.strip_prefix("run").unwrap_or("").trim();
                let (gate, rest) = match rest.strip_prefix("gate ") { Some(r) => (true, r.trim()), None => (false, rest) };
                let (name, cmd) = rest.split_once(": ").map(|(a, b)| (a.trim().to_string(), b.trim().to_string())).unwrap_or((String::new(), rest.to_string()));
                let mut st = json!({"cmd": cmd, "gate": gate});
                if !name.is_empty() { st["name"] = json!(name); }
                run.push(st);
            }
            "check" => check = Some(h.strip_prefix("check").unwrap_or("").trim().to_string()),
            "before" | "before:" => {
                let rest = h.strip_prefix("before").unwrap_or("").trim().trim_start_matches(':').trim();
                let (name, cmd) = rest.split_once(": ").map(|(a, b)| (a.trim().to_string(), b.trim().to_string())).unwrap_or((String::new(), rest.to_string()));
                let mut st = json!({"cmd": cmd});
                if !name.is_empty() { st["name"] = json!(name); }
                before.push(st);
            }
            "expect" => {
                let v: Value = serde_json::from_str(body.trim()).map_err(|e| format!("section {n}: @@ expect needs a JSON array of mt query claims: {e}"))?;
                expect = Some(v);
            }
            "parallel" => parallel = true,
            "format" | "format:" => format = Some(h.split_once(':').map(|x| x.1).unwrap_or("").trim().to_string()).filter(|c| !c.is_empty()),
            "echo" => echo = true,
            "with" => return Err(format!("section {n}: @@ with without a @@ find before it")),
            other => return Err(format!("section {n}: unknown or incomplete section \"@@ {h}\" ({other})")),
        }
        i += 1;
    }
    let mut b = json!({"ops": ops, "run": run, "parallel": parallel, "format": format, "echo": echo, "before": before});
    if let Some(e) = expect { b["expect"] = e; }
    if let Some(c) = check { b["check"] = json!(c); }
    Ok(b)
}

/// Run the repo's own formatter on exactly the files this batch wrote; returns a report line and
/// the files whose text the formatter changed.
fn format_written(root: &Path, cmd: &str, paths: &[String]) -> (String, Vec<(String, String)>) {
    if paths.is_empty() { return (String::new(), vec![]); }
    let quoted = paths.iter().map(|x| format!("'{}'", x.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
    let full = if cmd.contains("{changed}") { cmd.replace("{changed}", &quoted) } else { format!("{cmd} {quoted}") };
    let t = Instant::now();
    let before: Vec<String> = paths.iter().map(|p| fs::read_to_string(root.join(p)).unwrap_or_default()).collect();
    let out = sh(&full, root).output();
    let mut changed = vec![];
    for (p, b) in paths.iter().zip(before) {
        let a = fs::read_to_string(root.join(p)).unwrap_or_default();
        if a != b { changed.push((p.clone(), a)); }
    }
    let line = match out {
        Ok(o) if o.status.success() => format!("format: `{cmd}` on {} file(s), {} reformatted ({} ms)\n", paths.len(), changed.len(), t.elapsed().as_millis()),
        Ok(o) => format!("format: `{cmd}` failed, files kept as written: {}\n", String::from_utf8_lossy(&o.stderr).lines().chain(String::from_utf8_lossy(&o.stdout).lines()).take(3).collect::<Vec<_>>().join(" | ")),
        Err(e) => format!("format: could not run `{cmd}`: {e}\n"),
    };
    (line, changed)
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Step {
    cmd: String,
    name: Option<String>,
    /// a failing gate restores every file and skips the remaining steps
    #[serde(default)]
    gate: bool,
    timeout_s: Option<u64>,
    /// run once before the edits too; a failure identical to the baseline is reported as
    /// pre-existing ("KNOWN") instead of a failure of this change
    #[serde(default)]
    baseline: bool,
}

struct StepResult {
    status: &'static str,
    ms: u128,
    output: String,
}

/// Same failure before and after: the same set of non-empty output lines, ignoring timings.
fn same_failure(before: &str, after: &str) -> bool {
    let norm = |s: &str| -> std::collections::BTreeSet<String> {
        let t = Regex::new(r"\(?\d+(\.\d+)?m?s\)?").unwrap();
        s.lines().map(|l| t.replace_all(l.trim(), "").to_string()).filter(|l| !l.is_empty()).collect()
    };
    let a = norm(after);
    // a silent failure proves nothing: only a failure that names its problems can be matched
    !a.is_empty() && a.is_subset(&norm(before))
}

/// The calling process's working directory and environment, when the daemon applies a batch for
/// it: steps and formatters then run exactly as they would have in the caller (`env -u X mt
/// apply` keeps X unset), and relative body-file paths resolve against the caller's directory.
pub struct Client { pub cwd: PathBuf, pub env: Vec<(String, String)> }
static CLIENT: std::sync::Mutex<Option<Client>> = std::sync::Mutex::new(None);

pub fn set_client(c: Option<Client>) {
    *CLIENT.lock().unwrap_or_else(|e| e.into_inner()) = c;
}

/// The session the batch came from: the client's environment in the daemon, else ours.
fn client_session() -> Option<String> {
    if let Some(c) = CLIENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let var = |k: &str| c.env.iter().find(|(a, v)| a == k && !v.is_empty()).map(|(_, v)| v.clone());
        return var("MT_SESSION").or_else(|| var("CLAUDE_CODE_SESSION_ID")).or_else(|| var("MT_SOCKET").map(|s| format!("{:016x}", crate::fnv1a(s.as_bytes())))).map(|s| crate::coord::with_agent(s, var("MT_AGENT")));
    }
    crate::query::session_id()
}

fn sh(cmd: &str, root: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(root);
    if let Some(cl) = CLIENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() { c.env_clear().envs(cl.env.iter().cloned()); }
    c
}

fn caller_dir() -> PathBuf {
    CLIENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|c| c.cwd.clone()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Where a batch runs. One-shot: this process owns the index and refreshes it itself. In the
/// daemon: the shared graph is only read, briefly (claims, rename lookups), never held across the
/// writes or the steps, and nothing is re-indexed here: the watcher sees the written files and
/// the daemon's background reindexer picks them up, so no request waits on this batch.
pub enum Ctx<'a> {
    Local(&'a mut Index),
    Daemon { graph: &'a Index, root: PathBuf },
}

impl Ctx<'_> {
    fn root(&self) -> PathBuf {
        match self { Ctx::Local(i) => i.root.clone(), Ctx::Daemon { root, .. } => root.clone() }
    }
    fn text_files(&self) -> Vec<String> {
        match self { Ctx::Local(i) => i.text_files(), Ctx::Daemon { graph, .. } => graph.text_files() }
            .into_iter().map(|(p, _)| p).collect()
    }
    fn refs(&self, name: &str) -> Vec<(String, String, Vec<usize>)> {
        match self { Ctx::Local(i) => i.refs(name), Ctx::Daemon { graph, .. } => graph.refs(name) }
    }
    /// After this batch touched files (only `paths` when given). One-shot: refresh the index, the
    /// written files only, against their own rows. Daemon: nothing (the watcher handles it).
    fn resync(&mut self, paths: Option<&[String]>) {
        let Ctx::Local(idx) = self else { return };
        match paths.map(|p| (p, idx.known_for(p))) {
            Some((p, Ok(known))) => { let _ = idx.refresh(&known, Some(p), false); }
            _ => { let _ = idx.update(); }
        }
    }
}

/// Batches apply one at a time (each runs its steps in its caller's environment).
pub static APPLY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_step(root: &Path, st: &Step) -> StepResult {
    let t = Instant::now();
    let child = sh(&st.cmd, root)
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return StepResult { status: "ERROR", ms: 0, output: format!("could not start: {e}") },
    };
    // drain pipes on threads so a chatty command cannot block on a full pipe
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let ho = std::thread::spawn(move || { let mut v = vec![]; let _ = std::io::Read::read_to_end(&mut so, &mut v); v });
    let he = std::thread::spawn(move || { let mut v = vec![]; let _ = std::io::Read::read_to_end(&mut se, &mut v); v });
    let limit = std::time::Duration::from_secs(st.timeout_s.unwrap_or(600));
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break if s.success() { "PASS" } else if matches!(s.code(), Some(126 | 127)) { "ENV" } else { "FAIL" },
            Ok(None) if t.elapsed() > limit => { let _ = child.kill(); let _ = child.wait(); break "TIMEOUT" }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => break "ERROR",
        }
    };
    let out = format!("{}{}", String::from_utf8_lossy(&ho.join().unwrap_or_default()), String::from_utf8_lossy(&he.join().unwrap_or_default()));
    let status = if status == "FAIL" && env_problem(&out) { "ENV" } else { status };
    StepResult { status, ms: t.elapsed().as_millis(), output: out }
}

/// A step that could not run because a program is missing on this machine (exit 126/127, or the
/// shell saying so). Anything else is a failure; a step that already failed before the change is
/// what `"baseline": true` is for.
fn env_problem(out: &str) -> bool {
    // the shell's own words for a missing program; nothing tool- or language-specific
    let rx = Regex::new(r"(?m)(command not found|^(sh|bash|zsh|dash): (line \d+: )?\S+: (command )?not found)").unwrap();
    rx.is_match(out)
}

/// The lines worth reading from a failed step: failures, errors and file:line locations, then the tail.
fn excerpt(out: &str) -> Vec<String> {
    let rx = Regex::new(r"(?i)(--- FAIL|^FAIL|^\s*FAIL\b|panic:|\berror\b|Error:|failed|assert|expected|✗|×|\.(go|rs|ts|tsx|js|py|cs|swift|java|kt):\d+|line \d+)").unwrap();
    let lines: Vec<&str> = out.lines().collect();
    let mut keep: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| rx.is_match(l)).map(|(i, _)| i).take(40).collect();
    keep.extend(lines.len().saturating_sub(15)..lines.len());
    keep.sort();
    keep.dedup();
    let mut v = vec![];
    let mut prev: Option<usize> = None;
    for i in keep.into_iter().take(60) {
        if prev.map(|p| i > p + 1).unwrap_or(false) { v.push("  ...".to_string()); }
        v.push(lines[i].chars().take(220).collect());
        prev = Some(i);
    }
    v
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Op {
    span: Option<String>,
    hash: Option<String>,
    replace: Option<String>,
    #[serde(default)]
    delete: bool,
    insert_after: Option<String>,
    file: Option<String>,
    find: Option<String>,
    with: Option<String>,
    #[serde(default)]
    all: bool,
    create: Option<String>,
    text: Option<String>,
    #[serde(default)]
    overwrite: bool,
    rename: Option<String>,
    /// rename's new name; copy/move's destination: a file (appended) or a span / `#n` (after it)
    to: Option<String>,
    /// copy/move these lines (a span or `#n`) to `to`: mt writes the code, the model never retypes it
    copy: Option<String>,
    #[serde(rename = "move")]
    mv: Option<String>,
    /// the same bodies, read from a file (verbatim) instead of inlined in the JSON
    replace_file: Option<String>,
    with_file: Option<String>,
    text_file: Option<String>,
    insert_after_file: Option<String>,
    /// rename scope: "code" (default: everything except docs) or "all"
    scope: Option<String>,
    /// regex replacement ($1-style captures in `with`), applied to `files` and/or every code file
    /// that references the identifier in `refs`
    regex: Option<String>,
    files: Option<Vec<String>>,
    refs: Option<String>,
    /// regex: every indexed text file under this path prefix ("." for the whole repo)...
    #[serde(rename = "in")]
    in_dir: Option<String>,
    /// ...with one of these extensions (no dot), when given
    ext: Option<Vec<String>>,
    /// exact total number of matches expected across all files; anything else aborts the batch
    expect: Option<usize>,
}

enum Edit {
    Span { op: usize, s: usize, e: usize, hash: Option<String>, kind: SpanKind },
    Find { op: usize, find: String, with: String, all: bool, within: Option<(usize, usize, Option<String>)> },
    Create { op: usize, text: String, overwrite: bool },
    Rename { op: usize, from: String, to: String },
    Regex { op: usize, rx: Regex, with: String },
}

enum SpanKind {
    Replace(String),
    Delete,
    InsertAfter(String),
}

struct Planned {
    path: String,
    old: Option<String>,
    new: String,
    new_spans: Vec<(usize, usize, usize)>, // (op, new start line 1-based, new end line)
    ranges: usize,
}

fn parse_span(s: &str) -> Result<(String, usize, usize), String> {
    let (p, r) = s.rsplit_once(":L").ok_or_else(|| format!("bad span {s:?}: want path:L<start>-<end>"))?;
    let (a, b) = r.split_once('-').unwrap_or((r, r));
    let a: usize = a.parse().map_err(|_| format!("bad span start in {s:?}"))?;
    let b: usize = b.parse().map_err(|_| format!("bad span end in {s:?}"))?;
    if a == 0 || b < a {
        return Err(format!("bad span range in {s:?}"));
    }
    Ok((p.to_string(), a - 1, b - 1))
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// A hash vouches for a span if it is the hash of exactly those lines, or of an unchanged
/// definition that contains them. That is how hashes are handed out: per definition.
fn hash_vouches(path: &str, text: &str, s: usize, e: usize, h: &str) -> bool {
    if hash_of(text, s, e).eq_ignore_ascii_case(h) {
        return true;
    }
    // the whole file, unchanged: every range in it is as it was read (a brief or a span of a
    // short file hands out one hash for the file). ponytail: other enclosing line ranges are not
    // tried; they would need every (start, end) pair hashed
    let n = text.lines().count();
    if n > 0 && hash_of(text, 0, n - 1).eq_ignore_ascii_case(h) {
        return true;
    }
    let tier = crate::languages::detect(Path::new(path), text);
    crate::definitions_with(&tier, text).iter().filter(|d| d.start <= s && e <= d.end).any(|d| {
        hash_of(text, d.start, d.end).eq_ignore_ascii_case(h) || hash_of(text, d.head, d.end).eq_ignore_ascii_case(h)
    })
}

fn hash_of(text: &str, s: usize, e: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if s >= lines.len() {
        return "--------".into();
    }
    let e = e.min(lines.len() - 1);
    format!("{:08x}", fnv1a(lines[s..=e].join("\n").as_bytes()) as u32)
}

fn with_newline(t: &str) -> String {
    if t.is_empty() || t.ends_with('\n') { t.to_string() } else { format!("{t}\n") }
}

/// Plan one file: turn its edits into non-overlapping byte ranges on the original text.
fn plan_file(path: &str, abs: &Path, edits: &[Edit]) -> Result<Planned, Vec<String>> {
    let mut errs = vec![];
    let existing = fs::read(abs).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
    if let Some(Edit::Create { op, text, overwrite }) = edits.iter().find(|e| matches!(e, Edit::Create { .. })) {
        if existing.is_some() && !overwrite {
            return Err(vec![format!("op {op}: create {path}: file exists (set \"overwrite\": true)")]);
        }
        if edits.len() > 1 {
            return Err(vec![format!("op {op}: create {path} cannot be combined with other ops on the same file")]);
        }
        let n = text.lines().count();
        return Ok(Planned { path: path.into(), old: existing, new: with_newline(text), new_spans: vec![(*op, 1, n.max(1))], ranges: 1 });
    }
    let Some(old) = existing else { return Err(vec![format!("{path}: file not found")]) };
    let starts = line_starts(&old);
    let nlines = old.lines().count();
    let line_range = |s: usize, e: usize| -> (usize, usize) {
        let a = starts[s.min(starts.len() - 1)];
        let b = if e + 1 < starts.len() { starts[e + 1] } else { old.len() };
        (a, b)
    };
    // (byte start, byte end, replacement, op, span-op line count info)
    let mut ranges: Vec<(usize, usize, String, usize, Option<usize>)> = vec![];
    for ed in edits {
        match ed {
            Edit::Span { op, s, e, hash, kind } => {
                if *e >= nlines {
                    errs.push(format!("op {op}: {path}:L{}-{} is past the end of the file ({nlines} lines)", s + 1, e + 1));
                    continue;
                }
                if let Some(h) = hash {
                    if !hash_vouches(path, &old, *s, *e, h) {
                        let got = hash_of(&old, *s, *e);
                        errs.push(format!("op {op}: {path}:L{}-{} changed since it was read (hash {h} matches neither these lines nor an unchanged definition around them; these lines now hash {got}); re-read it with mt span/context", s + 1, e + 1));
                        continue;
                    }
                }
                let (a, b) = line_range(*s, *e);
                match kind {
                    SpanKind::Replace(t) => ranges.push((a, b, with_newline(t), *op, Some(t.lines().count()))),
                    SpanKind::Delete => ranges.push((a, b, String::new(), *op, None)),
                    SpanKind::InsertAfter(t) => ranges.push((b, b, with_newline(t), *op, Some(t.lines().count()))),
                }
            }
            Edit::Find { op, find, with, all, within } => {
                let (a, b) = match within {
                    Some((s, e, h)) => {
                        if let Some(h) = h {
                            if !hash_vouches(path, &old, *s, *e, h) {
                                let got = hash_of(&old, *s, *e);
                                errs.push(format!("op {op}: {path}:L{}-{} changed since it was read (hash {h} matches neither these lines nor an unchanged definition around them; these lines now hash {got})", s + 1, e + 1));
                                continue;
                            }
                        }
                        line_range(*s, (*e).min(nlines.saturating_sub(1)))
                    }
                    None => (0, old.len()),
                };
                let hay = &old[a..b];
                let hits: Vec<usize> = hay.match_indices(find.as_str()).map(|(i, _)| a + i).collect();
                if hits.is_empty() {
                    errs.push(format!("op {op}: {path}: {find:?} not found{}", if within.is_some() { " in the span" } else { "" }));
                    continue;
                }
                if hits.len() > 1 && !all {
                    let line_of = |b: usize| starts.partition_point(|&x| x <= b).saturating_sub(1);
                    let at: Vec<String> = hits.iter().take(8).map(|&h| {
                        let (s, e) = (line_of(h), line_of(h + find.len().saturating_sub(1)));
                        format!("\"span\": \"{path}:L{}-{}\", \"hash\": \"{}\"", s + 1, e + 1, hash_of(&old, s, e))
                    }).collect();
                    errs.push(format!("op {op}: {path}: {find:?} occurs {} times; add one of these to the op to pick it, or set \"all\": true:\n      {}{}",
                        hits.len(), at.join("\n      "), if hits.len() > 8 { format!("\n      ... {} more", hits.len() - 8) } else { String::new() }));
                    continue;
                }
                for h in hits {
                    ranges.push((h, h + find.len(), with.clone(), *op, None));
                }
            }
            Edit::Rename { op, from, to } => {
                let rx = Regex::new(&format!(r"\b{}\b", regex::escape(from))).unwrap();
                for m in rx.find_iter(&old) {
                    ranges.push((m.start(), m.end(), to.clone(), *op, None));
                }
            }
            Edit::Regex { op, rx, with } => {
                for c in rx.captures_iter(&old) {
                    let m = c.get(0).unwrap();
                    let mut rep = String::new();
                    c.expand(with, &mut rep);
                    ranges.push((m.start(), m.end(), rep, *op, None));
                }
            }
            Edit::Create { .. } => {}
        }
    }
    ranges.sort_by_key(|r| (r.0, r.1));
    // identical duplicates (a rename and a find hitting the same token the same way) collapse
    ranges.dedup_by(|b, a| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    for w in ranges.windows(2) {
        if w[1].0 < w[0].1 {
            errs.push(format!("{path}: op {} and op {} overlap (bytes {}-{} and {}-{})", w[0].3, w[1].3, w[0].0, w[0].1, w[1].0, w[1].1));
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let mut new = String::with_capacity(old.len() + 256);
    let mut pos = 0;
    let mut new_spans = vec![];
    for (a, b, rep, op, span_lines) in &ranges {
        new.push_str(&old[pos..*a]);
        if let Some(n) = span_lines {
            let start_line = new.matches('\n').count() + 1;
            new_spans.push((*op, start_line, start_line + n.saturating_sub(1).max(0)));
        }
        new.push_str(rep);
        pos = *b;
    }
    new.push_str(&old[pos..]);
    Ok(Planned { path: path.into(), old: Some(old), new, new_spans, ranges: ranges.len() })
}

fn resolve(root: &Path, p: &str) -> (String, PathBuf) {
    let pb = Path::new(p);
    // relative: the repo root when it exists there (or nowhere), else the caller's directory
    let abs = if pb.is_absolute() { pb.to_path_buf() } else if root.join(pb).exists() || !caller_dir().join(pb).exists() { root.join(pb) } else { caller_dir().join(pb) };
    let rel = abs.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| abs.to_string_lossy().into_owned());
    (rel, abs)
}

/// `f` over `items` on every core, results in the items' order (contiguous chunks).
fn par_in_order<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let chunk = items.len().div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = items.chunks(chunk).map(|c| s.spawn(move || c.iter().map(f).collect::<Vec<_>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

fn par<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut chunks: Vec<Vec<T>> = (0..threads).map(|_| vec![]).collect();
    for (i, it) in items.into_iter().enumerate() {
        chunks[i % threads].push(it);
    }
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = chunks.into_iter().map(|c| s.spawn(move || c.into_iter().map(f).collect::<Vec<_>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

pub fn run(idx: &mut Index, input: &str, force_dry: bool) -> (bool, String) {
    run_with(Ctx::Local(idx), input, force_dry)
}

/// `run`, with the daemon's graph when the daemon applies the batch: the graph is already fresh
/// (the request passed the cookie barrier), so no tree walk first, and afterwards exactly the
/// written files are re-indexed, into the index and the graph, once.
pub fn run_with(mut ctx: Ctx, input: &str, force_dry: bool) -> (bool, String) {
    let t0 = Instant::now();
    let batch: Batch = if input.trim_start().starts_with("@@") {
        match parse_bundle(input).and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
            Ok(b) => b,
            Err(e) => return (false, format!("mt apply: bundle: {e}; nothing written")),
        }
    } else {
        match serde_json::from_str(input) {
            Ok(b) => b,
            Err(e) if input.contains("\n@@ ") => return (false, format!("mt apply: input is neither JSON ({e}) nor a bundle: a bundle must start with its first @@ line; nothing written")),
            Err(e) => return (false, format!("mt apply: invalid JSON: {e}")),
        }
    };
    let dry = batch.dry_run || force_dry;
    // The index decides which files a rename touches: it must reflect the tree as it is now.
    let ustats = match &ctx {
        Ctx::Daemon { graph, .. } => UpdateStats { files: graph.file_count(), ..Default::default() },
        Ctx::Local(idx) => match idx.update() {
            Ok(s) => s,
            Err(e) => return (false, format!("mt apply: index refresh failed, nothing written: {e}")),
        },
    };
    // ---- preconditions: claims first, then the before-commands; either failing writes nothing
    let mut pre = String::new();
    if let Some(claims) = &batch.expect {
        let claims_in = serde_json::json!({"queries": claims, "session": false, "fresh": true, "budget": 1500}).to_string();
        let out = match &ctx {
            Ctx::Daemon { graph, .. } => crate::query::run(*graph, &claims_in, None, &ustats, 0, t0),
            Ctx::Local(idx) => crate::query::run(&**idx, &claims_in, None, &ustats, 0, t0),
        };
        let out = match out {
            Ok(o) => o,
            Err(e) => return (false, format!("mt apply: @@ expect is not a valid query batch ({e}); nothing written")),
        };
        let head = out.lines().next().unwrap_or("");
        let count = |mark: &str| head.split(';').next().unwrap_or("").split_whitespace().collect::<Vec<_>>().windows(2).find(|w| w[1] == mark).and_then(|w| w[0].parse::<usize>().ok()).unwrap_or(0);
        let (bad, unknown) = (count("✗"), head.split(" answered ").nth(1).and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse::<usize>().ok()).unwrap_or(0));
        if bad > 0 || unknown > 0 {
            return (false, format!("mt apply: {bad} claim(s) failed and {unknown} could not be checked; nothing written. The claims report:\n{out}"));
        }
        pre.push_str(&format!("expect: {} claim(s) hold\n", count("✓")));
    }
    for (k, st) in batch.before.iter().enumerate() {
        let r = run_step(&ctx.root(), st);
        let label = st.name.clone().unwrap_or_else(|| st.cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" "));
        if r.status != "PASS" {
            let ex: Vec<String> = excerpt(&r.output).into_iter().map(|l| format!("       | {l}")).collect();
            return (false, format!("mt apply: before-step {} ({label}) {}: nothing written\n{}  {} {}  ({} ms)\n{}\n", k + 1, r.status, pre, r.status, st.cmd, r.ms, ex.join("\n")));
        }
        pre.push_str(&format!("before {}: PASS  {label}  ({} ms)\n", k + 1, r.ms));
    }
    // the before-commands may have changed the tree (a lockfile, generated code): index it again
    if !batch.before.is_empty() { ctx.resync(None); }
    let root = ctx.root();
    let mut per_file: BTreeMap<String, (PathBuf, Vec<Edit>)> = BTreeMap::new();
    let mut errs: Vec<String> = vec![];
    let mut notes: Vec<String> = vec![];
    let sid = client_session();
    for (i, mut op) in batch.ops.into_iter().enumerate() {
        let n = i + 1;
        let mut bad = false;
        for (src, dst, what) in [(op.replace_file.take(), &mut op.replace, "replace"), (op.with_file.take(), &mut op.with, "with"),
                                 (op.text_file.take(), &mut op.text, "text"), (op.insert_after_file.take(), &mut op.insert_after, "insert_after")] {
            let Some(f) = src else { continue };
            if dst.is_some() { errs.push(format!("op {n}: give {what} or {what}_file, not both")); bad = true; continue }
            match fs::read_to_string(resolve(&root, &f).1) {
                Ok(t) => *dst = Some(t),
                Err(e) => { errs.push(format!("op {n}: {what}_file {f}: {e}")); bad = true; }
            }
        }
        if bad { continue }
        // codes: `#3` is item [3] of the session's last query/context reply, hash included
        let mut to_hash: Option<String> = None;
        for (field, is_to) in [(&mut op.span, false), (&mut op.copy, false), (&mut op.mv, false), (&mut op.to, true)] {
            let Some(k) = field.as_deref().and_then(crate::vctx::code) else { continue };
            if is_to && op.rename.is_some() { continue }
            match crate::vctx::pointer(sid.as_deref(), k) {
                Ok((sp, h)) => {
                    *field = Some(sp);
                    if is_to { to_hash = Some(h) } else if op.hash.is_none() { op.hash = Some(h) }
                }
                Err(e) => { errs.push(format!("op {n}: {e}")); bad = true; }
            }
        }
        if bad { continue }
        if let Some(src) = op.copy.clone().or_else(|| op.mv.clone()) {
            let verb = if op.mv.is_some() { "move" } else { "copy" };
            let Some(dest) = op.to.clone() else { errs.push(format!("op {n}: {verb} needs \"to\" (a file, a span or #n)")); continue };
            let (sp, s, e) = match parse_span(&src) { Ok(x) => x, Err(e) => { errs.push(format!("op {n}: {verb}: {e}")); continue } };
            let (srel, sabs) = resolve(&root, &sp);
            let Ok(text) = fs::read_to_string(&sabs) else { errs.push(format!("op {n}: {verb}: {srel}: file not found")); continue };
            let lines: Vec<&str> = text.lines().collect();
            if e >= lines.len() { errs.push(format!("op {n}: {verb}: {srel}:L{}-{} is past the end of the file ({} lines)", s + 1, e + 1, lines.len())); continue }
            if let Some(h) = &op.hash {
                if !hash_vouches(&srel, &text, s, e, h) { errs.push(format!("op {n}: {verb}: {srel}:L{}-{} changed since it was read (hash {h}); re-read it", s + 1, e + 1)); continue }
            }
            let body = lines[s..=e].join("\n");
            match parse_span(&dest) {
                Ok((dp, ds, de)) => {
                    let (drel, dabs) = resolve(&root, &dp);
                    per_file.entry(drel).or_insert((dabs, vec![])).1.push(Edit::Span { op: n, s: ds, e: de, hash: to_hash.clone(), kind: SpanKind::InsertAfter(format!("\n{body}")) });
                }
                Err(_) => {
                    let (drel, dabs) = resolve(&root, &dest);
                    // a file this batch creates (its header, or an earlier move into it): append
                    if let Some(Edit::Create { text, .. }) = per_file.get_mut(&drel).and_then(|f| f.1.iter_mut().find(|e| matches!(e, Edit::Create { .. }))) {
                        let sep = if text.is_empty() || text.ends_with("\n\n") { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
                        text.push_str(sep);
                        text.push_str(&body);
                        text.push('\n');
                        if op.mv.is_some() {
                            per_file.entry(srel).or_insert((sabs, vec![])).1.push(Edit::Span { op: n, s, e, hash: op.hash.clone(), kind: SpanKind::Delete });
                        }
                        continue;
                    }
                    let edit = match fs::read_to_string(&dabs).map(|t| t.lines().count()) {
                        Ok(last) if last > 0 => Edit::Span { op: n, s: last - 1, e: last - 1, hash: None, kind: SpanKind::InsertAfter(format!("\n{body}")) },
                        Ok(_) => Edit::Create { op: n, text: body.clone(), overwrite: true },
                        Err(_) => Edit::Create { op: n, text: body.clone(), overwrite: false },
                    };
                    per_file.entry(drel).or_insert((dabs, vec![])).1.push(edit);
                }
            }
            if op.mv.is_some() {
                per_file.entry(srel).or_insert((sabs, vec![])).1.push(Edit::Span { op: n, s, e, hash: op.hash.clone(), kind: SpanKind::Delete });
            }
        } else if let Some(p) = &op.create {
            let (rel, abs) = resolve(&root, p);
            per_file.entry(rel).or_insert((abs, vec![])).1.push(Edit::Create { op: n, text: op.text.clone().unwrap_or_default(), overwrite: op.overwrite });
        } else if let Some(from) = &op.rename {
            let Some(to) = op.to.clone() else { errs.push(format!("op {n}: rename needs \"to\"")); continue };
            let all_scope = op.scope.as_deref() == Some("all");
            let mut files: Vec<String> = vec![];
            for (p, role, _) in ctx.refs(from) {
                if role == "doc" && !all_scope {
                    notes.push(format!("op {n}: {p} mentions {from} but is a doc; not renamed (add \"scope\": \"all\" to include docs)"));
                } else {
                    files.push(p);
                }
            }
            files.sort();
            files.dedup();
            if files.is_empty() {
                errs.push(format!("op {n}: rename {from}: no indexed file mentions it"));
            }
            for f in files {
                let abs = root.join(&f);
                per_file.entry(f).or_insert((abs, vec![])).1.push(Edit::Rename { op: n, from: from.clone(), to: to.clone() });
            }
        } else if let Some(pat) = &op.regex {
            let rx = match Regex::new(pat) { Ok(r) => r, Err(e) => { errs.push(format!("op {n}: bad regex: {e}")); continue } };
            let Some(with) = op.with.clone() else { errs.push(format!("op {n}: regex needs \"with\"")); continue };
            let mut targets: Vec<String> = op.files.clone().unwrap_or_default();
            if let Some(name) = &op.refs {
                for (p, role, _) in ctx.refs(name) {
                    if role != "doc" && !targets.contains(&p) { targets.push(p); }
                }
            }
            if let Some(prefix) = &op.in_dir {
                // the files a script would loop over: under the prefix, with the extensions, and
                // actually matching (from memory in the daemon), on every core
                let pre = prefix.trim_start_matches("./").trim_end_matches('/');
                let exts: Vec<String> = op.ext.clone().unwrap_or_default().iter().map(|e| e.trim_start_matches('.').to_string()).collect();
                let cand: Vec<String> = ctx.text_files().into_iter()
                    .filter(|p| pre.is_empty() || pre == "." || p.starts_with(&format!("{pre}/")) || p == pre)
                    .filter(|p| exts.is_empty() || exts.iter().any(|e| p.ends_with(&format!(".{e}"))))
                    .filter(|p| !targets.contains(p)).collect();
                let hit: Vec<Option<String>> = par_in_order(&cand, |p| crate::texts::get(&root, p).filter(|t| rx.is_match(t)).map(|_| p.clone()));
                targets.extend(hit.into_iter().flatten());
                if targets.is_empty() { notes.push(format!("op {n}: regex matched no file under {prefix}")); continue }
            }
            if targets.is_empty() { errs.push(format!("op {n}: regex needs \"files\", \"refs\" and/or \"in\"")); continue }
            if let Some(want) = op.expect {
                let got: usize = targets.iter().map(|t| fs::read_to_string(resolve(&root, t).1).map(|s| rx.find_iter(&s).count()).unwrap_or(0)).sum();
                if got != want {
                    errs.push(format!("op {n}: regex matches {got} times across {} files, expected {want}; nothing written", targets.len()));
                    continue;
                }
            }
            for t in targets {
                let (rel, abs) = resolve(&root, &t);
                per_file.entry(rel).or_insert((abs, vec![])).1.push(Edit::Regex { op: n, rx: rx.clone(), with: with.clone() });
            }
        } else if let Some(find) = &op.find {
            let within = match &op.span {
                Some(sp) => match parse_span(sp) { Ok((p, s, e)) => Some((p, s, e)), Err(e) => { errs.push(format!("op {n}: {e}")); continue } },
                None => None,
            };
            let Some(path) = op.file.clone().or_else(|| within.as_ref().map(|w| w.0.clone())) else { errs.push(format!("op {n}: find needs \"file\" or \"span\"")); continue };
            let (rel, abs) = resolve(&root, &path);
            per_file.entry(rel).or_insert((abs, vec![])).1.push(Edit::Find {
                op: n, find: find.clone(), with: op.with.clone().unwrap_or_default(), all: op.all,
                within: within.map(|(_, s, e)| (s, e, op.hash.clone().filter(|h| !h.trim().is_empty()))),
            });
        } else if let Some(sp) = &op.span {
            let (p, s, e) = match parse_span(sp) { Ok(x) => x, Err(e) => { errs.push(format!("op {n}: {e}")); continue } };
            let kind = if let Some(t) = &op.replace { SpanKind::Replace(t.clone()) } else if op.delete { SpanKind::Delete } else if let Some(t) = &op.insert_after { SpanKind::InsertAfter(t.clone()) } else {
                errs.push(format!("op {n}: span op needs replace, delete or insert_after"));
                continue;
            };
            let (rel, abs) = resolve(&root, &p);
            per_file.entry(rel).or_insert((abs, vec![])).1.push(Edit::Span { op: n, s, e, hash: op.hash.clone(), kind });
        } else {
            errs.push(format!("op {n}: unrecognised op (need span, find, regex, create or rename)"));
        }
    }
    if !errs.is_empty() {
        return (false, format!("mt apply: nothing written, {} invalid op(s):\n  {}", errs.len(), errs.join("\n  ")));
    }

    // ---- validate + plan every file in parallel
    let jobs: Vec<(String, PathBuf, Vec<Edit>)> = per_file.into_iter().map(|(k, (a, e))| (k, a, e)).collect();
    let planned: Vec<Result<Planned, Vec<String>>> = par(jobs, |(rel, abs, edits)| plan_file(&rel, &abs, &edits));
    let validate_ms = t0.elapsed().as_millis();
    let mut ok = vec![];
    for p in planned {
        match p {
            Ok(p) => ok.push(p),
            Err(e) => errs.extend(e),
        }
    }
    if !errs.is_empty() {
        return (false, format!("mt apply: nothing written, {} problem(s):\n  {}", errs.len(), errs.join("\n  ")));
    }
    ok.retain(|p| p.old.as_deref() != Some(p.new.as_str()));
    // another agent's live claim on code this batch edits: refused before anything is written
    if let Some(e) = crate::coord::lease_conflicts(&ctx.root(), client_session().as_deref(), &ok.iter().map(|p| (p.path.as_str(), p.old.as_deref(), p.new.as_str())).collect::<Vec<_>>()) {
        return (false, e);
    }
    let total_ranges: usize = ok.iter().map(|p| p.ranges).sum();

    let mut diff = String::new();
    let mut diff_lines = 0;
    for p in &ok {
        let old = p.old.clone().unwrap_or_default();
        let d = similar::TextDiff::from_lines(&old, &p.new);
        let u = d.unified_diff().context_radius(1).header(&format!("a/{}", p.path), &format!("b/{}", p.path)).to_string();
        for l in u.lines() {
            if diff_lines < 400 {
                diff.push_str(l);
                diff.push('\n');
            }
            diff_lines += 1;
        }
    }
    if diff_lines > 400 {
        diff.push_str(&format!("... {} more diff lines\n", diff_lines - 400));
    }
    let mut spans = String::new();
    for p in &ok {
        for (op, s, e) in &p.new_spans {
            spans.push_str(&format!("  op {op}: {}:L{}-{} h={}\n", p.path, s, e, hash_of(&p.new, s - 1, e - 1)));
        }
    }
    let notes = if notes.is_empty() { String::new() } else { format!("notes:\n  {}\n", notes.join("\n  ")) };
    if dry {
        return (true, format!("{notes}dry run: {} ops would change {} files ({} edits), validated in {} ms\n{}{}", total_ranges, ok.len(), total_ranges, validate_ms, if spans.is_empty() { String::new() } else { format!("new spans:\n{spans}") }, diff));
    }

    // ---- steps: everything the change needs verified, in one go
    let mut steps: Vec<Step> = vec![];
    if let Some(cmd) = &batch.check {
        steps.push(Step { cmd: cmd.clone(), name: Some("check".into()), gate: true, timeout_s: None, baseline: false });
    }
    steps.extend(batch.run.iter().cloned());
    // {changed} and {changed_dirs}: scope checks to this batch's files, not the whole repo's history
    let changed: Vec<String> = ok.iter().map(|p| p.path.clone()).collect();
    let mut dirs: Vec<String> = changed.iter().map(|p| p.rsplit_once('/').map(|(d, _)| format!("./{d}")).unwrap_or_else(|| ".".into())).collect();
    dirs.sort();
    dirs.dedup();
    let quote = |v: &[String]| v.iter().map(|x| format!("'{}'", x.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
    for st in steps.iter_mut() {
        st.cmd = st.cmd.replace("{changed_dirs}", &quote(&dirs)).replace("{changed}", &quote(&changed));
    }
    // baselines run against the tree as it is before this batch
    let tb = Instant::now();
    let base_jobs: Vec<(usize, Step)> = steps.iter().enumerate().filter(|(_, st)| st.baseline).map(|(k, st)| (k, st.clone())).collect();
    let root_b = root.clone();
    let baselines: std::collections::HashMap<usize, StepResult> = std::thread::scope(|sc| {
        let hs: Vec<_> = base_jobs.into_iter().map(|(k, st)| { let r = &root_b; sc.spawn(move || (k, run_step(r, &st))) }).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let baseline_ms = tb.elapsed().as_millis();
    // ---- write every file in parallel (temp file + rename). No backups on disk: the originals are
    // in memory (`old`), which is all a failing gate needs; history is git's job, not mt's.
    let t1 = Instant::now();
    let txn = root.join(".mt/txn").join(format!("{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
    if !steps.is_empty() { let _ = fs::create_dir_all(&txn); } // step logs only
    let originals: Vec<(PathBuf, Option<String>)> = ok.iter().map(|p| (root.join(&p.path), p.old.clone())).collect();
    let writes: Vec<(PathBuf, String)> = ok.iter().map(|p| (root.join(&p.path), p.new.clone())).collect();
    let results: Vec<Result<(), String>> = par(writes, |(abs, text)| {
        if let Some(d) = abs.parent() {
            fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = abs.with_extension(format!("mt-tmp-{}", std::process::id()));
        fs::write(&tmp, text).map_err(|e| format!("{}: {e}", abs.display()))?;
        fs::rename(&tmp, &abs).map_err(|e| format!("{}: {e}", abs.display()))
    });
    let write_errs: Vec<String> = results.into_iter().filter_map(|r| r.err()).collect();
    if !write_errs.is_empty() {
        let restored = restore(originals);
        return (false, format!("mt apply: write failed, restored {restored} file(s): {}", write_errs.join("; ")));
    }
    let write_ms = t1.elapsed().as_millis();
    // the tree's other uncommitted changes, for the reply: started now, on its own thread, so it
    // overlaps the formatter and the steps instead of following them
    let git_status = { let root = root.clone(); std::thread::spawn(move || std::process::Command::new("git").args(["status", "--porcelain", "--untracked-files=all"]).current_dir(&root).output()) };
    let fmt_report = if let Some(cmd) = batch.format.as_deref().filter(|c| !c.trim().is_empty()) {
        let (rep, changed) = format_written(&root, cmd, &ok.iter().map(|p| p.path.clone()).collect::<Vec<_>>());
        for (path, text) in changed { if let Some(p) = ok.iter_mut().find(|p| p.path == path) { p.new = text; } }
        rep
    } else { String::new() };
    let t2 = Instant::now();
    let written: Vec<String> = ok.iter().map(|p| p.path.clone()).collect();
    ctx.resync(Some(&written));
    let reindex_ms = t2.elapsed().as_millis();

    let t3 = Instant::now();
    let mut results: Vec<Option<StepResult>> = (0..steps.len()).map(|_| None).collect();
    let mut rolled_back: Option<usize> = None;
    let mut i = 0;
    while i < steps.len() {
        if steps[i].gate || !batch.parallel {
            let mut r = run_step(&root, &steps[i]);
            if let Some(b) = baselines.get(&i) { if r.status != "PASS" && b.status == r.status && same_failure(&b.output, &r.output) { r.status = "KNOWN"; } }
            let failed = r.status != "PASS" && r.status != "KNOWN" && !(r.status == "ENV" && !steps[i].gate);
            results[i] = Some(r);
            if failed && steps[i].gate {
                rolled_back = Some(restore(originals.clone()));
                ctx.resync(Some(&written));
                break;
            }
            i += 1;
        } else {
            // a run of consecutive non-gate steps, concurrently
            let mut j = i;
            while j < steps.len() && !steps[j].gate { j += 1; }
            let group: Vec<(usize, Step)> = (i..j).map(|k| (k, steps[k].clone())).collect();
            let root2 = root.clone();
            let done: Vec<(usize, StepResult)> = std::thread::scope(|sc| {
                let hs: Vec<_> = group.into_iter().map(|(k, st)| { let r = &root2; sc.spawn(move || (k, run_step(r, &st))) }).collect();
                hs.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for (k, mut r) in done {
                if let Some(b) = baselines.get(&k) { if r.status != "PASS" && b.status == r.status && same_failure(&b.output, &r.output) { r.status = "KNOWN"; } }
                results[k] = Some(r);
            }
            i = j;
        }
    }
    let steps_ms = t3.elapsed().as_millis();
    let mut report = String::new();
    let mut all_ok = rolled_back.is_none();
    if all_ok {
        crate::vctx::note_written(sid.as_deref(), &written);
        crate::coord::note_change(&root, sid.as_deref(), &ok.iter().map(|p| (p.path.clone(), p.old.clone(), p.new.clone())).collect::<Vec<_>>());
    }
    let mut n_env = 0;
    if !steps.is_empty() {
        report.push_str(&format!("steps ({}{}):\n", if batch.parallel { "non-gate steps in parallel" } else { "in order" }, if rolled_back.is_some() { ", stopped at a failed gate" } else { "" }));
        for (k, st) in steps.iter().enumerate() {
            let label = st.name.clone().unwrap_or_else(|| st.cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" "));
            match &results[k] {
                None => report.push_str(&format!("  {:>2} SKIP  {:<10} {}\n", k + 1, label, st.cmd)),
                Some(r) => {
                    let log = txn.join(format!("step-{}.log", k + 1));
                    let _ = fs::write(&log, &r.output);
                    let rel = log.strip_prefix(&root).map(|p| p.display().to_string()).unwrap_or_default();
                    report.push_str(&format!("  {:>2} {:<5} {:<10} {}  ({} ms{}){}\n", k + 1, r.status, label, st.cmd, r.ms, if st.gate { ", gate" } else { "" },
                        if r.status == "PASS" { String::new() } else if r.status == "KNOWN" { "  already failing the same way before this change; not counted".to_string() }
                        else if r.status == "ENV" { format!("  could not run here (a tool or dependency is missing); not a failure of this change. full log: {rel}") } else { format!("  full log: {rel}") }));
                    if r.status == "ENV" {
                        n_env += 1;
                        for l in excerpt(&r.output).into_iter().take(6) { report.push_str(&format!("       | {l}\n")); }
                    } else if r.status != "PASS" && r.status != "KNOWN" {
                        all_ok = false;
                        for l in excerpt(&r.output) { report.push_str(&format!("       | {l}\n")); }
                    }
                }
            }
        }
    }
    let max_out: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let failed_out: Vec<&str> = results.iter().flatten().filter(|r| r.status != "PASS" && r.status != "KNOWN" && r.status != "ENV").map(|r| r.output.as_str()).collect();
    let fail_ctx = if failed_out.is_empty() { String::new() } else {
        // the code the failing command actually ran against: the attempted text, even after a rollback
        let attempted: std::collections::HashMap<String, String> = ok.iter().map(|p| (p.path.clone(), p.new.clone())).collect();
        let c = failure_context(&root, &failed_out, max_out.saturating_sub(report.len()) * 3 / 5, &attempted);
        if c.is_empty() { c } else { format!("failure context (the code the failing step ran against; ! marks the line named in the output):\n{c}") }
    };
    let total = t0.elapsed().as_millis();
    if let Some(n) = rolled_back {
        // the files are back to the original: show the attempted regions so the retry needs no re-read
        let regions = changed_regions(&ok, max_out.saturating_sub(report.len() + fail_ctx.len() + 400), true);
        return (false, format!(
            "gate FAILED after writing {} edits to {} files. Rolled back {n} file(s); nothing changed on disk. ({} ms total, steps {} ms)\n{}{}attempted change (NOT on disk now; resend a corrected batch against the original spans):\n{}",
            total_ranges, ok.len(), total, steps_ms, report, fail_ctx, regions
        ));
    }
    let t_tail = Instant::now();
    let regions = changed_regions(&ok, max_out.saturating_sub(report.len() + fail_ctx.len() + notes.len() + 600), batch.echo);
    let t_regions = t_tail.elapsed();
    // what changed, in one place: this batch's files, and dependency manifests changed by anyone
    let stat: String = par_in_order(&ok, |p| {
        let d = similar::TextDiff::from_lines(p.old.as_deref().unwrap_or(""), &p.new);
        let (mut a, mut r) = (0, 0);
        for op in d.ops() { match op.tag() { similar::DiffTag::Insert => a += op.new_range().len(), similar::DiffTag::Delete => r += op.old_range().len(), similar::DiffTag::Replace => { a += op.new_range().len(); r += op.old_range().len(); } _ => {} } }
        format!("  {} +{a} -{r}{}\n", p.path, if p.old.is_none() { " (new)" } else { "" })
    }).concat();
    // everything else uncommitted in the tree (a lockfile from a package manager, a generated file):
    // from git, so it holds for any language. Deletions are only counted.
    let in_batch: std::collections::HashSet<&str> = ok.iter().map(|p| p.path.as_str()).collect();
    let deps = git_status.join().ok().and_then(|r| r.ok()).map(|o| {
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let mut other: Vec<String> = vec![];
        let mut deleted = 0;
        for l in text.lines() {
            if l.len() < 4 { continue; }
            let (st, path) = (l[..2].trim(), l[3..].trim_matches('"'));
            if in_batch.contains(path) || path.starts_with(".mt/") { continue; }
            if st.contains('D') { deleted += 1; continue; }
            other.push(path.to_string());
        }
        let mut out = String::new();
        let numstat = std::process::Command::new("git").args(["diff", "--numstat", "--"]).args(&other).current_dir(&root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        for path in other.iter().take(20) {
            let n = numstat.lines().find(|l| l.ends_with(&format!("\t{path}"))).map(|l| { let f: Vec<&str> = l.split('\t').collect(); format!(" +{} -{}", f[0], f[1]) }).unwrap_or_else(|| " (untracked)".into());
            out.push_str(&format!("  {path}{n}\n"));
        }
        if other.len() > 20 { out.push_str(&format!("  ... {} more\n", other.len() - 20)); }
        if deleted > 0 { out.push_str(&format!("  ({deleted} deleted file(s) not listed)\n")); }
        out
    }).unwrap_or_default();
    if std::env::var_os("MT_TIMING").is_some() { eprintln!("apply tail: regions {:.1} ms, stat + git {:.1} ms", t_regions.as_secs_f64() * 1e3, (t_tail.elapsed() - t_regions).as_secs_f64() * 1e3); }
    let summary = format!("files:\n{stat}{}", if deps.is_empty() { String::new() } else { format!("other uncommitted changes in the tree, not made by this batch (e.g. a package manager's lockfile):\n{deps}") });
    let verdict = if steps.is_empty() { String::new() } else if all_ok && n_env > 0 { format!("result: every step that could run passed; {n_env} step(s) could not run here (ENV): fix the environment and rerun them\n") } else if all_ok { "result: every step passed, on the files exactly as written. They already ran: don't rerun them.\n".to_string() } else { "result: some steps failed; the edits are KEPT (only gate steps roll back). Fix forward in the next batch.\n".to_string() };
    let _ = (&spans, &diff);
    (all_ok, format!(
        "applied {} edits to {} files in {} ms (validate {} ms, baselines {baseline_ms} ms, write {} ms, reindex {} ms, steps {} ms)\n{pre}{fmt_report}{summary}{}{}{}{}changed regions ({}; spans and hashes are ready for the next batch):\n{}",
        total_ranges, ok.len(), total, validate_ms, write_ms, reindex_ms, steps_ms,
        report, verdict, fail_ctx, notes, if batch.echo { "code as it now is on disk, + marks changed lines" } else { "span and hash only: it is the code you just wrote; \"echo\": true shows it" }, regions
    ))
}

fn span_hash_of(lines: &[&str], s: usize, e: usize) -> String {
    format!("{:08x}", fnv1a(lines[s..=e].join("\n").as_bytes()) as u32)
}

/// Smallest definition containing [a, b] that is at most `max` lines long.
fn enclosing_def(path: &str, text: &str, a: usize, b: usize, max: usize) -> Option<crate::Def> {
    let tier = crate::languages::detect(Path::new(path), text);
    crate::definitions_with(&tier, text).into_iter().filter(|d| d.start <= a && b <= d.end && d.end - d.start < max).min_by_key(|d| d.end - d.start)
}

/// Every changed area of every file, as the code now reads, so the next batch needs no re-read.
/// Changed lines carry `+`; each block carries the span and hash to use in a follow-up op.
fn changed_regions(planned: &[Planned], budget: usize, echo: bool) -> String {
    if !echo && planned.len() > 32 {
        // headers only: no output budget to share, so each chunk of files diffs on its own core
        let parts: Vec<String> = par_in_order(&planned.chunks(planned.len().div_ceil(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)).max(1)).collect::<Vec<_>>(), |c| changed_regions(c, usize::MAX, false));
        return parts.concat();
    }
    let mut out = String::new();
    let mut omitted = 0;
    for p in planned {
        let old = p.old.clone().unwrap_or_default();
        let lines: Vec<&str> = p.new.lines().collect();
        if lines.is_empty() { continue; }
        let diff = similar::TextDiff::from_lines(&old, &p.new);
        let mut changed: Vec<usize> = vec![];
        let mut removed = 0usize;
        for op in diff.ops() {
            match op.tag() {
                similar::DiffTag::Insert | similar::DiffTag::Replace => changed.extend(op.new_range()),
                similar::DiffTag::Delete => removed += op.old_range().len(),
                _ => {}
            }
            if op.tag() == similar::DiffTag::Replace { removed += op.old_range().len().saturating_sub(op.new_range().len()); }
        }
        changed.retain(|&l| l < lines.len());
        if changed.is_empty() { continue; }
        // group changed lines, then widen each group to its enclosing definition (or 3 lines of context)
        let mut groups: Vec<(usize, usize)> = vec![];
        for &l in &changed {
            match groups.last_mut() { Some(g) if l <= g.1 + 4 => g.1 = l, _ => groups.push((l, l)) }
        }
        let mut blocks: Vec<(usize, usize, String)> = vec![];
        for (a, b) in groups {
            let (s, e, label) = match enclosing_def(&p.path, &p.new, a, b, 90) {
                Some(d) => (d.start, d.end, format!("{} {}", d.kind, d.name)),
                None => (a.saturating_sub(3), (b + 3).min(lines.len() - 1), "lines".to_string()),
            };
            match blocks.last_mut() { Some(bl) if s <= bl.1 + 1 => { bl.1 = bl.1.max(e); } _ => blocks.push((s, e, label)) }
        }
        let set: std::collections::HashSet<usize> = changed.into_iter().collect();
        if !echo {
            // the code is what the batch just wrote: spans and hashes are enough for the next batch
            for (s, e, label) in &blocks {
                let n = (*s..=*e).filter(|k| set.contains(k)).count();
                out.push_str(&format!("[{}:L{}-{} {} h={}] {}\n", p.path, s + 1, e + 1, label, span_hash_of(&lines, *s, *e),
                    if p.old.is_none() { format!("new file, {} lines", lines.len()) } else { format!("{n} line(s) changed") }));
                if p.old.is_none() { break; }
            }
            if removed > 0 { out.push_str(&format!("  ({removed} line(s) removed in {})\n", p.path)); }
            continue;
        }
        for (s, e, label) in blocks {
            let block_len: usize = lines[s..=e].iter().map(|l| l.len() + 8).sum();
            if out.len() + block_len > budget {
                out.push_str(&format!("[{}:L{}-{} {} h={}] (not shown: output limit; mt span to read)\n", p.path, s + 1, e + 1, label, span_hash_of(&lines, s, e)));
                omitted += 1;
                continue;
            }
            out.push_str(&format!("[{}:L{}-{} {} h={}]{}\n", p.path, s + 1, e + 1, label, span_hash_of(&lines, s, e), if p.old.is_none() { " (new file)" } else { "" }));
            let w = (e + 1).to_string().len();
            for (k, l) in lines[s..=e].iter().enumerate() {
                out.push_str(&format!("{:>w$}{} {}\n", s + k + 1, if set.contains(&(s + k)) && p.old.is_some() { "+" } else { " " }, l, w = w));
            }
        }
        if removed > 0 { out.push_str(&format!("  ({removed} line(s) removed in {})\n", p.path)); }
    }
    if omitted > 0 { out.push_str(&format!("({omitted} region(s) listed without bodies to stay under the output limit)\n")); }
    out
}

/// For failed steps: the failing tests' bodies and the code at every file:line the output names,
/// each with span + hash, so the fix can go straight into the next batch.
fn failure_context(root: &Path, outputs: &[&str], budget: usize, attempted: &std::collections::HashMap<String, String>) -> String {
    let loc = Regex::new(r"([A-Za-z0-9_./\-]+\.(?:go|rs|ts|tsx|js|jsx|py|cs|swift|java|kt|rb|php|c|cc|cpp|h)):(\d+)").unwrap();
    let test_rx = Regex::new(r"(?m)(?:--- FAIL: |FAIL: |✗ |× |failed: )(\w+)").unwrap();
    let mut seen: Vec<(String, usize, usize)> = vec![];
    let mut out = String::new();
    let mut emit = |path: &str, line0: Option<usize>, def: &crate::Def, why: &str, text: &str, out: &mut String| {
        if seen.iter().any(|(p, s, e)| p == path && *s == def.start && *e == def.end) { return; }
        seen.push((path.to_string(), def.start, def.end));
        let lines: Vec<&str> = text.lines().collect();
        let e = def.end.min(lines.len().saturating_sub(1));
        let body: usize = lines[def.start..=e].iter().map(|l| l.len() + 8).sum();
        if out.len() + body > budget {
            out.push_str(&format!("[{path}:L{}-{} {} {} h={}] {why} (not shown: output limit)\n", def.start + 1, e + 1, def.kind, def.name, span_hash_of(&lines, def.start, e)));
            return;
        }
        out.push_str(&format!("[{path}:L{}-{} {} {} h={}] {why}\n", def.start + 1, e + 1, def.kind, def.name, span_hash_of(&lines, def.start, e)));
        let w = (e + 1).to_string().len();
        for (k, l) in lines[def.start..=e].iter().enumerate() {
            out.push_str(&format!("{:>w$}{} {}\n", def.start + k + 1, if Some(def.start + k) == line0 { "!" } else { " " }, l, w = w));
        }
    };
    // 1. code at each location the output names
    let mut locs: Vec<(String, usize)> = vec![];
    for o in outputs {
        for c in loc.captures_iter(o) {
            let p = c[1].trim_start_matches("./").to_string();
            let l: usize = c[2].parse().unwrap_or(0);
            if l > 0 && !locs.contains(&(p.clone(), l)) { locs.push((p, l)); }
        }
    }
    for (p, l) in locs.iter().take(10) {
        let abs = if root.join(p).exists() || attempted.contains_key(p) { root.join(p) } else if let Some(k) = attempted.keys().find(|k| k.ends_with(p.as_str())) { root.join(k) } else {
            // go test prints paths relative to the package dir: find the file by suffix
            match crate::index::find_by_suffix(root, p) { Some(x) => x, None => continue }
        };
        let rel = abs.strip_prefix(root).map(|x| x.to_string_lossy().into_owned()).unwrap_or(p.clone());
        let text = match attempted.get(&rel) { Some(t) => t.clone(), None => match fs::read_to_string(&abs) { Ok(t) => t, Err(_) => continue } };
        let l0 = l - 1;
        let def = enclosing_def(&rel, &text, l0, l0, 200).unwrap_or(crate::Def { kind: "lines".into(), name: format!("around L{l}"), start: l0.saturating_sub(6), end: l0 + 6, path: vec![], head: l0 });
        emit(&rel, Some(l0), &def, &format!("named in the failure output at L{l}"), &text, &mut out);
    }
    // 2. failing tests by name, when the output did not already point inside them
    for o in outputs {
        for c in test_rx.captures_iter(o).take(6) {
            let name = &c[1];
            let in_attempt = attempted.iter().find_map(|(rel, text)| {
                let tier = crate::languages::detect(Path::new(rel), text);
                crate::definitions_with(&tier, text).into_iter().find(|d| d.name == name || d.name.ends_with(&format!(".{name}"))).map(|d| (rel.clone(), text.clone(), d))
            });
            if let Some((rel, text, def)) = in_attempt.or_else(|| crate::index::find_def_in_tests(root, name)) {
                emit(&rel, None, &def, "failing test", &text, &mut out);
            }
        }
    }
    out
}

/// Put the originals back, on every core: rewrite what existed, delete what the batch created.
fn restore(originals: Vec<(PathBuf, Option<String>)>) -> usize {
    let n = originals.len();
    par(originals, |(abs, old)| {
        match old {
            Some(text) => { let _ = fs::write(&abs, text); }
            None => { let _ = fs::remove_file(&abs); }
        }
    });
    n
}

