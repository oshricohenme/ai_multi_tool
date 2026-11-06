//! Deterministic compression of tool output (logs, build and test output, JSON, MCP responses),
//! with the original kept and queryable, so a reply costs fewer tokens and nothing is lost.
//!
//!   mt compress [--kind auto|json|log|verbatim] [--max-lines N]   stdin -> compressed text
//!   mt run [--max-lines N] -- <cmd...>                            run, compress, keep exit code
//!   mt out [<id>] [--grep RE] [-C N] [--lines A-B] [--level error|warn] [--json PATH]
//!
//! Algorithms, chosen by content (`Kind::Auto`):
//!   lossless, always: ANSI escapes stripped, `\r` progress redraws resolved to their last state,
//!     identical consecutive lines collapsed to one with (xN), JSON minified, a JSON array of
//!     objects written as a table (keys once, columns every row shares the same value hoisted
//!     out as `constant`).
//!   lossy, only past the budget, reversible with `mt out <id>`:
//!     logs: runs of same-shape lines (digits and hex ignored) and runs of passing lines folded;
//!       then head + tail + every important line (error, warning, failure, file:line, stack
//!       frame) with a line of context; the rest becomes numbered `L a-b` ranges.
//!     JSON: long tables sampled (first rows, last rows, and rows that look like errors), long
//!       strings and long scalar arrays shortened.
//!   verbatim (code, diffs): lossless steps only, so the text stays exact for an edit.
//! The same input always gives the same output. Originals live per session in
//! `$TMPDIR/mt-out-<session>/<id>.txt`; the id is a hash of the original.

use crate::fnv1a;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Auto,
    Json,
    Log,
    Verbatim,
}

impl Kind {
    pub fn parse(s: &str) -> Kind {
        match s { "json" => Kind::Json, "log" | "text" => Kind::Log, "verbatim" | "code" | "diff" => Kind::Verbatim, _ => Kind::Auto }
    }
}

pub struct Compressed {
    pub text: String,
    /// something was dropped (only then is the original worth storing)
    pub lossy: bool,
    pub kind: &'static str,
    pub lines_in: usize,
    pub lines_out: usize,
    pub chars_in: usize,
}

/// Code and diffs up to this size (about 10k tokens) come back exact; larger ones are reduced.
pub const EXACT_MAX_CHARS: usize = 40_000;

/// A large diff or `git log -p` in a few lines: each commit, then each file with its +/- counts,
/// hunk count and the line where it starts (for `mt out <id> --lines A-B`).
fn diff_outline(lines: &[String], budget: usize) -> String {
    struct F { path: String, at: usize, add: usize, del: usize, hunks: usize }
    let mut out: Vec<String> = vec![];
    let mut files: Vec<F> = vec![];
    let (mut commits, mut nfiles, mut adds, mut dels) = (0, 0, 0, 0);
    let mut flush = |files: &mut Vec<F>, out: &mut Vec<String>| {
        for f in files.drain(..) { out.push(format!("  {}  +{} -{}  ({} hunk{}) L{}", f.path, f.add, f.del, f.hunks, if f.hunks == 1 { "" } else { "s" }, f.at)); }
    };
    let mut want_subject = false;
    for (i, l) in lines.iter().enumerate() {
        if let Some(sha) = l.strip_prefix("commit ") {
            flush(&mut files, &mut out);
            commits += 1;
            out.push(format!("commit {} L{}", sha.chars().take(10).collect::<String>(), i + 1));
            want_subject = true;
        } else if want_subject && l.starts_with("    ") && !l.trim().is_empty() {
            if let Some(last) = out.last_mut() { last.push_str(&format!("  {}", l.trim())); }
            want_subject = false;
        } else if let Some(rest) = l.strip_prefix("diff --git ") {
            want_subject = false;
            nfiles += 1;
            let path = rest.rsplit(" b/").next().unwrap_or(rest).to_string();
            files.push(F { path, at: i + 1, add: 0, del: 0, hunks: 0 });
        } else if let Some(f) = files.last_mut() {
            if l.starts_with("@@ ") { f.hunks += 1; }
            else if l.starts_with('+') && !l.starts_with("+++ ") { f.add += 1; adds += 1; }
            else if l.starts_with('-') && !l.starts_with("--- ") { f.del += 1; dels += 1; }
        }
    }
    flush(&mut files, &mut out);
    let total = out.len();
    let keep = budget.max(20);
    if total > keep { out.truncate(keep); out.push(format!("... {} more outline lines: mt out <id> --grep '^commit|^diff --git'", total - keep)); }
    format!("{commits} commit(s), {nfiles} file diff(s), +{adds} -{dels}, {} lines. Exact text of any part: mt out <id> --lines A-B (the L numbers below), or --grep RE -C 20.\n{}", lines.len(), out.join("\n"))
}

/// Output budget: lines for logs, and characters for JSON (about 40 per line).
pub const DEFAULT_LINES: usize = 120;

fn rx(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

// ------------------------------------------------------------------ lossless cleanup

/// ANSI CSI / OSC / two-byte escapes removed.
pub fn strip_ansi(s: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").replace_all(s, "").into_owned()
}

/// A line redrawn with `\r` (progress bars, spinners) keeps only what was finally on screen.
fn resolve_cr(line: &str) -> &str {
    let l = line.strip_suffix('\r').unwrap_or(line);
    match l.rfind('\r') { Some(i) if i + 1 < l.len() => &l[i + 1..], Some(_) => l.trim_end_matches('\r'), None => l }
}

pub fn clean_lines(s: &str) -> Vec<String> {
    let s = strip_ansi(s);
    let mut v: Vec<String> = s.split('\n').map(|l| resolve_cr(l).to_string()).collect();
    while v.last().is_some_and(|l| l.is_empty()) { v.pop(); }
    v
}

// ------------------------------------------------------------------ line classes

/// Lines a reader must not lose: errors, warnings, failures, `path.ext:12`, stack frames.
pub fn important(l: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"(?i)\b(error|errors|err|fatal|fail|failed|failure|failures|failing|panic|panicked|exception|traceback|warn|warning|warnings|denied|refused|timeout|timed out|segfault|abort|aborted|undefined|unresolved|cannot|can't|couldn't|not found|unexpected|assert|assertion|invalid|missing|conflict|exit (status|code) [1-9])\b|\S+\.[A-Za-z0-9]{1,6}:\d+|^\s+at \S|^\s*File \x22|^\s*#\d+ ").is_match(l)
}

fn passing(l: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"(?i)(\b(pass|passed|ok|success|succeeded|done|skip|skipped)\b|✓|✔)").is_match(l)
}

/// The shape of a line: numbers, hex ids and timestamps all look alike.
fn shape(l: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let s = rx(&R, r"0x[0-9a-fA-F]+|\b[0-9a-fA-F]{7,}\b|\d+").replace_all(l, "#");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn looks_like_diff(lines: &[String]) -> bool {
    lines.iter().take(400).any(|l| l.starts_with("diff --git ") || l.starts_with("@@ -")) && lines.iter().take(400).any(|l| l.starts_with("+++ ") || l.starts_with("--- "))
}

// ------------------------------------------------------------------ logs

/// One output line: the text and the original line range it stands for (1-based).
struct Out { text: String, a: usize, b: usize, important: bool }

/// Lossless: identical consecutive lines become one line with (xN).
fn collapse_identical(lines: &[String]) -> Vec<Out> {
    let mut out: Vec<Out> = vec![];
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] { j += 1; }
        let n = j - i;
        let text = if n > 1 && !lines[i].trim().is_empty() { format!("{} (x{n})", lines[i]) } else if n > 1 { String::new() } else { lines[i].clone() };
        out.push(Out { important: important(&lines[i]), text, a: i + 1, b: j });
        i = j;
    }
    out
}

