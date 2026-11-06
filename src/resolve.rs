//! Which definition does a reference reach? mt's references are found by name, which is
//! complete but can't tell same-named definitions apart: `subscribe` in a TypeScript client, in
//! a Go package and in a test helper are three different things. This module attributes each
//! reference site to the definition it most likely reaches, with evidence, so `callers` (and
//! the hops after it) count only what really calls the target, and every answer says how sure
//! it is. Nothing here knows a language: it uses the grammar table (which language family a
//! file is), file roles (test or not), paths, and the words a file contains.
//!
//! Unlike a resolver that drops what it can't prove (and so loses calls through an interface or
//! a same-named test fake), an undecidable site is kept and labelled ambiguous.

use crate::index::DefRow;
use crate::languages::{detect, Tier};
use std::collections::HashSet;
use std::path::Path;

/// How sure mt is that a site reaches the target.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Conf {
    /// the only definition of the name the site can reach, or one in its own file, or one whose
    /// directory or module its file names (an import, a package qualifier)
    Exact,
    /// the nearest of several by path
    Likely,
    /// several definitions are equally near; the site is kept, not guessed
    Ambiguous,
}

impl Conf {
    pub fn label(self) -> &'static str {
        match self { Conf::Exact => "resolved", Conf::Likely => "nearest definition", Conf::Ambiguous => "ambiguous: several definitions equally near" }
    }
}

/// The language family of a file, from the grammar table: code in one family can't call
/// code in another. TypeScript, TSX and JavaScript are one family; C, C++ and Objective-C one.
/// None (an unknown kind of file) never filters: no data is not evidence of a mismatch.
pub fn family(path: &str) -> Option<&'static str> {
    match detect(Path::new(path), "") {
        Tier::TreeSitter(l) => Some(match l.id {
            "typescript" | "tsx" | "javascript" => "js",
            "c" | "cpp" | "objc" => "c",
            "json" | "yaml" | "toml" | "xml" | "html" | "css" => "data",
            other => other,
        }),
        _ => None,
    }
}

