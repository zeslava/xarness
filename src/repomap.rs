//! Repository map for the system prompt: every file with its line count and the definitions
//! found by tree-sitter tag queries. Built once and kept byte-identical between requests so
//! the server's KV cache stays valid; it is only rebuilt on demand (`/map`).

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
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub text: String,
    /// Header line of the innermost enclosing impl/class/trait/module.
    pub parent: Option<String>,
    depth: Depth,
}

/// Ordered by how early a definition is dropped when the map is over budget.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Depth {
    TopLevel,
    /// Impl blocks and methods.
    Member,
    /// Methods of trait impls: repetitive, and the `impl Trait for Type` line says enough.
    TraitMember,
}

struct File {
    path: String,
    lines: Option<usize>,
    defs: Vec<Def>,
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
            };
            (file, src)
        })
        .unzip();
    if files.is_empty() {
        return None;
    }

    // Parsing is wasted if even the bare paths don't fit.
    if render(&files, None).len() <= BUDGET_BYTES {
        let langs = languages();
        let mut ctx = TagsContext::new();
        for (file, src) in files.iter_mut().zip(&sources) {
            if let (Some(src), Some(config)) = (src, langs.for_path(&file.path)) {
                file.defs = defs(&mut ctx, config, src);
            }
        }
    }

    let text = [
        Some(Depth::TraitMember),
        Some(Depth::Member),
        Some(Depth::TopLevel),
        None,
    ]
    .into_iter()
    .map(|d| render(&files, d))
    .find(|t| t.len() <= BUDGET_BYTES)
    .unwrap_or_else(|| truncated_paths(&files));
    Some(RepoMap {
        text,
        files: files.len(),
    })
}

/// `max`: the deepest `Depth` to include; `None` renders only paths.
fn render(files: &[File], max: Option<Depth>) -> String {
    let mut out = String::new();
    for f in files {
        path_line(&mut out, f);
        let Some(max) = max else {
            continue;
        };
        for d in f.defs.iter().filter(|d| d.depth <= max) {
            let indent = if d.depth == Depth::TopLevel {
                "  "
            } else {
                "    "
            };
            let _ = writeln!(out, "{indent}{}: {}", d.line, d.text);
        }
    }
    out
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
    let Ok((tags, _)) = ctx.generate_tags(config, src.as_bytes(), None) else {
        return Vec::new();
    };
    let newlines: Vec<usize> = src.match_indices('\n').map(|(i, _)| i).collect();
    let line_of = |byte: usize| newlines.partition_point(|&i| i < byte) + 1;
    let mut tests: Vec<Range<usize>> = Vec::new();
    let mut trait_impls: Vec<Range<usize>> = Vec::new();
    let mut containers: Vec<(Range<usize>, String)> = Vec::new();
    let mut found: Vec<(Range<usize>, Def)> = Vec::new();
    for tag in tags.flatten() {
        let kind = config.syntax_type_name(tag.syntax_type_id);
        // Impl blocks are only references in the Rust query, but they tell which type the
        // following methods belong to.
        if !tag.is_definition && kind != "implementation" {
            continue;
        }
        if kind == "module" && &src[tag.name_range.clone()] == "tests" {
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
        let depth = match kind {
            "implementation" => Depth::Member,
            "method" if within(&tag.range, &trait_impls) => Depth::TraitMember,
            "method" => Depth::Member,
            _ => Depth::TopLevel,
        };
        let text = crate::tools::clip(line, MAX_LINE_CHARS).into_owned();
        if matches!(kind, "implementation" | "class" | "interface" | "module") {
            containers.push((tag.range.clone(), text.clone()));
        }
        let def = Def {
            line: tag.span.start.row + 1,
            start: line_of(tag.range.start),
            end: line_of(tag.range.end.saturating_sub(1)),
            name: src[tag.name_range.clone()].to_string(),
            text,
            parent: None,
            depth,
        };
        found.push((tag.range, def));
    }
    let mut defs: Vec<Def> = found
        .into_iter()
        .filter(|(r, _)| !within(r, &tests))
        .map(|(r, mut d)| {
            d.parent = containers
                .iter()
                .filter(|(o, _)| *o != r && o.start <= r.start && r.end <= o.end)
                .min_by_key(|(o, _)| o.len())
                .map(|(_, t)| t.clone());
            d
        })
        .collect();
    defs.sort_by_key(|d| d.line);
    defs.dedup_by_key(|d| d.line);
    defs
}

fn within(r: &Range<usize>, outer: &[Range<usize>]) -> bool {
    outer.iter().any(|o| o.start <= r.start && r.end <= o.end)
}

/// Compiling the tag queries is not free, and `read_symbol` needs them on every call.
pub(crate) fn languages() -> &'static Languages {
    static LANGS: OnceLock<Languages> = OnceLock::new();
    LANGS.get_or_init(Languages::new)
}

pub(crate) struct Languages {
    rust: Option<TagsConfiguration>,
    go: Option<TagsConfiguration>,
    js: Option<TagsConfiguration>,
    ts: Option<TagsConfiguration>,
    tsx: Option<TagsConfiguration>,
}

impl Languages {
    fn new() -> Self {
        // The TypeScript query only covers TS-specific nodes; functions and classes come from
        // the JavaScript one, which the TS grammar extends.
        let ts_query = format!(
            "{}\n{}",
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY
        );
        let config =
            |lang: tree_sitter::Language, query: &str| TagsConfiguration::new(lang, query, "").ok();
        Self {
            rust: config(
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::TAGS_QUERY,
            ),
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