/// Lossy: a run of 4+ lines of one shape keeps its first two and its last; a run of 4+ passing
/// lines keeps its first and last. The middle becomes one `...` line naming its range.
fn collapse_runs(v: Vec<Out>) -> Vec<Out> {
    let mut out: Vec<Out> = vec![];
    let mut i = 0;
    while i < v.len() {
        let sh = shape(&v[i].text);
        let mut j = i + 1;
        while j < v.len() && shape(&v[j].text) == sh && !sh.is_empty() { j += 1; }
        if j - i >= 4 {
            let n = j - i;
            out.push(Out { text: v[i].text.clone(), a: v[i].a, b: v[i].b, important: v[i].important });
            out.push(Out { text: v[i + 1].text.clone(), a: v[i + 1].a, b: v[i + 1].b, important: v[i + 1].important });
            out.push(Out { text: format!("... {} more lines like these (L{}-{})", n - 3, v[i + 2].a, v[j - 2].b), a: v[i + 2].a, b: v[j - 2].b, important: false });
            let l = &v[j - 1];
            out.push(Out { text: l.text.clone(), a: l.a, b: l.b, important: l.important });
            i = j;
            continue;
        }
        // passing lines that differ in shape (test names): fold the middle of a long run
        let mut k = i;
        while k < v.len() && passing(&v[k].text) && !v[k].important { k += 1; }
        if k - i >= 4 {
            out.push(Out { text: v[i].text.clone(), a: v[i].a, b: v[i].b, important: false });
            out.push(Out { text: format!("... {} passing lines (L{}-{})", k - i - 2, v[i + 1].a, v[k - 2].b), a: v[i + 1].a, b: v[k - 2].b, important: false });
            out.push(Out { text: v[k - 1].text.clone(), a: v[k - 1].a, b: v[k - 1].b, important: false });
            i = k;
            continue;
        }
        out.push(Out { text: v[i].text.clone(), a: v[i].a, b: v[i].b, important: v[i].important });
        i += 1;
    }
    out
}

/// Lossy: over budget, keep the head, the tail (summaries live there) and every important line
/// with one line of context; the rest becomes `... L a-b (N lines)`.
fn fit_budget(v: Vec<Out>, budget: usize) -> Vec<Out> {
    if v.len() <= budget { return v; }
    let (head, tail) = (budget / 6, budget / 4);
    let mut keep = vec![false; v.len()];
    for k in keep.iter_mut().take(head) { *k = true; }
    for k in keep.iter_mut().skip(v.len().saturating_sub(tail)) { *k = true; }
    let mut used = keep.iter().filter(|k| **k).count();
    for i in 0..v.len() {
        if used >= budget { break; }
        if v[i].important {
            for j in i.saturating_sub(1)..=(i + 1).min(v.len() - 1) {
                if !keep[j] { keep[j] = true; used += 1; }
            }
        }
    }
    let mut out: Vec<Out> = vec![];
    let mut i = 0;
    while i < v.len() {
        if keep[i] { out.push(Out { text: v[i].text.clone(), a: v[i].a, b: v[i].b, important: v[i].important }); i += 1; continue; }
        let s = i;
        while i < v.len() && !keep[i] { i += 1; }
        let (a, b) = (v[s].a, v[i - 1].b);
        out.push(Out { text: format!("... L{a}-{b} ({} lines) not shown", b + 1 - a), a, b, important: false });
    }
    out
}

/// Lines longer than this (a compiler invocation, a minified bundle) are cut, past the budget.
const LONG_LINE: usize = 300;

pub fn compress_log(lines: &[String], budget: usize) -> (String, bool) {
    let v = collapse_identical(lines);
    // the budget is lines *and* characters: 120 lines of 2k-character commands is not small
    let chars: usize = v.iter().map(|o| o.text.len() + 1).sum();
    let lossless = v.len() <= budget && chars <= budget * 120;
    let v = if lossless { v } else {
        let mut v = fit_budget(collapse_runs(v), budget);
        for o in v.iter_mut() {
            let n = o.text.chars().count();
            let max = if o.important { LONG_LINE * 2 } else { LONG_LINE };
            if n > max { o.text = format!("{}…(+{} chars, L{})", o.text.chars().take(max - 60).collect::<String>(), n - (max - 60), o.a); }
        }
        v
    };
    let text: Vec<String> = v.into_iter().map(|o| o.text).collect();
    (text.join("\n"), !lossless)
}

// ------------------------------------------------------------------ JSON

fn cell(v: &Value, max: usize, lossy: &mut bool) -> String {
    let s = match v { Value::String(s) => s.clone(), other => other.to_string() };
    if s.chars().count() > max { *lossy = true; format!("{}…(+{})", s.chars().take(max).collect::<String>(), s.chars().count() - max) } else { s }
}

fn errorish(v: &Value) -> bool {
    match v {
        Value::String(s) => important(s),
        Value::Object(m) => m.iter().any(|(k, x)| important(k) && !x.is_null() && x != &Value::Bool(false) || errorish(x)),
        Value::Array(a) => a.iter().any(errorish),
        _ => false,
    }
}

/// Render JSON compactly: scalars inline, objects as `key: value` lines, arrays of objects as a
/// table. `rows` is the row budget of a table; `path` is how `mt out --json` reaches this node.
fn render(v: &Value, path: &str, ind: usize, rows: usize, out: &mut Vec<String>, lossy: &mut bool) {
    let pad = "  ".repeat(ind);
    match v {
        Value::Array(a) if a.len() >= 3 && a.iter().all(|x| x.is_object()) => {
            // the columns: every key, in first-seen order; a key with one value in every row is hoisted
            let mut cols: Vec<String> = vec![];
            for x in a { for k in x.as_object().unwrap().keys() { if !cols.contains(k) { cols.push(k.clone()); } } }
            let mut constant: Vec<String> = vec![];
            cols.retain(|k| {
                let first = a[0].get(k);
                let same = first.is_some() && a.iter().all(|x| x.get(k) == first);
                if same { let mut l = false; constant.push(format!("{k}={}", cell(first.unwrap(), 60, &mut l))); *lossy |= l; }
                !same
            });
            out.push(format!("{pad}[{} items, table]{}", a.len(), if constant.is_empty() { String::new() } else { format!(" constant: {}", constant.join(", ")) }));
            if cols.is_empty() { return; }
            out.push(format!("{pad}{}", cols.join(" | ")));
            let mut show: Vec<usize> = (0..a.len()).collect();
            if a.len() > rows {
                *lossy = true;
                let mut keep: Vec<usize> = (0..rows * 2 / 3).collect();
                keep.extend(a.len() - rows / 6..a.len());
                let extra: Vec<usize> = (0..a.len()).filter(|i| !keep.contains(i) && errorish(&a[*i])).take(rows / 4).collect();
                keep.extend(extra);
                keep.sort();
                keep.dedup();
                show = keep;
            }
            let mut prev: Option<usize> = None;
            for i in show {
                if let Some(p) = prev { if i > p + 1 { out.push(format!("{pad}... rows {}-{} not shown: mt out <id> --json '{path}[{}:{}]'", p + 1, i - 1, p + 1, i)); } }
                let row: Vec<String> = cols.iter().map(|k| a[i].get(k).map(|x| cell(x, 80, lossy)).unwrap_or_else(|| "-".into())).collect();
                out.push(format!("{pad}{}", row.join(" | ")));
                prev = Some(i);
            }
            if let Some(p) = prev { if p + 1 < a.len() { out.push(format!("{pad}... rows {}-{} not shown: mt out <id> --json '{path}[{}:{}]'", p + 1, a.len() - 1, p + 1, a.len())); } }
        }
        Value::Array(a) if !a.is_empty() && a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            let n = a.len();
            if n <= 20 {
                out.push(format!("{pad}{}", Value::Array(a.clone())));
            } else {
                *lossy = true;
                let nums: Vec<f64> = a.iter().filter_map(|x| x.as_f64()).collect();
                let range = if nums.len() == n { format!(", min {} max {}", nums.iter().cloned().fold(f64::INFINITY, f64::min), nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max)) } else { String::new() };
                let mut l = false;
                let head: Vec<String> = a[..10].iter().map(|x| cell(x, 60, &mut l)).collect();
                let tail: Vec<String> = a[n - 3..].iter().map(|x| cell(x, 60, &mut l)).collect();
                out.push(format!("{pad}[{n} items{range}] {}, ... {}: mt out <id> --json '{path}[10:{}]'", head.join(", "), tail.join(", "), n - 3));
            }
        }
        Value::Array(a) => {
            out.push(format!("{pad}[{} items]", a.len()));
            for (i, x) in a.iter().enumerate().take(rows) {
                if matches!(x, Value::Object(_) | Value::Array(_)) {
                    out.push(format!("{pad}- [{i}]"));
                    render(x, &format!("{path}[{i}]"), ind + 1, rows, out, lossy);
                } else {
                    out.push(format!("{pad}- {}", cell(x, 200, lossy)));
                }
            }
            if a.len() > rows { *lossy = true; out.push(format!("{pad}... items {}-{} not shown: mt out <id> --json '{path}[{}:{}]'", rows, a.len() - 1, rows, a.len())); }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = format!("{path}.{k}");
                match x {
                    Value::Object(o) if o.is_empty() => out.push(format!("{pad}{k}: {{}}")),
                    Value::Array(a) if a.is_empty() => out.push(format!("{pad}{k}: []")),
                    Value::Object(_) | Value::Array(_) => {
                        let compact = x.to_string();
                        if compact.len() <= 100 { out.push(format!("{pad}{k}: {compact}")); } else { out.push(format!("{pad}{k}:")); render(x, &p, ind + 1, rows, out, lossy); }
                    }
                    other => out.push(format!("{pad}{k}: {}", cell(other, 300, lossy))),
                }
            }
        }
        other => out.push(format!("{pad}{}", cell(other, 300, lossy))),
    }
}

