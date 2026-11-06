//! Language support in three tiers.
//!
//! 1. tree-sitter: a bundled grammar plus a tags query (the shipped `TAGS_QUERY` where the
//!    grammar has one, plus mt's own patterns for what it misses). Definitions come from
//!    `@definition.<kind>` captures with an `@name` capture, the same convention GitHub's
//!    code navigation uses, so one engine serves every grammar.
//! 2. heuristic: no grammar for the file, but it looks like code. Definition lines are found
//!    by keyword and the span by braces or indentation. Kinds are prefixed `~` so the output
//!    says plainly that they are approximate.
//! 3. text: logs, prose, data mt has no structure for. No definitions; `/regex/` queries
//!    return line windows instead.

use crate::Def;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

pub struct Lang {
    pub id: &'static str,
    pub language: fn() -> Language,
    pub queries: &'static [&'static str],
    /// Line prefixes that attach to the definition below them (doc comments, attributes).
    pub leading: &'static [&'static str],
    /// Data/markup languages: keep only shallow keys so huge files stay readable.
    pub max_depth: Option<usize>,
}

pub enum Tier {
    TreeSitter(&'static Lang),
    Markdown,
    Heuristic,
    Text,
}

impl Tier {
    pub fn label(&self) -> String {
        match self {
            Tier::TreeSitter(l) => format!("tree-sitter:{}", l.id),
            Tier::Markdown => "markdown".into(),
            Tier::Heuristic => "heuristic".into(),
            Tier::Text => "text".into(),
        }
    }
}

const C_LIKE: &[&str] = &["//", "/*", "*", "*/"];
const HASH: &[&str] = &["#"];

// ------------------------------------------------------------------ mt's own query patterns

const GO_EXTRA: &str = r#"
(var_spec name: (identifier) @name) @definition.var
(const_spec name: (identifier) @name) @definition.constant
"#;

const CS_EXTRA: &str = r#"
(struct_declaration name: (identifier) @name) @definition.struct
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.record
(constructor_declaration name: (identifier) @name) @definition.ctor
(destructor_declaration name: (identifier) @name) @definition.dtor
(property_declaration name: (identifier) @name) @definition.property
(indexer_declaration) @definition.indexer
(event_declaration name: (identifier) @name) @definition.event
(delegate_declaration name: (identifier) @name) @definition.delegate
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
(field_declaration (variable_declaration (variable_declarator (identifier) @name))) @definition.field
(enum_member_declaration name: (identifier) @name) @definition.member
"#;

const TS_EXTRA: &str = r#"
(class_declaration name: (type_identifier) @name) @definition.class
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(public_field_definition name: (property_identifier) @name value: [(arrow_function) (function_expression)]) @definition.method
(program (lexical_declaration (variable_declarator name: (identifier) @name value: [(object) (array) (call_expression) (new_expression) (as_expression) (satisfies_expression) (template_string) (string) (number)]) @definition.constant))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name value: [(object) (array) (call_expression) (new_expression) (as_expression) (satisfies_expression) (template_string) (string) (number)]) @definition.constant)))
"#;

const JS_EXTRA: &str = r#"
(program (lexical_declaration (variable_declarator name: (identifier) @name value: [(object) (array) (call_expression) (new_expression) (template_string) (string) (number)]) @definition.constant))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name value: [(object) (array) (call_expression) (new_expression) (template_string) (string) (number)]) @definition.constant)))
(field_definition property: (property_identifier) @name value: [(arrow_function) (function_expression)]) @definition.method
"#;

const PY_EXTRA: &str = r#"
(class_definition body: (block (expression_statement (assignment left: (identifier) @name) @definition.field)))
"#;

const RUST_EXTRA: &str = r#"
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.static
(impl_item type: (type_identifier) @name) @definition.impl
(impl_item type: (generic_type type: (type_identifier) @name)) @definition.impl
(field_declaration name: (field_identifier) @name) @definition.field
(enum_variant name: (identifier) @name) @definition.variant
"#;

const JAVA_EXTRA: &str = r#"
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.record
(constructor_declaration name: (identifier) @name) @definition.ctor
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @definition.field
(annotation_type_declaration name: (identifier) @name) @definition.annotation
"#;

