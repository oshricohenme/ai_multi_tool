//! The in-memory graph the daemon (`mt serve`) holds: every file's definitions, identifier
//! references and table facts, with name -> file adjacency so "who references X" and "where is X
//! defined" are hash lookups. Loaded once from `.mt/index.sqlite`, then patched per changed file.
//!
//! Nodes are definitions; edges are resolved by name at query time (reference to `X` inside a
//! definition's span = an edge to every definition named `X`). Name-based, like the SQLite
//! index: an edge to a name defined in several files is labelled "possible" by the callers.

use crate::index::{norm, DefRow, Extracted, Index, Store};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct FileData {
    pub path: String,
    pub role: String,
    pub lang: String,
    pub stamp: (i64, i64),
    /// fnv1a of the content last parsed (0: unknown, e.g. indexed before hashes existed)
    pub hash: i64,
    /// sorted by start line
    pub defs: Vec<DefRow>,
    pub refs: HashMap<String, Vec<usize>>,
    /// (name, 0-based line, "define" | "use")
    pub tables: Vec<(String, usize, String)>,
}

pub struct Graph {
    root: PathBuf,
    files: Vec<Option<FileData>>,
    id_of: HashMap<String, u32>,
    /// last name segment -> files defining it
    by_last: HashMap<String, Vec<u32>>,
    /// identifier -> files mentioning it
    ref_files: HashMap<String, Vec<u32>>,
    /// normalized name (`userid`) -> every spelling seen (`userID`, `user_id`, `UserId`)
    norms: HashMap<String, Vec<String>>,
    next_def: i64,
}

fn last_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, l)| l).unwrap_or(name)
}