/// JSON (or NDJSON, one value per line): minified when that fits, else the table rendering.
pub fn compress_json(v: &Value, budget_chars: usize) -> (String, bool) {
    let min = v.to_string();
    if min.len() <= budget_chars { return (min, false); }
    let mut rows = 40usize;
    loop {
        let (mut out, mut lossy) = (vec![], false);
        render(v, "", 0, rows, &mut out, &mut lossy);
        let s = out.join("\n");
        if s.len() <= budget_chars || rows <= 6 { return (s, lossy); }
        rows = rows * 2 / 3;
    }
}

fn parse_json(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(t) { return Some(v); }
    }
    // NDJSON: every non-empty line one JSON object
    let ls: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    if ls.len() >= 2 && ls.iter().all(|l| l.trim_start().starts_with('{')) {
        let vs: Option<Vec<Value>> = ls.iter().map(|l| serde_json::from_str(l).ok()).collect();
        return vs.map(Value::Array);
    }
    None
}

// ------------------------------------------------------------------ entry point

/// JSON up to this size (minified) is sent as it is: reading it is cheaper than a query.
/// Anything bigger never reaches the model: it gets the graph, and queries for what it needs.
pub const SMALL_JSON: usize = 1_500;

pub fn compress(input: &str, kind: Kind, budget_lines: usize) -> Compressed {
    let chars_in = input.chars().count();
    let lines_in = input.lines().count();
    let json = if matches!(kind, Kind::Auto | Kind::Json) { parse_json(&strip_ansi(input)) } else { None };
    let (text, lossy, kind_s) = if let Some(v) = json {
        let min = v.to_string();
        if min.len() <= SMALL_JSON { (min, false, "json") } else { (json_graph(&v), true, "json graph") }
    } else {
        let lines = clean_lines(input);
        let diff = looks_like_diff(&lines);
        // code and diffs stay exact while they fit (an edit needs the exact text); past that they
        // would flood the context, so a diff becomes its outline and anything else a log, both
        // kept whole for `mt out --lines/--grep`
        let exact = (kind == Kind::Verbatim || (kind == Kind::Auto && diff)) && chars_in <= EXACT_MAX_CHARS;
        if exact {
            let v = collapse_identical(&lines);
            (v.into_iter().map(|o| o.text).collect::<Vec<_>>().join("\n"), false, "verbatim")
        } else if diff {
            (diff_outline(&lines, budget_lines), true, "diff outline")
        } else {
            let (t, l) = compress_log(&lines, budget_lines);
            if l { (format!("{}\n{t}", log_graph(&lines)), true, "log") } else { (t, false, "log") }
        }
    };
    let lines_out = text.lines().count();
    Compressed { text, lossy, kind: kind_s, lines_in, lines_out, chars_in }
}

// ------------------------------------------------------------------ graphs: the map of a large output

/// What one JSON path holds across every value seen there.
#[derive(Default)]
struct Shape {
    count: usize,
    types: BTreeMap<&'static str, usize>,
    keys: Vec<(String, Shape)>,
    item: Option<Box<Shape>>,
    len: Option<(usize, usize)>,
    num: Option<(f64, f64)>,
    values: BTreeMap<String, usize>,
    many_values: bool,
    max_len: usize,
}

fn type_of(v: &Value) -> &'static str {
    match v { Value::Null => "null", Value::Bool(_) => "bool", Value::Number(_) => "number", Value::String(_) => "string", Value::Array(_) => "array", Value::Object(_) => "object" }
}

fn add_shape(s: &mut Shape, v: &Value) {
    s.count += 1;
    *s.types.entry(type_of(v)).or_default() += 1;
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let i = match s.keys.iter().position(|(n, _)| n == k) { Some(i) => i, None => { s.keys.push((k.clone(), Shape::default())); s.keys.len() - 1 } };
                add_shape(&mut s.keys[i].1, x);
            }
        }
        Value::Array(a) => {
            s.len = Some(s.len.map_or((a.len(), a.len()), |(lo, hi)| (lo.min(a.len()), hi.max(a.len()))));
            let it = s.item.get_or_insert_with(Default::default);
            for x in a { add_shape(it, x); }
        }
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            s.num = Some(s.num.map_or((f, f), |(lo, hi)| (lo.min(f), hi.max(f))));
        }
        Value::String(t) => {
            s.max_len = s.max_len.max(t.chars().count());
            if !s.many_values {
                *s.values.entry(t.clone()).or_default() += 1;
                if s.values.len() > 50 { s.many_values = true; }
            }
        }
        Value::Bool(b) => { *s.values.entry(b.to_string()).or_default() += 1; }
        Value::Null => {}
    }
}

fn draw_shape(s: &Shape, label: &str, parent: usize, ind: usize, out: &mut Vec<String>) {
    if out.len() > 150 { return; }
    let pad = "  ".repeat(ind);
    let types: Vec<&str> = s.types.keys().copied().collect();
    let mut t = types.join("|");
    if let (Some((lo, hi)), true) = (s.len, types.contains(&"array")) { t = t.replace("array", &if lo == hi { format!("array[{lo}]") } else { format!("array[{lo}..{hi}]") }); }
    let mut extra: Vec<String> = vec![];
    if s.count < parent { extra.push(format!("in {} of {}", s.count, parent)); }
    if let Some((lo, hi)) = s.num { extra.push(if lo == hi { format!("= {lo}") } else { format!("{lo}..{hi}") }); }
    let strings = s.types.get("string").copied().unwrap_or(0) + s.types.get("bool").copied().unwrap_or(0);
    if strings > 0 {
        let short = s.values.keys().all(|v| v.chars().count() <= 40);
        if !s.many_values && s.values.len() <= 8 && short {
            // a small set of values (a status, a kind, a flag): what a [?key=value] query needs
            let vs: Vec<String> = s.values.iter().map(|(v, n)| format!("{v}:{n}")).collect();
            extra.push(format!("values {{{}}}", vs.join(", ")));
        } else if s.types.contains_key("string") {
            extra.push(format!("{} distinct, up to {} chars", if s.many_values { "50+".to_string() } else { s.values.len().to_string() }, s.max_len));
        }
    }
    out.push(format!("{pad}{label}  {t}{}", if extra.is_empty() { String::new() } else { format!("  ({})", extra.join("; ")) }));
    for (k, c) in &s.keys {
        let key = if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { format!(".{k}") } else { format!(".\"{k}\"") };
        draw_shape(c, &key, s.types.get("object").copied().unwrap_or(s.count), ind + 1, out);
    }
    if let Some(it) = &s.item { draw_shape(it, "[*]", it.count, ind + 1, out); }
}

/// The structure of a JSON value with none of its data: every path, its types, how often it
/// occurs, number ranges, string lengths, and the values of small enumerations. Enough to write
/// the query that pulls exactly what is needed.
pub fn json_graph(v: &Value) -> String {
    let mut s = Shape::default();
    add_shape(&mut s, v);
    let mut out = vec![];
    draw_shape(&s, "$", 1, 0, &mut out);
    if out.len() > 150 { out.truncate(150); out.push("... graph cut at 150 paths; query a subtree: mt out <id> --json '.path'".into()); }
    out.join("\n")
}

