//! What one answer cost mt and what it saved the model: the `[mt] work:` line at the end of every
//! `query`, `context`, `expand` and `search`, and the "saved" event the status line totals.
//!
//! Counters are per thread (the server answers each request on its own thread), so concurrent
//! requests never mix their counts. A request takes a `Mark` when it starts; the line reports the
//! difference since then.

use std::cell::Cell;
use std::path::Path;
use std::time::Instant;

thread_local! {
    static LOOKUPS: Cell<u64> = const { Cell::new(0) };
    static ROWS: Cell<u64> = const { Cell::new(0) };
    static FILES: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

/// One index lookup (a query against the SQLite index).
pub fn lookup() { LOOKUPS.with(|c| c.set(c.get() + 1)); }
/// Rows a lookup returned.
pub fn rows(n: usize) { ROWS.with(|c| c.set(c.get() + n as u64)); }
/// Files read from disk, and their bytes.
pub fn read(files: usize, bytes: usize) {
    FILES.with(|c| c.set(c.get() + files as u64));
    BYTES.with(|c| c.set(c.get() + bytes as u64));
}

/// Everything this thread has counted: a worker thread returns it with its result, and the
/// thread answering `absorb`s it, so a request's work is counted however many threads it used.
pub fn snapshot() -> [u64; 4] { [LOOKUPS.with(Cell::get), ROWS.with(Cell::get), FILES.with(Cell::get), BYTES.with(Cell::get)] }

pub fn absorb(s: [u64; 4]) {
    LOOKUPS.with(|c| c.set(c.get() + s[0]));
    ROWS.with(|c| c.set(c.get() + s[1]));
    FILES.with(|c| c.set(c.get() + s[2]));
    BYTES.with(|c| c.set(c.get() + s[3]));
}

pub struct Mark { t: Instant, lookups: u64, rows: u64, files: u64, bytes: u64 }

/// Where a request starts counting.
pub fn mark() -> Mark {
    Mark { t: Instant::now(), lookups: LOOKUPS.with(Cell::get), rows: ROWS.with(Cell::get), files: FILES.with(Cell::get), bytes: BYTES.with(Cell::get) }
}

/// A count with thousands separators: 1,234,567.
pub fn n(x: u64) -> String {
    let s = x.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out
}

/// The line that ends an answer: the work done since `m`, and the reply's size against reading
/// whole the files it covers (an estimate at 4 characters a token). Also records the saving for
/// the status line. Ends with a newline.
pub fn footer(root: &Path, session: Option<&str>, cmd: &str, reply: &str, m: &Mark) -> String {
    let ms = m.t.elapsed().as_secs_f64() * 1e3;
    let (lookups, rows) = (LOOKUPS.with(Cell::get) - m.lookups, ROWS.with(Cell::get) - m.rows);
    let (files, bytes) = (FILES.with(Cell::get) - m.files, BYTES.with(Cell::get) - m.bytes);
    // the files the reply covers, each once, as they are on disk
    let mut paths: Vec<String> = crate::events::touched(reply, 400).iter().filter_map(|t| t["path"].as_str().map(String::from)).collect();
    paths.sort();
    paths.dedup();
    let whole: u64 = paths.iter().filter_map(|p| std::fs::metadata(root.join(p)).ok()).map(|md| md.len()).sum();
    let (reply_tok, whole_tok) = (reply.len() as u64 / 4, whole / 4);
    let mut line = format!("[mt] work: {} index lookup{} ({} rows) · {} file{} read ({} KB) · {:.0} ms",
        n(lookups), if lookups == 1 { "" } else { "s" }, n(rows), n(files), if files == 1 { "" } else { "s" }, n(bytes.div_ceil(1024)), ms);
    let saved = whole_tok.saturating_sub(reply_tok);
    if saved > 0 {
        line.push_str(&format!(" · this reply ≈ {} tok vs ≈ {} tok to read the {} file{} it covers whole: ≈ {} tok saved ({}%, estimate)",
            n(reply_tok), n(whole_tok), paths.len(), if paths.len() == 1 { "" } else { "s" }, n(saved), saved * 100 / whole_tok.max(1)));
    }
    crate::events::emit(root, serde_json::json!({
        "kind": "saved", "cmd": cmd, "session": session.unwrap_or("").chars().take(8).collect::<String>(),
        "tokens": saved, "reply_tokens": reply_tok, "whole_tokens": whole_tok, "lookups": lookups, "rows": rows, "files": files, "ms": (ms * 100.0).round() / 100.0,
    }));
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_since_the_mark_and_sizes_the_saving() {
        let root = std::env::temp_dir().join(format!("mt-work-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.go"), "x".repeat(40_000)).unwrap();
        super::lookup();
        let m = super::mark();
        super::lookup();
        super::rows(12);
        super::read(1, 2048);
        let f = super::footer(&root, Some("s1"), "context", "[1] src/a.go:L3-9 function A\n", &m);
        assert!(f.starts_with("[mt] work: 1 index lookup (12 rows) · 1 file read (2 KB) · "), "{f}");
        assert!(f.contains("≈ 10,000 tok to read the 1 file it covers whole") && f.contains("tok saved (99%, estimate)"), "{f}");
        assert_eq!(super::n(1234567), "1,234,567");
        let _ = std::fs::remove_dir_all(&root);
    }
}
