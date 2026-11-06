//! `mt visualize`: a live view of the repo graph and of what mt is doing, in the browser.
//!
//! A small HTTP server on 127.0.0.1 (std only, one thread per connection):
//!   /         the page (src/viz.html, compiled in)
//!   /graph    files as nodes, cross-file references as weighted edges (from .mt/index.sqlite)
//!   /file     one file's definitions
//!   /status   index and daemon state
//!   /events   Server-Sent Events: the last events, then every new one as it happens, from
//!             .mt/events.jsonl (the daemon) and .mt/hooks.log (this repo's Claude Code hook decisions)
//! Only Host: localhost / 127.0.0.1 is answered, so another site can't read it via DNS rebinding.

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PAGE: &str = include_str!("viz.html");
const MAX_NODES: usize = 1500;
const MAX_EDGES: usize = 6000;

/// The scenes the page knows (src/viz.html, `SCENES`); `--mode` must be one of them.
pub const MODES: &[&str] = &["graph", "spectrum", "scope", "radial", "starfield", "tunnel", "matrix", "waterfall", "fireworks", "plasma", "leds"];

pub fn run(root: PathBuf, port: u16, open: bool, mode: Option<&str>) -> Result<(), String> {
    if let Some(m) = mode.filter(|m| !MODES.contains(m)) {
        return Err(format!("no scene {m:?}; scenes: {}", MODES.join(", ")));
    }
    let l = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    let url = format!("http://127.0.0.1:{port}/{}", mode.map(|m| format!("#{m}")).unwrap_or_default());
    eprintln!("mt visualize {}: {url}  (Ctrl-C stops)", root.display());
    if !root.join(".mt/index.sqlite").is_file() {
        eprintln!("no index yet: building it (mt index)");
        crate::index::Index::open(&root).and_then(|i| i.update().map(|_| ())).map_err(|e| e.to_string())?;
    }
    if open {
        let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }
    for c in l.incoming().flatten() {
        let root = root.clone();
        std::thread::spawn(move || { let _ = handle(c, &root, port); });
    }
    Ok(())
}

fn handle(mut c: TcpStream, root: &Path, port: u16) -> std::io::Result<()> {
    c.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut r = BufReader::new(c.try_clone()?);
    let mut first = String::new();
    r.read_line(&mut first)?;
    let mut host = String::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" { break; }
        if let Some(v) = h.strip_prefix("Host:").or_else(|| h.strip_prefix("host:")) { host = v.trim().to_string(); }
    }
    let ok_host = [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&host);
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if !ok_host { return send(&mut c, "403 Forbidden", "text/plain", b"bad host"); }
    if method != "GET" { return send(&mut c, "405 Method Not Allowed", "text/plain", b"GET only"); }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => send(&mut c, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        "/graph" => json_reply(&mut c, graph(root)),
        "/status" => json_reply(&mut c, status(root)),
        "/file" => {
            let p = query.split('&').find_map(|kv| kv.strip_prefix("path=")).map(pct_decode).unwrap_or_default();
            json_reply(&mut c, file_defs(root, &p))
        }
        "/events" => events(c, root),
        _ => send(&mut c, "404 Not Found", "text/plain", b"not found"),
    }
}

fn send(c: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(c, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len())?;
    c.write_all(body)
}

