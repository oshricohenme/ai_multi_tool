//! The repo index: `.mt/index.sqlite` at the repo root (a self-ignoring hidden folder).
//!
//! Serverless: every query first runs an incremental update, which stats the tree and
//! re-parses only files whose mtime or size changed.
//!
//! Tables
//!   files  path, mtime, size, lang (tier), role (source/test/integration/migration/model/query/doc/config)
//!   defs   every definition: kind, name, last name segment, scope, span, declaration line, signature
//!   refs   per file, every identifier (3+ chars) with the lines it occurs on. Name-based, so
//!          comments and strings count too; that is how table names in SQL strings are found.
//!   tables data-layer facts: `define` (CREATE TABLE, ORM mapping) and `use` (FROM/JOIN/INTO/UPDATE)

use crate::languages::{detect, Tier};
use crate::{definitions_with, Def};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The rows a lookup returned, counted for the answer's `[mt] work:` line.
fn counted<T>(v: Vec<T>) -> Vec<T> {
    crate::work::rows(v.len());
    v
}

/// SQLite said another connection holds the write lock past the busy timeout.
pub fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

/// The lookup tables one-shot commands, `mt tools` and the visualizer query through (the daemon
/// answers from memory and needs none of them): name, and what it indexes.
const LOOKUPS: [(&str, &str); 8] = [
    ("defs_last", "defs(last)"), ("defs_file", "defs(file)"),
    // a reference stores its name as a number (`names.id`): sorting numbers, not 2.4M strings;
    // the `refs` view shows the names
    ("refs_name", "refs_ids(name_id)"),
    ("tables_name", "tables(name COLLATE NOCASE)"), ("tables_file", "tables(file)"),
    // plain-word search: a word's rows (one per file, its postings packed). A file's postings and its
    // references are each one block of rows (files.post_lo..post_hi, refs_lo..refs_hi): read and
    // deleted by rowid range, with no index of their own.
    ("search_term", "search_post(term)"),
    // quoted paths and routes: by place (a file's lines; deletes), and by text (a route's prefix)
    ("lits_at", "lits(file, line)"), ("lits_text", "lits(kind, text)"),
];

/// Quoted paths and routes kept per file. ponytail: a file with more (a generated route table, a
/// minified bundle) has the rest unmatched by `context`'s route tests; raise it if that shows up.
const MAX_LITS: usize = 5000;

/// A full index parses and writes this many files at a time, so its memory stays flat however
/// large the repo.
const BATCH: usize = 2000;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The steps of an index (walk, parse, write, lookups, server), one line each, drawn together on
/// stderr and redrawn in place at most 10 times a second: a person at a terminal sees every step
/// move at once, with a bar and its time, and the finished ones stay. Off when stderr is not a
/// terminal: hooks, the daemon's log, pipes, CI.
struct Board { rows: Vec<Row>, drawn: usize, last: Option<Instant> }
struct Row { name: &'static str, text: String, start: Instant, took: Option<Duration> }

fn board() -> &'static std::sync::Mutex<Board> {
    static B: OnceLock<std::sync::Mutex<Board>> = OnceLock::new();
    B.get_or_init(|| std::sync::Mutex::new(Board { rows: vec![], drawn: 0, last: None }))
}

fn progress_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| { use std::io::IsTerminal; std::io::stderr().is_terminal() })
}

/// `██████████░░░░░░░░░░░░░░`: `done` of `total`, 24 cells.
pub fn bar(done: usize, total: usize) -> String {
    let w = 24;
    let full = if total == 0 { w } else { (done.min(total) * w) / total };
    format!("{}{}", "█".repeat(full), "░".repeat(w - full))
}

impl Board {
    fn set(&mut self, name: &'static str, text: String, finished: bool, force: bool) {
        match self.rows.iter_mut().find(|r| r.name == name) {
            Some(r) => { r.text = text; if finished && r.took.is_none() { r.took = Some(r.start.elapsed()); } }
            None => {
                let start = Instant::now();
                self.rows.push(Row { name, text, start, took: finished.then(|| start.elapsed()) });
            }
        }
        if !force && self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) { return; }
        self.last = Some(Instant::now());
        let mut out = String::new();
        // back to the board's first line, then every row again; the cursor ends below the board
        if self.drawn > 0 { out.push_str(&format!("\x1b[{}F", self.drawn)); }
        for r in &self.rows {
            let t = r.took.unwrap_or_else(|| r.start.elapsed()).as_secs_f64();
            let mark = if r.took.is_some() { "✓" } else { "·" };
            out.push_str(&format!("\x1b[2K{mark} {:<8} {}  {t:.1} s\n", r.name, r.text));
        }
        self.drawn = self.rows.len();
        eprint!("{out}");
    }
}

/// One step's line on the board. Off when stderr is not a terminal.
pub struct Progress { pub on: bool, row: &'static str }

impl Progress {
    pub fn row(name: &'static str) -> Self { Progress { on: progress_on(), row: name } }
    /// The step's line now (redrawn at most 10 times a second; `msg` is only built when it is).
    pub fn show(&mut self, msg: impl FnOnce() -> String) {
        if !self.on { return; }
        let mut b = board().lock().unwrap_or_else(|e| e.into_inner());
        if b.last.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) && b.rows.iter().any(|r| r.name == self.row) { return; }
        b.set(self.row, msg(), false, false);
    }
    /// The step is finished: its last line, and its time stops.
    pub fn done(&mut self, msg: impl FnOnce() -> String) {
        if !self.on { return; }
        board().lock().unwrap_or_else(|e| e.into_inner()).set(self.row, msg(), true, true);
    }
    /// Run one long step that cannot count its own progress (a commit, an index build): the line
    /// says what it is doing, and its time keeps moving.
    pub fn during<T>(&mut self, msg: &str, f: impl FnOnce() -> T) -> T {
        if !self.on { return f(); }
        let stop = std::sync::atomic::AtomicBool::new(false);
        let row = self.row;
        std::thread::scope(|s| {
            s.spawn(|| while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                board().lock().unwrap_or_else(|e| e.into_inner()).set(row, msg.to_string(), false, true);
                std::thread::sleep(Duration::from_millis(100));
            });
            let out = f();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            out
        })
    }
}

const SCHEMA_VERSION: &str = "10";
const MAX_FILE: u64 = 1_500_000;
const MAX_DATA_FILE: u64 = 200_000;

/// Never part of the repo: version control internals and mt's own folder. Everything else that git
/// does not ignore is indexed (`.gitignore`, `.git/info/exclude`, the global excludes; hidden files
/// too). There is no other skip list.
const NEVER: &[&str] = &[".git", ".hg", ".svn", ".jj", ".mt"];

/// Media, archives and compiled files: listed as binary without being read (their bytes are no
/// use to a code index and can be large).
const BINARY_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "icns", "bmp", "tga", "psd", "exr", "hdr", "tif", "tiff", "svgz",
    "mp3", "wav", "ogg", "flac", "mp4", "mov", "webm", "avi", "ttf", "otf", "woff", "woff2", "eot",
    "zip", "gz", "tgz", "bz2", "xz", "7z", "rar", "jar", "war", "dll", "so", "dylib", "a", "o", "obj", "exe", "pdb", "class",
    "pyc", "wasm", "bin", "dat", "db", "sqlite", "sqlite3", "npz", "npy", "pkl", "pt", "onnx", "glb", "fbx", "blend",
    "pdf", "parquet", "bmap", "bop", "pcm",
];

/// The tree as git sees it, from `dir`: what `.gitignore` (at every level, and in `dir`'s parents),
/// `.git/info/exclude` and the global excludes leave in, hidden files included, never `NEVER`.
pub fn walk(dir: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(dir).hidden(false).ignore(false).require_git(false)
        .filter_entry(|e| !e.file_name().to_str().is_some_and(|n| NEVER.contains(&n)))
        .build()
}

pub struct Index {
    pub root: PathBuf,
    /// behind a mutex so the index is a `Sync` store: query threads share it, lookups serialize
    pub db: std::sync::Mutex<Connection>,
    pub db_path: PathBuf,
    /// each folder's `.gitignore`, parsed once (folder relative to the root -> its rules)
    ignores: std::sync::Mutex<HashMap<String, Option<std::sync::Arc<ignore::gitignore::Gitignore>>>>,
}

