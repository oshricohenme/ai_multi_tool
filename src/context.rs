//! `mt context <query>`: everything related to a symbol in one shot, filled to a token budget.
//!
//! Seeds are definitions named by the query. From each seed the related set is:
//!   callers      every reference outside the seed, grouped by enclosing definition, with the
//!                exact call-site lines (so a call site can be edited from a pointer)
//!   types        type definitions named in the seed's signature
//!   callees      functions the seed's body names, when the name is unambiguous
//!   tests        unit tests referencing the seed; integration tests referencing it or its callers
//!   data         tables the seed touches: the CREATE TABLE / model that defines them, queries using them
//!   docs         markdown sections that mention it
//! Matching is by name. Anything ambiguous is labelled with how many places define the name.
//! Every item says why it is included. Bodies are inlined in priority order until the budget
//! is spent; the rest are one-line pointers with a hash, ready for `mt span` or `mt apply`.

use crate::fnv1a;
use crate::index::{DefRow, Store, UpdateStats};
use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Sec {
    Target,
    Callers,
    Types,
    Callees,
    Tests,
    Integration,
    Data,
    Docs,
    Other,
}

impl Sec {
    fn title(self) -> &'static str {
        match self {
            Sec::Target => "target",
            Sec::Callers => "callers (break if the signature changes)",
            Sec::Types => "types in the signature",
            Sec::Callees => "called by the target",
            Sec::Tests => "unit tests",
            Sec::Integration => "integration tests",
            Sec::Data => "data layer",
            Sec::Docs => "docs",
            Sec::Other => "other definitions with this name",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Sec::Target => "target",
            Sec::Callers => "callers",
            Sec::Types => "types",
            Sec::Callees => "callees",
            Sec::Tests => "tests",
            Sec::Integration => "integration",
            Sec::Data => "data",
            Sec::Docs => "docs",
            Sec::Other => "other",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub sec: Sec,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub kind: String,
    pub name: String,
    pub why: Vec<String>,
    pub score: i32,
    pub can_inline: bool,
    pub inline: bool,
    pub calls: Vec<usize>,
}

pub struct Opts {
    pub budget: usize,
    pub json: bool,
    pub max_pointers: usize,
    /// Edit mode: bodies only for the target and small types/table definitions; every other
    /// item is a pointer showing each referencing line with two lines either side.
    pub edit: bool,
    /// the client's session (pointer numbers for `mt expand`); None: this process's own
    pub session: Option<String>,
    /// map view: no bodies, one line per item with its relations; `mt expand` reads them
    pub map: bool,
}

pub(crate) const TYPE_KINDS: &[&str] = &["class", "struct", "interface", "type", "enum", "record", "trait", "protocol", "message", "object", "model", "~class", "~struct", "~interface", "~type", "~model", "~enum", "table"];
const VALUE_KINDS: &[&str] = &["constant", "const", "var", "static", "val", "~const"];
pub(crate) const FUNC_KINDS: &[&str] = &["function", "method", "func", "ctor", "macro", "~function", "~func", "~fn", "~def", "~sub", "~method", "~proc"];
const CONTAINER_KINDS: &[&str] = &["module", "namespace", "package", "impl", "class", "struct", "object", "interface", "trait"];

pub(crate) fn ident_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap())
}

pub(crate) struct Files {
    pub cache: HashMap<String, Vec<String>>,
}

impl Files {
    pub fn load_all(root: &std::path::Path, paths: &[String]) -> Files {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let chunk = paths.len().div_ceil(threads).max(1);
        let cache = std::thread::scope(|s| {
            let hs: Vec<_> = paths
                .chunks(chunk)
                .map(|c| s.spawn(move || c.iter().map(|p| (p.clone(), read_lines(root, p))).collect::<Vec<_>>()))
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        Files { cache }
    }
    pub fn get(&mut self, root: &std::path::Path, p: &str) -> &Vec<String> {
        self.cache.entry(p.to_string()).or_insert_with(|| read_lines(root, p))
    }
}

fn read_lines(root: &std::path::Path, p: &str) -> Vec<String> {
    crate::texts::get(root, p).map(|t| t.lines().map(|l| l.to_string()).collect()).unwrap_or_default()
}

pub(crate) fn span_hash(lines: &[String], s: usize, e: usize) -> String {
    if lines.is_empty() || s >= lines.len() {
        return "00000000".into();
    }
    let e = e.min(lines.len() - 1);
    format!("{:08x}", fnv1a(lines[s..=e].join("\n").as_bytes()) as u32)
}

pub(crate) fn enclosing<'a>(defs: &'a [DefRow], line0: usize) -> Option<&'a DefRow> {
    // Prefer the smallest enclosing definition that is not a bare container (a whole class/module)
    // unless nothing smaller exists.
    let inside: Vec<&DefRow> = defs.iter().filter(|d| d.start <= line0 && line0 <= d.end).collect();
    inside.iter().filter(|d| !CONTAINER_KINDS.contains(&d.kind.as_str()) || d.end - d.start < 40).min_by_key(|d| d.end - d.start).copied()
        .or_else(|| inside.iter().min_by_key(|d| d.end - d.start).copied())
}

