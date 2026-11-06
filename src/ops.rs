//! Lifecycle commands: `mt build`, `mt rebuild`, `mt logs`.
//!
//! - build: the index up to date (incremental) and the daemon running and warm. One command for
//!   "this repo is ready for mt".
//! - rebuild: stop the daemon, drop the index, index from scratch, start the daemon again, so the
//!   graph it holds in memory is the new one (`mt index --rebuild` alone leaves a running daemon
//!   serving the old graph). Kept outputs (`.mt/out`) and settings are left alone.
//! - logs: everything mt records, in one place, newest last: the daemon's events
//!   (`.mt/events.jsonl`), the daemon logs (`.mt/serve.log`, per-session `/tmp/mt-serve-*.log`), the
//!   Claude Code hook decisions (`.mt/hooks.log`) and `mt apply` step logs (`.mt/txn/*`).

use crate::{index, serve};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn hook_log(root: &Path) -> PathBuf {
    crate::hook::repo_log(root)
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "12s ago", "4m ago", "3h ago": time zones don't matter for a log read now.
fn ago(t: f64) -> String {
    let s = (now() - t).max(0.0) as u64;
    match s { 0..=89 => format!("{s}s ago"), 90..=5399 => format!("{}m ago", s / 60), _ if s < 172_800 => format!("{}h ago", s / 3600), _ => format!("{}d ago", s / 86_400) }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The daemon serving `root`: `Some(status line)` when one answers.
fn daemon_status(root: &Path) -> Option<String> {
    let r = serve::request(root, &serve::Req { cmd: "status".into(), exe: serve::exe_id(), ..Default::default() })?;
    r.ok.then(|| r.out.trim().to_string())
}

/// Start the daemon if none answers, and wait (up to 20 s) until it does.
fn ensure_daemon(root: &Path) -> (bool, String) {
    if let Some(s) = daemon_status(root) { return (true, format!("daemon: already running: {s}")); }
    serve::spawn(root);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(20) {
        if let Some(s) = daemon_status(root) { return (true, format!("daemon: started in {} ms: {s}", t.elapsed().as_millis())); }
        std::thread::sleep(Duration::from_millis(50));
    }
    (false, "daemon: did not answer within 20 s (see `mt logs --source serve`)".into())
}

fn index_now(root: &Path) -> Result<String, String> {
    let _ = std::fs::create_dir_all(root.join(".mt"));
    let mut idx = index::Index::open(root).map_err(|e| format!("cannot open the index: {e}"))?;
    let t = Instant::now();
    let st = idx.update().map_err(|e| format!("indexing failed: {e}"))?;
    Ok(format!("index: {} files, {} re-parsed, {} removed in {} ms (walk {} ms, parse {} ms, write {} ms)\n{}",
        st.files, st.changed, st.removed, t.elapsed().as_millis(), st.walk_ms, st.parse_ms, st.write_ms, idx.summary().trim_end()))
}

/// `mt build`: index up to date, daemon up.
pub fn build(root: &Path) -> (bool, String) {
    let mut out = format!("mt build {}\n", root.display());
    match index_now(root) {
        Ok(s) => out.push_str(&format!("{s}\n")),
        Err(e) => return (false, format!("{out}{e}\n")),
    }
    let (ok, d) = ensure_daemon(root);
    out.push_str(&format!("{d}\n"));
    (ok, out)
}

/// `mt rebuild`: daemon stopped, index dropped and rebuilt, daemon started on the new graph.
pub fn rebuild(root: &Path) -> (bool, String) {
    let mut out = format!("mt rebuild {}\n", root.display());
    let stopped = serve::request(root, &serve::Req { cmd: "stop".into(), exe: serve::exe_id(), ..Default::default() }).is_some();
    // wait for the old daemon to let go of its socket before a new one binds it
    let t = Instant::now();
    while stopped && daemon_status(root).is_some() && t.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(50)); }
    out.push_str(if stopped { "daemon: stopped\n" } else { "daemon: none was running\n" });
    let mut dropped = 0;
    for f in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        if std::fs::remove_file(root.join(".mt").join(f)).is_ok() { dropped += 1; }
    }
    out.push_str(&format!("index: dropped ({dropped} file(s)); kept outputs (.mt/out) and settings stay\n"));
    match index_now(root) {
        Ok(s) => out.push_str(&format!("{s}\n")),
        Err(e) => return (false, format!("{out}{e}\n")),
    }
    let (ok, d) = ensure_daemon(root);
    out.push_str(&format!("{d}\n"));
    (ok, out)
}