/// A line with its string literals blanked and its line comment removed. Lexical, for the
/// common syntaxes: `//`, a leading `#` (not `#[`), `--` at the start, `/* … */` on the line,
/// and "…", '…', `…` strings (escapes honoured; an unclosed ' is a char or lifetime, kept).
pub fn strip_code(line: &str) -> String {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let lead = |i: usize| b[..i].iter().all(|c| c.is_whitespace());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == '/' && next == Some('/') { break; }
        if c == '/' && next == Some('*') {
            match (i + 2..b.len().saturating_sub(1)).find(|&j| b[j] == '*' && b[j + 1] == '/') {
                Some(j) => { i = j + 2; out.push(' '); continue; }
                None => break,
            }
        }
        if c == '#' && lead(i) && next != Some('[') && next != Some('!') && next != Some('i') && next != Some('d') { break; }
        if c == '-' && next == Some('-') && lead(i) { break; }
        if c == '"' || c == '`' || c == '\'' {
            let mut j = i + 1;
            while j < b.len() && b[j] != c { if b[j] == '\\' { j += 1; } j += 1; }
            if j < b.len() { out.push(' '); i = j + 1; continue; }
            if c != '\'' { break; } // an unclosed "…" or `…` runs to the end of the line
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Is line `l0` inside a `/* … */` block that started on an earlier line?
fn in_block_comment(lines: &[String], l0: usize) -> bool {
    let mut open = false;
    for l in lines.iter().take(l0) {
        let s = strip_strings_only(l);
        let mut rest = s.as_str();
        loop {
            if open {
                match rest.find("*/") { Some(k) => { open = false; rest = &rest[k + 2..]; } None => break }
            } else {
                let line_comment = rest.find("//");
                match rest.find("/*") {
                    Some(k) if line_comment.is_none_or(|lc| k < lc) => { open = true; rest = &rest[k + 2..]; }
                    _ => break,
                }
            }
        }
    }
    open
}

fn strip_strings_only(line: &str) -> String {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '"' || c == '`' {
            let mut j = i + 1;
            while j < b.len() && b[j] != c { if b[j] == '\\' { j += 1; } j += 1; }
            i = j + 1;
            out.push(' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Does line `l0` use `name` in code (not only in a comment or a string)?
pub fn in_code(lines: &[String], l0: usize, name: &str) -> bool {
    let Some(line) = lines.get(l0) else { return false };
    if in_block_comment(lines, l0) {
        // the block may close on this line: only what follows `*/` is code
        let Some(k) = line.find("*/") else { return false };
        return has_word(&strip_code(&line[k + 2..]), name);
    }
    has_word(&strip_code(line), name)
}

fn has_word(s: &str, w: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    s.match_indices(w).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + w.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

fn dir(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// How near a definition is to a site: its own file, then a directory or module the file names,
/// then the same directory, then the longest shared path.
fn nearness(site: &str, d: &DefRow, words: &HashSet<String>) -> u32 {
    if site == d.path { return 100; }
    let names_it = crate::context::reachable(site, &d.path, words);
    if dir(site) == dir(&d.path) { return 70; }
    if names_it { return 60; }
    let shared = site.split('/').zip(d.path.split('/')).take_while(|(a, b)| a == b).count() as u32;
    (shared * 5).min(50)
}

/// Does a site in `site_path` (role `site_role`, words `words`) reach one of `targets`, given
/// every definition `cands` sharing the name? None: it reaches another definition (or none in
/// reach). Some(conf): it reaches a target, this sure.
pub fn attribute(site_path: &str, site_role: &str, words: &HashSet<String>, cands: &[DefRow], targets: &[DefRow]) -> Option<Conf> {
    let fam = family(site_path);
    let same = |a: &DefRow, b: &DefRow| a.path == b.path && a.start == b.start;
    let mut c: Vec<&DefRow> = cands.iter().filter(|d| fam.is_none() || family(&d.path).is_none() || family(&d.path) == fam).collect();
    // code that ships never runs a test's definitions (a fake, a helper): those reach only tests
    let test = |r: &str| matches!(r, "test" | "integration");
    if !test(site_role) {
        let shipped: Vec<&DefRow> = c.iter().copied().filter(|d| !test(&d.role)).collect();
        if !shipped.is_empty() { c = shipped; }
    }
    if c.is_empty() { return None; }
    if !targets.iter().any(|t| c.iter().any(|d| same(d, t))) { return None; }
    let scored: Vec<(&DefRow, u32)> = c.iter().map(|d| (*d, nearness(site_path, d, words))).collect();
    let best = scored.iter().map(|x| x.1).max().unwrap_or(0);
    let top: Vec<&DefRow> = scored.iter().filter(|x| x.1 == best).map(|x| x.0).collect();
    if !top.iter().any(|d| targets.iter().any(|t| same(d, t))) { return None; }
    Some(if c.len() == 1 || (top.len() == 1 && best >= 60) { Conf::Exact } else if top.len() == 1 { Conf::Likely } else { Conf::Ambiguous })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(path: &str, name: &str, role: &str) -> DefRow {
        DefRow { id: 0, path: path.into(), role: role.into(), kind: "function".into(), name: name.into(), scope: String::new(), start: 10, end: 20, head: 10, sig: String::new() }
    }

    #[test]
    fn code_versus_comments_and_strings() {
        let l = |s: &str| s.to_string();
        let lines = vec![l("x := subscribe(c) // subscribe here"), l("// subscribe in a comment"), l(r#"send({type: "subscribe"})"#), l("/* start"), l("  subscribe() inside"), l("*/ subscribe()"), l("# subscribe in shell"), l("#[derive(subscribe)]")];
        assert!(in_code(&lines, 0, "subscribe"));
        assert!(!in_code(&lines, 1, "subscribe"));
        assert!(!in_code(&lines, 2, "subscribe"));
        assert!(!in_code(&lines, 4, "subscribe"));
        assert!(in_code(&lines, 5, "subscribe"));
        assert!(!in_code(&lines, 6, "subscribe"));
        assert!(in_code(&lines, 7, "subscribe"));
        assert!(!in_code(&lines, 0, "subscri"));
    }

    #[test]
    fn attribution() {
        let go = def("internal/realtime/serve.go", "Client.subscribe", "source");
        let helper = def("internal/bus/bustest/conformance.go", "subscribe", "source");
        let ts = def("web/src/api/ws.ts", "subscribe", "source");
        let all = vec![go.clone(), helper.clone(), ts.clone()];
        let none = HashSet::new();
        // another language never reaches the Go method
        assert_eq!(attribute("web/src/hooks/useLive.ts", "source", &none, &all, &[go.clone()]), None);
        // a file defining its own `subscribe` calls that one
        assert_eq!(attribute("internal/bus/bustest/conformance.go", "source", &none, &all, &[go.clone()]), None);
        // in the method's own package: resolved
        assert_eq!(attribute("internal/realtime/client.go", "source", &none, &all, &[go.clone()]), Some(Conf::Exact));
        // shipped code never resolves to a test's fake, even in the same directory
        let real = def("internal/store/audit.go", "Store.WriteAudit", "source");
        let fake = def("internal/api/fake_test.go", "fakeStore.WriteAudit", "test");
        let w = vec![real.clone(), fake.clone()];
        assert_eq!(attribute("internal/api/server.go", "source", &none, &w, &[real.clone()]), Some(Conf::Exact));
        assert_eq!(attribute("internal/api/server.go", "source", &none, &w, &[fake.clone()]), None);
        // a test sees both; its own directory's fake is nearer
        assert_eq!(attribute("internal/api/routes_test.go", "test", &none, &w, &[fake.clone()]), Some(Conf::Exact));
        // two equally near definitions: kept, labelled ambiguous
        let a = def("pkg/a/x.go", "Run", "source");
        let b = def("pkg/b/x.go", "Run", "source");
        assert_eq!(attribute("cmd/main.go", "source", &none, &[a.clone(), b.clone()], &[a.clone()]), Some(Conf::Ambiguous));
        // the file names the definition's directory (an import): resolved
        let words: HashSet<String> = ["a".to_string()].into();
        assert_eq!(attribute("cmd/main.go", "source", &words, &[a.clone(), b], &[a]), Some(Conf::Exact));
    }
}
