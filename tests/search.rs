//! Plain-word search over code: a question finds the code that answers it, without a name.

use std::io::Write;
use std::process::{Command, Stdio};

fn mt(dir: &std::path::Path, args: &[&str], stdin: &str) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mt")).current_dir(dir).args(args)
        .env("MT_NO_DAEMON", "1").env("MT_SESSION", format!("search-it-{}", std::process::id())).env_remove("CLAUDE_CODE_SESSION_ID")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).into_owned()
}

#[test]
fn questions_find_code_by_meaning_of_names() {
    let dir = std::env::temp_dir().join(format!("mt-search-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("store")).unwrap();
    std::fs::create_dir_all(dir.join("api")).unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/s\n").unwrap();
    std::fs::write(dir.join("store/audit.go"), "package store\n\n// WriteAudit inserts one audit row.\nfunc (s *Store) WriteAudit(e Entry) error {\n\tvar out int\n\treturn s.db.Insert(\"audit_log\", e)\n}\n\nfunc (s *Store) ListAudit(org string, page int) []Entry {\n\treturn s.db.Select(\"audit_log\", org, page)\n}\n").unwrap();
    std::fs::write(dir.join("api/killswitch.go"), "package api\n\n// setKillswitch stops every agent of an org at once.\nfunc (s *Server) setKillswitch(org string, on bool) error {\n\treturn s.st.SetOrgKillswitch(org, on)\n}\n").unwrap();
    std::fs::write(dir.join("api/totp.go"), "package api\n\nfunc (s *Server) confirmTotpEnroll(code string) error {\n\treturn s.st.EnrollTOTP(code)\n}\n\nfunc (s *Server) decodeJSON(body []byte, v any) error {\n\treturn json.Unmarshal(body, v)\n}\n").unwrap();

    let first = |q: &str| -> String {
        let out = mt(&dir, &["search", q, "--map"], "");
        out.lines().find(|l| l.starts_with("[1] ")).unwrap_or("").to_string()
    };
    assert!(first("how is an audit row written").contains("Store.WriteAudit"), "{}", mt(&dir, &["search", "how is an audit row written"], ""));
    assert!(first("list the audit entries of an organization, a page at a time").contains("Store.ListAudit"));
    assert!(first("kill switch for an organization").contains("Server.setKillswitch"));
    assert!(first("enroll a TOTP code").contains("Server.confirmTotpEnroll"));
    assert!(first("decode the JSON body").contains("Server.decodeJSON"));
    // a function's own locals are never results
    assert!(!mt(&dir, &["search", "audit row written", "--map"], "").contains("var out"));
    // the best results come with their code, the same as a `search` op in a batch (fresh: the
    // searches above already showed them in this session, and mt doesn't repeat what was shown)
    let full = mt(&dir, &["query"], r#"{"fresh": true, "queries": [{"id": "s", "search": "how is an audit row written", "limit": 3}]}"#);
    assert!(full.contains("s = search how is an audit row written") && full.contains("return s.db.Insert(\"audit_log\", e)"), "{full}");
    let _ = std::fs::remove_dir_all(&dir);
}