const C_EXTRA: &str = r#"
(enum_specifier name: (type_identifier) @name body: (_)) @definition.enum
(union_specifier name: (type_identifier) @name body: (_)) @definition.union
(preproc_function_def name: (identifier) @name) @definition.macro
(preproc_def name: (identifier) @name) @definition.macro
(declaration declarator: (function_declarator declarator: (identifier) @name)) @definition.prototype
"#;

const CPP_EXTRA: &str = r#"
(class_specifier name: (type_identifier) @name body: (_)) @definition.class
(namespace_definition name: (namespace_identifier) @name) @definition.module
(function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @definition.method
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @definition.method
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @definition.method
"#;

const SWIFT_EXTRA: &str = r#"
(property_declaration name: (pattern (simple_identifier) @name)) @definition.property
(init_declaration) @definition.init
"#;

const KOTLIN: &str = r#"
(class_declaration name: (identifier) @name) @definition.class
(class_declaration (type_identifier) @name) @definition.class
(object_declaration name: (identifier) @name) @definition.object
(object_declaration (type_identifier) @name) @definition.object
(function_declaration name: (identifier) @name) @definition.function
(function_declaration (simple_identifier) @name) @definition.function
(property_declaration (variable_declaration (identifier) @name)) @definition.property
(property_declaration (variable_declaration (simple_identifier) @name)) @definition.property
(secondary_constructor) @definition.ctor
"#;

const SCALA: &str = r#"
(class_definition name: (identifier) @name) @definition.class
(object_definition name: (identifier) @name) @definition.object
(trait_definition name: (identifier) @name) @definition.trait
(enum_definition name: (identifier) @name) @definition.enum
(function_definition name: (identifier) @name) @definition.function
(function_declaration name: (identifier) @name) @definition.function
(val_definition pattern: (identifier) @name) @definition.val
(var_definition pattern: (identifier) @name) @definition.var
(type_definition name: (type_identifier) @name) @definition.type
"#;

const HASKELL: &str = r#"
(function name: (variable) @name) @definition.function
(bind name: (variable) @name) @definition.function
(signature name: (variable) @name) @definition.signature
(data_type name: (name) @name) @definition.type
(newtype name: (name) @name) @definition.type
(type_synomym name: (name) @name) @definition.type
(type_synonym name: (name) @name) @definition.type
(class name: (name) @name) @definition.class
(instance name: (name) @name) @definition.instance
"#;

const ZIG: &str = r#"
(function_declaration name: (identifier) @name) @definition.function
(variable_declaration (identifier) @name) @definition.const
(test_declaration (string) @name) @definition.test
"#;

const JULIA: &str = r#"
(function_definition (signature (call_expression . (identifier) @name))) @definition.function
(function_definition name: (identifier) @name) @definition.function
(assignment . (call_expression . (identifier) @name)) @definition.function
(struct_definition (type_head (identifier) @name)) @definition.struct
(struct_definition name: (identifier) @name) @definition.struct
(module_definition name: (identifier) @name) @definition.module
(module_definition (identifier) @name) @definition.module
(macro_definition (signature (call_expression . (identifier) @name))) @definition.macro
"#;

const OBJC: &str = r#"
(class_interface . (identifier) @name) @definition.interface
(class_implementation . (identifier) @name) @definition.class
(protocol_declaration . (identifier) @name) @definition.protocol
(method_declaration (identifier) @name) @definition.method
(method_definition (identifier) @name) @definition.method
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(struct_specifier name: (type_identifier) @name body: (_)) @definition.struct
"#;

const BASH: &str = r#"
(function_definition name: (word) @name) @definition.function
"#;

const MAKE: &str = r#"
(rule (targets (word) @name)) @definition.target
(variable_assignment name: (word) @name) @definition.var
(define_directive name: (word) @name) @definition.macro
"#;

const HCL: &str = r#"
(block (identifier) @name) @definition.block
(attribute (identifier) @name) @definition.attr
"#;

const CSS: &str = r#"
(rule_set (selectors) @name) @definition.rule
(keyframes_statement (keyframes_name) @name) @definition.keyframes
(media_statement (_) @name) @definition.media
"#;

const HTML: &str = r#"
(element (start_tag (attribute (attribute_name) @_a (quoted_attribute_value (attribute_value) @name))) (#eq? @_a "id")) @definition.id
(element (start_tag (tag_name) @name) (#match? @name "^(head|body|main|header|footer|nav|section|article|aside|form|template|table|dialog)$")) @definition.element
(script_element (start_tag (tag_name) @name)) @definition.script
(style_element (start_tag (tag_name) @name)) @definition.style
"#;

const XML: &str = r#"
(element (STag (Name) @name)) @definition.element
(element (EmptyElemTag (Name) @name)) @definition.element
"#;

const JSON: &str = r#"
(pair key: (_) @name) @definition.key
"#;

const YAML: &str = r#"
(block_mapping_pair key: (_) @name) @definition.key
(flow_pair key: (_) @name) @definition.key
"#;

const TOML: &str = r#"
(table (bare_key) @name) @definition.table
(table (dotted_key) @name) @definition.table
(table (quoted_key) @name) @definition.table
(table_array_element (bare_key) @name) @definition.table
(table_array_element (dotted_key) @name) @definition.table
(pair (bare_key) @name) @definition.key
(pair (dotted_key) @name) @definition.key
(pair (quoted_key) @name) @definition.key
"#;

const PROTO: &str = r#"
(message (message_name (identifier) @name)) @definition.message
(enum (enum_name (identifier) @name)) @definition.enum
(service (service_name (identifier) @name)) @definition.service
(rpc (rpc_name (identifier) @name)) @definition.rpc
(field (identifier) @name) @definition.field
"#;

const SQL_EXTRA: &str = r#"
(create_table (object_reference name: (identifier) @name)) @definition.table
(create_view (object_reference name: (identifier) @name)) @definition.view
(create_function (object_reference name: (identifier) @name)) @definition.function
(create_index (identifier) @name) @definition.index
(create_type (object_reference name: (identifier) @name)) @definition.type
(alter_table (object_reference name: (identifier) @name)) @definition.alter
"#;

// ------------------------------------------------------------------ registry

macro_rules! lang {
    ($id:literal, $lang:expr, [$($q:expr),*], $lead:expr) => {
        Lang { id: $id, language: || $lang.into(), queries: &[$($q),*], leading: $lead, max_depth: None }
    };
    ($id:literal, $lang:expr, [$($q:expr),*], $lead:expr, $depth:expr) => {
        Lang { id: $id, language: || $lang.into(), queries: &[$($q),*], leading: $lead, max_depth: Some($depth) }
    };
}

pub static LANGS: &[Lang] = &[
    lang!("go", tree_sitter_go::LANGUAGE, [tree_sitter_go::TAGS_QUERY, GO_EXTRA], C_LIKE),
    lang!("python", tree_sitter_python::LANGUAGE, [tree_sitter_python::TAGS_QUERY, PY_EXTRA], &["#", "@"]),
    lang!("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT, [tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, TS_EXTRA], &["//", "/*", "*", "*/", "@"]),
    lang!("tsx", tree_sitter_typescript::LANGUAGE_TSX, [tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, TS_EXTRA], &["//", "/*", "*", "*/", "@"]),
    lang!("javascript", tree_sitter_javascript::LANGUAGE, [tree_sitter_javascript::TAGS_QUERY, JS_EXTRA], &["//", "/*", "*", "*/", "@"]),
    lang!("csharp", tree_sitter_c_sharp::LANGUAGE, [tree_sitter_c_sharp::TAGS_QUERY, CS_EXTRA], &["//", "/*", "*", "*/", "["]),
    lang!("rust", tree_sitter_rust::LANGUAGE, [tree_sitter_rust::TAGS_QUERY, RUST_EXTRA], &["//", "/*", "*", "*/", "#["]),
    lang!("java", tree_sitter_java::LANGUAGE, [tree_sitter_java::TAGS_QUERY, JAVA_EXTRA], &["//", "/*", "*", "*/", "@"]),
    lang!("c", tree_sitter_c::LANGUAGE, [tree_sitter_c::TAGS_QUERY, C_EXTRA], C_LIKE),
    lang!("cpp", tree_sitter_cpp::LANGUAGE, [tree_sitter_cpp::TAGS_QUERY, C_EXTRA, CPP_EXTRA], &["//", "/*", "*", "*/", "template", "[["]),
    lang!("swift", tree_sitter_swift::LANGUAGE, [tree_sitter_swift::TAGS_QUERY, SWIFT_EXTRA], &["//", "/*", "*", "*/", "@"]),
    lang!("kotlin", tree_sitter_kotlin_ng::LANGUAGE, [KOTLIN], &["//", "/*", "*", "*/", "@"]),
    lang!("objc", tree_sitter_objc::LANGUAGE, [OBJC], C_LIKE),
    lang!("ruby", tree_sitter_ruby::LANGUAGE, [tree_sitter_ruby::TAGS_QUERY], HASH),
    lang!("php", tree_sitter_php::LANGUAGE_PHP, [tree_sitter_php::TAGS_QUERY], &["//", "/*", "*", "*/", "#", "#["]),
    lang!("scala", tree_sitter_scala::LANGUAGE, [SCALA], &["//", "/*", "*", "*/", "@"]),
    lang!("haskell", tree_sitter_haskell::LANGUAGE, [HASKELL], &["--", "{-"]),
    lang!("ocaml", tree_sitter_ocaml::LANGUAGE_OCAML, [tree_sitter_ocaml::TAGS_QUERY], &["(*"]),
    lang!("elixir", tree_sitter_elixir::LANGUAGE, [tree_sitter_elixir::TAGS_QUERY], &["#", "@"]),
    lang!("lua", tree_sitter_lua::LANGUAGE, [tree_sitter_lua::TAGS_QUERY], &["--"]),
    lang!("dart", tree_sitter_dart::LANGUAGE, [tree_sitter_dart::TAGS_QUERY], &["//", "/*", "*", "*/", "@"]),
    lang!("zig", tree_sitter_zig::LANGUAGE, [ZIG], &["//"]),
    lang!("julia", tree_sitter_julia::LANGUAGE, [JULIA], &["#"]),
    lang!("r", tree_sitter_r::LANGUAGE, [tree_sitter_r::TAGS_QUERY], HASH),
    lang!("bash", tree_sitter_bash::LANGUAGE, [BASH], HASH),
    lang!("make", tree_sitter_make::LANGUAGE, [MAKE], HASH),
    lang!("hcl", tree_sitter_hcl::LANGUAGE, [HCL], &["#", "//"], 2),
    lang!("proto", tree_sitter_proto::LANGUAGE, [PROTO], C_LIKE),
    lang!("sql", tree_sitter_sequel::LANGUAGE, [SQL_EXTRA], &["--"]),
    lang!("css", tree_sitter_css::LANGUAGE, [CSS], &["/*", "*"]),
    lang!("html", tree_sitter_html::LANGUAGE, [HTML], &[], 6),
    lang!("xml", tree_sitter_xml::LANGUAGE_XML, [XML], &[], 3),
    lang!("json", tree_sitter_json::LANGUAGE, [JSON], &[], 3),
    lang!("yaml", tree_sitter_yaml::LANGUAGE, [YAML], HASH, 3),
    lang!("toml", tree_sitter_toml_ng::LANGUAGE, [TOML], HASH, 3),
];

fn by_id(id: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.id == id)
}

pub fn detect(path: &Path, src: &str) -> Tier {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let id = match ext.as_str() {
        "md" | "markdown" | "mdx" | "mdc" => return Tier::Markdown,
        "go" => "go",
        "py" | "pyi" | "pyw" => "python",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "cs" | "csx" => "csharp",
        "rs" => "rust",
        "java" => "java",
        "c" => "c",
        "h" | "hh" | "hpp" | "hxx" | "cc" | "cpp" | "cxx" | "c++" | "ino" | "cu" => "cpp",
        "m" | "mm" => "objc",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "rb" | "rake" | "gemspec" => "ruby",
        "php" => "php",
        "scala" | "sc" | "sbt" => "scala",
        "hs" | "lhs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "lua" => "lua",
        "dart" => "dart",
        "zig" => "zig",
        "jl" => "julia",
        "r" => "r",
        "sh" | "bash" | "zsh" | "ksh" | "bats" => "bash",
        "mk" | "mak" => "make",
        "tf" | "tfvars" | "hcl" | "nomad" => "hcl",
        "proto" => "proto",
        "sql" | "psql" | "pgsql" => "sql",
        "css" | "scss" | "less" => "css",
        "html" | "htm" | "xhtml" | "vue" | "svelte" => "html",
        "xml" | "csproj" | "fsproj" | "vbproj" | "props" | "targets" | "plist" | "xaml" | "svg" | "resx" | "storyboard" | "xib" | "manifest" | "nuspec" => "xml",
        "json" | "jsonc" | "json5" | "webmanifest" | "asmdef" | "code-workspace" => "json",
        "yaml" | "yml" => "yaml",
        "toml" | "lock" if name.ends_with(".toml") || name == "cargo.lock" || name == "poetry.lock" || name == "uv.lock" => "toml",
        _ => match name.as_str() {
            "makefile" | "gnumakefile" => "make",
            "rakefile" | "gemfile" | "podfile" | "fastfile" | "brewfile" => "ruby",
            ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".envrc" => "bash",
            _ => match shebang(src) {
                Some(id) => id,
                None => return if looks_like_code(src) { Tier::Heuristic } else { Tier::Text },
            },
        },
    };
    by_id(id).map(Tier::TreeSitter).unwrap_or(Tier::Text)
}

fn shebang(src: &str) -> Option<&'static str> {
    let first = src.lines().next()?;
    if !first.starts_with("#!") {
        return None;
    }
    let interp = first.rsplit(['/', ' ']).find(|s| !s.is_empty() && *s != "env" && !s.starts_with('-'))?;
    Some(match interp.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
        "sh" | "bash" | "zsh" | "ksh" | "dash" => "bash",
        "python" => "python",
        "node" | "deno" | "bun" => "javascript",
        "ruby" => "ruby",
        "php" => "php",
        "lua" | "luajit" => "lua",
        "Rscript" => "r",
        "julia" => "julia",
        _ => return None,
    })
}

fn looks_like_code(src: &str) -> bool {
    let sample: Vec<&str> = src.lines().take(400).collect();
    if sample.is_empty() {
        return false;
    }
    let structural = sample.iter().filter(|l| {
        let t = l.trim_end();
        t.ends_with('{') || t.ends_with('}') || t.ends_with(';') || t.ends_with(':') || heuristic_rx().is_match(l)
    }).count();
    structural * 5 >= sample.len()
}

// ------------------------------------------------------------------ tree-sitter tier

/// Each shipped or local query is split into top-level patterns and compiled one by one:
/// a pattern written for a different grammar version is dropped instead of breaking the
/// whole language. `MT_DEBUG=1` prints what was dropped and why.
fn compiled(lang: &'static Lang) -> Option<Arc<Query>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<Arc<Query>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(q) = cache.lock().unwrap().get(lang.id) {
        return q.clone();
    }
    let language = (lang.language)();
    let debug = std::env::var_os("MT_DEBUG").is_some();
    let user = user_queries(lang.id);
    // Fast path: the whole query compiles as one. Only split it up when something is broken.
    let whole: String = lang.queries.iter().copied().chain(user.iter().map(|s| s.as_str())).collect::<Vec<_>>().join("\n");
    if !debug {
        if let Ok(q) = Query::new(&language, &whole) {
            let q = Some(Arc::new(q));
            cache.lock().unwrap().insert(lang.id, q.clone());
            return q;
        }
    }
    let mut kept = vec![];
    for q in lang.queries.iter().copied().chain(user.iter().map(|s| s.as_str())) {
        for pat in split_patterns(q) {
            match Query::new(&language, &pat) {
                Ok(_) => kept.push(pat),
                Err(e) if debug => eprintln!("mt[{}]: dropped pattern ({}): {}", lang.id, e.message.lines().next().unwrap_or(""), pat.lines().next().unwrap_or("")),
                Err(_) => {}
            }
        }
    }
    let q = Query::new(&language, &kept.join("\n")).ok().map(Arc::new);
    cache.lock().unwrap().insert(lang.id, q.clone());
    q
}

/// Extra patterns from `.mt/queries/<lang>.scm` (repo) and `~/.config/mt/queries/<lang>.scm`.
fn user_queries(id: &str) -> Vec<String> {
    let mut dirs = vec![std::path::PathBuf::from(".mt/queries")];
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(h).join(".config/mt/queries"));
    }
    dirs.iter().filter_map(|d| std::fs::read_to_string(d.join(format!("{id}.scm"))).ok()).collect()
}

