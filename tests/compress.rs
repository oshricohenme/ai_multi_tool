//! End to end through the real binary: `mt compress`, `mt run`, `mt out`, and the PostToolUse
//! hook that compresses Bash, Read and MCP output before the model sees it.

use std::io::Write;
use std::process::{Command, Stdio};

fn mt(args: &[&str], stdin: &str, sid: &str) -> (i32, String) {
    mt_as(args, stdin, sid, None)
}

/// `agent`: run as an agent with its own store name (MT_OUT_NAME). Each test has its own stores.
fn mt_as(args: &[&str], stdin: &str, sid: &str, agent: Option<&str>) -> (i32, String) {
    let base = sid.rsplit_once('-').map(|(a, _)| a).unwrap_or(sid);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mt"));
    cmd.args(args)
        .env("MT_SESSION", sid)
        .env("MT_OUT_DIR", std::env::temp_dir().join(format!("mt-it-stores-{base}-{}", std::process::id())))
        .env_remove("MT_OUT_NAME")
        .env("MT_SERVER_SCOPE", "off")
        .env("MT_NO_DAEMON", "1");
    if let Some(a) = agent { cmd.env("MT_OUT_NAME", a); }
    let mut c = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned())
}

fn hook(event: serde_json::Value, sid: &str) -> Option<serde_json::Value> {
    let (_, out) = mt(&["hook", "posttool"], &event.to_string(), sid);
    if out.trim().is_empty() { None } else { Some(serde_json::from_str(&out).expect("hook prints JSON")) }
}

fn id_in(s: &str) -> String {
    let i = s.find("mt out ").expect("names an id") + 7;
    s[i..i + 8].to_string()
}

