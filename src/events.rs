//! What the daemon is doing, one JSON line per event, appended to `<repo>/.mt/events.jsonl`
//! (`.mt/` is self-gitignored, and the watcher ignores it, so writing here triggers nothing).
//! `mt visualize` tails it. One append costs a few microseconds and happens after the reply has
//! been sent, so it never delays an answer. The file rolls over to `events.1.jsonl` at 4 MB.

use regex::Regex;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ROLL_AT: u64 = 4 << 20;

pub fn path(root: &Path) -> PathBuf { root.join(".mt/events.jsonl") }

fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }

pub fn emit(root: &Path, mut ev: Value) {
    if std::env::var("MT_EVENTS").as_deref() == Ok("0") { return; }
    if let Some(o) = ev.as_object_mut() { o.insert("t".into(), json!((now() * 1000.0).round() / 1000.0)); }
    let p = path(root);
    if p.metadata().map(|m| m.len() > ROLL_AT).unwrap_or(false) {
        let _ = std::fs::rename(&p, root.join(".mt/events.1.jsonl"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{ev}");
    }
}

static SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z0-9]{1,8}):L(\d+)(?:-(\d+))?").unwrap());

/// The files and line ranges a reply mentions, in order, at most `max`.
pub fn touched(out: &str, max: usize) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    SPAN.captures_iter(out).filter_map(|c| {
        let p = c[1].to_string();
        let s: u64 = c[2].parse().ok()?;
        let e: u64 = c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(s);
        seen.insert((p.clone(), s)).then(|| json!({"path": p, "s": s, "e": e}))
    }).take(max).collect()
}

/// The session an event belongs to, as the daemon records it (first 8 characters).
pub fn session8() -> String { crate::query::session_id().unwrap_or_default().chars().take(8).collect() }

/// `mt activity`: what mt did in this repo (or one session), from `.mt/events.jsonl`: totals and
/// the latest events, each in one line. The status line and the activity pane read `--json`.
pub fn activity(root: &Path, session: Option<&str>, limit: usize) -> Value {
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let text = read(root.join(".mt/events.1.jsonl")) + &read(path(root));
    let want = session.map(|s| s.chars().take(8).collect::<String>()).filter(|s| !s.is_empty());
    let evs: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| match (&want, e.get("session").and_then(|s| s.as_str())) {
            (None, _) => true,
            (Some(w), Some(s)) => s == w,
            // repo-wide events (reindex, daemon start) show in every session's view
            (Some(_), None) => matches!(e["kind"].as_str(), Some("reindex" | "daemon")),
        }).collect();
    let n = |k: &str| evs.iter().filter(|e| e["kind"] == k).count();
    let sum = |k: &str, f: &str| evs.iter().filter(|e| e["kind"] == k).map(|e| e[f].as_f64().unwrap_or(0.0)).sum::<f64>();
    let reqs: Vec<&Value> = evs.iter().filter(|e| e["kind"] == "request").collect();
    let mut by_cmd: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
    for r in &reqs { let x = by_cmd.entry(r["cmd"].as_str().unwrap_or("?").to_string()).or_default(); x.0 += 1; x.1 += r["ms"].as_f64().unwrap_or(0.0); }
    let kept_out = (sum("compress", "chars_in") - sum("compress", "chars_out")).max(0.0);
    let line = |e: &Value| -> Value {
        let (label, detail) = match e["kind"].as_str().unwrap_or("") {
            "request" => (e["cmd"].as_str().unwrap_or("?").to_string(), e["subject"].as_str().filter(|s| !s.is_empty()).unwrap_or(e["header"].as_str().unwrap_or("")).to_string()),
            "compress" => ("compress".into(), format!("{} {}→{} chars, kept as {}", e["what"].as_str().unwrap_or(""), e["chars_in"], e["chars_out"], e["id"].as_str().unwrap_or(""))),
            "mcp" => ("mcp".into(), format!("{}.{} {} chars{}", e["server"].as_str().unwrap_or(""), e["tool"].as_str().unwrap_or(""), e["chars"], if e["ok"] == false { " ✗" } else { "" })),
            "reindex" => ("reindex".into(), format!("{} changed, {} removed", e["changed"].as_array().map_or(0, |a| a.len()), e["removed"].as_array().map_or(0, |a| a.len()))),
            "daemon" => ("daemon".into(), format!("{} ({} files)", e["state"].as_str().unwrap_or(""), e["files"])),
            k => (k.to_string(), String::new()),
        };
        json!({"t": e["t"], "kind": e["kind"], "label": label, "detail": detail.chars().take(160).collect::<String>(), "ms": e["ms"], "ok": e.get("ok").cloned().unwrap_or(json!(true))})
    };
    let last = reqs.last().map(|r| json!({"cmd": r["cmd"], "ms": r["ms"]}));
    let mut out = json!({
        "session": want, "events": evs.len(),
        "requests": reqs.len(), "request_ms": reqs.iter().map(|r| r["ms"].as_f64().unwrap_or(0.0)).sum::<f64>().round(),
        "by_cmd": by_cmd.iter().map(|(k, (c, ms))| json!({"cmd": k, "count": c, "avg_ms": (ms / *c as f64 * 10.0).round() / 10.0})).collect::<Vec<_>>(),
        "compressions": n("compress"), "chars_kept_out": kept_out as u64, "tokens_kept_out": (kept_out / 4.0).round() as u64,
        "mcp_calls": n("mcp"), "reindexes": n("reindex"), "last_request": last,
        "recent": evs.iter().rev().take(limit).map(line).collect::<Vec<_>>(),
    });
    let l = status_line(&out);
    out["line"] = json!(l);
    out
}

