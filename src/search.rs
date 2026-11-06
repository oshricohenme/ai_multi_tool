//! Plain-word search over code: "how is an audit row written" finds `Store.WriteAudit`,
//! `Server.auditWrite`, … without knowing any name. Every definition is a document: its name split
//! into words (`WriteAudit` → write, audit; `user_id` → user, id), its scope, its path, its doc
//! comment and every word in its body (identifiers split the same way, strings and comments too).
//! Ranked by BM25, with a small boost for how much of the repo mentions it (coupling) and less
//! weight for tests and docs unless the question is about them. Deterministic.
//!
//! Each definition's weighted words are computed when its file is indexed and kept on disk, in
//! the index (`search_post`, `search_doc`): a question reads the postings of its own words only,
//! so its cost and memory follow the question, not the size of the repo.

use crate::index::{DefRow, Store};
use std::collections::{HashMap, HashSet};

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

/// A definition's searchable words, each with its weight (name words weigh most), and the
/// document's length: what the ranking sees of it. `container`: a class or impl holding other
/// definitions is its header and doc only (its members are documents of their own).
#[allow(clippy::too_many_arguments)]
pub fn terms(name: &str, scope: &str, path: &str, start: usize, end: usize, head: usize, container: bool, lines: &[&str]) -> (Vec<(String, f32)>, f32) {
    let mut tf: HashMap<String, f32> = HashMap::new();
    let mut add = |t: String, w: f32| *tf.entry(t).or_default() += w;
    for t in words(name) { add(t, 4.0); }
    for t in words(scope) { add(t, 1.5); }
    let path_words: HashSet<String> = words(&path.replace(['/', '.'], " ")).into_iter().collect();
    for t in path_words { add(t, 1.0); }
    let e = end.min(lines.len().saturating_sub(1));
    let last = if container { (head + 2).min(e) } else { e };
    for l in lines.iter().take(last + 1).skip(start).take(600) {
        for t in words(l) { add(t, 1.0); }
    }
    let len = tf.values().sum::<f32>().max(1.0);
    let mut v: Vec<(String, f32)> = tf.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    (v, len)
}

/// Every definition of a file as a search document, aligned with `defs` (at index time). None:
/// not searched (a function's own locals, `var out`, `e := …`; anything in a config file).
pub fn postings_for(rel: &str, role: &str, defs: &[(crate::Def, String)], lines: &[&str]) -> Vec<Option<(Vec<(String, f32)>, f32)>> {
    if role == "config" { return defs.iter().map(|_| None).collect(); }
    defs.iter().map(|(d, _)| {
        let inside_fn = defs.iter().any(|(o, _)| crate::context::FUNC_KINDS.contains(&o.kind.as_str()) && o.start <= d.start && d.end <= o.end && (o.start, o.end) != (d.start, d.end));
        if inside_fn { return None; }
        let container = defs.iter().any(|(o, _)| o.start > d.start && o.end <= d.end && (o.start, o.end) != (d.start, d.end));
        Some(terms(&d.name, &d.path.join("."), rel, d.start, d.end, d.head, container, lines))
    }).collect()
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
    let (n, avg) = st.search_stats();
    if n == 0.0 { return vec![]; }
    // BM25 over the postings of the question's words only: per definition, its sum and the words it matched
    let mut acc: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
    for t in &q {
        let post = st.postings(t);
        let f = post.len() as f64;
        if f == 0.0 { continue; }
        let idf = ((n - f + 0.5) / (f + 0.5) + 1.0).ln();
        for (def, tf, len) in post {
            let tf = tf as f64;
            let e = acc.entry(def).or_default();
            e.0 += idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * len as f64 / avg.max(1.0)));
            e.1.push(t.clone());
        }
    }
    let qn = q.len() as f64;
    // a definition matching more of the question's words beats one repeating a single word
    let mut pre: Vec<(i64, f64, Vec<String>)> = acc.into_iter().filter(|(_, (s, _))| *s > 0.0)
        .map(|(d, (s, h))| { let s = s * (0.5 + 0.5 * (h.len() as f64 / qn)); (d, s, h) }).collect();
    // The name and role factors need the definition itself: the best by the text score get them.
    // ponytail: a definition below this cut that the name factor (at most 1 + 0.4 per word) would
    // lift into the top is missed; raise the cut if that ever shows up.
    pre.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    pre.truncate((limit * 50).max(500));
    let ids: Vec<i64> = pre.iter().map(|x| x.0).collect();
    let defs: HashMap<i64, DefRow> = st.defs_by_ids(&ids).into_iter().map(|d| (d.id, d)).collect();
    let mut scored: Vec<(DefRow, f64, Vec<String>)> = vec![];
    for (id, mut s, hit) in pre {
        let Some(d) = defs.get(&id) else { continue };
        // the question's words in its name say most about what it is
        let name_words: HashSet<String> = words(&d.name).into_iter().collect();
        let in_name = q.iter().filter(|t| name_words.contains(*t)).count();
        s *= 1.0 + 0.4 * in_name as f64;
        s *= match d.role.as_str() {
            "test" | "integration" if !about_tests => 0.5,
            "doc" if !about_docs => 0.35,
            _ => 1.0,
        };
        scored.push((d.clone(), s, hit));
    }
    let order = |a: &(DefRow, f64, Vec<String>), b: &(DefRow, f64, Vec<String>)| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.path.cmp(&b.0.path)).then(a.0.start.cmp(&b.0.start));
    scored.sort_by(order);
    // the top candidates get the coupling boost (counting mentions for every document is not needed)
    let mut top: Vec<(DefRow, f64, Vec<String>)> = scored.into_iter().take(limit * 4).collect();
    for x in top.iter_mut() {
        let last = crate::context::last_seg(&x.0.name);
        let users = st.refs(last).len() as f64;
        x.1 *= 1.0 + 0.12 * (1.0 + users).ln();
    }
    top.sort_by(order);
    top.into_iter().take(limit).map(|(d, s, m)| Found { def: d, score: s, matched: m }).collect()
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