pub fn split_patterns(q: &str) -> Vec<String> {
    let b = q.as_bytes();
    let mut out: Vec<String> = vec![];
    let (mut i, mut depth, mut start) = (0usize, 0i32, None::<usize>);
    while i < b.len() {
        match b[i] {
            b';' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' => {
                if depth == 0 {
                    if let Some(s) = start {
                        push_pattern(&mut out, &q[s..i]);
                    }
                    start = Some(i);
                }
                depth += 1;
            }
            b')' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if let Some(s) = start {
        push_pattern(&mut out, &q[s..]);
    }
    out
}

fn push_pattern(out: &mut Vec<String>, text: &str) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    // A top-level predicate belongs to the pattern before it.
    if t.starts_with("(#") {
        if let Some(prev) = out.last_mut() {
            prev.push('\n');
            prev.push_str(t);
        }
        return;
    }
    out.push(t.to_string());
}

pub fn treesitter_defs(lang: &'static Lang, src: &str) -> Vec<Def> {
    let mut parser = Parser::new();
    if parser.set_language(&(lang.language)()).is_err() {
        return vec![];
    }
    let (Some(tree), Some(query)) = (parser.parse(src, None), compiled(lang)) else { return vec![] };
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src.as_bytes());
    let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
    let mut raw: Vec<(Node, String, String)> = vec![];
    while let Some(m) = matches.next() {
        let mut def: Option<(Node, &str)> = None;
        let mut name: Option<Node> = None;
        for c in m.captures() {
            let cn = names[c.index as usize];
            if let Some(kind) = cn.strip_prefix("definition.") {
                def = Some((c.node, kind));
            } else if cn == "name" {
                name = Some(c.node);
            }
        }
        let Some((mut node, kind)) = def else { continue };
        // C-family tags queries mark the declarator (the signature); the definition is its owner.
        while node.kind().contains("declarator") {
            match node.parent() {
                Some(p) if p.kind().contains("declarator") || matches!(p.kind(), "function_definition" | "declaration" | "field_declaration" | "template_declaration" | "type_definition") => node = p,
                _ => break,
            }
        }
        let name = match name {
            Some(n) => clean_name(n.utf8_text(src.as_bytes()).unwrap_or("")),
            None => first_line_label(node, src),
        };
        if name.is_empty() {
            continue;
        }
        let name = qualify_receiver(node, src, name);
        let key = (node.start_byte(), node.end_byte());
        // Several patterns can match one node (shipped + extra query): keep the first.
        if seen.contains_key(&key) {
            continue;
        }
        seen.insert(key, raw.len());
        raw.push((node, kind.to_string(), name));
    }
    let end_row = |n: Node| {
        let e = n.end_position();
        if e.column == 0 && e.row > n.start_position().row { e.row - 1 } else { e.row }
    };
    nest(raw.into_iter().map(|(n, k, name)| (n.start_position().row, end_row(n), n.start_byte(), n.end_byte(), k, name)).collect(), lang.max_depth)
}

