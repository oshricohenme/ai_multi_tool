//! `mt serve`: a per-repo daemon holding the graph in memory, kept fresh by a file watcher, so
//! `mt query` and `mt context` skip the tree walk and the SQLite reads.
//!
//! Transport: a Unix domain socket at `.mt/mt.sock` (mode 0600; a temp-dir path when the repo
//! path is too long for a socket name). One request per connection: the client writes JSON and
//! closes its write side, the daemon answers with JSON and closes. No HTTP, no ports.
//! ponytail: Unix only (macOS, Linux). Windows runs everything in-process; add named pipes
//! (the `interprocess` crate) if Windows needs the daemon.
//!
//! Freshness: a watcher thread applies every burst of file changes to the graph (and the
//! SQLite index) as it happens. Before answering, each request writes a cookie file and waits
//! for the watcher to report it (Watchman's technique): events arrive in order, so every change
//! made before the request is in the graph. If the watcher does not confirm within 1 s, the
//! request walks the tree instead.
//!
//! Lifecycle: the first `mt query`/`mt context` that finds no daemon answers in-process and
//! starts one in the background. The daemon exits after MT_IDLE_MIN minutes without requests
//! (default 30), on `mt serve --stop`, or when the `mt` binary changes (a rebuild).
//! Concurrency: a thread per connection; queries share a read lock on the graph, the watcher,
//! `touch` and `apply` take the write lock. ponytail: unbounded threads; a pool if hundreds of
//! agents ever share one daemon.

use crate::graph::Graph;
use crate::index::{Index, Store, UpdateStats};
use crate::{context, fnv1a, query};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Default)]
pub struct Req {
    pub cmd: String,
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub budget: usize,
    #[serde(default)]
    pub max_pointers: usize,
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub edit: bool,
    #[serde(default)]
    pub map: bool,
    /// the client's session id, so pointer numbers and the seen log match what it was shown
    #[serde(default)]
    pub session: String,
    /// the client binary's identity; a daemon from another build refuses and exits
    #[serde(default)]
    pub exe: String,
    /// `apply`: the caller's working directory and environment (steps run as the caller would
    /// have run them), and whether this is a dry run
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    pub out: String,
}

pub fn socket_path(root: &Path) -> PathBuf {
    // A session-scoped daemon (set by the plugin's SessionStart hook) applies only inside the
    // repo it was started for; any other repo falls back to its own per-repo socket.
    if let Some(p) = std::env::var_os("MT_SOCKET") {
        let for_root = std::env::var_os("MT_SOCKET_ROOT").map(PathBuf::from);
        if for_root.map(|r| r == root || std::fs::canonicalize(&r).ok().as_deref() == Some(root)).unwrap_or(true) {
            return PathBuf::from(p);
        }
    }
    let p = root.join(".mt/mt.sock");
    if p.as_os_str().len() < 100 {
        p
    } else {
        std::env::temp_dir().join(format!("mt-{:08x}.sock", fnv1a(root.to_string_lossy().as_bytes()) as u32))
    }
}

/// Identity of the running binary: path, size and mtime. Changes on every rebuild.
pub fn exe_id() -> String {
    let Ok(p) = std::env::current_exe() else { return String::new() };
    let md = std::fs::metadata(&p).ok();
    let m = md.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}:{}:{}", p.display(), md.map(|m| m.len()).unwrap_or(0), m)
}

/// Send one request. None: no daemon answering (or it is from another build and exited).
pub fn request(root: &Path, req: &Req) -> Option<Resp> {
    request_at(&socket_path(root), req)
}

/// Send one request to a known socket (the hook stopping a session's server).
pub fn request_at(sock: &Path, req: &Req) -> Option<Resp> {
    let mut c = UnixStream::connect(sock).ok()?;
    c.set_read_timeout(Some(Duration::from_secs(120))).ok()?;
    c.write_all(serde_json::to_string(req).ok()?.as_bytes()).ok()?;
    c.shutdown(std::net::Shutdown::Write).ok()?;
    let mut buf = String::new();
    c.read_to_string(&mut buf).ok()?;
    let r: Resp = serde_json::from_str(&buf).ok()?;
    if !r.ok && r.out == "stale" {
        // it was another build; it is exiting now. Give the socket a moment to go.
        std::thread::sleep(Duration::from_millis(50));
        return None;
    }
    Some(r)
}