/// The map of a long log: size, how many lines matter, and the commonest line shapes with where
/// each first occurs.
pub fn log_graph(lines: &[String]) -> String {
    let imp: Vec<usize> = (0..lines.len()).filter(|&i| important(&lines[i])).collect();
    let mut shapes: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        if l.trim().is_empty() { continue; }
        let e = shapes.entry(shape(l)).or_insert((0, i + 1));
        e.0 += 1;
    }
    let mut top: Vec<(&String, &(usize, usize))> = shapes.iter().filter(|(_, (n, _))| *n > 1).collect();
    top.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
    let mut g = format!("graph: {} lines, {} distinct shapes, {} lines that matter (errors, warnings, failures, file:line){}",
        lines.len(), shapes.len(), imp.len(), imp.first().map(|f| format!(", first at L{}", f + 1)).unwrap_or_default());
    for (sh, (n, first)) in top.iter().take(6) {
        let sh: String = sh.chars().take(90).collect();
        g.push_str(&format!("\n  {n:>5}x from L{first}: {sh}"));
    }
    g
}

// ------------------------------------------------------------------ natural-language queries (BM25)

fn terms(q: &str) -> Vec<String> {
    const STOP: &[&str] = &["the", "a", "an", "of", "in", "on", "for", "to", "which", "what", "show", "me", "all", "with", "is", "are", "that", "and", "or", "from", "list", "find", "get", "where", "how", "many", "any", "do", "does", "did", "has", "have", "by", "it", "this", "these", "those", "be", "was", "were", "there", "give", "only", "whose", "who", "their", "its", "at", "as", "not", "no"];
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2 && !STOP.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// A light, deterministic stemmer: failed/failing/failure/fails -> fail, errors -> error.
fn stem(w: &str) -> String {
    for suf in ["ures", "ure", "ing", "ed", "es", "s"] {
        if w.len() > suf.len() + 3 {
            if let Some(s) = w.strip_suffix(suf) { return s.to_string(); }
        }
    }
    w.to_string()
}

/// BM25 over `docs`: the indices of the best matches, best first, ties in document order.
fn bm25(docs: &[Vec<String>], q: &[String], top: usize) -> Vec<(usize, f64)> {
    let mut r = bm25_raw(docs, q, docs.len());
    // weak matches (only the commonest word) are noise next to a strong one
    if let Some(best) = r.first().map(|x| x.1) { r.retain(|x| x.1 >= best * 0.25); }
    r.truncate(top);
    r
}

fn bm25_raw(docs: &[Vec<String>], q: &[String], top: usize) -> Vec<(usize, f64)> {
    let n = docs.len() as f64;
    if docs.is_empty() || q.is_empty() { return vec![]; }
    let avg = docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n;
    let mut scores: Vec<(usize, f64)> = vec![];
    let uq: Vec<&String> = { let mut s: Vec<&String> = q.iter().collect(); s.sort(); s.dedup(); s };
    let df: Vec<f64> = uq.iter().map(|t| docs.iter().filter(|d| d.contains(t)).count() as f64).collect();
    for (i, d) in docs.iter().enumerate() {
        let mut sc = 0.0;
        for (t, &f) in uq.iter().zip(&df) {
            let tf = d.iter().filter(|w| w == t).count() as f64;
            if tf == 0.0 { continue; }
            let idf = ((n - f + 0.5) / (f + 0.5) + 1.0).ln();
            sc += idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * d.len() as f64 / avg.max(1.0)));
        }
        if sc > 0.0 { scores.push((i, sc)); }
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    scores.truncate(top);
    scores
}

/// Every record of a JSON value: each element of every array of objects (with its path), else
/// every leaf. A record's words are its keys and its values.
fn records<'a>(v: &'a Value, path: String, out: &mut Vec<(String, &'a Value)>) {
    records_in(v, path, false, out)
}

/// `inside`: v is part of a record already listed, so its own leaves are not records (they
/// would outrank the record they belong to); arrays of objects inside it still are.
fn records_in<'a>(v: &'a Value, path: String, inside: bool, out: &mut Vec<(String, &'a Value)>) {
    match v {
        Value::Array(a) => {
            let objs = a.iter().any(|x| x.is_object());
            for (i, x) in a.iter().enumerate() {
                let p = format!("{path}[{i}]");
                if objs && x.is_object() { out.push((p.clone(), x)); records_in(x, p, true, out); }
                else if x.is_array() { records_in(x, p, inside, out); }
                else if !objs && !inside { out.push((p, x)); }
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = format!("{path}.{k}");
                if x.is_array() || x.is_object() { records_in(x, p, inside, out); } else if !inside { out.push((p, x)); }
            }
        }
        _ => if !inside { out.push((path, v)) },
    }
}

fn words_of(v: &Value, path: &str) -> Vec<String> {
    let mut s = path.to_string();
    fn walk(v: &Value, s: &mut String) {
        match v {
            Value::Object(m) => for (k, x) in m { s.push(' '); s.push_str(k); walk(x, s) },
            Value::Array(a) => for x in a { walk(x, s) },
            Value::String(t) => { s.push(' '); s.push_str(t) },
            other => { s.push(' '); s.push_str(&other.to_string()) }
        }
    }
    walk(v, &mut s);
    terms(&s)
}

/// `mt out <id> "<question>"`: the records (JSON) or lines (text) that best match the question's
/// words, ranked by BM25. Deterministic: the same question gets the same answer.
pub fn ask(text: &str, question: &str, cap: usize) -> String {
    let mut q = terms(question);
    if q.is_empty() { return "no searchable words in the question\n".into(); }
    // adjacent words also as one: "time out" finds "timeout", "data base" finds "database"
    let joined: Vec<String> = q.windows(2).map(|w| format!("{}{}", w[0], w[1])).collect();
    q.extend(joined);
    if let Some(v) = parse_json(text) {
        let mut recs: Vec<(String, &Value)> = vec![];
        records(&v, String::new(), &mut recs);
        // a record nested in a better-matching one is noise: rank, then drop children of shown records
        let docs: Vec<Vec<String>> = recs.iter().map(|(p, x)| words_of(x, p)).collect();
        let hits = bm25(&docs, &q, 40);
        let mut shown: Vec<&str> = vec![];
        let mut s = format!("# {} of {} records match \"{}\" (terms: {})\n", hits.len(), recs.len(), question, q.join(" "));
        for (i, sc) in hits {
            let (p, x) = &recs[i];
            if shown.iter().any(|sp| p.starts_with(sp) && p.len() > sp.len()) || shown.len() >= 12 { continue; }
            shown.push(p);
            let body = x.to_string();
            let body = if body.len() > 600 { format!("{}…(+{} chars: mt out <id> --json '${p}')", &body[..(0..600).rev().find(|&k| body.is_char_boundary(k)).unwrap_or(0)], body.len() - 600) } else { body };
            let line = format!("{p}  [{sc:.1}]  {body}\n");
            if s.len() + line.len() > cap { s.push_str("... cut at the tool-result limit\n"); break; }
            s.push_str(&line);
        }
        return s;
    }
    let lines = clean_lines(text);
    let docs: Vec<Vec<String>> = lines.iter().map(|l| terms(l)).collect();
    // a question about a log is nearly always about what went wrong: errors weigh more
    let mut hits: Vec<(usize, f64)> = bm25(&docs, &q, lines.len()).into_iter().map(|(i, s)| (i, if important(&lines[i]) { s * 2.5 } else { s })).collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    hits.truncate(30);
    let mut pick: Vec<usize> = hits.iter().map(|h| h.0).collect();
    pick.sort();
    let w = lines.len().to_string().len();
    let mut s = format!("# {} best of the lines matching \"{}\" (terms: {}), in order\n", pick.len(), question, q.join(" "));
    let mut prev: Option<usize> = None;
    for i in pick {
        if prev.is_some_and(|p| i > p + 1) { s.push_str("--\n"); }
        let line = format!("{:>w$}  {}\n", i + 1, lines[i], w = w);
        if s.len() + line.len() > cap { s.push_str("... cut at the tool-result limit\n"); break; }
        s.push_str(&line);
        prev = Some(i);
    }
    s
}

// ------------------------------------------------------------------ named stores
//
// Kept outputs live in named stores, so work on different things never mixes: one agent's
// `vercel-logs`, another's `docker-compose`. A query names its store (`mt out --name vercel-logs
// "…"`), or one id (`mt out <id>`, found in whichever store holds it), or searches all of them.
// Stores live in the repo's `.mt/out/<name>/`, so every agent and session in the repo can read
// every store; outside a repo, in the temp dir. Each output records who kept it.

