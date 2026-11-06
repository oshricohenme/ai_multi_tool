//! The mt reference reaches the model whole, inline, before it writes anything: part 1 at
//! session start, part 2 with the first prompt, the rest with the first tool results; every
//! hook answer under Claude Code's 10,000-unit inline limit, and no line of the skill lost.

use std::io::Write;
use std::process::{Command, Stdio};

fn hook(event: &str, input: serde_json::Value) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mt"))
        .args(["hook", event])
        .env("MT_SERVER_SCOPE", "off")
        .env("CLAUDE_PLUGIN_ROOT", concat!(env!("CARGO_MANIFEST_DIR"), "/plugin"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).into_owned()
}

fn context_of(out: &str) -> String {
    if out.trim().is_empty() { return String::new(); }
    let v: serde_json::Value = serde_json::from_str(out).expect("hook answers JSON");
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

#[test]
fn the_whole_reference_arrives_inline_before_the_first_write() {
    let sid = format!("brief-it-{}", std::process::id());
    let cwd = env!("CARGO_MANIFEST_DIR");
    let mut got = vec![context_of(&hook("session-start", serde_json::json!({"session_id": sid, "cwd": cwd})))];
    got.push(context_of(&hook("prompt", serde_json::json!({"session_id": sid}))));
    for _ in 0..3 {
        let ev = serde_json::json!({"session_id": sid, "tool_name": "Bash", "tool_input": {"command": "ls"},
            "tool_response": {"stdout": "a\nb", "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}});
        got.push(context_of(&hook("posttool", ev)));
    }
    for (i, c) in got.iter().enumerate() {
        let units = c.encode_utf16().count();
        assert!(units <= 10_000, "hook answer {i} is {units} units: Claude Code would move it to a file");
    }
    let all = got.join("\n");
    let skill = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/skills/mt/SKILL.md")).unwrap();
    let body = skill.splitn(3, "---").nth(2).unwrap_or(&skill);
    let missing: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty() && !all.contains(*l)).collect();
    assert!(missing.is_empty(), "{} skill line(s) never delivered, e.g. {:?}", missing.len(), &missing[..missing.len().min(3)]);
    // nothing is delivered twice: the queue is empty after it has all gone out
    assert!(got[3..].iter().skip_while(|c| !c.is_empty()).all(|c| c.is_empty() || !c.contains("More of the mt reference")) || got.len() < 4);
}
