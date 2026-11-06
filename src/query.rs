//! `mt query`: many questions and claims in one call, answered from the index, in one reply.
//!
//! Input (file or stdin): a JSON array of queries, or {"queries": [...], "budget": 6000, "edit": true}.
//! Each query names one op and its subject, plus optional fields:
//!   {"id": "q1", "callers": "WriteAudit", "depth": 2, "expect": ["Login", "audit.go:auditWrite"], "exhaustive": true}
//!
//! ops        subject              answers
//!   context      Name             everything related (what `mt context` gathers)
//!   defined_in   Name             where Name is defined
//!   exists       Name             whether anything defines Name (expect: true/false)
//!   references   Name             every definition/line mentioning Name (any file)
//!   callers      Name             references from code (not tests/docs); "depth" hops up to 4
//!   tested_by    Name             references from unit and integration tests
//!   constructs   Type             where a value of Type is built (`Type{`, `Type(`, `new Type`): heuristic
//!   has_field    Type             identifiers in the type's body (expect: field names)
//!   calls        Name             names Name's body calls (expect: names)
//!   uses_table   Name             tables Name's body names (expect: table names)
//!   property     recv.name|name   every place a property shows up, in every spelling (userID,
//!                                 user_id, UserId): accesses (with their receivers), declarations,
//!                                 string keys and tags (json:"user_id"), SQL columns, mentions
//!   grep         text             literal lines (or "regex": true), optional "in": path prefix
//!   span         path:Name        the code itself: path:Name, path:Type.method, path:L10-20,
//!                                 path:/regex/ (the enclosing definition), always inlined
//!   skeleton     path             a file's outline: every definition with its line range
//!   map          dir|"."          the repo's layout: directories with file counts by role, to
//!                                 "depth" levels (default 2); replaces `ls`/`git ls-files`
//!   module       name             a dependency, any language: the manifest/lockfile lines that
//!                                 name it, the code that uses it, and with "path" (its source on
//!                                 disk, wherever the package manager keeps it) that source's
//!                                 layout and top-level outline ("package": one directory)
//!
//! "span": "$q1" inlines the code of what query q1 found (grep hits, callers, constructions...),
//! so "find, then read" is one batch. Bodies already inlined earlier in this session (same span,
//! same hash) come back as pointers marked unchanged; "fresh": true repeats them.
//!
//! "variants": true on references/callers/tested_by also follows every spelling of the name.
//! Queries that do not depend on each other run in parallel.
//!
//! A subject "$q1" means the names query q1 found (a hop without a round trip).
//! With "expect", the query is a claim: the reply is ✓ (with spans and hashes, ready for
//! `mt apply`), or ✗ with the difference, or "can't check". "exhaustive": the expected list
//! must be the complete set; anything the index finds that is not on it is shown in full.
//! Expect entries match a definition name, a path (suffix), "path:Name" or "path:L42".
//! Matching is mechanical (names, spans, text). Whether code is right is for the reader.

use crate::context::{self, call_rx, code_only, enclosing, ident_rx, last_seg, span_hash, Files, Item, Sec, FUNC_KINDS, TYPE_KINDS};
use crate::index::{DefRow, Store, UpdateStats};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

const OPS: &[&str] = &["context", "defined_in", "exists", "references", "callers", "tested_by", "constructs", "has_field", "calls", "uses_table", "property", "grep", "span", "skeleton", "map", "module", "search"];

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    role: String,
    start: usize,
    end: usize,
    kind: String,
    name: String,
    /// 0-based lines that matched inside the span
    lines: Vec<usize>,
    hop: usize,
}

#[derive(PartialEq, Clone, Copy)]
enum Verdict {
    Pass,
    Fail,
    Answer,
    Cannot,
}

struct Res {
    id: String,
    verdict: Verdict,
    head: String,
    how: String,
    notes: Vec<String>,
    /// compact `path:Ls-e kind name` lines (with hashes) for spans a passing claim covers
    spans: Vec<Hit>,
    /// items for the shared code section: (item, why)
    items: Vec<Item>,
    /// names for `$id` subjects in later queries
    names: Vec<String>,
}

fn sec_for(role: &str) -> Sec {
    match role {
        "test" => Sec::Tests,
        "integration" => Sec::Integration,
        "doc" => Sec::Docs,
        _ => Sec::Callers,
    }
}

fn hit_item(h: &Hit, why: String, score: i32) -> Item {
    let n = h.end - h.start + 1;
    // code first: docs and generated cards mention everything
    let score = score - match h.role.as_str() { "doc" => 30, "config" => 10, _ => 0 };
    Item { sec: sec_for(&h.role), path: h.path.clone(), start: h.start, end: h.end, kind: h.kind.clone(), name: h.name.clone(), why: vec![why], score, can_inline: n <= 80, inline: false, calls: h.lines.clone() }
}

fn def_hit(d: &DefRow) -> Hit {
    Hit { path: d.path.clone(), role: d.role.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), lines: vec![d.head], hop: 0 }
}

/// Definitions named `q` (`Type.method` narrows by scope), real code before tests.
fn defs_for<S: Store>(st: &S, q: &str) -> Vec<DefRow> {
    let last = last_seg(q);
    let mut v = st.defs_named(last);
    if let Some((scope, _)) = q.rsplit_once('.') {
        let scoped: Vec<DefRow> = v.iter().filter(|d| d.name == q || d.scope == scope || d.scope.ends_with(&format!(".{scope}")) || d.name.starts_with(&format!("{scope}."))).cloned().collect();
        if !scoped.is_empty() {
            v = scoped;
        }
    }
    v.sort_by_key(|d| (matches!(d.role.as_str(), "test" | "integration") as u8, d.path.clone(), d.start));
    v
}

/// Every mention of `name` outside its own definitions, grouped by enclosing definition.
fn ref_hits<S: Store>(st: &S, name: &str, hop: usize) -> Vec<Hit> {
    let last = last_seg(name);
    let own = st.defs_named(last);
    let mut groups: HashMap<(String, usize, usize), Hit> = HashMap::new();
    for (path, role, lines) in st.refs(last) {
        let fdefs = st.defs_in(&path);
        for l1 in lines {
            let l0 = l1 - 1;
            if own.iter().any(|d| d.path == path && (d.head == l0 || (d.start <= l0 && l0 <= d.end && !TYPE_KINDS.contains(&d.kind.as_str()) && d.end - d.start < 400))) {
                continue; // the definition itself, or recursion inside it
            }
            let (s, e, kind, dname) = match enclosing(&fdefs, l0) {
                Some(d) => (d.start, d.end, d.kind.clone(), d.name.clone()),
                None => (l0.saturating_sub(3), l0 + 3, "lines".to_string(), format!("line {l1}")),
            };
            groups.entry((path.clone(), s, e)).or_insert_with(|| Hit { path: path.clone(), role: role.clone(), start: s, end: e, kind, name: dname, lines: vec![], hop }).lines.push(l0);
        }
    }
    let mut v: Vec<Hit> = groups.into_values().collect();
    v.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    v
}

fn constructs_hits<S: Store>(st: &S, files: &mut Files, name: &str) -> Vec<Hit> {
    let last = last_seg(name);
    let rx = Regex::new(&format!(r"(?:\bnew\s+{n}\b|\b{n}\s*(?:<[^<>]*>)?\s*[\{{(]|\b{n}::new\s*\()", n = regex::escape(last))).unwrap();
    let decl = Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:impl|struct|type|class|interface|enum|trait|record|data\s+class|func|fn|def|case\s+class)\b").unwrap();
    let root = st.root().to_path_buf();
    let mut groups: HashMap<(String, usize, usize), Hit> = HashMap::new();
    for (path, role, lines) in st.refs(last) {
        let text = files.get(&root, &path).clone();
        let fdefs = st.defs_in(&path);
        for l1 in lines {
            let l0 = l1 - 1;
            let Some(line) = text.get(l0) else { continue };
            if decl.is_match(line) || fdefs.iter().any(|d| d.head == l0 && last_seg(&d.name) == last) {
                continue;
            }
            let real = rx.find_iter(line).any(|m| {
                let before = line[..m.start()].trim_end();
                // `) Type {` is a return type, `[]Type{` / `map[K]Type{` build a collection
                !(before.ends_with(')') || before.ends_with(']') || before.ends_with(':') || before.ends_with("->"))
            });
            if !real {
                continue;
            }
            let (s, e, kind, dname) = match enclosing(&fdefs, l0) {
                Some(d) => (d.start, d.end, d.kind.clone(), d.name.clone()),
                None => (l0.saturating_sub(3), l0 + 3, "lines".to_string(), format!("line {l1}")),
            };
            groups.entry((path.clone(), s, e)).or_insert_with(|| Hit { path: path.clone(), role: role.clone(), start: s, end: e, kind, name: dname, lines: vec![], hop: 1 }).lines.push(l0);
        }
    }
    let mut v: Vec<Hit> = groups.into_values().collect();
    v.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    v
}

fn grep_hits<S: Store>(st: &S, pat: &Regex, prefix: Option<&str>) -> (Vec<Hit>, usize, usize) {
    let root = st.root().to_path_buf();
    let paths: Vec<(String, String)> = st.text_files().into_iter().filter(|(p, _)| prefix.map(|x| p.starts_with(x)).unwrap_or(true)).collect();
    let n_files = paths.len();
    // ponytail: brute-force parallel scan; an FTS5 trigram table would make this sublinear on huge repos
    let per_file: Vec<(Vec<Hit>, usize)> = par_files(&paths, |(p, r)| {
        let t = crate::texts::get(&root, p)?;
        // one regex pass over the whole file; line numbers counted only up to each hit
        let (mut ls, mut line, mut pos) = (Vec::new(), 0usize, 0usize);
        for m in pat.find_iter(&t) {
            line += t.as_bytes()[pos..m.start()].iter().filter(|&&c| c == b'\n').count();
            pos = m.start();
            if ls.last() != Some(&line) { ls.push(line); }
            if ls.len() >= 200 { break; }
        }
        if ls.is_empty() { return None; }
        let n = ls.len();
        Some((group(&st.defs_in(p), p, r, ls), n))
    });
    let total = per_file.iter().map(|(_, n)| n).sum();
    let mut v: Vec<Hit> = per_file.into_iter().flat_map(|(h, _)| h).collect();
    v.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    (v, total, n_files)
}

/// Run `f` over items on every core, keeping the Some results (in input order per thread chunk).
fn par_files<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> Option<R> + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let chunk = items.len().div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = items.chunks(chunk).map(|c| s.spawn(move || (c.iter().filter_map(f).collect::<Vec<R>>(), crate::work::snapshot()))).collect();
        hs.into_iter().flat_map(|h| h.join().map(|(v, w)| { crate::work::absorb(w); v }).unwrap_or_default()).collect()
    })
}