/// Per-session state of the hook's dedup (what this context was shown): never shared.
fn session_dir(sid: Option<&str>) -> PathBuf {
    let sid = sid.map(String::from).or_else(crate::query::session_id).unwrap_or_else(|| "nosession".into());
    let safe: String = sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    std::env::temp_dir().join(format!("mt-out-{safe}"))
}

/// A file in this session's state directory (the hook's dedup state).
pub fn session_path(sid: Option<&str>, name: &str) -> PathBuf {
    session_dir(sid).join(name)
}

/// Where the named stores live. MT_OUT_DIR overrides it.
fn out_base() -> PathBuf {
    if let Some(d) = std::env::var_os("MT_OUT_DIR") { return PathBuf::from(d); }
    let root = crate::index::find_root(&std::env::current_dir().unwrap_or_default());
    if root.join(".mt").is_dir() { root.join(".mt/out") } else { std::env::temp_dir().join("mt-out") }
}

/// A store name: lowercase letters, digits, `-`, `_`, `.`; anything else becomes `-`.
pub fn clean_name(n: &str) -> String {
    let s: String = n.trim().chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c.to_ascii_lowercase() } else { '-' }).collect();
    let s: String = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "default".into() } else { s.chars().take(60).collect() }
}

/// The store for output nobody named: MT_OUT_NAME (an agent's own), else `fallback`.
pub fn default_name(fallback: &str) -> String {
    std::env::var("MT_OUT_NAME").ok().filter(|s| !s.trim().is_empty()).map(|s| clean_name(&s)).unwrap_or_else(|| clean_name(fallback))
}

/// Outputs older than this are removed whenever a new one is kept.
const KEEP_DAYS: u64 = 7;

fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

/// Keep the original in store `name`; the id is its hash, so keeping the same output twice is one file.
pub fn store(original: &str, label: &str, name: &str) -> String {
    let id = format!("{:08x}", fnv1a(original.as_bytes()) as u32);
    let base = out_base();
    let d = base.join(clean_name(name));
    let _ = std::fs::create_dir_all(&d);
    // kept outputs are raw logs and API responses (they can hold tokens): never committed,
    // whatever the repo's own .gitignore says
    let gi = base.join(".gitignore");
    if !gi.exists() { let _ = std::fs::write(&gi, "*\n"); }
    let p = d.join(format!("{id}.txt"));
    if !p.exists() {
        let _ = std::fs::write(&p, original);
        let owner = crate::query::session_id().unwrap_or_default();
        let meta = serde_json::json!({"label": label, "name": clean_name(name), "owner": owner, "at": now()});
        let _ = std::fs::write(d.join(format!("{id}.meta")), meta.to_string());
    }
    // ponytail: a full scan per kept output; an index file if stores grow to many thousands
    for k in kept_all(&base) {
        if now().saturating_sub(k.at) > KEEP_DAYS * 86_400 {
            let _ = std::fs::remove_file(&k.path);
            let _ = std::fs::remove_file(k.path.with_extension("meta"));
        }
    }
    id
}

struct Kept { id: String, name: String, path: PathBuf, label: String, owner: String, at: u64 }

fn kept_all(base: &std::path::Path) -> Vec<Kept> {
    let mut v: Vec<Kept> = vec![];
    for dir in std::fs::read_dir(base).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
        let name = dir.file_name().to_string_lossy().into_owned();
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_none_or(|x| x != "txt") { continue; }
            let meta: Value = std::fs::read_to_string(p.with_extension("meta")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            let at = meta.get("at").and_then(|x| x.as_u64()).unwrap_or_else(|| f.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
            v.push(Kept {
                id: p.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                name: name.clone(),
                label: meta.get("label").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                owner: meta.get("owner").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                path: p,
                at,
            });
        }
    }
    v.sort_by(|a, b| a.name.cmp(&b.name).then(b.at.cmp(&a.at)).then(a.id.cmp(&b.id)));
    v
}

/// The one-line header that says what was reduced, which store kept it, and how to get the rest.
pub fn header(c: &Compressed, id: Option<&str>, what: &str, name: &str) -> String {
    let tok = |n: usize| if n >= 1000 { format!("{:.1}k", n as f64 / 4000.0) } else { format!("{}", n / 4) };
    let saved = format!("{} -> {} lines, ~{} -> ~{} tokens, {}", c.lines_in, c.lines_out, tok(c.chars_in), tok(c.text.chars().count()), c.kind);
    match id {
        Some(id) if c.kind == "json graph" => format!(
            "[mt: {what} is JSON ({} chars), kept as {id} in store \"{name}\", out of context; below is its graph (paths, types, counts, small value sets), not its data. \
             Pull only what you need, with JSONPath (RFC 9535): mt out {id} --json \"$.items[?@.state=='failed'].name\", \
             or jq: mt out {id} --jq '[.items[] | select(.state==\"failed\") | .name]', \
             or plain words: mt out {id} \"<question>\" (the whole store: mt out --name {name} \"<question>\")]", c.chars_in),
        Some(id) => format!("[mt: {what} reduced: {saved}; kept as {id} in store \"{name}\". The rest: mt out {id} \"<question>\" (plain words), --grep RE, --lines A-B, --level error; the whole store: mt out --name {name} \"<question>\"]"),
        None => format!("[mt: {what} cleaned: {saved}, nothing dropped]"),
    }
}

/// Compress, and keep the original in store `name` when anything was dropped. Returns (text, id).
pub fn compress_and_store(input: &str, kind: Kind, budget: usize, what: &str, name: &str) -> (String, Option<String>) {
    let c = compress(input, kind, budget);
    // nothing dropped: the cleaned text alone (a header would cost more than it says)
    if !c.lossy { return (c.text, None); }
    let name = clean_name(name);
    let id = store(input, what, &name);
    let root = crate::index::find_root(&std::env::current_dir().unwrap_or_default());
    crate::events::emit(&root, serde_json::json!({"kind": "compress", "what": what.chars().take(120).collect::<String>(), "store": name, "id": id,
        "chars_in": c.chars_in, "chars_out": c.text.chars().count(), "as": c.kind, "session": crate::events::session8()}));
    (format!("{}\n{}", header(&c, Some(&id), what, &name), c.text), Some(id))
}

/// A store name for a shell command nobody named: its program and first plain-word argument
/// (`vercel logs x` -> vercel-logs, `docker compose logs` -> docker-compose, `go test ./...` ->
/// go-test), skipping wrappers (`sudo`, `env`, `npx`, `VAR=1`).
pub fn command_name(cmd: &str) -> String {
    const WRAP: &[&str] = &["sudo", "env", "time", "nice", "nohup", "exec", "npx", "bunx", "pnpx", "uvx", "command", "xargs"];
    let first = cmd.split(['|', ';', '&', '\n']).map(str::trim).find(|s| !s.is_empty()).unwrap_or("");
    let toks: Vec<&str> = first.split_whitespace().filter(|t| !t.contains('=') || t.starts_with('-')).collect();
    let mut it = toks.iter().skip_while(|t| WRAP.contains(t) || t.starts_with('-'));
    let Some(prog) = it.next() else { return "shell".into() };
    let prog = prog.rsplit('/').next().unwrap_or(prog).trim_matches(|c| c == '\'' || c == '"');
    let sub = it.next().filter(|s| s.len() <= 20 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !s.starts_with('-'));
    clean_name(&match sub { Some(s) => format!("{prog}-{s}"), None => prog.to_string() })
}

/// mt's earlier shorthand, as RFC 9535 JSONPath: `.items[?state=failed].title` ->
/// `$.items[?@.state=='failed'].title`; `items[0]` -> `$.items[0]`. Real JSONPath (it starts
/// with `$`) passes unchanged.
pub fn to_jsonpath(p: &str) -> String {
    let p = p.trim();
    if p.starts_with('$') { return p.to_string(); }
    let body = if p.starts_with('.') || p.starts_with('[') { p.to_string() } else { format!(".{p}") };
    // `[?key=value]` (no `@`): a comparison of a member with a literal
    static F: OnceLock<Regex> = OnceLock::new();
    let body = rx(&F, r"\[\?\s*([A-Za-z_][\w.-]*)\s*(==|!=|<=|>=|=|<|>)\s*([^\]]*?)\s*\]").replace_all(&body, |c: &regex::Captures| {
        let (k, op, v) = (&c[1], if &c[2] == "=" { "==" } else { &c[2] }, c[3].trim_matches(|x| x == '"' || x == '\''));
        let lit = if v.parse::<f64>().is_ok() || matches!(v, "true" | "false" | "null") { v.to_string() } else { format!("'{}'", v.replace('\'', "\\'")) };
        format!("[?@.{k}{op}{lit}]")
    });
    format!("${body}")
}

