//! mt — a multitool for coding agents. First function: `span`.
//!
//! `mt span <file> <query>` returns exactly the definition block (function, type, class,
//! method, config key, markdown section, ...) that matches `query`, with line numbers and a
//! content hash. It replaces the `grep -n X file` + `sed -n A,Bp file` pair.
//! Language support is tiered; see `languages.rs` and `mt langs`.

// Writing to a closed pipe (`mt ... | head`) must end quietly, not panic.
macro_rules! println {
    () => {{ use std::io::Write; if writeln!(std::io::stdout()).is_err() { std::process::exit(0) } }};
    ($($t:tt)*) => {{ use std::io::Write; if writeln!(std::io::stdout(), $($t)*).is_err() { std::process::exit(0) } }};
}
macro_rules! print {
    ($($t:tt)*) => {{ use std::io::Write; if write!(std::io::stdout(), $($t)*).is_err() { std::process::exit(0) } }};
}

mod apply;
mod mcp;
mod compress;
mod config;
mod context;
mod events;
mod viz;
mod embed;
mod index;
mod languages;
mod query;
mod resolve;
mod precache;
mod search;
mod init;
#[cfg(unix)]
mod ops;
mod texts;
mod tools;
mod tools_ext;
mod vctx;
mod coord;
#[cfg(unix)]
mod serve;
mod work;
mod trace;
#[cfg(unix)]
mod hook;