fn route_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    // "/v1/org", "POST /v1/org/{id}", '/api/users/:id', `/v2/x`
    RX.get_or_init(|| Regex::new(r#"["'`](?:(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+)?(/[A-Za-z0-9_\-./{}:]+)["'`]"#).unwrap())
}

pub(crate) fn call_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*(?:<[^<>()]*>)?\s*\(").unwrap())
}

/// Lines s..=e with comments removed: whole-line comments dropped, trailing `//` and ` #` cut.
/// String literals stay, because SQL inside strings is how code names its tables.
pub(crate) fn code_only(lines: &[String], s: usize, e: usize) -> Vec<String> {
    let mut out = vec![];
    let mut in_block = false;
    for l in lines.iter().skip(s).take(e.saturating_sub(s) + 1) {
        let t = l.trim_start();
        if in_block {
            if let Some(i) = t.find("*/") { in_block = false; out.push(t[i + 2..].to_string()); }
            continue;
        }
        if t.starts_with("/*") && !t.contains("*/") { in_block = true; continue; }
        if t.starts_with("//") || t.starts_with('#') || t.starts_with("--") || t.starts_with('*') || t.starts_with("/*") || t.starts_with(';') || t.starts_with("\"\"\"") {
            continue;
        }
        let mut code = l.as_str();
        if let Some(i) = code.find(" //").or_else(|| code.find("\t//")) { code = &code[..i]; }
        if let Some(i) = code.find(" # ") { code = &code[..i]; }
        out.push(code.to_string());
    }
    out
}

/// Is a definition in `def_path` plausibly what the target means? Same directory, or the
/// target file names its directory or file stem (import path, package qualifier, module name).
pub(crate) fn reachable(target_path: &str, def_path: &str, file_words: &HashSet<String>) -> bool {
    let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    if dir(target_path) == dir(def_path) {
        return true;
    }
    let d = dir(def_path);
    let last_dir = d.rsplit('/').next().unwrap_or("").to_lowercase();
    let stem = def_path.rsplit('/').next().unwrap_or("").split('.').next().unwrap_or("").to_lowercase();
    let norm = |s: &str| s.replace(['-', '.'], "_");
    (!last_dir.is_empty() && (file_words.contains(&last_dir) || file_words.contains(&norm(&last_dir))))
        || (stem.len() > 2 && stem != "index" && stem != "mod" && stem != "main" && (file_words.contains(&stem) || file_words.contains(&norm(&stem))))
}

pub(crate) fn last_seg(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, l)| l).unwrap_or(name)
}

pub(crate) struct Gathered {
    pub merged: Vec<Item>,
    pub files: Files,
    pub n_targets: usize,
    pub ref_lines: usize,
    pub ref_files: usize,
    pub last: String,
}