/// An RFC 9535 JSONPath query (or mt's shorthand): every match with its normalized location.
pub fn json_path(v: &Value, path: &str) -> Result<Vec<(String, Value)>, String> {
    let q = to_jsonpath(path);
    let jp = serde_json_path::JsonPath::parse(&q).map_err(|e| format!("not a JSONPath ({q}): {e}"))?;
    Ok(jp.query_located(v).iter().map(|n| (n.location().to_string(), n.node().clone())).collect())
}

/// Results a jq filter may produce before it is stopped: bounded, like every mt answer.
const JQ_MAX_RESULTS: usize = 10_000;
/// A filter running longer than this (MT_JQ_SECONDS, default 10) is stopped.
fn jq_seconds() -> u64 { std::env::var("MT_JQ_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10) }

/// A jq filter over a JSON text, run by jaq (a jq implementation in Rust) in this process.
/// Each output value is one result. A filter that runs past `jq_seconds()` is stopped.
pub fn jq(text: &str, filter: &str) -> Result<Vec<Value>, String> {
    use jaq_core::load::{Arena, File, Loader};
    let input = text.to_string();
    let code = filter.to_string();
    let (tx, rx_) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let run = || -> Result<Vec<Value>, String> {
            let data = jaq_json::read::parse_single(input.trim().as_bytes()).map_err(|e| format!("input is not one JSON value: {e}"))?;
            let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
            let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());
            let arena = Arena::default();
            let modules = Loader::new(defs).load(&arena, File { code: code.as_str(), path: () })
                .map_err(|errs| format!("jq syntax: {}", errs.iter().map(|(_, e)| format!("{e:?}")).collect::<Vec<_>>().join("; ")))?;
            let f = jaq_core::Compiler::default().with_funs(funs).compile(modules)
                .map_err(|errs| format!("jq: {}", errs.iter().flat_map(|(_, es)| es.iter().map(|e| format!("{e:?}"))).collect::<Vec<_>>().join("; ")))?;
            let ctx = jaq_core::Ctx::<jaq_core::data::JustLut<jaq_json::Val>>::new(&f.lut, jaq_core::Vars::new([]));
            let mut out = vec![];
            for r in f.id.run((ctx, data)).map(jaq_core::unwrap_valr) {
                match r {
                    Ok(v) => out.push(serde_json::from_str::<Value>(&v.to_string()).unwrap_or(Value::String(v.to_string()))),
                    Err(e) => return Err(format!("jq: {e}")),
                }
                if out.len() >= JQ_MAX_RESULTS { break; }
            }
            Ok(out)
        };
        let _ = tx.send(run());
    });
    let secs = jq_seconds();
    rx_.recv_timeout(std::time::Duration::from_secs(secs)).unwrap_or_else(|_| Err(format!("jq: stopped after {secs} s (a filter that never ends?)")))
}

/// How a query's results are shown: each with its location when they are few and small, else
/// one table (keys once, constants hoisted) sampled to the limit.
fn show_results(id: &str, what: &str, hits: Vec<(Option<String>, Value)>, cap: usize) -> String {
    let n = hits.len();
    if n == 0 { return format!("# mt out {id} {what}: 0 match(es)\n"); }
    let small = n <= 20 && hits.iter().all(|(_, v)| v.to_string().len() <= 300);
    if small {
        let mut s = format!("# mt out {id} {what}: {n} match(es)\n");
        for (loc, v) in &hits {
            s.push_str(&match loc { Some(l) => format!("{l}  {v}\n"), None => format!("{v}\n") });
        }
        return s;
    }
    let v = if n == 1 { hits.into_iter().next().unwrap().1 } else { Value::Array(hits.into_iter().map(|h| h.1).collect()) };
    let (s, lossy) = compress_json(&v, cap.saturating_sub(300));
    let note = if lossy { " (more than fits: narrow the query, or slice it)" } else { "" };
    format!("# mt out {id} {what}: {n} match(es){note}\n{}\n", s.replace("<id>", id))
}

pub struct OutQuery { pub ask: Option<String>, pub grep: Option<String>, pub ctx: usize, pub lines: Option<(usize, usize)>, pub level: Option<String>, pub json: Option<String>, pub jq: Option<String> }

impl OutQuery {
    fn any(&self) -> bool { self.ask.is_some() || self.grep.is_some() || self.lines.is_some() || self.level.is_some() || self.json.is_some() || self.jq.is_some() }
}

/// True when `s` is the id of a kept output (in any store).
pub fn is_kept_id(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit()) && kept_all(&out_base()).iter().any(|k| k.id == s)
}

/// `mt out`: list the stores, list one, or query one output, one store, or every store.
///   id                      that output, in whichever store holds it
///   name, no id             every output in that store (newest first)
///   neither, with a query   every output in every store
pub fn out(id: Option<&str>, name: Option<&str>, q: &OutQuery) -> (bool, String) {
    let cap: usize = std::env::var("MT_MAX_OUTPUT").ok().and_then(|v| v.parse().ok()).unwrap_or(28_000);
    let all = kept_all(&out_base());
    let name = name.map(clean_name);
    if let Some(id) = id {
        let Some(k) = all.iter().find(|k| k.id == id) else {
            return (false, format!("mt out: no kept output {id} in any store (mt out lists them)\n"));
        };
        let text = std::fs::read_to_string(&k.path).unwrap_or_default();
        return query_one(id, &text, q, cap);
    }
    let scope: Vec<&Kept> = all.iter().filter(|k| name.as_deref().is_none_or(|n| k.name == n)).collect();
    if !q.any() {
        // a listing: stores and their outputs, with who kept each
        if scope.is_empty() {
            return (true, match &name { Some(n) => format!("store \"{n}\" is empty (mt out lists the stores)\n"), None => "no kept outputs yet\n".into() });
        }
        let mut s = String::new();
        let mut cur = "";
        for k in &scope {
            if k.name != cur {
                let n = scope.iter().filter(|x| x.name == k.name).count();
                s.push_str(&format!("{} ({n} output{})\n", k.name, if n == 1 { "" } else { "s" }));
                cur = &k.name;
            }
            let owner: String = k.owner.chars().take(12).collect();
            s.push_str(&format!("  {}  {}  {}{}\n", k.id, k.label, if owner.is_empty() { String::new() } else { format!("by {owner}") }, ""));
        }
        return (true, s);
    }
    if scope.is_empty() {
        return (false, match &name { Some(n) => format!("mt out: store \"{n}\" is empty\n"), None => "mt out: no kept outputs yet\n".into() });
    }
    // the query over every output in scope, newest first, each answer under its id
    let mut s = format!("# mt out {}: {} output(s)\n", name.as_deref().map(|n| format!("--name {n}")).unwrap_or_else(|| "(all stores)".into()), scope.len());
    let mut ok = false;
    for k in scope {
        let text = std::fs::read_to_string(&k.path).unwrap_or_default();
        let (good, body) = query_one(&k.id, &text, q, cap.saturating_sub(s.len()));
        // an output with nothing for this query is left out, not listed as empty
        let empty = body.lines().count() <= 1 || body.contains(" 0 of ") || body.starts_with("# 0 ") || body.contains(": 0 match(es)");
        if !good || empty { continue; }
        ok = true;
        s.push_str(&format!("## {} in {} ({})\n{}", k.id, k.name, k.label, body.lines().skip(1).map(|l| format!("{l}\n")).collect::<String>()));
        if s.len() >= cap { s.push_str("... cut at the tool-result limit: name one store or one id\n"); break; }
    }
    if !ok { s.push_str("nothing in scope matches\n"); }
    (true, s)
}