/// 0-based lines of one file grouped by enclosing definition (or a small window when none).
fn group(fdefs: &[DefRow], path: &str, role: &str, lines: Vec<usize>) -> Vec<Hit> {
    let mut groups: Vec<Hit> = vec![];
    let mut at: HashMap<(usize, usize), usize> = HashMap::new();
    for l0 in lines {
        let (s, e, kind, dname) = match enclosing(fdefs, l0) {
            Some(d) => (d.start, d.end, d.kind.clone(), d.name.clone()),
            None => (l0.saturating_sub(2), l0 + 2, "lines".to_string(), format!("line {}", l0 + 1)),
        };
        let i = *at.entry((s, e)).or_insert_with(|| { groups.push(Hit { path: path.to_string(), role: role.to_string(), start: s, end: e, kind, name: dname, lines: vec![], hop: 1 }); groups.len() - 1 });
        groups[i].lines.push(l0);
    }
    groups
}

/// How a line uses property `name`: the patterns, compiled once per spelling.
struct Shapes {
    access: Regex,
    index: Regex,
    key: Regex,
    tag: Regex,
    decl: Regex,
}

fn shapes(name: &str) -> Shapes {
    let n = regex::escape(name);
    Shapes {
        access: Regex::new(&format!(r"([A-Za-z_][A-Za-z0-9_]*|\)|\])\s*(?:\?\.|\.|->|::)\s*{n}\b")).unwrap(),
        index: Regex::new(&format!(r#"\[\s*["'`]{n}["'`]\s*\]"#)).unwrap(),
        key: Regex::new(&format!(r#"["'`]{n}["'`,]"#)).unwrap(),
        tag: Regex::new(&format!(r#"(?:json|db|yaml|xml|bson|gorm|form):"{n}\b"#)).unwrap(),
        decl: Regex::new(&format!(r"^\s*(?:(?:pub|public|private|protected|readonly|static|final|val|var|let|const)\s+)*{n}\s*[:?=\s]")).unwrap(),
    }
}

fn sql_rx() -> &'static Regex {
    static RX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| Regex::new(r"(?i)\b(select|insert|update|create\s+table|alter\s+table|where)\b").unwrap())
}

/// Mechanical shapes only; what the code means is for the reader.
fn property_kind(line: &str, sh: &Shapes, path: &str) -> (&'static str, Option<String>) {
    if let Some(c) = sh.access.captures(line) {
        let r = match &c[1] { ")" => "(call)".to_string(), "]" => "(index)".to_string(), x => x.to_string() };
        return ("access", Some(r));
    }
    if sh.index.is_match(line) {
        return ("access", Some("[...]".into()));
    }
    if sh.key.is_match(line) || sh.tag.is_match(line) {
        return ("key/tag", None);
    }
    if path.ends_with(".sql") || sql_rx().is_match(line) {
        return ("column", None);
    }
    if sh.decl.is_match(line) {
        return ("declaration", None);
    }
    ("mention", None)
}

/// Does expect entry `e` name this hit? A definition name, a path or path suffix,
/// "path:Name", or "path:L42" / "path:42" (a line inside the span).
fn matches(e: &str, h: &Hit) -> bool {
    let e = e.trim();
    let name_ok = |n: &str| h.name == n || last_seg(&h.name) == n || h.name.ends_with(&format!(".{n}"));
    // whole path segments only: "audit.go", "api/audit.go", "internal/api" (a directory)
    let path_ok = |p: &str| {
        let p = p.trim_end_matches('/');
        h.path == p || h.path.ends_with(&format!("/{p}")) || (p.contains('/') && (h.path.starts_with(&format!("{p}/")) || h.path.contains(&format!("/{p}/"))))
    };
    if let Some((p, n)) = e.rsplit_once(':') {
        let line = n.trim_start_matches('L').parse::<usize>().ok();
        return path_ok(p) && match line { Some(l) => h.start < l && l <= h.end + 1, None => name_ok(n) };
    }
    name_ok(e) || path_ok(e)
}

fn expect_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()),
        _ => None,
    }
}

/// Judge a claim about a set of hits.
fn judge_list(r: &mut Res, hits: &[Hit], expect: &[String], exhaustive: bool, what: &str) {
    let missing: Vec<&String> = expect.iter().filter(|e| !hits.iter().any(|h| matches(e, h))).collect();
    let extra: Vec<&Hit> = hits.iter().filter(|h| !expect.iter().any(|e| matches(e, h))).collect();
    let ok = missing.is_empty() && (!exhaustive || extra.is_empty());
    r.verdict = if ok { Verdict::Pass } else { Verdict::Fail };
    if !missing.is_empty() {
        r.notes.push(format!("expected but not found: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if exhaustive && !extra.is_empty() {
        r.notes.push(format!("{} {what} not on your list, shown below", extra.len()));
        for h in &extra {
            r.items.push(hit_item(h, format!("{}: {what} missing from the expected list", r.id), 90));
        }
    }
    if ok {
        r.spans = hits.iter().filter(|h| expect.iter().any(|e| matches(e, h))).cloned().collect();
    } else if !missing.is_empty() {
        // show what is actually there, so the reader can correct the assumption
        for h in hits.iter().take(12) {
            if !exhaustive || expect.iter().any(|e| matches(e, h)) {
                r.items.push(hit_item(h, format!("{}: actual {what}", r.id), 70));
            }
        }
    }
}

fn subjects(subject: &str, prior: &HashMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    match subject.strip_prefix('$') {
        Some(id) => prior.get(id).cloned().ok_or_else(|| format!("${id} is not an earlier query id")),
        None => Ok(vec![subject.to_string()]),
    }
}

fn ambiguity<S: Store>(st: &S, name: &str) -> String {
    match st.def_count(last_seg(name)) {
        0 => format!("{} defined nowhere in the index", last_seg(name)),
        1 => "name defined in 1 file".into(),
        n => format!("name defined in {n} files: a match may refer to another one"),
    }
}

#[allow(clippy::too_many_lines)]
type Found = HashMap<String, Vec<(String, usize, usize, String, String)>>;

fn run_one<S: Store>(st: &S, files: &mut Files, q: &Value, n: usize, prior: &HashMap<String, Vec<String>>, found: &Found) -> Res {
    let id = q.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| format!("q{}", n + 1));
    let mut r = Res { id: id.clone(), verdict: Verdict::Answer, head: String::new(), how: String::new(), notes: vec![], spans: vec![], items: vec![], names: vec![] };
    let Some(op) = OPS.iter().find(|o| q.get(**o).is_some()) else {
        r.verdict = Verdict::Cannot;
        r.head = format!("{}", q);
        r.notes.push(format!("no known op; use one of: {}", OPS.join(", ")));
        return r;
    };
    let subject = q[*op].as_str().unwrap_or("").to_string();
    let expect = q.get("expect");
    let exhaustive = q.get("exhaustive").and_then(|v| v.as_bool()).unwrap_or(false);
    r.head = format!("{op} {subject}{}", if exhaustive { " (exhaustive)" } else { "" });
    // `span: $q1`: the code of what q1 found, without a round trip
    if *op == "span" && subject.starts_with('$') {
        let qid = &subject[1..];
        let Some(spans) = found.get(qid) else { r.verdict = Verdict::Cannot; r.notes.push(format!("${qid} is not an earlier query id")); return r; };
        if spans.is_empty() { r.verdict = Verdict::Cannot; r.notes.push(format!("${qid} found nothing to read")); return r; }
        r.how = format!("{} span(s) from {qid}", spans.len());
        for (p, s0, e0, kind, name) in spans {
            let h = Hit { path: p.clone(), role: crate::index::role_of(p).to_string(), start: *s0, end: *e0, kind: kind.clone(), name: name.clone(), lines: vec![], hop: 0 };
            let mut it = hit_item(&h, format!("{id}: span ${qid}"), 90);
            it.sec = Sec::Target;
            it.can_inline = true;
            r.names.push(last_seg(name).to_string());
            r.items.push(it);
        }
        return r;
    }
    // `search`: a question in plain words, no name needed (crate::search)
    if *op == "search" {
        let limit = q.get("limit").and_then(|v| v.as_u64()).unwrap_or(12).clamp(1, 60) as usize;
        let found = crate::search::search(st, &subject, limit);
        r.how = format!("plain words: BM25 over every definition (its name, scope, path, doc and body), {} result(s)", found.len());
        if found.is_empty() { r.verdict = Verdict::Cannot; r.notes.push(format!("nothing in the code matches {subject:?}")); return r; }
        for (k, f) in found.iter().enumerate() {
            let d = &f.def;
            let h = Hit { path: d.path.clone(), role: d.role.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), lines: vec![], hop: 0 };
            let mut it = hit_item(&h, format!("{id}: matches {} ({:.1})", f.matched.join(", "), f.score), 90 - 3 * k as i32);
            // the best three come with their code; the rest are pointers for one `mt expand`
            if k < 3 { it.sec = Sec::Target; it.can_inline = true; }
            r.names.push(last_seg(&d.name).to_string());
            r.items.push(it);
        }
        return r;
    }
    let names = match subjects(&subject, prior) {
        Ok(v) => v,
        Err(e) => { r.verdict = Verdict::Cannot; r.notes.push(e); return r; }
    };
    if names.is_empty() {
        r.verdict = Verdict::Cannot;
        r.notes.push(format!("{subject} found no names to follow"));
        return r;
    }
    let root = st.root().to_path_buf();
    let elist = expect.and_then(expect_list);
    let cannot = |r: &mut Res, why: String| { r.verdict = Verdict::Cannot; r.notes.push(why); };

    match *op {
        "map" => {
            let pre = subject.trim_start_matches("./").trim_matches('/');
            let depth = q.get("depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            let roles = ["source", "test", "integration", "migration", "model", "query", "doc", "config"];
            let mut dirs: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>> = Default::default();
            let mut n = 0;
            for (path, role) in st.files_with_roles(&roles) {
                if !pre.is_empty() && pre != "." && !path.starts_with(&format!("{pre}/")) { continue; }
                n += 1;
                let rel = if pre.is_empty() || pre == "." { path.as_str() } else { &path[pre.len() + 1..] };
                let parts: Vec<&str> = rel.split('/').collect();
                let key = if parts.len() <= 1 { "(files here)".to_string() } else { parts[..(parts.len() - 1).min(depth)].join("/") + "/" };
                *dirs.entry(key).or_default().entry(role).or_default() += 1;
            }
            r.how = format!("{n} indexed files under {}", if pre.is_empty() { "." } else { pre });
            for (d, by) in dirs.iter().take(150) {
                let total: usize = by.values().sum();
                r.notes.push(format!("{d:<44} {total:>4}  {}", by.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")));
            }
            if dirs.len() > 150 { r.notes.push(format!("... {} more directories: narrow with a subject path", dirs.len() - 150)); }
        }
        "module" => {
            // A dependency, for any language: where the repo mentions it (manifests and lockfiles
            // first: every file that defines no code), and, given "path" (the dependency's source
            // on disk, wherever its package manager keeps it), that source's layout and outline.
            // mt knows no ecosystem: it reads text and parses with the same grammars as the repo.
            let mut facts: Vec<String> = vec![];
            let roles = ["source", "test", "integration", "migration", "model", "query", "doc", "config"];
            let mut decl: Vec<String> = vec![];
            let mut docs: Vec<String> = vec![];
            let mut uses: Vec<String> = vec![];
            let mut use_files = 0;
            // the whole repo, on every core: read, match, classify by the language table
            enum Hit3 { Doc(String), Code(String), Decl(Vec<String>) }
            let files = st.files_with_roles(&roles);
            let found: Vec<Hit3> = par_files(&files, |(path, _role)| {
                let text = crate::texts::get(&root, path)?;
                if !text.contains(subject.as_str()) { return None; }
                let hits: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| l.contains(subject.as_str())).take(3).collect();
                let first = hits.first().map(|(n, l)| format!("  {path}:{} {}", n + 1, l.trim())).unwrap_or_default();
                // code is a file a programming-language grammar parses (data/markup grammars carry
                // max_depth); prose is docs; everything else (data, heuristic and plain-text files:
                // manifests, lockfiles) declares it
                Some(match crate::languages::detect(&root.join(path), &text) {
                    crate::languages::Tier::Markdown => Hit3::Doc(first),
                    crate::languages::Tier::TreeSitter(l) if l.max_depth.is_none() => Hit3::Code(first),
                    _ => Hit3::Decl(hits.iter().map(|(n, l)| format!("  {path}:{} {}", n + 1, l.trim())).collect()),
                })
            });
            let mut found = found;
            found.sort_by(|a, b| { let k = |h: &Hit3| match h { Hit3::Doc(s) | Hit3::Code(s) => s.clone(), Hit3::Decl(v) => v.first().cloned().unwrap_or_default() }; k(a).cmp(&k(b)) });
            for h in found {
                match h {
                    Hit3::Doc(l) => if docs.len() < 5 { docs.push(l) },
                    Hit3::Code(l) => { use_files += 1; if uses.len() < 12 { uses.push(l) } }
                    Hit3::Decl(v) => decl.extend(v),
                }
            }
            facts.push(if decl.is_empty() { format!("declared: {subject} is not named in any manifest or lockfile") } else { format!("declared ({} line(s) in files without code: manifests, lockfiles, config):", decl.len()) });
            facts.extend(decl.into_iter().take(20));
            facts.push(format!("used in {use_files} code file(s){}", if use_files == 0 { "" } else { ":" }));
            facts.extend(uses);
            if !docs.is_empty() { facts.push("mentioned in docs:".into()); facts.extend(docs); }
            if let Some(dir) = q.get("path").and_then(|v| v.as_str()) {
                let dir = std::path::PathBuf::from(dir.replace('~', &std::env::var("HOME").unwrap_or_default()));
                let dir = if dir.is_absolute() { dir } else { root.join(dir) };
                if !dir.is_dir() {
                    facts.push(format!("path {}: not a directory (not installed or downloaded yet?)", dir.display()));
                } else {
                    // layout: directories holding parseable source, two levels down
                    let mut files: Vec<std::path::PathBuf> = vec![];
                    let mut stack = vec![(dir.clone(), 0usize)];
                    while let Some((d, depth)) = stack.pop() {
                        let Ok(rd) = std::fs::read_dir(&d) else { continue };
                        for e in rd.flatten() {
                            let p = e.path();
                            let name = e.file_name().to_string_lossy().into_owned();
                            if name.starts_with('.') { continue; }
                            if p.is_dir() { if depth < 2 { stack.push((p, depth + 1)); } } else { files.push(p); }
                        }
                    }
                    files.sort();
                    let mut by_dir: std::collections::BTreeMap<String, usize> = Default::default();
                    let sub = q.get("package").and_then(|x| x.as_str()).unwrap_or("");
                    let mut outline: Vec<String> = vec![];
                    for f in &files {
                        let rel = f.strip_prefix(&dir).unwrap_or(f);
                        let parent = rel.parent().map(|x| x.display().to_string()).unwrap_or_default();
                        let src = std::fs::read_to_string(f).unwrap_or_default();
                        let defs = crate::definitions(f, &src);
                        if defs.is_empty() { continue; }
                        *by_dir.entry(if parent.is_empty() { ".".into() } else { parent.clone() }).or_default() += 1;
                        // outline of one directory: "package", or the top level
                        let here = if sub.is_empty() { parent.is_empty() } else { parent == sub || parent.ends_with(&format!("/{sub}")) };
                        if here && crate::index::role_of(&rel.display().to_string()) == "source" {
                            let lines: Vec<&str> = src.lines().collect();
                            for d in defs.iter().filter(|d| d.path.is_empty()) {
                                outline.push(format!("  {}:{} {}", rel.display(), d.head + 1, lines.get(d.head).map(|l| l.trim()).unwrap_or(&d.name)));
                            }
                        }
                    }
                    facts.push(format!("source at {} ({} directories with code):", dir.display(), by_dir.len()));
                    facts.extend(by_dir.iter().take(30).map(|(d, n)| format!("  {d}/ {n} file(s)")));
                    facts.push(format!("top-level definitions in {} ({}):", if sub.is_empty() { "the root" } else { sub }, outline.len()));
                    facts.extend(outline.into_iter().take(80));
                }
            }
            r.how = "manifests, lockfiles and code that name it; \"path\" for its source".into();
            r.notes = facts;
        }
        "skeleton" => {
            let abs = root.join(&subject);
            let Some(src) = crate::texts::get(&root, &subject).map(|t| t.to_string()) else { cannot(&mut r, format!("{subject}: file not found")); return r; };
            let defs = crate::definitions(&abs, &src);
            r.how = format!("{} definitions, {} lines", defs.len(), src.lines().count());
            for d in &defs {
                r.notes.push(format!("{}{} {} L{}-{}", "  ".repeat(d.path.len()), d.kind, d.name, d.start + 1, d.end + 1));
                r.names.push(last_seg(&d.name).to_string());
            }
        }
        "span" => {
            let Some((p, sel)) = subject.split_once(':') else { cannot(&mut r, "span needs \"path:Name\", \"path:L10-20\" or \"path:/regex/\"".into()); return r; };
            let abs = root.join(p);
            let Some(src) = crate::texts::get(&root, p).map(|t| t.to_string()) else { cannot(&mut r, format!("{p}: file not found")); return r; };
            let lines: Vec<&str> = src.lines().collect();
            let role = crate::index::role_of(p).to_string();
            let range = sel.strip_prefix('L').and_then(|x| {
                let (a, b) = x.split_once('-').unwrap_or((x, x));
                Some((a.parse::<usize>().ok()?, b.trim_start_matches('L').parse::<usize>().ok()?))
            });
            let mut hits: Vec<Hit> = vec![];
            if let Some((a, b)) = range {
                if a >= 1 && a <= b && a <= lines.len() {
                    hits.push(Hit { path: p.into(), role: role.clone(), start: a - 1, end: b.min(lines.len()) - 1, kind: "lines".into(), name: format!("L{a}-{b}"), lines: vec![], hop: 0 });
                }
            } else {
                let defs = crate::definitions(&abs, &src);
                for d in crate::find(&defs, &lines, sel) {
                    hits.push(Hit { path: p.into(), role: role.clone(), start: d.start, end: d.end, kind: d.kind.clone(), name: d.name.clone(), lines: vec![], hop: 0 });
                }
            }
            if hits.is_empty() { cannot(&mut r, format!("{p}: nothing matches {sel:?}")); return r; }
            r.how = format!("{} span(s)", hits.len());
            for h in &hits {
                let mut it = hit_item(h, format!("{id}: span {sel}"), 95);
                it.sec = Sec::Target;
                it.can_inline = true;
                r.names.push(last_seg(&h.name).to_string());
                r.items.push(it);
            }
        }
        "context" => {
            for nm in &names {
                match context::gather(st, nm) {
                    Ok(g) => {
                        r.notes.push(format!("{} target(s), {} related", g.n_targets, g.merged.len() - g.n_targets));
                        for mut it in g.merged {
                            it.why.insert(0, id.clone());
                            r.names.push(last_seg(&it.name).to_string());
                            r.items.push(it);
                        }
                        files.cache.extend(g.files.cache);
                    }
                    Err(e) => r.notes.push(e),
                }
            }
            r.how = "what `mt context` gathers".into();
            if expect.is_some() { r.notes.push("context takes no expect; use a specific op to check a claim".into()); }
        }
        "defined_in" | "exists" => {
            let hits: Vec<Hit> = names.iter().flat_map(|nm| defs_for(st, nm)).map(|d| def_hit(&d)).collect();
            r.names = hits.iter().map(|h| last_seg(&h.name).to_string()).collect();
            r.how = match hits.len() { 0 => "no definition by that name".into(), 1 => "exact: 1 definition".into(), k => format!("by name: {k} definitions") };
            if *op == "exists" {
                let want = expect.and_then(|v| v.as_bool()).unwrap_or(true);
                r.verdict = if hits.is_empty() != want { Verdict::Pass } else { Verdict::Fail };
                if r.verdict == Verdict::Pass { r.spans = hits.clone() } else { for h in &hits { r.items.push(hit_item(h, format!("{id}: defines {subject}"), 60)); } }
            } else if let Some(e) = &elist {
                judge_list(&mut r, &hits, e, exhaustive, "definitions");
            } else {
                for h in &hits {
                    let mut it = hit_item(h, format!("{id}: defines {}", last_seg(&h.name)), 85);
                    it.sec = Sec::Target;
                    r.items.push(it);
                }
            }
        }
        "property" => {
            let (recv, prop) = match subject.rsplit_once('.') { Some((r, p)) if !r.is_empty() && !p.is_empty() => (Some(r.to_string()), p.to_string()), _ => (None, subject.clone()) };
            let variants = q.get("variants").and_then(|v| v.as_bool()).unwrap_or(true);
            let spell = if variants { let v = st.spellings(&prop); if v.is_empty() { vec![prop.clone()] } else { v } } else { vec![prop.clone()] };
            // candidate lines from the reference index (graph or SQLite), classified in parallel
            let mut cand: Vec<(String, String, String, Vec<usize>)> = vec![];
            for sp in &spell {
                for (p, role, ls) in st.refs(sp) { cand.push((p, role, sp.clone(), ls)); }
            }
            let recv_norm = recv.as_deref().map(crate::index::norm);
            let shape_of: HashMap<String, Shapes> = spell.iter().map(|sp| (sp.clone(), shapes(sp))).collect();
            type Found = (Vec<Hit>, Vec<(&'static str, String, Option<String>)>);
            let found: Vec<Found> = par_files(&cand, |(p, role, sp, ls)| {
                let text = crate::texts::get(&root, p)?;
                let lines: Vec<&str> = text.lines().collect();
                let mut keep = vec![];
                let mut facts = vec![];
                for &l1 in ls {
                    let Some(line) = lines.get(l1 - 1) else { continue };
                    let (kind, rcv) = property_kind(line, &shape_of[sp], p);
                    // with a receiver (`user.userID`), accesses through other receivers are dropped;
                    // declarations, keys, columns and mentions of the name are kept
                    if let (Some(want), Some(got)) = (&recv_norm, &rcv) {
                        if kind == "access" && crate::index::norm(got) != *want && !crate::index::norm(got).ends_with(want.as_str()) { continue; }
                    }
                    keep.push(l1 - 1);
                    facts.push((kind, sp.clone(), rcv));
                }
                if keep.is_empty() { return None; }
                Some((group(&st.defs_in(p), p, role, keep), facts))
            });
            let mut hits: Vec<Hit> = vec![];
            let mut by_kind: HashMap<&str, usize> = HashMap::new();
            let mut by_spell: HashMap<String, usize> = HashMap::new();
            let mut by_recv: HashMap<String, usize> = HashMap::new();
            for (h, facts) in found {
                hits.extend(h);
                for (k, sp, r) in facts {
                    *by_kind.entry(k).or_default() += 1;
                    *by_spell.entry(sp).or_default() += 1;
                    if let Some(r) = r { *by_recv.entry(r).or_default() += 1; }
                }
            }
            hits.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
            let fmt = |m: &HashMap<String, usize>, max: usize| { let mut v: Vec<(&String, &usize)> = m.iter().collect(); v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0))); v.iter().take(max).map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ") };
            r.notes.push(format!("spellings: {}", fmt(&by_spell, 12)));
            let mut kinds: Vec<(&&str, &usize)> = by_kind.iter().collect();
            kinds.sort_by(|a, b| b.1.cmp(a.1));
            r.notes.push(format!("uses: {}", kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")));
            if !by_recv.is_empty() { r.notes.push(format!("receivers: {}", fmt(&by_recv, 15))); }
            r.how = format!("{} spellings from the name index, lines from the reference index, use shape by text; {} lines in {} spans, {} files", spell.len(), by_kind.values().sum::<usize>(), hits.len(), hits.iter().map(|h| &h.path).collect::<HashSet<_>>().len());
            r.names = hits.iter().filter(|h| h.kind != "lines").map(|h| last_seg(&h.name).to_string()).collect();
            match &elist {
                Some(e) => judge_list(&mut r, &hits, e, exhaustive, "places"),
                None => for h in &hits { r.items.push(hit_item(h, format!("{id}: uses {subject}"), 60)); },
            }
        }
        "references" | "callers" | "tested_by" => {
            let names: Vec<String> = if q.get("variants").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mut v: Vec<String> = names.iter().flat_map(|n| { let s = st.spellings(last_seg(n)); if s.is_empty() { vec![n.clone()] } else { s } }).collect();
                v.sort();
                v.dedup();
                v
            } else { names };
            let depth = q.get("depth").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 4) as usize;
            let keep = |role: &str| match *op { "callers" => !matches!(role, "test" | "integration" | "doc"), "tested_by" => matches!(role, "test" | "integration"), _ => true };
            let root = st.root().to_path_buf();
            // each hop follows definitions, not names: a site counts for a target only when, of
            // every definition sharing the name, the target is the one it reaches (crate::resolve)
            let mut frontier: Vec<(String, Vec<DefRow>)> = names.iter().map(|n| (last_seg(n).to_string(), defs_for(st, n))).collect();
            let mut seen_defs: HashSet<(String, usize)> = frontier.iter().flat_map(|(_, ds)| ds.iter().map(|d| (d.path.clone(), d.start))).collect();
            let mut hits: Vec<Hit> = vec![];
            let mut conf: HashMap<(String, usize, usize), crate::resolve::Conf> = HashMap::new();
            let mut dropped = (0usize, 0usize); // (not code: comments/strings, reaches another definition)
            let mut seen_spans: HashSet<(String, usize, usize)> = HashSet::new();
            let mut words: HashMap<String, HashSet<String>> = HashMap::new();
            for hop in 1..=depth {
                let mut next: Vec<(String, Vec<DefRow>)> = vec![];
                for (nm, targets) in frontier.iter().take(40) {
                    let cands = st.defs_named(nm);
                    for mut h in ref_hits(st, nm, hop) {
                        if !keep(&h.role) || seen_spans.contains(&(h.path.clone(), h.start, h.end)) { continue; }
                        let callable = FUNC_KINDS.contains(&h.kind.as_str());
                        // a caller is code that runs: a type or constant naming the function is a reference, not a call
                        if *op == "callers" && !(callable || h.kind == "lines" || h.kind.starts_with('~')) { continue; }
                        let lines = files.get(&root, &h.path).clone();
                        if *op != "references" {
                            // a call is code: a mention in a comment or a string is not one
                            h.lines.retain(|&l0| crate::resolve::in_code(&lines, l0, nm));
                            if h.lines.is_empty() { dropped.0 += 1; continue; }
                        }
                        let w = words.entry(h.path.clone()).or_insert_with(|| (*crate::precache::words(&root, &h.path, &lines)).clone());
                        let c = if targets.is_empty() { Some(crate::resolve::Conf::Ambiguous) } else { crate::resolve::attribute(&h.path, &h.role, w, &cands, targets) };
                        let Some(c) = c else {
                            // it reaches another definition of the same name
                            if *op != "references" { dropped.1 += 1; continue; }
                            conf.insert((h.path.clone(), h.start, h.end), crate::resolve::Conf::Ambiguous);
                            seen_spans.insert((h.path.clone(), h.start, h.end));
                            hits.push(h);
                            continue;
                        };
                        seen_spans.insert((h.path.clone(), h.start, h.end));
                        conf.insert((h.path.clone(), h.start, h.end), c);
                        // only a confident edge is followed further: an ambiguous one is shown, not expanded
                        if callable && c != crate::resolve::Conf::Ambiguous {
                            if let Some(d) = st.defs_in(&h.path).into_iter().find(|d| d.start == h.start && d.end == h.end) {
                                if seen_defs.insert((d.path.clone(), d.start)) { next.push((last_seg(&d.name).to_string(), vec![d])); }
                            }
                        }
                        hits.push(h);
                    }
                }
                frontier = next;
                if frontier.is_empty() || hits.len() > 300 { break; }
            }
            r.names = hits.iter().filter(|h| h.kind != "lines").map(|h| last_seg(&h.name).to_string()).collect();
            let label = |h: &Hit| conf.get(&(h.path.clone(), h.start, h.end)).copied().unwrap_or(crate::resolve::Conf::Ambiguous);
            let count = |c: crate::resolve::Conf| hits.iter().filter(|h| label(h) == c).count();
            let (ex, li, am) = (count(crate::resolve::Conf::Exact), count(crate::resolve::Conf::Likely), count(crate::resolve::Conf::Ambiguous));
            r.how = format!("{}; {} sites in {} files{}: {ex} resolved, {li} nearest, {am} ambiguous{}",
                ambiguity(st, &names[0]), hits.len(), hits.iter().map(|h| &h.path).collect::<HashSet<_>>().len(),
                if depth > 1 { format!(", {depth} hops") } else { String::new() },
                if dropped.0 + dropped.1 > 0 { format!("; left out: {} only in comments/strings, {} reaching another definition of the name", dropped.0, dropped.1) } else { String::new() });
            match &elist {
                Some(e) => judge_list(&mut r, &hits, e, exhaustive, "sites"),
                None => for h in &hits {
                    let c = label(h);
                    let note = if c == crate::resolve::Conf::Exact { String::new() } else { format!("; {}", c.label()) };
                    r.items.push(hit_item(h, format!("{id}: {} {}{}{note}", if *op == "tested_by" { "tests" } else { "references" }, names.join("/"), if h.hop > 1 { format!(" (hop {})", h.hop) } else { String::new() }), 60 - 10 * h.hop as i32 - if c == crate::resolve::Conf::Ambiguous { 15 } else { 0 }));
                },
            }
        }
        "constructs" => {
            let hits: Vec<Hit> = names.iter().flat_map(|nm| constructs_hits(st, files, nm)).collect();
            r.names = hits.iter().filter(|h| h.kind != "lines").map(|h| last_seg(&h.name).to_string()).collect();
            r.how = format!("heuristic: `{0}{{`, `{0}(`, `new {0}`; {1}; {2} sites", last_seg(&names[0]), ambiguity(st, &names[0]), hits.len());
            match &elist {
                Some(e) => judge_list(&mut r, &hits, e, exhaustive, "construction sites"),
                None => for h in &hits { r.items.push(hit_item(h, format!("{id}: builds {}", names[0]), 65)); },
            }
        }
        "has_field" | "calls" | "uses_table" => {
            let defs: Vec<DefRow> = names.iter().flat_map(|nm| defs_for(st, nm)).filter(|d| *op != "has_field" || TYPE_KINDS.contains(&d.kind.as_str()) || d.end > d.start).collect();
            let Some(d) = defs.iter().find(|d| !matches!(d.role.as_str(), "test" | "integration")).or(defs.first()).cloned() else {
                cannot(&mut r, format!("no definition named {subject}"));
                return r;
            };
            let lines = files.get(&root, &d.path).clone();
            let code = code_only(&lines, d.start, d.end);
            let found: Vec<String> = match *op {
                "has_field" => code.iter().flat_map(|l| ident_rx().find_iter(l).map(|m| m.as_str().to_string()).collect::<Vec<_>>()).collect(),
                "calls" => code.iter().flat_map(|l| call_rx().captures_iter(l).map(|c| c[1].to_string()).collect::<Vec<_>>()).collect(),
                _ => {
                    let text = code.join("\n").to_lowercase();
                    const SQL_WORDS: &[&str] = &["if", "not", "exists", "select", "table", "temp", "temporary", "the", "and", "for"];
                    st.defined_tables().into_keys().filter(|t| t.len() >= 3 && !SQL_WORDS.contains(&t.as_str())).filter(|t| Regex::new(&format!(r"\b{}\b", regex::escape(t))).map(|rx| rx.is_match(&text)).unwrap_or(false)).collect()
                }
            };
            let found: HashSet<String> = found.into_iter().collect();
            r.how = match *op {
                "has_field" => "word inside the type's span".into(),
                "calls" => "call syntax inside the body; name only".into(),
                _ => "defined table names inside the body, word match".into(),
            };
            let dh = def_hit(&d);
            r.how.push_str(&format!("; {} at {}:L{}-{}{}", d.name, d.path, d.start + 1, d.end + 1, if defs.len() > 1 { format!(", {} definitions, first used", defs.len()) } else { String::new() }));
            match &elist {
                Some(e) => {
                    let missing: Vec<&String> = e.iter().filter(|x| !found.iter().any(|f| f == *x || f.eq_ignore_ascii_case(x) && *op == "uses_table")).collect();
                    r.verdict = if missing.is_empty() { Verdict::Pass } else { Verdict::Fail };
                    if missing.is_empty() {
                        r.spans.push(dh);
                    } else {
                        r.notes.push(format!("not found: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                        let mut it = hit_item(&dh, format!("{id}: the actual {}", d.name), 95);
                        it.sec = Sec::Target;
                        r.items.push(it);
                    }
                }
                None => {
                    let mut f: Vec<String> = found.into_iter().filter(|x| *op != "calls" || st.def_count(x) > 0).collect();
                    f.sort();
                    r.notes.push(format!("{}: {}", match *op { "has_field" => "identifiers", "calls" => "calls (defined in the index)", _ => "tables" }, f.join(", ")));
                    r.names = f;
                    let mut it = hit_item(&dh, format!("{id}: {}", d.name), 80);
                    it.sec = Sec::Target;
                    r.items.push(it);
                }
            }
        }
        "grep" => {
            let is_rx = q.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
            let pat = if is_rx { format!("(?m){subject}") } else { regex::escape(&subject) };
            let rx = match Regex::new(&pat) { Ok(x) => x, Err(e) => { cannot(&mut r, format!("bad regex: {e}")); return r; } };
            // "in": "." or "./x" means the repo root or a path under it (stored paths have no "./")
            let prefix = q.get("in").and_then(|v| v.as_str()).map(|p| p.trim_start_matches("./").trim_start_matches('.').to_string()).filter(|p| !p.is_empty());
            let (mut hits, mut total, n_files) = grep_hits(st, &rx, prefix.as_deref());
            // `a|b` or `func .*X` sent as a literal and found nowhere was almost surely meant as a regex
            if !is_rx && total == 0 && subject.chars().any(|c| "|*+?[](){}^$\\".contains(c)) {
                if let Ok(rx2) = Regex::new(&format!("(?m){subject}")) {
                    let (h2, t2, _) = grep_hits(st, &rx2, prefix.as_deref());
                    if t2 > 0 {
                        r.notes.push(format!("no literal match; matched as a regex instead ({t2} lines). Pass \"regex\": true to say so."));
                        hits = h2;
                        total = t2;
                    }
                }
            }
            r.names = hits.iter().filter(|h| h.kind != "lines").map(|h| last_seg(&h.name).to_string()).collect();
            r.how = format!("text scan of {n_files} indexed files: {total} lines in {} spans", hits.len());
            match expect {
                Some(Value::Number(k)) => {
                    let k = k.as_u64().unwrap_or(0) as usize;
                    r.verdict = if k == total { Verdict::Pass } else { Verdict::Fail };
                    if k == total { r.spans = hits.clone() } else {
                        r.notes.push(format!("expected {k} matching lines, found {total}"));
                        for h in &hits { r.items.push(hit_item(h, format!("{id}: matches"), 70)); }
                    }
                }
                Some(v) => match expect_list(v) { Some(e) => judge_list(&mut r, &hits, &e, exhaustive, "matches"), None => cannot(&mut r, "expect must be a count, a string or a list".into()) },
                None => for h in &hits { r.items.push(hit_item(h, format!("{id}: matches"), 55)); },
            }
        }
        _ => unreachable!(),
    }
    r.names.sort();
    r.names.dedup();
    r
}

pub fn run<S: Store>(st: &S, input: &str, sid: Option<&str>, stats: &UpdateStats, index_ms: u128, t0: Instant) -> Result<String, String> {
    let v: Value = serde_json::from_str(input).map_err(|e| format!("mt query: input is not JSON: {e}"))?;
    let (queries, opts) = match &v {
        Value::Array(a) => (a.clone(), Value::Null),
        Value::Object(o) => (o.get("queries").and_then(|x| x.as_array()).cloned().ok_or("mt query: object input needs \"queries\": [...]")?, v.clone()),
        _ => return Err("mt query: input must be a JSON array of queries".into()),
    };
    let budget = opts.get("budget").and_then(|x| x.as_u64()).unwrap_or(6000) as usize;
    let maxp = opts.get("max_pointers").and_then(|x| x.as_u64()).unwrap_or(80) as usize;
    let edit = opts.get("edit").and_then(|x| x.as_bool()).unwrap_or(true);
    // "view": "map": no bodies but the spans asked for; one line per item, read with `mt expand`
    let map = opts.get("view").and_then(|x| x.as_str()) == Some("map");
    let root = st.root().to_path_buf();

    // ---- waves: every query whose `$id` subjects are answered runs now, all of them in parallel
    let ids: Vec<String> = queries.iter().enumerate().map(|(n, q)| q.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| format!("q{}", n + 1))).collect();
    let deps: Vec<Vec<String>> = queries.iter().map(|q| {
        OPS.iter().filter_map(|o| q.get(*o).and_then(|v| v.as_str())).filter_map(|s| s.strip_prefix('$')).filter(|d| ids.iter().any(|i| i == d)).map(|d| d.to_string()).collect()
    }).collect();
    let mut files = Files { cache: HashMap::new() };
    let mut prior: HashMap<String, Vec<String>> = HashMap::new();
    let mut found: Found = HashMap::new();
    let mut slots: Vec<Option<Res>> = queries.iter().map(|_| None).collect();
    let mut waves = 0;
    while slots.iter().any(|r| r.is_none()) {
        let ready: Vec<usize> = (0..queries.len()).filter(|&i| slots[i].is_none() && deps[i].iter().all(|d| prior.contains_key(d))).collect();
        // a cycle of `$` references: run the rest anyway; each reports the missing id
        let ready = if ready.is_empty() { (0..queries.len()).filter(|&i| slots[i].is_none()).collect() } else { ready };
        waves += 1;
        let done: Vec<(usize, Res, Files)> = std::thread::scope(|s| {
            let prior = &prior;
            let found = &found;
            let hs: Vec<_> = ready.iter().map(|&i| {
                let q = &queries[i];
                s.spawn(move || {
                    let tq = Instant::now();
                    let mut f = Files { cache: HashMap::new() };
                    let mut r = run_one(st, &mut f, q, i, prior, found);
                    r.how = if r.how.is_empty() { format!("{} ms", tq.elapsed().as_millis()) } else { format!("{} ms; {}", tq.elapsed().as_millis(), r.how) };
                    (i, r, f, crate::work::snapshot())
                })
            }).collect();
            hs.into_iter().filter_map(|h| h.join().ok()).map(|(i, r, f, w)| { crate::work::absorb(w); (i, r, f) }).collect()
        });
        for (i, r, f) in done {
            prior.insert(r.id.clone(), r.names.clone());
            let mut sp: Vec<(String, usize, usize, String, String)> = r.items.iter().map(|it| (it.path.clone(), it.start, it.end, it.kind.clone(), it.name.clone())).collect();
            sp.extend(r.spans.iter().map(|h| (h.path.clone(), h.start, h.end, h.kind.clone(), h.name.clone())));
            sp.dedup();
            found.insert(r.id.clone(), sp);
            files.cache.extend(f.cache);
            slots[i] = Some(r);
        }
    }
    let results: Vec<Res> = slots.into_iter().flatten().collect();

    // ---- one code section for every query: same span once, ranked by how many queries it answers
    let mut merged: Vec<Item> = vec![];
    let mut answers: Vec<Vec<String>> = vec![];
    let mut at: HashMap<(String, usize, usize), usize> = HashMap::new();
    for r in &results {
        for it in &r.items {
            let k = (it.path.clone(), it.start, it.end);
            if let Some(&i) = at.get(&k) {
                let m = &mut merged[i];
                for w in &it.why { if !m.why.contains(w) { m.why.push(w.clone()); } }
                m.calls.extend(it.calls.iter().copied());
                m.calls.sort();
                m.calls.dedup();
                m.score = m.score.max(it.score);
                if it.sec < m.sec { m.sec = it.sec; }
                if !answers[i].contains(&r.id) { answers[i].push(r.id.clone()); }
                continue;
            }
            at.insert(k, merged.len());
            merged.push(it.clone());
            answers.push(vec![r.id.clone()]);
        }
    }
    for (i, m) in merged.iter_mut().enumerate() {
        m.score += 40 * (answers[i].len() as i32 - 1);
        if (edit || map) && m.sec != Sec::Target { m.can_inline = false; }
    }
    let fresh = opts.get("fresh").and_then(|x| x.as_bool()).unwrap_or(false);
    // "session": false: no session state at all (apply's own claim checks must not count as reads)
    let sess = if opts.get("session").and_then(|x| x.as_bool()) == Some(false) { None } else { session_file(sid) };
    let seen: HashSet<String> = match (&sess, fresh) { (Some(p), false) => std::fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect(), _ => HashSet::new() };
    let seen_lines: Shown = if fresh { HashMap::new() } else { load_shown(sess.as_deref()) };
    let key_of = |files: &mut Files, it: &Item| -> String {
        let lines = files.get(&root, &it.path).clone();
        let e = it.end.min(lines.len().saturating_sub(1));
        format!("{}|{}|{}|{}", it.path, it.start, e, span_hash(&lines, it.start, e))
    };
    let mut n_seen = 0;
    for m in merged.iter_mut() {
        // generated or minified text (a line over 1000 characters) is never worth inlining
        if files.get(&root, &m.path).iter().skip(m.start).take(m.end + 1 - m.start.min(m.end)).any(|l| l.len() > 1000) {
            m.can_inline = false;
            m.why.push("generated or minified (a line over 1000 characters): not inlined".into());
            continue;
        }
        if !seen.is_empty() && seen.contains(&key_of(&mut files, m)) {
            m.can_inline = false;
            m.why.push("unchanged, already shown this session (\"fresh\": true repeats it)".into());
            n_seen += 1;
        }
    }
    let mut order: Vec<usize> = (0..merged.len()).collect();
    order.sort_by(|&a, &b| (-merged[a].score, merged[a].end - merged[a].start, &merged[a].path, merged[a].start).cmp(&(-merged[b].score, merged[b].end - merged[b].start, &merged[b].path, merged[b].start)));

    // spans the batch asked for by name come first, smallest first: one that fits on its own
    // always arrives, instead of losing its place to bigger ones and being asked for again
    let requested = |m: &Item| m.sec == Sec::Target && m.why.iter().any(|w| w.contains(": span "));
    order.sort_by_key(|&i| if requested(&merged[i]) { (0, merged[i].end - merged[i].start) } else { (1, 0) });
    let max_out: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let (mut budget, mut maxp) = (budget, maxp);
    let t_render = Instant::now();
    let mut passes = 0;
    loop {
        let (shown, used, omitted) = context::fill(&mut merged, &order, &mut files, &root, budget, maxp);
        passes += 1;
        let mut rows: Vec<String> = vec![];
        let mut out = render(&results, &merged, &answers, &order, &shown, used, &omitted, &mut files, &root, stats, index_ms, t0, edit, map, &mut rows, &seen_lines);
        let tok = out.len() / 4;
        out = out.replacen("@@TOK@@", &tok.to_string(), 1);
        if n_seen > 0 { out = out.replacen("\n", &format!("\n## {n_seen} item(s) already shown this session with the same hash are pointers now\n"), 1); }
        if std::env::var_os("MT_TIMING").is_some() {
            out.push_str(&format!("## timing: {waves} waves, render {} ms over {passes} passes, {} items before budgeting\n", t_render.elapsed().as_millis(), merged.len()));
        }
        if out.len() <= max_out || (budget == 0 && maxp == 0) {
            save_pointers(sess.as_deref(), &rows);
            if let Some(sid) = sid.map(String::from).filter(|s| !s.is_empty()).or_else(session_id) { crate::precache::put_reply(&sid, &root, &rows); }
            if let Some(p) = &sess {
                // a folded body was not shown whole: it must not come back later as "already shown"
                let whole = |files: &mut Files, it: &Item| { let l = files.get(&root, &it.path).clone(); let e = it.end.min(l.len().saturating_sub(1)); context::fold_plan(&l, it, e).is_none() };
                let mut new: Vec<String> = vec![];
                let mut ranges: Vec<(String, u64, usize, usize)> = vec![];
                for m in merged.iter().filter(|m| m.inline) {
                    if whole(&mut files, m) {
                        new.push(key_of(&mut files, m));
                        let l = files.get(&root, &m.path).clone();
                        ranges.push((m.path.clone(), file_hash(&l), m.start, m.end.min(l.len().saturating_sub(1))));
                    }
                }
                record_shown(Some(p.as_path()), &ranges);
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(p) { use std::io::Write; for k in new { let _ = writeln!(f, "{k}"); } }
                // the read-batch count for this session: the nudge toward one batch
                let n = bump_count(p);
                if n > 2 { out.push_str(&format!("## read batch {n} this session: put every remaining question, and every span you will need, into ONE batch (chain with \"span\": \"$id\")\n")); }
            }
            if out.len() > max_out {
                let cut = (0..=max_out.saturating_sub(200)).rev().find(|&i| out.is_char_boundary(i)).unwrap_or(0);
                return Ok(format!("{}\n... output cut at {cut} characters to fit the tool-result limit; split the batch.\n", &out[..cut]));
            }
            return Ok(out);
        }
        if budget > 400 { budget = budget * 3 / 4 } else if budget > 0 { budget = 0 } else { maxp = maxp * 3 / 4 }
    }
}

/// The session as this process sees it: MT_SESSION (set by the plugin), else Claude Code's own
/// CLAUDE_CODE_SESSION_ID (the same id, exported to every Bash call, so this works without the
/// plugin), else the per-session daemon's socket. Clients send theirs to the daemon, so both
/// sides use the same state files.
pub fn session_id() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let s = var("MT_SESSION").or_else(|| var("CLAUDE_CODE_SESSION_ID")).or_else(|| var("MT_SOCKET").map(|s| format!("{:016x}", crate::fnv1a(s.as_bytes()))))?;
    // a subagent has its parent's environment; the hook exports MT_AGENT into its mt commands
    Some(crate::coord::with_agent(s, var("MT_AGENT")))
}

/// Per-session state lives in the temp dir: the seen log (`.log`), the batch count (`.count`)
/// and the last reply's numbered items (`.ptrs`, read by `mt expand N`).
pub(crate) fn session_file(sid: Option<&str>) -> Option<std::path::PathBuf> {
    let sid = sid.filter(|s| !s.is_empty()).map(String::from).or_else(session_id)?;
    let safe: String = sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    Some(std::env::temp_dir().join(format!("mt-query-seen-{safe}.log")))
}

fn bump_count(p: &std::path::Path) -> usize {
    let c = p.with_extension("count");
    let n = std::fs::read_to_string(&c).ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0) + 1;
    let _ = std::fs::write(&c, n.to_string());
    n
}

#[allow(clippy::too_many_arguments)]
fn render(results: &[Res], merged: &[Item], answers: &[Vec<String>], order: &[usize], shown: &[bool], used: usize, omitted: &HashMap<Sec, usize>, files: &mut Files, root: &std::path::Path, stats: &UpdateStats, index_ms: u128, t0: Instant, edit: bool, map: bool, rows: &mut Vec<String>, seen_lines: &Shown) -> String {
    // number visible items in rank order, so the summary can point at them
    let visible: Vec<usize> = order.iter().copied().filter(|&i| shown[i]).collect();
    let num: HashMap<usize, usize> = visible.iter().enumerate().map(|(n, &i)| (i, n + 1)).collect();
    let count = |v: Verdict| results.iter().filter(|r| r.verdict == v).count();
    let mut s = format!(
        "# mt query: {} queries: {} ✓ {} ✗ {} answered {} can't check; {} items ({} inlined, ~{} tokens of bodies), reply ~@@TOK@@ tokens, {} ms (index: {} files, {} re-parsed, {} ms)\n",
        results.len(), count(Verdict::Pass), count(Verdict::Fail), count(Verdict::Answer), count(Verdict::Cannot),
        visible.len(), visible.iter().filter(|&&i| merged[i].inline).count(), used, t0.elapsed().as_millis(), stats.files, stats.changed, index_ms
    );
    let big: Vec<String> = visible.iter().filter(|&&i| merged[i].sec == Sec::Target && !merged[i].inline && merged[i].can_inline).map(|&i| {
        let m = &merged[i];
        let tok: usize = files.get(root, &m.path).iter().skip(m.start).take(m.end + 1 - m.start).map(|l| l.len() + 6).sum::<usize>() / 4;
        let cap = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(28_000) / 4 * 85 / 100;
        format!("[{}] {}:L{}-{} ~{tok} tokens ({})", num[&i], m.path, m.start + 1, m.end + 1, if tok <= cap { "fits in a batch of its own" } else { "larger than any one reply: use narrower spans" })
    }).collect();
    // what mt can still give for each span that did not fit: its outline, with spans and hashes
    let mut outlines = String::new();
    for &i in visible.iter().filter(|&&i| merged[i].sec == Sec::Target && !merged[i].inline && merged[i].can_inline) {
        let m = &merged[i];
        let abs = root.join(&m.path);
        let Some(src) = crate::texts::get(root, &m.path) else { continue };
        let lines: Vec<String> = src.lines().map(String::from).collect();
        let defs: Vec<_> = crate::definitions(&abs, &src).into_iter().filter(|d| d.start >= m.start && d.end <= m.end && d.path.len() <= 1).collect();
        if defs.is_empty() { continue; }
        outlines.push_str(&format!("## outline of [{}] {}:L{}-{} (read the parts you need as spans):\n", num[&i], m.path, m.start + 1, m.end + 1));
        for d in defs.iter().take(40) {
            outlines.push_str(&format!("    {}{} {} {}:L{}-{} h={}\n", "  ".repeat(d.path.len()), d.kind, d.name, m.path, d.start + 1, d.end + 1, span_hash(&lines, d.start, d.end)));
        }
        if defs.len() > 40 { outlines.push_str(&format!("    ... {} more\n", defs.len() - 40)); }
    }
    if !big.is_empty() {
        s.push_str(&outlines);
        s.push_str(&format!("## not inlined: one reply carries ~{} tokens of code and these did not fit after the smaller spans: {}. A whole file is rarely needed: a `skeleton` plus the spans you will edit is cheaper\n",
            std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(28_000) / 4 * 85 / 100, big.join("; ")));
    }
    for r in results {
        let mark = match r.verdict { Verdict::Pass => "✓", Verdict::Fail => "✗", Verdict::Answer => "=", Verdict::Cannot => "?" };
        s.push_str(&format!("{} {mark} {}", r.id, r.head));
        if !r.how.is_empty() { s.push_str(&format!("   [{}]", r.how)); }
        s.push('\n');
        for n in &r.notes { s.push_str(&format!("    {n}\n")); }
        for h in r.spans.iter().take(20) {
            let lines = files.get(root, &h.path).clone();
            let e = h.end.min(lines.len().saturating_sub(1));
            s.push_str(&format!("    {}:L{}-{} {} {} h={}\n", h.path, h.start + 1, e + 1, h.kind, h.name, span_hash(&lines, h.start, e)));
        }
        if r.spans.len() > 20 { s.push_str(&format!("    ... {} more\n", r.spans.len() - 20)); }
        let mine: Vec<usize> = visible.iter().filter(|&&i| answers[i].contains(&r.id)).map(|i| num[i]).collect();
        let hidden = (0..merged.len()).filter(|&i| !shown[i] && answers[i].contains(&r.id)).count();
        if !mine.is_empty() || hidden > 0 {
            let refs: Vec<String> = mine.iter().take(30).map(|n| format!("[{n}]")).collect();
            s.push_str(&format!("    items: {}{}{}\n", refs.join(" "), if mine.len() > 30 { format!(" +{}", mine.len() - 30) } else { String::new() }, if hidden > 0 { format!(" ({hidden} not shown: output limit)") } else { String::new() }));
        }
    }
    if !visible.is_empty() {
        s.push_str("## code\n");
    }
    for &i in &visible {
        let it = &merged[i];
        let lines = files.get(root, &it.path).clone();
        if lines.is_empty() { continue; }
        let e = it.end.min(lines.len() - 1);
        let h = span_hash(&lines, it.start, e);
        rows.push(pointer_row(num[&i], &it.path, it.start, e, &it.kind, &it.name, &h));
        if map && !it.inline {
            s.push_str(&format!("[{}] {}:L{}-{} {} {} ({} lines) h={}  ({})  why: {}\n", num[&i], it.path, it.start + 1, e + 1, it.kind, it.name, e + 1 - it.start.min(e), h, answers[i].join(","), it.why.join("; ")));
            continue;
        }
        s.push_str(&format!("[{}] {}:L{}-{} {} {} h={}  ({})  why: {}\n", num[&i], it.path, it.start + 1, e + 1, it.kind, it.name, h, answers[i].join(","), it.why.join("; ")));
        // lines this session was already shown, in this very version of the file, are in the
        // model's context: point at them instead of sending them again
        if it.inline {
            if let Some(rs) = seen_lines.get(&(it.path.clone(), file_hash(&lines))) {
                let cov: Vec<bool> = (it.start..=e).map(|l| rs.iter().any(|(a, b)| *a <= l && l <= *b)).collect();
                let n = cov.iter().filter(|c| **c).count();
                let w = (e + 1).to_string().len();
                if n == cov.len() {
                    s.push_str(&format!("{:>w$}  (unchanged, already shown earlier in this session: it is in your context above; `mt expand {}` repeats it)\n", "", num[&i], w = w));
                    continue;
                }
                if n >= 6 {
                    let mut k = 0;
                    while k < cov.len() {
                        let mut j = k;
                        while j < cov.len() && cov[j] == cov[k] { j += 1; }
                        if cov[k] {
                            s.push_str(&format!("{:>w$}  … L{}-{} already shown earlier in this session (unchanged)\n", "", it.start + k + 1, it.start + j, w = w));
                        } else {
                            for l in k..j { s.push_str(&format!("{:>w$}  {}\n", it.start + l + 1, lines[it.start + l], w = w)); }
                        }
                        k = j;
                    }
                    continue;
                }
            }
        }
        context::render_body(&mut s, it, &lines, e, edit);
    }
    let ptrs: Vec<usize> = visible.iter().filter(|&&i| !merged[i].inline).map(|i| num[i]).collect();
    s.push_str(&expand_hint(&ptrs));
    if !omitted.is_empty() {
        let n: usize = omitted.values().sum();
        // never silently missing: every item that did not fit is named, as a handle `mt expand` takes
        let hidden: Vec<String> = order.iter().filter(|&&i| !shown[i]).map(|&i| {
            let it = &merged[i];
            format!("{}:L{}-{} {}", it.path, it.start + 1, it.end + 1, it.name)
        }).collect();
        s.push_str(&format!("## not shown, over the output limit ({n}): {}{}. Read them with `mt expand <path:Ls-e> ...` in one call\n",
            hidden.iter().take(60).cloned().collect::<Vec<_>>().join(", "), if hidden.len() > 60 { format!(" … +{}", hidden.len() - 60) } else { String::new() }));
    }
    s
}

// ---------------------------------------------------------------- pointers and `mt expand`

/// Line ranges already shown whole in this session, per file version (path + content hash), so a
/// later reply sends only the lines the model has not seen and points at the rest: they are in
/// its context already. A file that changed at all is a new version: nothing in it counts as
/// shown. Kept in the session's `.ranges` file; the SessionStart hook forgets it on a compaction
/// or /clear, when those earlier replies leave the context. MT_LINE_DEDUPE=0 turns it off.
pub(crate) type Shown = HashMap<(String, u64), Vec<(usize, usize)>>;

pub(crate) fn line_dedupe_on() -> bool { std::env::var("MT_LINE_DEDUPE").as_deref() != Ok("0") }

pub(crate) fn file_hash(lines: &[String]) -> u64 { crate::fnv1a(lines.join("\n").as_bytes()) }

pub(crate) fn load_shown(sess: Option<&std::path::Path>) -> Shown {
    let mut m: Shown = HashMap::new();
    if !line_dedupe_on() { return m; }
    let Some(t) = sess.and_then(|p| std::fs::read_to_string(p.with_extension("ranges")).ok()) else { return m };
    for l in t.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if let (4, Ok(h), Ok(a), Ok(b)) = (f.len(), u64::from_str_radix(f.get(1).copied().unwrap_or(""), 16), f.get(2).copied().unwrap_or("").parse::<usize>(), f.get(3).copied().unwrap_or("").parse::<usize>()) {
            m.entry((f[0].to_string(), h)).or_default().push((a, b));
        }
    }
    m
}

pub(crate) fn record_shown(sess: Option<&std::path::Path>, rows: &[(String, u64, usize, usize)]) {
    let Some(p) = sess else { return };
    if rows.is_empty() || !line_dedupe_on() { return; }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(p.with_extension("ranges")) {
        use std::io::Write;
        for (path, h, a, b) in rows { let _ = writeln!(f, "{path}\t{h:016x}\t{a}\t{b}"); }
    }
}

/// One numbered item of a reply, as stored in the session's `.ptrs` table (1-based lines).
pub(crate) fn pointer_row(n: usize, path: &str, s: usize, e: usize, kind: &str, name: &str, h: &str) -> String {
    format!("{n}\t{path}\t{}\t{}\t{kind}\t{name}\t{h}", s + 1, e + 1)
}

/// The last reply that numbered items replaces the table; a reply without items keeps it.
pub(crate) fn save_pointers(sess: Option<&std::path::Path>, rows: &[String]) {
    if let (Some(p), false) = (sess, rows.is_empty()) {
        let _ = std::fs::write(p.with_extension("ptrs"), rows.join("\n") + "\n");
    }
}

/// The footer that tells the reader how to get a pointer's body.
pub(crate) fn expand_hint(ptrs: &[usize]) -> String {
    // MT_NO_EXPAND: the A/B control, replies exactly as before `mt expand` existed
    if ptrs.is_empty() || std::env::var_os("MT_NO_EXPAND").is_some() {
        return String::new();
    }
    let list: Vec<String> = ptrs.iter().take(20).map(|n| format!("[{n}]")).collect();
    let more = if ptrs.len() > 20 { format!(" +{}", ptrs.len() - 20) } else { String::new() };
    format!(
        "## {} pointer(s) without bodies: {}{more}. Read only the ones you need, together in one call: `mt expand <n> <n> ...` (numbers from this reply; `path:Ls-e@hash` also works)\n",
        ptrs.len(), list.join(" ")
    )
}

struct Target { label: String, path: String, s: usize, e: usize, kind: String, name: String, hash: Option<String> }

/// `path:L13-72`, `path:13-72`, `path:L13`, with an optional `@hash`.
pub(crate) fn parse_handle(t: &str) -> Option<(String, usize, usize, Option<String>)> {
    let (t, hash) = match t.rsplit_once('@') { Some((a, h)) if !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()) => (a, Some(h.to_string())), _ => (t, None) };
    let (path, r) = t.rsplit_once(':')?;
    let r = r.trim_start_matches('L');
    let (a, b) = match r.split_once('-') { Some((a, b)) => (a, b.trim_start_matches('L')), None => (r, r) };
    let (a, b): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
    if path.is_empty() || a == 0 || b < a { return None; }
    Some((path.to_string(), a - 1, b - 1, hash))
}

