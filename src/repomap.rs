//! Repository map for the system prompt: every file with its line count and the definitions
//! found by tree-sitter tag queries. Built once and kept byte-identical between requests so
//! the server's KV cache stays valid; it is only rebuilt on `/map` and on compaction. Definitions
//! are listed without line numbers: those go stale with every edit, names rarely do, and
//! `read_symbol` finds code by name.
//!
//! Over budget, definitions are kept by importance: files are ranked by PageRank over the
//! graph "file references a name defined in file", and each definition scores by its file's
//! rank plus the rank flowing into it through references.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use ignore::WalkBuilder;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

/// ~2k tokens at 3 chars/token; a fixed cost in every request, so keep it small.
const BUDGET_BYTES: usize = 6000;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_LINE_CHARS: usize = 120;

pub struct RepoMap {
    pub text: String,
    pub files: usize,
}

pub(crate) struct Def {
    /// Line of the name; `start..=end` is the whole node.
    pub line: usize,
    /// Exact name offset in UTF-8 bytes, for semantic navigation.
    pub name_byte: usize,
    pub start: usize,
    pub end: usize,
    pub name: String,
    /// Scope-qualified name accepted by read_symbol, without signature syntax.
    pub qualified_name: String,
    pub text: String,
    /// Header line of the innermost enclosing impl/class/trait/module.
    pub parent: Option<String>,
    depth: Depth,
}

impl Def {
    pub(crate) fn declaration(&self) -> String {
        if self.qualified_name == self.name {
            self.text.clone()
        } else {
            format!("{}: {}", self.qualified_name, self.text)
        }
    }
}

/// Ordered by how early a definition is dropped when the map is over budget.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Depth {
    TopLevel,
    /// Impl blocks and methods.
    Member,
    /// Methods of trait impls: repetitive, and the `impl Trait for Type` line says enough.
    TraitMember,
    /// Values are less useful for navigating behavior than callable definitions.
    Value,
}

struct File {
    path: String,
    lines: Option<usize>,
    defs: Vec<Def>,
    /// Names referenced from this file (calls, types), with repeats.
    refs: Vec<String>,
}

pub fn build(root: &Path) -> Option<RepoMap> {
    let (mut files, sources): (Vec<File>, Vec<Option<String>>) = WalkBuilder::new(root)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| {
            let src = e
                .metadata()
                .is_ok_and(|m| m.len() <= MAX_FILE_BYTES)
                .then(|| std::fs::read_to_string(e.path()).ok())
                .flatten();
            let file = File {
                path: crate::tools::display(root, e.path()),
                lines: src.as_ref().map(|s| s.lines().count()),
                defs: Vec::new(),
                refs: Vec::new(),
            };
            (file, src)
        })
        .unzip();
    if files.is_empty() {
        return None;
    }

    // Parsing is wasted if even the bare paths don't fit.
    let paths_len = render(&files, None).len();
    if paths_len > BUDGET_BYTES {
        return Some(RepoMap {
            text: truncated_paths(&files),
            files: files.len(),
        });
    }
    let langs = languages();
    let mut ctx = TagsContext::new();
    for (file, src) in files.iter_mut().zip(&sources) {
        if let (Some(src), Some(config)) = (src, langs.for_path(&file.path)) {
            (file.defs, file.refs) = tags(&mut ctx, config, src, true);
        }
    }
    let shown = select(&files, BUDGET_BYTES - paths_len);
    Some(RepoMap {
        text: render(&files, Some(&shown)),
        files: files.len(),
    })
}

/// `shown[file][def]`: which definitions to list; `None` renders only paths.
fn render(files: &[File], shown: Option<&[Vec<bool>]>) -> String {
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        path_line(&mut out, f);
        let Some(shown) = shown else {
            continue;
        };
        for (d, _) in f.defs.iter().zip(&shown[i]).filter(|(_, s)| **s) {
            out.push_str(&def_line(d));
        }
    }
    out
}

fn def_line(d: &Def) -> String {
    let indent = if matches!(d.depth, Depth::Member | Depth::TraitMember) {
        "    "
    } else {
        "  "
    };
    format!("{indent}{}\n", d.declaration())
}