/// Every item related to `query`, deduplicated by span, not yet budgeted or rendered.
pub(crate) fn gather<S: Store>(idx: &S, query: &str) -> Result<Gathered, String> {
    let root = idx.root().to_path_buf();
    let q = query.trim();
    let (scope, last) = match q.rsplit_once('.') {
        Some((p, n)) if !p.is_empty() && !n.is_empty() && !q.contains('/') => (Some(p), n),
        _ => (None, q),
    };
    let mut seeds: Vec<DefRow> = idx.defs_named(last);
    if let Some(p) = scope {
        let word = Regex::new(&format!(r"(^|[^A-Za-z0-9_]){}([^A-Za-z0-9_]|$)", regex::escape(p))).unwrap();
        let scoped: Vec<DefRow> = seeds.iter().filter(|d| d.scope == p || d.scope.ends_with(&format!(".{p}")) || d.name.starts_with(&format!("{p}.")) || (d.scope.is_empty() && !d.name.contains('.') && word.is_match(&d.sig))).cloned().collect();
        if !scoped.is_empty() {
            seeds = scoped;
        }
    }
    let role_rank = |r: &str| match r { "source" => 0, "model" => 1, "query" => 2, "migration" => 3, "config" => 4, "test" => 6, "integration" => 7, "doc" => 8, _ => 5 };
    seeds.sort_by_key(|d| (role_rank(&d.role), d.kind.starts_with('h') as u8, std::cmp::Reverse(d.end - d.start)));
    // Prefer real code: when the name is defined outside tests, test fakes are not targets.
    if seeds.iter().any(|d| d.role != "test" && d.role != "integration") {
        seeds.retain(|d| d.role != "test" && d.role != "integration");
    }
    if seeds.is_empty() {
        return Err(format!("no definition named {q:?} in the index ({} files). try a different name, or `mt span <file> /regex/`", idx.file_count()));
    }

    let all_paths: Vec<String> = {
        let mut v: Vec<String> = seeds.iter().map(|d| d.path.clone()).collect();
        for (p, _, _) in idx.refs(last) {
            v.push(p);
        }
        v.sort();
        v.dedup();
        v
    };
    let mut files = Files::load_all(&root, &all_paths);
    let mut defs_cache: HashMap<String, Vec<DefRow>> = HashMap::new();
    let mut defs_in = |idx: &S, p: &str| -> Vec<DefRow> { defs_cache.entry(p.to_string()).or_insert_with(|| idx.defs_in(p)).clone() };

    let mut items: Vec<Item> = vec![];
    let mut ref_lines = 0usize;
    let mut ref_files: HashSet<String> = HashSet::new();
    let targets: Vec<DefRow> = seeds.iter().take(3).cloned().collect();
    for d in seeds.iter().skip(3) {
        items.push(Item { sec: Sec::Other, path: d.path.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![format!("also defines {last}")], score: 5, can_inline: false, inline: false, calls: vec![] });
    }
    let defined_tables = idx.defined_tables();
    let ambiguous = idx.def_count(last);

    for (ti, t) in targets.iter().enumerate() {
        let tid = format!("[{}]", ti + 1);
        items.push(Item { sec: Sec::Target, path: t.path.clone(), start: t.start, end: t.end, kind: t.kind.clone(), name: t.name.clone(), why: vec![], score: 1000, can_inline: true, inline: true, calls: vec![] });

        // ---- callers, tests, integration, docs: every reference to the name outside this target
        let mut groups: HashMap<(String, usize, usize), (DefRow, Vec<usize>, String)> = HashMap::new();
        for (path, role, lines) in idx.refs(last) {
            let fdefs = defs_in(idx, &path);
            for l1 in lines {
                let l0 = l1 - 1;
                if path == t.path && t.start <= l0 && l0 <= t.end {
                    continue;
                }
                if fdefs.iter().any(|d| d.head == l0 && last_seg(&d.name) == last) {
                    continue; // another definition of the same name, not a use
                }
                let enc = enclosing(&fdefs, l0).cloned().unwrap_or(DefRow {
                    id: -1, path: path.clone(), role: role.clone(), kind: "lines".into(), name: format!("line {l1}"), scope: String::new(),
                    start: l0.saturating_sub(3), end: l0 + 3, head: l0, sig: String::new(),
                });
                groups.entry((path.clone(), enc.start, enc.end)).or_insert_with(|| (enc.clone(), vec![], role.clone())).1.push(l0);
                ref_lines += 1;
                ref_files.insert(path.clone());
            }
        }
        let mut caller_names: Vec<(String, String)> = vec![];
        for ((path, s, e), (d, calls, role)) in groups {
            let n = e - s + 1;
            let (sec, score, inl) = match role.as_str() {
                "test" => (Sec::Tests, 70, n <= 120),
                "integration" => (Sec::Integration, 65, n <= 160),
                "doc" => (Sec::Docs, 20, false),
                _ => (Sec::Callers, 60, n <= 80),
            };
            if sec == Sec::Callers && d.id >= 0 {
                caller_names.push((last_seg(&d.name).to_string(), path.clone()));
            }
            let mut why = format!("references {tid} at L{}", calls.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join(","));
            if ambiguous > 1 {
                why.push_str(&format!(" (name defined in {ambiguous} files: possible)"));
            }
            items.push(Item { sec, path, start: s, end: e, kind: d.kind.clone(), name: d.name.clone(), why: vec![why], score: score - (n as i32 / 40), can_inline: inl, inline: false, calls });
        }

        // ---- integration tests reaching the target through a caller (two hops)
        caller_names.sort();
        caller_names.dedup();
        let mut route_tests = 0usize;
        {
            // routes: string literals that look like URL paths, on the lines where a caller is registered
            let mut routes: Vec<(String, String)> = vec![]; // (route prefix, handler name)
            let mut names: Vec<String> = caller_names.iter().map(|(n, _)| n.clone()).collect();
            if FUNC_KINDS.contains(&t.kind.as_str()) { names.push(last.to_string()); }
            for cname in names.iter().take(40) {
                for (path, role, lines) in idx.refs(cname) {
                    if role == "test" || role == "integration" || role == "doc" { continue; }
                    let fl = files.get(&root, &path).clone();
                    for l1 in lines.iter().take(20) {
                        let Some(text) = fl.get(l1 - 1) else { continue };
                        for c in route_rx().captures_iter(text) {
                            let r = c[1].split('{').next().unwrap_or("").split(':').next().unwrap_or("").trim_end_matches('/').to_string();
                            if r.len() >= 5 && !routes.iter().any(|(x, _)| *x == r) {
                                routes.push((r, cname.clone()));
                            }
                        }
                    }
                }
            }
            if !routes.is_empty() {
                let tests = idx.files_with_roles(&["test", "integration"]);
                let routes: Vec<&(String, String)> = routes.iter().take(30).collect();
                // each route quoted the three ways a test writes a URL
                let needles: Vec<[String; 3]> = routes.iter().map(|(r, _)| [format!("\"{r}"), format!("`{r}"), format!("'{r}")]).collect();
                // one pass over the test files, on every core: only a file that names a route anywhere
                // is split into lines (a large repo has thousands of test files and few name a route)
                let matching: HashMap<String, Vec<String>> = {
                    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                    let chunk = tests.len().div_ceil(threads).max(1);
                    let (root, needles) = (&root, &needles);
                    std::thread::scope(|s| {
                        let hs: Vec<_> = tests.chunks(chunk).map(|c| s.spawn(move || c.iter().filter_map(|(tp, _)| {
                            let t = crate::texts::get(root, tp)?;
                            needles.iter().any(|ns| ns.iter().any(|n| t.contains(n.as_str()))).then(|| (tp.clone(), t.lines().map(String::from).collect()))
                        }).collect::<Vec<_>>())).collect();
                        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
                    })
                };
                for ((route, handler), ns) in routes.iter().zip(&needles) {
                    for (tp, role) in &tests {
                        let Some(tl) = matching.get(tp) else { continue };
                        let hits: Vec<usize> = tl.iter().enumerate().filter(|(_, l)| ns.iter().any(|n| l.contains(n.as_str()))).map(|(i, _)| i).take(3).collect();
                        if hits.is_empty() { continue; }
                        let fdefs = defs_in(idx, tp);
                        for h in hits {
                            if let Some(d) = enclosing(&fdefs, h) {
                                let (sec, score) = if role == "integration" { (Sec::Integration, 45) } else { (Sec::Tests, 47) };
                                let via = if handler == last { format!("calls route {route}, served by {tid}") } else { format!("calls route {route}, served by {handler}, which calls {tid}") };
                                items.push(Item { sec, path: tp.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![via], score, can_inline: d.end - d.start < 120, inline: false, calls: vec![h] });
                                route_tests += 1;
                            }
                        }
                    }
                }
            }
        }
        let _ = route_tests;
        for (cname, _) in caller_names.iter().take(8) {
            if cname.len() < 4 || idx.def_count(cname) > 2 {
                continue;
            }
            for (path, role, lines) in idx.refs(cname) {
                if role != "integration" && role != "test" {
                    continue;
                }
                let (sec, score) = if role == "integration" { (Sec::Integration, 30) } else { (Sec::Tests, 28) };
                let fdefs = defs_in(idx, &path);
                for l1 in lines.iter().take(3) {
                    if let Some(d) = enclosing(&fdefs, l1 - 1) {
                        items.push(Item { sec, path: path.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![format!("calls {cname}, which calls {tid}")], score, can_inline: d.end - d.start < 120, inline: false, calls: vec![l1 - 1] });
                    }
                }
            }
        }

        let tl = files.get(&root, &t.path).clone();
        let code: Vec<String> = code_only(&tl, t.start, t.end);
        let head_end = (t.head..=t.end).find(|&i| tl.get(i).map(|l| l.contains('{') || l.trim_end().ends_with(':') || l.contains("=>")).unwrap_or(true)).unwrap_or(t.head).min(t.head + 6);
        let sig_code = code_only(&tl, t.head, head_end);
        let sig_ids: HashSet<String> = sig_code.iter().flat_map(|l| ident_rx().find_iter(l).map(|m| m.as_str().to_string()).collect::<Vec<_>>()).collect();
        let body_ids: HashSet<String> = code.iter().flat_map(|l| ident_rx().find_iter(l).map(|m| m.as_str().to_string()).collect::<Vec<_>>()).collect();
        let call_ids: HashSet<String> = code.iter().flat_map(|l| call_rx().captures_iter(l).map(|c| c[1].to_string()).collect::<Vec<_>>()).collect();
        // Words the whole target file uses: import paths and package qualifiers live here.
        let file_words: HashSet<String> = tl.iter().flat_map(|l| ident_rx().find_iter(l).map(|m| m.as_str().to_lowercase()).collect::<Vec<_>>()).collect();
        let local = |d: &DefRow| reachable(&t.path, &d.path, &file_words);

        // ---- types named in the signature
        for id in &sig_ids {
            if id == last || idx.def_count(id) > 6 {
                continue;
            }
            for d in idx.defs_named(id) {
                if TYPE_KINDS.contains(&d.kind.as_str()) && !(d.path == t.path && d.start == t.start) && d.role != "test" && local(&d) {
                    items.push(Item { sec: Sec::Types, path: d.path.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![format!("named in the signature of {tid}")], score: 80, can_inline: d.end - d.start < 120, inline: false, calls: vec![] });
                }
            }
        }
        // ---- callees: names the body calls (call syntax), plus types it uses, resolved locally
        let mut callees = 0;
        let mut callee_rows: Vec<DefRow> = vec![];
        let mut sorted_ids: Vec<&String> = body_ids.iter().filter(|i| !sig_ids.contains(*i)).collect();
        sorted_ids.sort();
        for id in sorted_ids {
            if id == last || callees >= 15 {
                continue;
            }
            let n = idx.def_count(id);
            if n == 0 || n > 6 {
                continue;
            }
            let called = call_ids.contains(id);
            for d in idx.defs_named(id) {
                // package-level values only (a local `err` elsewhere is not this one)
                let value_ok = VALUE_KINDS.contains(&d.kind.as_str()) && d.scope.is_empty();
                // a method is only "this" method if it is local, or the target names its type
                let owner = d.name.rsplit_once('.').map(|(o, _)| o.to_string()).or_else(|| d.scope.rsplit('.').next().map(|x| x.to_string())).unwrap_or_default().to_lowercase();
                let same_dir = d.path.rsplit_once('/').map(|x| x.0) == t.path.rsplit_once('/').map(|x| x.0);
                let method_ok = d.kind != "method" || same_dir || (!owner.is_empty() && file_words.contains(&owner));
                let fits = method_ok && ((called && FUNC_KINDS.contains(&d.kind.as_str())) || (TYPE_KINDS.contains(&d.kind.as_str()) && d.kind != "table") || value_ok);
                if fits && d.role != "test" && d.role != "integration" && !(d.path == t.path && d.start == t.start) && local(&d) {
                    let lines = d.end - d.start + 1;
                    items.push(Item { sec: Sec::Callees, path: d.path.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![format!("used by {tid}")], score: 50 - (lines as i32 / 10), can_inline: lines <= 40, inline: false, calls: vec![] });
                    callees += 1;
                    callee_rows.push(d.clone());
                }
            }
        }
        // ---- data layer: tables the target (or its file, for a model) touches
        let mut touched: HashSet<String> = HashSet::new();
        let mut data_ids: HashSet<String> = body_ids.clone();
        for c in &callee_rows {
            let cl = files.get(&root, &c.path).clone();
            for l in code_only(&cl, c.start, c.end) {
                for m in ident_rx().find_iter(&l) {
                    data_ids.insert(m.as_str().to_string());
                }
            }
        }
        for id in data_ids.iter().chain(std::iter::once(&t.name)) {
            let l = id.to_lowercase();
            for cand in [l.clone(), format!("{l}s"), l.trim_end_matches('s').to_string(), format!("{l}es")] {
                if defined_tables.contains_key(&cand) {
                    touched.insert(cand);
                }
            }
        }
        for table in touched.iter().take(6) {
            for (p, line) in &defined_tables[table] {
                let fdefs = defs_in(idx, p);
                let (s, e, kind, name) = match enclosing(&fdefs, *line) {
                    Some(d) => (d.start, d.end, d.kind.clone(), d.name.clone()),
                    None => (*line, line + 12, "lines".into(), format!("table {table}")),
                };
                items.push(Item { sec: Sec::Data, path: p.clone(), start: s, end: e, kind, name, why: vec![format!("defines table {table}, used by {tid}")], score: 55, can_inline: e - s < 120, inline: false, calls: vec![*line] });
            }
            for (p, role, line) in idx.table_uses(table).into_iter().filter(|(p, r, _)| (r == "query" || r == "model") && *p != t.path).take(4) {
                let _ = role;
                let fdefs = defs_in(idx, &p);
                if let Some(d) = enclosing(&fdefs, line) {
                    items.push(Item { sec: Sec::Data, path: p.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), why: vec![format!("queries table {table}")], score: 35, can_inline: d.end - d.start < 40, inline: false, calls: vec![line] });
                }
            }
        }
    }

    // ---- dedupe: same span keeps the best section/score and merges reasons
    let mut merged: Vec<Item> = vec![];
    let mut at: HashMap<(String, usize, usize), usize> = HashMap::new();
    items.sort_by(|a, b| (a.sec, -a.score, &a.path, a.start, a.end).cmp(&(b.sec, -b.score, &b.path, b.start, b.end)));
    for it in items {
        let k = (it.path.clone(), it.start, it.end);
        if let Some(&i) = at.get(&k) {
            let m = &mut merged[i];
            for w in it.why {
                if !m.why.contains(&w) {
                    m.why.push(w);
                }
            }
            m.calls.extend(it.calls);
            m.calls.sort();
            m.calls.dedup();
            m.score = m.score.max(it.score);
            continue;
        }
        at.insert(k, merged.len());
        merged.push(it);
    }
    Ok(Gathered { merged, files, n_targets: targets.len(), ref_lines, ref_files: ref_files.len(), last: last.to_string() })
}