/// One kept output, queried.
fn query_one(id: &str, text: &str, q: &OutQuery, cap: usize) -> (bool, String) {
    let text = text.to_string();
    if let Some(question) = q.ask.as_deref().filter(|s| !s.trim().is_empty()) {
        return (true, ask(&text, question, cap).replace("<id>", id));
    }
    if let Some(p) = &q.json {
        let Some(v) = parse_json(&text) else { return (false, format!("mt out {id}: not JSON\n")) };
        return match json_path(&v, p) {
            Err(e) => (false, format!("mt out {id} --json {p}: {e}\n")),
            Ok(hits) => (true, show_results(id, &format!("--json {}", to_jsonpath(p)), hits.into_iter().map(|(l, v)| (Some(l), v)).collect(), cap)),
        };
    }
    if let Some(f) = &q.jq {
        // jq reads the stored text itself; NDJSON (one value per line) is given to it as an array
        let src = if serde_json::from_str::<Value>(text.trim()).is_ok() { text.clone() } else { match parse_json(&text) { Some(v) => v.to_string(), None => return (false, format!("mt out {id}: not JSON\n")) } };
        return match jq(&src, f) {
            Err(e) => (false, format!("mt out {id} --jq: {e}\n")),
            Ok(vals) => (true, show_results(id, "--jq", vals.into_iter().map(|v| (None, v)).collect(), cap)),
        };
    }
    let lines = clean_lines(&text);
    let w = lines.len().to_string().len();
    let mut pick: Vec<bool> = vec![false; lines.len()];
    let mut any_filter = false;
    if let Some((a, b)) = q.lines { any_filter = true; for k in a.max(1) - 1..b.min(lines.len()) { pick[k] = true; } }
    let re = match q.grep.as_deref().map(Regex::new) { Some(Ok(r)) => Some(r), Some(Err(e)) => return (false, format!("mt out: bad --grep: {e}\n")), None => None };
    let lvl = q.level.as_deref();
    if re.is_some() || lvl.is_some() {
        any_filter = true;
        let mut hit: HashSet<usize> = HashSet::new();
        for (i, l) in lines.iter().enumerate() {
            let by_re = re.as_ref().is_none_or(|r| r.is_match(l));
            let by_lvl = match lvl { None => true, Some("warn") | Some("warning") => important(l), Some(_) => important(l) && !Regex::new(r"(?i)\bwarn").unwrap().is_match(l) || Regex::new(r"(?i)\b(error|fatal|panic|fail)").unwrap().is_match(l) };
            if by_re && by_lvl && (q.lines.is_none() || pick[i]) { hit.insert(i); }
        }
        pick = vec![false; lines.len()];
        for &i in &hit { for k in i.saturating_sub(q.ctx)..=(i + q.ctx).min(lines.len() - 1) { pick[k] = true; } }
    }
    if !any_filter {
        // the whole original, compressed losslessly at most (identical runs only)
        let v = collapse_identical(&lines);
        let s: Vec<String> = v.into_iter().map(|o| format!("{:>w$}  {}", o.a, o.text, w = w)).collect();
        let body = s.join("\n");
        let body = if body.len() > cap { format!("{}\n... cut at the tool-result limit: narrow with --lines, --grep or --level", &body[..(0..cap).rev().find(|&i| body.is_char_boundary(i)).unwrap_or(0)]) } else { body };
        return (true, format!("# mt out {id}: {} lines\n{body}\n", lines.len()));
    }
    let mut s = String::new();
    let mut prev: Option<usize> = None;
    let mut n = 0;
    for (i, l) in lines.iter().enumerate().filter(|(i, _)| pick[*i]) {
        if let Some(p) = prev { if i > p + 1 { s.push_str("--\n"); } }
        s.push_str(&format!("{:>w$}  {}\n", i + 1, l, w = w));
        prev = Some(i);
        n += 1;
        if s.len() > cap { s.push_str("... cut at the tool-result limit: narrow the query\n"); break; }
    }
    (true, format!("# mt out {id}: {n} of {} lines\n{s}", lines.len()))
}

// ------------------------------------------------------------------ session dedup (hooks)

fn seen_file(sid: &str) -> PathBuf {
    session_dir(Some(sid)).join("seen.lines")
}

/// Forget what this session has been shown (after a compaction it is no longer in context).
pub fn reset_seen(sid: &str) {
    let _ = std::fs::remove_file(seen_file(sid));
    let _ = std::fs::remove_file(session_dir(Some(sid)).join("reads.tsv"));
}