/// Where a definition named `name` is now, nearest to where it was.
fn relocate(rel: &str, lines: &[String], name: &str, was: usize) -> Option<(usize, usize)> {
    if name.is_empty() { return None; }
    let src = lines.join("\n");
    let tier = crate::languages::detect(std::path::Path::new(rel), &src);
    let defs = crate::definitions_with(&tier, &src);
    let full = |d: &crate::Def| if d.path.is_empty() { d.name.clone() } else { format!("{}.{}", d.path.join("."), d.name) };
    defs.iter()
        .filter(|d| d.name == name || full(d) == name || name.ends_with(&format!(".{}", d.name)))
        .min_by_key(|d| d.start.abs_diff(was))
        .map(|d| (d.start, d.end))
}

/// `mt expand 3 5 8` / `mt expand path:L13-72@4c2ed24b`: the bodies of pointers from an earlier
/// reply, read from disk now. A hash that no longer matches is reported, and the definition is
/// found again by name, so the reader never edits from stale lines.
pub fn expand(root: &std::path::Path, args: &[String], sid: Option<&str>) -> (bool, String) {
    let sess = session_file(sid);
    let table: Vec<Vec<String>> = sess.as_ref()
        .and_then(|p| std::fs::read_to_string(p.with_extension("ptrs")).ok())
        .map(|t| t.lines().map(|l| l.split('\t').map(String::from).collect::<Vec<_>>()).filter(|r| r.len() == 7).collect())
        .unwrap_or_default();
    // a pasted pointer line ("[3] a.go:L1-9 function F h=ab12") is fine: the number and the
    // span name the same item, `h=` attaches to the span before it, and kind/name words are skipped
    let mut targets: Vec<Target> = vec![];
    let mut errs: Vec<String> = vec![];
    let mut skipped: Vec<String> = vec![];
    for a in args {
    let before = (targets.len(), errs.len());
    let mut unknown: Vec<String> = vec![];
    let toks: Vec<String> = a.split([' ', ',', '\t', '\n']).map(|x| x.trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')').to_string()).filter(|x| !x.is_empty()).collect();
    for t in &toks {
        if let Ok(n) = t.parse::<usize>() {
            match table.iter().find(|r| r[0] == n.to_string()) {
                Some(r) => targets.push(Target { label: n.to_string(), path: r[1].clone(), s: r[2].parse::<usize>().unwrap_or(1) - 1, e: r[3].parse::<usize>().unwrap_or(1) - 1, kind: r[4].clone(), name: r[5].clone(), hash: Some(r[6].clone()) }),
                None if sess.is_none() => errs.push(format!("{n}: no session (neither MT_SESSION nor CLAUDE_CODE_SESSION_ID is set), so numbers mean nothing here; pass path:Ls-e@hash")),
                None => errs.push(format!("{n}: the last mt query/context reply in this session has no item [{n}]; pass path:Ls-e@hash")),
            }
        } else if let Some(h) = t.strip_prefix("h=") {
            match targets.last_mut() { Some(x) if x.label.is_empty() && x.hash.is_none() => x.hash = Some(h.to_string()), _ => unknown.push(t.clone()) }
        } else if let Some(r) = crate::coord::at_rows(root, t) {
            // `@brief` / `@brief/changes`: every span another agent handed over, hash-checked
            match r {
                Ok(rows) => targets.extend(rows.into_iter().map(|r| Target { label: String::new(), path: r.path, s: r.s.saturating_sub(1), e: r.e.saturating_sub(1), kind: r.kind, name: r.name, hash: Some(r.hash).filter(|h| !h.is_empty()) })),
                Err(e) => errs.push(e),
            }
        } else if let Some((path, s, e, hash)) = parse_handle(t) {
            let path = std::path::Path::new(&path).strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or(path);
            targets.push(Target { label: String::new(), path, s, e, kind: String::new(), name: String::new(), hash });
        } else {
            unknown.push(t.clone());
        }
    }
    // a pasted line names its item twice; when the span resolved, a stale number is moot
    if targets[before.0..].iter().any(|t| t.label.is_empty()) { errs.truncate(before.1); }
    if (targets.len(), errs.len()) == before { skipped.extend(unknown); }
    }
    // the same item named twice (a number and its span) is shown once
    let mut seen_t: HashSet<(String, usize, usize)> = HashSet::new();
    targets.retain(|t| seen_t.insert((t.path.clone(), t.s, t.e)));

    let max_out: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let mut files = Files { cache: HashMap::new() };
    let mut body = String::new();
    let mut keys: Vec<String> = vec![];
    let mut ranges: Vec<(String, u64, usize, usize)> = vec![];
    let mut left: Vec<String> = vec![];
    let mut shown = 0;
    let mut from_mem = 0;
    let sid_key: Option<String> = sid.map(String::from).filter(|s| !s.is_empty()).or_else(session_id);
    for t in &targets {
        let handle = if t.label.is_empty() { format!("{}:L{}-{}", t.path, t.s + 1, t.e + 1) } else { t.label.clone() };
        if !left.is_empty() { left.push(handle); continue; }
        // from memory: the daemon kept this reply's items, and the file has not changed since
        let mem = (!t.label.is_empty()).then(|| sid_key.as_deref().and_then(|k| crate::precache::get(k, &t.label, root))).flatten()
            .filter(|c| c.path == t.path && c.s == t.s && t.hash.as_deref().is_none_or(|h| h == c.hash));
        if let Some(c) = mem {
            let w = (c.e + 1).to_string().len();
            let mut chunk = format!("[{}] {}:L{}-{} {} {} h={}\n", t.label, c.path, c.s + 1, c.e + 1, c.kind, c.name, c.hash);
            for (k, l) in c.lines.iter().enumerate() { chunk.push_str(&format!("{:>w$}  {}\n", c.s + k + 1, l, w = w)); }
            if body.len() + chunk.len() > max_out && shown > 0 { left.push(handle); continue; }
            keys.push(format!("{}|{}|{}|{}", c.path, c.s, c.e, c.hash));
            if let Some(t) = crate::texts::get(root, &c.path) { let l: Vec<String> = t.lines().map(String::from).collect(); ranges.push((c.path.clone(), file_hash(&l), c.s, c.e)); }
            body.push_str(&chunk);
            shown += 1;
            from_mem += 1;
            continue;
        }
        // straight from disk, not the daemon's in-memory texts: expand skips the sync barrier (it
        // needs no graph), so the watcher may not have caught up yet; the disk is always current
        let lines: Vec<String> = files.cache.entry(t.path.clone()).or_insert_with(|| std::fs::read(root.join(&t.path)).map(|b| String::from_utf8_lossy(&b).lines().map(String::from).collect()).unwrap_or_default()).clone();
        if lines.is_empty() { errs.push(format!("{handle}: {} is missing or empty", t.path)); continue; }
        let (mut s, mut e) = (t.s.min(lines.len() - 1), t.e.min(lines.len() - 1));
        let mut h = span_hash(&lines, s, e);
        let mut note = String::new();
        if let Some(old) = t.hash.as_deref().filter(|o| *o != h) {
            match relocate(&t.path, &lines, &t.name, t.s) {
                Some((ns, ne)) => {
                    (s, e) = (ns, ne);
                    h = span_hash(&lines, s, e);
                    // same hash at a new place: an edit above it moved it; its text is what was shown
                    note = if h == old {
                        format!("  MOVED since the pointer (was L{}-{}), text unchanged", t.s + 1, t.e + 1)
                    } else {
                        format!("  CHANGED since the pointer (was L{}-{} h={old}); this is {} now", t.s + 1, t.e + 1, t.name)
                    };
                }
                None => note = format!("  CHANGED since the pointer (was h={old}); the same lines as they are now"),
            }
        }
        let mut chunk = format!("{}{}:L{}-{} {} {} h={h}{note}\n", if t.label.is_empty() { String::new() } else { format!("[{}] ", t.label) }, t.path, s + 1, e + 1, t.kind, t.name);
        let w = (e + 1).to_string().len();
        let mut cut_at = None;
        for (k, l) in lines[s..=e].iter().enumerate() {
            if body.len() + chunk.len() + l.len() + w + 3 > max_out { cut_at = Some(s + k); break; }
            chunk.push_str(&format!("{:>w$}  {}\n", s + k + 1, l, w = w));
        }
        if cut_at == Some(s) && shown > 0 { left.push(handle); continue; }
        if let Some(c) = cut_at {
            chunk.push_str(&format!("... cut at L{} to fit the tool-result limit; the rest: mt expand {}:L{}-{}\n", c + 1, t.path, c + 1, e + 1));
        } else {
            keys.push(format!("{}|{}|{}|{}", t.path, s, e, h));
            ranges.push((t.path.clone(), file_hash(&lines), s, e));
        }
        body.push_str(&chunk);
        shown += 1;
    }
    let mut out = format!("# mt expand: {shown} span(s), ~{} tokens{}\n", body.len() / 4, if from_mem > 0 { format!(", {from_mem} from memory") } else { String::new() });
    out.push_str(&body);
    if !left.is_empty() { out.push_str(&format!("## not expanded (output limit): mt expand {}\n", left.join(" "))); }
    for e in &errs { out.push_str(&format!("## {e}\n")); }
    if !skipped.is_empty() { out.push_str(&format!("## ignored: {}\n", skipped.join(" "))); }
    if targets.is_empty() && errs.is_empty() { out.push_str("## nothing to expand: give pointer numbers from the last reply, or path:Ls-e@hash\n"); }
    // expanded bodies count as shown: a later query points at them instead of repeating them
    if let Some(p) = &sess {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(p) { use std::io::Write; for k in keys { let _ = writeln!(f, "{k}"); } }
    }
    record_shown(sess.as_deref(), &ranges);
    (errs.is_empty() && shown > 0, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(path: &str, name: &str, s: usize, e: usize) -> Hit {
        Hit { path: path.into(), role: "source".into(), start: s, end: e, kind: "function".into(), name: name.into(), lines: vec![], hop: 1 }
    }

    #[test]
    fn expand_pointers() {
        let dir = std::env::temp_dir().join(format!("mt-expand-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = "package a\n\nfunc One() {\n\treturn\n}\n\nfunc Two() int {\n\treturn 2\n}\n";
        std::fs::write(dir.join("a.go"), src).unwrap();
        let lines: Vec<String> = src.lines().map(String::from).collect();
        let sid = format!("expand-test-{}", std::process::id());
        let sess = session_file(Some(&sid)).unwrap();
        let h2 = span_hash(&lines, 6, 8);
        save_pointers(Some(&sess), &[pointer_row(1, "a.go", 2, 4, "function", "One", &span_hash(&lines, 2, 4)), pointer_row(2, "a.go", 6, 8, "function", "Two", &h2)]);

        // by number
        let (ok, out) = expand(&dir, &["2".into()], Some(&sid));
        assert!(ok, "{out}");
        assert!(out.contains("[2] a.go:L7-9 function Two h=") && out.contains("8  \treturn 2"), "{out}");
        // a pasted pointer line is one item, not two
        let (ok, out) = expand(&dir, &[format!("[2] a.go:L7-9 function Two h={h2}")], Some(&sid));
        assert!(ok && out.contains("1 span(s)"), "{out}");
        // an explicit handle with a hash
        let (ok, out) = expand(&dir, &[format!("a.go:L3-5@{}", span_hash(&lines, 2, 4))], Some(&sid));
        assert!(ok && out.contains("func One()") && !out.contains("CHANGED"), "{out}");
        // the file changed underneath: the number still finds Two, moved down two lines
        std::fs::write(dir.join("a.go"), src.replace("package a\n", "package a\n\n// x\n")).unwrap();
        let (ok, out) = expand(&dir, &["2".into()], Some(&sid));
        assert!(ok && out.contains("a.go:L9-11") && out.contains("MOVED since the pointer (was L7-9), text unchanged"), "{out}");
        // its body edited too: CHANGED, with the new text
        std::fs::write(dir.join("a.go"), src.replace("package a\n", "package a\n\n// x\n").replace("return 2", "return 3")).unwrap();
        let (ok, out) = expand(&dir, &["2".into()], Some(&sid));
        assert!(ok && out.contains("CHANGED since the pointer (was L7-9") && out.contains("return 3"), "{out}");
        // an unknown number is an error that says what to do instead
        let (ok, out) = expand(&dir, &["7".into()], Some(&sid));
        assert!(!ok && out.contains("no item [7]"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(sess.with_extension("ptrs"));
        let _ = std::fs::remove_file(&sess);
    }

    #[test]
    fn expect_matching() {
        let a = h("internal/api/audit.go", "Server.auditWrite", 40, 55);
        assert!(matches("auditWrite", &a));
        assert!(matches("Server.auditWrite", &a));
        assert!(matches("audit.go", &a));
        assert!(matches("api/audit.go:auditWrite", &a));
        assert!(matches("audit.go:L42", &a));
        assert!(matches("audit.go:56", &a));
        assert!(!matches("audit.go:L57", &a));
        assert!(!matches("write", &a));
        assert!(!matches("dit.go", &a));
    }

    #[test]
    fn exhaustive_claims() {
        let hits = vec![h("a.go", "One", 0, 5), h("b.go", "Two", 0, 5), h("c.go", "Three", 0, 5)];
        let mut r = Res { id: "q1".into(), verdict: Verdict::Answer, head: String::new(), how: String::new(), notes: vec![], spans: vec![], items: vec![], names: vec![] };
        judge_list(&mut r, &hits, &["One".into(), "Two".into()], false, "sites");
        assert!(r.verdict == Verdict::Pass);
        let mut r2 = Res { id: "q2".into(), ..r };
        r2.items.clear();
        judge_list(&mut r2, &hits, &["One".into(), "Two".into()], true, "sites");
        assert!(r2.verdict == Verdict::Fail);
        assert_eq!(r2.items.len(), 1);
        assert_eq!(r2.items[0].name, "Three");
    }
}