fn clean_name(s: &str) -> String {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 120 { format!("{}…", &s[..s.char_indices().nth(117).map(|(i, _)| i).unwrap_or(s.len())]) } else { s }
}

fn first_line_label(n: Node, src: &str) -> String {
    clean_name(n.utf8_text(src.as_bytes()).unwrap_or("").lines().next().unwrap_or("").trim_end_matches('{'))
}

/// Go and similar: `func (w *walker) walk()` is `walker.walk`.
fn qualify_receiver(n: Node, src: &str, name: String) -> String {
    let Some(r) = n.child_by_field_name("receiver") else { return name };
    let inner = r.utf8_text(src.as_bytes()).unwrap_or("").trim_matches(|c| c == '(' || c == ')');
    let ty = inner.split_whitespace().last().unwrap_or("").trim_start_matches('*');
    let ty = ty.split('[').next().unwrap_or("");
    if ty.is_empty() { name } else { format!("{ty}.{name}") }
}

/// Turn flat (start,end) definitions into ones carrying their enclosing definition path.
fn nest(mut v: Vec<(usize, usize, usize, usize, String, String)>, max_depth: Option<usize>) -> Vec<Def> {
    v.sort_by_key(|d| (d.2, std::cmp::Reverse(d.3)));
    let mut stack: Vec<(usize, String)> = vec![]; // (end_byte, name)
    let mut out = vec![];
    for (sr, er, sb, eb, kind, name) in v {
        while stack.last().map(|(e, _)| *e < eb || sb >= *e).unwrap_or(false) {
            stack.pop();
        }
        let path: Vec<String> = stack.iter().map(|(_, n)| n.clone()).collect();
        if max_depth.map(|m| path.len() < m).unwrap_or(true) {
            out.push(Def { kind, name: name.clone(), start: sr, end: er, path, head: sr });
        }
        stack.push((eb, name));
    }
    out
}