fn issues() -> String {
    let rows: Vec<serde_json::Value> = (0..300)
        .map(|i| serde_json::json!({"number": i, "state": if i % 50 == 7 { "failed" } else { "passed" }, "title": format!("check {i} on the billing service"), "labels": ["ci"]}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({"total_count": 300, "items": rows})).unwrap()
}

#[test]
fn json_never_reaches_the_model_only_its_graph_and_answers() {
    let sid = format!("it-json-{}", std::process::id());
    let input = issues();
    let (code, out) = mt(&["compress", "--label", "gh api checks"], &input, &sid);
    assert_eq!(code, 0);
    assert!(out.contains("out of context") && out.contains("$  object") && out.contains(".state  string  (values {failed:6, passed:294})"), "{out}");
    assert!(!out.contains("billing service"), "data leaked into the graph:\n{out}");
    assert!(out.len() * 15 < input.len(), "graph {} vs input {}", out.len(), input.len());
    let id = id_in(&out);
    // exactly what is needed, by path: mt's shorthand is translated to RFC 9535
    let (_, a) = mt(&["out", &id, "--json", ".items[?state=failed].number"], "", &sid);
    assert!(a.contains("--json $.items[?@.state=='failed'].number: 6 match(es)") && a.contains("$['items'][257]['number']  257"), "{a}");
    // real JSONPath: logic, comparisons, recursive descent, functions
    let (_, a) = mt(&["out", &id, "--json", "$.items[?@.state=='failed' && @.number > 100].number"], "", &sid);
    assert!(a.contains(": 4 match(es)") && !a.contains("  57\n"), "{a}");
    let (_, a) = mt(&["out", &id, "--json", "$..number"], "", &sid);
    assert!(a.contains(": 300 match(es)"), "{a}");
    let (_, a) = mt(&["out", &id, "--json", "$.items[?match(@.title, 'check 1[0-9] on.*')].number"], "", &sid);
    assert!(a.contains(": 10 match(es)"), "{a}");
    let (code, a) = mt(&["out", &id, "--json", "$.items[?@.state=]"], "", &sid);
    assert!(code == 1 && a.contains("not a JSONPath"), "{a}");
    // jq, in process: filter, reshape, count
    let (_, a) = mt(&["out", &id, "--jq", "[.items[] | select(.state==\"failed\") | .number]"], "", &sid);
    assert!(a.contains("--jq: 1 match(es)") && a.contains("[7,57,107,157,207,257]"), "{a}");
    let (_, a) = mt(&["out", &id, "--jq", ".items | group_by(.state) | map({key: .[0].state, value: length}) | from_entries"], "", &sid);
    assert!(a.contains(r#"{"failed":6,"passed":294}"#), "{a}");
    let (code, a) = mt(&["out", &id, "--jq", ".items[] |"], "", &sid);
    assert!(code == 1 && a.contains("jq syntax"), "{a}");
    // a filter that never ends is stopped, not hung
    let (code, a) = {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_mt"));
        c.args(["out", &id, "--jq", "last(repeat(1))"]).env("MT_SESSION", &sid).env("MT_JQ_SECONDS", "1").env("MT_NO_DAEMON", "1")
            .env("MT_OUT_DIR", std::env::temp_dir().join(format!("mt-it-stores-{}-{}", sid.rsplit_once('-').unwrap().0, std::process::id())));
        let o = c.output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    assert!(code == 1 && a.contains("stopped after 1 s"), "{a}");
    // NDJSON goes to jq as one array
    let nd: String = (0..120).map(|i| format!("{{\"level\":\"{}\",\"msg\":\"m{i}\"}}\n", if i % 40 == 3 { "error" } else { "info" })).collect();
    let (_, o) = mt(&["compress", "--label", "ndjson"], &nd, &sid);
    let nid = id_in(&o);
    let (_, a) = mt(&["out", &nid, "--jq", "[.[] | select(.level==\"error\") | .msg]"], "", &sid);
    assert!(a.contains(r#"["m3","m43","m83"]"#), "{a}");
    // by plain words
    let (_, a) = mt(&["out", &id, "which checks failed?"], "", &sid);
    assert!(a.lines().nth(1).is_some_and(|l| l.contains("\"state\":\"failed\"")), "{a}");
    // listing
    let (_, l) = mt(&["out"], "", &sid);
    assert!(l.contains(&id) && l.contains("gh api checks"), "{l}");
}

#[test]
fn named_stores_keep_work_apart_and_any_store_can_be_read() {
    let sid = format!("it-names-{}", std::process::id());
    // agent 1 debugs a Vercel deployment, agent 2 studies docker compose; both see a "timeout"
    let vercel: String = (0..300).map(|i| format!("2026-11-05T10:00:{:02} λ GET /api/checkout {} in {}ms\n", i % 60, if i == 200 { 504 } else { 200 }, 40 + i % 9)).collect::<String>()
        + "Error: Task timed out after 10.00 seconds (api/checkout.ts:31)\n";
    let compose: String = (0..300).map(|i| format!("db-1  | 2026-11-05 10:00:{:02} LOG: checkpoint starting: time {i}\n", i % 60)).collect::<String>()
        + "api-1 | error: connect ETIMEDOUT 172.18.0.2:5432 (timeout waiting for db)\n";
    let (_, a) = mt_as(&["compress", "--name", "vercel-logs", "--label", "vercel logs"], &vercel, &sid, None);
    let (_, b) = mt_as(&["compress", "--label", "docker compose logs"], &compose, &sid, Some("compose-study"));
    assert!(a.contains("in store \"vercel-logs\"") && b.contains("in store \"compose-study\""), "{a}\n{b}");
    let (va, vb) = (a[a.find("kept as ").unwrap() + 8..][..8].to_string(), b[b.find("kept as ").unwrap() + 8..][..8].to_string());
    // a question to one store gets only that store's lines
    let (_, r) = mt_as(&["out", "--name", "vercel-logs", "why did it time out"], "", &sid, None);
    assert!(r.contains("Task timed out after 10.00 seconds") && !r.contains("ETIMEDOUT"), "{r}");
    let (_, r) = mt_as(&["out", "--name", "compose-study", "why did it time out"], "", &sid, None);
    assert!(r.contains("ETIMEDOUT 172.18.0.2:5432") && !r.contains("Task timed out"), "{r}");
    // any agent can read any output by id, whoever kept it
    let (_, r) = mt_as(&["out", &vb, "--level", "error"], "", &sid, Some("vercel-debug"));
    assert!(r.contains("ETIMEDOUT"), "{r}");
    // and search every store at once
    let (_, r) = mt_as(&["out", "--grep", "time(d)? ?out"], "", &sid, None);
    assert!(r.contains(&format!("## {va} in vercel-logs")) && r.contains(&format!("## {vb} in compose-study")), "{r}");
    // the listing groups by store
    let (_, l) = mt_as(&["out"], "", &sid, None);
    assert!(l.contains("compose-study (1 output)") && l.contains("vercel-logs (1 output)"), "{l}");
    // unnamed hook output is filed under the command that made it: vercel and docker never mix
    let ev = |cmd: &str, out: &str| serde_json::json!({"session_id": sid, "tool_name": "Bash", "tool_input": {"command": cmd},
        "tool_response": {"stdout": out, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}});
    let (_, h1) = mt_as(&["hook", "posttool"], &ev("vercel logs dpl_9 --since 1h", &vercel).to_string(), &sid, None);
    let (_, h2) = mt_as(&["hook", "posttool"], &ev("docker compose logs --tail 400", &compose).to_string(), &sid, None);
    assert!(h1.contains("in store \\\"vercel-logs\\\"") && h2.contains("in store \\\"docker-compose\\\""), "{h1}\n{h2}");
}

#[test]
fn logs_through_run_keep_errors_exit_code_and_are_queryable() {
    let sid = format!("it-log-{}", std::process::id());
    let script = "for i in $(seq 1 600); do echo \"[$i] compiling crate number $i\"; done; echo 'error[E0308]: mismatched types --> src/lib.rs:42:9'; exit 3";
    let (code, out) = mt(&["run", "--", script], "", &sid);
    assert_eq!(code, 3, "exit code kept");
    assert!(out.contains("error[E0308]: mismatched types --> src/lib.rs:42:9") && out.contains("[exit 3]"), "{out}");
    assert!(out.lines().count() < 140, "{} lines", out.lines().count());
    assert!(out.contains("graph: 601 lines"), "{out}");
    let id = id_in(&out);
    let (_, a) = mt(&["out", &id, "--lines", "300-302"], "", &sid);
    assert!(a.contains("300  [300] compiling crate number 300") && a.contains("3 of 601 lines"), "{a}");
    let (_, a) = mt(&["out", &id, "--level", "error"], "", &sid);
    assert!(a.contains("lib.rs:42:9") && a.contains("1 of 601"), "{a}");
    let (_, a) = mt(&["out", &id, "--grep", "number 59[0-9]$"], "", &sid);
    assert!(a.contains("10 of 601"), "{a}");
}

#[test]
fn hook_compresses_bash_keeps_structure_and_leaves_mt_alone() {
    let sid = format!("it-hook-{}", std::process::id());
    let noisy: String = (0..500).map(|i| format!("\x1b[32mPASS\x1b[0m test_case_{i} (0.0{}s)\n", i % 9)).collect::<String>() + "FAIL test_login: expected 200, got 500 (auth_test.py:88)\n";
    let ev = |cmd: &str, stdout: &str| serde_json::json!({"session_id": sid, "tool_name": "Bash", "tool_input": {"command": cmd},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}});
    let r = hook(ev("pytest -q", &noisy), &sid).expect("big output is replaced");
    let o = &r["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(o["interrupted"], false, "the tool's own structure is kept");
    let s = o["stdout"].as_str().unwrap();
    assert!(s.starts_with("[mt: `pytest -q` stdout reduced") && s.contains("FAIL test_login: expected 200, got 500 (auth_test.py:88)") && !s.contains("\x1b["), "{s}");
    assert!(s.len() * 5 < noisy.len());
    // output Claude Code found too large arrives as a preview plus a path: the full text is compressed
    let saved = std::env::temp_dir().join(format!("mt-it-persisted-{}.txt", std::process::id()));
    std::fs::write(&saved, issues()).unwrap();
    let stub = format!("<persisted-output>\nOutput too large (73.9KB). Full output saved to: {}\n\nPreview (first 2KB):\n{{\n  \"total_count\": 300,\n</persisted-output>", saved.display());
    let r = hook(ev("cat checks.json", &stub), &sid).expect("persisted output is replaced");
    let s = r["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap();
    assert!(s.contains("out of context") && s.contains(".items  array[300]") && !s.contains("persisted-output"), "{s}");
    // the structured form Claude Code 2.1.318 sends: the first ~30k in stdout, the whole in a file
    let truncated: String = issues().chars().take(30_000).collect();
    let mut e = ev("cat checks.json", &truncated);
    e["tool_response"]["persistedOutputPath"] = serde_json::json!(saved.display().to_string());
    let r = hook(e, &sid).expect("persistedOutputPath is followed");
    let o = &r["hookSpecificOutput"]["updatedToolOutput"];
    assert!(o.get("persistedOutputPath").is_none() && o["stdout"].as_str().unwrap().contains(".items  array[300]"), "{o}");
    let _ = std::fs::remove_file(&saved);
    // mt's own output is never touched, nor is small output
    assert!(hook(ev("mt query - < q.json", &noisy), &sid).is_none());
    assert!(hook(ev("ls", "a\nb\n"), &sid).is_none());
    // code printed by cat stays exact (lossless), even when big
    let code: String = (0..400).map(|i| format!("    let value_{i} = compute_something({i});\n")).collect();
    let r = hook(ev("cat src/big.rs", &code), &sid);
    if let Some(r) = r {
        let s = r["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap().to_string();
        assert!(!s.contains("[mt:") && s.contains("let value_399 = compute_something(399);"), "{s}");
    }
}

#[test]
fn hook_reads_and_mcp() {
    let sid = format!("it-read-{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("mt-it-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let small = dir.join("small.py");
    std::fs::write(&small, "def f():\n    return 1\n").unwrap();
    let read = |p: &std::path::Path, content: &str| serde_json::json!({"session_id": sid, "tool_name": "Read", "tool_input": {"file_path": p},
        "tool_response": {"type": "text", "file": {"filePath": p, "content": content, "numLines": content.lines().count(), "startLine": 1, "totalLines": content.lines().count()}}});
    // the first Read passes, the identical second one is a one-line note
    assert!(hook(read(&small, "def f():\n    return 1\n"), &sid).is_none());
    let r = hook(read(&small, "def f():\n    return 1\n"), &sid).expect("unchanged re-read");
    assert!(r["hookSpecificOutput"]["updatedToolOutput"]["file"]["content"].as_str().unwrap().contains("unchanged since your earlier Read"));
    // a compaction forgets it: the next Read is served whole again
    let (_, _) = mt(&["hook", "session-start"], &serde_json::json!({"session_id": sid, "source": "compact", "cwd": dir}).to_string(), &sid);
    // (the answer may carry the next part of the mt reference, queued again by that session start:
    // what matters is that the Read itself is not replaced)
    let r = hook(read(&small, "def f():\n    return 1\n"), &sid);
    assert!(r.as_ref().is_none_or(|r| r["hookSpecificOutput"].get("updatedToolOutput").is_none()), "{r:?}");
    // a huge code file comes back as its outline
    let big = dir.join("big.py");
    let src: String = (0..900).map(|i| format!("def handler_{i}(req):\n    return {i}\n")).collect();
    std::fs::write(&big, &src).unwrap();
    let r = hook(read(&big, &src), &sid).expect("outline");
    let c = r["hookSpecificOutput"]["updatedToolOutput"]["file"]["content"].as_str().unwrap();
    assert!(c.starts_with("[mt: 1800-line file shown as its outline") && c.contains("handler_899") && c.len() * 2 < src.len(), "{}", &c[..300.min(c.len())]);
    // MCP: a large JSON text block becomes the graph; the rest of the response is untouched
    let mcp = serde_json::json!({"session_id": sid, "tool_name": "mcp__gh__list_checks", "tool_input": {},
        "tool_response": [{"type": "text", "text": issues()}]});
    let r = hook(mcp, &sid).expect("mcp compressed");
    let t = r["hookSpecificOutput"]["updatedMCPToolOutput"][0]["text"].as_str().unwrap();
    assert!(t.contains("out of context") && t.contains(".items  array[300]") && !t.contains("billing service"), "{t}");
    assert_eq!(r["hookSpecificOutput"]["updatedMCPToolOutput"][0]["type"], "text");
    let _ = std::fs::remove_dir_all(&dir);
}
