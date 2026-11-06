//! The map view and memory-served expansion, through a real `mt serve`: a map has no bodies and
//! names relations by number; `mt expand` of those numbers comes from the daemon's memory while the
//! file is unchanged, and from disk, hash-checked, after an edit (MOVED / CHANGED, never stale).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn mt(dir: &Path, sock: &str, sid: &str, args: &[&str], stdin: &str, daemon: bool) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mt"));
    c.current_dir(dir).args(args).env("MT_SESSION", sid).env("MT_SOCKET", sock).env("MT_SOCKET_ROOT", dir)
        .env_remove("CLAUDE_CODE_SESSION_ID").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if daemon { c.env_remove("MT_NO_DAEMON"); } else { c.env("MT_NO_DAEMON", "1"); }
    let mut ch = c.spawn().unwrap();
    ch.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    String::from_utf8_lossy(&ch.wait_with_output().unwrap().stdout).into_owned()
}

fn repo(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("store")).unwrap();
    std::fs::create_dir_all(dir.join("api")).unwrap();
    std::fs::create_dir_all(dir.join(".mt")).unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/x\n").unwrap();
    std::fs::write(dir.join("store/audit.go"), "package store\n\n// WriteAudit records one row.\nfunc (s *Store) WriteAudit(e Entry) error {\n\tif e.ID == \"\" {\n\t\treturn errEmpty\n\t}\n\treturn s.db.Insert(e)\n}\n").unwrap();
    std::fs::write(dir.join("api/handlers.go"), "package api\n\nimport \"example.com/x/store\"\n\nfunc (h *H) create(e store.Entry) error {\n\treturn h.st.WriteAudit(e)\n}\n\nfunc (h *H) remove(e store.Entry) error {\n\treturn h.st.WriteAudit(e)\n}\n").unwrap();
    std::fs::write(dir.join("api/handlers_test.go"), "package api\n\nfunc TestCreate(t *testing.T) {\n\th := &H{}\n\t_ = h.st.WriteAudit(store.Entry{ID: \"x\"})\n}\n").unwrap();
}

#[test]
fn map_then_expand_from_memory_then_disk_after_an_edit() {
    for round in 0..3 {
        let dir = std::env::temp_dir().join(format!("mt-map-it-{}-{round}", std::process::id()));
        repo(&dir);
        let sock = format!("/tmp/mt-map-it-{}-{round}.sock", std::process::id());
        let sid = format!("map-it-{}-{round}", std::process::id());
        let mut d = Command::new(env!("CARGO_BIN_EXE_mt")).current_dir(&dir).arg("serve")
            .env("MT_SOCKET", &sock).env("MT_SOCKET_ROOT", &dir).env_remove("MT_SESSION").env_remove("MT_NO_DAEMON")
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let t = Instant::now();
        while !Path::new(&sock).exists() && t.elapsed() < Duration::from_secs(10) { std::thread::sleep(Duration::from_millis(20)); }
        assert!(Path::new(&sock).exists(), "daemon up");

        let map = mt(&dir, &sock, &sid, &["context", "WriteAudit", "--map"], "", true);
        // no bodies: only item lines, sizes, hashes, relations
        assert!(!map.contains("return s.db.Insert(e)") && !map.contains("return h.st.WriteAudit(e)"), "a map has no bodies:\n{map}");
        assert!(map.contains("[1] store/audit.go:L3-9 method Store.WriteAudit (7 lines) h="), "{map}");
        assert!(map.contains("## relations of [1]: called by ") && map.contains("unit tests "), "{map}");
        let n_create = map.lines().find(|l| l.contains("H.create")).and_then(|l| l.strip_prefix('[')).and_then(|l| l.split(']').next()).expect("create is in the map").to_string();

        // expand the target and a caller, in one call: from memory
        let e = mt(&dir, &sock, &sid, &["expand", "1", &n_create], "", true);
        assert!(e.contains("2 from memory") && e.contains("return s.db.Insert(e)") && e.contains("return h.st.WriteAudit(e)"), "{e}");

        // an edit above the target: memory is not used, the disk answers, and says it moved
        std::fs::write(dir.join("store/audit.go"), std::fs::read_to_string(dir.join("store/audit.go")).unwrap().replacen("package store\n", "package store\n\n// moved down\n", 1)).unwrap();
        let e = mt(&dir, &sock, &sid, &["expand", "1"], "", true);
        assert!(!e.contains("from memory") && e.contains("[1] store/audit.go:L5-11") && e.contains("MOVED since the pointer (was L3-9)"), "round {round}:\n{e}");
        // and its body edited: CHANGED, with the new text
        std::fs::write(dir.join("store/audit.go"), std::fs::read_to_string(dir.join("store/audit.go")).unwrap().replace("return s.db.Insert(e)", "return s.db.InsertAudit(e)")).unwrap();
        let e = mt(&dir, &sock, &sid, &["expand", "1"], "", true);
        assert!(e.contains("CHANGED since the pointer") && e.contains("InsertAudit"), "round {round}:\n{e}");

        let _ = mt(&dir, &sock, &sid, &["serve", "--stop"], "", true);
        let _ = d.wait();
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn query_map_view_has_no_bodies_but_keeps_requested_spans() {
    let dir = std::env::temp_dir().join(format!("mt-qmap-it-{}", std::process::id()));
    repo(&dir);
    let sid = format!("qmap-{}", std::process::id());
    let q = r#"{"view": "map", "queries": [{"id": "c", "callers": "WriteAudit"}, {"span": "store/audit.go:Store.WriteAudit"}]}"#;
    let out = mt(&dir, "/tmp/none.sock", &sid, &["query"], q, false);
    assert!(out.contains("return s.db.Insert(e)"), "the span asked for is inline:\n{out}");
    assert!(!out.contains("return h.st.WriteAudit(e)"), "callers are map lines, not bodies:\n{out}");
    assert!(out.contains("method H.create (3 lines) h="), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
