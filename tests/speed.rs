//! Speed: the bash/grep chains Claude actually writes, against the mt call that answers the same
//! question, on generated repos of three sizes and three languages (Go, Python, TypeScript).
//!
//!   cargo test --release --test speed -- --ignored --nocapture
//!
//! Knobs: MT_SPEED_SIZES (default "500,5000,20000"), MT_SPEED_RUNS (median of N, default 5),
//! MT_SPEED_REPO (also run the read scenarios against a real repo, no ground truth there).
//!
//! Every scenario checks correctness first: bash and mt must both find the generator's ground
//! truth (counts of sites, lines, files). Speed is reported, not asserted, except one claim: the
//! whole read phase (S11) through the warm daemon beats the bash chain at 5,000 files and more.
//! Modes: bash (macOS /usr/bin/grep, one command after another); bash as Claude Code runs it
//! (`grep` is the ugrep bundled in the Claude Code binary, with the session wrapper's flags, and
//! per-hit work fans out with `xargs -P <cores>`); mt in-process (MT_NO_DAEMON=1, warm on-disk
//! index); mt through the warm daemon (the in-memory graph). Index build time is reported apart.
//! Then agents: 1, 2, 4, 8 at once on the largest repo, each running an agent's read sequence,
//! all against ONE daemon, or each in-process, or each in Claude Code's bash.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MT: &str = env!("CARGO_BIN_EXE_mt");

// ------------------------------------------------------------------ fixture

struct Truth {
    files: usize,
    go_callers: usize,   // call lines of ResolveAccount (not the definition)
    go_constructs: usize, // `AccountRecord{` lines in Go
    go_construct_sites: usize, // the functions those lines sit in (mt reports sites, one per function)
    go_word_refs: usize, // every whole-word ResolveAccount: definition, its doc comment, calls
    prop_lines: usize,   // lines matching account_?id, any case, in code
    ledger_decl: usize,  // manifest lines naming ledger
    grep_files: usize,   // Go files containing `AccountRecord{`
}