#[derive(Default, Debug)]
pub struct UpdateStats {
    /// the refresh walked the whole tree (no path list)
    pub walked: bool,
    /// files whose mtime/size changed but whose content did not: stamp updated, nothing re-parsed
    pub restamped: Vec<(String, i64, i64)>,
    pub files: usize,
    pub changed: usize,
    pub removed: usize,
    pub walk_ms: u128,
    pub parse_ms: u128,
    pub write_ms: u128,
}

#[derive(Clone, Debug)]
pub struct DefRow {
    pub id: i64,
    pub path: String,
    pub role: String,
    pub kind: String,
    pub name: String,
    pub scope: String,
    pub start: usize,
    pub end: usize,
    pub head: usize,
    pub sig: String,
}

/// Would a full walk index this repo-relative path? (Ignoring .gitignore, which only the walk applies.)
/// Markdown files that are generated per-file cards of the code: under a top-level directory where
/// at least 20 markdown files, and 80% of all its markdown, are named after a source file at the
/// same relative path (`<dir>/internal/api/x.md` for `internal/api/x.go`). Language-agnostic: it
/// is a fact about paths, so it catches any tool that mirrors the tree as docs.
pub fn mirrors(paths: &std::collections::HashSet<String>) -> std::collections::HashSet<String> {
    let stem = |p: &str| p.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or_else(|| p.to_string());
    let sources: std::collections::HashSet<String> = paths.iter().filter(|p| !p.ends_with(".md")).map(|p| stem(p)).collect();
    let mut by_dir: HashMap<&str, (Vec<&String>, usize)> = HashMap::new();
    for p in paths.iter().filter(|p| p.ends_with(".md")) {
        let Some((top, rest)) = p.split_once('/') else { continue };
        let e = by_dir.entry(top).or_default();
        e.1 += 1;
        if sources.contains(&stem(rest)) { e.0.push(p); }
    }
    by_dir.into_values().filter(|(twins, all)| twins.len() >= 20 && twins.len() * 5 >= all * 4).flat_map(|(t, _)| t.into_iter().cloned()).collect()
}

/// Worth a look when the watcher reports it: not under `NEVER` and not mt's own temp file.
/// Directories pass (their files may be). Whether git ignores it is `Index::ignored`.
pub fn watch_relevant(rel: &str) -> bool {
    !rel.split('/').any(|s| NEVER.contains(&s)) && !rel.contains(".mt-tmp-")
}

pub fn indexable(rel: &str) -> bool {
    !rel.split('/').any(|s| NEVER.contains(&s))
}

fn binary_by_ext(rel: &str) -> bool {
    let lower = rel.to_lowercase();
    lower.rsplit_once('.').is_some_and(|(_, x)| BINARY_EXT.contains(&x))
}

pub fn find_root(start: &Path) -> PathBuf {
    let start = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if d.join(".mt").is_dir() || d.join(".git").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    start
}

pub fn role_of(rel: &str) -> &'static str {
    let p = rel.to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let seg = |s: &str| p.split('/').any(|x| x == s);
    if seg("e2e") || seg("integration") || seg("integration-tests") || seg("playwright") || seg("cypress") || seg("acceptance") || seg("functional")
        || name.contains("e2e") || name.contains("integration") || name.contains(".int.") {
        return "integration";
    }
    if name.ends_with("_test.go") || (name.starts_with("test_") && name.ends_with(".py")) || name.ends_with("_test.py")
        || [".test.", ".spec.", "_spec.", "tests.cs", "test.cs", "test.java", "tests.java", "test.kt", "spec.scala", "_test.rs", "tests.swift", "test.swift"].iter().any(|s| name.contains(s))
        || seg("tests") || seg("test") || seg("__tests__") || seg("spec") || seg("testing") {
        return "test";
    }
    if name.ends_with(".md") || name.ends_with(".mdx") || name.ends_with(".rst") || name.ends_with(".txt") {
        return "doc";
    }
    if seg("migrations") || seg("migration") || seg("migrate") || (name.ends_with(".sql") && p.contains("migrat")) {
        return "migration";
    }
    if name == "schema.prisma" || name.ends_with(".prisma") || name == "models.py" || name == "models.go" || name.contains(".model.") || name.ends_with("model.cs")
        || seg("models") || seg("model") || seg("entities") || seg("entity") || seg("schema") || seg("schemas") {
        return "model";
    }
    if name.ends_with(".sql") || name.ends_with(".sql.go") || seg("queries") || seg("dbq") || seg("repository") || seg("repositories") {
        return "query";
    }
    if [".json", ".yaml", ".yml", ".toml", ".xml", ".ini", ".env"].iter().any(|s| name.ends_with(s)) {
        return "config";
    }
    "source"
}

fn ident_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap())
}

fn table_define_rx() -> &'static [Regex] {
    static RX: OnceLock<Vec<Regex>> = OnceLock::new();
    RX.get_or_init(|| {
        [
            r#"(?i)\bcreate\s+(?:unlogged\s+|temp(?:orary)?\s+)?table\s+(?:if\s+not\s+exists\s+)?[`"\[]?(?:\w+[`"\]]?\.)?[`"\[]?(\w+)"#,
            r#"^\s*model\s+(\w+)\s*\{"#,
            r#"@@map\(\s*"(\w+)"\s*\)"#,
            r#"__tablename__\s*=\s*['"](\w+)['"]"#,
            r#"\bdb_table\s*=\s*['"](\w+)['"]"#,
            r#"\[Table\(\s*"(\w+)""#,
            r#"@Table\(\s*name\s*=\s*"(\w+)""#,
            r#"TableName\(\)\s*string\s*\{\s*return\s*"(\w+)""#,
        ]
        .iter()
        .map(|r| Regex::new(r).unwrap())
        .collect()
    })
}

fn table_use_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r#"(?i)\b(?:from|join|into|update|references|alter\s+table)\s+[`"\[]?(?:\w+[`"\]]?\.)?[`"\[]?([A-Za-z_]\w*)"#).unwrap())
}

