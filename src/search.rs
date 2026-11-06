//! Plain-word search over code: "how is an audit row written" finds `Store.WriteAudit`,
//! `Server.auditWrite`, … without knowing any name. Every definition is a document: its name split
//! into words (`WriteAudit` → write, audit; `user_id` → user, id), its scope, its path, its doc
//! comment and every word in its body (identifiers split the same way, strings and comments too).
//! Ranked by BM25, with a small boost for how much of the repo mentions it (coupling) and less
//! weight for tests and docs unless the question is about them. Deterministic.
//!
//! The daemon keeps each file's documents per file version (`precache`), so a question costs a
//! ranking pass, not a re-read of the repo.

use crate::index::{DefRow, Store};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

const STOP: &[&str] = &[
    "the", "a", "an", "of", "in", "on", "for", "to", "which", "what", "how", "is", "are", "was", "were", "be", "been", "it", "its",
    "this", "that", "these", "those", "and", "or", "from", "by", "with", "where", "when", "does", "do", "did", "show", "me", "find", "get",
    "all", "any", "there", "who", "whom", "why", "into", "about", "can", "should", "would", "will", "has", "have", "having", "func", "fn",
    "function", "method", "code", "return", "returns", "let", "var", "const", "self", "this", "nil", "null", "none", "true", "false", "err",
];

/// Irregular forms a question uses and code doesn't ("written" vs `Write`).
fn irregular(w: &str) -> &str {
    match w { "written" | "wrote" | "writes" | "writing" => "write", "read" | "reads" | "reading" => "read", "sent" | "sends" | "sending" => "send",
        "built" | "builds" | "building" => "build", "ran" | "runs" | "running" => "run", "made" | "makes" | "making" => "make",
        "stored" | "stores" | "storing" => "store", "deleted" | "deletes" | "deleting" | "removed" | "removes" => "delete",
        "created" | "creates" | "creating" => "create", "updated" | "updates" | "updating" => "update", "logged" | "logs" | "logging" => "log",
        "authenticated" | "authenticates" | "authentication" | "auth" => "auth", "validated" | "validates" | "validation" => "validate",
        // the abbreviations code habitually uses for the words a question spells out
        "organization" | "organizations" | "organisation" | "organisations" | "orgs" => "org",
        "configuration" | "configurations" | "configure" | "configured" | "cfg" => "config",
        "repository" | "repositories" | "repos" => "repo", "message" | "messages" | "msgs" => "msg",
        "database" | "databases" => "db", "parameter" | "parameters" | "params" => "param",
        "administrator" | "administrators" | "admins" => "admin", "directory" | "directories" | "dirs" => "dir",
        other => other }
}

fn stem(w: &str) -> String {
    let w = irregular(w);
    for suf in ["ations", "ation", "ings", "ing", "ers", "er", "ies", "ed", "es", "s"] {
        if w.len() > suf.len() + 3 {
            if let Some(s) = w.strip_suffix(suf) { return if suf == "ies" { format!("{s}y") } else { s.to_string() }; }
        }
    }
    w.to_string()
}

/// Words of text, code-aware: `WriteAudit` → write, audit; `user_id` → user, id; `HTTPServer` →
/// http, server. Lowercased, stemmed, stop words dropped.
pub fn words(text: &str) -> Vec<String> {
    let mut out = vec![];
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() { continue; }
        let cs: Vec<char> = raw.chars().collect();
        let mut start = 0;
        for i in 1..=cs.len() {
            let split = i == cs.len()
                || (cs[i].is_uppercase() && cs[i - 1].is_lowercase())
                || (cs[i].is_uppercase() && i + 1 < cs.len() && cs[i + 1].is_lowercase() && cs[i - 1].is_uppercase())
                || (cs[i].is_ascii_digit() != cs[i - 1].is_ascii_digit());
            if split {
                let w: String = cs[start..i].iter().collect::<String>().to_lowercase();
                start = i;
                if w.len() < 2 || w.chars().all(|c| c.is_ascii_digit()) || STOP.contains(&w.as_str()) { continue; }
                out.push(stem(&w));
            }
        }
    }
    out
}

/// One definition, as the ranking sees it.
#[derive(Clone)]
struct Doc {
    def: DefRow,
    /// term -> weighted count (name words weigh most)
    tf: HashMap<String, f32>,
    len: f32,
}

fn doc_for(d: &DefRow, lines: &[String], container: bool) -> Doc {
    let mut tf: HashMap<String, f32> = HashMap::new();
    let mut add = |t: String, w: f32| *tf.entry(t).or_default() += w;
    for t in words(&d.name) { add(t, 4.0); }
    for t in words(&d.scope) { add(t, 1.5); }
    let path_words: HashSet<String> = words(&d.path.replace(['/', '.'], " ")).into_iter().collect();
    for t in path_words { add(t, 1.0); }
    let e = d.end.min(lines.len().saturating_sub(1));
    // a container (a class or impl holding other definitions) is its header and doc only: its
    // members are documents of their own
    let last = if container { (d.head + 2).min(e) } else { e };
    for l in lines.iter().take(last + 1).skip(d.start).take(600) {
        for t in words(l) { add(t, 1.0); }
    }
    let len = tf.values().sum::<f32>().max(1.0);
    Doc { def: d.clone(), tf, len }
}

/// path -> (version stamp, its documents)
fn cache() -> &'static Mutex<HashMap<String, ((u128, u64), Arc<Vec<Doc>>)>> {
    static C: OnceLock<Mutex<HashMap<String, ((u128, u64), Arc<Vec<Doc>>)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn stamp(root: &std::path::Path, rel: &str) -> Option<(u128, u64)> {
    let m = std::fs::metadata(root.join(rel)).ok()?;
    Some((m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos(), m.len()))
}