/// `st`/`index_ms`: how the caller brought its store up to date, for the header line.
pub fn run<S: Store>(idx: &S, query: &str, opts: &Opts, st: &UpdateStats, index_ms: u128, t0: Instant) -> Result<String, String> {
    let Gathered { mut merged, mut files, n_targets, ref_lines, ref_files, last } = gather(idx, query)?;
    let root = idx.root().to_path_buf();
    let q = query.trim();
    let last = last.as_str();

    // ---- budget fill + render. Claude Code cuts a tool result off at ~30k characters, so the
    // rendered text must fit MAX_OUT: if it does not, inline less (then point at less) and retry.
    let max_out: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let mut order: Vec<usize> = (0..merged.len()).collect();
    order.sort_by(|&a, &b| (-merged[a].score, merged[a].end - merged[a].start, &merged[a].path, merged[a].start).cmp(&(-merged[b].score, merged[b].end - merged[b].start, &merged[b].path, merged[b].start)));
    let (mut budget, mut maxp) = (opts.budget, opts.max_pointers);
    let mut shrunk = false;
    loop {
        if opts.map {
            for m in merged.iter_mut() { m.can_inline = false; }
        } else if opts.edit {
            for m in merged.iter_mut() {
                let n = m.end - m.start + 1;
                m.can_inline = match m.sec {
                    Sec::Target => true,
                    Sec::Types | Sec::Data => n <= 40,
                    _ => false,
                };
            }
        }
        let (shown, used, omitted) = fill(&mut merged, &order, &mut files, &root, budget, maxp);
        let total_ms = t0.elapsed().as_millis();
        let mut rows: Vec<String> = vec![];
        let out = if opts.json {
            render_json(&merged, &shown, &mut files, &root, query, used, &omitted, st, index_ms, total_ms)
        } else {
            render_text(&merged, &shown, &mut files, &root, q, n_targets, used, &omitted, st, index_ms, total_ms, shrunk.then_some(budget), (ref_lines, ref_files), last, opts.edit, opts.map, &mut rows)
        };
        if out.len() <= max_out || (budget == 0 && maxp == 0) {
            crate::query::save_pointers(crate::query::session_file(opts.session.as_deref()).as_deref(), &rows);
            if let Some(sid) = opts.session.clone().filter(|s| !s.is_empty()).or_else(crate::query::session_id) { crate::precache::put_reply(&sid, &root, &rows); }
            if out.len() > max_out {
                // Only the target is left and it alone is too long: cut it, and say so.
                let cut = (0..=max_out.saturating_sub(200)).rev().find(|&i| out.is_char_boundary(i)).unwrap_or(0);
                return Ok(format!("{}\n... output cut at {cut} characters to fit the tool-result limit. Read the rest with: mt span <file> <name> --context 0, or narrow the query.\n", &out[..cut]));
            }
            return Ok(out);
        }
        shrunk = true;
        if budget > 400 { budget = budget * 3 / 4 } else if budget > 0 { budget = 0 } else { maxp = maxp * 3 / 4 }
    }
}