/// Send a batch to apply. None only when no daemon took it (none running, or another build):
/// the caller may then apply in-process. Once the daemon has the batch it is never retried
/// in-process (that would apply it twice): any later failure comes back as an error, and there
/// is no read timeout, because the batch's steps (build, tests) may take minutes.
pub fn request_apply(root: &Path, req: &Req) -> Option<Resp> {
    let mut c = UnixStream::connect(socket_path(root)).ok()?;
    let body = serde_json::to_string(req).ok()?;
    c.write_all(body.as_bytes()).ok()?;
    c.shutdown(std::net::Shutdown::Write).ok()?;
    let mut buf = String::new();
    if let Err(e) = c.read_to_string(&mut buf) {
        return Some(Resp { ok: false, out: format!("mt apply: lost the daemon while it held the batch ({e}); check `git status` before resending") });
    }
    match serde_json::from_str::<Resp>(&buf) {
        Ok(r) if !r.ok && r.out == "stale" => { std::thread::sleep(Duration::from_millis(50)); None }
        Ok(r) => Some(r),
        Err(_) => Some(Resp { ok: false, out: "mt apply: the daemon closed without an answer; check `git status` before resending".into() }),
    }
}

/// Start a daemon for `root` in the background, detached from this process group so a shell
/// that kills its children on exit leaves it running.
pub fn spawn(root: &Path) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let log = std::fs::OpenOptions::new().create(true).append(true).open(root.join(".mt/serve.log"));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").current_dir(root).stdin(std::process::Stdio::null()).process_group(0);
    match log {
        Ok(f) => { let f2 = f.try_clone().ok(); cmd.stdout(f); if let Some(f2) = f2 { cmd.stderr(f2); } }
        Err(_) => { cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()); }
    }
    let _ = cmd.spawn();
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn serve(root: PathBuf) -> Result<(), String> {
    let sock = socket_path(&root);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&sock).is_ok() {
                return Err(format!("already running for {}", root.display()));
            }
            let _ = std::fs::remove_file(&sock); // left by a daemon that died
            UnixListener::bind(&sock).map_err(|e| format!("bind {}: {e}", sock.display()))?
        }
        Err(e) => return Err(format!("bind {}: {e}", sock.display())),
    };
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    let me = exe_id();
    let t_load = Instant::now();

    // watch before the first refresh so no change slips between the two
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    // requests that need a full refresh ask the reindexer for it on this channel (never scan
    // themselves: one reindexer, one writer, so no older scan can land after a newer one)
    let walk_tx = std::sync::Mutex::new(tx.clone());
    use notify::Watcher;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                let _ = tx.send(p);
            }
        }
    })
    .map_err(|e| format!("file watcher: {e}"))?;
    watcher.watch(&root, notify::RecursiveMode::Recursive).map_err(|e| format!("watch {}: {e}", root.display()))?;

    let idx = Index::open(&root).map_err(|e| e.to_string())?;
    idx.update_serving().map_err(|e| e.to_string())?;
    crate::texts::enable();
    let g = Graph::load(&idx).map_err(|e| e.to_string())?;
    println!("mt serve {}: {} files in memory, loaded in {} ms, socket {}", root.display(), g.file_count(), t_load.elapsed().as_millis(), sock.display());
    crate::events::emit(&root, serde_json::json!({"kind": "daemon", "state": "start", "files": g.file_count(), "ms": t_load.elapsed().as_millis() as u64, "pid": std::process::id()}));
    // The graph is what requests read: they share its read lock. Only patching it takes the write
    // lock, for microseconds. The SQLite index has its own lock and is not behind the graph's.
    // replies' numbered items and files' evidence stay in memory between requests
    crate::precache::enable();
    let live = Arc::new(std::sync::RwLock::new(g));
    let idx = Arc::new(idx);
    // Sync barrier (the Watchman "cookie"): a request writes .mt/cookie-N and waits until the
    // watcher reports it. Events arrive in order, so by then every earlier change is in the graph.
    let cookies = Arc::new((std::sync::Mutex::new(0u64), std::sync::Condvar::new()));
    let walks = Arc::new((std::sync::Mutex::new(0u64), std::sync::Condvar::new()));

    // ---- the persister: writes what the reindexer parsed to SQLite, on its own thread, after the
    // graph is patched and waiting requests have their answer (in order; the graph is the truth)
    let (ptx, prx) = std::sync::mpsc::channel::<(UpdateStats, Vec<crate::index::Extracted>, Vec<String>)>();
    {
        let idx = idx.clone();
        std::thread::spawn(move || {
            // the lookup tables one-shot commands use: built here, once the daemon answers, in line
            // with the writes (so no lock contention); nothing the daemon answers needs them
            let t = std::time::Instant::now();
            match idx.ensure_lookups() {
                Ok(0) => {}
                Ok(n) => println!("lookup tables: built {n} in {} ms", t.elapsed().as_millis()),
                Err(e) => println!("lookup tables failed (the next one-shot `mt` command builds them): {e}"),
            }
            while let Ok((st, ex, rm)) = prx.recv() {
                if let Err(e) = idx.persist(&st, &ex, &rm, false) { println!("persist failed: {e}"); }
            }
        });
    }

    // ---- the reindexer: a background thread applying each burst of file events (30 ms debounce,
    // so an editor's write+rename or a multi-file `mt apply` lands as one update). It finds and
    // parses what changed with no lock held, patches the graph under a brief write lock, answers
    // the waiting requests, and hands the SQLite write to the persister.
    {
        let live = live.clone();
        let idx = idx.clone();
        let cookies = cookies.clone();
        let walks = walks.clone();
        let root = root.clone();
        std::thread::spawn(move || {
            // cookies come through the file watcher; walk requests straight down this channel, so
            // they are numbered and acknowledged apart (a walk overtaking a cookie must not
            // release that cookie's request early)
            let tagged = |p: &Path, tag: &str| p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(tag)).and_then(|n| n.parse::<u64>().ok()).filter(|_| p.parent().map(|d| d.ends_with(".mt")).unwrap_or(false));
            let cookie_of = |p: &Path| tagged(p, "cookie-");
            let walk_of = |p: &Path| tagged(p, "walk-");
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                // a waiting request (cookie) ends the debounce at once
                while !batch.iter().any(|p| cookie_of(p).is_some()) {
                    match rx.recv_timeout(Duration::from_millis(30)) { Ok(p) => batch.push(p), Err(_) => break }
                }
                batch.extend(rx.try_iter());
                let seen_cookie = batch.iter().filter_map(|p| cookie_of(p)).max();
                let seen_walk = batch.iter().filter_map(|p| walk_of(p)).max();
                let walk = seen_walk.is_some();
                // mt's own files (.mt/), VCS internals and skipped directories are not the repo
                let mut rels: Vec<String> = batch.iter().filter_map(|p| p.strip_prefix(&root).ok().map(|r| r.to_string_lossy().replace('\\', "/"))).filter(|r| crate::index::watch_relevant(r)).collect();
                rels.sort();
                rels.dedup();
                let t = Instant::now();
                if !rels.is_empty() || walk {
                    let (known, total) = { let g = live.read().unwrap_or_else(|e| e.into_inner()); (if walk { g.known() } else { g.known_for(&rels) }, g.file_count()) };
                    let hashes = |paths: &[&str]| live.read().unwrap_or_else(|e| e.into_inner()).hashes(paths);
                    match idx.scan(&known, if walk { None } else { Some(&rels) }, &hashes) {
                        Ok((st, ex, rm)) => {
                            let t_scan = t.elapsed();
                            if !ex.is_empty() || !rm.is_empty() || !st.restamped.is_empty() {
                                let mut g = live.write().unwrap_or_else(|e| e.into_inner());
                                g.restamp(&st.restamped);
                                g.apply(&ex, &rm);
                            }
                            if std::env::var_os("MT_TIMING").is_some() {
                                println!("watch: {} path(s) [{}] -> {} re-parsed, {} restamped (same content), {} removed, walked the tree: {}, known map: {}, scan {:.2} ms (no lock), graph patched at {:.2} ms",
                                    rels.len(), rels.iter().take(3).cloned().collect::<Vec<_>>().join(", "), ex.len(), st.restamped.len(), rm.len(), st.walked, if known.len() < total { "partial" } else { "full" }, t_scan.as_secs_f64() * 1e3, t.elapsed().as_secs_f64() * 1e3);
                            }
                            if !ex.is_empty() || !rm.is_empty() {
                                println!("updated {} re-parsed, {} removed in {} ms: {}", ex.len(), rm.len(), t.elapsed().as_millis(), ex.iter().map(|e| e.rel.as_str()).chain(rm.iter().map(|r| r.as_str())).take(5).collect::<Vec<_>>().join(", "));
                                crate::events::emit(&root, serde_json::json!({"kind": "reindex", "changed": ex.iter().map(|e| e.rel.as_str()).take(200).collect::<Vec<_>>(), "removed": rm.iter().map(|r| r.as_str()).take(200).collect::<Vec<_>>(), "ms": (t.elapsed().as_secs_f64() * 1e5).round() / 100.0}));
                            }
                            if !ex.is_empty() || !rm.is_empty() || !st.restamped.is_empty() { let _ = ptx.send((st, ex, rm)); }
                        }
                        Err(e) => println!("update failed: {e}"),
                    }
                }
                for (seen, ack) in [(seen_cookie, &*cookies), (seen_walk, &*walks)] {
                    if let Some(c) = seen {
                        let (m, cv) = ack;
                        let mut acked = m.lock().unwrap_or_else(|e| e.into_inner());
                        *acked = (*acked).max(c);
                        cv.notify_all();
                    }
                }
            }
        });
    }

    let last = Arc::new(AtomicU64::new(now_s()));
    {
        let last = last.clone();
        let sock = sock.clone();
        let idle = std::env::var("MT_IDLE_MIN").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(30) * 60;
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(15));
            if now_s().saturating_sub(last.load(Ordering::Relaxed)) > idle {
                let _ = std::fs::remove_file(&sock);
                std::process::exit(0);
            }
        });
    }

    // one thread per connection: every agent's request is read, synced and answered at once,
    // instead of waiting for the requests ahead of it (barrier included) to finish
    let cookie_n = Arc::new(AtomicU64::new(0));
    let walk_n = Arc::new(AtomicU64::new(0));
    let walk_tx = Arc::new(walk_tx);
    for conn in listener.incoming() {
        let Ok(c) = conn else { continue };
        last.store(now_s(), Ordering::Relaxed);
        let (live, walk_tx, cookies, walks, cookie_n, walk_n, root, sock, me) = (live.clone(), walk_tx.clone(), cookies.clone(), walks.clone(), cookie_n.clone(), walk_n.clone(), root.clone(), sock.clone(), me.clone());
        std::thread::spawn(move || handle(c, &live, (&walk_tx, &walks, &walk_n), &cookies, &cookie_n, &root, &sock, &me));
    }
    Ok(())
}