// ------------------------------------------------------------------ heuristic tier

fn heuristic_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new(r"^\s*(?:(?:export|public|private|protected|internal|static|async|pub|inline|virtual|override|abstract|final|extern|local|global)\s+)*(function|func|fn|def|defp|sub|proc|procedure|model|class|struct|interface|enum|trait|module|namespace|type|record|object|impl|macro|shader|kernel|Shader|SubShader|Pass|CBUFFER_START)\b\s*(?:\([^)]*\)\s*)?([A-Za-z_][\w.:$]*|\x22[^\x22]+\x22)?")
            .unwrap()
    })
}

fn c_func_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"^[A-Za-z_][\w<>,\s\*&:]*?\b([A-Za-z_]\w*)\s*\([^;]*$").unwrap())
}

pub fn heuristic_defs(lines: &[&str]) -> Vec<Def> {
    let mut v = vec![];
    for (i, l) in lines.iter().enumerate() {
        let (kind, name) = if let Some(c) = heuristic_rx().captures(l) {
            let name = c.get(2).map(|m| clean_name(m.as_str())).unwrap_or_default();
            (c[1].to_lowercase(), name)
        } else if let Some(c) = c_func_rx().captures(l) {
            // `float4 frag(v2f i) : SV_Target` style at column 0, not a control statement
            let n = &c[1];
            if matches!(n, "if" | "for" | "while" | "switch" | "return" | "catch" | "elif" | "foreach") || l.starts_with(char::is_whitespace) {
                continue;
            }
            ("function".to_string(), n.to_string())
        } else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let end = block_end(lines, i);
        v.push((i, end, line_offset(lines, i), line_offset(lines, end) + lines[end].len(), format!("~{kind}"), name));
    }
    nest(v, None)
}

