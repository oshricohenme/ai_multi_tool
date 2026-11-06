//! The model's virtual context: what it has been shown, by handle, so mt can refer to things
//! instead of the model retyping them, and can rebuild the working set after a compaction.
//!
//! - **Codes**: `#3` (or `3`) in an `mt apply` span means item [3] of the session's last
//!   mt query/context reply, with its hash. `@@ copy`/`@@ move` take them too, so moving code is
//!   one line, not the code retyped.
//! - **`mt show`**: text for the human. The PostToolUse hook puts it in front of the user as a
//!   `systemMessage` (verified on Claude Code 2.1.319: shown to the user, never sent to the model;
//!   whole up to ~9,000 characters) and the model gets a one-line receipt.
//! - **Working set**: after a compaction the summary is prose; this puts back the exact handles
//!   (files changed, spans read) so the model can `mt expand` instead of searching again.

use std::path::PathBuf;

/// `#3` or `3` -> 3. Anything else (a path, a span) is not a code.
pub fn code(s: &str) -> Option<usize> {
    let t = s.trim();
    let d = t.strip_prefix('#').unwrap_or(t);
    (!d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

/// Item `n` of the session's last numbered reply: (`path:Ls-e`, hash).
pub fn pointer(sid: Option<&str>, n: usize) -> Result<(String, String), String> {
    let Some(sess) = crate::query::session_file(sid) else {
        return Err(format!("#{n}: no session (neither MT_SESSION nor CLAUDE_CODE_SESSION_ID is set), so codes mean nothing here; pass path:Ls-e"));
    };
    let table = std::fs::read_to_string(sess.with_extension("ptrs")).unwrap_or_default();
    table.lines().map(|l| l.split('\t').collect::<Vec<_>>()).find(|r| r.len() == 7 && r[0] == n.to_string())
        .map(|r| (format!("{}:L{}-{}", r[1], r[2], r[3]), r[6].to_string()))
        .ok_or_else(|| format!("#{n}: the last mt query/context reply in this session has no item [{n}]; pass path:Ls-e"))
}

fn written_file(sid: Option<&str>) -> Option<PathBuf> {
    crate::query::session_file(sid).map(|p| p.with_extension("written"))
}

/// Remember the files an `mt apply` changed (kept across compactions: it stays true).
pub fn note_written(sid: Option<&str>, paths: &[String]) {
    let Some(f) = written_file(sid) else { return };
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
        use std::io::Write;
        let _ = fh.write_all(paths.iter().map(|p| format!("{p}\n")).collect::<String>().as_bytes());
    }
}

/// Newest first, each once.
fn newest_unique(items: impl DoubleEndedIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.rev().filter(|x| seen.insert(x.clone())).collect()
}

/// The `n` spans this session read most recently with mt (newest first), as `path:Ls-e@hash`
/// handles: what to put back, with its current code, after a compaction.
pub fn recent_spans(sid: &str, n: usize) -> Vec<String> {
    let read = |p: Option<PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    newest_unique(read(crate::query::session_file(Some(sid))).lines().filter_map(|l| {
        let r: Vec<&str> = l.split('|').collect();
        let (s, e) = (r.get(1)?.parse::<usize>().ok()?, r.get(2)?.parse::<usize>().ok()?);
        Some(format!("{}:L{}-{}@{}", r[0], s + 1, e + 1, r.get(3)?))
    }).collect::<Vec<_>>().into_iter()).into_iter().take(n).collect()
}

/// The handles the session worked with, for the context after a compaction. Empty when none.
pub fn working_set(sid: &str, max_chars: usize) -> String {
    let sid = Some(sid);
    let read = |p: Option<PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let written = newest_unique(read(written_file(sid)).lines().map(String::from).collect::<Vec<_>>().into_iter());
    // spans mt query/context showed: path|start0|end0|hash
    let spans = newest_unique(read(crate::query::session_file(sid)).lines().filter_map(|l| {
        let r: Vec<&str> = l.split('|').collect();
        let (s, e) = (r.get(1)?.parse::<usize>().ok()?, r.get(2)?.parse::<usize>().ok()?);
        Some(format!("{}:L{}-{}@{}", r[0], s + 1, e + 1, r.get(3)?))
    }).collect::<Vec<_>>().into_iter());
    // the last reply's numbered items: shown as pointers, not bodies, so the seen log lacks them
    let pointers: Vec<String> = read(crate::query::session_file(sid).map(|p| p.with_extension("ptrs"))).lines().filter_map(|l| {
        let r: Vec<&str> = l.split('\t').collect();
        (r.len() == 7).then(|| format!("{}:L{}-{}@{} ({} {})", r[1], r[2], r[3], r[6], r[4], r[5]))
    }).collect();
    let whole = newest_unique(read(Some(crate::compress::session_path(sid, "reads.tsv"))).lines().filter_map(|l| l.split('\t').next().map(String::from)).collect::<Vec<_>>().into_iter());
    if written.is_empty() && spans.is_empty() && pointers.is_empty() && whole.is_empty() { return String::new(); }
    let mut s = String::from("## Working set from before the compaction\nThe summary is prose; these are the exact handles, newest first, without bodies. Read only what the next step needs, together: `mt expand <handle> <handle>` (hash-checked; a moved definition is found by name).\n");
    let add = |label: &str, items: &[String], s: &mut String| {
        if items.is_empty() { return; }
        let mut line = format!("- {label}:");
        let mut n = 0;
        for it in items {
            if s.len() + line.len() + it.len() + 40 > max_chars { break; }
            line.push(' ');
            line.push_str(it);
            n += 1;
        }
        if n < items.len() { line.push_str(&format!(" … +{} older", items.len() - n)); }
        s.push_str(&line);
        s.push('\n');
    };
    add("changed with mt apply", &written, &mut s);
    add("read with mt (spans)", &spans, &mut s);
    add("pointers in your last mt reply", &pointers, &mut s);
    add("read whole", &whole, &mut s);
    s
}

// ------------------------------------------------------------------ mt show

/// What a `systemMessage` can carry and still arrive whole (measured: ~9,000 chars inline; past
/// the 10,000-character hook limit Claude Code swaps it for a 2 KB preview).
pub const SHOW_LIMIT: usize = 8_500;
const MARK: &str = "[mt show] ";

/// `mt show <file>|-  [--title T]`: keep the text in a file and print it after a marker line.
/// With the hook, the user sees it and the model gets a receipt; without, it is ordinary output.
pub fn show(src: Option<&str>, title: &str) -> (bool, String) {
    let text = match src {
        None | Some("-") => { let mut s = String::new(); let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s); s }
        Some(p) => match std::fs::read_to_string(p) { Ok(t) => t, Err(e) => return (false, format!("mt show: {p}: {e}")) },
    };
    let dir = std::env::temp_dir().join("mt-show");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let file = dir.join(format!("{stamp}.txt"));
    let body = if title.is_empty() { text.clone() } else { format!("{title}\n{text}") };
    if let Err(e) = std::fs::write(&file, &body) { return (false, format!("mt show: {}: {e}", file.display())); }
    (true, format!("{MARK}{}\n{body}", file.display()))
}