impl Graph {
    /// Load everything from SQLite. The big tables are read in rowid ranges on every core, each
    /// thread with its own read-only connection (WAL allows concurrent readers), and linked into
    /// the name maps in parallel shards.
    pub fn load(idx: &Index) -> rusqlite::Result<Graph> {
        let t0 = std::time::Instant::now();
        let mut g = Graph { root: idx.root.clone(), files: vec![], id_of: HashMap::new(), by_last: HashMap::new(), ref_files: HashMap::new(), norms: HashMap::new(), next_def: 1 };
        let mut by_db: HashMap<i64, u32> = HashMap::new();
        {
            let db = idx.conn();
            let mut q = db.prepare("SELECT id, path, role, lang, mtime, size, hash FROM files")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, Option<i64>>(6)?.unwrap_or(0))))?;
            for (id, path, role, lang, m, z, hash) in rows.flatten() {
                by_db.insert(id, g.files.len() as u32);
                g.id_of.insert(path.clone(), g.files.len() as u32);
                g.files.push(Some(FileData { path, role, lang, stamp: (m, z), hash, defs: vec![], refs: HashMap::new(), tables: vec![] }));
            }
        }
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        type DefRaw = (i64, i64, String, String, String, i64, i64, i64, String);
        let (defs, refs, tables): (Vec<DefRaw>, Vec<(i64, String, Vec<usize>)>, Vec<(i64, String, i64, String)>) = std::thread::scope(|s| {
            let path = &idx.db_path;
            let ranges = |table: &str| -> Vec<(i64, i64)> {
                let max: i64 = idx.conn().query_row(&format!("SELECT COALESCE(MAX(rowid),0) FROM {table}"), [], |r| r.get(0)).unwrap_or(0);
                let step = (max / threads as i64).max(1);
                (0..threads as i64).map(|i| (i * step + 1, if i == threads as i64 - 1 { max } else { (i + 1) * step })).filter(|(a, b)| a <= b).collect()
            };
            let open = move || rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX);
            let dh: Vec<_> = ranges("defs").into_iter().map(|(a, b)| s.spawn(move || -> Vec<DefRaw> {
                let Ok(db) = open() else { return vec![] };
                let Ok(mut q) = db.prepare("SELECT file, id, kind, name, scope, start, end, head, sig FROM defs WHERE rowid BETWEEN ?1 AND ?2") else { return vec![] };
                q.query_map([a, b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))).map(|it| it.flatten().collect()).unwrap_or_default()
            })).collect();
            let rh: Vec<_> = ranges("refs").into_iter().map(|(a, b)| s.spawn(move || -> Vec<(i64, String, Vec<usize>)> {
                let Ok(db) = open() else { return vec![] };
                let Ok(mut q) = db.prepare("SELECT file, name, lines FROM refs WHERE rowid BETWEEN ?1 AND ?2") else { return vec![] };
                q.query_map([a, b], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                    .map(|it| it.flatten().map(|(f, n, l)| (f, n, l.split(',').filter_map(|x| x.parse().ok()).collect())).collect())
                    .unwrap_or_default()
            })).collect();
            let th = s.spawn(move || -> Vec<(i64, String, i64, String)> {
                let Ok(db) = open() else { return vec![] };
                let Ok(mut q) = db.prepare("SELECT file, name, line, kind FROM tables") else { return vec![] };
                q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|it| it.flatten().collect()).unwrap_or_default()
            });
            (dh.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect(), rh.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect(), th.join().unwrap_or_default())
        });
        let t_read = t0.elapsed().as_millis();
        for (file, id, kind, name, scope, start, end, head, sig) in defs {
            let Some(&f) = by_db.get(&file) else { continue };
            let Some(fd) = g.files[f as usize].as_mut() else { continue };
            g.next_def = g.next_def.max(id + 1);
            fd.defs.push(DefRow { id, path: fd.path.clone(), role: fd.role.clone(), kind, name, scope, start: start as usize, end: end as usize, head: head as usize, sig });
        }
        for (file, name, lines) in refs {
            let Some(&f) = by_db.get(&file) else { continue };
            if let Some(fd) = g.files[f as usize].as_mut() {
                fd.refs.insert(name, lines);
            }
        }
        for (file, name, line, kind) in tables {
            let Some(&f) = by_db.get(&file) else { continue };
            if let Some(fd) = g.files[f as usize].as_mut() {
                fd.tables.push((name, line as usize, kind));
            }
        }
        let t_assign = t0.elapsed().as_millis();
        g.link_all(threads);
        // the daemon also holds every file's text (texts.rs); a no-op unless it turned that on
        let t_text = std::time::Instant::now();
        let paths: Vec<String> = g.all().map(|f| f.path.clone()).collect();
        crate::texts::load_all(&g.root, &paths);
        if crate::texts::on() { let (n, b) = crate::texts::size(); println!("texts: {n} files, {} MB in memory, loaded in {} ms", b / 1_000_000, t_text.elapsed().as_millis()); }
        if std::env::var_os("MT_TIMING").is_some() {
            eprintln!("graph load: read {t_read} ms, assign {} ms, link {} ms ({threads} threads)", t_assign - t_read, t0.elapsed().as_millis() - t_assign);
        }
        Ok(g)
    }

    /// Build the name maps for every file at once: each thread takes a slice of files and builds
    /// partial maps, which are then merged.
    fn link_all(&mut self, threads: usize) {
        for fd in self.files.iter_mut().flatten() {
            fd.defs.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
        }
        type Maps = (HashMap<String, Vec<u32>>, HashMap<String, Vec<u32>>);
        let files = &self.files;
        let chunk = files.len().div_ceil(threads).max(1);
        let parts: Vec<Maps> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..files.len()).step_by(chunk).map(|a| s.spawn(move || {
                let (mut by_last, mut refs): Maps = (HashMap::new(), HashMap::new());
                for f in a..(a + chunk).min(files.len()) {
                    let Some(fd) = files[f].as_ref() else { continue };
                    let mut lasts: Vec<&str> = fd.defs.iter().map(|d| last_of(&d.name)).collect();
                    lasts.sort();
                    lasts.dedup();
                    for l in lasts { by_last.entry(l.to_string()).or_default().push(f as u32); }
                    for n in fd.refs.keys() { refs.entry(n.clone()).or_default().push(f as u32); }
                }
                (by_last, refs)
            })).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for (bl, rf) in parts {
            for (k, v) in bl { self.by_last.entry(k).or_default().extend(v); }
            for (k, v) in rf { self.ref_files.entry(k).or_default().extend(v); }
        }
        for n in self.ref_files.keys() {
            self.norms.entry(norm(n)).or_default().push(n.clone());
        }
    }

    /// path -> (mtime, size): what this graph holds, for `Index::refresh`.
    pub fn known(&self) -> HashMap<String, (i64, i64)> {
        self.files.iter().flatten().map(|f| (f.path.clone(), f.stamp)).collect()
    }

    /// `known()` for these paths and every file under any of them that is (or was) a directory:
    /// what `refresh(.., Some(paths))` needs. Saves cloning every path in the repo per burst.
    pub fn known_for(&self, rels: &[String]) -> HashMap<String, (i64, i64)> {
        let mut m = HashMap::new();
        // paths we do not know may be directories (new, or deleted): one pass over the graph finds
        // the files under any of them (a temp file per edited file must not cost a scan each)
        let mut unknown: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for r in rels {
            match self.file(r) {
                Some(f) => { m.insert(r.clone(), f.stamp); }
                None => { unknown.insert(r.as_str()); }
            }
        }
        if !unknown.is_empty() {
            for f in self.all() {
                let mut p = f.path.as_str();
                while let Some((parent, _)) = p.rsplit_once('/') {
                    if unknown.contains(parent) { m.insert(f.path.clone(), f.stamp); break; }
                    p = parent;
                }
            }
        }
        m
    }

    /// The content hashes held for these paths (the graph is the truth in the daemon; the
    /// SQLite copy may lag behind its writer thread).
    pub fn hashes(&self, paths: &[&str]) -> HashMap<String, i64> {
        paths.iter().filter_map(|p| self.file(p).filter(|f| f.hash != 0).map(|f| (p.to_string(), f.hash))).collect()
    }

    /// Files whose content did not change: only their stamps move.
    pub fn restamp(&mut self, list: &[(String, i64, i64)]) {
        for (p, m, z) in list {
            if let Some(&f) = self.id_of.get(p) { if let Some(fd) = self.files[f as usize].as_mut() { fd.stamp = (*m, *z); } }
        }
    }

    fn link(&mut self, f: u32) {
        let Some(fd) = self.files[f as usize].as_mut() else { return };
        fd.defs.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
        let mut lasts: Vec<&str> = fd.defs.iter().map(|d| last_of(&d.name)).collect();
        lasts.sort();
        lasts.dedup();
        for l in lasts {
            self.by_last.entry(l.to_string()).or_default().push(f);
        }
        for n in fd.refs.keys() {
            let e = self.ref_files.entry(n.clone()).or_default();
            if e.is_empty() {
                let v = self.norms.entry(norm(n)).or_default();
                if !v.contains(n) { v.push(n.clone()); }
            }
            e.push(f);
        }
    }

    fn unlink(&mut self, f: u32) {
        let Some(fd) = self.files[f as usize].take() else { return };
        for d in &fd.defs {
            if let Some(v) = self.by_last.get_mut(last_of(&d.name)) {
                v.retain(|&x| x != f);
            }
        }
        for n in fd.refs.keys() {
            if let Some(v) = self.ref_files.get_mut(n) {
                v.retain(|&x| x != f);
            }
        }
    }

    /// Patch the graph with what `Index::refresh` re-parsed and removed.
    pub fn apply(&mut self, extracted: &[Extracted], removed: &[String]) {
        crate::texts::refresh(&self.root, &extracted.iter().map(|e| e.rel.clone()).collect::<Vec<_>>(), removed);
        for p in removed {
            if let Some(f) = self.id_of.remove(p) {
                self.unlink(f);
            }
        }
        for e in extracted {
            let f = match self.id_of.get(&e.rel) {
                Some(&f) => { self.unlink(f); f }
                None => {
                    let f = self.files.len() as u32;
                    self.files.push(None);
                    self.id_of.insert(e.rel.clone(), f);
                    f
                }
            };
            let mut defs = vec![];
            for (d, sig) in &e.defs {
                defs.push(DefRow { id: self.next_def, path: e.rel.clone(), role: e.role.to_string(), kind: d.kind.clone(), name: d.name.clone(), scope: d.path.join("."), start: d.start, end: d.end, head: d.head, sig: sig.clone() });
                self.next_def += 1;
            }
            self.files[f as usize] = Some(FileData {
                path: e.rel.clone(), role: e.role.to_string(), lang: e.lang.clone(), stamp: (e.mtime, e.size), hash: e.hash, defs,
                refs: e.refs.iter().cloned().collect(),
                tables: e.tables.iter().map(|(n, l, k)| (n.clone(), *l, k.to_string())).collect(),
            });
            self.link(f);
        }
    }

    fn file(&self, path: &str) -> Option<&FileData> {
        self.id_of.get(path).and_then(|&f| self.files[f as usize].as_ref())
    }

    fn all(&self) -> impl Iterator<Item = &FileData> {
        self.files.iter().flatten()
    }
}