fn docs_of<S: Store>(st: &S, rel: &str) -> Arc<Vec<Doc>> {
    let root = st.root();
    let st_now = stamp(root, rel);
    if crate::precache::enabled() {
        if let (Some(s), Some((cs, d))) = (st_now, cache().lock().unwrap_or_else(|e| e.into_inner()).get(rel)) {
            if *cs == s { return d.clone(); }
        }
    }
    let Some(text) = crate::texts::get(root, rel) else { return Arc::new(vec![]) };
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let defs = st.defs_in(rel);
    let inside_fn = |d: &DefRow| defs.iter().any(|o| crate::context::FUNC_KINDS.contains(&o.kind.as_str()) && o.start <= d.start && d.end <= o.end && (o.start, o.end) != (d.start, d.end));
    let docs: Vec<Doc> = defs.iter()
        // a function's own locals (`var out`, `e := …`) are not what anyone searches for
        .filter(|d| !inside_fn(d))
        .map(|d| {
            let container = defs.iter().any(|o| o.start > d.start && o.end <= d.end && (o.start, o.end) != (d.start, d.end));
            doc_for(d, &lines, container)
        }).collect();
    let docs = Arc::new(docs);
    if crate::precache::enabled() { if let Some(s) = st_now { cache().lock().unwrap_or_else(|e| e.into_inner()).insert(rel.to_string(), (s, docs.clone())); } }
    docs
}

pub struct Found {
    pub def: DefRow,
    pub score: f64,
    /// the question's words this definition matched
    pub matched: Vec<String>,
}

/// The definitions that best answer `question`, best first.
pub fn search<S: Store>(st: &S, question: &str, limit: usize) -> Vec<Found> {
    let base: Vec<String> = words(question);
    // adjacent words also as one: "kill switch" finds `Killswitch`, "time out" `timeout`
    let joined: Vec<String> = base.windows(2).map(|w| format!("{}{}", w[0], w[1])).collect();
    let mut q: Vec<String> = base.into_iter().chain(joined).collect();
    q.sort();
    q.dedup();
    if q.is_empty() { return vec![]; }
    let about_tests = q.iter().any(|t| matches!(t.as_str(), "test" | "spec" | "fixture" | "mock" | "fake"));
    let about_docs = q.iter().any(|t| matches!(t.as_str(), "doc" | "readme" | "guide"));
    // every file's documents (cached per file version in the daemon)
    let files: Vec<(String, String)> = st.text_files();
    let all: Vec<Arc<Vec<Doc>>> = files.iter().filter(|(_, role)| role != "config").map(|(p, _)| docs_of(st, p)).collect();
    let docs: Vec<&Doc> = all.iter().flat_map(|v| v.iter()).collect();
    if docs.is_empty() { return vec![]; }
    let n = docs.len() as f64;
    let avg = docs.iter().map(|d| d.len as f64).sum::<f64>() / n;
    let df: Vec<f64> = q.iter().map(|t| docs.iter().filter(|d| d.tf.contains_key(t)).count() as f64).collect();
    // coupling: how many documents mention a definition's own name (its callers, users, tests)
    let mut scored: Vec<(usize, f64, Vec<String>)> = vec![];
    for (i, d) in docs.iter().enumerate() {
        let mut s = 0.0;
        let mut hit = vec![];
        for (t, &f) in q.iter().zip(&df) {
            let Some(&tf) = d.tf.get(t) else { continue };
            let tf = tf as f64;
            let idf = ((n - f + 0.5) / (f + 0.5) + 1.0).ln();
            s += idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * d.len as f64 / avg.max(1.0)));
            hit.push(t.clone());
        }
        if s <= 0.0 { continue; }
        // a definition matching more of the question's words beats one repeating a single word
        s *= 0.5 + 0.5 * (hit.len() as f64 / q.len() as f64);
        // and the question's words in its name say most about what it is
        let name_words: HashSet<String> = words(&d.def.name).into_iter().collect();
        let in_name = q.iter().filter(|t| name_words.contains(*t)).count();
        s *= 1.0 + 0.4 * in_name as f64;
        s *= match d.def.role.as_str() {
            "test" | "integration" if !about_tests => 0.5,
            "doc" if !about_docs => 0.35,
            _ => 1.0,
        };
        scored.push((i, s, hit));
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(docs[a.0].def.path.cmp(&docs[b.0].def.path)).then(docs[a.0].def.start.cmp(&docs[b.0].def.start)));
    // the top candidates get the coupling boost (counting mentions for every document is not needed)
    let mut top: Vec<(usize, f64, Vec<String>)> = scored.into_iter().take(limit * 4).collect();
    for x in top.iter_mut() {
        let last = crate::context::last_seg(&docs[x.0].def.name);
        let users = st.refs(last).len() as f64;
        x.1 *= 1.0 + 0.12 * (1.0 + users).ln();
    }
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(docs[a.0].def.path.cmp(&docs[b.0].def.path)).then(docs[a.0].def.start.cmp(&docs[b.0].def.start)));
    top.into_iter().take(limit).map(|(i, s, m)| Found { def: docs[i].def.clone(), score: s, matched: m }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_words() {
        assert_eq!(words("WriteAudit"), vec!["write", "audit"]);
        // questions and code are stemmed alike ("server" and "servers" both become "serv")
        assert_eq!(words("user_id HTTPServer"), vec!["user", "id", "http", "serv"]);
        assert_eq!(words("how is an audit row written?"), vec!["audit", "row", "write"]);
        assert_eq!(words("validates the session tokens"), vec!["validate", "session", "token"]);
        assert_eq!(words("organization configuration"), vec!["org", "config"]);
    }
}
