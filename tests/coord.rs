//! Agent coordination end to end, through the real binary and its hook, in scratch repos:
//! agent identity, briefs, the hook's automatic brief, claims, the change feed, reports, split,
//! and the measured saving: a brief replaces the receiving agent's orientation calls.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One agent: a session, and optionally the subagent id the hook would export.
struct Agent<'a> {
    sid: &'a str,
    agent: Option<&'a str>,
}

fn mt(dir: &Path, who: &Agent, args: &[&str], stdin: &str) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mt"));
    cmd.args(args).current_dir(dir)
        .env("MT_SESSION", who.sid).env("MT_NO_DAEMON", "1").env("MT_SERVER_SCOPE", "off").env_remove("MT_AGENT")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(a) = who.agent { cmd.env("MT_AGENT", a); }
    let mut c = cmd.spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    println!("$ [{}{}] mt {}\n{}", who.sid, who.agent.map(|a| format!("~{a}")).unwrap_or_default(), args.join(" "),
        out.lines().map(|l| format!("  | {l}")).collect::<Vec<_>>().join("\n"));
    (o.status.code().unwrap_or(-1), out)
}

fn hook(dir: &Path, event: &str, input: serde_json::Value) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mt"))
        .args(["hook", event]).current_dir(dir)
        .env("MT_NO_DAEMON", "1").env("MT_SERVER_SCOPE", "off").env_remove("MT_AGENT").env_remove("MT_SESSION")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    c.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    println!("$ mt hook {event}\n  | {out}");
    out
}

fn repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("it-coord-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Command::new("git").arg("init").arg("-q").current_dir(&dir).status().unwrap();
    dir
}

fn write(dir: &Path, f: &str, s: &str) {
    std::fs::write(dir.join(f), s).unwrap();
}

fn read(dir: &Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap_or_default()
}

/// The hash of item [n] in a numbered reply.
fn hash_of(out: &str, n: usize) -> String {
    out.lines().find(|l| l.starts_with(&format!("[{n}] "))).and_then(|l| l.split("h=").nth(1)).and_then(|h| h.split_whitespace().next()).unwrap_or_default().to_string()
}

const A_GO: &str = "package a\n\nfunc One() int {\n\treturn 1\n}\n\nfunc Two() int {\n\treturn 2\n}\n";
const B_GO: &str = "package a\n\nfunc Three() int {\n\treturn One() + 2\n}\n";