impl Store for Graph {
    fn root(&self) -> &Path {
        &self.root
    }

    fn file_count(&self) -> usize {
        self.id_of.len()
    }

    fn defs_named(&self, last: &str) -> Vec<DefRow> {
        let Some(fs) = self.by_last.get(last) else { return vec![] };
        fs.iter().filter_map(|&f| self.files[f as usize].as_ref()).flat_map(|fd| fd.defs.iter().filter(|d| last_of(&d.name) == last).cloned()).collect()
    }

    fn defs_in(&self, path: &str) -> Vec<DefRow> {
        self.file(path).map(|f| f.defs.clone()).unwrap_or_default()
    }

    fn def_count(&self, last: &str) -> usize {
        self.by_last.get(last).map(|v| v.len()).unwrap_or(0)
    }

    fn refs(&self, name: &str) -> Vec<(String, String, Vec<usize>)> {
        let Some(fs) = self.ref_files.get(name) else { return vec![] };
        fs.iter().filter_map(|&f| self.files[f as usize].as_ref()).filter_map(|fd| fd.refs.get(name).map(|l| (fd.path.clone(), fd.role.clone(), l.clone()))).collect()
    }

    fn defined_tables(&self) -> HashMap<String, Vec<(String, usize)>> {
        let mut out: HashMap<String, Vec<(String, usize)>> = HashMap::new();
        for fd in self.all() {
            if !matches!(fd.role.as_str(), "migration" | "model" | "query" | "source") || fd.lang == "markdown" || fd.lang == "text" {
                continue;
            }
            for (n, l, k) in &fd.tables {
                if k == "define" {
                    out.entry(n.to_lowercase()).or_default().push((fd.path.clone(), *l));
                }
            }
        }
        out
    }

    fn files_with_roles(&self, roles: &[&str]) -> Vec<(String, String)> {
        self.all().filter(|f| roles.contains(&f.role.as_str())).map(|f| (f.path.clone(), f.role.clone())).collect()
    }

    fn table_uses(&self, table: &str) -> Vec<(String, String, usize)> {
        let mut out = vec![];
        for fd in self.all() {
            for (n, l, k) in &fd.tables {
                if k == "use" && n.eq_ignore_ascii_case(table) {
                    out.push((fd.path.clone(), fd.role.clone(), *l));
                }
            }
        }
        out
    }

    fn spellings(&self, name: &str) -> Vec<String> {
        let mut v: Vec<String> = self.norms.get(&norm(name)).map(|v| v.iter().filter(|n| self.ref_files.get(*n).map(|f| !f.is_empty()).unwrap_or(false)).cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }

    fn text_files(&self) -> Vec<(String, String)> {
        self.all().filter(|f| f.lang != "binary" && f.lang != "skipped" && !f.lang.contains("too large")).map(|f| (f.path.clone(), f.role.clone())).collect()
    }
}
