//! What the daemon keeps in memory between requests, so the next call is near instant:
//!
//! - expansions: when a reply hands out pointer numbers (`[3] path:L42-55 … h=…`), the lines of
//!   every numbered item are kept, per session, so `mt expand 3 7 12` answers from memory;
//! - evidence: each file's identifier set (what `resolve` uses to tell which of several
//!   same-named definitions a call reaches), per file version, so it is built once, not per query.
//!
//! An entry is used only while its file's (mtime, size) is unchanged; a changed file is read
//! again and checked by hash as usual, so the cache can make an answer faster, never different.
//! Only the daemon enables it: a one-shot CLI process would fill it and exit.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

static ON: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

type Stamp = (u128, u64);

fn stamp(root: &Path, rel: &str) -> Option<Stamp> {
    let m = std::fs::metadata(root.join(rel)).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some((t, m.len()))
}

#[derive(Clone)]
pub struct Span {
    pub path: String,
    pub s: usize,
    pub e: usize,
    pub kind: String,
    pub name: String,
    pub hash: String,
    /// the span's own lines, `s..=e`
    pub lines: Arc<Vec<String>>,
    stamp: Stamp,
}

/// session -> pointer number -> span
fn spans() -> &'static Mutex<HashMap<String, HashMap<String, Span>>> {
    static S: OnceLock<Mutex<HashMap<String, HashMap<String, Span>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Keep the numbered items of a reply (rows as `pointer_row` writes them) for `mt expand`.
/// The previous reply's numbers of this session are replaced, as in the `.ptrs` table.
pub fn put_reply(sid: &str, root: &Path, rows: &[String]) {
    if !enabled() || rows.is_empty() { return; }
    let mut by_file: HashMap<String, Vec<String>> = HashMap::new();
    let mut m: HashMap<String, Span> = HashMap::new();
    for r in rows {
        let f: Vec<&str> = r.split('\t').collect();
        if f.len() != 7 { continue; }
        let (Ok(s1), Ok(e1)) = (f[2].parse::<usize>(), f[3].parse::<usize>()) else { continue };
        let Some(st) = stamp(root, f[1]) else { continue };
        let lines = by_file.entry(f[1].to_string()).or_insert_with(|| {
            std::fs::read(root.join(f[1])).map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect()).unwrap_or_default()
        });
        if lines.is_empty() || s1 == 0 { continue; }
        let (s, e) = (s1 - 1, (e1 - 1).min(lines.len() - 1));
        if s > e { continue; }
        m.insert(f[0].to_string(), Span { path: f[1].into(), s, e, kind: f[4].into(), name: f[5].into(), hash: f[6].into(), lines: Arc::new(lines[s..=e].to_vec()), stamp: st });
    }
    let mut g = spans().lock().unwrap_or_else(|e| e.into_inner());
    g.insert(sid.to_string(), m);
    // a long-lived daemon serves many sessions: keep the most recent few hundred
    if g.len() > 256 { if let Some(k) = g.keys().next().cloned() { g.remove(&k); } }
}

/// Item `label` of the session's last numbered reply, if its file is unchanged since.
pub fn get(sid: &str, label: &str, root: &Path) -> Option<Span> {
    if !enabled() { return None; }
    let sp = spans().lock().unwrap_or_else(|e| e.into_inner()).get(sid)?.get(label)?.clone();
    (stamp(root, &sp.path)? == sp.stamp).then_some(sp)
}

/// path -> (stamp, lowercased identifiers)
fn words_map() -> &'static Mutex<HashMap<String, (Stamp, Arc<HashSet<String>>)>> {
    static W: OnceLock<Mutex<HashMap<String, (Stamp, Arc<HashSet<String>>)>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// The identifiers a file contains (lowercased), built from `lines` once per file version.
pub fn words(root: &Path, rel: &str, lines: &[String]) -> Arc<HashSet<String>> {
    let build = || Arc::new(lines.iter().flat_map(|l| crate::context::ident_rx().find_iter(l).map(|m| m.as_str().to_lowercase()).collect::<Vec<_>>()).collect::<HashSet<String>>());
    if !enabled() { return build(); }
    let Some(st) = stamp(root, rel) else { return build() };
    if let Some((s, w)) = words_map().lock().unwrap_or_else(|e| e.into_inner()).get(rel) {
        if *s == st { return w.clone(); }
    }
    let w = build();
    words_map().lock().unwrap_or_else(|e| e.into_inner()).insert(rel.to_string(), (st, w.clone()));
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_served_until_the_file_changes() {
        enable();
        let dir = std::env::temp_dir().join(format!("mt-precache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.go"), "package a\n\nfunc One() {\n\treturn\n}\n").unwrap();
        put_reply("s1", &dir, &["3\ta.go\t3\t5\tfunction\tOne\tabcd1234".to_string()]);
        let sp = get("s1", "3", &dir).expect("cached");
        assert_eq!((sp.s, sp.e, sp.lines.len(), sp.lines[0].as_str()), (2, 4, 3, "func One() {"));
        assert!(get("s1", "4", &dir).is_none() && get("other", "3", &dir).is_none());
        // a changed file is never served from memory
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(dir.join("a.go"), "package a\n\nfunc One() {\n\treturn 1\n}\n").unwrap();
        assert!(get("s1", "3", &dir).is_none());
        // words: rebuilt when the file changes
        let l1: Vec<String> = vec!["import store".into()];
        assert!(words(&dir, "a.go", &l1).contains("store"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