/// For the hook: Bash stdout from `mt show` -> (what the user sees, what the model gets).
pub fn show_split(stdout: &str) -> Option<(String, String)> {
    let path = stdout.strip_prefix(MARK)?.lines().next()?.trim();
    let text = std::fs::read_to_string(path).ok()?;
    let (n, c, was_cut) = (text.lines().count(), text.chars().count(), text.len() > SHOW_LIMIT);
    let user = if !was_cut {
        text
    } else {
        let cut = (0..SHOW_LIMIT).rev().find(|&i| text.is_char_boundary(i) && text.as_bytes().get(i) == Some(&b'\n')).unwrap_or(0);
        format!("{}\n… {} more lines; the full text is in {path}", &text[..cut], text[cut..].lines().count())
    };
    let cut_note = if was_cut { format!(" The user saw the first part; the full text is in {path}.") } else { String::new() };
    let model = format!("[mt show: {n} lines ({c} chars) were shown to the user directly and are not in your context.{cut_note} Don't repeat them; refer to them in a sentence.]");
    Some((user, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(code("#3"), Some(3));
        assert_eq!(code(" 12 "), Some(12));
        assert_eq!(code("a.go:L1-9"), None);
        assert_eq!(code("#"), None);
        assert_eq!(code("#3a"), None);
    }

    #[test]
    fn show_round_trip_and_cut() {
        let (ok, out) = show(Some("/nonexistent-mt-show"), "T");
        assert!(!ok && out.contains("nonexistent"));
        let dir = std::env::temp_dir().join(format!("mt-show-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("s.txt");
        std::fs::write(&small, "a\nb\n").unwrap();
        let (ok, out) = show(Some(&small.display().to_string()), "");
        assert!(ok);
        let (user, model) = show_split(&out).unwrap();
        assert_eq!(user, "a\nb\n");
        assert!(model.contains("2 lines") && !model.contains("full text"));
        let big = dir.join("b.txt");
        std::fs::write(&big, (0..400).map(|i| format!("row {i:04} {}\n", "x".repeat(40))).collect::<String>()).unwrap();
        let (_, out) = show(Some(&big.display().to_string()), "");
        let (user, model) = show_split(&out).unwrap();
        assert!(user.len() < SHOW_LIMIT + 200 && user.contains("more lines; the full text is in"));
        assert!(model.contains("400 lines") && model.contains("full text"));
        assert!(show_split("plain output").is_none());
    }

    #[test]
    fn working_set_lists_handles_newest_first() {
        let sid = format!("vctx-test-{}", std::process::id());
        let sess = crate::query::session_file(Some(&sid)).unwrap();
        std::fs::write(&sess, "a.go|0|8|ab12\nb.go|4|9|cd34\na.go|0|8|ab12\n").unwrap();
        note_written(Some(&sid), &["x.rs".into(), "y.rs".into()]);
        let ws = working_set(&sid, 4000);
        assert!(ws.contains("changed with mt apply: y.rs x.rs"), "{ws}");
        assert!(ws.contains("read with mt (spans): a.go:L1-9@ab12 b.go:L5-10@cd34\n"), "{ws}");
        assert!(working_set("vctx-test-none", 4000).is_empty());
        let _ = std::fs::remove_file(&sess);
        let _ = std::fs::remove_file(sess.with_extension("written"));
    }
}