#[test]
fn subagents_get_their_own_identity() {
    let dir = repo("id");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    let sid = format!("it-coord-id-{}", std::process::id());
    let parent = Agent { sid: &sid, agent: None };
    let sub = Agent { sid: &sid, agent: Some("a11") };

    // the parent reads One: in its context now
    let (_, out) = mt(&dir, &parent, &["query", "-"], r#"[{"span":"a.go:One"}]"#);
    assert!(out.contains("return 1"), "{out}");
    let (_, out) = mt(&dir, &parent, &["query", "-"], r#"[{"references":"One"}]"#);
    assert!(out.contains("[1] b.go:L3-5 function Three"), "{out}");

    // the subagent shares the session id but is told by MT_AGENT: it gets the body, not
    // "already shown" (it never saw it), and its numbering does not replace the parent's
    let (_, out) = mt(&dir, &sub, &["query", "-"], r#"[{"span":"a.go:One"}]"#);
    assert!(out.contains("return 1") && !out.contains("already shown"), "the subagent must see the body: {out}");
    let (_, out) = mt(&dir, &sub, &["query", "-"], r#"[{"defined_in":"Two"}]"#);
    assert!(out.contains("[1] a.go:L7-9 function Two"), "{out}");
    let (_, out) = mt(&dir, &parent, &["expand", "1"], "");
    assert!(out.contains("b.go:L3-5") && out.contains("return One() + 2"), "the parent's [1] is still Three: {out}");

    // the hook tags a subagent's mt commands with its agent id, and nothing else
    let ev = |cmd: &str, agent: Option<&str>| {
        let mut v = serde_json::json!({"hook_event_name": "PreToolUse", "session_id": sid, "cwd": dir, "tool_name": "Bash", "tool_use_id": "t1", "tool_input": {"command": cmd}});
        if let Some(a) = agent { v["agent_id"] = a.into(); v["agent_type"] = "general-purpose".into(); }
        v
    };
    let out = hook(&dir, "pretool", ev("mt query - <<'EOF'\n[]\nEOF", Some("a77")));
    assert!(out.contains("export MT_AGENT=a77; mt query"), "{out}");
    assert!(!out.contains("permissionDecision"), "tagging must not decide permissions: {out}");
    let out = hook(&dir, "pretool", ev("mt query - <<'EOF'\n[]\nEOF", None));
    assert!(!out.contains("MT_AGENT"), "the parent is not tagged: {out}");
    let out = hook(&dir, "pretool", ev("ls -la", Some("a77")));
    assert!(!out.contains("MT_AGENT"), "non-mt commands are left alone: {out}");
}

#[test]
fn briefs_hand_over_code_in_one_call() {
    let dir = repo("brief");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    let sid = format!("it-coord-brief-{}", std::process::id());
    let parent = Agent { sid: &sid, agent: None };
    let sub = Agent { sid: &sid, agent: Some("a22") };

    let (_, out) = mt(&dir, &parent, &["query", "-"], r#"[{"defined_in":"One"},{"references":"One"}]"#);
    assert!(out.contains("[1]") && out.contains("[2]"), "{out}");
    let (code, out) = mt(&dir, &parent, &["brief", "save", "fix", "last", "a.go:Two", "--note", "make Three use Two"], "");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("3 span(s)"), "{out}");
    let (_, out) = mt(&dir, &parent, &["brief"], "");
    assert!(out.contains("@fix  3 span(s)") && out.contains("make Three use Two"), "{out}");

    // the receiver: every span in one call
    let (code, out) = mt(&dir, &sub, &["expand", "@fix"], "");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("# mt expand: 3 span(s)"), "{out}");
    for body in ["return 1", "return 2", "return One() + 2"] { assert!(out.contains(body), "{body} missing: {out}"); }

    // code moved under the brief: found again by name, and said so
    write(&dir, "a.go", &format!("// header\n{A_GO}"));
    let (_, out) = mt(&dir, &sub, &["expand", "@fix"], "");
    assert!(out.contains("MOVED since the pointer"), "{out}");

    // a whole-file span hands out one hash; it vouches for any lines inside the file
    let (_, out) = mt(&dir, &sub, &["expand", "b.go:L1-5"], "");
    let whole = out.lines().next().map(|_| out.split("h=").nth(1).and_then(|h| h.split_whitespace().next()).unwrap_or("").to_string()).unwrap();
    let (code, out) = mt(&dir, &sub, &["apply"], &format!("@@ replace b.go:L4-4 {whole}\n\treturn One() + 7\n"));
    assert_eq!(code, 0, "{out}");
    assert!(read(&dir, "b.go").contains("One() + 7"));
    // ... and once the file changed it vouches for nothing
    let (code, out) = mt(&dir, &sub, &["apply"], &format!("@@ replace b.go:L4-4 {whole}\n\treturn One() + 8\n"));
    assert_ne!(code, 0, "{out}");
    // `mt span path:L3-5` and `mt span path L3-5`: the forms models write
    let (code, out) = mt(&dir, &sub, &["span", "b.go:L3-5"], "");
    assert!(code == 0 && out.contains("func Three"), "{out}");
    let (code, out) = mt(&dir, &sub, &["span", "b.go", "L4"], "");
    assert!(code == 0 && out.contains("One() + 7"), "{out}");

    // an unknown brief is an error, not silence
    let (code, out) = mt(&dir, &sub, &["expand", "@nope"], "");
    assert_ne!(code, 0);
    assert!(out.contains("@nope: no brief by that name"), "{out}");
}

#[test]
fn the_hook_briefs_a_spawned_agent() {
    let dir = repo("auto");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    std::fs::create_dir_all(dir.join(".mt")).unwrap();
    let sid = format!("it-coord-auto-{}", std::process::id());
    let parent = Agent { sid: &sid, agent: None };
    mt(&dir, &parent, &["query", "-"], r#"[{"span":"a.go:One"},{"span":"b.go:Three"},{"span":"a.go:Two"}]"#);

    let spawn = |prompt: &str| serde_json::json!({"hook_event_name": "PreToolUse", "session_id": sid, "cwd": dir, "tool_name": "Agent", "tool_use_id": "toolu_0123456789abcdef",
        "tool_input": {"description": "fix", "subagent_type": "general-purpose", "prompt": prompt}});
    let out = hook(&dir, "pretool", spawn("Make Two the callee of Three in b.go."));
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("the hook answers JSON");
    let p = v["hookSpecificOutput"]["updatedInput"]["prompt"].as_str().unwrap_or_default();
    assert!(p.starts_with("Make Two the callee of Three in b.go."), "the prompt is kept: {p}");
    assert!(p.contains("mt expand @a-89abcdef"), "{p}");
    assert!(v["hookSpecificOutput"].get("permissionDecision").is_none(), "{out}");
    // the brief holds what the prompt named and the parent read: Three (b.go) and Two, not One
    let sub = Agent { sid: &sid, agent: Some("a33") };
    let (_, out) = mt(&dir, &sub, &["expand", "@a-89abcdef"], "");
    assert!(out.contains("return One() + 2") && out.contains("return 2"), "{out}");
    assert!(!out.contains("return 1\n"), "One was not named: {out}");

    // a prompt that names nothing the parent read gets no brief
    let out = hook(&dir, "pretool", spawn("Write a haiku about rust."));
    assert!(out.trim().is_empty(), "{out}");
}

#[test]
fn claims_stop_a_second_agent_before_it_writes() {
    let dir = repo("lease");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    let sid = format!("it-coord-lease-{}", std::process::id());
    let x = Agent { sid: &sid, agent: Some("ax") };
    let y = Agent { sid: &sid, agent: Some("ay") };

    let (code, out) = mt(&dir, &x, &["claim", "b.go:Three", "--note", "rewriting Three"], "");
    assert_eq!(code, 0, "{out}");
    let (_, out) = mt(&dir, &y, &["claims"], "");
    assert!(out.contains("b.go:L3-5 function Three held by agent ax") && out.contains("rewriting Three"), "{out}");

    // y's batch touches Three: refused, nothing written, not even y's other edit
    let (code, out) = mt(&dir, &y, &["apply"], "@@ find b.go\n\treturn One() + 2\n@@ with\n\treturn One() + 3\n@@ find a.go\n\treturn 2\n@@ with\n\treturn 20\n");
    assert_ne!(code, 0);
    assert!(out.contains("nothing written") && out.contains("held by agent ax"), "{out}");
    assert_eq!(read(&dir, "b.go"), B_GO);
    assert_eq!(read(&dir, "a.go"), A_GO);
    // y can claim elsewhere, not the same span
    let (code, out) = mt(&dir, &y, &["claim", "b.go:L4-4"], "");
    assert_ne!(code, 0, "{out}");
    let (code, _) = mt(&dir, &y, &["claim", "a.go:Two"], "");
    assert_eq!(code, 0);
    // x edits its own claim
    let (code, out) = mt(&dir, &x, &["apply"], "@@ find b.go\n\treturn One() + 2\n@@ with\n\treturn One() + 3\n");
    assert_eq!(code, 0, "{out}");
    // an edit inside x's claim after it moved (y adds a line above it in the same file? no: y edits a.go, unclaimed by x)
    let (code, out) = mt(&dir, &y, &["apply"], "@@ find a.go\n\treturn 2\n@@ with\n\treturn 20\n");
    assert_eq!(code, 0, "{out}");
    // released: y may now edit Three
    let (_, out) = mt(&dir, &x, &["release"], "");
    assert!(out.contains("released 1"), "{out}");
    let (code, out) = mt(&dir, &y, &["apply"], "@@ find b.go\n\treturn One() + 3\n@@ with\n\treturn One() + 4\n");
    assert_eq!(code, 0, "{out}");
    // expired claims hold nothing
    mt(&dir, &x, &["claim", "a.go:One", "--minutes", "0"], "");
    let (code, out) = mt(&dir, &y, &["apply"], "@@ find a.go\n\treturn 1\n@@ with\n\treturn 10\n");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn the_change_feed_tells_an_agent_what_others_changed_in_code_it_read() {
    let dir = repo("feed");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    let sid = format!("it-coord-feed-{}", std::process::id());
    let x = Agent { sid: &sid, agent: Some("ax") };
    let y = Agent { sid: &sid, agent: Some("ay") };

    // x reads One (its first mt call starts its feed)
    mt(&dir, &x, &["query", "-"], r#"[{"span":"a.go:One"}]"#);
    // y changes One and, separately, Three (which x never read)
    let (code, out) = mt(&dir, &y, &["apply"], "@@ find a.go\n\treturn 1\n@@ with\n\treturn 11\n@@ find b.go\n\treturn One() + 2\n@@ with\n\treturn One() + 5\n");
    assert_eq!(code, 0, "{out}");
    // any reply to x now carries the notice
    let (_, out) = mt(&dir, &x, &["query", "-"], r#"[{"defined_in":"Two"}]"#);
    assert!(out.contains("## 1 span(s) you read were changed by another agent since: `mt changes`"), "{out}");
    let (_, out) = mt(&dir, &x, &["changes"], "");
    assert!(out.contains("agent ay:") && out.contains("a.go:L3-5") && out.contains("function One") && out.contains("OVERLAPS code you read"), "{out}");
    assert!(!out.contains("b.go"), "x never read b.go: {out}");
    let (_, out) = mt(&dir, &x, &["changes", "--all"], "");
    assert!(out.contains("nothing new"), "the cursor moved: {out}");
    // an agent's own changes are not news to it
    let (_, out) = mt(&dir, &y, &["changes", "--all"], "");
    assert!(out.contains("nothing new"), "{out}");
}

#[test]
fn reports_let_the_parent_verify_in_one_call() {
    let dir = repo("report");
    write(&dir, "a.go", A_GO);
    write(&dir, "b.go", B_GO);
    let sid = format!("it-coord-report-{}", std::process::id());
    let parent = Agent { sid: &sid, agent: None };
    let sub = Agent { sid: &sid, agent: Some("a44") };
    mt(&dir, &parent, &["brief", "save", "job", "b.go:Three"], "");
    mt(&dir, &sub, &["expand", "@job"], "");
    let (code, out) = mt(&dir, &sub, &["apply"], "@@ find b.go\n\treturn One() + 2\n@@ with\n\treturn Two() + 1\n");
    assert_eq!(code, 0, "{out}");
    // without a report, verify still lists what changed since the brief, and by whom
    let (_, out) = mt(&dir, &parent, &["brief", "verify", "job"], "");
    assert!(out.contains("no report yet") && out.contains("b.go:L3-5") && out.contains("by agent a44"), "{out}");
    let (code, out) = mt(&dir, &sub, &["brief", "report", "job", "--note", "Three calls Two now", "-"],
        r#"[{"id":"calls","references":"Two","expect":["b.go:Three"]},{"id":"gone","references":"One","expect":[]}]"#);
    assert_eq!(code, 0, "{out}");

    let (_, out) = mt(&dir, &parent, &["brief", "verify", "job"], "");
    assert!(out.contains("report by agent a44") && out.contains("Three calls Two now"), "{out}");
    assert!(out.contains("b.go:L3-5") && out.contains("by agent a44") && out.contains("as written"), "{out}");
    assert!(out.contains("calls ✓") || out.contains("calls ✓"), "{out}");
    let (_, out2) = mt(&dir, &parent, &["expand", "@job/changes"], "");
    assert!(out2.contains("return Two() + 1"), "{out2}");

    // the code moves on after the report: verify says so
    write(&dir, "b.go", "package a\n\nfunc Three() int {\n\treturn One() + 9\n}\n");
    let (_, out) = mt(&dir, &parent, &["brief", "verify", "job"], "");
    assert!(out.contains("CHANGED again since"), "{out}");
    assert!(out.contains("calls ✗"), "{out}");
}

#[test]
fn split_finds_independent_slices() {
    let dir = repo("split");
    write(&dir, "x1.go", "package p\n\nfunc Xalpha() int {\n\treturn Xbeta()\n}\n");
    write(&dir, "x2.go", "package p\n\nfunc Xbeta() int {\n\treturn 1\n}\n");
    write(&dir, "y1.go", "package p\n\nfunc Yalpha() int {\n\treturn Ybeta()\n}\n");
    write(&dir, "y2.go", "package p\n\nfunc Ybeta() int {\n\treturn 2\n}\n");
    write(&dir, "z.go", "package p\n\nfunc Zonly() int {\n\treturn 3\n}\n");
    let sid = format!("it-coord-split-{}", std::process::id());
    let me = Agent { sid: &sid, agent: None };

    let (code, out) = mt(&dir, &me, &["split", "x1.go", "x2.go", "y1.go", "y2.go", "--agents", "2", "--name", "job"], "");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("into 2 independent slice(s)"), "{out}");
    // coupled files stay together
    let slice_of = |f: &str| out.lines().scan(0, |cur, l| { if let Some(n) = l.strip_prefix("slice ") { *cur = n.chars().next().unwrap_or('0').to_digit(10).unwrap_or(0); } Some((*cur, l.trim().to_string())) }).find(|(_, l)| l == f).map(|(c, _)| c).unwrap();
    assert_eq!(slice_of("x1.go"), slice_of("x2.go"), "{out}");
    assert_eq!(slice_of("y1.go"), slice_of("y2.go"), "{out}");
    assert_ne!(slice_of("x1.go"), slice_of("y1.go"), "{out}");
    let (_, e) = mt(&dir, &me, &["expand", "@job-1"], "");
    assert!(e.contains("# mt expand: 2 span(s)"), "{e}");

    // more agents than independent groups: said so, never a split through coupled code
    let (_, out) = mt(&dir, &me, &["split", "x1.go", "x2.go", "y1.go", "y2.go", "--agents", "3", "--name", "j3"], "");
    assert!(out.contains("into 2 independent slice(s) (asked for 3"), "{out}");
    // symbols work as targets, and brief their definitions only
    let (_, out) = mt(&dir, &me, &["split", "Xalpha", "Zonly", "--agents", "2", "--name", "sym"], "");
    assert!(out.contains("into 2 independent slice(s)"), "{out}");
}

/// The optimization itself, measured on the mechanics: what a subagent does to rebuild the
/// parent's context without a brief (locate each named symbol, then read it: the pattern in real
/// transcripts, a median 12 calls before the first edit), against one `mt expand @brief`.
#[test]
fn a_brief_replaces_the_receivers_orientation_calls() {
    let dir = repo("bench");
    // 8 functions over 4 files: the task names all of them
    let names: Vec<String> = (0..8).map(|i| format!("Func{i}")).collect();
    for f in 0..4 {
        let body: String = (0..2).map(|k| { let i = f * 2 + k; format!("func Func{i}(x int) int {{\n\ty := x * {i}\n\treturn y + {i}\n}}\n\n") }).collect();
        write(&dir, &format!("f{f}.go"), &format!("package p\n\n{body}"));
    }
    let sid = format!("it-coord-bench-{}", std::process::id());
    let parent = Agent { sid: &sid, agent: None };
    let cold = Agent { sid: &sid, agent: Some("acold") };
    let briefed = Agent { sid: &sid, agent: Some("abrief") };

    // the parent already read them (that is when it decides to delegate)
    let q: Vec<String> = names.iter().map(|n| format!(r#"{{"defined_in":"{n}"}}"#)).collect();
    let (_, out) = mt(&dir, &parent, &["query", "-"], &format!("[{}]", q.join(",")));
    assert!(out.contains("[8]"), "{out}");
    mt(&dir, &parent, &["brief", "save", "task", "last"], "");

    // without a brief: per name, find where it is, then read it (mt's own fastest per-item path)
    let mut cold_calls = 0;
    let mut cold_bytes = 0;
    let mut cold_bodies = 0;
    for n in &names {
        let (_, loc) = mt(&dir, &cold, &["query", "-"], &format!(r#"[{{"defined_in":"{n}"}}]"#));
        cold_calls += 1;
        cold_bytes += loc.len();
        let (_, body) = mt(&dir, &cold, &["expand", "1"], "");
        cold_calls += 1;
        cold_bytes += body.len();
        if body.contains(&format!("func {n}(")) { cold_bodies += 1; }
    }
    // with the brief: one call
    let (_, out) = mt(&dir, &briefed, &["expand", "@task"], "");
    let brief_calls = 1;
    let brief_bodies = names.iter().filter(|n| out.contains(&format!("func {n}("))).count();
    println!("orientation: without brief {cold_calls} calls, {cold_bytes} bytes, {cold_bodies}/8 bodies; with brief {brief_calls} call, {} bytes, {brief_bodies}/8 bodies", out.len());
    assert_eq!(cold_bodies, 8);
    assert_eq!(brief_bodies, 8, "the brief carries every body: {out}");
    assert!(cold_calls >= 8 * brief_calls * 2, "{cold_calls} vs {brief_calls}");
    assert!(out.len() < cold_bytes, "one reply is smaller than the per-item replies: {} vs {cold_bytes}", out.len());
}