/// One line for a status bar: `mt ▸ 14 calls · ~52k tok kept out · last query 4 ms`.
pub fn status_line(a: &Value) -> String {
    let k = |n: f64| if n >= 1000.0 { format!("{:.1}k", n / 1000.0) } else { format!("{n}") };
    let mut parts = vec![format!("{} call{}", a["requests"], if a["requests"] == 1 { "" } else { "s" })];
    if a["mcp_calls"].as_u64().unwrap_or(0) > 0 { parts.push(format!("{} mcp", a["mcp_calls"])); }
    if a["tokens_kept_out"].as_u64().unwrap_or(0) > 0 { parts.push(format!("~{} tok kept out", k(a["tokens_kept_out"].as_u64().unwrap_or(0) as f64))); }
    if let Some(l) = a.get("last_request").filter(|l| !l.is_null()) { parts.push(format!("last {} {} ms", l["cmd"].as_str().unwrap_or(""), l["ms"])); }
    format!("mt ▸ {}", parts.join(" · "))
}

/// One daemon request: what was asked, how it went, which code it touched.
pub fn request(root: &Path, cmd: &str, subject: &str, session: &str, ok: bool, out: &str, took: Duration) {
    if cmd == "status" { return; }
    let header = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(240).collect::<String>();
    emit(root, json!({
        "kind": "request", "cmd": cmd, "ok": ok,
        "ms": (took.as_secs_f64() * 1e5).round() / 100.0,
        "bytes": out.len(), "tokens": out.len() / 4,
        "session": session.chars().take(8).collect::<String>(),
        "subject": subject.chars().take(300).collect::<String>(),
        "header": header,
        "touched": touched(out, 60),
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn touched_finds_spans_once() {
        let t = super::touched("[1] src/a.rs:L10-20 fn a\n[2] web/b.tsx:L5 x\nsrc/a.rs:L10-20 again", 10);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0]["path"], "src/a.rs");
        assert_eq!(t[0]["e"], 20);
        assert_eq!(t[1]["s"], 5);
    }

    #[test]
    fn activity_counts_one_session() {
        let d = std::env::temp_dir().join(format!("mt-activity-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".mt")).unwrap();
        std::fs::write(d.join(".mt/events.jsonl"), [
            r#"{"kind":"request","cmd":"query","ok":true,"ms":4.0,"session":"aaaaaaaa","subject":"q1","t":1}"#,
            r#"{"kind":"request","cmd":"apply","ok":true,"ms":90.0,"session":"bbbbbbbb","subject":"other","t":2}"#,
            r#"{"kind":"compress","what":"go test","store":"go-test","id":"x1","chars_in":40000,"chars_out":2000,"session":"aaaaaaaa","t":3}"#,
            r#"{"kind":"mcp","server":"graft","tool":"find","ok":true,"ms":200,"chars":900,"session":"aaaaaaaa","t":4}"#,
            r#"{"kind":"reindex","changed":["a.rs"],"removed":[],"ms":2.0,"t":5}"#,
        ].join("\n")).unwrap();
        let a = super::activity(&d, Some("aaaaaaaa-rest"), 10);
        assert_eq!(a["requests"], 1);
        assert_eq!(a["compressions"], 1);
        assert_eq!(a["tokens_kept_out"], 9500);
        assert_eq!(a["mcp_calls"], 1);
        assert_eq!(a["reindexes"], 1);
        assert_eq!(a["recent"][0]["label"], "reindex");
        assert_eq!(super::status_line(&a), "mt ▸ 1 call · 1 mcp · ~9.5k tok kept out · last query 4.0 ms");
        assert_eq!(super::activity(&d, None, 10)["requests"], 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
