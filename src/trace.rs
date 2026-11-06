//! `MT_DEBUG=1` (or `mt index --debug`): everything indexing and the server's start do, with
//! timings, written to `.mt/debug/<command>-<pid>.jsonl`, one JSON event per line, to send back
//! for analysis. Off: every call here is one relaxed load and returns.
//!
//! Events: `start` (machine, mt build, repo), `span` (a timed step: walk, fresh build, each batch's
//! parse and write with its parts, each lookup table, the server's start), `file` (each file's parse
//! time, language, size, definitions), `summary` (the slowest files, totals per language, rows per
//! table, index file sizes), `note`.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
struct Sink { out: std::io::BufWriter<std::fs::File>, path: PathBuf, t0: Instant, files: Vec<(String, String, u64, f64, usize)> }
static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sink>> { SINK.get_or_init(|| Mutex::new(None)) }

pub fn on() -> bool { ON.load(Ordering::Relaxed) }

/// Start tracing into `<root>/.mt/debug/<cmd>-<pid>.jsonl` when `MT_DEBUG` is set (or `force`).
/// Returns the file's path when tracing.
pub fn start(root: &Path, cmd: &str, force: bool) -> Option<PathBuf> {
    if on() { return sink().lock().ok()?.as_ref().map(|s| s.path.clone()); }
    if !force && std::env::var_os("MT_DEBUG").is_none_or(|v| v.is_empty() || v == "0") { return None; }
    let dir = root.join(".mt/debug");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{cmd}-{}.jsonl", std::process::id()));
    let f = std::fs::File::create(&path).ok()?;
    *sink().lock().ok()? = Some(Sink { out: std::io::BufWriter::new(f), path: path.clone(), t0: Instant::now(), files: vec![] });
    ON.store(true, Ordering::Relaxed);
    let load = std::process::Command::new("sysctl").args(["-n", "vm.loadavg"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .or_else(|| std::fs::read_to_string("/proc/loadavg").ok()).unwrap_or_default();
    event("start", json!({
        "cmd": cmd, "root": root.display().to_string(), "pid": std::process::id(),
        "mt": crate::serve::exe_id(), "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "load": load, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
    }));
    Some(path)
}

/// One event. `t` is seconds since tracing started; `thread` names who did it.
pub fn event(ev: &str, mut v: Value) {
    if !on() { return; }
    let Ok(mut g) = sink().lock() else { return };
    let Some(s) = g.as_mut() else { return };
    v["t"] = json!((s.t0.elapsed().as_secs_f64() * 1e4).round() / 1e4);
    v["ev"] = json!(ev);
    v["thread"] = json!(std::thread::current().name().map(String::from).unwrap_or_else(|| format!("{:?}", std::thread::current().id())));
    let _ = writeln!(s.out, "{v}");
}

/// A timed step: `let _s = trace::span("write.batch", json!({...}));` logs it, with its
/// duration, when dropped.
pub struct Span { name: &'static str, t: Instant, extra: Value }
pub fn span(name: &'static str, extra: Value) -> Option<Span> { on().then(|| Span { name, t: Instant::now(), extra }) }
impl Drop for Span {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.extra);
        if !v.is_object() { v = json!({}); }
        v["name"] = json!(self.name);
        v["ms"] = json!((self.t.elapsed().as_secs_f64() * 1e5).round() / 100.0);
        event("span", v);
    }
}

/// One parsed file (kept for the summary's slowest files and per-language totals, and logged).
pub fn file(rel: &str, lang: &str, size: u64, ms: f64, defs: usize) {
    if !on() { return; }
    if let Ok(mut g) = sink().lock() {
        if let Some(s) = g.as_mut() { s.files.push((rel.to_string(), lang.to_string(), size, ms, defs)); }
    }
}

/// The end: slowest files, per-language totals, `extra` (rows per table, sizes), and flush.
/// Returns the trace file's path.
pub fn finish(extra: Value) -> Option<PathBuf> {
    if !on() { return None; }
    let files = sink().lock().ok()?.as_mut().map(|s| std::mem::take(&mut s.files)).unwrap_or_default();
    let mut slow = files.clone();
    slow.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let mut by_lang: std::collections::BTreeMap<String, (usize, u64, f64)> = Default::default();
    for (_, l, z, ms, _) in &files { let e = by_lang.entry(l.clone()).or_default(); e.0 += 1; e.1 += z; e.2 += ms; }
    let mut v = json!({
        "files_parsed": files.len(),
        "parse_ms_sum": (files.iter().map(|f| f.3).sum::<f64>() * 100.0).round() / 100.0,
        "slowest": slow.iter().take(40).map(|(p, l, z, ms, d)| json!({"path": p, "lang": l, "bytes": z, "ms": (ms * 100.0).round() / 100.0, "defs": d})).collect::<Vec<_>>(),
        "by_lang": by_lang.iter().map(|(l, (n, z, ms))| json!({"lang": l, "files": n, "bytes": z, "parse_ms": (ms * 100.0).round() / 100.0})).collect::<Vec<_>>(),
    });
    if let (Some(a), Some(b)) = (v.as_object_mut(), extra.as_object()) { for (k, x) in b { a.insert(k.clone(), x.clone()); } }
    event("summary", v);
    let mut g = sink().lock().ok()?;
    let s = g.as_mut()?;
    let _ = s.out.flush();
    Some(s.path.clone())
}