pub struct Extracted {
    pub rel: String,
    pub mtime: i64,
    pub size: i64,
    pub lang: String,
    pub role: &'static str,
    pub lines: usize,
    /// Each definition with its signature line (trimmed, at most 200 chars).
    pub defs: Vec<(Def, String)>,
    /// identifier -> 1-based lines (at most 300 per file)
    pub refs: Vec<(String, Vec<usize>)>,
    pub tables: Vec<(String, usize, &'static str)>,
    /// fnv1a of the file's bytes
    pub hash: i64,
    /// each definition as a plain-word search document (weighted words, length), aligned with `defs`
    pub search: Vec<Option<(Vec<(String, f32)>, f32)>>,
    /// (1-based line, kind, text): `route` (a URL route the line registers, as `context` reads it),
    /// `path` (a quoted string starting with `/`: the run of path characters after the quote)
    pub lits: Vec<(usize, &'static str, String)>,
}

fn content_hash(bytes: &[u8]) -> i64 {
    crate::fnv1a(bytes) as i64
}

pub fn extract(root: &Path, rel: &str, mtime: i64, size: i64) -> Extracted {
    let role = role_of(rel);
    let mut ex = Extracted { rel: rel.to_string(), mtime, size, lang: "skipped".into(), role, lines: 0, defs: vec![], refs: vec![], tables: vec![], hash: 0, search: vec![], lits: vec![] };
    // listed, not read: too large to index, or a media, archive or compiled file
    if size as u64 > MAX_FILE { ex.lang = "(too large to index)".into(); return ex; }
    if binary_by_ext(rel) { ex.lang = "binary".into(); return ex; }
    let Ok(bytes) = fs::read(root.join(rel)) else { return ex };
    ex.hash = content_hash(&bytes);
    if bytes.iter().take(8000).any(|&b| b == 0) {
        ex.lang = "binary".into();
        return ex;
    }
    let src = String::from_utf8_lossy(&bytes);
    let tier = detect(Path::new(rel), &src);
    ex.lang = tier.label();
    ex.lines = src.lines().count();
    let is_data = matches!(&tier, Tier::TreeSitter(l) if l.max_depth.is_some()) && !rel.ends_with(".html");
    if is_data && size as u64 > MAX_DATA_FILE {
        ex.lang = format!("{} (too large to index)", ex.lang);
        return ex;
    }
    if matches!(tier, Tier::Text) && role != "doc" {
        return ex;
    }
    let src_lines: Vec<&str> = src.lines().collect();
    ex.defs = definitions_with(&tier, &src).into_iter().map(|d| {
        let sig: String = src_lines.get(d.head).map(|l| l.trim()).unwrap_or("").chars().take(200).collect();
        (d, sig)
    }).collect();
    ex.search = crate::search::postings_for(rel, role, &ex.defs, &src_lines);
    let mut refs: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, line) in src.lines().enumerate() {
        for m in ident_rx().find_iter(line) {
            let v = refs.entry(m.as_str()).or_default();
            if v.last() != Some(&(i + 1)) && v.len() < 300 {
                v.push(i + 1);
            }
        }
        for rx in table_define_rx() {
            for c in rx.captures_iter(line) {
                ex.tables.push((c[1].to_string(), i, "define"));
            }
        }
        if line.len() < 2000 {
            for c in table_use_rx().captures_iter(line) {
                ex.tables.push((c[1].to_string(), i, "use"));
            }
        }
        if line.contains('/') && ex.lits.len() < MAX_LITS {
            for c in crate::context::route_rx().captures_iter(line) {
                ex.lits.push((i + 1, "route", crate::context::route_of(&c[1])));
            }
            // a quote, then `/`, then the longest run of path characters: a test line holds `"<route>`
            // exactly when one of its runs starts with the route (a route is path characters only)
            let b = line.as_bytes();
            for j in 0..b.len().saturating_sub(1) {
                if matches!(b[j], b'"' | b'\'' | b'`') && b[j + 1] == b'/' {
                    let mut k = j + 1;
                    while k < b.len() && (b[k].is_ascii_alphanumeric() || b"_-./{}:".contains(&b[k])) { k += 1; }
                    ex.lits.push((i + 1, "path", line[j + 1..k].to_string()));
                }
            }
        }
    }
    ex.refs = refs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    ex
}

impl Index {
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        crate::work::lookup();
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn open(root: &Path) -> rusqlite::Result<Index> {
        let dir = root.join(".mt");
        let _ = fs::create_dir_all(&dir);
        if !dir.join(".gitignore").exists() {
            let _ = fs::write(dir.join(".gitignore"), "*\n");
        }
        let db = Connection::open(dir.join("index.sqlite"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;")?;
        Self::schema(&db)?;
        Ok(Index { root: root.to_path_buf(), db: std::sync::Mutex::new(db), db_path: dir.join("index.sqlite"), ignores: Default::default() })
    }

    /// The tables, created when missing or when they are another schema version's (the index is
    /// then empty again, and the next update rebuilds it).
    fn schema(db: &Connection) -> rusqlite::Result<()> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);")?;
        let v: Option<String> = db.query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0)).ok();
        if v.as_deref() != Some(SCHEMA_VERSION) {
            db.execute_batch(
                "DROP VIEW IF EXISTS refs; DROP TABLE IF EXISTS refs; DROP TABLE IF EXISTS refs_ids; DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS defs; DROP TABLE IF EXISTS tables;
                 DROP TABLE IF EXISTS search_post; DROP TABLE IF EXISTS search_doc; DROP TABLE IF EXISTS names; DROP TABLE IF EXISTS lits;
                 CREATE TABLE lits(file INTEGER NOT NULL, line INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);
                 CREATE INDEX lits_at ON lits(file, line); CREATE INDEX lits_text ON lits(kind, text);
                 CREATE TABLE search_post(term TEXT NOT NULL, file INTEGER NOT NULL, posts BLOB NOT NULL);
                 CREATE INDEX search_term ON search_post(term);
                 CREATE TABLE search_doc(def INTEGER PRIMARY KEY, len REAL NOT NULL);
                 CREATE TABLE names(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, norm TEXT NOT NULL);
                 CREATE INDEX names_norm ON names(norm);
                 CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, mtime INTEGER, size INTEGER, lang TEXT, role TEXT, lines INTEGER, post_lo INTEGER, post_hi INTEGER, refs_lo INTEGER, refs_hi INTEGER);
                 CREATE TABLE defs(id INTEGER PRIMARY KEY, file INTEGER NOT NULL, kind TEXT, name TEXT, last TEXT, scope TEXT, start INTEGER, end INTEGER, head INTEGER, sig TEXT);
                 CREATE INDEX defs_last ON defs(last); CREATE INDEX defs_file ON defs(file);
                 CREATE TABLE refs_ids(file INTEGER NOT NULL, name_id INTEGER NOT NULL, lines TEXT NOT NULL);
                 CREATE INDEX refs_name ON refs_ids(name_id);
                 CREATE VIEW refs AS SELECT r.file AS file, n.name AS name, r.lines AS lines FROM refs_ids r JOIN names n ON n.id = r.name_id;
                 CREATE TABLE tables(file INTEGER NOT NULL, name TEXT NOT NULL, line INTEGER, kind TEXT);
                 CREATE INDEX tables_name ON tables(name COLLATE NOCASE); CREATE INDEX tables_file ON tables(file);",
            )?;
            db.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema', ?1)", [SCHEMA_VERSION])?;
        }
        // content hash per file (added within schema 5, so builds without it keep sharing the
        // index): a file whose mtime changed but whose bytes did not is restamped, not re-parsed
        let has_hash = db.prepare("SELECT hash FROM files LIMIT 0").is_ok();
        if !has_hash { db.execute_batch("ALTER TABLE files ADD COLUMN hash INTEGER;")?; }
        Ok(())
    }

    /// A fresh index (empty: the first run, or a new schema) is built in its own file with no
    /// journal and an exclusive lock, then renamed into place. Writing it is about twice as fast
    /// (nothing written to a write-ahead log and copied back), and an interrupted build leaves only
    /// that file, which the next build removes: never a half-written index.
    fn fresh<T>(&self, build: impl FnOnce() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        let lock = || self.db.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.db_path.parent().map(Path::to_path_buf).unwrap_or_default();
        // builds of other processes, interrupted
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with("index.sqlite.new-") && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 600) {
                let _ = fs::remove_file(e.path());
            }
        }
        let new = dir.join(format!("index.sqlite.new-{}", std::process::id()));
        let _ = fs::remove_file(&new);
        let c = Connection::open(&new)?;
        c.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA locking_mode=EXCLUSIVE; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-65536;")?;
        Self::schema(&c)?;
        let old = std::mem::replace(&mut *lock(), c);
        let out = build();
        drop(std::mem::replace(&mut *lock(), old));
        if out.is_err() { let _ = fs::remove_file(&new); return out; }
        // swap it in: the old index closed, its log files gone (they would be replayed into the new
        // one), the new file renamed into place, and opened as any index is (write-ahead log on)
        let mut g = lock();
        drop(std::mem::replace(&mut *g, Connection::open_in_memory()?));
        for ext in ["sqlite-wal", "sqlite-shm"] { let _ = fs::remove_file(self.db_path.with_extension(ext)); }
        fs::rename(&new, &self.db_path).map_err(|_| rusqlite::Error::InvalidPath(new.clone()))?;
        let c = Connection::open(&self.db_path)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;")?;
        *g = c;
        out
    }

    /// Whether git ignores `rel`: the `.gitignore` of each folder from its own up to the root,
    /// deepest first (the first that decides wins, as in git), then `.git/info/exclude` and the
    /// global excludes. The watcher's paths are checked here; a full walk applies the same rules.
    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let abs = self.root.join(rel);
        let mut dirs: Vec<&str> = vec![""];
        dirs.extend(rel.char_indices().filter(|(_, c)| *c == '/').map(|(i, _)| &rel[..i]));
        for d in dirs.iter().rev() {
            if let Some(g) = self.ignore_rules(d) {
                let m = g.matched_path_or_any_parents(&abs, is_dir);
                if m.is_ignore() { return true; }
                if m.is_whitelist() { return false; }
            }
        }
        static GLOBAL: OnceLock<ignore::gitignore::Gitignore> = OnceLock::new();
        let g = GLOBAL.get_or_init(|| ignore::gitignore::Gitignore::global().0);
        !g.is_empty() && g.matched_path_or_any_parents(rel, is_dir).is_ignore()
    }

    /// A `.gitignore` changed: read the rules again.
    pub fn forget_ignores(&self) {
        self.ignores.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn ignore_rules(&self, dir: &str) -> Option<std::sync::Arc<ignore::gitignore::Gitignore>> {
        let mut c = self.ignores.lock().unwrap_or_else(|e| e.into_inner());
        c.entry(dir.to_string()).or_insert_with(|| {
            let base = self.root.join(dir);
            let mut b = ignore::gitignore::GitignoreBuilder::new(&base);
            let mut any = base.join(".gitignore").is_file() && b.add(base.join(".gitignore")).is_none();
            if dir.is_empty() && self.root.join(".git/info/exclude").is_file() { any |= b.add(self.root.join(".git/info/exclude")).is_none(); }
            if !any { return None; }
            b.build().ok().map(std::sync::Arc::new)
        }).clone()
    }

    /// Stat the tree, re-parse what changed, drop what disappeared; then make sure the lookup
    /// tables exist, since one-shot commands query through them.
    pub fn update(&self) -> rusqlite::Result<UpdateStats> {
        if self.known()?.is_empty() {
            return self.fresh(|| { let st = self.update_serving()?; self.ensure_lookups()?; Ok(st) });
        }
        // The worktree's server may be writing (a large persist, its background lookup build):
        // then answer from the index as it stands, rather than fail the command.
        let st = match self.update_serving() {
            Err(e) if is_busy(&e) => { eprintln!("mt: the index is busy (the worktree's server is writing it); answering from it as it stands"); return Ok(UpdateStats::default()); }
            r => r?,
        };
        match self.ensure_lookups() {
            Err(e) if is_busy(&e) => {}
            r => { r?; }
        }
        Ok(st)
    }

    /// `update` without building missing lookup tables: for the daemon, which answers from memory
    /// and builds them afterwards on its writer thread, and for `mt build`, which starts it. A first
    /// index of a large repo is then not held up by them (on 5,073 files: 2.5 s of 4.5 s).
    pub fn update_serving(&self) -> rusqlite::Result<UpdateStats> {
        let known = self.known()?;
        let bulk = known.is_empty();
        self.refresh(&known, None, bulk).map(|(st, _, _)| st)
    }

    /// The content hashes the index holds for these paths (the server's reindexer: a file whose
    /// stamp moved but whose bytes did not is only restamped).
    pub fn stored_hashes(&self, paths: &[&str]) -> HashMap<String, i64> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT hash FROM files WHERE path=?1") else { return HashMap::new() };
        paths.iter().filter_map(|p| q.query_row([p], |r| r.get::<_, Option<i64>>(0)).ok().flatten().map(|h| (p.to_string(), h))).collect()
    }

    /// The lookup tables not built yet: after a first index, or a build that was interrupted.
    pub fn missing_lookups(&self) -> rusqlite::Result<Vec<(&'static str, &'static str)>> {
        let db = self.conn();
        let mut q = db.prepare("SELECT name FROM sqlite_master WHERE type='index'")?;
        let have: std::collections::HashSet<String> = q.query_map([], |r| r.get::<_, String>(0))?.flatten().collect();
        Ok(LOOKUPS.iter().filter(|(n, _)| !have.contains(*n)).copied().collect())
    }

    /// Build the missing lookup tables, one at a time (each its own short write), with progress.
    /// Returns how many it built.
    pub fn ensure_lookups(&self) -> rusqlite::Result<usize> {
        let todo = self.missing_lookups()?;
        if todo.is_empty() { return Ok(0); }
        let db = self.conn();
        let mut prog = Progress::row("lookups");
        for (i, (name, on)) in todo.iter().enumerate() {
            prog.during(&format!("{} {}/{} lookup tables ({name})", bar(i, todo.len()), i, todo.len()),
                || db.execute(&format!("CREATE INDEX IF NOT EXISTS {name} ON {on}"), []))?;
        }
        prog.done(|| format!("{} {n}/{n} lookup tables", bar(1, 1), n = todo.len()));
        Ok(todo.len())
    }

    /// `known()` for these paths and everything under any of them that is a directory: what
    /// `refresh(.., Some(paths))` needs, without reading every row.
    pub fn known_for(&self, rels: &[String]) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
        let db = self.conn();
        let mut q = db.prepare_cached("SELECT path, mtime, size FROM files WHERE path=?1 OR (path >= ?2 AND path < ?3)")?;
        let mut m = HashMap::new();
        for r in rels {
            let rows = q.query_map(params![r, format!("{r}/"), format!("{r}0")], |x| Ok((x.get::<_, String>(0)?, x.get::<_, i64>(1)?, x.get::<_, i64>(2)?)))?;
            for (p, a, b) in rows.flatten() { m.insert(p, (a, b)); }
        }
        Ok(m)
    }

    /// path -> (mtime, size) for every indexed file.
    pub fn known(&self) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
        let mut known = HashMap::new();
        let db = self.conn();
        let mut q = db.prepare("SELECT path, mtime, size FROM files")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for r in rows.flatten() {
            known.insert(r.0, (r.1, r.2));
        }
        Ok(known)
    }

    /// Bring the index up to date relative to `known` (what the caller already holds: the
    /// database's own rows, or the daemon's in-memory graph). `only`: just these repo-relative
    /// paths (from a file watcher) instead of walking the tree; a path not seen before falls
    /// back to a full walk, so .gitignore rules decide whether it belongs.
    /// Returns what was re-parsed and what was removed, so a caller can update its own copy.
    /// Find what changed, parse it, and write it to the index (the one-shot path).
    pub fn refresh(&self, known: &HashMap<String, (i64, i64)>, only: Option<&[String]>, bulk: bool) -> rusqlite::Result<(UpdateStats, Vec<Extracted>, Vec<String>)> {
        let stored = |paths: &[&str]| -> HashMap<String, i64> {
            let db = self.conn();
            let Ok(mut q) = db.prepare_cached("SELECT hash FROM files WHERE path=?1") else { return HashMap::new() };
            paths.iter().filter_map(|p| q.query_row([p], |r| r.get::<_, Option<i64>>(0)).ok().flatten().map(|h| (p.to_string(), h))).collect()
        };
        if only.is_none() {
            // the whole tree: parsed and written in batches, so memory stays flat; the parsed files
            // are not returned (nobody holds the whole repo)
            let mut write_ms = 0u128;
            let mut sink = |ex: Vec<Extracted>, base: usize, total: usize| -> rusqlite::Result<()> {
                let t = Instant::now();
                self.write(&ex, &[], bulk, Some((base, total)))?;
                write_ms += t.elapsed().as_millis();
                Ok(())
            };
            let (mut st, _, rm) = self.scan_batched(known, None, &stored, Some(&mut sink))?;
            let t = Instant::now();
            self.persist(&st, &[], &rm, false)?;
            st.write_ms = write_ms + t.elapsed().as_millis();
            return Ok((st, vec![], rm));
        }
        let (mut st, ex, rm) = self.scan(known, only, &stored)?;
        let t = Instant::now();
        self.persist(&st, &ex, &rm, bulk)?;
        st.write_ms = t.elapsed().as_millis();
        Ok((st, ex, rm))
    }

    /// Write what `scan` found: new stamps for unchanged content, the re-parsed files, removals.
    /// The daemon does this on its own thread, after the graph is patched and requests answered.
    pub fn persist(&self, st: &UpdateStats, extracted: &[Extracted], removed: &[String], bulk: bool) -> rusqlite::Result<()> {
        if !st.restamped.is_empty() {
            let mut db = self.conn();
            let tx = db.transaction()?;
            {
                let mut up = tx.prepare("UPDATE files SET mtime=?2, size=?3 WHERE path=?1")?;
                for (p, m, z) in &st.restamped { up.execute(params![p, m, z])?; }
            }
            tx.commit()?;
        }
        if !extracted.is_empty() || !removed.is_empty() {
            self.write(extracted, removed, bulk && !extracted.is_empty(), None)?;
        }
        Ok(())
    }

    /// Find what changed and parse it, on every core; writes nothing. `stored` gives the content
    /// hash we hold for a path (the daemon: its graph, the in-memory truth; one-shot: the index),
    /// so a file whose stamp moved but whose bytes did not is only restamped.
    pub fn scan(&self, known: &HashMap<String, (i64, i64)>, only: Option<&[String]>, stored: &dyn Fn(&[&str]) -> HashMap<String, i64>) -> rusqlite::Result<(UpdateStats, Vec<Extracted>, Vec<String>)> {
        self.scan_batched(known, only, stored, None)
    }

    /// `scan`; with a `sink`, the parsed files go to it `BATCH` at a time (with how many came
    /// before and the total) instead of being returned.
    #[allow(clippy::type_complexity)]
    fn scan_batched(&self, known: &HashMap<String, (i64, i64)>, only: Option<&[String]>, stored: &dyn Fn(&[&str]) -> HashMap<String, i64>,
        mut sink: Option<&mut (dyn FnMut(Vec<Extracted>, usize, usize) -> rusqlite::Result<()> + Send)>) -> rusqlite::Result<(UpdateStats, Vec<Extracted>, Vec<String>)> {
        let mut st = UpdateStats::default();
        let mut prog = Progress::row("parse");
        let mut walk_prog = Progress::row("walk");
        walk_prog.show(|| "walking the tree".into());
        let t0 = Instant::now();
        let mut todo: Vec<(String, i64, i64)> = vec![];
        let mut removed: Vec<String> = vec![];
        let stamp = |md: &fs::Metadata| (md.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0), md.len() as i64);
        if let Some(paths) = only {
            let mut gone: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for rel in paths {
                let abs = self.root.join(rel);
                let dir = format!("{rel}/");
                if abs.is_dir() {
                    // a directory appeared or changed: walk that directory only (as git sees it). `known`
                    // must hold the entries under it (Index::known_for does)
                    let mut seen = std::collections::HashSet::new();
                    let walker = if self.ignored(rel, true) { None } else { Some(walk(&abs)) };
                    for e in walker.into_iter().flatten().flatten() {
                        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) { continue; }
                        let Ok(r) = e.path().strip_prefix(&self.root) else { continue };
                        let r = r.to_string_lossy().replace('\\', "/");
                        let Ok(md) = e.metadata() else { continue };
                        if !indexable(&r) { continue; }
                        let (m, z) = stamp(&md);
                        seen.insert(r.clone());
                        if known.get(&r) != Some(&(m, z)) { todo.push((r, m, z)); }
                    }
                    removed.extend(known.keys().filter(|k| k.starts_with(&dir) && !seen.contains(*k)).cloned());
                    continue;
                }
                // a file git ignores is not the repo's: treated as gone (dropped if it was indexed)
                let md = fs::metadata(&abs).ok().filter(|m| m.is_file() && !self.ignored(rel, false));
                match (md, known.get(rel)) {
                    (Some(md), Some(k)) if indexable(rel) => {
                        let (m, z) = stamp(&md);
                        if *k != (m, z) { todo.push((rel.clone(), m, z)); }
                    }
                    // a new file: just parse it (no walk)
                    (Some(md), None) if indexable(rel) => { let (m, z) = stamp(&md); todo.push((rel.clone(), m, z)); }
                    (_, Some(_)) => removed.push(rel.clone()),
                    // gone, and not a file we knew: maybe a directory that went away (resolved below,
                    // in one pass, so a burst of vanished temp files does not cost a scan each)
                    (None, None) => { gone.insert(rel.as_str()); }
                    _ => {}
                }
                let _ = &dir;
            }
            if !gone.is_empty() {
                for k in known.keys() {
                    let mut p = k.as_str();
                    while let Some((parent, _)) = p.rsplit_once('/') {
                        if gone.contains(parent) { removed.push(k.clone()); break; }
                        p = parent;
                    }
                }
            }
            todo.sort();
            todo.dedup();
            removed.sort();
            removed.dedup();
            st.files = known.len() + todo.iter().filter(|t| !known.contains_key(&t.0)).count() - removed.len();
        } else {
            st.walked = true;
            let mut seen = std::collections::HashSet::new();
            for e in walk(&self.root).flatten() {
                if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let Ok(rel) = e.path().strip_prefix(&self.root) else { continue };
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !indexable(&rel) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let (mtime, size) = stamp(&md);
                seen.insert(rel.clone());
                walk_prog.show(|| format!("{} files found", seen.len()));
                if known.get(&rel) != Some(&(mtime, size)) {
                    todo.push((rel, mtime, size));
                }
            }
            // per-file cards a tool generated and committed (`cards/src/a.md` for `src/a.go`) repeat
            // the code they describe: every search would find each thing twice
            let mirror = mirrors(&seen);
            if !mirror.is_empty() {
                seen.retain(|p| !mirror.contains(p));
                todo.retain(|t| !mirror.contains(&t.0));
            }
            removed = known.keys().filter(|k| !seen.contains(*k)).cloned().collect();
            st.files = seen.len();
        }
        st.walk_ms = t0.elapsed().as_millis();
        if st.walked { walk_prog.done(|| format!("{} files, {} to parse", st.files, todo.len())); }
        st.removed = removed.len();

        // files we already know whose stamp changed: same bytes (touch, a checkout that rewrote the
        // same content, an editor saving an unchanged buffer, a formatter changing nothing) only
        // get their stamp updated; nothing is re-parsed or rewritten
        if todo.iter().any(|t| known.contains_key(&t.0)) {
            let cand: Vec<&str> = todo.iter().filter(|t| known.contains_key(&t.0)).map(|t| t.0.as_str()).collect();
            let stored: HashMap<String, i64> = stored(&cand);
            if !stored.is_empty() {
                let root = self.root.clone();
                let same: std::collections::HashSet<String> = todo.iter().filter(|t| stored.contains_key(&t.0))
                    .filter(|t| fs::read(root.join(&t.0)).map(|b| content_hash(&b) == stored[&t.0]).unwrap_or(false)).map(|t| t.0.clone()).collect();
                if !same.is_empty() {
                    st.restamped = todo.iter().filter(|t| same.contains(&t.0)).cloned().collect();
                    todo.retain(|t| !same.contains(&t.0));
                }
            }
        }

        let t1 = Instant::now();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let root = self.root.clone();
        let done = std::sync::atomic::AtomicUsize::new(0);
        let total = todo.len();
        let batch = if sink.is_some() { BATCH } else { total.max(1) };
        let mut extracted: Vec<Extracted> = vec![];
        let mut parse_ms = 0u128;
        // With a sink (a full index) parsing and writing overlap: a writer thread writes batch k
        // while every core parses batch k+1 (the channel holds one batch, so memory stays ~2 batches).
        let written: rusqlite::Result<()> = std::thread::scope(|sc| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<(Vec<Extracted>, usize)>(1);
            let writer = sink.take().map(|f| sc.spawn(move || -> rusqlite::Result<()> {
                for (ex, base) in rx { f(ex, base, total)?; }
                Ok(())
            }));
        for part in todo.chunks(batch) {
            let tp = Instant::now();
            let chunk = part.len().div_ceil(threads).max(1);
            let ex: Vec<Extracted> = std::thread::scope(|s| {
                let hs: Vec<_> = part.chunks(chunk).map(|c| {
                    let (root, done) = (&root, &done);
                    s.spawn(move || c.iter().map(|(r, m, z)| {
                        let e = extract(root, r, *m, *z);
                        done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        e
                    }).collect::<Vec<_>>())
                }).collect();
                // the workers parse; this thread only reports, while anyone is watching
                while prog.on && total > 0 && !hs.iter().all(|h| h.is_finished()) {
                    let d = done.load(std::sync::atomic::Ordering::Relaxed);
                    prog.show(|| format!("{} {}/{} files · {threads} cores", bar(d, total), d, total));
                    std::thread::sleep(Duration::from_millis(50));
                }
                hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
            });
            parse_ms += tp.elapsed().as_millis();
            st.changed += ex.len();
            if writer.is_some() {
                // the writer stopped (an error): stop parsing; its error is returned below
                if tx.send((ex, st.changed - part.len())).is_err() { break; }
            } else {
                extracted = ex;
            }
        }
            drop(tx);
            if total > 0 { prog.done(|| format!("{} {total}/{total} files · {threads} cores", bar(1, 1))); }
            match writer { Some(h) => h.join().unwrap_or(Ok(())), None => Ok(()) }
        });
        written?;
        let _ = t1;
        st.parse_ms = parse_ms;

        Ok((st, extracted, removed))
    }

    /// `of`: (files written before this call, total) when this is one batch of a larger write.
    fn write(&self, extracted: &[Extracted], removed: &[String], bulk: bool, of: Option<(usize, usize)>) -> rusqlite::Result<()> {
        // Bulk loads are much faster without secondary indexes; rebuild them once at the end.
        let mut db = self.conn();
        let mut prog = Progress::row("write");
        let (base, total) = of.unwrap_or((0, extracted.len()));
        // Bulk loads are much faster without the lookup tables; `ensure_lookups` builds them after
        if bulk {
            for (name, _) in LOOKUPS { db.execute(&format!("DROP INDEX IF EXISTS {name}"), [])?; }
        }
        let tx = db.transaction()?;
        {
            let mut id_of = tx.prepare("SELECT id FROM files WHERE path=?1")?;
            let del = |id: i64, all: bool| -> rusqlite::Result<()> {
                // the file's postings and references are blocks of rows: deleted by their rowid ranges
                let ranges = tx.query_row("SELECT post_lo, post_hi, refs_lo, refs_hi FROM files WHERE id=?1", [id],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?)));
                if let Ok((pl, ph, rl, rh)) = ranges {
                    if let (Some(lo), Some(hi)) = (pl, ph) { tx.execute("DELETE FROM search_post WHERE rowid BETWEEN ?1 AND ?2", [lo, hi])?; }
                    if let (Some(lo), Some(hi)) = (rl, rh) { tx.execute("DELETE FROM refs_ids WHERE rowid BETWEEN ?1 AND ?2", [lo, hi])?; }
                }
                // and the ranges forgotten, so a later block reusing those rowids is never taken for them
                tx.execute("UPDATE files SET post_lo=NULL, post_hi=NULL, refs_lo=NULL, refs_hi=NULL WHERE id=?1", [id])?;
                tx.execute("DELETE FROM search_doc WHERE def IN (SELECT id FROM defs WHERE file=?1)", [id])?;
                tx.execute("DELETE FROM defs WHERE file=?1", [id])?;
                tx.execute("DELETE FROM tables WHERE file=?1", [id])?;
                tx.execute("DELETE FROM lits WHERE file=?1", [id])?;
                if all {
                    tx.execute("DELETE FROM files WHERE id=?1", [id])?;
                }
                Ok(())
            };
            for p in removed {
                if let Ok(id) = id_of.query_row([p], |r| r.get::<_, i64>(0)) {
                    del(id, true)?;
                }
            }
            for e in extracted {
                if let Ok(id) = id_of.query_row([&e.rel], |r| r.get::<_, i64>(0)) {
                    del(id, false)?;
                }
            }
            let mut up = tx.prepare(
                "INSERT INTO files(path, mtime, size, lang, role, lines, hash) VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime, size=excluded.size, lang=excluded.lang, role=excluded.role, lines=excluded.lines, hash=excluded.hash RETURNING id",
            )?;
            let mut ins_def = tx.prepare("INSERT INTO defs(file, kind, name, last, scope, start, end, head, sig) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
            let mut ins_ref = tx.prepare("INSERT INTO refs_ids(file, name_id, lines) VALUES(?1,?2,?3)")?;
            let mut ins_tab = tx.prepare("INSERT INTO tables(file, name, line, kind) VALUES(?1,?2,?3,?4)")?;
            let mut ins_lit = tx.prepare("INSERT INTO lits(file, line, kind, text) VALUES(?1,?2,?3,?4)")?;
            let mut ins_post = tx.prepare("INSERT INTO search_post(term, file, posts) VALUES(?1,?2,?3)")?;
            let mut ins_doc = tx.prepare("INSERT INTO search_doc(def, len) VALUES(?1,?2)")?;
            let mut set_ranges = tx.prepare("UPDATE files SET post_lo=?1, post_hi=?2, refs_lo=?3, refs_hi=?4 WHERE id=?5")?;
            // every spelling of every identifier, once (`spellings` looks names up by their norm)
            // a name's number: the existing one, or a new one (one statement either way)
            let mut ins_name = tx.prepare("INSERT INTO names(name, norm) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET norm=excluded.norm RETURNING id")?;
            let mut named: HashMap<&str, i64> = HashMap::new();
            for (i, e) in extracted.iter().enumerate() {
                prog.show(|| format!("{} {}/{} files", bar(base + i + 1, total), base + i + 1, total));
                let id: i64 = up.query_row(params![e.rel, e.mtime, e.size, e.lang, e.role, e.lines as i64, e.hash], |r| r.get(0))?;
                // each word's postings in this file, packed (def, tf, len), so one row per word and file
                let mut packed: std::collections::BTreeMap<&str, Vec<u8>> = Default::default();
                for (i, (d, sig)) in e.defs.iter().enumerate() {
                    let last = d.name.rsplit_once('.').map(|(_, l)| l).unwrap_or(&d.name);
                    ins_def.execute(params![id, d.kind, d.name, last, d.path.join("."), d.start as i64, d.end as i64, d.head as i64, sig])?;
                    if let Some(Some((terms, len))) = e.search.get(i) {
                        let did = tx.last_insert_rowid();
                        ins_doc.execute(params![did, len])?;
                        for (t, tf) in terms {
                            let b = packed.entry(t.as_str()).or_default();
                            b.extend_from_slice(&did.to_le_bytes());
                            b.extend_from_slice(&tf.to_le_bytes());
                            b.extend_from_slice(&len.to_le_bytes());
                        }
                    }
                }
                let (mut plo, mut phi) = (None::<i64>, 0i64);
                for (t, b) in &packed {
                    ins_post.execute(params![t, id, b])?;
                    let r = tx.last_insert_rowid();
                    plo.get_or_insert(r);
                    phi = r;
                }
                let (mut rlo, mut rhi) = (None::<i64>, 0i64);
                for (n, l) in &e.refs {
                    let nid = match named.get(n.as_str()) {
                        Some(&x) => x,
                        None => { let x: i64 = ins_name.query_row(params![n, norm(n)], |r| r.get(0))?; named.insert(n.as_str(), x); x }
                    };
                    ins_ref.execute(params![id, nid, l.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")])?;
                    let r = tx.last_insert_rowid();
                    rlo.get_or_insert(r);
                    rhi = r;
                }
                if plo.is_some() || rlo.is_some() { set_ranges.execute(params![plo, plo.map(|_| phi), rlo, rlo.map(|_| rhi), id])?; }
                for (n, l, k) in &e.tables {
                    ins_tab.execute(params![id, n, *l as i64, k])?;
                }
                for (l, k, t) in &e.lits {
                    ins_lit.execute(params![id, *l as i64, k, t])?;
                }
            }
        }
        prog.during(&format!("{} {}/{} files · saving", bar(base + extracted.len(), total), base + extracted.len(), total), || tx.commit())?;
        if base + extracted.len() >= total && total > 0 { prog.done(|| format!("{} {total}/{total} files", bar(1, 1))); }
        Ok(())
    }

    fn rows(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Vec<DefRow> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached(sql) else { return vec![] };
        q.query_map(p, |r| {
            Ok(DefRow {
                id: r.get(0)?, path: r.get(1)?, role: r.get(2)?, kind: r.get(3)?, name: r.get(4)?, scope: r.get(5)?,
                start: r.get::<_, i64>(6)? as usize, end: r.get::<_, i64>(7)? as usize, head: r.get::<_, i64>(8)? as usize, sig: r.get(9)?,
            })
        })
        .map(|it| counted(it.flatten().collect()))
        .unwrap_or_default()
    }

    const DEF_COLS: &'static str = "d.id, f.path, f.role, d.kind, d.name, d.scope, d.start, d.end, d.head, d.sig FROM defs d JOIN files f ON f.id=d.file";

    pub fn summary(&self) -> String {
        let n = |sql: &str| self.conn().query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        let mut s = format!(
            "{} files, {} definitions, {} identifier rows, {} table facts\n",
            n("SELECT COUNT(*) FROM files"), n("SELECT COUNT(*) FROM defs"), n("SELECT COUNT(*) FROM refs_ids"), n("SELECT COUNT(*) FROM tables")
        );
        let db = self.conn();
        if let Ok(mut q) = db.prepare("SELECT role, COUNT(*) FROM files GROUP BY role ORDER BY 2 DESC") {
            let rows: Vec<String> = q.query_map([], |r| Ok(format!("{}={}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map(|it| counted(it.flatten().collect())).unwrap_or_default();
            s.push_str(&format!("roles: {}\n", rows.join(" ")));
        }
        if let Ok(mut q) = db.prepare("SELECT lang, COUNT(*) FROM files GROUP BY lang ORDER BY 2 DESC LIMIT 14") {
            let rows: Vec<String> = q.query_map([], |r| Ok(format!("{}={}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map(|it| counted(it.flatten().collect())).unwrap_or_default();
            s.push_str(&format!("tiers: {}\n", rows.join(" ")));
        }
        s
    }
}


/// The lookups every query needs. Two homes: the SQLite index (one-shot CLI calls) and the
/// daemon's in-memory graph (`graph.rs`), which answers the same questions from hash maps.
pub trait Store: Sync {
    fn root(&self) -> &Path;
    fn file_count(&self) -> usize;
    fn defs_named(&self, last: &str) -> Vec<DefRow>;
    fn defs_in(&self, path: &str) -> Vec<DefRow>;
    /// Number of distinct files defining `last`.
    fn def_count(&self, last: &str) -> usize;
    /// (path, role, 1-based lines) for every file mentioning `name`.
    fn refs(&self, name: &str) -> Vec<(String, String, Vec<usize>)>;
    /// Tables defined anywhere in the repo (lowercased name -> define sites).
    fn defined_tables(&self) -> HashMap<String, Vec<(String, usize)>>;
    fn files_with_roles(&self, roles: &[&str]) -> Vec<(String, String)>;
    fn table_uses(&self, table: &str) -> Vec<(String, String, usize)>;
    /// (path, role) of every file holding code or docs (not binary, not skipped).
    fn text_files(&self) -> Vec<(String, String)>;
    /// Every identifier in the repo that spells the same name: `userID`, `user_id`, `UserId`, `USER_ID`.
    fn spellings(&self, name: &str) -> Vec<String>;
    /// Plain-word search: (definition id, weighted count, document length) of every definition
    /// whose words include `term`.
    fn postings(&self, term: &str) -> Vec<(i64, f32, f32)>;
    /// Plain-word search: how many definitions are documents, and their average length.
    fn search_stats(&self) -> (f64, f64);
    /// These definitions, by id.
    fn defs_by_ids(&self, ids: &[i64]) -> Vec<DefRow>;
    /// The routes registered on these 1-based lines of `path` (as `context` reads them), in the
    /// order of `lines`, then along each line.
    fn routes_at(&self, path: &str, lines: &[usize]) -> Vec<String>;
    /// (path, 1-based line) of every line, in files of these roles, holding a quoted path that
    /// starts with `prefix`.
    fn quoted_paths(&self, prefix: &str, roles: &[&str]) -> Vec<(String, usize)>;
}

/// The key two spellings of one name share: lowercase, underscores and dashes dropped.
pub fn norm(name: &str) -> String {
    name.chars().filter(|c| *c != '_' && *c != '-').flat_map(|c| c.to_lowercase()).collect()
}

impl Store for Index {
    fn root(&self) -> &Path {
        &self.root
    }

    fn file_count(&self) -> usize {
        self.conn().query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    fn spellings(&self, name: &str) -> Vec<String> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT name FROM names WHERE norm=?1") else { return vec![] };
        let mut v: Vec<String> = q.query_map([norm(name)], |r| r.get::<_, String>(0)).map(|it| counted(it.flatten().collect())).unwrap_or_default();
        v.sort();
        v
    }

    fn postings(&self, term: &str) -> Vec<(i64, f32, f32)> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT posts FROM search_post WHERE term=?1") else { return vec![] };
        let blobs: Vec<Vec<u8>> = q.query_map([term], |r| r.get::<_, Vec<u8>>(0)).map(|it| counted(it.flatten().collect())).unwrap_or_default();
        // (def i64, tf f32, len f32), little-endian, 16 bytes each
        blobs.iter().flat_map(|b| b.chunks_exact(16).map(|c| (
            i64::from_le_bytes(c[0..8].try_into().unwrap_or_default()),
            f32::from_le_bytes(c[8..12].try_into().unwrap_or_default()),
            f32::from_le_bytes(c[12..16].try_into().unwrap_or_default()),
        )).collect::<Vec<_>>()).collect()
    }

    fn search_stats(&self) -> (f64, f64) {
        self.conn().query_row("SELECT count(*), coalesce(avg(len), 0) FROM search_doc", [], |r| Ok((r.get::<_, i64>(0)? as f64, r.get::<_, f64>(1)?))).unwrap_or((0.0, 0.0))
    }

    fn routes_at(&self, path: &str, lines: &[usize]) -> Vec<String> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT l.line, l.text FROM lits l JOIN files f ON f.id=l.file WHERE f.path=?1 AND l.kind='route' ORDER BY l.line, l.rowid") else { return vec![] };
        let all: Vec<(usize, String)> = q.query_map([path], |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, String>(1)?))).map(|it| counted(it.flatten().collect())).unwrap_or_default();
        lines.iter().flat_map(|l| all.iter().filter(move |(x, _)| x == l).map(|(_, t)| t.clone())).collect()
    }

    fn quoted_paths(&self, prefix: &str, roles: &[&str]) -> Vec<(String, usize)> {
        let q = format!("SELECT DISTINCT f.path, l.line FROM lits l JOIN files f ON f.id=l.file WHERE l.kind='path' AND l.text >= ?1 AND l.text < ?2 AND f.role IN ({})",
            roles.iter().map(|r| format!("'{r}'")).collect::<Vec<_>>().join(","));
        let db = self.conn();
        let Ok(mut st) = db.prepare_cached(&q) else { return vec![] };
        let hi = format!("{prefix}\u{10FFFF}");
        st.query_map(params![prefix, hi], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))).map(|it| counted(it.flatten().collect())).unwrap_or_default()
    }

    fn defs_by_ids(&self, ids: &[i64]) -> Vec<DefRow> {
        // integers only, so they go in the statement itself, in chunks
        ids.chunks(500).flat_map(|c| {
            let list = c.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            self.rows(&format!("SELECT {} WHERE d.id IN ({list})", Self::DEF_COLS), &[])
        }).collect()
    }

    fn text_files(&self) -> Vec<(String, String)> {
        let db = self.conn();
        let Ok(mut st) = db.prepare("SELECT path, role FROM files WHERE lang NOT IN ('binary','skipped') AND lang NOT LIKE '%too large%'") else { return vec![] };
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map(|it| counted(it.flatten().collect())).unwrap_or_default()
    }

    fn defs_named(&self, last: &str) -> Vec<DefRow> {
        self.rows(&format!("SELECT {} WHERE d.last=?1", Self::DEF_COLS), &[&last])
    }

    fn defs_in(&self, path: &str) -> Vec<DefRow> {
        self.rows(&format!("SELECT {} WHERE f.path=?1 ORDER BY d.start", Self::DEF_COLS), &[&path])
    }

    fn def_count(&self, last: &str) -> usize {
        self.conn().query_row("SELECT COUNT(DISTINCT file) FROM defs WHERE last=?1", [last], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    fn refs(&self, name: &str) -> Vec<(String, String, Vec<usize>)> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT f.path, f.role, r.lines FROM refs_ids r JOIN files f ON f.id=r.file WHERE r.name_id=(SELECT id FROM names WHERE name=?1)") else { return vec![] };
        q.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .map(|it| counted(it.flatten().map(|(p, ro, l)| (p, ro, l.split(',').filter_map(|x| x.parse().ok()).collect())).collect()))
            .unwrap_or_default()
    }

    fn defined_tables(&self) -> HashMap<String, Vec<(String, usize)>> {
        let mut out: HashMap<String, Vec<(String, usize)>> = HashMap::new();
        let db = self.conn();
        if let Ok(mut q) = db.prepare_cached("SELECT t.name, f.path, t.line FROM tables t JOIN files f ON f.id=t.file WHERE t.kind='define' AND f.role IN ('migration','model','query','source') AND f.lang NOT IN ('markdown','text')") {
            if let Ok(rows) = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))) {
                for (n, p, l) in rows.flatten() {
                    out.entry(n.to_lowercase()).or_default().push((p, l as usize));
                }
            }
        }
        out
    }

    fn files_with_roles(&self, roles: &[&str]) -> Vec<(String, String)> {
        let q = format!("SELECT path, role FROM files WHERE role IN ({})", roles.iter().map(|r| format!("'{r}'")).collect::<Vec<_>>().join(","));
        let db = self.conn();
        let Ok(mut st) = db.prepare(&q) else { return vec![] };
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map(|it| counted(it.flatten().collect())).unwrap_or_default()
    }

    fn table_uses(&self, table: &str) -> Vec<(String, String, usize)> {
        let db = self.conn();
        let Ok(mut q) = db.prepare_cached("SELECT f.path, f.role, t.line FROM tables t JOIN files f ON f.id=t.file WHERE t.kind='use' AND t.name=?1 COLLATE NOCASE") else { return vec![] };
        q.query_map([table], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as usize))).map(|it| counted(it.flatten().collect())).unwrap_or_default()
    }

}

