//! `mt init [dir] [--dry-run] [--remove]`: tell every agent that works in a repo to use mt for
//! code. Claude Code reads CLAUDE.md, Codex / opencode / most others AGENTS.md, Gemini GEMINI.md,
//! Cursor `.cursor/rules/*.mdc`. Each gets one block between `<!-- mt:start -->` and
//! `<!-- mt:end -->`: written if missing, replaced if present (running it twice changes nothing),
//! removed with --remove. Everything outside the markers is left exactly as it was.

use std::path::{Path, PathBuf};

const START: &str = "<!-- mt:start -->";
const END: &str = "<!-- mt:end -->";

/// The instructions, the same for every agent. Short on purpose: it is read on every turn.
pub fn block(bin: &str) -> String {
    format!(
        "{START}
## Code: use mt, and only mt

This repo is set up for **mt**{bin}, a code multitool that answers from a live index of the whole
repo. Use it for **all** code work. It returns the code, its callers, its tests and its data with
spans and hashes, in one call, and changes many files in one checked call.

- **Read and search code with mt, not with grep, sed, cat, head, Read, or another code-search tool**
  (graft, graphify, codegraph): where instructions elsewhere in this file say to use one of those
  first for code, mt takes precedence in this repo.
- **Map, then expand once.** `mt context <Name> --map` lists the target, every caller, its unit and
  integration tests, the types and tables it touches, one line each with an id. Read what the task
  needs in one call: `mt expand 3 7 12`.
- **Don't know the name?** `mt search \"how are audit rows written\"` finds the code by meaning.
- **Many questions, one call:** `mt query` takes a batch of questions and claims (`expect`).
- **Change code in one batch:** `mt apply` writes every edit, then formats, builds and tests, and
  restores the files if a gate step fails.
- **Long output** (builds, tests, logs, JSON, API responses): `mt run -- <cmd>` or `<cmd> | mt compress`;
  then `mt out <id> --json '$.path'`, `--jq '<filter>'` or `mt out <id> \"<question>\"` for only what you
  need. Never paste a whole JSON document into the conversation.
- The full reference: `mt ref` (sections: `mt ref <name>`).
- Plain tools stay for what is not code: literal text in docs or config, and running programs.
{END}
"
    )
}

/// Where each agent looks, and how its file starts when mt creates it.
fn targets(dir: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut v = vec![(dir.join("CLAUDE.md"), ""), (dir.join("AGENTS.md"), ""), (dir.join("GEMINI.md"), "")];
    // Cursor only when the repo uses it: a rule file needs its own front matter to always apply
    if dir.join(".cursor").is_dir() {
        v.push((dir.join(".cursor/rules/mt.mdc"), "---\ndescription: Use mt for all code reading, searching and editing\nalwaysApply: true\n---\n\n"));
    }
    v
}

/// `text` with the mt block replaced (or appended), or removed.
pub fn apply_block(text: &str, block: &str, remove: bool) -> String {
    let (before, after) = match (text.find(START), text.find(END)) {
        (Some(a), Some(b)) if b > a => (&text[..a], &text[b + END.len()..]),
        _ => (text, ""),
    };
    let before = before.trim_end();
    let after = after.trim_start_matches('\n');
    if remove {
        let mut s = before.to_string();
        if !after.trim().is_empty() { if !s.is_empty() { s.push_str("\n\n"); } s.push_str(after); }
        if !s.is_empty() && !s.ends_with('\n') { s.push('\n'); }
        return s;
    }
    let mut s = String::new();
    if !before.is_empty() { s.push_str(before); s.push_str("\n\n"); }
    s.push_str(block);
    if !after.trim().is_empty() { s.push('\n'); s.push_str(after); }
    s
}

pub fn run(dir: &Path, dry_run: bool, remove: bool) -> (bool, String) {
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("mt").is_file()));
    let bin = if on_path { String::new() } else {
        std::env::current_exe().ok().map(|p| format!(" (`{}`; not on PATH, call it by this path)", p.display())).unwrap_or_default()
    };
    let block = block(&bin);
    let mut out = String::new();
    for (path, header) in targets(dir) {
        let old = std::fs::read_to_string(&path).ok();
        if remove && old.as_deref().is_none_or(|t| !t.contains(START)) { continue; }
        let base = old.clone().unwrap_or_else(|| header.to_string());
        let new = apply_block(&base, &block, remove);
        let rel = path.strip_prefix(dir).unwrap_or(&path).display().to_string();
        let what = match (&old, remove) {
            (_, true) if new.trim().is_empty() => "removed (the file held only the mt block)",
            (_, true) => "mt block removed",
            (None, _) => "created",
            (Some(o), _) if *o == new => "unchanged",
            (Some(o), _) if o.contains(START) => "mt block replaced",
            _ => "mt block added",
        };
        out.push_str(&format!("{rel}: {what}\n"));
        if dry_run || old.as_deref() == Some(new.as_str()) { continue; }
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        let r = if remove && new.trim().is_empty() { std::fs::remove_file(&path) } else { std::fs::write(&path, &new) };
        if let Err(e) = r { return (false, format!("{out}mt init: {}: {e}\n", path.display())); }
    }
    if dry_run { out.push_str("(dry run: nothing written)\n"); }
    (true, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_added_replaced_and_removed_leaving_the_rest() {
        let other = "<!-- graft:start -->\nUse graft first.\n<!-- graft:end -->\n";
        let b1 = block("");
        let once = apply_block(other, &b1, false);
        assert!(once.starts_with(other.trim_end()) && once.contains("## Code: use mt, and only mt") && once.contains("mt takes precedence"));
        // twice: the same text
        assert_eq!(apply_block(&once, &b1, false), once);
        // a newer block replaces the old one, in place
        let b2 = block(" (`/x/mt`)");
        let twice = apply_block(&once, &b2, false);
        assert_eq!(twice.matches(START).count(), 1);
        assert!(twice.contains("/x/mt") && twice.contains("Use graft first."));
        // removed: what was there before, exactly
        assert_eq!(apply_block(&twice, &b2, true), other);
        // a file holding only the block becomes empty
        assert_eq!(apply_block(&b1, &b1, true), "");
    }

    #[test]
    fn init_writes_every_agent_file_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("mt-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# Repo notes\n\nKeep this.\n").unwrap();
        let (ok, out) = run(&dir, false, false);
        assert!(ok && out.contains("CLAUDE.md: created") && out.contains("AGENTS.md: mt block added") && out.contains(".cursor/rules/mt.mdc: created"), "{out}");
        assert!(std::fs::read_to_string(dir.join("AGENTS.md")).unwrap().starts_with("# Repo notes\n\nKeep this.\n\n<!-- mt:start -->"));
        assert!(std::fs::read_to_string(dir.join(".cursor/rules/mt.mdc")).unwrap().starts_with("---\ndescription:"));
        let (_, again) = run(&dir, false, false);
        assert!(again.lines().all(|l| l.ends_with("unchanged")), "{again}");
        let (_, rm) = run(&dir, false, true);
        assert!(rm.contains("AGENTS.md: mt block removed") && rm.contains("CLAUDE.md: removed"), "{rm}");
        assert_eq!(std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(), "# Repo notes\n\nKeep this.\n");
        assert!(!dir.join("CLAUDE.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