fn line_offset(lines: &[&str], i: usize) -> usize {
    lines[..i].iter().map(|l| l.len() + 1).sum()
}

/// Braces if the block opens within three lines, otherwise indentation.
fn block_end(lines: &[&str], start: usize) -> usize {
    let opens = (start..(start + 3).min(lines.len())).find(|&i| lines[i].contains('{'));
    if let Some(o) = opens {
        let mut depth = 0i32;
        for (i, l) in lines.iter().enumerate().skip(o) {
            for c in l.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return i;
                        }
                    }
                    _ => {}
                }
            }
        }
        return lines.len() - 1;
    }
    let indent = |l: &str| l.len() - l.trim_start().len();
    let base = indent(lines[start]);
    let mut end = start;
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if l.trim().is_empty() {
            continue;
        }
        if indent(l) <= base {
            // `end` / `end function` style closers belong to the block
            if l.trim_start().starts_with("end") {
                end = i;
            }
            break;
        }
        end = i;
    }
    end
}

// ------------------------------------------------------------------ debug: print the syntax tree

pub fn dump_tree(path: &Path, src: &str, max_depth: usize) -> Result<String, String> {
    let Tier::TreeSitter(lang) = detect(path, src) else { return Err("no tree-sitter grammar for this file".into()) };
    let mut parser = Parser::new();
    parser.set_language(&(lang.language)()).map_err(|e| e.to_string())?;
    let tree = parser.parse(src, None).ok_or("parse failed")?;
    let mut out = String::new();
    fn walk(n: Node, src: &str, field: Option<&str>, depth: usize, max: usize, out: &mut String) {
        if depth > max {
            return;
        }
        let text = if n.named_child_count() == 0 { format!("  {:?}", n.utf8_text(src.as_bytes()).unwrap_or("").chars().take(40).collect::<String>()) } else { String::new() };
        out.push_str(&format!("{}{}{} L{}{}\n", "  ".repeat(depth), field.map(|f| format!("{f}: ")).unwrap_or_default(), n.kind(), n.start_position().row + 1, text));
        let mut c = n.walk();
        for (i, child) in n.named_children(&mut c).enumerate() {
            let f = n.field_name_for_named_child(i as u32);
            walk(child, src, f, depth + 1, max, out);
        }
    }
    walk(tree.root_node(), src, None, 0, max_depth, &mut out);
    Ok(out)
}