/// A repo of `n` files: Go, Python and TypeScript in turn, 40 packages, each file ~60 lines with
/// five functions. Targets: ResolveAccount (Go), resolve_account (Python), resolveAccount (TS);
/// AccountRecord constructions; one property in three spellings; manifests naming `ledger`.
fn generate(root: &Path, n: usize) -> Truth {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    let mut t = Truth { files: 0, go_callers: 0, go_constructs: 0, go_construct_sites: 0, go_word_refs: 2, prop_lines: 0, ledger_decl: 3, grep_files: 0 };
    std::fs::write(root.join("go.mod"), "module example.com/fixture\n\ngo 1.22\n\nrequire github.com/acme/ledger v1.4.0\n").unwrap();
    std::fs::write(root.join("package.json"), "{\n  \"name\": \"fixture\",\n  \"dependencies\": {\n    \"ledger\": \"^2.1.0\"\n  }\n}\n").unwrap();
    std::fs::write(root.join("requirements.txt"), "ledger==1.0.3\nrequests==2.32.3\n").unwrap();
    t.files += 3;
    for i in 0..n {
        let d = i % 40;
        let dir = root.join(format!("pkg{d}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = String::new();
        let calls = i > 2 && i % 7 == 3;
        let builds = i > 2 && i % 11 == 5;
        match i % 3 {
            0 => {
                let _ = writeln!(s, "package pkg{d}\n");
                if i == 0 {
                    s.push_str("// AccountRecord is the ledger row.\ntype AccountRecord struct {\n\tID        string\n\tAccountID string\n}\n\n");
                    s.push_str("// ResolveAccount finds the account for a key.\nfunc ResolveAccount(key int) AccountRecord {\n\tif key < 0 {\n\t\treturn AccountRecord{}\n\t}\n\treturn AccountRecord{ID: \"x\"}\n}\n\n");
                    t.go_constructs += 2;
                    t.go_construct_sites += 1;
                    t.prop_lines += 1;
                    t.grep_files += 1;
                }
                for k in 0..5 {
                    let _ = writeln!(s, "// Work{i}K{k} does step {k}.\nfunc Work{i}K{k}(x int) int {{\n\ty := x + {k}\n\tfor j := 0; j < 3; j++ {{\n\t\ty += j * {i}\n\t}}");
                    if calls && k == 2 { s.push_str("\tr := ResolveAccount(y)\n\t_ = r\n"); t.go_callers += 1; t.go_word_refs += 1; }
                    if builds && k == 4 { s.push_str("\trec := AccountRecord{ID: \"b\"}\n\t_ = rec\n"); t.go_constructs += 1; t.go_construct_sites += 1; t.grep_files += 1; }
                    s.push_str("\treturn y\n}\n\n");
                }
                if i > 0 { let _ = writeln!(s, "// Thing{i} carries an account.\ntype Thing{i} struct {{\n\tAccountID string\n\tN         int\n}}"); t.prop_lines += 1; }
                std::fs::write(dir.join(format!("mod{i}.go")), s).unwrap();
            }
            1 => {
                if i == 1 { s.push_str("def resolve_account(key):\n    \"\"\"Find the account.\"\"\"\n    return {\"id\": key}\n\n\n"); }
                for k in 0..5 {
                    let _ = writeln!(s, "def work_{i}_{k}(x):\n    \"\"\"Step {k}.\"\"\"\n    y = x + {k}\n    for j in range(3):\n        y += j * {i}");
                    if calls && k == 2 { s.push_str("    r = resolve_account(y)\n"); }
                    s.push_str("    return y\n\n\n");
                }
                let _ = writeln!(s, "class Thing{i}:\n    def __init__(self):\n        self.account_id = None\n        self.n = {i}");
                t.prop_lines += 1;
                std::fs::write(dir.join(format!("mod{i}.py")), s).unwrap();
            }
            _ => {
                if i == 2 { s.push_str("export function resolveAccount(key: number): { id: number } {\n  return { id: key };\n}\n\n"); }
                for k in 0..5 {
                    let _ = writeln!(s, "export function work{i}K{k}(x: number): number {{\n  let y = x + {k};\n  for (let j = 0; j < 3; j++) {{\n    y += j * {i};\n  }}");
                    if calls && k == 2 { s.push_str("  const r = resolveAccount(y);\n  void r;\n"); }
                    s.push_str("  return y;\n}\n\n");
                }
                let _ = writeln!(s, "export interface Thing{i} {{\n  accountId: string;\n  n: number;\n}}");
                t.prop_lines += 1;
                std::fs::write(dir.join(format!("mod{i}.ts")), s).unwrap();
            }
        }
        t.files += 1;
    }
    // a git repo, as real ones are (bash's `git ls-files` needs it); not timed
    sh(root, "git init -q && git add -A && git -c user.email=t@t -c user.name=t commit -qm fixture");
    t
}

// ------------------------------------------------------------------ running

fn sh(cwd: &Path, cmd: &str) -> (String, Duration) {
    let t = Instant::now();
    let o = Command::new("sh").arg("-c").arg(cmd).current_dir(cwd)
        .env_remove("MT_SOCKET").env_remove("MT_SOCKET_ROOT").env_remove("MT_SESSION").env_remove("CLAUDE_CODE_SESSION_ID")
        .env("MT", MT).stdin(Stdio::null()).output().expect("sh");
    (String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr), t.elapsed())
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Bash, BashCC, MtInproc, MtDaemon }

impl Mode {
    fn name(self) -> &'static str { match self { Mode::Bash => "bash", Mode::BashCC => "bash (Claude Code)", Mode::MtInproc => "mt in-process", Mode::MtDaemon => "mt daemon" } }
    fn bash(self) -> bool { matches!(self, Mode::Bash | Mode::BashCC) }
}

/// The Claude Code binary (it is ugrep and ripgrep too, dispatching on its program name), or None.
fn claude_bin() -> Option<PathBuf> {
    let c = std::env::var_os("CLAUDE_CODE_EXECPATH").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/claude")))?;
    c.is_file().then_some(c)
}

/// A directory whose `grep` is exactly what a Claude Code session's Bash runs: the bundled
/// ugrep with the session wrapper's flags. Prepended to PATH in BashCC mode.
fn cc_shim() -> Option<PathBuf> {
    let c = claude_bin()?;
    let d = std::env::temp_dir().join("mt-speed-ccbin");
    std::fs::create_dir_all(&d).ok()?;
    let ug = d.join("ugrep");
    let _ = std::fs::remove_file(&ug);
    std::os::unix::fs::symlink(&c, &ug).ok()?;
    let g = d.join("grep");
    std::fs::write(&g, format!("#!/bin/sh\nexec '{}' -G --ignore-files --hidden -I --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg --exclude-dir=.bzr --exclude-dir=.jj --exclude-dir=.sl \"$@\"\n", ug.display())).ok()?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o755)).ok()?;
    Some(d)
}