type Live = std::sync::RwLock<Graph>;
type Cookies = (std::sync::Mutex<u64>, std::sync::Condvar);

#[allow(clippy::too_many_arguments)]
type Walker<'a> = (&'a std::sync::Mutex<std::sync::mpsc::Sender<PathBuf>>, &'a Cookies, &'a AtomicU64);

fn handle(mut c: UnixStream, live: &Live, walker: Walker, cookies: &Cookies, cookie_n: &AtomicU64, root: &Path, sock: &Path, me: &str) {
    let t_accept = Instant::now();
    let mut buf = String::new();
    if c.read_to_string(&mut buf).is_err() {
        return;
    }
    let reply = |c: &mut UnixStream, ok: bool, out: String| {
        let _ = c.write_all(serde_json::to_string(&Resp { ok, out }).unwrap_or_default().as_bytes());
    };
    let req: Req = match serde_json::from_str(&buf) {
        Ok(r) => r,
        Err(e) => { reply(&mut c, false, format!("bad request: {e}")); return; }
    };
    if !req.exe.is_empty() && req.exe != me {
        reply(&mut c, false, "stale".into());
        drop(c);
        let _ = std::fs::remove_file(sock);
        std::process::exit(0);
    }
    // every answer from here on is also an event for `mt visualize`, written after the reply
    let reply = |c: &mut UnixStream, ok: bool, out: String| {
        let _ = c.write_all(serde_json::to_string(&Resp { ok, out: out.clone() }).unwrap_or_default().as_bytes());
        let _ = c.flush();
        let subject = if req.query.is_empty() { req.input.as_str() } else { req.query.as_str() };
        crate::events::request(root, &req.cmd, subject, &req.session, ok, &out, t_accept.elapsed());
    };

    // ---- expand needs neither the graph nor the barrier: it serves the session's kept items from
    // memory, each only while its file's stamp is unchanged, and reads the disk (hash-checked) otherwise
    if req.cmd == "expand" {
        let args: Vec<String> = req.input.split('\n').map(String::from).filter(|s| !s.trim().is_empty()).collect();
        let (ok, s) = crate::query::expand(root, &args, Some(req.session.as_str()).filter(|s| !s.is_empty()));
        reply(&mut c, ok, s);
        return;
    }

    // ---- never answer from a graph that is behind the disk. Each request writes its own cookie
    // and waits for the watcher to report it; concurrent requests wait at the same time.
    let t0 = Instant::now();
    let n = cookie_n.fetch_add(1, Ordering::SeqCst) + 1;
    let cookie = root.join(format!(".mt/cookie-{n}"));
    let synced = std::fs::write(&cookie, b"").is_ok() && {
        let (m, cv) = cookies;
        let acked = m.lock().unwrap_or_else(|e| e.into_inner());
        let (acked, _) = cv.wait_timeout_while(acked, Duration::from_secs(1), |a| *a < n).unwrap_or_else(|e| e.into_inner());
        *acked >= n
    };
    let _ = std::fs::remove_file(&cookie);
    let t_barrier = t0.elapsed();
    let timing = std::env::var_os("MT_TIMING").is_some();
    if !synced {
        println!("watcher did not confirm within 1 s; walking the tree instead");
    }

    // ---- a batch to apply: no graph lock across its writes or steps; the reindexer (watcher)
    // indexes the written files in the background, and the next request's barrier waits for them
    if req.cmd == "apply" {
        let _one_at_a_time = crate::apply::APPLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::apply::set_client(Some(crate::apply::Client { cwd: PathBuf::from(&req.cwd), env: req.env.clone() }));
        let (ok, out) = crate::apply::run_with(crate::apply::Ctx::Daemon { graph: live, root: root.to_path_buf() }, &req.input, req.dry_run);
        crate::apply::set_client(None);
        reply(&mut c, ok, out);
        if timing { println!("request apply: barrier {:.2} ms, total {:.2} ms", t_barrier.as_secs_f64() * 1e3, t0.elapsed().as_secs_f64() * 1e3); }
        return;
    }
    // ---- a full refresh (touch, or the watcher did not confirm): the reindexer walks the tree
    // and patches the graph; this request waits for that, and only that
    if req.cmd == "touch" || !synced {
        let (walk_tx, walks, walk_n) = walker;
        let n = walk_n.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = walk_tx.lock().unwrap_or_else(|e| e.into_inner()).send(root.join(format!(".mt/walk-{n}")));
        let (m, cv) = walks;
        let acked = m.lock().unwrap_or_else(|e| e.into_inner());
        let (acked, _) = cv.wait_timeout_while(acked, Duration::from_secs(120), |a| *a < n).unwrap_or_else(|e| e.into_inner());
        let done = *acked >= n;
        drop(acked);
        if req.cmd == "touch" { reply(&mut c, done, if done { "refreshed\n".into() } else { "refresh still running after 120 s".into() }); return; }
    }

    // ---- readers: share the graph
    let guard = live.read().unwrap_or_else(|e| e.into_inner());
    let g: &Graph = &guard;
    let st = UpdateStats { files: g.file_count(), ..Default::default() };
    let index_ms = t0.elapsed().as_millis();
    let out = match req.cmd.as_str() {
        "query" => query::run(g, &req.input, Some(req.session.as_str()), &st, index_ms, t0),
        "context" => context::run(g, &req.query, &context::Opts { budget: req.budget, json: req.json, max_pointers: req.max_pointers, edit: req.edit, map: req.map, session: Some(req.session.clone()) }, &st, index_ms, t0),
        "expand" => {
            let args: Vec<String> = req.input.split('\n').map(String::from).filter(|s| !s.trim().is_empty()).collect();
            let (ok, s) = query::expand(root, &args, Some(req.session.as_str()).filter(|s| !s.is_empty()));
            if ok { Ok(s) } else { Err(s) }
        }
        "status" => Ok(format!("mt serve {}: pid {}, {} files in memory, socket {}\n", root.display(), std::process::id(), g.file_count(), sock.display())),
        "stop" => {
            reply(&mut c, true, "stopped\n".into());
            let _ = std::fs::remove_file(sock);
            std::process::exit(0);
        }
        other => Err(format!("unknown command {other:?}")),
    };
    drop(guard);
    let t_work = t0.elapsed();
    match out {
        Ok(s) => reply(&mut c, true, s),
        Err(e) => reply(&mut c, false, e),
    }
    if timing {
        println!("request {}: read {:.2} ms, barrier {:.2} ms, work {:.2} ms, reply {:.2} ms", req.cmd, (t0 - t_accept).as_secs_f64() * 1e3, t_barrier.as_secs_f64() * 1e3, (t_work - t_barrier).as_secs_f64() * 1e3, (t0.elapsed() - t_work).as_secs_f64() * 1e3);
    }
}