/// `go test` and friends print paths relative to the package: find the repo file ending in `suffix`.
pub fn find_by_suffix(root: &Path, suffix: &str) -> Option<PathBuf> {
    let name = suffix.rsplit('/').next()?;
    for e in walk(root).flatten() {
        if e.file_name().to_str() == Some(name) {
            let p = e.path().to_string_lossy();
            if p.ends_with(suffix) { return Some(e.path().to_path_buf()); }
        }
    }
    None
}

/// The definition of a test function `name`, searched in test files only (fresh from disk).
pub fn find_def_in_tests(root: &Path, name: &str) -> Option<(String, String, Def)> {
    let rx = Regex::new(&format!(r"\b{}\b", regex::escape(name))).ok()?;
    for e in walk(root).flatten() {
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().into_owned();
        if !matches!(role_of(&rel), "test" | "integration") { continue; }
        let Ok(text) = fs::read_to_string(e.path()) else { continue };
        if !rx.is_match(&text) { continue; }
        let tier = detect(Path::new(&rel), &text);
        if let Some(d) = definitions_with(&tier, &text).into_iter().find(|d| d.name == name || d.name.ends_with(&format!(".{name}"))) {
            return Some((rel, text, d));
        }
    }
    None
}

#[cfg(test)]
mod reindex_tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Bump a file's mtime without touching its bytes.
    fn touch(root: &Path, rel: &str) {
        let f = fs::File::options().write(true).open(root.join(rel)).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)).unwrap();
    }

    /// Refresh exactly these paths, the way the daemon and `mt apply` do.
    fn only(idx: &mut Index, rels: &[&str]) -> (UpdateStats, Vec<Extracted>, Vec<String>) {
        let rels: Vec<String> = rels.iter().map(|s| s.to_string()).collect();
        let known = idx.known_for(&rels).unwrap();
        idx.refresh(&known, Some(&rels), false).unwrap()
    }

    #[test]
    fn git_decides_what_is_indexed() {
        let root = std::env::temp_dir().join(format!("mt-gitignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, ".gitignore", "build/\n*.tmp\n!keep.tmp\n/secret.txt\n");
        write(&root, "sub/.gitignore", "local.go\n!build/\n");
        write(&root, ".git/info/exclude", "scratch/\n");
        for f in ["a.go", "build/out.go", "x.tmp", "keep.tmp", "secret.txt", "sub/secret.txt", "sub/local.go", "sub/build/b.go",
                  "node_modules/m/i.js", "bin/tool.sh", ".github/workflows/ci.yml", "scratch/s.go", "logo.png"] {
            write(&root, f, "package a\n");
        }
        let idx = Index::open(&root).unwrap();
        idx.update().unwrap();
        let mut got: Vec<String> = idx.known().unwrap().into_keys().collect();
        got.sort();
        // everything git keeps, hidden files and folders like node_modules/ and bin/ included; a
        // binary is listed (not read); nothing of .git or .mt
        assert_eq!(got, [".github/workflows/ci.yml", ".gitignore", "a.go", "bin/tool.sh", "keep.tmp", "logo.png", "node_modules/m/i.js",
                         "sub/.gitignore", "sub/build/b.go", "sub/secret.txt"], "{got:?}");
        // the watcher's single paths get the same answers
        for (p, dir, want) in [("build/out.go", false, true), ("build", true, true), ("x.tmp", false, true), ("keep.tmp", false, false),
                               ("secret.txt", false, true), ("sub/secret.txt", false, false), ("sub/local.go", false, true),
                               ("sub/build/b.go", false, false), ("scratch/s.go", false, true), ("node_modules/m/i.js", false, false)] {
            assert_eq!(idx.ignored(p, dir), want, "{p}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reindex_touches_only_what_changed() {
        let root = std::env::temp_dir().join(format!("mt-reindex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, "a.go", "package a\n\nfunc Alpha() int {\n\treturn 1\n}\n");
        write(&root, "sub/b.go", "package sub\n\nfunc Beta() int {\n\treturn 2\n}\n");
        let mut idx = Index::open(&root).unwrap();
        assert_eq!(idx.update_serving().unwrap().changed, 2, "first index parses both files");
        assert_eq!(idx.missing_lookups().unwrap().len(), LOOKUPS.len(), "a first index for the daemon leaves the lookup tables to it");
        assert_eq!(idx.update().unwrap().changed, 0, "nothing changed: nothing re-parsed");
        assert!(idx.missing_lookups().unwrap().is_empty(), "update builds what is missing");
        assert_eq!(idx.ensure_lookups().unwrap(), 0, "and once built, nothing to do");

        // same bytes, new mtime: restamped, not re-parsed, and it stays that way
        touch(&root, "a.go");
        let (st, ex, rm) = only(&mut idx, &["a.go"]);
        assert!(ex.is_empty() && rm.is_empty() && !st.walked, "touch re-parsed {:?}", ex.iter().map(|e| &e.rel).collect::<Vec<_>>());
        assert_eq!(st.restamped.len(), 1);
        assert_eq!(idx.update().unwrap().changed, 0, "after a restamp the stored stamp matches the disk");

        // a real edit: that file, once
        write(&root, "a.go", "package a\n\nfunc Alpha() int {\n\treturn 3\n}\n");
        let (st, ex, _) = only(&mut idx, &["a.go"]);
        assert_eq!((ex.len(), st.restamped.len(), st.walked), (1, 0, false));

        // a new file: parsed without walking the tree
        write(&root, "c.go", "package a\n\nfunc Gamma() {}\n");
        let (st, ex, _) = only(&mut idx, &["c.go"]);
        assert_eq!((ex.len(), st.walked), (1, false));

        // a new directory: only that directory is walked
        write(&root, "d/e.go", "package d\n\nfunc Delta() {}\n");
        write(&root, "d/f.go", "package d\n\nfunc Epsilon() {}\n");
        let (st, ex, _) = only(&mut idx, &["d"]);
        assert_eq!((ex.len(), st.walked), (2, false));

        // the directory goes away: its files are removed, nothing else is looked at
        fs::remove_dir_all(root.join("d")).unwrap();
        let (st, ex, rm) = only(&mut idx, &["d"]);
        assert_eq!((ex.len(), rm.len(), st.walked), (0, 2, false));

        // the watcher skips mt's own files and VCS internals; anything else is git's call (`ignored`)
        assert!(!watch_relevant(".mt/index.sqlite-wal") && !watch_relevant(".git/index"));
        assert!(watch_relevant("node_modules/x/y.js") && watch_relevant(".github/workflows/ci.yml"));
        // generated per-file cards mirror the tree: dropped; a docs folder of its own is not
        let mut ps: std::collections::HashSet<String> = (0..30).flat_map(|i| [format!("internal/m{i}.go"), format!("cards/internal/m{i}.md")]).collect();
        ps.extend((0..30).map(|i| format!("docs/guide-{i}.md")));
        ps.insert("cards/INDEX.md".into());
        let m = mirrors(&ps);
        assert_eq!(m.len(), 30);
        assert!(m.contains("cards/internal/m3.md") && !m.contains("cards/INDEX.md") && !m.iter().any(|p| p.starts_with("docs/")));
        assert!(!watch_relevant("pkg/mod.go.mt-tmp-123") && watch_relevant("pkg/mod.go") && watch_relevant("pkg"));

        // a full refresh afterwards finds nothing left to do
        assert_eq!(idx.update().unwrap().changed, 0);
        let _ = fs::remove_dir_all(&root);
    }
}