use clap::{Parser, Subcommand};
use languages::{detect, Tier};
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "mt", version, about = "multitool for coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the definition block matching QUERY: a name, `Type.method`, a dotted key path
    /// (`scripts.build`), a markdown heading, or `/regex/` (enclosing definition of each match;
    /// in files without definitions, a window of lines around each match).
    Span {
        file: PathBuf,
        /// A name, "## Heading", key path, '/regex/', or a line range `L10-20`; or give it as
        /// `path:query` in FILE.
        query: Option<String>,
        /// Extra lines before and after the block (default window for text files: 10).
        #[arg(short, long)]
        context: Option<usize>,
        /// Session id: a block already printed in this session with the same hash
        /// comes back as one "unchanged" line instead of the full text.
        #[arg(short, long, env = "MT_SESSION")]
        session: Option<String>,
        /// Print the block even if the session cache has seen it.
        #[arg(long)]
        force: bool,
        /// Maximum number of blocks to print when several definitions match.
        #[arg(long, default_value_t = 5)]
        max: usize,
    },
    /// Print every definition in FILE as `kind name Lstart-Lend`, no bodies.
    Skeleton { file: PathBuf },
    /// Build or refresh the repo index (.mt/index.sqlite). Incremental: only changed files re-parse.
    /// Everything git does not ignore is indexed; `mt index add <dir>` brings in a folder git
    /// ignores (every file in it), `mt index remove <dir>` takes it out again, `mt index list` shows them.
    Index {
        /// Drop the index and rebuild from scratch.
        #[arg(long)]
        rebuild: bool,
        /// `add <dir>...`, `remove <dir>...` or `list`
        action: Vec<String>,
        /// Write everything indexing does, with timings, to .mt/debug/index-<pid>.jsonl (as MT_DEBUG=1)
        #[arg(long)]
        debug: bool,
    },
    /// Everything related to a symbol in one shot: target, callers with call sites, types,
    /// callees, unit and integration tests, tables/models/migrations, docs. Filled to a budget.
    Context {
        query: String,
        /// Token budget for inlined bodies; the rest become pointers.
        #[arg(long, default_value_t = 6000)]
        budget: usize,
        #[arg(long, default_value_t = 60)]
        max_pointers: usize,
        #[arg(long)]
        json: bool,
        /// Edit mode: target body plus every referencing line with 2 lines of context; other bodies
        /// become pointers. Use when you are about to change the code rather than study it.
        #[arg(long)]
        edit: bool,
        /// Map view: every item as one line (location, size, hash, why it is here, its relations)
        /// with no bodies; read the ones you need with one `mt expand <n> <n> ...`.
        #[arg(long)]
        map: bool,
    },
    /// Apply a batch of edits across files in one call (JSON from FILE or stdin). All-or-nothing,
    /// hash-checked, parallel; optional "check" command rolls back on failure.
    Apply {
        file: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    /// The bodies of pointers from an earlier reply: their numbers in the last `mt query` /
    /// `mt context` reply of this session (`mt expand 3 5 8`), or `path:Ls-e@hash` (a pasted
    /// pointer line works). Hash-checked: a span that changed is found again by name.
    Expand { handles: Vec<String> },
    /// Show text to the USER, not the model: a file, or stdin with `-`. With the mt hook the user
    /// sees it (whole up to ~8,500 chars) and the model gets a one-line receipt, so a table, diff
    /// or log the model would otherwise retype costs one line of context.
    Show {
        src: Option<String>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Compress tool output from stdin (logs, build/test output, JSON, an MCP response): the
    /// same input always gives the same output. Large JSON becomes a graph of its structure with
    /// none of its data; the original is kept for `mt out`.
    Compress {
        #[arg(long, default_value = "auto")]
        kind: String,
        #[arg(long, default_value_t = compress::DEFAULT_LINES)]
        max_lines: usize,
        /// What the output is, for the header (e.g. "go test", "gh api issues").
        #[arg(long, default_value = "output")]
        label: String,
        /// The store to keep it in, so different work never mixes (default: MT_OUT_NAME, else "default").
        #[arg(long)]
        name: Option<String>,
    },
    /// Run a command, compress its combined stdout+stderr, keep its exit code.
    Run {
        #[arg(long, default_value_t = compress::DEFAULT_LINES)]
        max_lines: usize,
        /// The store to keep it in (default: MT_OUT_NAME, else named after the command).
        #[arg(long)]
        name: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Query an output mt kept: `mt out <id> "<question in plain words>"`, `--json '<path>'`,
    /// `--grep RE`, `--lines A-B`, `--level error|warn`. Without an id: list this session's outputs.
    Out {
        /// One kept output, found in whichever store holds it. Without it, `--name` (one store)
        /// or every store.
        id: Option<String>,
        /// Only this store.
        #[arg(long)]
        name: Option<String>,
        /// A question in plain words: the best-matching records (JSON) or lines (text), BM25-ranked.
        question: Vec<String>,
        /// RFC 9535 JSONPath: `$.items[?@.state=='failed'].name`, `$..id`, `$.items[0:5]`
        /// (mt's shorthand `.items[?state=failed]` is accepted too).
        #[arg(long)]
        json: Option<String>,
        /// A jq filter, run in process: `[.items[] | select(.state=="failed") | .name]`.
        #[arg(long)]
        jq: Option<String>,
        #[arg(long)]
        grep: Option<String>,
        #[arg(short = 'C', long, default_value_t = 0)]
        context: usize,
        #[arg(long)]
        lines: Option<String>,
        #[arg(long)]
        level: Option<String>,
    },
    /// Many questions and claims in one call (JSON array from FILE or stdin). Each query is an
    /// op on a subject (`{"callers": "WriteAudit", "depth": 2}`); add "expect" to make it a claim
    /// that comes back ✓, or ✗ with the difference. Ops: context, defined_in, exists, references,
    /// callers, tested_by, constructs, has_field, calls, uses_table, grep. "$q1" as a subject
    /// follows the names an earlier query found. One shared output budget.
    Query { file: Option<PathBuf> },
    /// Run the per-repo daemon in the foreground (normally started automatically). It holds the
    /// graph in memory, watches files, and exits after MT_IDLE_MIN idle minutes (default 30).
    Serve {
        #[arg(long)]
        stop: bool,
        #[arg(long)]
        status: bool,
    },
    /// What mt did here: request counts and times, compressions (tokens kept out of context), MCP
    /// calls, reindexes, and the latest events. `--session` narrows to one session, `--json` for tools.
    Activity {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
        /// one line for a status bar
        #[arg(long)]
        line: bool,
        /// Claude Code's `statusLine` command: reads its JSON (session_id, workspace) from stdin,
        /// prints that session's line. Settings: {"statusLine": {"type": "command", "command": "mt activity --statusline"}}
        #[arg(long)]
        statusline: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Claude Code's status line, two lines as graft draws its own: files indexed, the server,
    /// this session's calls and tokens saved; then the context window and the last call. Reads the
    /// JSON Claude Code sends on stdin. Settings: {"statusLine": {"type": "command", "command": "mt statusline"}}
    Statusline,
    /// Call MCP tools through mt: `list`, `tools <server> [tool..]`, `call` (a batch from stdin,
    /// or `<server> <tool> [json]`). Long results are kept for `mt out`. `mt mcp help`.
    Mcp {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The graph of skills, agents, MCP servers and tools (from disk and past transcripts), and
    /// the per-project settings layer that replaces Claude Code's always-on listings.
    Tools {
        #[command(subcommand)]
        op: ToolsCmd,
    },
    /// Tell every agent in a repo to use mt for code: a marked block in CLAUDE.md, AGENTS.md,
    /// GEMINI.md and (when the repo uses Cursor) .cursor/rules/mt.mdc. Idempotent.
    Init {
        dir: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        /// Take the mt block out again.
        #[arg(long)]
        remove: bool,
    },
    /// Every mt setting in one place. No argument on a terminal: guided setup. Also: list [--json],
    /// get KEY, set KEY VALUE, unset KEY, preset lockdown|balanced|nudge|off, env, doctor, path.
    /// --global writes the user file instead of the repo's.
    Config {
        args: Vec<String>,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        json: bool,
    },
    /// Open a live view of the repo graph in the browser, lit up by what the daemon and the
    /// Claude Code hooks are doing as it happens.
    #[command(alias = "viz")]
    Visualize {
        /// Port on 127.0.0.1 (0: any free port).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print the URL without opening a browser.
        #[arg(long)]
        no_open: bool,
        /// Scene to open with (switch any time in the page, keys 1-0 and -): graph, spectrum,
        /// scope, radial, starfield, tunnel, matrix, waterfall, fireworks, plasma, leds.
        #[arg(long)]
        mode: Option<String>,
    },
    /// Make a repo ready: the index up to date (incremental) and the daemon running and warm.
    Build { dir: Option<PathBuf> },
    /// From scratch: stop the daemon, drop the index, index again, start the daemon on the new
    /// graph. Kept outputs (.mt/out) and settings stay.
    Rebuild { dir: Option<PathBuf> },
    /// Everything mt records, in one view: the daemon's events, its logs, the Claude Code hook
    /// decisions, and `mt apply` step logs. `-f` follows events and hooks as they happen.
    Logs {
        /// Lines per source.
        #[arg(short = 'n', long, default_value_t = 15)]
        lines: usize,
        /// Only one source: events, serve, hooks, apply.
        #[arg(long)]
        source: Option<String>,
        /// Only lines that name this session id (or a prefix of it).
        #[arg(long)]
        session: Option<String>,
        #[arg(short, long)]
        follow: bool,
    },
    /// Hand work between agents. `mt brief save <name> <items>` (pointer numbers, `last`,
    /// `seen[:N]`, `path:Ls-e`, `path:Name`, files, `@brief`); the receiver reads it in one call
    /// with `mt expand @<name>`. `mt brief report <name> [--note ..] -` stores the receiver's
    /// claims (mt query JSON on stdin); `mt brief verify <name>` checks them and lists what other
    /// agents changed since. `mt brief` lists the repo's briefs.
    Brief {
        action: Option<String>,
        name: Option<String>,
        items: Vec<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Claim spans (same items as `mt brief save`) so other agents' `mt apply` into them is
    /// refused until you `mt release` or the claim expires.
    Claim {
        items: Vec<String>,
        #[arg(long, default_value_t = 30)]
        minutes: u64,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Release your claims: the given spans, or all of them.
    Release { items: Vec<String> },
    /// Every live claim in the repo.
    Claims,
    /// What other agents changed (through mt apply) in code you read, since you last asked.
    /// `--all`: in any file. `--peek`: do not move your cursor.
    Changes {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        peek: bool,
    },
    /// Split targets (files, directories, symbols) into slices with no code-graph edge between
    /// them, packed into `--agents N` by size; one brief per slice (`@<name>-1` ...).
    Split {
        targets: Vec<String>,
        #[arg(long, default_value_t = 2)]
        agents: usize,
        #[arg(long, default_value = "split")]
        name: String,
    },
    /// Find code by a question in plain words, no name needed: `mt search "how is an audit row
    /// written"`. The best three come with their code, the rest as numbered pointers.
    Search {
        question: Vec<String>,
        #[arg(long, default_value_t = 12)]
        limit: usize,
        /// Map view: every result as one line, no bodies.
        #[arg(long)]
        map: bool,
    },
    /// One section of the mt reference (the skill), or the list of sections.
    Ref { section: Option<String> },
    /// Claude Code plugin hook (session-start, prompt, pretool, session-end): reads the hook's
    /// JSON on stdin, prints the hook's JSON answer.
    Hook { event: String },
    /// List supported languages and how each file type is handled.
    Langs,
    /// Print the syntax tree of FILE (for writing `.mt/queries/<lang>.scm` patterns).
    Tree {
        file: PathBuf,
        #[arg(long, default_value_t = 8)]
        depth: usize,
    },
}

#[derive(Subcommand)]
enum ToolsCmd {
    /// Build the graph from definitions on disk and each MCP server's tools/list (servers are
    /// re-asked only when their config changes).
    Index {
        /// Ask every MCP server again.
        #[arg(long)]
        reprobe: bool,
        #[arg(long)]
        quiet: bool,
    },
    /// What the project's code says it uses: dependencies, marker files, languages, identifiers.
    Facts {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Rank skills, agents, MCP servers and tools for a task, with how to reach each one here.
    Find {
        query: Vec<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Extra settings file to count as a layer (what `claude --settings` would add).
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Print ranking evidence (scores) for every candidate.
        #[arg(long)]
        explain: bool,
    },
    /// Print one item: its graph neighbours and, for a skill or agent, its full body.
    Show {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Compute the settings layer that turns off what this project never used, with evidence.
    Profile {
        #[arg(long)]
        project: Option<PathBuf>,
        /// Merge it into <project>/.claude/settings.local.json (backup kept for --undo), plus the
        /// SessionStart index and UserPromptSubmit discovery hooks.
        #[arg(long)]
        write: bool,
        /// Write the layer alone to FILE (for `claude --settings FILE`).
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        undo: bool,
        /// Names, ids or plugin names to keep on regardless of usage.
        #[arg(long, value_delimiter = ',')]
        keep: Vec<String>,
        /// Also turn off plugins the code doesn't need even when they run hooks (their hooks stop).
        #[arg(long)]
        include_hooked: bool,
        /// Keep skill descriptions in the listing (default: names only, the hook adds descriptions).
        #[arg(long)]
        keep_descriptions: bool,
        /// Also build trimmed copies of kept plugins (only the skills this code needs) in a
        /// project-local marketplace, and switch the originals off.
        #[arg(long)]
        slim: bool,
        /// With --slim --write: install the trimmed copies for this project now (claude plugin
        /// marketplace add + install --scope local). Changes the plugin registry; --undo reverses it.
        #[arg(long)]
        install: bool,
    },
    /// Run one hook command behind the gate: same stdin, exit code and decisions; repeated or
    /// off-project context text is dropped, and bytes in and out are logged to /tmp/mt-gate.log.
    Gate {
        #[arg(long)]
        event: String,
        #[arg(long)]
        cmd_b64: Option<String>,
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Every hook Claude Code runs here with what the gate measured; --wrap FILE puts a settings
    /// file's hooks behind the gate (backup kept as FILE.mt-backup).
    Hooks {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        wrap: Option<PathBuf>,
    },
    /// SessionStart hook: recompute the written profile and apply what changed.
    HookStart,
    /// PostToolUse hook: record the MCP tool names this session uses.
    HookPost,
    /// Take one session's first request apart: each listing's size, each item with mt's decision.
    Explain {
        #[arg(long)]
        project: Option<PathBuf>,
        /// A session transcript; default: this project's latest.
        #[arg(long)]
        session: Option<PathBuf>,
    },
    /// Wire the graph into Codex (.codex/hooks.json) and Cursor (.cursor/rules/mt-tools.mdc).
    Agents {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Turn one item back on in <project>/.claude/settings.local.json.
    Enable {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// UserPromptSubmit hook: reads the hook JSON on stdin, adds the matching cards as context.
    Hook {
        #[arg(long, default_value_t = 3)]
        limit: usize,
        #[arg(long, default_value_t = 3.0)]
        min_score: f64,
        /// Extra settings layer to honour (what the session was started with via --settings).
        #[arg(long, env = "MT_TOOLS_SETTINGS")]
        settings: Option<PathBuf>,
    },
    /// Score ranking against a gold set: [{"prompt", "expect": [names], "k"?}].
    Bench {
        gold: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Fill the embedding cache for this repo's definitions (needed before a BERT model is used by
    /// the hook; the static model needs no cache). MT_EMBED_MODEL picks the model directory.
    Embed {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Every HTTP route: its handler and every call site in any sub-project (frontend, services, tests).
    Routes {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Counts per kind and how much context the always-on listings cost here.
    Stats {
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub kind: String,
    pub name: String,
    /// 0-based inclusive line range.
    pub start: usize,
    pub end: usize,
    /// Enclosing definition names, outermost first (class for a method, etc.).
    pub path: Vec<String>,
    /// 0-based line of the declaration itself (start may include doc comments above it).
    pub head: usize,
}

fn read(file: &Path) -> Result<String, ExitCode> {
    match fs::read(file) {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(e) => {
            eprintln!("mt: {}: {}", file.display(), e);
            Err(ExitCode::from(2))
        }
    }
}

fn print_skeleton(file: &Path, src: &str, tier: &Tier, defs: &[Def]) {
    if defs.is_empty() {
        println!("{}: no definitions found ({} lines, {})", file.display(), src.lines().count(), tier.label());
    }
    for d in defs {
        println!("{}{} {} L{}-{}", "  ".repeat(d.path.len()), d.kind, d.name, d.start + 1, d.end + 1);
    }
}

fn main() -> ExitCode {
    config::apply_env();
    let cli = Cli::parse();
    let open_index = |rebuild: bool| -> Result<index::Index, ExitCode> {
        let root = index::find_root(&std::env::current_dir().unwrap_or_default());
        if rebuild {
            let _ = fs::remove_file(root.join(".mt/index.sqlite"));
        }
        index::Index::open(&root).map_err(|e| { eprintln!("mt: cannot open index: {e}"); ExitCode::from(2) })
    };
    match cli.cmd {
        Cmd::Tools { op } => return tools_cmd(op),
        Cmd::Statusline => {
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            println!("{}", events::statusline(&serde_json::from_str(&input).unwrap_or_default()));
            ExitCode::SUCCESS
        }
        Cmd::Activity { session, json, line, statusline, limit } => {
            let mut root = index::find_root(&std::env::current_dir().unwrap_or_default());
            let mut session = session;
            if statusline {
                let mut input = String::new();
                let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
                let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
                if let Some(d) = v.pointer("/workspace/current_dir").or_else(|| v.get("cwd")).and_then(|d| d.as_str()) { root = index::find_root(Path::new(d)); }
                session = session.or_else(|| v.get("session_id").and_then(|s| s.as_str()).map(String::from));
            }
            let a = events::activity(&root, session.as_deref(), limit);
            if statusline {
                // nothing done yet in this session: an empty line, not a row of zeros
                if a["requests"] != 0 || a["compressions"] != 0 || a["mcp_calls"] != 0 { println!("{}", a["line"].as_str().unwrap_or("")); }
                return ExitCode::SUCCESS;
            }
            if json { println!("{a}"); }
            else if line { println!("{}", events::status_line(&a)); }
            else {
                println!("{}", events::status_line(&a));
                for c in a["by_cmd"].as_array().into_iter().flatten() { println!("  {:<10} {:>5}x  avg {} ms", c["cmd"].as_str().unwrap_or(""), c["count"], c["avg_ms"]); }
                println!("  compressions {}, ~{} tokens kept out of context; mcp calls {}; reindexes {}", a["compressions"], a["tokens_kept_out"], a["mcp_calls"], a["reindexes"]);
                for e in a["recent"].as_array().into_iter().flatten() { println!("  {:<9} {}{}", e["label"].as_str().unwrap_or(""), e["detail"].as_str().unwrap_or(""), e["ms"].as_f64().map(|m| format!("  ({m} ms)")).unwrap_or_default()); }
            }
            return ExitCode::SUCCESS;
        }
        Cmd::Mcp { args } => {
            let project = PathBuf::from(tools::repo_root(&std::env::current_dir().unwrap_or_default().to_string_lossy()));
            let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let (ok, out) = match a.as_slice() {
                ["list"] => (true, mcp::list(&project)),
                ["tools", server, names @ ..] => match mcp::tools(&project, server, &names.iter().map(|s| s.to_string()).collect::<Vec<_>>()) { Ok(o) => (true, o), Err(e) => (false, format!("mt mcp tools: {e}\n")) },
                ["call", server, tool, rest @ ..] if *server != "-" => {
                    let args: serde_json::Value = if rest.is_empty() { serde_json::json!({}) } else { match serde_json::from_str(&rest.join(" ")) { Ok(v) => v, Err(e) => { eprint!("mt mcp call: the arguments are not JSON: {e}\n"); return ExitCode::from(2); } } };
                    mcp::call(&project, &serde_json::json!([{"id": "a", "server": server, "tool": tool, "args": args}]))
                }
                ["call"] | ["call", "-"] => {
                    let mut input = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
                    match serde_json::from_str::<serde_json::Value>(&input) { Ok(b) => mcp::call(&project, &b), Err(e) => (false, format!("mt mcp call: the batch is not JSON: {e}\n")) }
                }
                _ => (a.first() == Some(&"help"), mcp::HELP.to_string()),
            };
            print!("{out}");
            return if ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
        }
        Cmd::Index { rebuild, action, debug } => {
            trace::start(&root_of_cwd(), "index", debug);
            let mut idx = match open_index(rebuild) { Ok(i) => i, Err(c) => return c };
            if let Some(verb) = action.first() {
                let mut list = idx.included();
                let cwd = std::env::current_dir().unwrap_or_default();
                // each folder relative to the repo root, as given (relative to here) or absolute
                let rel = |d: &str| -> Result<String, String> {
                    let abs = std::fs::canonicalize(cwd.join(d)).map_err(|e| format!("{d}: {e}"))?;
                    let root = std::fs::canonicalize(&idx.root).unwrap_or(idx.root.clone());
                    if !abs.is_dir() { return Err(format!("{d}: not a folder")); }
                    abs.strip_prefix(&root).map(|p| p.to_string_lossy().replace('\\', "/")).map_err(|_| format!("{d}: outside this repo ({})", root.display()))
                };
                match verb.as_str() {
                    "list" => {
                        if list.is_empty() { println!("no folders added: the index holds everything git does not ignore"); }
                        for d in &list { println!("{d}"); }
                        return ExitCode::SUCCESS;
                    }
                    "add" | "remove" if action.len() > 1 => {
                        for d in &action[1..] {
                            let r = if verb == "add" { rel(d) } else { rel(d).or_else(|_| Ok::<_, String>(d.trim_end_matches('/').to_string())) };
                            match r {
                                Ok(r) if verb == "add" => { if !list.contains(&r) { list.push(r.clone()); } println!("added {r}"); }
                                Ok(r) => { let n = list.len(); list.retain(|x| *x != r); println!("{} {r}", if list.len() < n { "removed" } else { "was not added:" }); }
                                Err(e) => { eprintln!("mt index {verb}: {e}"); return ExitCode::from(2); }
                            }
                        }
                        if let Err(e) = idx.set_included(&list) { eprintln!("mt index {verb}: {e}"); return ExitCode::from(2); }
                        // the worktree's server walks again now; without one, this process indexes
                        #[cfg(unix)]
                        if serve::request(&idx.root, &serve::Req { cmd: "touch".into(), exe: serve::exe_id(), ..Default::default() }).is_some() {
                            println!("the worktree's server re-indexed");
                            return ExitCode::SUCCESS;
                        }
                    }
                    other => { eprintln!("mt index: unknown action {other:?} (add <dir>..., remove <dir>..., list)"); return ExitCode::from(2); }
                }
            }
            let t = std::time::Instant::now();
            match idx.update() {
                Ok(st) => {
                    println!("{}: {} files, {} re-parsed, {} removed in {} ms (walk {} ms, parse {} ms, write {} ms)", idx.root.display(), st.files, st.changed, st.removed, t.elapsed().as_millis(), st.walk_ms, st.parse_ms, st.write_ms);
                    print!("{}", idx.summary());
                    if trace::on() { if let Some(p) = trace::finish(idx.trace_summary(t.elapsed().as_millis())) { eprintln!("mt: debug trace written to {}", p.display()); } }
                    ExitCode::SUCCESS
                }
                Err(e) => { eprintln!("mt index: {e}"); ExitCode::from(2) }
            }
        }
        Cmd::Context { query, budget, max_pointers, json, edit, map } => {
            #[cfg(unix)]
            if let Some(r) = via_daemon(&root_of_cwd(), serve::Req { cmd: "context".into(), query: query.clone(), budget, max_pointers, json, edit, map, session: query::session_id().unwrap_or_default(), ..Default::default() }) {
                return emit(r);
            }
            let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
            let t0 = std::time::Instant::now();
            let st = match idx.update() { Ok(s) => s, Err(e) => { eprintln!("mt: {e}"); return ExitCode::from(2) } };
            let wm = work::mark();
            match context::run(&idx, &query, &context::Opts { budget, json, max_pointers, edit, map, session: None }, &st, t0.elapsed().as_millis(), t0) {
                Ok(s) => { print!("{s}{}", if json { String::new() } else { work::footer(&idx.root, query::session_id().as_deref(), "context", &s, &wm) }); ExitCode::SUCCESS }
                Err(e) => { println!("{e}"); ExitCode::from(1) }
            }
        }
        Cmd::Apply { file, dry_run } => {
            let input = match &file {
                Some(f) if f.as_os_str() != "-" => match fs::read_to_string(f) { Ok(s) => s, Err(e) => { eprintln!("mt apply: {}: {e}", f.display()); return ExitCode::from(2) } },
                _ => { let mut s = String::new(); let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s); s }
            };
            // the repo's daemon applies it when one is running: its graph is already fresh, and it
            // re-indexes exactly the written files once (no second indexer racing its watcher)
            #[cfg(unix)]
            if std::env::var_os("MT_NO_DAEMON").is_none() {
                let root = root_of_cwd();
                let req = serve::Req { cmd: "apply".into(), input: input.clone(), dry_run, exe: serve::exe_id(),
                    cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned(), env: std::env::vars().collect(), ..Default::default() };
                if let Some(r) = serve::request_apply(&root, &req) {
                    println!("{}", r.out.trim_end());
                    return if r.ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
                }
            }
            let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
            let (ok, out) = apply::run(&mut idx, &input, dry_run);
            #[cfg(unix)]
            if !dry_run && std::env::var_os("MT_NO_DAEMON").is_none() {
                // the daemon's watcher would catch up in milliseconds; tell it now anyway
                let _ = serve::request(&idx.root, &serve::Req { cmd: "touch".into(), exe: serve::exe_id(), ..Default::default() });
            }
            if out.ends_with('\n') { print!("{out}") } else { println!("{out}") }
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Cmd::Compress { kind, max_lines, label, name } => {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
            let name = name.map(|n| compress::clean_name(&n)).unwrap_or_else(|| compress::default_name("default"));
            let (out, _) = compress::compress_and_store(&s, compress::Kind::parse(&kind), max_lines, &label, &name);
            println!("{out}");
            ExitCode::SUCCESS
        }
        Cmd::Run { max_lines, name, cmd } => {
            // one argument is a shell command line; several are argv
            let line = if cmd.len() == 1 { cmd[0].clone() } else { cmd.iter().map(|a| if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) { a.clone() } else { format!("'{}'", a.replace('\'', "'\\''")) }).collect::<Vec<_>>().join(" ") };
            let o = std::process::Command::new("sh").arg("-c").arg(format!("{line} 2>&1")).output();
            match o {
                Ok(o) => {
                    let code = o.status.code().unwrap_or(1);
                    let text = String::from_utf8_lossy(&o.stdout);
                    let name = name.map(|n| compress::clean_name(&n)).unwrap_or_else(|| compress::default_name(&compress::command_name(&line)));
                    let (out, _) = compress::compress_and_store(&text, compress::Kind::Auto, max_lines, &format!("`{line}` (exit {code})"), &name);
                    println!("{out}");
                    if code != 0 { println!("[exit {code}]"); }
                    ExitCode::from(code.clamp(0, 255) as u8)
                }
                Err(e) => { eprintln!("mt run: {e}"); ExitCode::from(127) }
            }
        }
        Cmd::Out { id, name, mut question, json, jq, grep, context, lines, level } => {
            let lines = lines.and_then(|l| { let (a, b) = l.trim_start_matches('L').split_once('-')?; Some((a.parse().ok()?, b.trim_start_matches('L').parse().ok()?)) });
            // `mt out --name vercel-logs "why did it fail"`: the first word is the question, not an id
            let id = match id { Some(i) if !compress::is_kept_id(&i) => { question.insert(0, i); None } other => other };
            let q = compress::OutQuery { ask: (!question.is_empty()).then(|| question.join(" ")), grep, ctx: context, lines, level, json, jq };
            let (ok, out) = compress::out(id.as_deref(), name.as_deref(), &q);
            print!("{out}");
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Cmd::Show { src, title } => {
            let (ok, out) = vctx::show(src.as_deref(), title.as_deref().unwrap_or(""));
            print!("{out}");
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Cmd::Expand { handles } => {
            // the session's daemon holds the last reply's items in memory; without one, from disk
            #[cfg(unix)]
            if let Some(r) = via_daemon(&root_of_cwd(), serve::Req { cmd: "expand".into(), input: handles.join("\n"), session: query::session_id().unwrap_or_default(), ..Default::default() }) {
                return emit(r);
            }
            let wm = work::mark();
            let (ok, out) = query::expand(&root_of_cwd(), &handles, None);
            print!("{out}");
            if ok { print!("{}", work::footer(&root_of_cwd(), query::session_id().as_deref(), "expand", &out, &wm)); }
            if let Some(n) = coord::notice(&root_of_cwd(), &coord::me()) { print!("{n}") }
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Cmd::Query { file } => {
            let input = match &file {
                Some(f) if f.as_os_str() != "-" => match fs::read_to_string(f) { Ok(s) => s, Err(e) => { eprintln!("mt query: {}: {e}", f.display()); return ExitCode::from(2) } },
                _ => { let mut s = String::new(); let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s); s }
            };
            #[cfg(unix)]
            if let Some(r) = via_daemon(&root_of_cwd(), serve::Req { cmd: "query".into(), input: input.clone(), session: query::session_id().unwrap_or_default(), ..Default::default() }) {
                return emit(r);
            }
            let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
            let t0 = std::time::Instant::now();
            let st = match idx.update() { Ok(s) => s, Err(e) => { eprintln!("mt: {e}"); return ExitCode::from(2) } };
            let wm = work::mark();
            match query::run(&idx, &input, None, &st, t0.elapsed().as_millis(), t0) {
                Ok(s) => { print!("{s}{}", work::footer(&idx.root, query::session_id().as_deref(), "query", &s, &wm)); if let Some(n) = coord::notice(&root_of_cwd(), &coord::me()) { print!("{n}") } ExitCode::SUCCESS }
                Err(e) => { println!("{e}"); ExitCode::from(1) }
            }
        }
        #[cfg(unix)]
        Cmd::Serve { stop, status } => {
            let root = root_of_cwd();
            if stop || status {
                let cmd = if stop { "stop" } else { "status" };
                return match serve::request(&root, &serve::Req { cmd: cmd.into(), ..Default::default() }) {
                    Some(r) => emit(r),
                    None => { println!("no mt daemon running for {}", root.display()); ExitCode::from(1) }
                };
            }
            let _ = fs::create_dir_all(root.join(".mt"));
            match serve::serve(root) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("mt serve: {e}"); ExitCode::from(1) }
            }
        }
        #[cfg(not(unix))]
        Cmd::Serve { .. } => { eprintln!("mt serve: the daemon needs Unix sockets; on this OS every command runs in-process"); ExitCode::from(1) }
        #[cfg(unix)]
        Cmd::Build { dir } => { let r = dir.map(|d| index::find_root(&d)).unwrap_or_else(root_of_cwd); trace::start(&r, "build", false); let (ok, s) = ops::build(&r); print!("{s}"); if let Some(p) = trace::finish(serde_json::json!({})) { eprintln!("mt: debug trace written to {}", p.display()); } if ok { ExitCode::SUCCESS } else { ExitCode::from(1) } }
        #[cfg(unix)]
        Cmd::Rebuild { dir } => { let r = dir.map(|d| index::find_root(&d)).unwrap_or_else(root_of_cwd); trace::start(&r, "rebuild", false); let (ok, s) = ops::rebuild(&r); print!("{s}"); if let Some(p) = trace::finish(serde_json::json!({})) { eprintln!("mt: debug trace written to {}", p.display()); } if ok { ExitCode::SUCCESS } else { ExitCode::from(1) } }
        #[cfg(unix)]
        Cmd::Logs { lines, source, session, follow } => {
            let o = ops::LogsOpts { n: lines, source, session };
            if follow { ops::follow(&root_of_cwd(), &o) }
            print!("{}", ops::logs(&root_of_cwd(), &o));
            ExitCode::SUCCESS
        }
        #[cfg(not(unix))]
        Cmd::Build { .. } | Cmd::Rebuild { .. } | Cmd::Logs { .. } => { eprintln!("mt: build/rebuild/logs need the Unix daemon"); ExitCode::from(1) }
        Cmd::Search { question, limit, map } => {
            let q = serde_json::json!({"queries": [{"id": "s", "search": question.join(" "), "limit": limit}], "view": if map { "map" } else { "" }}).to_string();
            #[cfg(unix)]
            if let Some(r) = via_daemon(&root_of_cwd(), serve::Req { cmd: "query".into(), input: q.clone(), session: query::session_id().unwrap_or_default(), ..Default::default() }) {
                return emit(r);
            }
            let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
            let t0 = std::time::Instant::now();
            let st = match idx.update() { Ok(s) => s, Err(e) => { eprintln!("mt: {e}"); return ExitCode::from(2) } };
            let wm = work::mark();
            match query::run(&idx, &q, None, &st, t0.elapsed().as_millis(), t0) {
                Ok(s) => { print!("{s}{}", work::footer(&idx.root, query::session_id().as_deref(), "search", &s, &wm)); ExitCode::SUCCESS }
                Err(e) => { println!("{e}"); ExitCode::from(1) }
            }
        }
        Cmd::Init { dir, dry_run, remove } => {
            let d = dir.unwrap_or_else(root_of_cwd);
            let (ok, s) = init::run(&d, dry_run, remove);
            print!("{s}");
            if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
        }
        Cmd::Config { args, global, json } => {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let r = match a.as_slice() {
                [] => config::wizard(),
                ["list"] | ["ls"] => Ok(config::list(json)),
                ["get", k] => config::get(k),
                ["set", k, v] => config::set(k, v, global),
                ["unset", k] => config::unset(k, global),
                ["preset", p] => config::preset(p, global),
                ["env"] => Ok(config::env_lines()),
                ["doctor"] => Ok(config::doctor()),
                ["path"] | ["paths"] => Ok(config::paths()),
                _ => Err("usage: mt config [list|get KEY|set KEY VALUE|unset KEY|preset NAME|env|doctor|path] [--global] [--json]".into()),
            };
            match r { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { eprintln!("{e}"); ExitCode::from(2) } }
        }
        Cmd::Visualize { port, no_open, mode } => match viz::run(root_of_cwd(), port, !no_open, mode.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => { eprintln!("mt visualize: {e}"); ExitCode::from(1) }
        },
        Cmd::Brief { action, name, items, note } => {
            let root = root_of_cwd();
            let sid = coord::me();
            let r: Result<String, String> = match (action.as_deref(), name.as_deref()) {
                (None, _) | (Some("list"), _) => Ok(coord::list_briefs(&root)),
                (Some("save"), Some(n)) => {
                    let (rows, errs) = coord::resolve(&root, Some(&sid), &items);
                    if rows.is_empty() { Err(format!("mt brief save: nothing to save\n  {}", errs.join("\n  "))) }
                    else { coord::save_brief(&root, &sid, n, rows, &note).map(|s| if errs.is_empty() { s } else { format!("{s}## skipped: {}\n", errs.join("; ")) }) }
                }
                (Some("report"), Some(n)) => {
                    let q = if items.iter().any(|i| i == "-") {
                        let mut s = String::new();
                        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
                        if s.trim().is_empty() { None } else { match serde_json::from_str(&s) { Ok(v) => Some(v), Err(e) => { eprintln!("mt brief report: the claims are not JSON: {e}"); return ExitCode::from(2) } } }
                    } else { None };
                    coord::report(&root, &sid, n, &note, q)
                }
                (Some("verify"), Some(n)) => match coord::verify(&root, n) {
                    Err(e) => Err(e),
                    Ok((text, None)) => Ok(text),
                    Ok((text, Some(input))) => {
                        print!("{text}");
                        #[cfg(unix)]
                        if let Some(r) = via_daemon(&root, serve::Req { cmd: "query".into(), input: input.clone(), session: query::session_id().unwrap_or_default(), ..Default::default() }) {
                            return emit(r);
                        }
                        let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
                        let t0 = std::time::Instant::now();
                        let st = match idx.update() { Ok(s) => s, Err(e) => { eprintln!("mt: {e}"); return ExitCode::from(2) } };
                        query::run(&idx, &input, None, &st, t0.elapsed().as_millis(), t0)
                    }
                },
                (Some(a), None) if ["save", "report", "verify"].contains(&a) => Err(format!("mt brief {a}: give the brief's name")),
                (Some(a), _) => Err(format!("mt brief {a}: unknown action (save, report, verify, list)")),
            };
            match r { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        Cmd::Claim { items, minutes, note } => {
            let root = root_of_cwd();
            let sid = coord::me();
            let (rows, errs) = coord::resolve(&root, Some(&sid), &items);
            if !errs.is_empty() { println!("mt claim: nothing claimed\n  {}", errs.join("\n  ")); return ExitCode::from(1); }
            match coord::claim(&root, &sid, rows, minutes * 60, &note) { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        Cmd::Release { items } => {
            let root = root_of_cwd();
            let sid = coord::me();
            let rows = if items.is_empty() || items.iter().any(|i| i == "all") { None } else { Some(coord::resolve(&root, Some(&sid), &items).0) };
            match coord::release(&root, &sid, rows) { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        Cmd::Claims => { print!("{}", coord::list_leases(&root_of_cwd())); ExitCode::SUCCESS }
        Cmd::Changes { all, peek } => { print!("{}", coord::changes(&root_of_cwd(), &coord::me(), all, peek)); ExitCode::SUCCESS }
        Cmd::Split { targets, agents, name } => {
            let root = root_of_cwd();
            let mut idx = match open_index(false) { Ok(i) => i, Err(c) => return c };
            if let Err(e) = idx.update() { eprintln!("mt: {e}"); return ExitCode::from(2) }
            match coord::split(&idx, &root, &coord::me(), &targets, agents, &name) { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        #[cfg(unix)]
        Cmd::Ref { section } => { let (ok, s) = hook::reference(section.as_deref()); print!("{s}"); if ok { ExitCode::SUCCESS } else { ExitCode::from(1) } }
        #[cfg(not(unix))]
        Cmd::Ref { .. } => ExitCode::SUCCESS,
        #[cfg(unix)]
        Cmd::Hook { event } => ExitCode::from(hook::run(&event) as u8),
        #[cfg(not(unix))]
        Cmd::Hook { .. } => ExitCode::SUCCESS,
        Cmd::Langs => {
            println!("tree-sitter (exact spans):");
            for l in languages::LANGS {
                println!("  {}", l.id);
            }
            println!("markdown: headings as nested sections");
            println!("heuristic (approximate, kinds shown as ~kind): any other file that looks like code");
            println!("text: everything else; /regex/ returns line windows");
            println!("add patterns for any bundled grammar in .mt/queries/<lang>.scm or ~/.config/mt/queries/<lang>.scm");
            ExitCode::SUCCESS
        }
        Cmd::Tree { file, depth } => {
            let src = match read(&file) { Ok(s) => s, Err(c) => return c };
            match languages::dump_tree(&file, &src, depth) {
                Ok(t) => { print!("{t}"); ExitCode::SUCCESS }
                Err(e) => { eprintln!("mt: {}: {e}", file.display()); ExitCode::from(1) }
            }
        }
        Cmd::Skeleton { file } => {
            let src = match read(&file) { Ok(s) => s, Err(c) => return c };
            let tier = detect(&file, &src);
            let defs = definitions_with(&tier, &src);
            print_skeleton(&file, &src, &tier, &defs);
            ExitCode::SUCCESS
        }
        Cmd::Span { file, query, context, session, force, max } => {
            // `mt span path:L10-20` and `mt span path:Name` are the forms models write
            let (file, query) = match query {
                Some(q) => (file, q),
                None => match file.to_string_lossy().rsplit_once(':') {
                    Some((f, q)) if !q.is_empty() && !f.is_empty() => (PathBuf::from(f), q.to_string()),
                    _ => { eprintln!("mt span: give FILE and QUERY (a name, \"## Heading\", key path, '/regex/' or L10-20), or FILE:QUERY"); return ExitCode::from(2) }
                },
            };
            let src = match read(&file) { Ok(s) => s, Err(c) => return c };
            let lines: Vec<&str> = src.lines().collect();
            let tier = detect(&file, &src);
            let defs = definitions_with(&tier, &src);
            let mut hits: Vec<(usize, usize, String, String)> =
                find(&defs, &lines, &query).into_iter().map(|d| (d.start, d.end, d.kind.clone(), d.name.clone())).collect();
            // a line range: L10-20, 10-20, L10
            if hits.is_empty() && !lines.is_empty() {
                let r = query.trim_start_matches('L');
                let (a, b) = r.split_once('-').map(|(a, b)| (a, b.trim_start_matches('L'))).unwrap_or((r, r));
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    if a >= 1 && b >= a && a <= lines.len() { hits.push((a - 1, (b - 1).min(lines.len() - 1), "lines".into(), format!("L{a}-{b}"))); }
                }
            }
            let ctx = context.unwrap_or(0);
            if hits.is_empty() {
                // No definition: for a regex, fall back to line windows around matches.
                if let Some(re) = as_regex(&query) {
                    let w = context.unwrap_or(10);
                    hits = lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| (i.saturating_sub(w), (i + w).min(lines.len().saturating_sub(1)), "lines".to_string(), format!("match L{}", i + 1))).collect();
                    hits.dedup_by(|b, a| b.0 <= a.1 && { a.1 = a.1.max(b.1); true });
                }
            }
            if hits.is_empty() {
                println!("no definition matching {:?} in {} ({} lines, {}). skeleton:", query, file.display(), lines.len(), tier.label());
                print_skeleton(&file, &src, &tier, &defs);
                return ExitCode::from(1);
            }
            let shown = hits.len().min(max);
            for (start, end, kind, name) in hits.iter().take(shown) {
                let (s, e) = if kind == "lines" { (*start, *end) } else { (start.saturating_sub(ctx), (end + ctx).min(lines.len().saturating_sub(1))) };
                let body: String = lines[s..=e].join("\n");
                let h = fnv1a(body.as_bytes());
                let key = format!("{}|{}|{}|{:016x}", file.display(), kind, name, h);
                let seen = session.as_deref().map(|sid| session_seen(sid, &key)).unwrap_or(false);
                println!(
                    "{} L{}-{} {} {} hash={:08x} ({} lines){}",
                    file.display(), s + 1, e + 1, kind, name, h as u32, e - s + 1,
                    if seen && !force { " unchanged, already shown this session" } else { "" }
                );
                if seen && !force {
                    continue;
                }
                let w = (e + 1).to_string().len();
                for (i, l) in lines[s..=e].iter().enumerate() {
                    println!("{:>w$}\t{}", s + i + 1, l, w = w);
                }
            }
            if hits.len() > shown {
                println!("... {} more matches not shown (raise --max)", hits.len() - shown);
            }
            ExitCode::SUCCESS
        }
    }
}

fn root_of_cwd() -> PathBuf {
    index::find_root(&std::env::current_dir().unwrap_or_default())
}

/// Ask the repo's daemon; if none answers, start one for next time and return None so this
/// call runs in-process. MT_NO_DAEMON=1 turns the daemon off.
#[cfg(unix)]
fn via_daemon(root: &Path, mut req: serve::Req) -> Option<serve::Resp> {
    if std::env::var_os("MT_NO_DAEMON").is_some() || !root.join(".mt").is_dir() {
        return None;
    }
    req.exe = serve::exe_id();
    let r = serve::request(root, &req);
    if r.is_none() {
        serve::spawn(root);
    }
    r
}

#[cfg(unix)]
fn emit(r: serve::Resp) -> ExitCode {
    if r.out.ends_with('\n') { print!("{}", r.out) } else { println!("{}", r.out) }
    if let Some(n) = coord::notice(&root_of_cwd(), &coord::me()) { print!("{n}") }
    if r.ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

// ---------------------------------------------------------------- matching

fn as_regex(q: &str) -> Option<Regex> {
    let q = q.trim();
    if q.len() > 2 && q.starts_with('/') && q.ends_with('/') { Regex::new(&q[1..q.len() - 1]).ok() } else { None }
}

pub fn find<'a>(defs: &'a [Def], lines: &[&str], query: &str) -> Vec<&'a Def> {
    let q = query.trim();
    if let Some(re) = as_regex(q) {
        let mut out: Vec<&Def> = vec![];
        for ln in lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i) {
            if let Some(d) = enclosing(defs, ln) {
                if !out.iter().any(|o| std::ptr::eq(*o, d)) {
                    out.push(d);
                }
            }
        }
        return out;
    }
    let q = q.trim_start_matches('#').trim();
    // Whole-name match first: headings and selectors may contain dots themselves.
    let whole: Vec<&Def> = defs.iter().filter(|d| d.name == q).collect();
    if !whole.is_empty() {
        return whole;
    }
    let (prefix, name) = match q.rsplit_once('.') {
        Some((p, n)) if !p.is_empty() && !n.is_empty() => (Some(p), n),
        _ => (None, q),
    };
    let in_scope = |d: &Def| match prefix {
        None => true,
        Some(p) => {
            let joined = d.path.join(".");
            joined == p || joined.ends_with(&format!(".{p}")) || d.path.iter().any(|a| a == p)
                || d.name.starts_with(&format!("{p}."))
                || lines[d.start..=d.end.min(d.start + 3)].iter().any(|l| l.contains(p))
        }
    };
    // Receiver-qualified names (`walker.walk`) also match on their last segment.
    let last = |d: &Def| d.name.rsplit_once('.').map(|(_, l)| l.to_string()).unwrap_or_else(|| d.name.clone());
    let exact: Vec<&Def> = defs.iter().filter(|d| (d.name == name || last(d) == name) && in_scope(d)).collect();
    if !exact.is_empty() {
        return exact;
    }
    let lname = name.to_lowercase();
    let ci: Vec<&Def> = defs.iter().filter(|d| (d.name.to_lowercase() == lname || last(d).to_lowercase() == lname) && in_scope(d)).collect();
    if !ci.is_empty() {
        return ci;
    }
    defs.iter().filter(|d| d.name.to_lowercase().contains(&lname) && in_scope(d)).collect()
}

/// Smallest definition containing line `ln`.
fn enclosing(defs: &[Def], ln: usize) -> Option<&Def> {
    defs.iter().filter(|d| d.start <= ln && ln <= d.end).min_by_key(|d| d.end - d.start)
}

// ---------------------------------------------------------------- definitions

pub fn definitions(path: &Path, src: &str) -> Vec<Def> {
    definitions_with(&detect(path, src), src)
}

pub(crate) fn definitions_with(tier: &Tier, src: &str) -> Vec<Def> {
    let lines: Vec<&str> = src.lines().collect();
    let mut defs = match tier {
        Tier::Markdown => return markdown_defs(&lines),
        Tier::TreeSitter(l) => {
            let mut d = languages::treesitter_defs(l, src);
            for x in d.iter_mut() {
                x.start = with_leading(&lines, x.start, l.leading);
            }
            d
        }
        Tier::Heuristic => {
            let mut d = languages::heuristic_defs(&lines);
            for x in d.iter_mut() {
                x.start = with_leading(&lines, x.start, &["//", "/*", "*", "#", "--", ";", "["]);
            }
            d
        }
        Tier::Text => vec![],
    };
    defs.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    defs
}

/// Pull contiguous comment / decorator / attribute lines directly above a definition into it.
fn with_leading(lines: &[&str], start: usize, prefixes: &[&str]) -> usize {
    let mut s = start.min(lines.len().saturating_sub(1));
    while s > 0 {
        let t = lines[s - 1].trim_start();
        let attached = prefixes.iter().any(|p| t.starts_with(p))
            // `[` attributes (C#) and `#[` (Rust) only when the line is the whole attribute
            && (!(t.starts_with('[') || t.starts_with("#[")) || t.trim_end().ends_with(']'))
            // `#` lines in C-family files are preprocessor, not comments
            && !(t.starts_with("#include") || t.starts_with("#import") || t.starts_with("#define") || t.starts_with("#if") || t.starts_with("#endif") || t.starts_with("#pragma") || t.starts_with("#region") || t.starts_with("#endregion"));
        if !attached || t.is_empty() || t.starts_with("#!") {
            break;
        }
        s -= 1;
    }
    s
}

fn markdown_defs(lines: &[&str]) -> Vec<Def> {
    let mut heads: Vec<(usize, usize, String)> = vec![]; // (level, line, title)
    let mut in_fence = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            heads.push((hashes, i, t[hashes..].trim().trim_end_matches('#').trim().to_string()));
        }
    }
    let mut out = vec![];
    for (idx, (level, line, title)) in heads.iter().enumerate() {
        let end = heads[idx + 1..].iter().find(|(l, _, _)| l <= level).map(|(_, ln, _)| ln - 1).unwrap_or(lines.len().saturating_sub(1));
        let mut e = end;
        while e > *line && lines[e].trim().is_empty() {
            e -= 1;
        }
        let mut path = vec![];
        let mut lvl = *level;
        for (l, _, t) in heads[..idx].iter().rev() {
            if *l < lvl {
                lvl = *l;
                path.push(t.clone());
            }
        }
        path.reverse();
        out.push(Def { kind: format!("h{level}"), name: title.clone(), start: *line, end: e, path, head: *line });
    }
    out
}

// ---------------------------------------------------------------- session cache

fn session_seen(sid: &str, key: &str) -> bool {
    let safe: String = sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let p = std::env::temp_dir().join(format!("mt-session-{safe}.log"));
    let prior = fs::read_to_string(&p).unwrap_or_default();
    if prior.lines().any(|l| l == key) {
        return true;
    }
    let _ = fs::OpenOptions::new().append(true).create(true).open(&p).and_then(|mut f| {
        use std::io::Write;
        writeln!(f, "{key}")
    });
    false
}

pub(crate) fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn defs_named(file: &str, src: &str) -> Vec<Def> {
        definitions(Path::new(file), src)
    }
    fn span_in(file: &str, src: &str, q: &str) -> Option<(usize, usize)> {
        let d = defs_named(file, src);
        let lines: Vec<&str> = src.lines().collect();
        find(&d, &lines, q).first().map(|d| (d.start + 1, d.end + 1))
    }
    fn span(ext: &str, src: &str, q: &str) -> Option<(usize, usize)> {
        span_in(&format!("f.{ext}"), src, q)
    }
    fn names(file: &str, src: &str) -> Vec<String> {
        defs_named(file, src).into_iter().map(|d| d.name).collect()
    }

    const GO: &str = "package x\n\n// Doc for Foo.\nfunc Foo() int {\n\treturn 1\n}\n\ntype S struct {\n\tA int\n}\n\nfunc (s *S) Bar() {\n\tFoo()\n}\n\nfunc (t T) Bar() {}\n\nconst Limit = 10\n";

    #[test]
    fn go_func_with_doc_comment() {
        assert_eq!(span("go", GO, "Foo"), Some((3, 6)));
    }
    #[test]
    fn go_type_and_qualified_method() {
        assert_eq!(span("go", GO, "S"), Some((8, 10)));
        assert_eq!(span("go", GO, "S.Bar"), Some((12, 14)));
        assert_eq!(span("go", GO, "T.Bar"), Some((16, 16)));
        assert_eq!(span("go", GO, "Limit"), Some((18, 18)));
        let d = defs_named("f.go", GO);
        let lines: Vec<&str> = GO.lines().collect();
        assert_eq!(find(&d, &lines, "Bar").len(), 2);
    }
    #[test]
    fn go_method_beats_type_with_same_prefix() {
        let src = "package x\n\ntype walker struct{}\n\nfunc (w *walker) walk() {}\n";
        assert_eq!(span("go", src, "walker.walk"), Some((5, 5)));
        assert_eq!(span("go", src, "walk"), Some((5, 5)));
        assert_eq!(span("go", src, "walker"), Some((3, 3)));
    }
    #[test]
    fn go_regex_picks_enclosing() {
        assert_eq!(span("go", GO, "/\\tFoo\\(\\)/"), Some((12, 14)));
    }

    const PY: &str = "import os\n\n@dataclass\nclass C:\n    x: int = 0\n\n    def m(self):\n        return self.x\n\n\ndef top():\n    pass\n\nLIMIT = 3\n";

    #[test]
    fn py_decorated_class_method_constant() {
        assert_eq!(span("py", PY, "C"), Some((3, 8)));
        assert_eq!(span("py", PY, "C.m"), Some((7, 8)));
        assert_eq!(span("py", PY, "top"), Some((11, 12)));
        assert_eq!(span("py", PY, "LIMIT"), Some((14, 14)));
    }

    const TS: &str = "import x from 'y';\n\nexport const handler = async (req: Req) => {\n  return 1;\n};\n\nexport interface Opts {\n  a: number;\n}\n\nclass K {\n  run() {\n    return 2;\n  }\n}\n\nfunction inner() {\n  const local = () => 3;\n  return local();\n}\n\nexport type Id = string;\nexport const CONFIG = { a: 1 };\n";

    #[test]
    fn ts_exported_arrow_interface_method_type_const() {
        assert_eq!(span("ts", TS, "handler"), Some((3, 5)));
        assert_eq!(span("ts", TS, "Opts"), Some((7, 9)));
        assert_eq!(span("ts", TS, "K.run"), Some((12, 14)));
        assert_eq!(span("ts", TS, "local"), Some((18, 18)));
        assert_eq!(span("ts", TS, "Id"), Some((22, 22)));
        assert_eq!(span("ts", TS, "CONFIG"), Some((23, 23)));
        assert_eq!(span("tsx", TS, "K.run"), Some((12, 14)));
    }

    const CS: &str = "namespace N {\n  public class P {\n    [SerializeField] private int f;\n    public int Prop { get; set; }\n    public P() {}\n    public void Go() {\n      f++;\n    }\n  }\n  public enum E { A, B }\n  public struct V { }\n}\n";

    #[test]
    fn cs_members() {
        assert_eq!(span("cs", CS, "P.Go"), Some((6, 8)));
        assert_eq!(span("cs", CS, "f"), Some((3, 3)));
        assert_eq!(span("cs", CS, "Prop"), Some((4, 4)));
        assert_eq!(span("cs", CS, "E"), Some((10, 10)));
        assert_eq!(span("cs", CS, "V"), Some((11, 11)));
    }

    #[test]
    fn rust_items_and_impl_methods() {
        let src = "/// Doc\npub struct S { a: u8 }\n\nimpl S {\n    pub fn go(&self) -> u8 {\n        self.a\n    }\n}\n\nconst MAX: u8 = 3;\n\n#[derive(Debug)]\nenum E { A }\n";
        assert_eq!(span("rs", src, "S"), Some((1, 2)));
        assert_eq!(span("rs", src, "S.go"), Some((5, 7)));
        assert_eq!(span("rs", src, "MAX"), Some((10, 10)));
        assert_eq!(span("rs", src, "E"), Some((12, 13)));
    }

    #[test]
    fn java_class_method_ctor() {
        let src = "class A {\n  private int x;\n  A() {}\n  @Override\n  public String toString() {\n    return \"\";\n  }\n}\n";
        assert_eq!(span("java", src, "A.toString"), Some((4, 7)));
        assert_eq!(span("java", src, "x"), Some((2, 2)));
    }

    #[test]
    fn c_and_cpp() {
        let c = "#include <x.h>\n/* add */\nint add(int a, int b) {\n  return a + b;\n}\nstruct P { int x; };\n";
        assert_eq!(span("c", c, "add"), Some((2, 5)));
        assert_eq!(span("c", c, "P"), Some((6, 6)));
        let cpp = "namespace n {\nclass W {\n public:\n  void f();\n};\nvoid W::f() {\n}\n}\n";
        assert_eq!(span("cpp", cpp, "W"), Some((2, 5)));
        assert!(names("f.cpp", cpp).contains(&"f".to_string()));
    }

    #[test]
    fn swift_kotlin_scala() {
        let sw = "struct Point {\n  var x: Int\n  func len() -> Int {\n    return x\n  }\n}\nclass Box {}\n";
        assert_eq!(span("swift", sw, "Point.len"), Some((3, 5)));
        assert_eq!(span("swift", sw, "Box"), Some((7, 7)));
        let kt = "package p\nclass Foo(val x: Int) {\n    fun bar(): Int { return x }\n}\nobject Single { fun go() {} }\nfun top() {}\n";
        assert_eq!(span("kt", kt, "Foo.bar"), Some((3, 3)));
        assert_eq!(span("kt", kt, "Single"), Some((5, 5)));
        assert_eq!(span("kt", kt, "top"), Some((6, 6)));
        let sc = "class Foo {\n  def bar(x: Int): Int = x\n}\nobject O { val v = 1 }\n";
        assert_eq!(span("scala", sc, "Foo.bar"), Some((2, 2)));
        assert_eq!(span("scala", sc, "O"), Some((4, 4)));
    }

    #[test]
    fn ruby_php_lua_bash() {
        let rb = "class A\n  def go\n    1\n  end\nend\n";
        assert_eq!(span("rb", rb, "A.go"), Some((2, 4)));
        let php = "<?php\nclass A {\n  function go() { return 1; }\n}\nfunction top() {}\n";
        assert_eq!(span("php", php, "A.go"), Some((3, 3)));
        assert_eq!(span("php", php, "top"), Some((5, 5)));
        let lua = "local function f()\n  return 1\nend\nfunction M.g() end\n";
        assert_eq!(span("lua", lua, "f"), Some((1, 3)));
        let sh = "#!/bin/bash\n# build it\nbuild() {\n  echo hi\n}\nfunction clean {\n  rm -rf x\n}\n";
        assert_eq!(span("sh", sh, "build"), Some((2, 5)));
        assert_eq!(span("sh", sh, "clean"), Some((6, 8)));
        // shebang detection for an extensionless script
        assert_eq!(span_in("tools/run", sh, "build"), Some((2, 5)));
    }

    #[test]
    fn data_and_markup() {
        let json = "{\n  \"name\": \"x\",\n  \"scripts\": {\n    \"build\": \"tsc\",\n    \"test\": \"vitest\"\n  }\n}\n";
        assert_eq!(span("json", json, "scripts"), Some((3, 6)));
        assert_eq!(span("json", json, "scripts.test"), Some((5, 5)));
        let yaml = "services:\n  web:\n    image: x\n  db:\n    image: y\n";
        assert_eq!(span("yaml", yaml, "services.db"), Some((4, 5)));
        let toml = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n";
        assert_eq!(span("toml", toml, "dependencies"), Some((4, 5)));
        let css = ".btn { color: red; }\n.card {\n  margin: 0;\n}\n";
        assert_eq!(span("css", css, ".card"), Some((2, 4)));
        let html = "<html>\n<body>\n<div id=\"main\">\n  <p>x</p>\n</div>\n</body>\n</html>\n";
        assert_eq!(span("html", html, "main"), Some((3, 5)));
        let xml = "<Project>\n  <PropertyGroup>\n    <TargetFramework>net8.0</TargetFramework>\n  </PropertyGroup>\n</Project>\n";
        assert_eq!(span("csproj", xml, "PropertyGroup"), Some((2, 4)));
        let make = "VAR = 1\nbuild: dep\n\techo hi\n";
        assert_eq!(span_in("Makefile", make, "build"), Some((2, 3)));
    }

    #[test]
    fn sql_and_proto() {
        let sql = "-- users\nCREATE TABLE users (\n  id int\n);\n\nCREATE VIEW v AS SELECT 1;\n";
        assert_eq!(span("sql", sql, "users"), Some((1, 4)));
        let proto = "syntax = \"proto3\";\nmessage User {\n  string id = 1;\n}\nservice S {\n  rpc Get(User) returns (User);\n}\n";
        assert_eq!(span("proto", proto, "User"), Some((2, 4)));
        assert_eq!(span("proto", proto, "S"), Some((5, 7)));
    }

    #[test]
    fn heuristic_tier_for_unknown_code() {
        // Unity shader: no bundled grammar
        let shader = "Shader \"Custom/X\" {\n  SubShader {\n    Pass {\n      CGPROGRAM\n      float4 frag(v2f i) : SV_Target {\n        return 1;\n      }\n      ENDCG\n    }\n  }\n}\n";
        let d = defs_named("x.shader", shader);
        assert!(d.iter().all(|d| d.kind.starts_with('~')), "heuristic kinds are marked approximate: {d:?}");
        assert_eq!(span("shader", shader, "Custom/X"), Some((1, 11)));
    }

    #[test]
    fn text_tier_has_no_definitions() {
        let log = "starting\nERROR: boom\nmore\n";
        assert!(defs_named("run.log", log).is_empty());
    }

    const MD: &str = "# Title\n\nintro\n\n## Task 1\n\nbody 1\n\n```\n# not a heading\n```\n\n### Sub\n\nsub body\n\n## Task 2\n\nbody 2\n\n";

    #[test]
    fn md_sections_nest_and_skip_fences() {
        assert_eq!(span("md", MD, "Task 1"), Some((5, 15)));
        assert_eq!(span("md", MD, "## Task 2"), Some((17, 19)));
        assert_eq!(span("md", MD, "Sub"), Some((13, 15)));
        assert_eq!(span("md", MD, "task 2"), Some((17, 19)));
        assert_eq!(defs_named("f.md", MD).len(), 4);
    }

    #[test]
    fn every_bundled_grammar_keeps_its_patterns() {
        // A query pattern that no longer matches the grammar is dropped silently at runtime;
        // this test makes that loud: each language must keep at least one pattern.
        for l in languages::LANGS {
            let lang: tree_sitter::Language = (l.language)();
            let kept = l.queries.iter().flat_map(|q| languages::split_patterns(q)).filter(|p| tree_sitter::Query::new(&lang, p).is_ok()).count();
            assert!(kept > 0, "{}: every query pattern failed to compile", l.id);
        }
    }

    #[test]
    fn missing_symbol_is_empty() {
        assert_eq!(span("go", GO, "Nope"), None);
    }
}

fn tools_cmd(op: ToolsCmd) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let proj = |p: &Option<PathBuf>| -> PathBuf {
        let p = p.clone().unwrap_or(cwd.clone());
        let p = fs::canonicalize(&p).unwrap_or(p);
        PathBuf::from(tools::repo_root(&p.to_string_lossy()))
    };
    let mut c = match tools::open() { Ok(c) => c, Err(e) => { eprintln!("mt tools: {e}"); return ExitCode::from(2) } };
    let n: i64 = c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0)).unwrap_or(0);
    if n == 0 && !matches!(op, ToolsCmd::Index { .. }) {
        if let Err(e) = tools::index(&mut c, false) { eprintln!("mt tools: {e}"); return ExitCode::from(2); }
    }
    let graph = |c: &rusqlite::Connection, p: &Path| -> Result<tools::Graph, ExitCode> {
        tools::Graph::load(c, tools::facts(p)).map_err(|e| { eprintln!("mt tools: {e}"); ExitCode::from(2) })
    };
    match op {
        ToolsCmd::Index { reprobe, quiet } => match tools::index(&mut c, reprobe) {
            Ok(s) => {
                if !quiet {
                    for p in &s.probed { println!("  probe {p}"); }
                    println!("mt tools index: {} nodes, {} edges in {} ms ({} MCP servers asked)", s.nodes, s.edges, s.ms, s.probed.len());
                }
                ExitCode::SUCCESS
            }
            Err(e) => { eprintln!("mt tools index: {e}"); ExitCode::from(2) }
        },
        ToolsCmd::Facts { project } => {
            let p = proj(&project);
            let f = tools::facts(&p);
            println!("project {} — {} files walked in {} ms", p.display(), f.files, f.ms);
            println!("languages: {}", f.langs.iter().map(|(l, n)| format!("{l} {n}")).collect::<Vec<_>>().join(", "));
            for (k, e) in &f.strong { println!("  {k:24} {e}"); }
            let mut r: Vec<_> = f.refs.iter().filter(|x| *x.1 >= 3).collect();
            r.sort_by_key(|x| -x.1);
            println!("identifiers referenced 3+ times: {}", r.len());
            ExitCode::SUCCESS
        }
        ToolsCmd::Find { query, limit, project, settings, explain } => {
            let p = proj(&project);
            let t = std::time::Instant::now();
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            let st = tools::state(&p, settings.as_deref());
            let q = query.join(" ");
            let hits = tools::find(&g, &q, limit);
            if explain { for h in hits.iter().take(limit * 3) { println!("{:6.2} text {:5.2}  {}  [{}]", h.score, h.text, h.id, h.why.join(", ")); } println!(); }
            let code = tools::code_cards(&g, &p, &q, 3);
            let cards = tools::cards(&g, &st, &hits, limit, 0.0);
            if cards.is_empty() && code.is_empty() { println!("mt tools: nothing matches {q:?}"); }
            for c in code.iter().chain(cards.iter()) { println!("{c}"); }
            println!("({} nodes searched in {} ms, project {})", g.nodes.len(), t.elapsed().as_millis(), p.display());
            ExitCode::SUCCESS
        }
        ToolsCmd::Show { name, project, settings } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            match tools::show(&g, &tools::state(&p, settings.as_deref()), &name) { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        ToolsCmd::Profile { project, write, out, undo, keep, include_hooked, keep_descriptions, slim, install } => {
            let p = proj(&project);
            if undo {
                for l in tools_ext::remove_slim(&p) { println!("{l}"); }
                let _ = fs::remove_file(p.join(".mt/tools-profile.json"));
                return match tools::undo_profile(&p) { Ok(s) => { println!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } };
            }
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            let opts = tools::ProfileOpts { keep, include_hooked, keep_descriptions };
            let (mut layer, mut log) = tools::profile(&g, &opts);
            if slim {
                let off: std::collections::HashSet<String> = layer.get("enabledPlugins").and_then(|x| x.as_object()).map(|m| m.keys().cloned().collect()).unwrap_or_default();
                let plans = tools_ext::slim_plans(&g, &off);
                let exe = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mt".into());
                if !plans.is_empty() { log.push(format!("note  --slim: Claude Code installs the trimmed plugins (scope local, marketplace {}) the first time a session here loads them; `mt tools profile --undo` uninstalls them", tools_ext::marketplace_name(&p))); }
                for pl in &plans {
                    log.push(format!("slim  plugin {} — {} of {} skills kept; dropped: {}", pl.plugin, pl.keep.len(), pl.keep.len() + pl.drop.len(), pl.drop.iter().map(|(d, w)| format!("{d} ({w})")).collect::<Vec<_>>().join(", ")));
                }
                if !plans.is_empty() && (write || out.is_some()) {
                    match tools_ext::write_slim(&p, &plans, &exe) {
                        Ok(v) => {
                            for (k, val) in v.as_object().unwrap() {
                                if let (Some(dst), Some(src)) = (layer.get_mut(k).and_then(|x| x.as_object_mut()), val.as_object()) { for (a, b) in src { dst.insert(a.clone(), b.clone()); } } else { layer[k] = val.clone(); }
                            }
                        }
                        Err(e) => { eprintln!("mt tools slim: {e}"); return ExitCode::from(2) }
                    }
                }
            }
            println!("mt tools profile for {} ({} files, {} code facts)", p.display(), g.facts.files, g.facts.strong.len());
            for l in &log { println!("  {l}"); }
            let count = |k: &str| layer.pointer(k).map(|v| v.as_array().map(|a| a.len()).or(v.as_object().map(|o| o.len())).unwrap_or(0)).unwrap_or(0);
            println!("layer: {} plugins off, {} skills off, {} denies (agents, plugin MCP), {} MCP servers denied", count("/enabledPlugins"), count("/skillOverrides"), count("/permissions/deny"), count("/deniedMcpServers"));
            if let Some(o) = &out {
                if let Err(e) = fs::write(o, serde_json::to_string_pretty(&layer).unwrap()) { eprintln!("mt tools: {}: {e}", o.display()); return ExitCode::from(2); }
                println!("wrote {}", o.display());
            }
            if write {
                let exe = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mt".into());
                let hooks = [("SessionStart", None, format!("\"{exe}\" tools hook-start")), ("UserPromptSubmit", None, format!("\"{exe}\" tools hook")), ("PostToolUse", Some("mcp__.*|ToolSearch"), format!("\"{exe}\" tools hook-post"))];
                match tools::write_profile(&p, &layer, &hooks) {
                    Ok(f) => {
                        tools_ext::save_profile(&p, &layer, &opts);
                        println!("merged into {} with SessionStart, UserPromptSubmit and PostToolUse hooks (undo: mt tools profile --undo)", f.display());
                        if slim && install { for l in tools_ext::install_slim(&p) { println!("  {l}"); } }
                        else if slim { println!("  trimmed plugins are declared but not installed: add --install, or accept the install prompt in an interactive session"); }
                    }
                    Err(e) => { eprintln!("mt tools: {e}"); return ExitCode::from(2) }
                }
            }
            if !write && out.is_none() { println!("{}", serde_json::to_string_pretty(&layer).unwrap()); }
            ExitCode::SUCCESS
        }
        ToolsCmd::Enable { name, project } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            match tools::enable(&g, &p, &name) { Ok(s) => { println!("{s}"); ExitCode::SUCCESS } Err(e) => { println!("{e}"); ExitCode::from(1) } }
        }
        ToolsCmd::Hook { limit, min_score, settings } => {
            let t = std::time::Instant::now();
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
            // Claude Code and Codex send "prompt"; take the first of the names agents use
            let prompt = ["prompt", "user_prompt", "input", "message"].iter().find_map(|k| v.get(*k).and_then(|x| x.as_str())).unwrap_or("");
            {
                use std::io::Write;
                let keys: Vec<&String> = v.as_object().map(|o| o.keys().collect()).unwrap_or_default();
                if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open("/tmp/mt-tools-hook-in.log") { let _ = writeln!(f, "{}", serde_json::json!({"keys": keys, "prompt_chars": prompt.len(), "cwd": v.get("cwd")})); }
            }
            let hcwd = v.get("cwd").and_then(|x| x.as_str()).map(PathBuf::from).unwrap_or(cwd.clone());
            if prompt.trim().is_empty() { return ExitCode::SUCCESS; }
            let p = proj(&Some(hcwd));
            let t_facts = std::time::Instant::now();
            let facts = tools::facts(&p);
            let facts_ms = t_facts.elapsed().as_millis();
            let t_graph = std::time::Instant::now();
            let Ok(g) = tools::Graph::load(&c, facts) else { return ExitCode::SUCCESS };
            let graph_ms = t_graph.elapsed().as_millis();
            let st = tools::state(&p, settings.as_deref());
            let t_code = std::time::Instant::now();
            let mut cards = tools::code_cards(&g, &p, prompt, 3);
            let code_ms = t_code.elapsed().as_millis();
            let t_tools = std::time::Instant::now();
            cards.extend(tools::cards(&g, &st, &tools::find(&g, prompt, limit), limit, min_score));
            let tools_ms = t_tools.elapsed().as_millis();
            tools::hook_log(&p, prompt, &cards, t.elapsed().as_millis(), serde_json::json!({"facts": facts_ms, "graph": graph_ms, "code": code_ms, "tools": tools_ms}));
            if cards.is_empty() { return ExitCode::SUCCESS; }
            let ctx = format!("mt — the code, tests, data and docs, and the skills, agents and MCP servers relevant to this prompt, from mt's graph of this repo. Lines marked \"read from disk now\" are the file as it is at this moment (the h= hash is mt's span hash): quote and edit from them, there is no need to read them again. This project's settings turn off what its code doesn't use; everything stays reachable as shown.\n{}", cards.join("\n"));
            println!("{}", serde_json::json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}}));
            ExitCode::SUCCESS
        }
        ToolsCmd::Bench { gold, project } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            let v: serde_json::Value = match fs::read_to_string(&gold).ok().and_then(|s| serde_json::from_str(&s).ok()) { Some(v) => v, None => { eprintln!("mt tools bench: cannot read {}", gold.display()); return ExitCode::from(2) } };
            print!("{}", tools::bench(&g, &tools::state(&p, None), &p, &v));
            ExitCode::SUCCESS
        }
        ToolsCmd::Gate { event, cmd_b64, cmd } => {
            let line = cmd_b64.map(|b| tools_ext::unb64(&b)).unwrap_or_else(|| cmd.join(" "));
            return ExitCode::from(tools_ext::gate(&event, &line).clamp(0, 255) as u8);
        }
        ToolsCmd::Hooks { project, wrap } => {
            if let Some(f) = wrap {
                let exe = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mt".into());
                return match tools_ext::wrap_settings(&f, &exe) { Ok(s) => { println!("{s}"); ExitCode::SUCCESS } Err(e) => { eprintln!("mt tools hooks: {e}"); ExitCode::from(2) } };
            }
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            print!("{}", tools_ext::hooks_report(&g));
            ExitCode::SUCCESS
        }
        ToolsCmd::HookStart => {
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
            let p = proj(&v.get("cwd").and_then(|x| x.as_str()).map(PathBuf::from));
            let _ = tools::index(&mut c, false);
            let Ok(g) = graph(&c, &p) else { return ExitCode::SUCCESS };
            let lines = tools_ext::refresh(&p, &g);
            if !lines.is_empty() {
                let ctx = format!("mt tools: this project's code changed what it needs, so its settings were updated (skills now; plugins and MCP servers from the next session):\n{}", lines.join("\n"));
                println!("{}", serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ctx}}));
            }
            ExitCode::SUCCESS
        }
        ToolsCmd::HookPost => {
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
            tools_ext::capture(&v);
            ExitCode::SUCCESS
        }
        ToolsCmd::Explain { project, session } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            match tools_ext::explain(&p, &g, session.as_deref()) { Ok(s) => { print!("{s}"); ExitCode::SUCCESS } Err(e) => { eprintln!("mt tools explain: {e}"); ExitCode::from(1) } }
        }
        ToolsCmd::Agents { project } => {
            let p = proj(&project);
            let exe = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mt".into());
            match tools_ext::agents(&p, &exe) { Ok(v) => { for l in v { println!("{l}"); } ExitCode::SUCCESS } Err(e) => { eprintln!("mt tools agents: {e}"); ExitCode::from(2) } }
        }
        ToolsCmd::Embed { project } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            let t = std::time::Instant::now();
            // SAFETY of env: single-threaded at this point; code_cards reads it to fill the cache
            unsafe { std::env::set_var("MT_EMBED_FILL", "1"); }
            let n = tools::code_cards(&g, &p, "warm the embedding cache", 1).len();
            println!("embedding cache filled for {} in {} ms ({n} card)", p.display(), t.elapsed().as_millis());
            ExitCode::SUCCESS
        }
        ToolsCmd::Routes { project } => { print!("{}", tools::routes_report(&proj(&project))); ExitCode::SUCCESS }
        ToolsCmd::Stats { project } => {
            let p = proj(&project);
            let g = match graph(&c, &p) { Ok(g) => g, Err(e) => return e };
            let mut kinds: std::collections::BTreeMap<String, (usize, usize, usize)> = Default::default();
            for n in g.nodes.values() {
                let e = kinds.entry(n.kind.clone()).or_default();
                e.0 += 1;
                if g.anchors(n).is_empty() { e.1 += 1; } else if g.grounded(n).is_some() { e.2 += 1; }
            }
            println!("project {}", p.display());
            for (k, (n, general, gr)) in kinds { println!("  {k:7} {n:5} known: {general:4} general-purpose, {gr:4} tied to this code, {:4} tied to something this code doesn't use", n - general - gr); }
            ExitCode::SUCCESS
        }
    }
}