pub(crate) fn fill(merged: &mut [Item], order: &[usize], files: &mut Files, root: &std::path::Path, budget: usize, maxp: usize) -> (Vec<bool>, usize, HashMap<Sec, usize>) {
    let mut used = 0usize;
    let mut pointers = 0usize;
    let mut omitted: HashMap<Sec, usize> = HashMap::new();
    let mut shown = vec![false; merged.len()];
    for m in merged.iter_mut() {
        m.inline = false;
    }
    // Targets are inlined whatever the budget, but never past what one tool result can carry:
    // beyond that a target becomes a pointer (span + hash) instead of the reply being cut off.
    let max_out: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let target_cap = max_out / 4 * 85 / 100;
    for &i in order {
        let it = &merged[i];
        let lines = files.get(root, &it.path);
        // a folded body costs only the lines it keeps (plus one marker per folded run)
        let body_tokens: usize = match fold_plan(lines, it, it.end.min(lines.len().saturating_sub(1))) {
            Some(keep) => keep.iter().map(|&k| lines.get(k).map_or(0, |l| l.len()) + 6).sum::<usize>() / 4 + 20 + 8 * runs(&keep),
            None => lines.iter().skip(it.start).take(it.end + 1 - it.start).map(|l| l.len() + 6).sum::<usize>() / 4 + 20,
        };
        if (it.sec == Sec::Target && it.can_inline && used + body_tokens <= target_cap) || (it.sec != Sec::Target && it.can_inline && used + body_tokens <= budget) {
            used += body_tokens;
            merged[i].inline = true;
            shown[i] = true;
        } else if pointers < maxp && (pointers < 20 || used + 25 + it.calls.len().min(3) * 20 <= budget) {
            // pointers cost context too: past the first 20 they come out of the same budget
            pointers += 1;
            used += 25 + it.calls.len().min(3) * 20;
            shown[i] = true;
        } else {
            *omitted.entry(it.sec).or_default() += 1;
        }
    }
    (shown, used, omitted)
}

