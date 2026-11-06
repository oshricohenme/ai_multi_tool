//! The virtual context end to end, through the real binary and its hooks, in a scratch repo:
//! a query hands out numbered items, `#n` codes edit and move code without retyping it,
//! `mt show` reaches the user and not the model, and a compaction puts back the working set.

use std::io::Write;
use std::process::{Command, Stdio};

fn mt(dir: &std::path::Path, sid: &str, args: &[&str], stdin: &str) -> (i32, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mt"))
        .args(args).current_dir(dir)
        .env("MT_SESSION", sid).env("MT_NO_DAEMON", "1").env("MT_SERVER_SCOPE", "off")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    println!("$ mt {}\n{}", args.join(" "), out.lines().map(|l| format!("  | {l}")).collect::<Vec<_>>().join("\n"));
    (o.status.code().unwrap_or(-1), out)
}

fn read(dir: &std::path::Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap_or_else(|_| "<missing>".into())
}

#[test]
fn codes_show_and_compaction() {
    let sid = format!("it-vctx-{}", std::process::id());
    let dir = std::env::temp_dir().join(&sid);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Command::new("git").arg("init").arg("-q").current_dir(&dir).status().unwrap();
    std::fs::write(dir.join("a.go"), "package a\n\nfunc One() int {\n\treturn 1\n}\n").unwrap();
    std::fs::write(dir.join("b.go"), "package a\n\nfunc Three() int {\n\treturn One() + 2\n}\n").unwrap();

    // 1. a query numbers what it found: [1] is Three, with its hash
    let (code, out) = mt(&dir, &sid, &["query", "-"], r#"[{"id":"c","references":"One"}]"#);
    assert_eq!(code, 0);
    assert!(out.contains("[1] b.go:L3-5 function Three"), "{out}");

    // 1b. a span read, so the session has a body in context (One, in a.go)
    let (code, _) = mt(&dir, &sid, &["query", "-"], r#"[{"id":"s","span":"a.go:One"}]"#);
    assert_eq!(code, 0);
    let (_, out) = mt(&dir, &sid, &["query", "-"], r#"[{"id":"c","references":"One"}]"#);
    assert!(out.contains("[1] b.go:L3-5 function Three"), "{out}");

    // 2. a code moves it: one line from the model, mt writes the function into c.go
    let (code, out) = mt(&dir, &sid, &["apply"], "@@ move #1 to c.go\n");
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&dir, "c.go"), "func Three() int {\n\treturn One() + 2\n}\n");
    assert_eq!(read(&dir, "b.go"), "package a\n\n");

    // 2b. into a new file that needs a header: create the header, then move into it
    std::fs::write(dir.join("d.go"), "package a\n\nfunc Four() int {\n\treturn 4\n}\n\nfunc Five() int {\n\treturn 5\n}\n").unwrap();
    let (_, out) = mt(&dir, &sid, &["query", "-"], r#"[{"id":"f","defined_in":"Four"},{"id":"g","defined_in":"Five"}]"#);
    assert!(out.contains("[1] d.go:L3-5 function Four") && out.contains("[2] d.go:L7-9 function Five"), "{out}");
    // (the second move uses the spelled-out form a model writes: span, hash, destination)
    let five = out.lines().find(|l| l.starts_with("[2] d.go:L7-9")).and_then(|l| l.split("h=").nth(1)).and_then(|h| h.split_whitespace().next()).unwrap().to_string();
    let (code, out) = mt(&dir, &sid, &["apply"], &format!("@@ create e.go\npackage a\n\n@@ move #1 to e.go\n@@ move d.go:L7-9 {five} to e.go\n"));
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&dir, "e.go"), "package a\n\nfunc Four() int {\n\treturn 4\n}\n\nfunc Five() int {\n\treturn 5\n}\n");
    assert_eq!(read(&dir, "d.go").trim(), "package a");

    // 3. #1 (Four, d.go:L3-5) now names lines that are gone: refused, nothing written
    let before = read(&dir, "d.go");
    let (code, out) = mt(&dir, &sid, &["apply"], "@@ replace #1\nfunc X() {}\n");
    assert_ne!(code, 0);
    assert!(out.contains("nothing written"), "{out}");
    assert_eq!(read(&dir, "d.go"), before);

    // 4. mt show: the hook gives the text to the user (systemMessage) and the model a receipt
    let table: String = std::iter::once("| n | n² |\n|---|---|\n".to_string()).chain((1..=30).map(|i| format!("| {i} | {} |\n", i * i))).collect();
    std::fs::write(dir.join("t.md"), &table).unwrap();
    let (_, shown) = mt(&dir, &sid, &["show", "t.md", "--title", "Squares"], "");
    let event = serde_json::json!({"session_id": sid, "tool_name": "Bash", "tool_input": {"command": "mt show t.md --title Squares"},
        "tool_response": {"stdout": shown, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}});
    let (_, hook_out) = mt(&dir, &sid, &["hook", "posttool"], &event.to_string());
    let h: serde_json::Value = serde_json::from_str(&hook_out).expect("hook prints JSON");
    let user = h["systemMessage"].as_str().unwrap();
    let model = h["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap();
    assert!(user.starts_with("Squares\n") && user.contains("| 30 | 900 |"), "{user}");
    assert!(!model.contains("900") && model.contains("shown to the user directly"), "{model}");

    // 5. a compaction: the working set comes back as handles; the numbered items do not
    let start = serde_json::json!({"session_id": sid, "source": "compact", "cwd": dir});
    let (_, ss) = mt(&dir, &sid, &["hook", "session-start"], &start.to_string());
    let ctx = serde_json::from_str::<serde_json::Value>(&ss).unwrap()["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.contains("## Working set from before the compaction"), "{ctx}");
    let changed = ctx.lines().find(|l| l.starts_with("- changed with mt apply:")).unwrap_or("");
    assert!(["b.go", "c.go", "d.go", "e.go"].iter().all(|f| changed.contains(f)), "{ctx}");
    let spans = ctx.lines().find(|l| l.starts_with("- read with mt (spans):")).unwrap_or("");
    assert!(spans.contains("a.go:L3-5@"), "{ctx}");
    assert!(ctx.contains("pointers in your last mt reply: d.go:L3-5@") && ctx.contains("(function Four)"), "{ctx}");
    let (code, out) = mt(&dir, &sid, &["apply"], "@@ delete #1\n");
    assert_ne!(code, 0);
    assert!(out.contains("has no item [1]"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("/tmp/mt-brief-{}.txt", &sid[..sid.len().min(12)])));
}