/// Takes definitions in priority order while they fit. Stops at the first one that doesn't,
/// so a container (which scores at least as high as its members and comes first) is never
/// skipped while its members are shown.
fn select(files: &[File], budget: usize) -> Vec<Vec<bool>> {
    let priority = priorities(files);
    let mut order: Vec<(usize, usize)> = files
        .iter()
        .enumerate()
        .flat_map(|(i, f)| (0..f.defs.len()).map(move |d| (i, d)))
        .collect();
    // Stable, so ties keep file and line order.
    order.sort_by(|&(a, x), &(b, y)| priority[b][y].total_cmp(&priority[a][x]));
    let mut shown: Vec<Vec<bool>> = files.iter().map(|f| vec![false; f.defs.len()]).collect();
    let mut used = 0;
    for (i, d) in order {
        let len = def_line(&files[i].defs[d]).len();
        if used + len > budget {
            break;
        }
        used += len;
        shown[i][d] = true;
    }
    shown
}

struct Edge<'a> {
    from: usize,
    to: usize,
    name: &'a str,
    weight: f64,
}

fn priorities(files: &[File]) -> Vec<Vec<f64>> {
    let mut definers: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        for d in &f.defs {
            let v = definers.entry(d.name.as_str()).or_default();
            if v.last() != Some(&i) {
                v.push(i);
            }
        }
    }
    let mut edges = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &f.refs {
            *counts.entry(r.as_str()).or_default() += 1;
        }
        for (name, count) in counts {
            let Some(targets) = definers.get(name) else {
                continue;
            };
            // Sublinear in repeats; a name defined in many files (`new`, `run`) says little
            // about which of them is meant.
            let weight = (count as f64).sqrt() / targets.len() as f64;
            for &to in targets.iter().filter(|&&t| t != i) {
                edges.push(Edge {
                    from: i,
                    to,
                    name,
                    weight,
                });
            }
        }
    }
    let mut out_weight = vec![0.0; files.len()];
    for e in &edges {
        out_weight[e.from] += e.weight;
    }
    let rank = pagerank(&edges, &out_weight);

    let mut inflow: BTreeMap<(usize, &str), f64> = BTreeMap::new();
    for e in &edges {
        *inflow.entry((e.to, e.name)).or_default() += rank[e.from] * e.weight / out_weight[e.from];
    }
    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut p: Vec<f64> = f
                .defs
                .iter()
                .map(|d| {
                    let depth = match d.depth {
                        Depth::TopLevel | Depth::Member => 1.0,
                        Depth::TraitMember | Depth::Value => 0.1,
                    };
                    rank[i] * depth + inflow.get(&(i, d.name.as_str())).unwrap_or(&0.0)
                })
                .collect();
            // Members come after their container, so walking backwards carries scores up
            // through nested containers.
            for k in (0..f.defs.len()).rev() {
                if let Some(c) = container(&f.defs, k) {
                    p[c] = p[c].max(p[k]);
                }
            }
            p
        })
        .collect()
}

pub(crate) fn container(defs: &[Def], k: usize) -> Option<usize> {
    let d = &defs[k];
    let parent = d.parent.as_ref()?;
    defs.iter()
        .enumerate()
        .filter(|&(i, c)| i != k && &c.text == parent && c.start <= d.start && d.end <= c.end)
        .min_by_key(|(_, c)| c.end - c.start)
        .map(|(i, _)| i)
}

fn pagerank(edges: &[Edge], out_weight: &[f64]) -> Vec<f64> {
    const DAMPING: f64 = 0.85;
    let n = out_weight.len();
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..30 {
        let dangling: f64 = (0..n)
            .filter(|&i| out_weight[i] == 0.0)
            .map(|i| rank[i])
            .sum();
        let mut next = vec![(1.0 - DAMPING + DAMPING * dangling) / n as f64; n];
        for e in edges {
            next[e.to] += DAMPING * rank[e.from] * e.weight / out_weight[e.from];
        }
        rank = next;
    }
    rank
}

fn truncated_paths(files: &[File]) -> String {
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        let mut line = String::new();
        path_line(&mut line, f);
        if out.len() + line.len() > BUDGET_BYTES - 64 {
            let _ = writeln!(out, "… and {} more files", files.len() - i);
            break;
        }
        out.push_str(&line);
    }
    out
}

fn path_line(out: &mut String, f: &File) {
    let _ = match f.lines {
        Some(n) => writeln!(out, "{} ({n} lines)", f.path),
        None => writeln!(out, "{}", f.path),
    };
}

pub(crate) fn defs(ctx: &mut TagsContext, config: &TagsConfiguration, src: &str) -> Vec<Def> {
    tags(ctx, config, src, true).0
}

/// Navigation also needs definitions in tests and multiple definitions on a single line.
pub(crate) fn navigation_defs(
    ctx: &mut TagsContext,
    config: &TagsConfiguration,
    src: &str,
) -> Vec<Def> {
    tags(ctx, config, src, false).0
}