/// The last `n` lines of a file (and the byte length read up to).
fn tail(path: &Path, n: usize) -> (Vec<String>, u64) {
    let Ok(mut f) = std::fs::File::open(path) else { return (vec![], 0) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // read only the end: logs grow, `mt logs` must stay instant
    let start = len.saturating_sub(256 * 1024);
    let _ = f.seek(SeekFrom::Start(start));
    let mut s = String::new();
    let _ = f.take(256 * 1024).read_to_string(&mut s);
    let mut lines: Vec<String> = s.lines().map(String::from).collect();
    if start > 0 && !lines.is_empty() { lines.remove(0); } // a partial first line
    let k = lines.len().saturating_sub(n);
    (lines.split_off(k), len)
}

fn event_line(v: &Value) -> String {
    let t = v.get("t").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
    let body = match kind {
        "request" => format!("{} {} {} ms, ~{} tok{}  {}  → {}",
            v.get("cmd").and_then(|x| x.as_str()).unwrap_or("?"),
            if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) { "ok" } else { "FAILED" },
            v.get("ms").and_then(|x| x.as_f64()).unwrap_or(0.0),
            v.get("tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            v.get("session").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| format!(", session {s}")).unwrap_or_default(),
            clip(v.get("subject").and_then(|x| x.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").as_str(), 70),
            clip(v.get("header").and_then(|x| x.as_str()).unwrap_or(""), 90)),
        _ => {
            let mut o = v.clone();
            if let Some(m) = o.as_object_mut() { m.remove("t"); m.remove("kind"); }
            clip(&o.to_string(), 160)
        }
    };
    format!("{:>8}  {kind:<8} {body}", ago(t))
}

/// Log files of session daemons that serve this repo (their start line names the root).
fn session_serve_logs(root: &Path) -> Vec<PathBuf> {
    let want = root.display().to_string();
    let mut v: Vec<(SystemTime, PathBuf)> = std::fs::read_dir("/tmp").into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("mt-serve-") && n.ends_with(".log")))
        .filter(|p| tail(p, 400).0.iter().any(|l| l.contains(&want)))
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .collect();
    v.sort();
    v.into_iter().rev().take(3).map(|x| x.1).collect()
}

/// The newest `mt apply` runs: (dir, when).
fn apply_runs(root: &Path, k: usize) -> Vec<(PathBuf, f64)> {
    let mut v: Vec<(PathBuf, f64)> = std::fs::read_dir(root.join(".mt/txn")).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.path(), e.file_name().to_str()?.parse::<f64>().ok()? / 1000.0)))
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(k);
    v
}

pub struct LogsOpts { pub n: usize, pub source: Option<String>, pub session: Option<String> }

/// `mt logs`: one view of everything mt records.
pub fn logs(root: &Path, o: &LogsOpts) -> String {
    let want = |s: &str| o.source.as_deref().is_none_or(|x| x == s || x == "all");
    let by_session = |l: &str| o.session.as_deref().is_none_or(|s| l.contains(s));
    let mut out = String::new();
    if want("events") {
        let p = crate::events::path(root);
        let (lines, _) = tail(&p, o.n * 4);
        let ev: Vec<String> = lines.iter().filter(|l| by_session(l)).filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|v| event_line(&v)).collect();
        let ev = &ev[ev.len().saturating_sub(o.n)..];
        out.push_str(&format!("## events ({}): what the daemon did\n", p.display()));
        if ev.is_empty() { out.push_str("  (none yet)\n"); } else { for l in ev { out.push_str(&format!("{l}\n")); } }
    }
    if want("serve") {
        let mut files = vec![root.join(".mt/serve.log")];
        files.extend(session_serve_logs(root));
        for f in files.iter().filter(|f| f.exists()) {
            let age = f.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| ago(d.as_secs_f64())).unwrap_or_default();
            let (lines, _) = tail(f, o.n);
            out.push_str(&format!("## daemon log {} (last written {age})\n", f.display()));
            for l in lines.iter().filter(|l| by_session(l)) { out.push_str(&format!("  {}\n", clip(l, 300))); }
        }
    }
    if want("hooks") {
        let hl_path = hook_log(root);
        let (lines, _) = tail(&hl_path, o.n * 20);
        let hl: Vec<String> = lines.iter().filter(|l| by_session(l)).map(|l| {
            let (t, rest) = l.split_once(' ').unwrap_or(("0", l));
            format!("{:>8}  {}", ago(t.parse().unwrap_or(0.0)), clip(rest, 220))
        }).collect();
        let hl = &hl[hl.len().saturating_sub(o.n)..];
        out.push_str(&format!("## hook decisions ({})\n", hl_path.display()));
        if hl.is_empty() { out.push_str("  (none)\n"); } else { for l in hl { out.push_str(&format!("{l}\n")); } }
    }
    if want("apply") {
        let runs = apply_runs(root, 3);
        out.push_str(&format!("## mt apply runs ({}/.mt/txn)\n", root.display()));
        if runs.is_empty() { out.push_str("  (none)\n"); }
        for (dir, t) in runs {
            out.push_str(&format!("{:>8}  {}\n", ago(t), dir.display()));
            let mut steps: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
            steps.sort();
            for s in steps {
                let (last, _) = tail(&s, 3);
                out.push_str(&format!("    {}: {}\n", s.file_name().and_then(|n| n.to_str()).unwrap_or(""), clip(&last.join(" | "), 200)));
            }
        }
    }
    out
}

/// `mt logs -f`: print what is appended to the events and hook logs, as it happens (Ctrl-C ends).
pub fn follow(root: &Path, o: &LogsOpts) -> ! {
    let ev = crate::events::path(root);
    let hk = hook_log(root);
    let mut at: Vec<(PathBuf, u64)> = [ev.clone(), hk.clone()].into_iter().map(|p| { let l = p.metadata().map(|m| m.len()).unwrap_or(0); (p, l) }).collect();
    println!("following {} and {} (Ctrl-C to stop)", ev.display(), hk.display());
    loop {
        for (p, pos) in at.iter_mut() {
            let len = p.metadata().map(|m| m.len()).unwrap_or(0);
            if len < *pos { *pos = 0; } // rolled over
            if len > *pos {
                if let Ok(mut f) = std::fs::File::open(&*p) {
                    let _ = f.seek(SeekFrom::Start(*pos));
                    let mut s = String::new();
                    let _ = f.take(len - *pos).read_to_string(&mut s);
                    for l in s.lines().filter(|l| o.session.as_deref().is_none_or(|x| l.contains(x))) {
                        if *p == ev { if let Ok(v) = serde_json::from_str::<Value>(l) { println!("{}", event_line(&v)); } }
                        else { let (t, rest) = l.split_once(' ').unwrap_or(("0", l)); println!("{:>8}  hook     {}", ago(t.parse().unwrap_or(0.0)), clip(rest, 220)); }
                    }
                }
                *pos = len;
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}