fn json_reply(c: &mut TcpStream, v: Result<Value, String>) -> std::io::Result<()> {
    match v {
        Ok(v) => send(c, "200 OK", "application/json", v.to_string().as_bytes()),
        Err(e) => send(c, "500 Internal Server Error", "application/json", json!({"error": e}).to_string().as_bytes()),
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) { Some(v) => { out.push(v); i += 3; } None => { out.push(b'%'); i += 1; } }
            }
            b'+' => { out.push(b' '); i += 1; }
            x => { out.push(x); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn db(root: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(root.join(".mt/index.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("open index: {e} (run `mt index`)"))
}

/// Files as nodes (the most connected first, up to MAX_NODES) and file-to-file references as
/// edges. A reference counts when its name has at most 3 definitions in the repo, so `new` or
/// `get` don't tie everything to everything.
fn graph(root: &Path) -> Result<Value, String> {
    let d = db(root)?;
    let e = |e: rusqlite::Error| e.to_string();
    let mut st = d.prepare(
        "WITH names AS (SELECT last FROM defs GROUP BY last HAVING COUNT(*) <= 3)
         SELECT r.file, df.file, COUNT(*) FROM refs r
         JOIN defs df ON df.last = r.name AND df.file != r.file
         WHERE r.name IN (SELECT last FROM names)
         GROUP BY r.file, df.file ORDER BY 3 DESC LIMIT ?1").map_err(e)?;
    let edges: Vec<(i64, i64, i64)> = st.query_map([MAX_EDGES as i64 * 2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e)?.flatten().collect();
    let mut degree: std::collections::HashMap<i64, i64> = Default::default();
    for (a, b, w) in &edges { *degree.entry(*a).or_default() += w; *degree.entry(*b).or_default() += w; }
    let mut st = d.prepare("SELECT f.id, f.path, COALESCE(f.lang,''), COALESCE(f.role,''), COALESCE(f.lines,0), (SELECT COUNT(*) FROM defs WHERE file = f.id) FROM files f").map_err(e)?;
    let mut files: Vec<(i64, String, String, String, i64, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).map_err(e)?.flatten().collect();
    let total_files = files.len();
    files.retain(|f| f.5 > 0 || degree.contains_key(&f.0));
    files.sort_by_key(|f| -(degree.get(&f.0).copied().unwrap_or(0) * 4 + f.5));
    files.truncate(MAX_NODES);
    let keep: std::collections::HashSet<i64> = files.iter().map(|f| f.0).collect();
    let nodes: Vec<Value> = files.iter().map(|(id, p, lang, role, lines, defs)| json!({"id": id, "path": p, "lang": lang, "role": role, "lines": lines, "defs": defs, "deg": degree.get(id).copied().unwrap_or(0)})).collect();
    let edges: Vec<Value> = edges.iter().filter(|(a, b, _)| keep.contains(a) && keep.contains(b)).take(MAX_EDGES).map(|(a, b, w)| json!([a, b, w])).collect();
    let defs: i64 = d.query_row("SELECT COUNT(*) FROM defs", [], |r| r.get(0)).unwrap_or(0);
    Ok(json!({"root": root.display().to_string(), "files": total_files, "defs": defs, "nodes": nodes, "edges": edges}))
}

fn file_defs(root: &Path, path: &str) -> Result<Value, String> {
    let d = db(root)?;
    let mut st = d.prepare("SELECT d.kind, d.name, d.start, d.end FROM defs d JOIN files f ON f.id = d.file WHERE f.path = ?1 ORDER BY d.start LIMIT 400").map_err(|e| e.to_string())?;
    let defs: Vec<Value> = st.query_map([path], |r| Ok(json!({"kind": r.get::<_, Option<String>>(0)?, "name": r.get::<_, Option<String>>(1)?, "s": r.get::<_, i64>(2)?, "e": r.get::<_, i64>(3)?}))).map_err(|e| e.to_string())?.flatten().collect();
    Ok(json!({"path": path, "defs": defs}))
}

fn status(root: &Path) -> Result<Value, String> {
    #[cfg(unix)]
    let (sock, up) = {
        let s = crate::serve::socket_path(root);
        let up = std::os::unix::net::UnixStream::connect(&s).is_ok();
        (s.display().to_string(), up)
    };
    #[cfg(not(unix))]
    let (sock, up) = (String::new(), false);
    let idx = root.join(".mt/index.sqlite");
    Ok(json!({"root": root.display().to_string(), "daemon": up, "socket": sock, "index_bytes": idx.metadata().map(|m| m.len()).unwrap_or(0), "version": env!("CARGO_PKG_VERSION")}))
}

/// A file being tailed: where we are in it.
struct Tail { path: PathBuf, pos: u64, hook: bool }

impl Tail {
    /// Start `backlog` bytes before the end, at a line start.
    fn new(path: PathBuf, backlog: u64, hook: bool) -> Self {
        let len = path.metadata().map(|m| m.len()).unwrap_or(0);
        Tail { path, pos: len.saturating_sub(backlog), hook }
    }
    fn lines(&mut self) -> Vec<Value> {
        let len = self.path.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos { self.pos = 0; } // rolled over
        if len == self.pos { return vec![]; }
        let Ok(mut f) = std::fs::File::open(&self.path) else { return vec![] };
        if f.seek(SeekFrom::Start(self.pos)).is_err() { return vec![]; }
        let mut buf = Vec::new();
        let _ = f.take(len - self.pos).read_to_end(&mut buf);
        // only whole lines; a line still being written waits for the next poll
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return vec![] };
        self.pos += end as u64 + 1;
        String::from_utf8_lossy(&buf[..end + 1]).lines()
            .filter_map(|l| if self.hook { hook_event(l) } else { serde_json::from_str(l).ok() }).collect()
    }
}

/// `1793969755.737 session-start <sid> <message>` from the hook log, as an event.
fn hook_event(l: &str) -> Option<Value> {
    // "<epoch> <event> <rest>"; only session-start / session-end put the session id first in <rest>
    let mut it = l.splitn(3, ' ');
    let t: f64 = it.next()?.parse().ok().filter(|t: &f64| *t > 1e9)?; // a wrapped message line is not an entry
    let ev = it.next()?.to_string();
    let rest = it.next().unwrap_or("");
    let (sid, msg) = match ev.as_str() {
        "session-start" | "session-end" => rest.split_once(' ').unwrap_or((rest, "")),
        _ => ("", rest),
    };
    let sid: String = sid.chars().take(8).collect();
    let msg: String = msg.chars().take(400).collect();
    Some(json!({"kind": "hook", "t": t, "event": ev, "session": sid, "msg": msg, "touched": crate::events::touched(&msg, 8)}))
}

fn events(mut c: TcpStream, root: &Path) -> std::io::Result<()> {
    write!(c, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n")?;
    let mut tails = [Tail::new(crate::events::path(root), 64 << 10, false), Tail::new(hook_log(root), 16 << 10, true)];
    // the backlog starts mid-file: drop its first, possibly partial, line
    for t in tails.iter_mut() { if t.pos > 0 { skip_partial(t); } }
    let mut idle = 0u32;
    loop {
        let mut batch: Vec<Value> = tails.iter_mut().flat_map(|t| t.lines()).collect();
        batch.sort_by(|a, b| a["t"].as_f64().unwrap_or(0.0).total_cmp(&b["t"].as_f64().unwrap_or(0.0)));
        for ev in &batch { write!(c, "data: {ev}\n\n")?; }
        if batch.is_empty() {
            idle += 1;
            if idle % 100 == 0 { c.write_all(b": keep-alive\n\n")?; } // ~15 s; also detects a closed tab
        } else { idle = 0; }
        c.flush()?;
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// This repo's hook decisions (written by `mt hook`, same line format as before).
fn hook_log(root: &Path) -> PathBuf {
    #[cfg(unix)]
    { crate::hook::repo_log(root) }
    #[cfg(not(unix))]
    { root.join(".mt/hooks.log") }
}

fn skip_partial(t: &mut Tail) {
    let Ok(mut f) = std::fs::File::open(&t.path) else { return };
    if f.seek(SeekFrom::Start(t.pos)).is_err() { return; }
    let mut r = BufReader::new(f);
    let mut s = Vec::new();
    if let Ok(n) = r.read_until(b'\n', &mut s) { t.pos += n as u64; }
}

#[cfg(test)]
mod tests {
    #[test]
    fn modes_match_the_page() {
        for m in super::MODES {
            assert!(super::PAGE.contains(&format!("\n  {m}: {{ name: \"{m}\"")) || *m == "graph" && super::PAGE.contains("graph: { name: \"graph\""), "scene {m} missing from viz.html");
        }
    }

    #[test]
    fn hook_lines_and_percent_decoding() {
        let e = super::hook_event("1793969755.737 pretool rewrite sed-range | sed -n 3,9p src/a.rs => mt span src/a.rs:L3-9").unwrap();
        assert_eq!(e["event"], "pretool");
        assert_eq!(e["session"], "");
        assert!(e["msg"].as_str().unwrap().starts_with("rewrite sed-range"));
        assert_eq!(e["touched"][0]["path"], "src/a.rs");
        let s = super::hook_event("1793969755.737 session-start 2a856dde-3fac /repo session server pid 1").unwrap();
        assert_eq!(s["session"], "2a856dde");
        assert_eq!(s["msg"], "/repo session server pid 1");
        assert!(super::hook_event("42 wrapped continuation").is_none());
        assert_eq!(super::pct_decode("src%2Fa%20b.rs"), "src/a b.rs");
        assert_eq!(super::pct_decode("100%"), "100%");
    }
}