#[allow(clippy::too_many_arguments)]
fn render_json(merged: &[Item], shown: &[bool], files: &mut Files, root: &std::path::Path, query: &str, used: usize, omitted: &HashMap<Sec, usize>, st: &UpdateStats, index_ms: u128, total_ms: u128) -> String {
    let visible: Vec<usize> = (0..merged.len()).filter(|&i| shown[i]).collect();
    let n_inline = visible.iter().filter(|&&i| merged[i].inline).count();
    let mut out = vec![];
    for (n, &i) in visible.iter().enumerate() {
        let it = &merged[i];
        let lines = files.get(root, &it.path).clone();
        let e = it.end.min(lines.len().saturating_sub(1));
        // a folded body: the kept lines in order, and the folded line ranges (1-based, inclusive)
        let fold = if it.inline { fold_plan(&lines, it, e) } else { None };
        let folded: Vec<[usize; 2]> = fold.as_ref().map(|k| k.iter().zip(k.iter().skip(1)).filter(|(a, b)| **b > **a + 1).map(|(a, b)| [a + 2, *b]).collect()).unwrap_or_default();
        let body = match &fold {
            Some(k) => serde_json::Value::String(k.iter().map(|&i| lines[i].as_str()).collect::<Vec<_>>().join("\n")),
            None if it.inline && !lines.is_empty() => serde_json::Value::String(lines[it.start.min(e)..=e].join("\n")),
            None => serde_json::Value::Null,
        };
        out.push(serde_json::json!({
            "id": n + 1, "section": it.sec.key(), "path": it.path, "start": it.start + 1, "end": e + 1,
            "span": format!("{}:L{}-{}", it.path, it.start + 1, e + 1), "hash": span_hash(&lines, it.start, e),
            "kind": it.kind, "name": it.name, "why": it.why, "inline": it.inline,
            "calls": it.calls.iter().map(|c| serde_json::json!([c + 1, lines.get(*c).cloned().unwrap_or_default()])).collect::<Vec<_>>(),
            "body": body, "folded": folded,
        }));
    }
    let v = serde_json::json!({
        "query": query, "tokens": used, "inlined": n_inline, "pointers": visible.len() - n_inline,
        "omitted": omitted.iter().map(|(k, v)| (k.key().to_string(), *v)).collect::<HashMap<_, _>>(),
        "index": {"files": st.files, "reparsed": st.changed, "walk_ms": st.walk_ms, "parse_ms": st.parse_ms, "write_ms": st.write_ms, "update_ms": index_ms},
        "ms": total_ms, "items": out,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn render_text(merged: &[Item], shown: &[bool], files: &mut Files, root: &std::path::Path, q: &str, n_targets: usize, used: usize, omitted: &HashMap<Sec, usize>, st: &UpdateStats, index_ms: u128, total_ms: u128, shrunk_to: Option<usize>, refs: (usize, usize), last: &str, edit: bool, map: bool, rows: &mut Vec<String>) -> String {
    let visible: Vec<usize> = (0..merged.len()).filter(|&i| shown[i]).collect();
    let n_inline = visible.iter().filter(|&&i| merged[i].inline).count();
    let mut s = format!(
        "# mt context {q}: {} target, {} related, {} inlined, {} pointers, ~{} tokens, {} ms (index: {} files, {} re-parsed, {} ms){}\n",
        n_targets, visible.len() - n_targets, n_inline, visible.len() - n_inline, used, total_ms, st.files, st.changed, index_ms,
        shrunk_to.map(|b| format!(" [budget lowered to {b} to fit the tool-result limit]")).unwrap_or_default()
    );
    let mut cur: Option<Sec> = None;
    for (n, &i) in visible.iter().enumerate() {
        let it = &merged[i];
        if cur != Some(it.sec) {
            s.push_str(&format!("## {}\n", it.sec.title()));
            cur = Some(it.sec);
        }
        let lines = files.get(root, &it.path).clone();
        if lines.is_empty() {
            continue;
        }
        let e = it.end.min(lines.len() - 1);
        let why = if it.why.is_empty() { String::new() } else { format!("  why: {}", it.why.join("; ")) };
        let h = span_hash(&lines, it.start, e);
        rows.push(crate::query::pointer_row(n + 1, &it.path, it.start, e, &it.kind, &it.name, &h));
        if map {
            // one line: where, how big, its hash, why it is here (its relation to the target)
            s.push_str(&format!("[{}] {}:L{}-{} {} {} ({} lines) h={}{}\n", n + 1, it.path, it.start + 1, e + 1, it.kind, it.name, e + 1 - it.start.min(e), h, why));
            continue;
        }
        s.push_str(&format!("[{}] {}:L{}-{} {} {} h={}{}\n", n + 1, it.path, it.start + 1, e + 1, it.kind, it.name, h, why));
        render_body(&mut s, it, &lines, e, edit);
    }
    if map && !visible.is_empty() {
        // the relations at a glance, by number: who calls it, what tests it, what it uses
        let by = |sec: Sec| -> String { visible.iter().enumerate().filter(|(_, i)| merged[**i].sec == sec).map(|(n, _)| format!("[{}]", n + 1)).collect::<Vec<_>>().join(" ") };
        let parts: Vec<String> = [(Sec::Callers, "called by"), (Sec::Tests, "unit tests"), (Sec::Integration, "integration tests"), (Sec::Types, "types it uses"), (Sec::Callees, "calls"), (Sec::Data, "data"), (Sec::Docs, "docs")]
            .iter().filter_map(|(sec, label)| { let b = by(*sec); (!b.is_empty()).then(|| format!("{label} {b}")) }).collect();
        s.push_str(&format!("## relations of [1]: {}\n## read what you need in one call: `mt expand <n> <n> ...` (served from memory)\n", parts.join(" · ")));
    }
    let ptrs: Vec<usize> = visible.iter().enumerate().filter(|(_, i)| !merged[**i].inline).map(|(n, _)| n + 1).collect();
    s.push_str(&crate::query::expand_hint(&ptrs));
    if !omitted.is_empty() {
        let mut parts: Vec<String> = omitted.iter().map(|(k, v)| format!("{v} {}", k.key())).collect();
        parts.sort();
        s.push_str(&format!("## not shown (raise --max-pointers, or query a narrower name): {}\n", parts.join(", ")));
    } else {
        s.push_str(&format!(
            "## complete: all {} references to {last} in {} indexed files are listed above (as bodies or pointers with their exact lines); no need to grep again. Index covers {} files; it skips hidden dirs, node_modules, vendor, build output.\n",
            refs.0, refs.1, st.files
        ));
    }
    s
}

/// Indentation width (a tab counts 4): how mt sees block structure in any language.
fn indent(l: &str) -> usize {
    l.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// Runs of consecutive line numbers in `keep` minus one: the number of folded gaps between them.
fn runs(keep: &BTreeSet<usize>) -> usize {
    keep.iter().zip(keep.iter().skip(1)).filter(|(a, b)| **b > **a + 1).count()
}

/// The lines that make a referencing line readable on its own: `ctx` lines either side, plus
/// every line above it that opens an enclosing block (less indented than what it encloses),
/// up to the definition's own line. Indentation only, so it works for every language.
pub(crate) fn scope_lines(lines: &[String], s: usize, e: usize, calls: &[usize], ctx: usize) -> BTreeSet<usize> {
    let mut keep = BTreeSet::new();
    let base = lines.get(s).map_or(0, |l| indent(l));
    for &c in calls.iter().filter(|&&c| c >= s && c <= e) {
        keep.extend(c.saturating_sub(ctx).max(s)..=(c + ctx).min(e));
        let mut cur = lines.get(c).map_or(0, |l| indent(l));
        for k in (s..c).rev() {
            let l = &lines[k];
            if l.trim().is_empty() || indent(l) >= cur { continue; }
            keep.insert(k);
            cur = indent(l);
            if cur <= base { break; }
        }
    }
    keep
}

/// How an inlined body is folded: None to show it whole. Only bodies shown for their references
/// (callers, tests, users; never a target or a span you asked for), only past 20 lines, and only
/// when folding hides at least 30%. Kept: the signature, each referencing line with one line
/// either side and the lines opening its enclosing blocks, and the last line. MT_NO_FOLD=1 turns
/// folding off.
pub(crate) fn fold_plan(lines: &[String], it: &Item, e: usize) -> Option<BTreeSet<usize>> {
    let s = it.start;
    if it.sec == Sec::Target || it.calls.is_empty() || e < s || e + 1 - s < 20 || e >= lines.len() || std::env::var_os("MT_NO_FOLD").is_some() {
        return None;
    }
    let mut keep = scope_lines(lines, s, e, &it.calls, 1);
    // the signature: the last line at the definition's own indentation before its body starts
    // (a doc comment above it shares that indentation and is folded with the rest)
    let base = indent(&lines[s]);
    let head = (s..=e).take_while(|&k| k == s || lines[k].trim().is_empty() || indent(&lines[k]) <= base).last().unwrap_or(s);
    keep.insert(head);
    keep.insert(e);
    // a gap of one or two lines costs as much as its marker: show it
    let ks: Vec<usize> = keep.iter().copied().collect();
    for w in ks.windows(2) {
        if w[1] - w[0] <= 3 { keep.extend(w[0] + 1..w[1]); }
    }
    if keep.len() * 10 > (e + 1 - s) * 7 { None } else { Some(keep) }
}

/// Numbered lines of `keep`, with one `...` line per folded run.
fn render_kept(s: &mut String, lines: &[String], keep: &BTreeSet<usize>, mark: &[usize], w: usize, clip: bool) {
    let mut prev: Option<usize> = None;
    for &k in keep {
        if let Some(p) = prev {
            if k > p + 1 { s.push_str(&format!("{:>w$}  ... L{}-{} folded ({} lines)\n", "", p + 2, k, k - p - 1, w = w)); }
        }
        let l = lines.get(k).map_or("", |l| l.as_str());
        let l = if clip && l.chars().count() > 240 { format!("{}…", l.trim_end().chars().take(240).collect::<String>()) } else { l.trim_end().to_string() };
        s.push_str(&format!("{:>w$}{} {}\n", k + 1, if mark.contains(&k) { ">" } else { " " }, l, w = w));
        prev = Some(k);
    }
}

/// An item's body (inlined, folded to its references when that saves a lot) or its referencing
/// lines (pointer; in edit mode each with two lines either side and the lines opening its
/// enclosing blocks, so a pointer reads on its own).
pub(crate) fn render_body(s: &mut String, it: &Item, lines: &[String], e: usize, edit: bool) {
    let w = (e + 1).to_string().len();
    // a pointer shows lines, not paragraphs: long prose or minified lines are cut
    let clip = |l: &str| -> String { let t = l.trim_end(); if t.chars().count() > 240 { format!("{}…", t.chars().take(240).collect::<String>()) } else { t.to_string() } };
    if it.inline {
        if let Some(keep) = fold_plan(lines, it, e) {
            s.push_str(&format!("{:>w$}  (folded to its references: {} of {} lines; whole body: mt expand {}:L{}-{})\n", "", keep.len(), e + 1 - it.start, it.path, it.start + 1, e + 1, w = w));
            render_kept(s, lines, &keep, &it.calls, w, false);
            return;
        }
        for (k, l) in lines[it.start.min(e)..=e].iter().enumerate() {
            s.push_str(&format!("{:>w$}  {}\n", it.start + k + 1, l, w = w));
        }
    } else if edit {
        let calls: Vec<usize> = it.calls.iter().take(8).copied().collect();
        render_kept(s, lines, &scope_lines(lines, it.start, e, &calls, 2), &it.calls, w, true);
        if it.calls.len() > 8 {
            s.push_str(&format!("{:>w$}  ... {} more referencing lines in this span: mt span {} {}\n", "", it.calls.len() - 8, it.path, it.name, w = w));
        }
    } else {
        for c in it.calls.iter().take(3) {
            if let Some(l) = lines.get(*c) {
                s.push_str(&format!("{:>w$}  {}\n", c + 1, clip(l), w = w));
            }
        }
        if it.calls.len() > 3 {
            s.push_str(&format!("{:>w$}  ... {} more lines in this span\n", "", it.calls.len() - 3, w = w));
        }
    }
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    fn item(sec: Sec, start: usize, end: usize, calls: Vec<usize>) -> Item {
        Item { sec, path: "x".into(), start, end, kind: "function".into(), name: "f".into(), why: vec![], score: 0, can_inline: true, inline: true, calls }
    }

    #[test]
    fn folds_by_indentation_in_any_language() {
        // Go-like: doc comment, signature, filler, an if block holding the reference
        let mut go: Vec<String> = vec!["// F does things.".into(), "// More words.".into(), "func F(a int) error {".into()];
        go.extend((0..20).map(|i| format!("\tx{i} := {i}")));
        go.extend(["\tif a > 0 {".into(), "\t\tfor _, v := range xs {".into(), "\t\t\tWriteAudit(v)".into(), "\t\t}".into(), "\t}".into(), "\treturn nil".into(), "}".into()]);
        let e = go.len() - 1;
        let c = go.iter().position(|l| l.contains("WriteAudit")).unwrap();
        let keep = fold_plan(&go, &item(Sec::Callers, 0, e, vec![c]), e).expect("folds");
        let shown: Vec<&str> = keep.iter().map(|&k| go[k].trim()).collect();
        assert!(shown.contains(&"func F(a int) error {") && shown.contains(&"if a > 0 {") && shown.contains(&"for _, v := range xs {"), "{shown:?}");
        assert!(!shown.contains(&"// F does things.") && !shown.contains(&"x5 := 5") && shown.last() == Some(&"}"), "{shown:?}");

        // Python-like: decorator + def, no braces at all
        let mut py: Vec<String> = vec!["@route".into(), "def handler(req):".into()];
        py.extend((0..20).map(|i| format!("    y{i} = {i}")));
        py.extend(["    with lock:".into(), "        write_audit(req)".into(), "    return y1".into()]);
        let e = py.len() - 1;
        let c = py.iter().position(|l| l.contains("write_audit")).unwrap();
        let keep = fold_plan(&py, &item(Sec::Callers, 0, e, vec![c]), e).expect("folds");
        let shown: Vec<&str> = keep.iter().map(|&k| py[k].trim()).collect();
        assert!(shown.contains(&"def handler(req):") && shown.contains(&"with lock:") && !shown.contains(&"y7 = 7"), "{shown:?}");

        // never folded: a target, a body with no references, a short body
        assert!(fold_plan(&go, &item(Sec::Target, 0, go.len() - 1, vec![c]), go.len() - 1).is_none());
        assert!(fold_plan(&go, &item(Sec::Callers, 0, go.len() - 1, vec![]), go.len() - 1).is_none());
        assert!(fold_plan(&go, &item(Sec::Callers, 20, 27, vec![25]), 27).is_none());
    }
}
