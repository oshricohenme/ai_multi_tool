//! Every indexed file's text, in memory, inside the daemon. `mt serve` turns it on and loads
//! every file in parallel at start; `Graph::apply` (the watcher, `touch`, `mt apply`) keeps it
//! current, and the cookie barrier means a request never sees text older than the disk. Text
//! scans (grep, module, property, span, skeleton, context bodies) then never touch the disk.
//! Off (the CLI's in-process path, and tests), `get` reads the disk as before.
//! Keys are absolute paths, so two repos in one process (tests) never share an entry.
//! ponytail: one process-wide map behind an RwLock (the daemon serves one repo); shard it if
//! parallel requests ever contend on it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

static ON: AtomicBool = AtomicBool::new(false);
static MAP: OnceLock<RwLock<HashMap<String, Arc<str>>>> = OnceLock::new();

fn map() -> &'static RwLock<HashMap<String, Arc<str>>> {
    MAP.get_or_init(Default::default)
}

fn key(root: &Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().into_owned()
}

fn read_disk(root: &Path, rel: &str) -> Option<Arc<str>> {
    std::fs::read(root.join(rel)).ok().map(|b| Arc::from(String::from_utf8_lossy(&b).as_ref()))
}

/// Turn the cache on (the daemon only).
pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The file's text: from memory when the daemon holds it, else from disk.
pub fn get(root: &Path, rel: &str) -> Option<Arc<str>> {
    if on() {
        if let Some(t) = map().read().unwrap_or_else(|e| e.into_inner()).get(&key(root, rel)) {
            return Some(t.clone());
        }
    }
    read_disk(root, rel)
}

/// Read these files from disk on every core and hold them (daemon start).
pub fn load_all(root: &Path, rels: &[String]) {
    if !on() { return; }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let chunk = rels.len().div_ceil(threads).max(1);
    let loaded: Vec<(String, Arc<str>)> = std::thread::scope(|s| {
        let hs: Vec<_> = rels.chunks(chunk).map(|c| s.spawn(move || c.iter().filter_map(|r| Some((key(root, r), read_disk(root, r)?))).collect::<Vec<_>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut m = map().write().unwrap_or_else(|e| e.into_inner());
    m.extend(loaded);
}

/// A file changed or appeared: hold its new text. Removed: forget it.
pub fn refresh(root: &Path, changed: &[String], removed: &[String]) {
    if !on() { return; }
    let fresh: Vec<(String, Option<Arc<str>>)> = changed.iter().map(|r| (key(root, r), read_disk(root, r))).collect();
    let mut m = map().write().unwrap_or_else(|e| e.into_inner());
    for r in removed { m.remove(&key(root, r)); }
    for (k, t) in fresh {
        match t { Some(t) => { m.insert(k, t); } None => { m.remove(&k); } }
    }
}

/// (files, bytes) held, for `mt serve --status`.
pub fn size() -> (usize, usize) {
    let m = map().read().unwrap_or_else(|e| e.into_inner());
    (m.len(), m.values().map(|t| t.len()).sum())
}