fn cores() -> usize { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) }

/// What every command in this mode starts with (BashCC: Claude Code's grep first on PATH).
fn prefix(mode: Mode) -> String {
    match (mode, cc_shim()) { (Mode::BashCC, Some(d)) => format!("PATH='{}':$PATH; ", d.display()), _ => String::new() }
}

/// The mt command line for this mode. In-process forces MT_NO_DAEMON; daemon mode needs the
/// daemon already running for the repo (the client finds it by its socket).
fn mt(mode: Mode, args: &str) -> String {
    match mode {
        Mode::MtInproc => format!("MT_NO_DAEMON=1 \"$MT\" {args}"),
        _ => format!("\"$MT\" {args}"),
    }
}

fn median(mut v: Vec<Duration>) -> Duration { v.sort(); v[v.len() / 2] }

fn runs() -> usize { std::env::var("MT_SPEED_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(5) }

/// Run `cmd` R times (after one warm-up), return (median, output of the last run).
fn timedp(cwd: &Path, pre: &str, cmd: &str, reset: Option<&str>) -> (Duration, String) {
    timed(cwd, &format!("{pre}{cmd}"), reset)
}

fn timed(cwd: &Path, cmd: &str, reset: Option<&str>) -> (Duration, String) {
    let mut times = vec![];
    let mut out = String::new();
    for k in 0..=runs() {
        if let Some(r) = reset { sh(cwd, r); }
        let (o, d) = sh(cwd, cmd);
        if k > 0 { times.push(d); }
        out = o;
    }
    (median(times), out)
}

/// A query batch, written to a file (no shell quoting problems), as one mt command.
fn query(root: &Path, mode: Mode, name: &str, batch: serde_json::Value) -> String {
    let p = root.join(format!(".mt-speed-{name}.json"));
    let body = serde_json::json!({"queries": batch, "session": false, "fresh": true, "max_pointers": 2000});
    std::fs::write(&p, body.to_string()).unwrap();
    mt(mode, &format!("query '{}'", p.display()))
}

/// The number before `what` in a query's result line, e.g. "18 sites", "443 lines".
fn count(out: &str, id: &str, what: &str) -> Option<usize> {
    let line = out.lines().find(|l| l.starts_with(&format!("{id} ")))?;
    let w: Vec<&str> = line.split(|c: char| c.is_whitespace() || c == ';' || c == '[' || c == ']').filter(|x| !x.is_empty()).collect();
    w.windows(2).find_map(|p| if p[1].starts_with(what) { p[0].parse().ok() } else { None })
}

fn lines(out: &str) -> usize { out.lines().filter(|l| !l.trim().is_empty()).count() }

struct Row { size: usize, scenario: &'static str, bash_cmds: usize, mode: Mode, time: Duration, bytes: usize, ok: bool, note: String }

// ------------------------------------------------------------------ scenarios

fn scenarios(root: &Path, t: &Truth, size: usize, mode: Mode, rows: &mut Vec<Row>) {
    let gofiles = "--include='*.go'";
    let pre = prefix(mode);
    let par = mode == Mode::BashCC;
    // the enclosing-function lookup for one `file:line`, as a script, so it can fan out with xargs -P
    let _ = std::fs::write(root.join(".mt-speed-encl.sh"), "f=${1%:*}; l=${1##*:}; awk -v L=$l 'NR<=L && /^func /{h=$0} NR==L{print FILENAME\":\"L\": \"h; exit}' $f\n");
    let s2_bash = if par { format!("grep -rn {gofiles} 'ResolveAccount(' . | grep -v 'func ResolveAccount' | cut -d: -f1,2 | xargs -P {} -n1 sh .mt-speed-encl.sh", cores()) }
        else { format!("grep -rn {gofiles} 'ResolveAccount(' . | grep -v 'func ResolveAccount' | while IFS=: read f l rest; do awk -v L=$l 'NR<=L && /^func /{{h=$0}} NR==L{{print FILENAME\":\"L\": \"h; exit}}' $f; done") };
    let s4_bash = format!("grep -rl {gofiles} 'AccountRecord{{' . | xargs {}-n1 sed -n '1,25p'", if par { format!("-P {} ", cores()) } else { String::new() });
    let mut push = |scenario: &'static str, bash_cmds: usize, time: Duration, out: &str, ok: bool, note: String| {
        rows.push(Row { size, scenario, bash_cmds, mode, time, bytes: out.len(), ok, note });
    };

    // S1: find a definition and read its body
    if mode.bash() {
        let c = format!("hit=$(grep -rn {gofiles} '^func ResolveAccount' . | head -1); f=${{hit%%:*}}; rest=${{hit#*:}}; a=${{rest%%:*}}; \
                         b=$(awk -v a=$a 'NR>a && /^}}/ {{print NR; exit}}' $f); sed -n \"${{a}},${{b}}p\" $f");
        let (d, o) = timedp(root, &pre, &c, None);
        push("S1 definition + body", 4, d, &o, o.contains("func ResolveAccount") && o.contains("return AccountRecord{ID"), String::new());
    } else {
        let c = query(root, mode, "s1", serde_json::json!([{"id": "d", "defined_in": "ResolveAccount"}, {"id": "b", "span": "$d"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        push("S1 definition + body", 4, d, &o, o.contains("func ResolveAccount") && o.contains("return AccountRecord{ID"), String::new());
    }

    // S2: every caller, with the function each call sits in
    if mode.bash() {
        let c = s2_bash.clone();
        let (d, o) = timedp(root, &pre, &c, None);
        let n = lines(&o);
        push("S2 callers + enclosing fn", 1 + t.go_callers, d, &o, n == t.go_callers, format!("{n} of {}", t.go_callers));
    } else {
        let c = query(root, mode, "s2", serde_json::json!([{"id": "c", "callers": "ResolveAccount"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "c", "sites").unwrap_or(0);
        push("S2 callers + enclosing fn", 1 + t.go_callers, d, &o, n == t.go_callers, format!("{n} of {}", t.go_callers));
    }

    // S3: eight separate reads of known line ranges
    let picks: Vec<(String, usize, usize)> = (0..8).map(|k| { let i = 3 * (1 + k * (size / 3 / 9).max(1)); (format!("pkg{}/mod{}.go", i % 40, i), 3, 9) }).collect();
    if mode.bash() {
        let c = picks.iter().map(|(f, a, b)| format!("sed -n '{a},{b}p' {f}")).collect::<Vec<_>>().join("; ");
        let (d, o) = timedp(root, &pre, &c, None);
        push("S3 eight reads", 8, d, &o, o.matches("func Work").count() == 8, String::new());
    } else {
        let qs: Vec<_> = picks.iter().enumerate().map(|(k, (f, a, b))| serde_json::json!({"id": format!("r{k}"), "span": format!("{f}:L{a}-{b}")})).collect();
        let c = query(root, mode, "s3", serde_json::Value::Array(qs));
        let (d, o) = timedp(root, &pre, &c, None);
        push("S3 eight reads", 8, d, &o, o.matches("func Work").count() >= 8, String::new());
    }

    // S4: grep, then read every file it found
    if mode.bash() {
        let c = s4_bash.clone();
        let (d, o) = timedp(root, &pre, &c, None);
        let n = o.matches("package pkg").count();
        push("S4 grep, then read hits", 1 + t.grep_files, d, &o, n == t.grep_files, format!("{n} of {} files", t.grep_files));
    } else {
        let c = query(root, mode, "s4", serde_json::json!([{"id": "g", "grep": "AccountRecord{", "in": "pkg"}, {"id": "b", "span": "$g"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "g", "lines").unwrap_or(0);
        push("S4 grep, then read hits", 1 + t.grep_files, d, &o, n == t.go_constructs, format!("{n} lines, {} expected", t.go_constructs));
    }

    // S5: the repo's layout
    if mode.bash() {
        let (d, o) = timedp(root, &pre, "git ls-files | awk -F/ '{ if (NF>1) print $1\"/\"; else print \"(top)\" }' | sort | uniq -c", None);
        let n: usize = o.lines().filter_map(|l| l.split_whitespace().next()?.parse::<usize>().ok()).sum();
        push("S5 repo layout", 3, d, &o, n == t.files, format!("{n} files"));
    } else {
        let c = query(root, mode, "s5", serde_json::json!([{"id": "m", "map": ".", "depth": 1}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "m", "indexed").unwrap_or(0);
        push("S5 repo layout", 3, d, &o, n == t.files, format!("{n} files"));
    }

    // S6: one file's outline
    if mode.bash() {
        let (d, o) = timedp(root, &pre, "grep -nE '^(func|type) ' pkg3/mod3.go", None);
        push("S6 file outline", 1, d, &o, lines(&o) == 6, format!("{} defs", lines(&o)));
    } else {
        let c = query(root, mode, "s6", serde_json::json!([{"id": "s", "skeleton": "pkg3/mod3.go"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "s", "definitions").unwrap_or(0);
        push("S6 file outline", 1, d, &o, n >= 6, format!("{n} defs"));
    }

    // S7: every place a type is constructed, as an exhaustive claim
    if mode.bash() {
        let (d, o) = timedp(root, &pre, &format!("grep -rn {gofiles} 'AccountRecord{{' . | wc -l"), None);
        let n: usize = o.trim().parse().unwrap_or(0);
        push("S7 constructions", 2, d, &o, n == t.go_constructs, format!("{n} of {}", t.go_constructs));
    } else {
        let c = query(root, mode, "s7", serde_json::json!([{"id": "k", "constructs": "AccountRecord"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "k", "sites").unwrap_or(0);
        push("S7 constructions", 2, d, &o, n == t.go_construct_sites, format!("{n} sites of {} ({} lines)", t.go_construct_sites, t.go_constructs));
    }

    // S8: one property in every spelling
    if mode.bash() {
        let (d, o) = timedp(root, &pre, "grep -rniE --include='*.go' --include='*.py' --include='*.ts' 'account_?id' . | wc -l", None);
        let n: usize = o.trim().parse().unwrap_or(0);
        push("S8 property, all spellings", 2, d, &o, n == t.prop_lines, format!("{n} of {}", t.prop_lines));
    } else {
        let c = query(root, mode, "s8", serde_json::json!([{"id": "p", "property": "accountID"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = count(&o, "p", "lines").unwrap_or(0);
        push("S8 property, all spellings", 2, d, &o, n == t.prop_lines, format!("{n} of {}", t.prop_lines));
    }

    // S9: a dependency: where it is declared
    if mode.bash() {
        let (d, o) = timedp(root, &pre, "grep -n 'ledger' go.mod package.json requirements.txt", None);
        push("S9 dependency", 1, d, &o, lines(&o) == t.ledger_decl, format!("{} declared", lines(&o)));
    } else {
        let c = query(root, mode, "s9", serde_json::json!([{"id": "l", "module": "ledger"}]));
        let (d, o) = timedp(root, &pre, &c, None);
        let n = o.lines().find(|l| l.contains("declared (")).and_then(|l| l.split('(').nth(1)?.split_whitespace().next()?.parse::<usize>().ok()).unwrap_or(0);
        push("S9 dependency", 1, d, &o, n == t.ledger_decl, format!("{n} declared"));
    }

    // S10: rename across every file, then format what changed (edit cost; reset between runs)
    let reset = "git checkout -q -- . && git clean -fdq -e .mt -e '.mt-speed-*'";
    let fmt = if sh(root, "command -v gofmt").0.trim().is_empty() { "true" } else { "gofmt -w" };
    if mode.bash() {
        let c = format!("files=$(grep -rlw {gofiles} ResolveAccount .); echo \"$files\" | xargs perl -pi -e 's/\\bResolveAccount\\b/ResolveAccountV2/g'; echo \"$files\" | xargs {fmt}");
        let (d, _) = timedp(root, &pre, &c, Some(reset));
        let (o, _) = sh(root, &format!("grep -rnw {gofiles} ResolveAccountV2 . | wc -l"));
        let n: usize = o.trim().parse().unwrap_or(0);
        push("S10 rename + format", 3, d, &o, n == t.go_word_refs, format!("{n} of {}", t.go_word_refs));
    } else {
        let b = root.join(".mt-speed-s10.bundle");
        std::fs::write(&b, format!("@@ rename ResolveAccount ResolveAccountV2\n@@ format: {fmt} {{changed}}\n")).unwrap();
        let c = mt(mode, &format!("apply '{}'", b.display()));
        // the reset rewrites hundreds of files: a query waits until the daemon's watcher has caught
        // up with the disk, so the timed apply does not compete with that reindex
        let sync = query(root, mode, "sync", serde_json::json!([{"id": "x", "exists": "ResolveAccount"}]));
        let (d, _) = timedp(root, &pre, &c, Some(&format!("{reset} && {sync} >/dev/null")));
        let (o, _) = sh(root, &format!("grep -rnw {gofiles} ResolveAccountV2 . | wc -l"));
        let n: usize = o.trim().parse().unwrap_or(0);
        push("S10 rename + format", 3, d, &o, n == t.go_word_refs, format!("{n} of {}", t.go_word_refs));
    }
    sh(root, reset);

    // S11: a whole read phase: S1, S2, S4, S5, S7, S8, S9 as one sequence
    if mode.bash() {
        let parts = [
            format!("grep -rn {gofiles} '^func ResolveAccount' . | head -1"),
            s2_bash.clone(),
            s4_bash.clone(),
            "git ls-files | awk -F/ '{ if (NF>1) print $1\"/\"; else print \"(top)\" }' | sort | uniq -c".to_string(),
            format!("grep -rn {gofiles} 'AccountRecord{{' . | wc -l"),
            "grep -rniE --include='*.go' --include='*.py' --include='*.ts' 'account_?id' . | wc -l".to_string(),
            "grep -n 'ledger' go.mod package.json requirements.txt".to_string(),
        ];
        let (d, o) = timedp(root, &pre, &parts.join("\necho ===\n"), None);
        push("S11 whole read phase", 8 + t.go_callers + t.grep_files, d, &o, o.matches("===").count() == 6, String::new());
    } else {
        let c = query(root, mode, "s11", serde_json::json!([
            {"id": "d", "defined_in": "ResolveAccount"}, {"id": "b", "span": "$d"},
            {"id": "c", "callers": "ResolveAccount"},
            {"id": "g", "grep": "AccountRecord{", "in": "pkg"}, {"id": "gb", "span": "$g"},
            {"id": "m", "map": ".", "depth": 1},
            {"id": "k", "constructs": "AccountRecord"},
            {"id": "p", "property": "accountID"},
            {"id": "l", "module": "ledger"}
        ]));
        let (d, o) = timedp(root, &pre, &c, None);
        let ok = count(&o, "c", "sites") == Some(t.go_callers) && count(&o, "k", "sites") == Some(t.go_construct_sites) && count(&o, "p", "lines") == Some(t.prop_lines);
        push("S11 whole read phase", 8 + t.go_callers + t.grep_files, d, &o, ok, String::new());
    }
}

// ------------------------------------------------------------------ the test

fn ms(d: Duration) -> f64 { d.as_secs_f64() * 1000.0 }

/// The read sequence one agent runs: definition, callers, constructions, then a whole read
/// phase. The query files were written by `scenarios`; the bash commands are Claude Code's.
fn agent_cmds(root: &Path, mode: Mode) -> Vec<String> {
    if mode.bash() {
        let pre = prefix(mode);
        let g = "--include='*.go'";
        let s2 = format!("grep -rn {g} 'ResolveAccount(' . | grep -v 'func ResolveAccount' | cut -d: -f1,2 | xargs -P {} -n1 sh .mt-speed-encl.sh", cores());
        let s11 = [
            format!("grep -rn {g} '^func ResolveAccount' . | head -1"), s2.clone(),
            format!("grep -rl {g} 'AccountRecord{{' . | xargs -P {} -n1 sed -n '1,25p'", cores()),
            "git ls-files | awk -F/ '{ if (NF>1) print $1\"/\"; else print \"(top)\" }' | sort | uniq -c".to_string(),
            format!("grep -rn {g} 'AccountRecord{{' . | wc -l"),
            "grep -rniE --include='*.go' --include='*.py' --include='*.ts' 'account_?id' . | wc -l".to_string(),
            "grep -n 'ledger' go.mod package.json requirements.txt".to_string(),
        ].join("\n");
        return [format!("grep -rn {g} '^func ResolveAccount' . | head -1"), s2, format!("grep -rn {g} 'AccountRecord{{' . | wc -l"), s11]
            .iter().map(|c| format!("{pre}{c}")).collect();
    }
    ["s1", "s2", "s7", "s11"].iter().map(|q| mt(mode, &format!("query '{}'", root.join(format!(".mt-speed-{q}.json")).display()))).collect()
}

/// `n` agents start at the same instant, each running `cmds` in order. Returns the wall time
/// until the last finished and every request's latency.
fn run_agents(root: &Path, cmds: &[String], n: usize) -> (Duration, Vec<Duration>) {
    let gate = std::sync::Barrier::new(n + 1);
    let lat = std::sync::Mutex::new(vec![]);
    let t0 = std::thread::scope(|sc| {
        for _ in 0..n {
            sc.spawn(|| {
                gate.wait();
                for c in cmds { let (_, d) = sh(root, c); lat.lock().unwrap().push(d); }
            });
        }
        gate.wait();
        Instant::now()
    });
    let wall = t0.elapsed();
    (wall, lat.into_inner().unwrap())
}

fn pct(v: &[Duration], p: f64) -> Duration { let mut v = v.to_vec(); v.sort(); v[((v.len() - 1) as f64 * p).round() as usize] }

struct AgentRow { agents: usize, mode: Mode, wall: Duration, median: Duration, p95: Duration, requests: usize }

fn start_daemon(root: &Path) -> std::process::Child {
    let d = Command::new(MT).arg("serve").current_dir(root).env_remove("MT_SOCKET").env_remove("MT_SOCKET_ROOT").env_remove("MT_NO_DAEMON")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("mt serve");
    let t0 = Instant::now();
    while !root.join(".mt/mt.sock").exists() && t0.elapsed() < Duration::from_secs(60) { std::thread::sleep(Duration::from_millis(50)); }
    let up = sh(root, &format!("\"{MT}\" serve --status")).0;
    assert!(up.contains("mt serve"), "daemon did not come up: {up}");
    d
}

fn stop_daemon(root: &Path, mut d: std::process::Child) {
    let _ = sh(root, &format!("\"{MT}\" serve --stop"));
    let _ = d.kill();
    let _ = d.wait();
}

#[test]
#[ignore]
fn speed_bash_vs_mt() {
    let sizes: Vec<usize> = std::env::var("MT_SPEED_SIZES").unwrap_or_else(|_| "500,5000,20000".into()).split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let agent_counts: Vec<usize> = std::env::var("MT_SPEED_AGENTS").unwrap_or_else(|_| "1,2,4,8".into()).split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let cc = cc_shim().is_some();
    if !cc { println!("note: no Claude Code binary found (CLAUDE_CODE_EXECPATH or ~/.local/bin/claude): the 'bash (Claude Code)' mode is skipped"); }
    let base = std::env::temp_dir().join("mt-speed");
    let mut rows: Vec<Row> = vec![];
    let mut agents: Vec<AgentRow> = vec![];
    let mut builds: BTreeMap<usize, (Duration, Duration)> = BTreeMap::new();
    let largest = sizes.iter().copied().max().unwrap_or(0);
    for &n in &sizes {
        let root = base.join(format!("r{n}"));
        let t = generate(&root, n);
        // index: cold build, then the warm (incremental) refresh every in-process call pays
        let (_, cold) = sh(&root, &mt(Mode::MtInproc, "index"));
        let (_, warm) = sh(&root, &mt(Mode::MtInproc, "index"));
        builds.insert(n, (cold, warm));
        scenarios(&root, &t, n, Mode::Bash, &mut rows);
        if cc { scenarios(&root, &t, n, Mode::BashCC, &mut rows); }
        scenarios(&root, &t, n, Mode::MtInproc, &mut rows);
        let daemon = start_daemon(&root);
        scenarios(&root, &t, n, Mode::MtDaemon, &mut rows);
        // ---- several agents at once on the largest repo: all against this ONE daemon first
        if n == largest {
            let mut modes = vec![Mode::MtDaemon, Mode::MtInproc];
            if cc { modes.push(Mode::BashCC); }
            let mut daemon = Some(daemon);
            for mode in modes {
                if mode != Mode::MtDaemon { if let Some(d) = daemon.take() { stop_daemon(&root, d); } }
                let cmds = agent_cmds(&root, mode);
                let _ = run_agents(&root, &cmds, 1); // warm-up
                for &k in &agent_counts {
                    let (wall, lat) = run_agents(&root, &cmds, k);
                    agents.push(AgentRow { agents: k, mode, wall, median: pct(&lat, 0.5), p95: pct(&lat, 0.95), requests: lat.len() });
                }
            }
            if let Some(d) = daemon.take() { stop_daemon(&root, d); }
        } else {
            stop_daemon(&root, daemon);
        }
    }

    // ---- report: one table per size, every scenario in every mode
    let mut report = String::new();
    let row_of = |n: usize, s: &str, m: Mode| rows.iter().find(|r| r.size == n && r.scenario == s && r.mode == m);
    for &n in &sizes {
        let (cold, warm) = builds[&n];
        let _ = writeln!(report, "\n=== {n} files (index: cold build {:.0} ms, warm refresh {:.0} ms) ===", ms(cold), ms(warm));
        let _ = writeln!(report, "{:<28} {:>5} | {:>9} {:>9} {:>9} {:>9} | {:>7} {:>7} | {:>8} {:>8} {:>8} | correct", "scenario", "bash#", "bash ms", "bashCC ms", "inproc ms", "daemon ms", "x bash", "x bashCC", "bash B", "bashCC B", "daemon B");
        let scen: Vec<&'static str> = rows.iter().filter(|r| r.size == n && r.mode == Mode::Bash).map(|r| r.scenario).collect();
        for s in scen {
            let (b, i, d) = (row_of(n, s, Mode::Bash).unwrap(), row_of(n, s, Mode::MtInproc).unwrap(), row_of(n, s, Mode::MtDaemon).unwrap());
            let c = row_of(n, s, Mode::BashCC);
            let cms = c.map(|c| format!("{:.1}", ms(c.time))).unwrap_or_else(|| "-".into());
            let cx = c.map(|c| format!("{:.1}x", ms(c.time) / ms(d.time))).unwrap_or_else(|| "-".into());
            let ok = |r: &Row| if r.ok { "✓" } else { "✗" };
            let _ = writeln!(report, "{:<28} {:>5} | {:>9.1} {:>9} {:>9.1} {:>9.1} | {:>6.1}x {:>7} | {:>8} {:>8} {:>8} | {} {} {} {}  {}",
                s, b.bash_cmds, ms(b.time), cms, ms(i.time), ms(d.time), ms(b.time) / ms(d.time), cx, b.bytes, c.map(|c| c.bytes.to_string()).unwrap_or_else(|| "-".into()), d.bytes,
                ok(b), c.map(ok).unwrap_or("-"), ok(i), ok(d), [&b.note, &d.note].iter().filter(|x| !x.is_empty()).map(|x| x.as_str()).collect::<Vec<_>>().join(" / "));
        }
    }
    let _ = writeln!(report, "\nbash# = processes the plain bash chain starts; x bash / x bashCC = that bash time / mt daemon time; B = output bytes. Median of {} runs after one warm-up.", runs());
    if !agents.is_empty() {
        let _ = writeln!(report, "\n=== agents at once, {largest} files: each agent runs definition, callers, constructions, whole read phase (4 requests) ===");
        let _ = writeln!(report, "{:<20} {:>6} | {:>9} {:>10} {:>10} {:>9}", "mode", "agents", "wall ms", "median ms", "p95 ms", "req/s");
        for a in &agents {
            let _ = writeln!(report, "{:<20} {:>6} | {:>9.0} {:>10.1} {:>10.1} {:>9.1}", a.mode.name(), a.agents, ms(a.wall), ms(a.median), ms(a.p95), a.requests as f64 / a.wall.as_secs_f64());
        }
        let _ = writeln!(report, "(the daemon answers each connection on its own thread; queries share a read lock, so light requests do not wait behind heavy ones)");
    }
    println!("{report}");
    let json: Vec<_> = rows.iter().map(|r| serde_json::json!({"files": r.size, "scenario": r.scenario, "mode": r.mode.name(), "ms": ms(r.time), "bytes": r.bytes, "bash_processes": r.bash_cmds, "correct": r.ok, "note": r.note})).collect();
    let ajson: Vec<_> = agents.iter().map(|a| serde_json::json!({"agents": a.agents, "mode": a.mode.name(), "wall_ms": ms(a.wall), "median_ms": ms(a.median), "p95_ms": ms(a.p95), "requests": a.requests})).collect();
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench/speed_last.json");
    let _ = std::fs::write(&out, serde_json::to_string_pretty(&serde_json::json!({"builds_ms": builds.iter().map(|(n, (c, w))| (n.to_string(), [ms(*c), ms(*w)])).collect::<BTreeMap<_, _>>(), "rows": json, "agents": ajson})).unwrap());
    let _ = std::fs::write(out.with_file_name("speed_last.txt"), &report);

    // ---- assertions: correctness everywhere; speed only where it is the claim
    let wrong: Vec<String> = rows.iter().filter(|r| !r.ok).map(|r| format!("{} files, {}, {}: {}", r.size, r.scenario, r.mode.name(), r.note)).collect();
    assert!(wrong.is_empty(), "wrong answers:\n  {}", wrong.join("\n  "));
    for &n in sizes.iter().filter(|&&n| n >= 5000) {
        let get = |m: Mode| rows.iter().find(|r| r.size == n && r.scenario == "S11 whole read phase" && r.mode == m).unwrap().time;
        assert!(get(Mode::MtDaemon) < get(Mode::Bash), "S11 at {n} files: daemon {:.0} ms is not faster than bash {:.0} ms", ms(get(Mode::MtDaemon)), ms(get(Mode::Bash)));
    }
}
