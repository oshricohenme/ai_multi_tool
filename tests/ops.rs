//! `mt build`, `mt rebuild`, `mt logs`, through the real binary and a real daemon (on a private
//! socket, so nothing else is touched).

use std::path::Path;
use std::process::{Command, Stdio};

fn mt(dir: &Path, sock: &str, args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_mt")).current_dir(dir).args(args)
        .env("MT_SOCKET", sock).env("MT_SOCKET_ROOT", dir).env("MT_SESSION", "ops-it")
        .env_remove("MT_NO_DAEMON").env_remove("CLAUDE_CODE_SESSION_ID")
        .stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
}

#[test]
fn build_rebuild_and_logs() {
    let dir = std::env::temp_dir().join(format!("mt-ops-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/ops\n").unwrap();
    std::fs::write(dir.join("src/a.go"), "package src\n\nfunc One() int {\n\treturn Two()\n}\n\nfunc Two() int {\n\treturn 2\n}\n").unwrap();
    let sock = format!("/tmp/mt-ops-it-{}.sock", std::process::id());
    let dir = std::fs::canonicalize(&dir).unwrap();

    // build: indexes, and starts the daemon
    let (code, out) = mt(&dir, &sock, &["build"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("index: 2 files, 2 re-parsed") && out.contains("daemon: started in"), "{out}");
    assert!(dir.join(".mt/index.sqlite").exists());
    // again: nothing to re-parse, the same daemon answers
    let (_, out) = mt(&dir, &sock, &["build"]);
    assert!(out.contains("0 re-parsed") && out.contains("daemon: already running"), "{out}");

    // a request through the daemon shows up in the logs
    let (_, ctx) = mt(&dir, &sock, &["context", "Two", "--map"]);
    assert!(ctx.contains("function Two"), "{ctx}");
    let (_, logs) = mt(&dir, &sock, &["logs", "--source", "events", "-n", "5"]);
    assert!(logs.contains("## events") && logs.contains("request ") && logs.contains("context ok"), "{logs}");
    let (_, logs) = mt(&dir, &sock, &["logs"]);
    assert!(logs.contains("## events") && logs.contains("## hook decisions") && logs.contains("## mt apply runs"), "{logs}");

    // hook decisions are this repo's only: an event from here is listed, one from elsewhere is not
    let other = std::env::temp_dir().join(format!("mt-ops-other-{}", std::process::id()));
    std::fs::create_dir_all(other.join(".mt")).unwrap();
    let hook = |cwd: &Path, cmd: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mt")).args(["hook", "pretool"]).env("MT_SERVER_SCOPE", "off")
            .stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let ev = serde_json::json!({"session_id": "ops-it", "cwd": cwd, "tool_name": "Grep", "tool_input": {"pattern": cmd}});
        std::io::Write::write_all(c.stdin.as_mut().unwrap(), ev.to_string().as_bytes()).unwrap();
        drop(c.stdin.take());
        c.wait().unwrap();
    };
    hook(&dir, "HereOnlyIdentifier");
    hook(&other, "ElsewhereIdentifier");
    let (_, logs) = mt(&dir, &sock, &["logs", "--source", "hooks"]);
    assert!(logs.contains(&format!("## hook decisions ({}/.mt/hooks.log)", dir.display())), "{logs}");
    assert!(logs.contains("HereOnlyIdentifier") && !logs.contains("ElsewhereIdentifier"), "{logs}");
    let _ = std::fs::remove_dir_all(&other);

    // rebuild: stops the daemon, drops the index, indexes from scratch, starts a new daemon
    let (code, out) = mt(&dir, &sock, &["rebuild"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("daemon: stopped") && out.contains("index: dropped") && out.contains("2 re-parsed") && out.contains("daemon: started in"), "{out}");
    // the new daemon serves the new graph
    std::fs::write(dir.join("src/b.go"), "package src\n\nfunc Three() int { return One() }\n").unwrap();
    let (_, out) = mt(&dir, &sock, &["build"]);
    assert!(out.contains("index: 3 files") && out.contains("already running"), "{out}");

    let _ = mt(&dir, &sock, &["serve", "--stop"]);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&dir);
}