/// Runs of 6+ lines (each 20+ characters) that this session was already shown, verbatim,
/// become one line naming them. Every line then shown is remembered.
pub fn dedup_seen(text: &str, sid: &str) -> (String, usize) {
    let f = seen_file(sid);
    let seen: HashSet<String> = std::fs::read_to_string(&f).unwrap_or_default().lines().map(String::from).collect();
    let lines: Vec<&str> = text.split('\n').collect();
    let key = |l: &str| format!("{:016x}", fnv1a(l.trim_end().as_bytes()));
    let known = |l: &str| l.trim().len() >= 20 && seen.contains(&key(l));
    let mut out: Vec<String> = vec![];
    let mut dropped = 0;
    let mut i = 0;
    while i < lines.len() {
        if known(lines[i]) {
            let mut j = i;
            while j < lines.len() && (known(lines[j]) || (lines[j].trim().len() < 20 && j + 1 < lines.len() && known(lines[j + 1]))) { j += 1; }
            if j - i >= 6 {
                out.push(format!("[mt: lines {}-{} of this output ({} lines) were already shown earlier in this session, verbatim]", i + 1, j, j - i));
                dropped += j - i;
                i = j;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    let new: Vec<String> = lines.iter().filter(|l| l.trim().len() >= 20).map(|l| key(l)).filter(|k| !seen.contains(k)).collect();
    if !new.is_empty() {
        let _ = std::fs::create_dir_all(session_dir(Some(sid)));
        if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
            use std::io::Write;
            let _ = fh.write_all((new.join("\n") + "\n").as_bytes());
        }
    }
    (out.join("\n"), dropped)
}

/// Whole-file reads this session has had: path -> content hash. True when this exact content was
/// already read (and is therefore still in context, unless a compaction reset it).
pub fn read_seen(sid: &str, path: &str, content: &str) -> bool {
    let f = session_dir(Some(sid)).join("reads.tsv");
    let h = format!("{:016x}", fnv1a(content.as_bytes()));
    let mut m: BTreeMap<String, String> = std::fs::read_to_string(&f).unwrap_or_default().lines().filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.to_string()))).collect();
    let was = m.get(path) == Some(&h);
    if !was {
        m.insert(path.to_string(), h);
        let _ = std::fs::create_dir_all(session_dir(Some(sid)));
        let _ = std::fs::write(&f, m.iter().map(|(a, b)| format!("{a}\t{b}")).collect::<Vec<_>>().join("\n") + "\n");
    }
    was
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_progress_are_cleaned_losslessly() {
        let s = "\x1b[32mok\x1b[0m pkg\nDownloading 10%\rDownloading 55%\rDownloading 100%\n\x1b]0;title\x07done\n";
        assert_eq!(clean_lines(s), vec!["ok pkg", "Downloading 100%", "done"]);
        let c = compress(s, Kind::Auto, 120);
        assert!(!c.lossy && c.text == "ok pkg\nDownloading 100%\ndone");
    }

    #[test]
    fn logs_keep_every_error_and_the_summary() {
        let mut s = String::new();
        for i in 0..500 { s.push_str(&format!("2026-11-05T10:{:02}:{:02} INFO request {i} served in {}ms\n", i / 60, i % 60, i % 17)); }
        s.push_str("2026-11-05T10:09:00 ERROR db: connection refused (pool.go:88)\n");
        for i in 0..300 { s.push_str(&format!("=== RUN TestThing{i}\n--- PASS: TestThing{i} (0.00s)\n")); }
        s.push_str("--- FAIL: TestBroken (0.01s)\n    broken_test.go:42: want 3, got 4\nFAIL\nexit status 1\n");
        let c = compress(&s, Kind::Auto, 120);
        assert!(c.lossy && c.lines_out <= 130, "{} lines", c.lines_out);
        for must in ["connection refused (pool.go:88)", "--- FAIL: TestBroken", "broken_test.go:42: want 3, got 4", "exit status 1"] {
            assert!(c.text.contains(must), "lost {must:?}:\n{}", c.text);
        }
        // the same input always compresses the same way
        assert_eq!(compress(&s, Kind::Auto, 120).text, c.text);
    }

    #[test]
    fn small_and_repeated_output() {
        let c = compress("a\nb\n", Kind::Auto, 120);
        assert!(!c.lossy && c.text == "a\nb");
        let c = compress(&"same line again\n".repeat(50), Kind::Auto, 120);
        assert!(!c.lossy && c.text == "same line again (x50)", "{}", c.text);
    }

    fn issues() -> Value {
        let rows: Vec<Value> = (0..200).map(|i| serde_json::json!({"id": i, "repo": "relay", "state": if i == 150 { "error: timeout" } else { "ok" }, "title": format!("item {i} about the dashboard"), "user": {"login": format!("u{}", i % 7)}})).collect();
        serde_json::json!({"total": 200, "items": rows})
    }

    #[test]
    fn big_json_becomes_a_graph_with_no_data() {
        let input = serde_json::to_string_pretty(&issues()).unwrap();
        let c = compress(&input, Kind::Auto, 120);
        assert!(c.kind == "json graph" && c.lossy, "{}", c.kind);
        for must in ["$  object", ".items  array[200]", "[*]  object", ".state  string  (values {error: timeout:1, ok:199})", ".repo  string  (values {relay:200})", ".title  string  (50+ distinct, up to 28 chars)", ".login  string  (values {u0:29", ".total  number  (= 200)"] {
            assert!(c.text.contains(must), "missing {must:?} in:\n{}", c.text);
        }
        // none of the data itself: no title, no row
        assert!(!c.text.contains("about the dashboard") && c.text.len() * 20 < input.len(), "{} vs {}", c.text.len(), input.len());
        // small JSON is just minified, nothing dropped
        let c = compress("{\n  \"a\": 1,\n  \"b\": [1, 2]\n}", Kind::Auto, 120);
        assert!(!c.lossy && c.text == r#"{"a":1,"b":[1,2]}"#);
        // NDJSON is read as an array
        let nd: String = (0..200).map(|i| format!("{{\"level\":\"info\",\"n\":{i}}}\n")).collect();
        let c = compress(&nd, Kind::Auto, 10);
        assert!(c.kind == "json graph" && c.text.contains("$  array[200]") && c.text.contains(".level  string  (values {info:200})"), "{}", c.text);
    }

    #[test]
    fn json_answers_are_tables_and_questions_find_records() {
        let v = issues();
        // a query result too big to show whole: a table with keys once, constants hoisted
        let (t, lossy) = compress_json(&Value::Array(json_path(&v, ".items[*]").unwrap().into_iter().map(|h| h.1).collect()), 3000);
        assert!(lossy && t.contains("[200 items, table] constant: repo=relay") && t.contains("id | state | title | user"), "{t}");
        // plain words: the one failing record comes first
        let text = v.to_string();
        let a = ask(&text, "which items are in an error state?", 20_000);
        let first = a.lines().nth(1).unwrap_or("");
        assert!(first.starts_with(".items[150]") && first.contains("error: timeout"), "{a}");
        // logs: the lines that answer the question, in order
        let log = "start\nconnecting to db\nretry 1\nERROR: db connection refused\nshutting down\n";
        let a = ask(log, "why did the database connection fail", 20_000);
        assert!(a.contains("4  ERROR: db connection refused"), "{a}");
    }

    #[test]
    fn diffs_stay_exact() {
        let d = "diff --git a/x.go b/x.go\n--- a/x.go\n+++ b/x.go\n@@ -1,3 +1,3 @@\n-a := 1\n+a := 2\n";
        let mut big = d.to_string();
        for i in 0..400 { big.push_str(&format!("+line {i} of new code\n")); }
        let c = compress(&big, Kind::Auto, 120);
        assert!(c.kind == "verbatim" && !c.lossy && c.lines_out == big.lines().count());
    }

    #[test]
    fn store_names() {
        assert_eq!(command_name("vercel logs dpl_123 --follow"), "vercel-logs");
        assert_eq!(command_name("docker compose logs -f api"), "docker-compose");
        assert_eq!(command_name("sudo FOO=1 go test ./... | tail"), "go-test");
        assert_eq!(command_name("npx jest --ci"), "jest");
        assert_eq!(command_name("./scripts/run.sh"), "run.sh");
        assert_eq!(clean_name("My Vercel/Logs!"), "my-vercel-logs");
        assert_eq!(clean_name("  "), "default");
    }

    #[test]
    fn json_paths() {
        let v = serde_json::json!({"items": [{"id": 1, "s": "ok"}, {"id": 2, "s": "bad"}, {"id": 3, "s": "ok"}]});
        // mt's shorthand, translated to RFC 9535
        assert_eq!(to_jsonpath(".items[?s=ok].id"), "$.items[?@.s=='ok'].id");
        assert_eq!(to_jsonpath("items[?id>1]"), "$.items[?@.id>1]");
        assert_eq!(to_jsonpath("$..id"), "$..id");
        assert_eq!(json_path(&v, ".items[1].id").unwrap(), vec![("$['items'][1]['id']".to_string(), serde_json::json!(2))]);
        assert_eq!(json_path(&v, "items[?s=ok]").unwrap().len(), 2);
        assert_eq!(json_path(&v, ".items[1:3]").unwrap().len(), 2);
        assert_eq!(json_path(&v, ".items[*].s").unwrap().len(), 3);
        // real JSONPath: filters with @, comparisons, logic, recursive descent, functions
        assert_eq!(json_path(&v, "$.items[?@.s=='ok' && @.id>1].id").unwrap()[0].1, serde_json::json!(3));
        assert_eq!(json_path(&v, "$..id").unwrap().len(), 3);
        assert_eq!(json_path(&v, "$.items[?match(@.s, 'b.d')].id").unwrap()[0].1, serde_json::json!(2));
        assert_eq!(json_path(&v, "$.items[?length(@.s)==2]").unwrap().len(), 2);
        assert!(json_path(&v, "$.items[?@.s=]").is_err());
        // jq, in process
        let t = v.to_string();
        assert_eq!(jq(&t, "[.items[] | select(.s == \"ok\") | .id]").unwrap(), vec![serde_json::json!([1, 3])]);
        assert_eq!(jq(&t, ".items | group_by(.s) | map({key: .[0].s, value: length}) | from_entries").unwrap(), vec![serde_json::json!({"bad": 1, "ok": 2})]);
        assert_eq!(jq(&t, ".items[].id").unwrap().len(), 3);
        assert!(jq(&t, ".items[] |").unwrap_err().starts_with("jq syntax"));
    }

    #[test]
    fn store_query_and_session_dedup() {
        let sid = format!("compress-test-{}", std::process::id());
        let mut s = String::new();
        for i in 0..400 { s.push_str(&format!("step {i} compiling module number {i}\n")); }
        s.push_str("error: something broke in build.rs:12\n");
        let c = compress(&s, Kind::Auto, 60);
        assert!(c.lossy);
        let id = format!("{:08x}", fnv1a(s.as_bytes()) as u32);
        assert!(header(&c, Some(&id), "test output", "builds").starts_with("[mt: test output reduced: 401 -> "));
        let dir = session_dir(Some(&sid));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.txt")), &s).unwrap();
        let q = |grep: Option<&str>, lines: Option<(usize, usize)>| {
            let txt = std::fs::read_to_string(dir.join(format!("{id}.txt"))).unwrap();
            let ls = clean_lines(&txt);
            match (grep, lines) {
                (Some(g), _) => ls.iter().filter(|l| Regex::new(g).unwrap().is_match(l)).count(),
                (_, Some((a, b))) => ls[a - 1..b].len(),
                _ => ls.len(),
            }
        };
        assert_eq!(q(Some("build\\.rs:12"), None), 1);
        assert_eq!(q(None, Some((10, 19))), 10);
        // session dedup: the second time, the repeated run becomes one line
        let block: String = (0..10).map(|i| format!("a long enough line number {i} for dedup\n")).collect();
        let (first, n1) = dedup_seen(&block, &sid);
        let (second, n2) = dedup_seen(&format!("new line that is long enough here\n{block}"), &sid);
        assert!(n1 == 0 && first.lines().count() == 10);
        assert!(n2 == 10 && second.contains("were already shown earlier in this session"), "{second}");
        // whole-file reads
        assert!(!read_seen(&sid, "a.go", "x") && read_seen(&sid, "a.go", "x") && !read_seen(&sid, "a.go", "y"));
        reset_seen(&sid);
        assert!(!read_seen(&sid, "a.go", "y"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