/// Definitions and referenced names; repository-map callers omit test modules.
fn tags(
    ctx: &mut TagsContext,
    config: &TagsConfiguration,
    src: &str,
    skip_tests: bool,
) -> (Vec<Def>, Vec<String>) {
    let Ok((tags, _)) = ctx.generate_tags(config, src.as_bytes(), None) else {
        return (Vec::new(), Vec::new());
    };
    let newlines: Vec<usize> = src.match_indices('\n').map(|(i, _)| i).collect();
    let line_of = |byte: usize| newlines.partition_point(|&i| i < byte) + 1;
    let mut tests: Vec<Range<usize>> = Vec::new();
    let mut trait_impls: Vec<Range<usize>> = Vec::new();
    let mut classes: Vec<Range<usize>> = Vec::new();
    let mut containers: Vec<(Range<usize>, String)> = Vec::new();
    let mut scopes: Vec<(Range<usize>, String)> = Vec::new();
    let mut found: Vec<(Range<usize>, Def)> = Vec::new();
    let mut refs: Vec<(Range<usize>, String)> = Vec::new();
    for tag in tags.flatten() {
        let kind = config.syntax_type_name(tag.syntax_type_id);
        if !tag.is_definition {
            refs.push((tag.range.clone(), src[tag.name_range.clone()].to_string()));
        }
        // Impl blocks are only references in the Rust query, but they tell which type the
        // following methods belong to.
        if !tag.is_definition && kind != "implementation" {
            continue;
        }
        if skip_tests && kind == "module" && &src[tag.name_range.clone()] == "tests" {
            tests.push(tag.range);
            continue;
        }
        let line = src[tag.line_range.clone()].trim();
        // `mod foo;` only repeats the file tree.
        if kind == "module" && line.ends_with(';') {
            continue;
        }
        let line = line.strip_suffix('{').unwrap_or(line).trim_end();
        if kind == "implementation" && line.contains(" for ") {
            trait_impls.push(tag.range.clone());
        }
        if kind == "class" {
            classes.push(tag.range.clone());
        }
        let depth = match kind {
            "constant" => Depth::Value,
            "implementation" => Depth::Member,
            "method" if within(&tag.range, &trait_impls) => Depth::TraitMember,
            "method" => Depth::Member,
            // Python methods are tagged as plain functions.
            "function" if within(&tag.range, &classes) => Depth::Member,
            _ => Depth::TopLevel,
        };
        let text = crate::tools::clip(line, MAX_LINE_CHARS).into_owned();
        let name = src[tag.name_range.clone()].to_string();
        if matches!(kind, "implementation" | "class" | "interface" | "module") {
            containers.push((tag.range.clone(), text.clone()));
        }
        if matches!(
            kind,
            "implementation" | "class" | "interface" | "module" | "function" | "method"
        ) {
            // Trait impl tags name the trait, while their members belong to the target type.
            let scope = if kind == "implementation" {
                line.rsplit_once(" for ")
                    .map(|(_, ty)| {
                        ty.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or(&name)
                    })
                    .unwrap_or(&name)
            } else {
                &name
            };
            scopes.push((tag.range.clone(), scope.to_string()));
        }
        // Go receivers are scopes without enclosing declaration nodes.
        let header = &src[tag.range.start..tag.name_range.start];
        let receiver = (kind == "method")
            .then(|| {
                header
                    .strip_prefix("func")?
                    .trim_start()
                    .strip_prefix('(')?
                    .split_once(')')
            })
            .flatten()
            .and_then(|(receiver, _)| receiver.split_whitespace().last())
            .map(|ty| ty.trim_start_matches('*').split('[').next().unwrap_or(ty));
        let def = Def {
            line: tag.span.start.row + 1,
            name_byte: tag.name_range.start,
            start: line_of(tag.range.start),
            end: line_of(tag.range.end.saturating_sub(1)),
            qualified_name: receiver.map_or_else(|| name.clone(), |ty| format!("{ty}.{name}")),
            name,
            text,
            parent: None,
            depth,
        };
        found.push((tag.range, def));
    }
    let separator = if config.language == tree_sitter_rust::LANGUAGE.into() {
        "::"
    } else {
        "."
    };
    let mut defs: Vec<Def> = found
        .into_iter()
        .filter(|(r, _)| !within(r, &tests))
        .map(|(r, mut d)| {
            let mut parents: Vec<_> = scopes
                .iter()
                .filter(|(o, _)| *o != r && o.start <= r.start && r.end <= o.end)
                .collect();
            parents.sort_by_key(|(o, _)| std::cmp::Reverse(o.len()));
            parents.dedup_by(|a, b| a.0 == b.0);
            if !parents.is_empty() {
                let mut names: Vec<_> = parents.iter().map(|(_, name)| name.as_str()).collect();
                names.push(&d.qualified_name);
                d.qualified_name = names.join(separator);
            }
            d.parent = containers
                .iter()
                .filter(|(o, _)| *o != r && o.start <= r.start && r.end <= o.end)
                .min_by_key(|(o, _)| o.len())
                .map(|(_, t)| t.clone());
            d
        })
        .collect();
    if skip_tests {
        defs.sort_by_key(|d| d.line);
        defs.dedup_by_key(|d| d.line);
    } else {
        defs.sort_by_key(|d| d.name_byte);
    }
    let refs = refs
        .into_iter()
        .filter(|(r, _)| !within(r, &tests))
        .map(|(_, n)| n)
        .collect();
    (defs, refs)
}

fn within(r: &Range<usize>, outer: &[Range<usize>]) -> bool {
    outer.iter().any(|o| o.start <= r.start && r.end <= o.end)
}

/// Compiling the tag queries is not free, and `read_symbol` needs them on every call.
pub(crate) fn languages() -> &'static Languages {
    static LANGS: OnceLock<Languages> = OnceLock::new();
    LANGS.get_or_init(Languages::new)
}

const RUST_CONSTS_QUERY: &str = r#"
(source_file (const_item name: (identifier) @name) @definition.constant)
(source_file (static_item name: (identifier) @name) @definition.constant)
(declaration_list (const_item name: (identifier) @name) @definition.constant)
(declaration_list (static_item name: (identifier) @name) @definition.constant)
"#;

/// The stock query only sees plain and method calls; types and `Type::f()` paths carry most
/// of the cross-file links in Rust.
const RUST_REFS_QUERY: &str = r#"
(impl_item type: (generic_type type: (type_identifier) @name) !trait) @reference.implementation
(impl_item trait: (generic_type type: (type_identifier) @name)) @reference.implementation
(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(scoped_identifier path: (identifier) @name) @reference.type
(type_identifier) @name @reference.type
"#;

/// The grammar crate ships no tags query.
const BASH_QUERY: &str = "(function_definition name: (word) @name) @definition.function";

pub(crate) struct Languages {
    rust: Option<TagsConfiguration>,
    go: Option<TagsConfiguration>,
    js: Option<TagsConfiguration>,
    ts: Option<TagsConfiguration>,
    tsx: Option<TagsConfiguration>,
    python: Option<TagsConfiguration>,
    bash: Option<TagsConfiguration>,
}

impl Languages {
    fn new() -> Self {
        // The TypeScript query only covers TS-specific nodes; functions and classes come from
        // the JavaScript one, which the TS grammar extends.
        let ts_query = format!(
            "{}\n{}\n(internal_module name: (identifier) @name) @definition.module",
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY
        );
        // The stock Rust query has no consts/statics, which often hold tables worth reading.
        // Anchored to items and impl/mod bodies so locals inside function bodies stay out.
        let rust_query = format!(
            "{}\n{RUST_CONSTS_QUERY}\n{RUST_REFS_QUERY}",
            tree_sitter_rust::TAGS_QUERY
        );
        let config =
            |lang: tree_sitter::Language, query: &str| TagsConfiguration::new(lang, query, "").ok();
        Self {
            rust: config(tree_sitter_rust::LANGUAGE.into(), &rust_query),
            go: config(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY),
            js: config(
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::TAGS_QUERY,
            ),
            ts: config(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                &ts_query,
            ),
            tsx: config(tree_sitter_typescript::LANGUAGE_TSX.into(), &ts_query),
            python: config(
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::TAGS_QUERY,
            ),
            bash: config(tree_sitter_bash::LANGUAGE.into(), BASH_QUERY),
        }
    }

    pub(crate) fn for_path(&self, path: &str) -> Option<&TagsConfiguration> {
        let ext = Path::new(path).extension()?.to_str()?;
        match ext {
            "rs" => self.rust.as_ref(),
            "go" => self.go.as_ref(),
            "js" | "jsx" | "mjs" | "cjs" => self.js.as_ref(),
            "ts" | "mts" | "cts" => self.ts.as_ref(),
            "tsx" => self.tsx.as_ref(),
            "py" | "pyi" => self.python.as_ref(),
            "sh" | "bash" => self.bash.as_ref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_configs_compile() {
        let langs = Languages::new();
        assert!(langs.rust.is_some());
        assert!(langs.go.is_some());
        assert!(langs.js.is_some());
        assert!(langs.ts.is_some());
        assert!(langs.tsx.is_some());
        assert!(langs.python.is_some());
        assert!(langs.bash.is_some());
    }

    #[test]
    fn python_and_bash_defs() {
        let langs = Languages::new();
        let src = "class A:\n    def m(self):\n        pass\n\ndef f():\n    pass\n";
        let py = defs(&mut TagsContext::new(), langs.python.as_ref().unwrap(), src);
        let got: Vec<_> = py
            .iter()
            .map(|d| (d.line, d.name.as_str(), d.depth))
            .collect();
        assert_eq!(
            got,
            [
                (1, "A", Depth::TopLevel),
                (2, "m", Depth::Member),
                (5, "f", Depth::TopLevel),
            ]
        );
        let src = "build() {\n  make\n}\nfunction run {\n  build\n}\n";
        let sh = defs(&mut TagsContext::new(), langs.bash.as_ref().unwrap(), src);
        let got: Vec<_> = sh
            .iter()
            .map(|d| (d.start, d.end, d.name.as_str()))
            .collect();
        assert_eq!(got, [(1, 3, "build"), (4, 6, "run")]);
    }

    #[test]
    fn scoped_names_in_map_cover_supported_languages() {
        let cases = [
            (
                "x.rs",
                "mod outer {\nstruct A<T>(T);\nimpl<T> A<T> {\nfn run() {}\n}\n}\n",
                "outer::A::run",
            ),
            (
                "x.rs",
                "struct A;\nimpl Tr for A {\nfn run() {}\n}\n",
                "A::run",
            ),
            (
                "x.go",
                "package p\ntype A[T any] struct {}\nfunc (a *A[T]) run() {}\n",
                "A.run",
            ),
            ("x.js", "class A {\nrun() {}\n}\n", "A.run"),
            (
                "x.ts",
                "namespace outer {\nexport class A {\nrun() {}\n}\n}\n",
                "outer.A.run",
            ),
            ("x.tsx", "class A {\nrun() {}\n}\n", "A.run"),
            (
                "x.py",
                "class Outer:\n    class A:\n        def run(self):\n            pass\n",
                "Outer.A.run",
            ),
            (
                "x.py",
                "def outer():\n    def run():\n        pass\n",
                "outer.run",
            ),
            ("x.sh", "run() {\n true\n}\n", "run"),
        ];
        for (path, src, expected) in cases {
            let defs = defs(
                &mut TagsContext::new(),
                languages().for_path(path).unwrap(),
                src,
            );
            let d = defs.iter().find(|d| d.name == "run").unwrap();
            assert_eq!(d.qualified_name, expected, "{path}: {src}");
            assert!(def_line(d).contains(expected), "{path}: {src}");
        }
    }

    #[test]
    fn limited_budget_keeps_methods_before_unreferenced_values() {
        let src = "const FIRST: &str = \"long descriptive text\";\n\
                   const SECOND: &str = \"more descriptive text\";\n\
                   struct Store;\nimpl Store {\nfn read() {}\n}\n";
        let defs = defs(
            &mut TagsContext::new(),
            languages().for_path("store.rs").unwrap(),
            src,
        );
        let budget = defs
            .iter()
            .filter(|d| d.depth != Depth::Value)
            .map(|d| def_line(d).len())
            .sum();
        let files = [File {
            path: "store.rs".into(),
            lines: Some(src.lines().count()),
            defs,
            refs: Vec::new(),
        }];
        let shown = select(&files, budget);
        let map = render(&files, Some(&shown));
        assert!(map.contains("Store::read"), "{map}");
        assert!(!map.contains("FIRST"), "{map}");
        assert!(!map.contains("SECOND"), "{map}");
        assert_eq!(map.len(), render(&files, None).len() + budget);
    }

    #[test]
    fn rust_defs_levels() {
        let src = "pub struct A;\nimpl A {\n    pub fn new() -> Self {\n        A\n    }\n}\n\
                   impl Tr for A {\n    fn f() {}\n}\nmod sub;\n\
                   #[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        let langs = Languages::new();
        let defs = defs(&mut TagsContext::new(), langs.rust.as_ref().unwrap(), src);
        let got: Vec<_> = defs
            .iter()
            .map(|d| (d.line, d.text.as_str(), d.depth))
            .collect();
        assert_eq!(
            got,
            [
                (1, "pub struct A;", Depth::TopLevel),
                (2, "impl A", Depth::Member),
                (3, "pub fn new() -> Self", Depth::Member),
                (7, "impl Tr for A", Depth::Member),
                (8, "fn f() {}", Depth::TraitMember),
            ]
        );
    }
}
